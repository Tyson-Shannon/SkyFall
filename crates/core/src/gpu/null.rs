//! Placeholder GPU provider. Real providers (NVML, Windows PDH, Linux sysfs/rocmsmi)
//! land in phase 2 of the ROADMAP. This keeps the core compiling without depending on
//! vendor SDKs and gives the collector a stable, honest "no GPU backend" mode.

use super::GpuProvider;
use crate::metrics::GpuMetrics;

/// Reports no GPUs. Used until the vendor providers ship or when none apply.
#[derive(Debug, Default)]
pub struct NullGpuProvider;

impl GpuProvider for NullGpuProvider {
    fn collect(&mut self) -> Vec<GpuMetrics> {
        Vec::new()
    }

    fn name(&self) -> &'static str {
        "null"
    }
}
