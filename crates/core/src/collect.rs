use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use sysinfo::{Components, Disks, Networks, ProcessesToUpdate, System};

use crate::config::Config;
use crate::gpu::GpuProvider;
use crate::metrics::{DiskMetrics, ProcessMetrics, Snapshot, SystemMetrics, TempMetrics};

/// Owns the system-info handles and per-snapshot state needed to produce deltas
/// (per-process disk I/O, network throughput).
pub struct Collector {
    sys: System,
    networks: Networks,
    disks: Disks,
    components: Components,
    gpu: Box<dyn GpuProvider>,
    cfg: Config,
    prev_proc_io: HashMap<u32, (u64, u64)>,
    prev_net: Option<(u64, u64)>,
    sample_start: Instant,
}

impl Collector {
    pub fn new(cfg: Config) -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        // Prime the process table so the first collect() has CPU/IO baselines.
        let _ = sys.refresh_processes(ProcessesToUpdate::All, true);
        Collector {
            sys,
            networks: Networks::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
            components: Components::new_with_refreshed_list(),
            gpu: Box::new(crate::gpu::CompositeProvider::from_defaults()),
            cfg,
            prev_proc_io: HashMap::new(),
            prev_net: None,
            sample_start: Instant::now(),
        }
    }

    /// Replace the GPU backend (NVML/PDH/sysfs providers land in phase 2).
    pub fn set_gpu_provider(&mut self, provider: Box<dyn GpuProvider>) {
        self.gpu = provider;
    }

    fn refreshed_full(&mut self) {
        self.sys.refresh_memory();
        self.sys.refresh_cpu_usage();
        let _ = self.sys.refresh_processes(ProcessesToUpdate::All, true);
        self.networks.refresh(true);
        self.disks.refresh(true);
        self.components.refresh(true);
    }

    /// Collect a fresh snapshot of the whole machine.
    pub fn collect(&mut self) -> Result<Snapshot> {
        let now = Instant::now();
        let dt_secs = (now - self.sample_start).as_secs_f32().max(0.001);
        self.sample_start = now;

        self.refreshed_full();

        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        let processes = self.collect_processes(dt_secs);
        let (net_rx_bps, net_tx_bps, net_rx_total, net_tx_total) = self.collect_network(dt_secs);

        let disks = self.collect_disks();
        let temps = self.collect_temps();
        let gpus = self.gpu.collect();

        let load = System::load_average();
        let snapshot = Snapshot {
            ts_ms,
            system: SystemMetrics {
                cpu_count: self.sys.cpus().len(),
                cpu_total: self.sys.global_cpu_usage(),
                cpus: if self.cfg.per_core_cpu {
                    self.sys.cpus().iter().map(|c| c.cpu_usage()).collect()
                } else {
                    Vec::new()
                },
                ram_total: self.sys.total_memory(),
                ram_used: self.sys.used_memory(),
                ram_percent: percent(self.sys.used_memory(), self.sys.total_memory()),
                swap_total: self.sys.total_swap(),
                swap_used: self.sys.used_swap(),
                load1: load.one,
                load5: load.five,
                load15: load.fifteen,
                uptime_secs: System::uptime(),
                net_rx_bps,
                net_tx_bps,
                net_rx_total,
                net_tx_total,
            },
            disks,
            temps,
            gpus,
            processes,
        };
        Ok(snapshot)
    }

    fn collect_processes(&mut self, dt_secs: f32) -> Vec<ProcessMetrics> {
        let mut out: Vec<ProcessMetrics> = Vec::new();
        let mut next_io: HashMap<u32, (u64, u64)> =
            HashMap::with_capacity(self.sys.processes().len().saturating_div(2).max(16));

        for (pid, proc) in self.sys.processes() {
            let key = pid.as_u32();
            let (disk_read_bps, disk_write_bps) = if self.cfg.per_process_io {
                let du = proc.disk_usage();
                let prev = self.prev_proc_io.get(&key).copied();
                next_io.insert(key, (du.total_read_bytes, du.total_written_bytes));
                match prev {
                    Some((pr, pw)) => (
                        delta_bps(du.total_read_bytes, pr, dt_secs),
                        delta_bps(du.total_written_bytes, pw, dt_secs),
                    ),
                    None => (0, 0),
                }
            } else {
                (0, 0)
            };

            out.push(ProcessMetrics {
                pid: key,
                name: proc.name().to_string_lossy().into_owned(),
                cpu_percent: proc.cpu_usage(),
                ram_kb: proc.memory() / 1024,
                disk_read_bps,
                disk_write_bps,
            });
        }

        if self.cfg.per_process_io {
            self.prev_proc_io = next_io;
        }

        out.sort_by(|a, b| {
            b.cpu_percent
                .total_cmp(&a.cpu_percent)
                .then(b.ram_kb.cmp(&a.ram_kb))
        });
        out.truncate(self.cfg.top_processes);
        out
    }

    fn collect_network(&mut self, dt_secs: f32) -> (u64, u64, u64, u64) {
        let mut rx_total = 0u64;
        let mut tx_total = 0u64;
        for (_iface, data) in self.networks.iter() {
            rx_total += data.total_received();
            tx_total += data.total_transmitted();
        }
        let (rx_bps, tx_bps) = match self.prev_net {
            Some((pr, pt)) => (
                delta_bps(rx_total, pr, dt_secs),
                delta_bps(tx_total, pt, dt_secs),
            ),
            None => (0, 0),
        };
        self.prev_net = Some((rx_total, tx_total));
        (rx_bps, tx_bps, rx_total, tx_total)
    }

    fn collect_disks(&self) -> Vec<DiskMetrics> {
        self.disks
            .list()
            .iter()
            .map(|d| {
                let used = d.total_space().saturating_sub(d.available_space());
                DiskMetrics {
                    name: d.name().to_string_lossy().into_owned(),
                    mount_point: d.mount_point().to_string_lossy().into_owned(),
                    total: d.total_space(),
                    used,
                    percent: percent(used, d.total_space()),
                }
            })
            .collect()
    }

    fn collect_temps(&self) -> Vec<TempMetrics> {
        self.components
            .list()
            .iter()
            .map(|c| TempMetrics {
                label: c.label().to_string(),
                temperature: c.temperature().unwrap_or(0.0),
            })
            .collect()
    }
}

fn delta_bps(cur: u64, prev: u64, dt_secs: f32) -> u64 {
    if cur >= prev {
        ((cur - prev) as f64 / dt_secs as f64) as u64
    } else {
        0
    }
}

fn percent(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        0.0
    } else {
        part as f32 / whole as f32 * 100.0
    }
}
