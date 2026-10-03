//! Background collector loop (phase 6).
//!
//! Wraps the collection → storage → sidecar pipeline in a single self-contained
//! thread running its own tokio runtime, so a shell (CLI or the Tauri app) can
//! start it, read from it, hot-swap configuration, and stop it without owning
//! the async machinery.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{broadcast, watch};

use crate::collect::Collector;
use crate::config::Config;
use crate::sidecar::{AnomalyEvent, Narrative, Sidecar};
use crate::storage::Store;
use crate::VERSION;

/// Something the shell may want to act on (desktop notifications, …).
#[derive(Debug, Clone)]
pub enum AppEvent {
    Alert(AnomalyEvent),
    Narrative(Narrative),
}

/// Handle to a running collector loop.
pub struct Runner {
    /// Shared store for read commands (history / alerts).
    pub store: Arc<Mutex<Store>>,
    /// Live config: the loop re-reads it each tick; the shell swaps + saves it.
    pub cfg: Arc<Mutex<Config>>,
    /// Where config changes are persisted.
    pub cfg_path: PathBuf,
    /// Latest-snapshot feed: the shell streams these to the UI.
    pub latest: watch::Receiver<Option<Arc<crate::metrics::Snapshot>>>,
    /// Alert/narrative stream for the shell's notification layer.
    pub events: broadcast::Receiver<AppEvent>,
    /// When this runner started, for the UI's uptime readout.
    pub started: Instant,
    stop_tx: watch::Sender<()>,
    reload_tx: watch::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
    outcome: Arc<Mutex<Option<Result<()>>>>,
}

impl Runner {
    /// Spawn the loop on a dedicated thread (own tokio runtime). Storage failure
    /// surfaces synchronously so callers never bind to a broken DB.
    pub fn spawn(cfg: Config) -> Result<Runner> {
        let store = Arc::new(Mutex::new(Store::open(&cfg.storage_path).map_err(|e| {
            anyhow::anyhow!("opening storage {}: {e:#}", cfg.storage_path.display())
        })?));
        let (latest_tx, latest) = watch::channel::<Option<Arc<crate::metrics::Snapshot>>>(None);
        let (events_tx, events) = broadcast::channel(256);
        let (stop_tx, _) = watch::channel(());
        let (reload_tx, _) = watch::channel(());
        let cfg_path = cfg.effective_path();
        let cfg = Arc::new(Mutex::new(cfg));
        let outcome: Arc<Mutex<Option<Result<()>>>> = Arc::new(Mutex::new(None));

        let cfg_t = cfg.clone();
        let store_t = store.clone();
        let latest_t = latest_tx;
        let events_t = events_tx;
        // The loop needs its own subscription; the runner keeps the senders so
        // `stop()` / `reload()` can poke it later.
        let stop_t = stop_tx.subscribe();
        let reload_t = reload_tx.subscribe();
        let outcome_t = outcome.clone();
        let handle = std::thread::Builder::new()
            .name("skyfall-collector".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        *outcome_t.lock().expect("outcome poisoned") = Some(Err(e.into()));
                        return;
                    }
                };
                let res = rt.block_on(run_forever(
                    cfg_t, store_t, latest_t, events_t, stop_t, reload_t,
                ));
                *outcome_t.lock().expect("outcome poisoned") = Some(res);
            })
            .expect("spawning collector thread");

        Ok(Runner {
            store,
            cfg,
            cfg_path,
            latest,
            events,
            started: Instant::now(),
            stop_tx,
            reload_tx,
            handle: Some(handle),
            outcome,
        })
    }

    /// Current config snapshot (for the settings UI).
    pub fn config(&self) -> Config {
        self.cfg.lock().expect("cfg poisoned").clone()
    }

    /// Hot-swap the config the loop reads. Call `save_config` to persist.
    pub fn set_config(&self, cfg: Config) {
        *self.cfg.lock().expect("cfg poisoned") = cfg;
    }

    /// Persist the current config to `cfg_path`.
    pub fn save_config(&self) -> Result<()> {
        self.config().save(&self.cfg_path)
    }

    /// Re-read the config file, swap it in, and re-apply what can be applied
    /// live (collector + AI sidecar respawn). The sidecar's own start-only knobs
    /// still need a full restart — noted in the UI.
    pub fn reload(&self) -> Result<()> {
        let cfg = Config::load(&self.cfg_path)?;
        self.set_config(cfg);
        let _ = self.reload_tx.send(());
        Ok(())
    }

    /// Ask the loop to wind down (stops the sidecar, finalizes the WAL) and
    /// wait for the thread to exit.
    pub fn stop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// The loop's final result, if it has returned already. Consumes it.
    pub fn outcome(&self) -> Option<Result<()>> {
        self.outcome.lock().expect("outcome poisoned").take()
    }
}

/// The collector loop itself: sample → store → sidecar → emit events, until
/// `stop` fires. `reload` causes the config to be re-read and reapplied live.
async fn run_forever(
    cfg: Arc<Mutex<Config>>,
    store: Arc<Mutex<Store>>,
    latest_tx: watch::Sender<Option<Arc<crate::metrics::Snapshot>>>,
    events_tx: broadcast::Sender<AppEvent>,
    mut stop: watch::Receiver<()>,
    mut reload: watch::Receiver<()>,
) -> Result<()> {
    let initial = cfg.lock().expect("cfg poisoned").clone();
    log::info!(
        "skyfall v{} — sampling every {}/{} ms (active/idle), storing to {}",
        VERSION,
        initial.sampling_interval_ms,
        initial.idle_interval_ms,
        initial.storage_path.display()
    );

    let mut collector = Collector::new(initial.clone());
    let mut sidecar = Sidecar::new(&initial);
    let mut last_write = Instant::now();

    // Warm up CPU sampling so first reported percentages are meaningful.
    tokio::time::sleep(Duration::from_millis(
        initial.sampling_interval_ms.min(1_000),
    ))
    .await;

    loop {
        // Backpressure for config reloads: respawn collector + sidecar live.
        if reload.has_changed().unwrap_or(false) {
            reload.borrow_and_update();
            let cur = cfg.lock().expect("cfg poisoned").clone();
            sidecar = Sidecar::new(&cur);
            collector = Collector::new(cur.clone());
            last_write = Instant::now();
            log::info!("config reloaded (sidecar/collector reapplied)");
        }

        let cur = cfg.lock().expect("cfg poisoned").clone();
        let t0 = Instant::now();
        let snap = match collector.collect() {
            Ok(s) => s,
            Err(e) => {
                log::error!("collect failed: {e:#}");
                tokio::time::sleep(Duration::from_millis(cur.sampling_interval_ms)).await;
                continue;
            }
        };
        let arc = Arc::new(snap);

        // Publish for the UI, then persist at the configured cadence.
        let _ = latest_tx.send(Some(arc.clone()));

        if last_write.elapsed().as_millis() as u64 >= cur.storage_interval_ms {
            store
                .lock()
                .expect("store lock poisoned")
                .write_snapshot(&arc)?;
            last_write = Instant::now();
        }

        if cur.echo_stdout {
            println!("{}", serde_json::to_string(arc.as_ref())?);
        }

        // Feed the AI sidecar and persist whatever it flagged.
        if let Err(e) = sidecar.feed(&arc) {
            log::warn!("ai sidecar feed failed, disabling alerts: {e:#}");
            sidecar.set_disabled();
        }
        let outcome = sidecar.drain();
        if !outcome.events.is_empty() || !outcome.narratives.is_empty() {
            let mut store_guard = store.lock().expect("store lock poisoned");
            for ev in outcome.events {
                let dossier = ev.dossier.as_deref().unwrap_or("");
                if let Err(e) = store_guard.insert_alert(
                    ev.ts_ms,
                    &ev.severity,
                    &ev.kind,
                    &ev.scope,
                    &ev.message,
                    dossier,
                ) {
                    log::error!("inserting alert: {e:#}");
                } else {
                    log::info!("alert [{}|{}] {}", ev.severity, ev.kind, ev.message);
                    let _ = events_tx.send(AppEvent::Alert(ev));
                }
            }
            for n in outcome.narratives {
                let v = &n.verdict;
                match store_guard.alert_id_for(n.alert_ts_ms, &n.alert_kind, &n.alert_scope) {
                    Ok(Some(id)) => match store_guard.insert_narrative(id, n.ts_ms, v) {
                        Ok(()) => {
                            log::info!("narrative [{}]: {}", v.severity, v.headline);
                            let _ = events_tx.send(AppEvent::Narrative(n));
                        }
                        Err(e) => log::error!("inserting narrative: {e:#}"),
                    },
                    Ok(None) => log::warn!(
                        "narrative for unmatched alert skipped ({})",
                        n.alert_message
                    ),
                    Err(e) => log::error!("looking up alert id: {e:#}"),
                }
            }
        }

        let interval = if arc.system.cpu_total > 0.0 {
            cur.effective_interval(arc.system.cpu_total)
        } else {
            cur.sampling_interval_ms
        };
        let sleep_for = Duration::from_millis(interval).saturating_sub(t0.elapsed());

        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => {}
            _ = stop.changed() => {
                log::info!("shutting down");
                let _ = latest_tx.send(None);
                sidecar.shutdown();
                store.lock().expect("store lock poisoned").finalize();
                break;
            }
        }
    }

    Ok(())
}
