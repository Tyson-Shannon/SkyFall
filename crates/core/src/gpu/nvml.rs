//! NVIDIA provider backed by NVML.
//!
//! `nvml-wrapper` loads `libnvidia-ml`/`nvml.dll` at runtime, so on machines
//! without an NVIDIA GPU this provider is simply empty — never an error.

use super::GpuProvider;
use crate::metrics::GpuMetrics;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::Nvml;

pub struct NvmlProvider {
    nvml: Option<Nvml>,
}

impl NvmlProvider {
    pub fn new() -> Self {
        match Nvml::init() {
            Ok(nvml) => {
                log::info!("GPU: NVML initialized (NVIDIA present)");
                NvmlProvider { nvml: Some(nvml) }
            }
            Err(e) => {
                log::debug!("GPU: no NVIDIA NVML ({e}), using fallback providers");
                NvmlProvider { nvml: None }
            }
        }
    }
}

impl Default for NvmlProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuProvider for NvmlProvider {
    fn collect(&mut self) -> Vec<GpuMetrics> {
        let Some(nvml) = &self.nvml else {
            return Vec::new();
        };
        let Ok(count) = nvml.device_count() else {
            return Vec::new();
        };

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let Ok(dev) = nvml.device_by_index(i) else {
                continue;
            };
            let mut g = GpuMetrics {
                id: String::new(),
                vendor: "nvidia".into(),
                model: dev.name().unwrap_or_else(|_| "NVIDIA GPU".into()),
                utilization: None,
                memory_used: None,
                memory_total: None,
                temperature: None,
                power_watts: None,
                clock_mhz: None,
            };

            // Stable identity: PCI bus id (e.g. "0000:01:00.0").
            if let Ok(pci) = dev.pci_info() {
                let bytes = pci.bus_id.as_bytes();
                g.id = String::from_utf8_lossy(bytes)
                    .trim_end_matches('\0')
                    .to_string();
            }
            if g.id.is_empty() {
                g.id = format!("nvidia:{i}");
            }

            if let Ok(u) = dev.utilization_rates() {
                // API reports percent units directly.
                g.utilization = Some(u.gpu as f32);
            }
            if let Ok(mem) = dev.memory_info() {
                g.memory_used = Some(mem.used);
                g.memory_total = Some(mem.total);
            }
            if let Ok(t) = dev.temperature(TemperatureSensor::Gpu) {
                g.temperature = Some(t as f32);
            }
            if let Ok(mw) = dev.power_usage() {
                g.power_watts = Some(mw as f32 / 1_000.0);
            }
            if let Ok(mhz) = dev.clock_info(Clock::Graphics) {
                g.clock_mhz = Some(mhz as f32);
            }

            out.push(g);
        }
        out
    }

    fn name(&self) -> &'static str {
        "nvml"
    }
}
