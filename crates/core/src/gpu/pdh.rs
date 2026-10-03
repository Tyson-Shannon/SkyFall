//! Windows provider: PDH GPU counters + DXGI adapter enumeration.
//!
//! - **Utilization**: `\GPU Engine(*)\Utilization Percentage`, summed per adapter LUID
//!   matched against DXGI `AdapterLuid`. This is Task Manager's number (scheduler
//!   duty-cycle — same honest caveat as NVML's percent figure).
//! - **Memory**: `\GPU Adapter Memory\Dedicated Usage` per adapter ordinal; totals from
//!   DXGI `DedicatedVideoMemory`.
//! - **Temp / power / clocks**: not exposed publicly for AMD/Intel on Windows without
//!   vendor SDKs → `None` (documented N/A). NVIDIA GPUs are claimed by the NVML
//!   provider first (composite priority) and do expose these.
//!
//! Any PDH failure degrades this provider to an empty vec — never a crash. Notably,
//! missing `GPU Engine` countersets (`PDH_CSTATUS_NO_OBJECT`) just yield nothing.
//!
//! Handles in the `windows` crate (0.58) are plain `isize`; every PDH routine returns
//! a `u32` error code where 0 = `PDH_ERROR_SUCCESS`.

use super::GpuProvider;
use crate::metrics::GpuMetrics;

use std::collections::HashMap;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhExpandWildCardPathW,
    PdhGetFormattedCounterValue, PdhOpenQueryW, PDH_FMT, PDH_FMT_COUNTERVALUE,
};

const PDH_FMT_DOUBLE: u32 = 0x0000_0200;
const PDH_FMT_LARGE: u32 = 0x0000_0100;

const GPU_ENGINE_UTIL: &str = "\\GPU Engine(*)\\Utilization Percentage";
const GPU_ADAPTER_MEM: &str = "\\GPU Adapter Memory(*)\\Dedicated Usage";

pub struct WindowsPdhProvider;

impl WindowsPdhProvider {
    pub fn new() -> Self {
        WindowsPdhProvider
    }
}

struct Adapter {
    luid: windows::Win32::Foundation::LUID,
    name: String,
    memory_total: u64,
}

impl GpuProvider for WindowsPdhProvider {
    fn collect(&mut self) -> Vec<GpuMetrics> {
        let adapters = dxgi_adapters();
        if adapters.is_empty() {
            return Vec::new();
        }

        let util: HashMap<(u64, u64), f64> = enum_sum(GPU_ENGINE_UTIL, PDH_FMT_DOUBLE, |text| {
            super::instance::parse_gpu_engine(text).map(|inst| {
                (
                    (inst.luid_low, inst.luid_high),
                    0u8,
                    (|v: f64| v) as fn(f64) -> f64,
                )
            })
        });

        // Adapter ordinal -> dedicated bytes (instance "adapterN").
        let mem: HashMap<usize, f64> = enum_sum(GPU_ADAPTER_MEM, PDH_FMT_LARGE, |text| {
            let n = text.strip_prefix("adapter")?.parse::<usize>().ok()?;
            Some((n, 0u8, (|v: f64| v) as fn(f64) -> f64))
        });

        let mut out = Vec::with_capacity(adapters.len());
        for (idx, a) in adapters.iter().enumerate() {
            let key = (a.luid.LowPart as u64, a.luid.HighPart as u32 as u64);
            out.push(GpuMetrics {
                id: format!("luid:{:08x}{:08x}", a.luid.HighPart as u32, a.luid.LowPart),
                vendor: "windows".into(),
                model: if a.name.is_empty() {
                    format!("Adapter {}", idx)
                } else {
                    a.name.clone()
                },
                utilization: util.get(&key).copied().map(|v| v as f32),
                memory_total: Some(a.memory_total),
                memory_used: mem.get(&idx).map(|v| *v as u64),
                temperature: None,
                power_watts: None,
                clock_mhz: None,
            });
        }
        out
    }

    fn name(&self) -> &'static str {
        "pdh"
    }
}

/// Enumerate physical DXGI adapters (skipping the Microsoft software adapter).
fn dxgi_adapters() -> Vec<Adapter> {
    let mut out = Vec::new();
    let factory = unsafe { CreateDXGIFactory1::<IDXGIFactory1>() };
    let Ok(factory) = factory else {
        log::debug!("GPU: DXGI factory unavailable");
        return out;
    };
    let mut idx = 0u32;
    loop {
        let adapter = unsafe { factory.EnumAdapters1(idx) };
        let Ok(adapter) = adapter else {
            break;
        };
        idx += 1;
        let desc = unsafe { adapter.GetDesc1() };
        let Ok(desc) = desc else {
            continue;
        };
        let software = desc.VendorId == 0x1414 // Microsoft Basic Render Driver
            || desc.Flags != 0;
        if !software {
            out.push(Adapter {
                luid: desc.AdapterLuid,
                name: wide_to_string(&desc.Description),
                memory_total: desc.DedicatedVideoMemory as u64,
            });
        }
    }
    out
}

fn wide_to_string(ws: &[u16; 128]) -> String {
    let end = ws.iter().position(|&c| c == 0).unwrap_or(ws.len());
    String::from_utf16_lossy(&ws[..end])
}

/// Expand a wildcard PDH path and fold each concrete instance's formatted value into
/// a map. `key` maps an instance-path text to `(counts[key], _ignored, convert)`; rows
/// it can't map (aggregate `*` totals, unknown instances) are dropped.
fn enum_sum<K, F>(wildcard_text: &str, fmt: u32, key: F) -> HashMap<K, f64>
where
    K: std::hash::Hash + Eq,
    F: Fn(&str) -> Option<(K, u8, fn(f64) -> f64)>,
{
    let mut out = HashMap::new();

    let mut query: isize = 0;
    if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } != 0 || query == 0 {
        log::debug!("GPU: PdhOpenQueryW failed");
        return out;
    }
    let close = || unsafe {
        let _ = PdhCloseQuery(query);
    };

    let mut wild_wide: Vec<u16> = wildcard_text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let wildcard = PCWSTR(wild_wide.as_mut_ptr());

    let mut needed: u32 = 0;
    if unsafe { PdhExpandWildCardPathW(PCWSTR::null(), wildcard, PWSTR::null(), &mut needed, 0) }
        != 0
        || needed == 0
    {
        close();
        return out;
    }
    let mut buf = vec![0u16; needed as usize];
    let mut filled = needed;
    if unsafe {
        PdhExpandWildCardPathW(
            PCWSTR::null(),
            wildcard,
            PWSTR(buf.as_mut_ptr()),
            &mut filled,
            0,
        )
    } != 0
    {
        close();
        return out;
    }

    // Collect (handle, decoded instance text) for each concrete path.
    let mut pairs: Vec<(isize, String)> = Vec::new();
    for (p, text) in multi_sz(&buf) {
        let mut counter: isize = 0;
        if unsafe { PdhAddEnglishCounterW(query, p, 0, &mut counter) } == 0 {
            pairs.push((counter, text));
        }
    }
    if pairs.is_empty() {
        close();
        return out;
    }

    // Prime rate counters, then sample.
    unsafe {
        let _ = PdhCollectQueryData(query);
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    if unsafe { PdhCollectQueryData(query) } != 0 {
        close();
        return out;
    }

    for (counter, text) in pairs {
        let Some((k, _, conv)) = key(&text) else {
            continue;
        };
        let mut value = PDH_FMT_COUNTERVALUE::default();
        if unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT(fmt), None, &mut value) } == 0
            && value.CStatus == 0
        {
            let d = if fmt == PDH_FMT_LARGE {
                (unsafe { value.Anonymous.largeValue }) as f64
            } else {
                unsafe { value.Anonymous.doubleValue }
            };
            *out.entry(k).or_insert(0.0) += (conv)(d);
        }
    }
    close();
    out
}

/// Split a double-NUL-terminated wide multi-string, returning each entry as a
/// wide pointer (into `buf`) plus its decoded text.
fn multi_sz(buf: &[u16]) -> Vec<(PCWSTR, String)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < buf.len() && buf[start] != 0 {
        let mut end = start;
        while end < buf.len() && buf[end] != 0 {
            end += 1;
        }
        out.push((
            PCWSTR(buf.as_ptr().wrapping_add(start)),
            String::from_utf16_lossy(&buf[start..end]),
        ));
        start = end + 1;
    }
    out
}
