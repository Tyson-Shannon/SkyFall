use serde::{Deserialize, Serialize};

/// One full observation of the system at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Unix milliseconds.
    pub ts_ms: i64,
    pub system: SystemMetrics,
    pub disks: Vec<DiskMetrics>,
    pub temps: Vec<TempMetrics>,
    pub gpus: Vec<GpuMetrics>,
    pub processes: Vec<ProcessMetrics>,
}

impl Snapshot {
    pub fn cpu_total(&self) -> f32 {
        self.system.cpu_total
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    /// Number of logical CPUs.
    pub cpu_count: usize,
    /// Global CPU usage percent 0-100.
    pub cpu_total: f32,
    /// Per-core CPU usage percent (only if enabled).
    pub cpus: Vec<f32>,
    /// RAM totals in bytes.
    pub ram_total: u64,
    pub ram_used: u64,
    pub ram_percent: f32,
    /// Swap totals in bytes.
    pub swap_total: u64,
    pub swap_used: u64,
    /// 1/5/15-minute load averages.
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    /// System uptime in seconds.
    pub uptime_secs: u64,
    /// Instantaneous network throughput, bytes/sec.
    pub net_rx_bps: u64,
    pub net_tx_bps: u64,
    /// Cumulative bytes since collection start.
    pub net_rx_total: u64,
    pub net_tx_total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskMetrics {
    pub name: String,
    pub mount_point: String,
    pub total: u64,
    pub used: u64,
    pub percent: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempMetrics {
    pub label: String,
    pub temperature: f32,
}

/// A single GPU's live state. Fields marked `Option` are reported as N/A on
/// backends that cannot expose them (see ROADMAP gotchas).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuMetrics {
    /// Stable identity across boots where available (e.g. PCI BDF); falls back to
    /// vendor + index.
    pub id: String,
    pub vendor: String,
    pub model: String,
    /// Compute/duty-cycle utilization percent 0-100.
    pub utilization: Option<f32>,
    pub memory_used: Option<u64>,
    pub memory_total: Option<u64>,
    pub temperature: Option<f32>,
    pub power_watts: Option<f32>,
    /// Current core/engine clock in MHz (best available engine).
    pub clock_mhz: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessMetrics {
    pub pid: u32,
    /// Statically linked basename; falls back to the executable path.
    pub name: String,
    pub cpu_percent: f32,
    pub ram_kb: u64,
    /// Per-second read/write since the previous snapshot.
    pub disk_read_bps: u64,
    pub disk_write_bps: u64,
}
