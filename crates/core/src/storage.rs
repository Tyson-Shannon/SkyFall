use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::metrics::Snapshot;

#[cfg(test)]
use crate::metrics::{DiskMetrics, GpuMetrics, ProcessMetrics, SystemMetrics, TempMetrics};

/// Primary time series the UI charts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct History {
    pub system: Vec<SystemPoint>,
    pub gpus: Vec<GpuSeries>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemPoint {
    pub ts: i64,
    pub cpu: f32,
    pub ram: f32,
    pub load1: f64,
    pub rx_bps: u64,
    pub tx_bps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuSeries {
    pub id: String,
    pub model: String,
    pub points: Vec<GpuPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuPoint {
    pub ts: i64,
    pub util: Option<f32>,
    pub temp: Option<f32>,
    pub mem_used: Option<u64>,
}

/// One alert from the feed, with its (nullable) LLM narrative (phase 5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRecord {
    pub id: i64,
    pub ts: i64,
    pub severity: String,
    pub kind: String,
    pub scope: String,
    pub message: String,
    pub dossier: Option<String>,
    pub headline: Option<String>,
    pub root_cause: Option<String>,
    pub explanation: Option<String>,
    pub recommended_action: Option<String>,
    pub confidence: Option<f64>,
}

/// Wraps the SQLite history store. Opaque to the rest of the crate so the schema
/// can evolve privately (see ROADMAP: rollups in phase 8).
pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let path = expand_tilde(path);
        if path != Path::new(":memory:") {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)
                    .with_context(|| format!("creating db dir {}", dir.display()))?;
            }
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening database {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        Self::init(&conn)?;
        Ok(Store { conn })
    }

    fn init(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS system_metrics (
                ts         INTEGER PRIMARY KEY,
                cpu_total  REAL,
                ram_pct    REAL,
                ram_used   INTEGER,
                swap_used  INTEGER,
                load1      REAL,
                load5      REAL,
                load15     REAL,
                net_rx_bps INTEGER,
                net_tx_bps INTEGER
            );

            CREATE TABLE IF NOT EXISTS process_metrics (
                ts         INTEGER,
                pid        INTEGER,
                name       TEXT,
                cpu        REAL,
                ram_kb     INTEGER,
                disk_r_bps INTEGER,
                disk_w_bps INTEGER,
                PRIMARY KEY (ts, pid)
            ) WITHOUT ROWID;

            CREATE INDEX IF NOT EXISTS idx_process_metrics_pid_ts
                ON process_metrics (pid, ts);

            CREATE TABLE IF NOT EXISTS disk_metrics (
                ts     INTEGER,
                mount  TEXT,
                total  INTEGER,
                used   INTEGER,
                pct    REAL,
                PRIMARY KEY (ts, mount)
            ) WITHOUT ROWID;

            CREATE TABLE IF NOT EXISTS gpu_metrics (
                ts         INTEGER,
                idx        INTEGER,
                id         TEXT,
                vendor     TEXT,
                model      TEXT,
                util       REAL,
                mem_used   INTEGER,
                mem_total  INTEGER,
                temp       REAL,
                power      REAL,
                clock_mhz  REAL,
                PRIMARY KEY (ts, idx)
            ) WITHOUT ROWID;

            CREATE TABLE IF NOT EXISTS alerts (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                ts       INTEGER,
                severity TEXT,
                kind     TEXT,
                scope    TEXT,
                message  TEXT,
                dossier  TEXT
            );

            CREATE TABLE IF NOT EXISTS narratives (
                id                INTEGER PRIMARY KEY AUTOINCREMENT,
                alert_id          INTEGER,
                ts                INTEGER,
                severity          TEXT,
                headline          TEXT,
                root_cause        TEXT,
                explanation       TEXT,
                recommended_action TEXT,
                confidence        REAL
            );

            CREATE INDEX IF NOT EXISTS idx_narratives_alert_id
                ON narratives (alert_id);
            "#,
        )?;
        // Phase 6 migration: add root_cause to narratives created by older builds.
        if !column_exists(conn, "narratives", "root_cause") {
            conn.execute_batch("ALTER TABLE narratives ADD COLUMN root_cause TEXT;")?;
        }
        Ok(())
    }

    /// Persist one snapshot atomically.
    pub fn write_snapshot(&mut self, snap: &Snapshot) -> Result<()> {
        let conn = &mut self.conn;
        let tx = conn.transaction()?;
        insert_system(&tx, snap)?;
        insert_disks(&tx, snap)?;
        insert_gpus(&tx, snap)?;
        insert_processes(&tx, snap)?;
        tx.commit().context("committing snapshot")
    }

    /// Latest stored snapshot timestamp, if any.
    pub fn last_ts(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MAX(ts) FROM system_metrics", [], |r| r.get(0))?)
    }

    /// Checkpoint the WAL into the main file (used on graceful shutdown).
    pub fn finalize(&self) {
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }

    /// Persist one sidecar anomaly event (phase 4).
    pub fn insert_alert(
        &mut self,
        ts_ms: i64,
        severity: &str,
        kind: &str,
        scope: &str,
        message: &str,
        dossier: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO alerts (ts, severity, kind, scope, message, dossier)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![ts_ms, severity, kind, scope, message, dossier],
        )?;
        Ok(())
    }

    /// Find the alert id for a (ts, kind, scope) triple — used to attach narratives.
    pub fn alert_id_for(&self, ts_ms: i64, kind: &str, scope: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM alerts WHERE ts = ?1 AND kind = ?2 AND scope = ?3
                 ORDER BY id DESC LIMIT 1",
                params![ts_ms, kind, scope],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Persist an LLM narrative for a stored alert (phase 5).
    pub fn insert_narrative(
        &mut self,
        alert_id: i64,
        ts_ms: i64,
        verdict: &crate::sidecar::Verdict,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO narratives
             (alert_id, ts, severity, headline, root_cause, explanation, recommended_action, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                alert_id, ts_ms, verdict.severity, verdict.headline, verdict.root_cause,
                verdict.explanation, verdict.recommended_action, verdict.confidence
            ],
        )?;
        Ok(())
    }

    /// Recent alerts joined with their (nullable) LLM narratives, newest first.
    pub fn recent_alerts(&self, limit: u32) -> Result<Vec<AlertRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.id, a.ts, a.severity, a.kind, a.scope, a.message, a.dossier,
                    n.headline, n.root_cause, n.explanation, n.recommended_action, n.confidence
             FROM alerts a
             LEFT JOIN narratives n ON n.alert_id = a.id
             ORDER BY a.ts DESC, a.id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(AlertRecord {
                id: r.get(0)?,
                ts: r.get(1)?,
                severity: r.get(2)?,
                kind: r.get(3)?,
                scope: r.get(4)?,
                message: r.get(5)?,
                dossier: r.get(6)?,
                headline: r.get(7)?,
                root_cause: r.get(8)?,
                explanation: r.get(9)?,
                recommended_action: r.get(10)?,
                confidence: r.get(11)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Downsampled time series for the charts, bucketed by `bucket_ms`.
    pub fn history(&self, since_ms: i64, bucket_ms: i64) -> Result<History> {
        let mut system = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT (ts / ?2) * ?2 AS t, AVG(cpu_total), AVG(ram_pct), AVG(load1),
                    AVG(net_rx_bps), AVG(net_tx_bps)
             FROM system_metrics
             WHERE ts >= ?1
             GROUP BY t
             ORDER BY t",
        )?;
        let rows = stmt.query_map(params![since_ms, bucket_ms], |r| {
            Ok(SystemPoint {
                ts: r.get(0)?,
                cpu: r.get(1)?,
                ram: r.get(2)?,
                load1: r.get(3)?,
                rx_bps: r.get::<_, f64>(4)? as u64,
                tx_bps: r.get::<_, f64>(5)? as u64,
            })
        })?;
        for row in rows {
            system.push(row?);
        }

        // GPU series, grouped per device id (stable across boots).
        let mut gpus: Vec<GpuSeries> = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT (ts / ?2) * ?2 AS t, MIN(id), MIN(model), AVG(util), AVG(temp), AVG(mem_used)
             FROM gpu_metrics
             WHERE ts >= ?1
             GROUP BY id, t
             ORDER BY id, t",
        )?;
        let rows = stmt.query_map(params![since_ms, bucket_ms], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                GpuPoint {
                    ts: r.get(0)?,
                    util: r.get(3)?,
                    temp: r.get(4)?,
                    mem_used: r.get::<_, Option<f64>>(5)?.map(|v| v as u64),
                },
            ))
        })?;
        for row in rows {
            let (id, model, point) = row?;
            if gpus.last().map(|g| g.id.as_str()) != Some(id.as_str()) {
                gpus.push(GpuSeries {
                    id,
                    model,
                    points: Vec::new(),
                });
            }
            gpus.last_mut().expect("just pushed").points.push(point);
        }
        Ok(History { system, gpus })
    }
}

/// Resolve a leading `~/` to the current user's home directory (Win: USERPROFILE).
/// True when `table` already has a `column` (idempotent migration guard).
fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return false;
    };
    let names: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(1)) {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => return false,
    };
    names.iter().any(|name| name == column)
}

pub(crate) fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
        {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

fn insert_system(tx: &rusqlite::Transaction, s: &Snapshot) -> Result<()> {
    let sys = &s.system;
    tx.execute(
        "INSERT OR REPLACE INTO system_metrics
         (ts, cpu_total, ram_pct, ram_used, swap_used, load1, load5, load15, net_rx_bps, net_tx_bps)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            s.ts_ms,
            sys.cpu_total,
            sys.ram_percent,
            sys.ram_used as i64,
            sys.swap_used as i64,
            sys.load1,
            sys.load5,
            sys.load15,
            sys.net_rx_bps as i64,
            sys.net_tx_bps as i64
        ],
    )?;
    Ok(())
}

fn insert_disks(tx: &rusqlite::Transaction, s: &Snapshot) -> Result<()> {
    let mut stmt = tx.prepare(
        "INSERT OR REPLACE INTO disk_metrics (ts, mount, total, used, pct)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for d in &s.disks {
        stmt.execute(params![
            s.ts_ms,
            d.mount_point,
            d.total as i64,
            d.used as i64,
            d.percent
        ])?;
    }
    Ok(())
}

fn insert_gpus(tx: &rusqlite::Transaction, s: &Snapshot) -> Result<()> {
    let mut stmt = tx.prepare(
        "INSERT OR REPLACE INTO gpu_metrics
         (ts, idx, id, vendor, model, util, mem_used, mem_total, temp, power, clock_mhz)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for (idx, g) in s.gpus.iter().enumerate() {
        stmt.execute(params![
            s.ts_ms,
            idx as i64,
            g.id,
            g.vendor,
            g.model,
            g.utilization,
            g.memory_used.map(|v| v as i64),
            g.memory_total.map(|v| v as i64),
            g.temperature,
            g.power_watts,
            g.clock_mhz
        ])?;
    }
    Ok(())
}

fn insert_processes(tx: &rusqlite::Transaction, s: &Snapshot) -> Result<()> {
    let mut stmt = tx.prepare(
        "INSERT OR REPLACE INTO process_metrics
         (ts, pid, name, cpu, ram_kb, disk_r_bps, disk_w_bps)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for p in &s.processes {
        stmt.execute(params![
            s.ts_ms,
            p.pid as i64,
            p.name,
            p.cpu_percent,
            p.ram_kb as i64,
            p.disk_read_bps as i64,
            p.disk_write_bps as i64
        ])?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn tests_sample_snapshot(ts: i64) -> Snapshot {
    Snapshot {
        ts_ms: ts,
        system: SystemMetrics {
            cpu_count: 8,
            cpu_total: 42.5,
            cpus: vec![41.0, 44.0],
            ram_total: 16 * 1024 * 1024 * 1024,
            ram_used: 8 * 1024 * 1024 * 1024,
            ram_percent: 50.0,
            swap_total: 2 * 1024 * 1024 * 1024,
            swap_used: 1024 * 1024 * 1024,
            load1: 1.0,
            load5: 0.8,
            load15: 0.6,
            uptime_secs: 1000,
            net_rx_bps: 1_000_000,
            net_tx_bps: 2_000_000,
            net_rx_total: 100_000_000,
            net_tx_total: 50_000_000,
        },
        disks: vec![DiskMetrics {
            name: "nvme0n1".into(),
            mount_point: "/".into(),
            total: 512 * 1024 * 1024 * 1024,
            used: 256 * 1024 * 1024 * 1024,
            percent: 50.0,
        }],
        temps: vec![TempMetrics {
            label: "CPU Package".into(),
            temperature: 61.0,
        }],
        gpus: vec![GpuMetrics {
            id: "0000:01:00.0".into(),
            vendor: "nvidia".into(),
            model: "RTX Test".into(),
            utilization: Some(33.0),
            memory_used: Some(1 << 30),
            memory_total: Some(8 << 30),
            temperature: Some(55.0),
            power_watts: Some(120.0),
            clock_mhz: Some(1800.0),
        }],
        processes: vec![ProcessMetrics {
            pid: 1234,
            name: "testproc".into(),
            cpu_percent: 12.5,
            ram_kb: 65536,
            disk_read_bps: 1000,
            disk_write_bps: 2000,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn sample_snapshot(ts: i64) -> Snapshot {
        tests_sample_snapshot(ts)
    }

    #[test]
    fn in_memory_write_and_read_back() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.write_snapshot(&sample_snapshot(1_000)).unwrap();
        store.write_snapshot(&sample_snapshot(2_000)).unwrap();

        assert_eq!(store.last_ts().unwrap(), Some(2_000));

        let sys_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM system_metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sys_count, 2);

        let proc_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM process_metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(proc_count, 2);
    }

    #[test]
    fn expands_tilde_to_home() {
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", "/tmp/fakehome");
        let expanded = expand_tilde(Path::new("~/local/share/skyfall.db"));
        assert_eq!(
            expanded,
            PathBuf::from("/tmp/fakehome/local/share/skyfall.db")
        );
        // Non-tilde paths pass through untouched.
        assert_eq!(
            expand_tilde(Path::new("/etc/skyfall.db")),
            PathBuf::from("/etc/skyfall.db")
        );
        assert_eq!(
            expand_tilde(Path::new("relative.db")),
            PathBuf::from("relative.db")
        );
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn gpu_fields_allow_null() {
        let mut snap = sample_snapshot(1_000);
        snap.gpus[0].utilization = None;
        snap.gpus[0].temperature = None;
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.write_snapshot(&snap).unwrap();
        let util: Option<f32> = store
            .conn
            .query_row("SELECT util FROM gpu_metrics", [], |r| r.get(0))
            .unwrap();
        assert_eq!(util, None);
    }

    #[test]
    fn alerts_roundtrip() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store
            .insert_alert(1_234, "high", "rule", "global", "CPU hot", "context")
            .unwrap();
        let (sev, msg): (String, String) = store
            .conn
            .query_row("SELECT severity, message FROM alerts", [], |r| {
                r.get(0)
                    .and_then(|s: String| r.get(1).map(|m: String| (s, m)))
            })
            .unwrap();
        assert_eq!(sev, "high");
        assert_eq!(msg, "CPU hot");
    }

    #[test]
    fn narrative_attaches_to_alert_and_shows_in_feed() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store
            .insert_alert(1_234, "high", "rule", "global", "CPU hot", "")
            .unwrap();
        store
            .insert_alert(9_999, "low", "baseline", "global", "nothing", "")
            .unwrap();
        let id = store
            .alert_id_for(1_234, "rule", "global")
            .unwrap()
            .expect("alert id");
        store
            .insert_narrative(
                id,
                1_900,
                &crate::sidecar::Verdict {
                    severity: "critical".into(),
                    headline: "CPU is wildly hot".into(),
                    root_cause: "rustc compiling many crates (CPU + disk writes rising)".into(),
                    explanation: "packages throttle".into(),
                    recommended_action: "clear fans".into(),
                    confidence: 0.9,
                },
            )
            .unwrap();

        let feed = store.recent_alerts(10).unwrap();
        assert_eq!(feed.len(), 2);
        assert_eq!(feed[0].message, "nothing"); // newest first (ts 9999)
        let narrated = feed.iter().find(|a| a.message == "CPU hot").unwrap();
        assert_eq!(narrated.headline.as_deref(), Some("CPU is wildly hot"));
        assert_eq!(
            narrated.root_cause.as_deref(),
            Some("rustc compiling many crates (CPU + disk writes rising)")
        );
        assert_eq!(narrated.recommended_action.as_deref(), Some("clear fans"));
        let unnarated = feed[0].clone();
        assert!(unnarated.headline.is_none());
        assert!(unnarated.root_cause.is_none());
    }

    #[test]
    fn migrates_legacy_narratives_table_with_root_cause() {
        // Simulate a pre-phase-6 database: narratives without a root_cause column.
        let dir = std::env::temp_dir().join(format!("skyfall-mig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("legacy.db");
        let _ = std::fs::remove_file(&path);
        {
            let legacy = Connection::open(&path).unwrap();
            legacy
                .execute_batch(
                    "CREATE TABLE narratives (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    alert_id INTEGER, ts INTEGER, severity TEXT,
                    headline TEXT, explanation TEXT, recommended_action TEXT, confidence REAL
                 );",
                )
                .unwrap();
            assert!(!column_exists(&legacy, "narratives", "root_cause"));
        }
        // Opening it through Store must add the column, idempotently.
        let mut store = Store::open(&path).unwrap();
        let has = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('narratives') WHERE name='root_cause'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(has, 1);
        // And a narrative with a root cause round-trips.
        store
            .insert_alert(5_000, "high", "rule", "global", "CPU hot", "")
            .unwrap();
        let id = store
            .alert_id_for(5_000, "rule", "global")
            .unwrap()
            .unwrap();
        store
            .insert_narrative(
                id,
                5_100,
                &crate::sidecar::Verdict {
                    severity: "high".into(),
                    headline: "h".into(),
                    root_cause: "rustc".into(),
                    explanation: "e".into(),
                    recommended_action: "a".into(),
                    confidence: 0.5,
                },
            )
            .unwrap();
        let feed = store.recent_alerts(1).unwrap();
        assert_eq!(feed[0].root_cause.as_deref(), Some("rustc"));
        // Re-opening must not error or duplicate the column.
        drop(store);
        let _again = Store::open(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
