//! Python anomaly-detection sidecar (phase 4).
//!
//! Spawns `python3 -m skyfall_ai` (the package under `python/sidecar`), feeds it
//! each collected snapshot as one NDJSON line on stdin, and drains AnomalyEvent
//! objects from stdout via a reader thread. Failures to spawn, feed, or run the
//! child never crash the collector — the sidecar simply disables itself and logs
//! a warning.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::metrics::Snapshot;
use crate::storage::expand_tilde;

/// Expand a config path for handing to the sidecar child (handles `~`).
fn expand_tilde_for_child(path: &std::path::Path) -> PathBuf {
    expand_tilde(path)
}

/// One anomaly event emitted by the sidecar (mirrors the Python AnomalyEvent).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnomalyEvent {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub ts_ms: i64,
    /// `rule` | `baseline` | `creep` | `ml`
    pub kind: String,
    pub metric: String,
    pub scope: String,
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub delta_vs_baseline: Option<f64>,
    #[serde(default)]
    pub dossier: Option<String>,
}

/// An LLM verdict attached to an alert (phase 5). Mirrors the Python narrator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Verdict {
    pub severity: String,
    pub headline: String,
    /// Most likely cause of the anomaly, inferred from the dossier's causal
    /// context (phase 6). Optional so older/partial verdicts still parse.
    #[serde(default)]
    pub root_cause: String,
    #[serde(default)]
    pub explanation: String,
    #[serde(default)]
    pub recommended_action: String,
    #[serde(default)]
    pub confidence: f64,
}

/// A narrated anomaly (phase 5): the LLM's structured write-up of an alert.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Narrative {
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Time the narrative was generated, unix ms.
    pub ts_ms: i64,
    /// `ts_ms` of the AnomalyEvent this narrates (links to the alerts row).
    pub alert_ts_ms: i64,
    pub alert_kind: String,
    pub alert_scope: String,
    #[serde(default)]
    pub alert_message: String,
    pub verdict: Verdict,
}

/// Everything the sidecar surface during one drain.
#[derive(Debug, Default)]
pub struct SidecarOutcome {
    pub events: Vec<AnomalyEvent>,
    pub narratives: Vec<Narrative>,
}

/// Owns the child process and the reader thread feeding its stdout messages.
pub struct Sidecar {
    child: Option<Child>,
    writer: Option<BufWriter<ChildStdin>>,
    rx: Option<Receiver<String>>,
    enabled: bool,
}

impl Sidecar {
    /// Spawn the sidecar according to `cfg.ai`. Any failure disables it.
    pub fn new(cfg: &Config) -> Self {
        if !cfg.ai.enabled {
            log::info!("ai sidecar disabled (ai.enabled = false)");
            return Self::disabled();
        }
        match spawn(cfg) {
            Ok(sc) => {
                log::info!(
                    "ai sidecar spawned ({} -m {})",
                    cfg.ai.command,
                    cfg.ai.module
                );
                sc
            }
            Err(e) => {
                log::warn!("ai sidecar unavailable, alerts disabled: {e:#}");
                Self::disabled()
            }
        }
    }

    fn disabled() -> Self {
        Sidecar {
            child: None,
            writer: None,
            rx: None,
            enabled: false,
        }
    }

    /// Detach from a dead/broken child so subsequent calls no-op.
    pub fn set_disabled(&mut self) {
        self.child = None;
        self.writer = None;
        self.rx = None;
        self.enabled = false;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// True while the child is still alive.
    pub fn running(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(Some(_)) => {
                self.set_disabled();
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Stream one snapshot to the sidecar. No-op when disabled.
    pub fn feed(&mut self, snap: &Snapshot) -> Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        let mut v = serde_json::to_value(snap)?;
        v["type"] = serde_json::Value::String("snapshot".to_string());
        serde_json::to_writer(&mut *writer, &v)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }

    /// Collect the AnomalyEvents and Narratives the sidecar emitted since last drained.
    pub fn drain(&mut self) -> SidecarOutcome {
        let mut out = SidecarOutcome::default();
        let Some(rx) = self.rx.as_ref() else {
            return out;
        };
        loop {
            match rx.try_recv() {
                Ok(line) => match parse_msg(&line) {
                    WireMsg::Hello => log::info!("ai sidecar ready: {}", line),
                    WireMsg::Event(ev) => out.events.push(ev),
                    WireMsg::Narrative(n) => out.narratives.push(n),
                    WireMsg::Other => log::debug!("ai sidecar: {line}"),
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    log::warn!("ai sidecar exited");
                    self.rx = None;
                    break;
                }
            }
        }
        out
    }

    /// Ask the sidecar to stop cleanly and reap it.
    pub fn shutdown(&mut self) {
        if self.enabled {
            if let Some(writer) = self.writer.as_mut() {
                let _ = writer.write_all(b"{\"type\":\"shutdown\"}\n");
                let _ = writer.flush();
            }
        }
        self.writer = None;
        if let Some(mut child) = self.child.take() {
            match child.wait() {
                Ok(status) => log::info!("ai sidecar exited: {status}"),
                Err(e) => log::warn!("ai sidecar wait failed: {e}"),
            }
        }
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum WireMsg {
    Hello,
    Event(AnomalyEvent),
    Narrative(Narrative),
    Other,
}

fn parse_msg(line: &str) -> WireMsg {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return WireMsg::Other;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("hello") => WireMsg::Hello,
        Some("event") => match serde_json::from_value::<AnomalyEvent>(v) {
            Ok(ev) => WireMsg::Event(ev),
            Err(_) => WireMsg::Other,
        },
        Some("narrative") => match serde_json::from_value::<Narrative>(v) {
            Ok(n) => WireMsg::Narrative(n),
            Err(_) => WireMsg::Other,
        },
        _ => WireMsg::Other,
    }
}

/// Locate the `python/sidecar` package directory.
///
/// Resolution order: `SKYFALL_AI_DIR` env override, then the repo-relative
/// `python/sidecar` under the current working directory. If neither exists we
/// still launch the module on the system `PYTHONPATH` (it may be pip-installed).
fn sidecar_python_path() -> PathBuf {
    if let Ok(dir) = std::env::var("SKYFALL_AI_DIR") {
        return PathBuf::from(dir);
    }
    std::env::current_dir()
        .unwrap_or_default()
        .join("python/sidecar")
}

fn spawn(cfg: &Config) -> Result<Sidecar> {
    let py_path = sidecar_python_path();

    let mut cmd = Command::new(&cfg.ai.command);
    cmd.arg("-m").arg(&cfg.ai.module);
    // Always tell the child where its config lives, even when the user never set
    // `sidecar_config`: Settings writes the narrator on/off + model there, so
    // leaving the flag off made those settings unsaveable in practice. The
    // sidecar falls back to its packaged config when this file does not exist yet.
    cmd.arg("--config").arg(cfg.ai.config_path());
    // Give the sidecar read-only access to the history DB so it can attach
    // trends / co-moving metrics / resource hogs to each alert's dossier.
    let db = expand_tilde_for_child(&cfg.storage_path);
    cmd.arg("--db").arg(db);

    let mut dirs = vec![py_path];
    if let Ok(prev) = std::env::var("PYTHONPATH") {
        dirs.extend(std::env::split_paths(&prev));
    }
    let py_path_env = std::env::join_paths(dirs).expect("joining PYTHONPATH");
    cmd.env("PYTHONPATH", py_path_env);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning {} -m {}: {e}", cfg.ai.command, cfg.ai.module))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout).lines();
        for line in reader.map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    Ok(Sidecar {
        child: Some(child),
        writer: Some(BufWriter::new(stdin)),
        rx: Some(rx),
        enabled: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event_messages() {
        let line = r#"{"type":"event","ts_ms":1234,"kind":"rule","metric":"system.cpu_total","scope":"global","severity":"medium","message":"CPU hot","delta_vs_baseline":null,"dossier":""}"#;
        match parse_msg(line) {
            WireMsg::Event(ev) => {
                assert_eq!(ev.ts_ms, 1234);
                assert_eq!(ev.kind, "rule");
                assert_eq!(ev.message, "CPU hot");
            }
            _ => panic!("expected event"),
        }
    }

    #[test]
    fn parses_narrative_messages() {
        let line = r#"{"type":"narrative","ts_ms":2000,"alert_ts_ms":1234,"alert_kind":"rule","alert_scope":"global","alert_message":"CPU hot","verdict":{"severity":"high","headline":"CPU is wildly hot","root_cause":"rustc compiling many crates","explanation":"packages throttle","recommended_action":"clear fans","confidence":0.9}}"#;
        match parse_msg(line) {
            WireMsg::Narrative(n) => {
                assert_eq!(n.alert_ts_ms, 1234);
                assert_eq!(n.verdict.severity, "high");
                assert_eq!(n.verdict.root_cause, "rustc compiling many crates");
                assert_eq!(n.verdict.confidence, 0.9);
            }
            _ => panic!("expected narrative"),
        }
    }

    #[test]
    fn parses_hello_and_junk() {
        assert!(matches!(
            parse_msg(r#"{"type":"hello","version":"0.1.0","detectors":{}}"#),
            WireMsg::Hello
        ));
        assert!(matches!(parse_msg("not json"), WireMsg::Other));
        assert!(matches!(parse_msg(r#"{"type":"nope"}"#), WireMsg::Other));
    }

    #[test]
    fn disabled_sidecar_noops() {
        let mut sc = Sidecar::disabled();
        assert!(!sc.is_enabled());
        assert!(!sc.running());
        let out = sc.drain();
        assert!(out.events.is_empty() && out.narratives.is_empty());
        assert!(sc.feed(&crate::storage::tests_sample_snapshot(1)).is_ok());
        sc.shutdown();
    }
}
