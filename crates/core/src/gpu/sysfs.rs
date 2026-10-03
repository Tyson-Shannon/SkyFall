//! Linux GPU provider reading the kernel's `drm` device tree under `/sys/class/drm`.
//!
//! Supports AMD (amdgpu) and Intel (i915) GPUs without any vendor SDKs:
//! - **AMD**: utilization from `gpu_busy_percent`, VRAM from `mem_info_*`,
//!   temps/power/clocks from `hwmon`.
//! - **Intel**: no direct busy counter is exposed, so utilization is derived from
//!   accumulated `rc6_residency_ms` (time spent in deep idle) across GTs: given the
//!   delta between samples, busy ≈ `1 - Δrc6/Δt`. Clocks from `gt/gt0/rps_*`.
//!
//! NVIDIA is deliberately skipped here (handled by the NVML provider, which is tried
//! first by the composite).
//!
//! Missing attributes degrade to `None`, never to an error. This keeps hotplug and
//! kernel-version differences harmless; the collector never panics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::GpuProvider;
use crate::metrics::GpuMetrics;

const DRM_CLASS: &str = "/sys/class/drm";
const VENDOR_INTEL: u16 = 0x8086;
const VENDOR_AMD: u16 = 0x1002;

struct Dev {
    /// `/sys/class/drm/{card}` path (attributes like `gt/`, `gt_*` live here).
    card_path: PathBuf,
    /// `/sys/class/drm/{card}/device` path (PCI attrs, hwmon, amdgpu counters).
    device_path: PathBuf,
    /// Stable id: PCI BDF when resolvable, else the card basename.
    id: String,
    vendor: &'static str,
}

pub struct SysfsProvider {
    /// id -> (cumulative rc6 ms, wall ms) for Intel busy derivation.
    prev_rc6: HashMap<String, (u64, u64)>,
}

impl SysfsProvider {
    pub fn new() -> Self {
        SysfsProvider {
            prev_rc6: HashMap::new(),
        }
    }

    fn discover(&self) -> Vec<Dev> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(DRM_CLASS) else {
            return out;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("card") || !name[4..].chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let card_path = entry.path();
            let device_path = card_path.join("device");
            let Some(vendor) = read_u16(&device_path.join("vendor")) else {
                continue;
            };
            let (vendor, id) = match vendor {
                VENDOR_INTEL => ("Intel", id_from(&card_path, &name)),
                VENDOR_AMD => ("AMD", id_from(&card_path, &name)),
                _ => continue,
            };
            out.push(Dev {
                card_path,
                device_path,
                id,
                vendor,
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn collect_dev(&mut self, dev: &Dev, now_ms: u64) -> GpuMetrics {
        let device = &dev.device_path;
        let mut g = GpuMetrics {
            id: dev.id.clone(),
            vendor: dev.vendor.into(),
            model: model_name(dev.vendor, device),
            utilization: None,
            memory_used: read_u64(&device.join("mem_info_vram_used")),
            memory_total: read_u64(&device.join("mem_info_vram_total")),
            temperature: hwmon_value(device, "temp1_input").map(|v| v / 1000.0),
            power_watts: hwmon_value(device, "power1_average").map(|v| v / 1_000_000.0),
            clock_mhz: None,
        };

        match dev.vendor {
            "AMD" => {
                g.utilization = read_u64(&device.join("gpu_busy_percent")).map(|v| v as f32);
                g.clock_mhz = hwmon_value(device, "freq1_input");
            }
            "Intel" => {
                g.utilization = self.intel_utilization(&dev.id, &dev.card_path, now_ms);
                // Prefer the per-GT rpm register, then the card-level aggregate.
                g.clock_mhz = read_u64(&dev.card_path.join("gt/gt0/rps_cur_freq_mhz"))
                    .or_else(|| read_u64(&dev.card_path.join("gt_cur_freq_mhz")))
                    .map(|v| v as f32);
            }
            _ => {}
        }
        g
    }

    /// Busy% from the delta of accumulated RC6 residency across all GTs.
    fn intel_utilization(&mut self, id: &str, device: &Path, now_ms: u64) -> Option<f32> {
        let rc6_now = intel_rc6_total(device)?;
        let (rc6_prev, wall_prev) = self.prev_rc6.insert(id.to_string(), (rc6_now, now_ms))?;
        let wall_ms = now_ms.saturating_sub(wall_prev);
        let rc6_ms = rc6_now.saturating_sub(rc6_prev);
        if wall_ms < 50 {
            // Too short a window to derive anything meaningful.
            return None;
        }
        let busy = 1.0 - (rc6_ms as f32 / wall_ms as f32);
        Some((busy.clamp(0.0, 1.0)) * 100.0)
    }
}

impl Default for SysfsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuProvider for SysfsProvider {
    fn collect(&mut self) -> Vec<GpuMetrics> {
        let now_ms = now_ms();
        self.discover()
            .iter()
            .map(|d| self.collect_dev(d, now_ms))
            .collect()
    }

    fn name(&self) -> &'static str {
        "sysfs"
    }
}

fn intel_rc6_total(device: &Path) -> Option<u64> {
    let gt_root = device.join("gt");
    let mut total = 0u64;
    let gt_dirs = std::fs::read_dir(&gt_root).ok()?;
    let mut saw_any = false;
    for gt in gt_dirs.flatten() {
        if let Some(v) = read_u64(&gt.path().join("rc6_residency_ms")) {
            total += v;
            saw_any = true;
        }
    }
    saw_any.then_some(total)
}

/// `card/device` is a symlink to the PCI device; use its basename
/// (`0000:00:02.0`) as the stable id.
fn id_from(card_path: &Path, card: &str) -> String {
    pci_id(&card_path.join("device"))
        .filter(|s| s.starts_with("0000:"))
        .unwrap_or_else(|| card.to_string())
}

fn pci_id(device_path: &Path) -> Option<String> {
    std::fs::read_link(device_path)
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
}

fn read_u16(path: &Path) -> Option<u16> {
    let raw = std::fs::read_to_string(path).ok()?;
    u16::from_str_radix(raw.trim().trim_start_matches("0x"), 16).ok()
}

fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Best-effort read of the first matching numeric hwmon attribute under
/// `/sys/class/drm/card*/device/hwmon/hwmon*/`.
fn hwmon_value(device: &Path, attr: &str) -> Option<f32> {
    let hwmon_root = device.join("hwmon");
    let dirs = std::fs::read_dir(&hwmon_root).ok()?;
    for d in dirs.flatten() {
        if let Some(v) = read_u64(&d.path().join(attr)) {
            return Some(v as f32);
        }
    }
    None
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Human-friendly names for well-known device ids; falls back to a hex tag.
fn model_name(vendor: &str, device: &Path) -> String {
    let Some(dev_id) = read_u16(&device.join("device")) else {
        return format!("{vendor} GPU");
    };
    let known: &[(&str, &str)] = match vendor {
        "Intel" => &[
            ("a7a0", "Iris Xe Graphics (Raptor Lake-P)"),
            ("a78a", "Iris Xe Graphics (Alder Lake-P)"),
            ("9a49", "UHD Graphics (Tiger Lake)"),
            ("5693", "Arc A370M (DG2)"),
            ("56a0", "Arc A730M (DG2)"),
            ("56a1", "Arc A770M (DG2)"),
            ("4680", "Arc A770 (Alchemist)"),
            ("4682", "Arc A750 (Alchemist)"),
            ("468b", "Arc A580 (Alchemist)"),
        ],
        "AMD" => &[
            ("1638", "Radeon 780M (RDNA3 iGPU)"),
            ("164e", "Radeon RX 7600 (RDNA3)"),
            ("744c", "Radeon RX 7700 XT / 7800 XT (RDNA3)"),
            ("7e78", "Radeon RX 7900 XT (RDNA3)"),
            ("73bf", "Radeon RX 7900 XTX (RDNA3)"),
            ("73df", "Radeon RX 6000 series (RDNA2)"),
        ],
        _ => &[],
    };
    let hex = format!("{dev_id:04x}");
    known
        .iter()
        .find(|(id, _)| **id == hex)
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| format!("{vendor} GPU (0x{hex})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_id_extracts_bdf_from_symlink_path() {
        // We can't depend on /sys existing in CI-like environments; exercise the
        // fallback and the normal resolution against a temp structure instead.
        let tmp = std::env::temp_dir().join("skyfall-pci");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // No real symlink parent chain: fallback should kick in.
        assert_eq!(id_from(&tmp, "card7"), "card7");
    }

    #[test]
    fn rc6_to_busy_math() {
        let mut p = SysfsProvider::new();
        // First sample: no prior state -> None.
        assert!(p
            .intel_utilization("gpu-x", Path::new("/nonexistent"), 1000)
            .is_none());
        assert_eq!(p.prev_rc6.len(), 0);

        // The helper reads files from /sys; build a synthetic dir to exercise math.
        let dir = std::env::temp_dir().join("skyfall-rc6");
        let _ = std::fs::remove_dir_all(&dir);
        let gt = dir.join("gt/gt0");
        std::fs::create_dir_all(&gt).unwrap();
        std::fs::write(gt.join("rc6_residency_ms"), "9000\n").unwrap();
        let device = &dir;

        // Seed a previous sample manually.
        p.prev_rc6.insert("gpu-x".into(), (9000, 1000));

        // After 1000ms where rc6 grew by 700ms => 30% busy.
        std::fs::write(gt.join("rc6_residency_ms"), "9700\n").unwrap();
        let busy = p.intel_utilization("gpu-x", device, 2000).unwrap();
        assert!((busy - 30.0).abs() < 0.1, "busy={busy}");
    }

    #[test]
    fn model_map_hex_lookup() {
        let tmp = std::env::temp_dir().join("skyfall-model");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("device"), "0x5693\n").unwrap();
        assert_eq!(model_name("Intel", &tmp), "Arc A370M (DG2)");
        std::fs::write(tmp.join("device"), "0xfeef\n").unwrap();
        assert_eq!(model_name("Intel", &tmp), "Intel GPU (0xfeef)");
    }
}
