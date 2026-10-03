#[allow(dead_code)]
pub mod instance;
pub mod null;
pub mod nvml;
#[cfg(windows)]
pub mod pdh;
#[cfg(target_os = "linux")]
pub mod sysfs;

use std::collections::HashSet;

use crate::metrics::GpuMetrics;

/// A backend that reports live state for one or more GPUs.
pub trait GpuProvider: Send {
    /// Collect current metrics for all GPUs this provider owns.
    ///
    /// A provider must never panic or crash the collector: if a GPU vanished or the
    /// underlying API is temporarily unavailable, return an empty vec (or an error
    /// the caller can log and continue from).
    fn collect(&mut self) -> Vec<GpuMetrics>;

    /// Human-readable names of the vendors this provider handles.
    fn name(&self) -> &'static str;
}

/// Runs several providers in priority order and dedupes devices by `id`, so a GPU
/// claimed by a higher-priority provider (e.g. NVIDIA via NVML) is not re-reported
/// by a lower one.
pub struct CompositeProvider {
    providers: Vec<Box<dyn GpuProvider>>,
}

impl CompositeProvider {
    pub fn new(providers: Vec<Box<dyn GpuProvider>>) -> Self {
        CompositeProvider { providers }
    }

    pub fn from_defaults() -> Self {
        let mut providers: Vec<Box<dyn GpuProvider>> = vec![Box::new(nvml::NvmlProvider::new())];
        #[cfg(target_os = "linux")]
        providers.push(Box::new(sysfs::SysfsProvider::new()));
        #[cfg(windows)]
        providers.push(Box::new(pdh::WindowsPdhProvider::new()));
        CompositeProvider::new(providers)
    }
}

impl GpuProvider for CompositeProvider {
    fn collect(&mut self) -> Vec<GpuMetrics> {
        let mut out = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for provider in self.providers.iter_mut() {
            for g in provider.collect() {
                if !g.id.is_empty() && seen.insert(g.id.clone()) {
                    out.push(g);
                }
            }
        }
        // Deterministic ordering by id.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn name(&self) -> &'static str {
        "composite"
    }
}

/// Order in which providers are tried at startup. Higher priority wins for a GPU;
/// a GPU claimed by one provider is not re-probed by another.
pub const PROVIDER_PRIORITY: &[&str] = &["nvml", "pdh", "sysfs"];
