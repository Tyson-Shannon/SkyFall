//! SkyFall desktop shell (Tauri 2): the window, system tray, native
//! notifications, and launch-at-login — all on top of `skyfall-core`'s `Runner`,
//! which owns the collector, the history store, and the AI sidecar.
//!
//! There is no local web server. The window loads the dashboard straight out of
//! the bundle (`tauri.conf.json` → `frontendDist`) and everything it needs comes
//! back over IPC: `#[tauri::command]`s for reads and writes, and events for the
//! things that used to be a WebSocket and an NDJSON response stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use futures_util::StreamExt;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, State, WebviewWindow, WindowEvent,
};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_notification::NotificationExt;

use skyfall_core::ai::{self, AiStatus, PullLine};
use skyfall_core::api;
use skyfall_core::api::ConfigBundle;
use skyfall_core::config::Config;
use skyfall_core::metrics::Snapshot;
use skyfall_core::runner::{AppEvent, Runner};
use skyfall_core::storage::{AlertRecord, History, Store};

/// Live snapshots, pushed to the window on every sample (was the `/ws` socket).
const EVENT_SNAPSHOT: &str = "skyfall://snapshot";
/// One line of model-download progress (was the `POST /api/ai/pull` NDJSON body).
const EVENT_PULL: &str = "skyfall://ai-pull";

/// Shared state: the core `Runner`, plus the cancel flag for a model download.
pub struct AppState {
    runner: Arc<Mutex<Runner>>,
    pull_cancel: Arc<AtomicBool>,
    /// Set from the dashboard's pause button. Alerts are always written to the
    /// database by the collector; this only withholds them from the UI and from
    /// the notification pump so an in-progress investigation is not interrupted.
    alert_paused: Arc<AtomicBool>,
}

impl AppState {
    /// Clone the shared handles so long-running work never holds the runner lock
    /// (the UI stays responsive while the store is queried or the disk is written).
    fn handles(&self) -> (Arc<Mutex<Store>>, Arc<Mutex<Config>>, std::path::PathBuf) {
        let r = self.runner.lock().expect("runner poisoned");
        (r.store.clone(), r.cfg.clone(), r.cfg_path.clone())
    }
}

/// Re-read the config file and re-apply it live (collector + sidecar respawn).
#[tauri::command]
fn set_alert_pause(state: State<AppState>, paused: bool) {
    state.alert_paused.store(paused, Ordering::Relaxed);
}

#[tauri::command]
fn reload_config(state: State<AppState>) -> Result<(), String> {
    state
        .runner
        .lock()
        .expect("runner poisoned")
        .reload()
        .map_err(|e| format!("{e:#}"))
}

/// Persist the current config through the core `Runner` (hot-swap + save).
#[tauri::command]
fn save_config(state: State<AppState>) -> Result<(), String> {
    state
        .runner
        .lock()
        .expect("runner poisoned")
        .save_config()
        .map_err(|e| format!("{e:#}"))
}

/// Version + uptime for the window header.
#[tauri::command]
fn health(state: State<AppState>) -> api::Health {
    let started: Instant = state.runner.lock().expect("runner poisoned").started;
    api::Health::new(started)
}

/// The most recent snapshot, or `None` until the first one lands.
#[tauri::command]
fn snapshot(state: State<AppState>) -> Option<Arc<Snapshot>> {
    let r = state.runner.lock().expect("runner poisoned");
    let snap = r.latest.borrow().clone();
    snap
}

/// Downsampled stored history for the chart backfill.
#[tauri::command]
async fn history(hours: Option<u64>, state: State<'_, AppState>) -> Result<History, String> {
    let (store, _, _) = state.handles();
    let hours = hours.unwrap_or(1);
    tauri::async_runtime::spawn_blocking(move || {
        let guard = store.lock().expect("store poisoned");
        api::history(&guard, hours).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("history task failed: {e}"))?
}

/// Recent alerts for the feed, newest first, with their narratives.
#[tauri::command]
async fn alerts(
    limit: Option<u32>,
    state: State<'_, AppState>,
) -> Result<Vec<AlertRecord>, String> {
    let (store, _, _) = state.handles();
    let limit = limit.unwrap_or(25);
    tauri::async_runtime::spawn_blocking(move || {
        let guard = store.lock().expect("store poisoned");
        api::alerts(&guard, limit).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| format!("alerts task failed: {e}"))?
}

/// The whole config bundle, so Settings can round-trip untouched sections.
#[tauri::command]
async fn get_config(state: State<'_, AppState>) -> Result<ConfigBundle, String> {
    let (_, cfg, _) = state.handles();
    let cfg = cfg.lock().expect("cfg poisoned").clone();
    Ok(api::config_bundle(&cfg))
}

/// Persist a Settings bundle, then hot-apply it (collector + sidecar respawn).
///
/// The file is the source of truth: save first, then let `Runner::reload` read
/// it back, so what the UI showed as "saved" is exactly what the loop picked up.
#[tauri::command]
async fn put_config(
    bundle: ConfigBundle,
    state: State<'_, AppState>,
) -> Result<ConfigBundle, String> {
    let (_, _, cfg_path) = state.handles();
    let saved = api::save_bundle(&bundle, &cfg_path).map_err(|e| format!("{e:#}"))?;
    state
        .runner
        .lock()
        .expect("runner poisoned")
        .reload()
        .map_err(|e| format!("{e:#}"))?;
    Ok(saved)
}

/// Is the LLM endpoint reachable, and is the configured model installed?
#[tauri::command]
async fn ai_status(state: State<'_, AppState>) -> Result<AiStatus, String> {
    let (_, cfg, _) = state.handles();
    let cfg = cfg.lock().expect("cfg poisoned").clone();
    let (base_url, model) = api::narrator_target(&cfg);
    Ok(ai::status(&base_url, &model).await)
}

/// Download a model, reporting progress as [`EVENT_PULL`] events.
///
/// Resolves when the download ends (or is cancelled); progress and failures both
/// arrive as events so the dialog has a single path for reading them.
#[tauri::command]
async fn ai_pull(
    window: WebviewWindow,
    model: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let (_, cfg, _) = state.handles();
    let cancel = state.pull_cancel.clone();
    cancel.store(false, Ordering::SeqCst);

    let (base_url, configured) = {
        let cfg = cfg.lock().expect("cfg poisoned").clone();
        api::narrator_target(&cfg)
    };
    let model = model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or(configured);

    let mut stream = match ai::pull(&base_url, &model).await {
        Ok(s) => Box::pin(s),
        Err(e) => {
            // Report in-stream so the client only needs one parsing path.
            let _ = window.emit(EVENT_PULL, PullLine::failed(format!("{e:#}")));
            return Ok(());
        }
    };

    while let Some(line) = stream.next().await {
        if cancel.load(Ordering::SeqCst) {
            let _ = window.emit(EVENT_PULL, PullLine::cancelled());
            break;
        }
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                let _ = window.emit(EVENT_PULL, PullLine::failed(e));
                break;
            }
        };
        if window.emit(EVENT_PULL, &line).is_err() {
            break; // window went away mid-download
        }
    }
    Ok(())
}

/// Stop reporting download progress. Dropping the response stream closes the
/// request, so Ollama stops sending and the next install can resume.
#[tauri::command]
fn cancel_ai_pull(state: State<AppState>) {
    state.pull_cancel.store(true, Ordering::SeqCst);
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn quit(app: &AppHandle) {
    app.exit(0);
}

fn build_tray(app: &AppHandle) -> Result<()> {
    let show_i = MenuItem::with_id(app, "show", "Open SkyFall", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit SkyFall", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &sep, &quit_i])?;

    let tray = TrayIconBuilder::with_id("skyfall-tray")
        .icon(app.default_window_icon().cloned().unwrap())
        .tooltip("SkyFall — AI system monitor")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "quit" => quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    tray.build(app)?;
    Ok(())
}

/// Push every live snapshot to the window (the old `/ws` stream).
fn spawn_snapshot_pump(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut latest = app
            .state::<AppState>()
            .runner
            .lock()
            .expect("runner poisoned")
            .latest
            .clone();

        // Replay the newest snapshot immediately so a reopened window is never blank.
        if let Some(snap) = latest.borrow().clone() {
            let _ = app.emit(EVENT_SNAPSHOT, snap);
        }
        loop {
            match latest.changed().await {
                Ok(()) => {
                    let Some(snap) = latest.borrow().clone() else {
                        continue;
                    };
                    if app.emit(EVENT_SNAPSHOT, snap).is_err() {
                        return; // every window is gone
                    }
                }
                Err(_) => return, // sender dropped (process shutting down)
            }
        }
    });
}

/// Pump core `AppEvent::Alert`s into native desktop notifications.
///
/// The config is re-read per alert rather than captured once, so toggling the
/// setting in the settings modal takes effect immediately — `put_config` saves
/// the file and `Runner::reload` swaps the live config this reads.
fn spawn_notification_pump(app: AppHandle) {
    let paused = app.state::<AppState>().alert_paused.clone();
    let cfg = app
        .state::<AppState>()
        .runner
        .lock()
        .expect("runner poisoned")
        .cfg
        .clone();
    tauri::async_runtime::spawn(async move {
        // Re-subscribe to the runner's broadcast.
        let mut rx = app
            .state::<AppState>()
            .runner
            .lock()
            .expect("runner poisoned")
            .events
            .resubscribe();
        loop {
            match rx.recv().await {
                Ok(AppEvent::Alert(alert)) => {
                    // Recording already happened in the collector; the popup is
                    // the only thing either flag withholds.
                    let paused = paused.load(Ordering::Relaxed);
                    let wanted = cfg.lock().expect("cfg poisoned").desktop_notifications;
                    if paused {
                        log::info!("alerts paused: withheld {}", alert.message);
                    }
                    if !should_notify(wanted, paused) {
                        continue;
                    }
                    let _ = app
                        .notification()
                        .builder()
                        .title(format!("SkyFall · {}", alert.severity))
                        .body(alert.message.clone())
                        .show();
                }
                Ok(AppEvent::Narrative(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
}

/// Whether an alert should raise a desktop popup.
///
/// Kept separate from the pump so the rule is testable: popups are opt-in, and
/// pausing the feed withholds them too. Neither affects the alert itself, which
/// is always written to the database and shown in the dashboard.
fn should_notify(desktop_notifications: bool, paused: bool) -> bool {
    desktop_notifications && !paused
}

/// Resolve the config path from CLI args, mirroring the core binary.
fn parse_config_path() -> Option<std::path::PathBuf> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--config" {
            return it.next().map(std::path::PathBuf::from);
        }
    }
    None
}

pub fn run() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cfg_path = parse_config_path();
    let cfg = match &cfg_path {
        Some(p) => Config::load(p)?,
        None => Config::default(),
    };

    let runner = Arc::new(Mutex::new(Runner::spawn(cfg)?));
    let state = AppState {
        runner,
        pull_cancel: Arc::new(AtomicBool::new(false)),
        alert_paused: Arc::new(AtomicBool::new(false)),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_notification::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            health,
            snapshot,
            history,
            alerts,
            get_config,
            put_config,
            ai_status,
            ai_pull,
            cancel_ai_pull,
            set_alert_pause,
            save_config,
            reload_config,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            build_tray(&handle)?;
            spawn_snapshot_pump(handle.clone());
            spawn_notification_pump(handle);
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Close to tray, not quit.
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())?
        .run(|app, event| {
            if let RunEvent::ExitRequested { .. } = event {
                if let Some(state) = app.try_state::<AppState>() {
                    let mut runner = state.runner.lock().expect("runner poisoned");
                    runner.stop();
                }
            }
        });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::should_notify;

    #[test]
    fn popups_are_opt_in_and_pause_still_wins() {
        assert!(!should_notify(false, false), "quiet by default");
        assert!(should_notify(true, false), "opted in, running");
        assert!(!should_notify(true, true), "pausing withholds popups");
        assert!(!should_notify(false, true));
    }
}
