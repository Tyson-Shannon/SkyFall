//! The view layer the desktop UI reads: liveness, downsampled history, the alert
//! feed, and the config bundle behind Settings.
//!
//! This used to live behind axum routes (`/api/snapshot`, `/api/history`, …) with a
//! WebSocket pushing every live sample. There is no server any more: the desktop
//! shell serves the same HTML/JS/CSS straight out of the bundle and calls these
//! functions through Tauri commands. Keeping them here rather than in the Tauri
//! crate means they stay testable without a GUI toolkit.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::ai;
use crate::config::{read_sidecar_config, write_sidecar_config, Config};
use crate::storage::{AlertRecord, History, Store};
use crate::VERSION;

/// Liveness/version banner in the window header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub ok: bool,
    pub version: &'static str,
    pub ts_ms: u64,
    pub uptime_secs: u64,
}

impl Health {
    pub fn new(started: std::time::Instant) -> Health {
        Health {
            ok: true,
            version: VERSION,
            ts_ms: now_ms(),
            uptime_secs: started.elapsed().as_secs(),
        }
    }
}

/// Core config + sidecar JSON, for the settings dialog.
///
/// The dialog only edits a handful of fields and PUTs the whole bundle back, so
/// the untouched sections have to survive the round-trip — that is why the
/// sidecar part stays a raw `serde_json::Value` instead of a typed struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigBundle {
    pub core: Config,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<serde_json::Value>,
}

/// Read the current bundle (core config + sidecar JSON) for the settings dialog.
///
/// The sidecar block is always present and its `narrator` is filled in with the
/// values that are actually in effect (see [`effective_narrator`]). The dialog
/// round-trips this bundle verbatim, so anything missing here comes back as an
/// empty form field rather than as the setting the user actually has.
pub fn config_bundle(cfg: &Config) -> ConfigBundle {
    let raw = read_sidecar_config(&cfg.ai.config_path()).filter(|v| v.is_object());
    let sidecar = match raw {
        Some(v) => with_effective_narrator(&v),
        None => serde_json::json!({ "narrator": effective_narrator(None) }),
    };
    ConfigBundle {
        core: cfg.clone(),
        sidecar: Some(sidecar),
    }
}

/// The narrator block as the sidecar will actually read it: whatever the JSON
/// says, with blank/missing fields replaced by our defaults.
///
/// The sidecar defaults the narrator to *enabled*, and treats a blank
/// `base_url` as a literal empty endpoint, so showing the raw file would both
/// render the toggle as off when it is on and save blanks the narrator cannot
/// use.
fn effective_narrator(sidecar: Option<&serde_json::Value>) -> serde_json::Value {
    let mut n = sidecar
        .and_then(|v| v.get("narrator"))
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let obj = n.as_object_mut().expect("narrator is an object");
    if !obj
        .get("enabled")
        .is_some_and(serde_json::Value::is_boolean)
    {
        obj.insert("enabled".into(), serde_json::Value::Bool(true));
    }
    for (key, default) in [
        ("base_url", ai::DEFAULT_BASE_URL),
        ("model", ai::DEFAULT_MODEL),
    ] {
        let blank = obj
            .get(key)
            .and_then(|v| v.as_str())
            .is_none_or(|s| s.trim().is_empty());
        if blank {
            obj.insert(key.into(), serde_json::Value::String(default.into()));
        }
    }
    n
}

/// The core config as it should be written out: where it came from, plus the
/// resolved sidecar path.
///
/// The path is stamped into the TOML on purpose — it used to default to `None`,
/// which meant the narrator settings Settings saved had no file to live in and
/// were dropped on the floor.
fn persisted_core(core: &Config, cfg_path: &Path) -> Config {
    let mut cfg = core.clone();
    cfg.source_path = Some(cfg_path.to_path_buf());
    cfg.ai.sidecar_config = Some(cfg.ai.config_path_raw());
    cfg
}

/// Persist a bundle from the settings dialog: validate + write the core TOML,
/// write the sidecar JSON if one came back, and hand back what actually landed.
pub fn save_bundle(bundle: &ConfigBundle, cfg_path: &Path) -> Result<ConfigBundle> {
    let cfg = persisted_core(&bundle.core, cfg_path);
    let sidecar_path = cfg.ai.config_path();
    cfg.save(cfg_path)?;
    if let Some(v) = &bundle.sidecar {
        write_sidecar_config(&sidecar_path, &with_effective_narrator(v))?;
    }
    Ok(config_bundle(&cfg))
}

/// [`effective_narrator`] applied inside `sidecar`, leaving the rest alone.
fn with_effective_narrator(sidecar: &serde_json::Value) -> serde_json::Value {
    let mut v = sidecar.clone();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("narrator".into(), effective_narrator(Some(sidecar)));
    }
    v
}

/// The narrator's endpoint + model, as currently configured.
pub fn narrator_target(cfg: &Config) -> (String, String) {
    let sidecar = read_sidecar_config(&cfg.ai.config_path());
    let n = effective_narrator(sidecar.as_ref());
    let get = |k: &str| {
        n.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    (get("base_url"), get("model"))
}

/// Downsample bucket for a requested history window, in ms.
pub fn bucket_ms(hours: u64) -> i64 {
    match hours {
        0 | 1 => 10_000,
        2..=6 => 60_000,
        _ => 300_000,
    }
}

/// Clamp a requested window (1-24 h) into a `(since_ms, bucket_ms)` query.
pub fn history_window(hours: u64) -> (i64, i64) {
    let hours = hours.clamp(1, 24);
    let since = now_ms().saturating_sub(hours * 3_600_000) as i64;
    (since, bucket_ms(hours))
}

/// Stored history for the last `hours`, downsampled.
pub fn history(store: &Store, hours: u64) -> Result<History> {
    let (since, bucket) = history_window(hours);
    store.history(since, bucket)
}

/// Recent alerts for the feed, clamped to a sane page size.
pub fn alerts(store: &Store, limit: u32) -> Result<Vec<AlertRecord>> {
    store.recent_alerts(limit.clamp(1, 100))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_size_by_window() {
        assert_eq!(bucket_ms(1), 10_000);
        assert_eq!(bucket_ms(6), 60_000);
        assert_eq!(bucket_ms(24), 300_000);
    }

    #[test]
    fn history_window_clamps_and_backdates() {
        let (since, bucket) = history_window(0);
        assert_eq!(bucket, 10_000, "0h means the finest bucket");
        assert!(since <= now_ms() as i64, "window start is in the past");

        let (since, bucket) = history_window(999);
        assert_eq!(bucket, 300_000, "24h cap keeps the bucket at 5m");
        assert!(since <= now_ms() as i64 - 23 * 3_600_000);
    }

    #[test]
    fn alerts_are_clamped_to_the_page_size() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        for i in 0..3 {
            store
                .insert_alert(1_000 + i, "high", "rule", "global", "CPU hot", "")
                .unwrap();
        }
        // limit 0 clamps up to 1 rather than returning nothing.
        assert_eq!(alerts(&store, 0).unwrap().len(), 1);
        assert_eq!(
            alerts(&store, 1000).unwrap().len(),
            3,
            "over-wide limit is capped at 100"
        );
    }

    #[test]
    fn narrator_target_falls_back_to_defaults_without_a_sidecar_file() {
        let (base_url, model) = narrator_target(&Config::default());
        assert_eq!(base_url, ai::DEFAULT_BASE_URL);
        assert_eq!(model, ai::DEFAULT_MODEL);
    }

    #[test]
    fn narrator_target_reads_the_sidecar_file() {
        let dir = std::env::temp_dir().join(format!("skyfall-api-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sidecar.json");
        std::fs::write(
            &path,
            r#"{"narrator": {"enabled": true, "base_url": "http://127.0.0.1:1", "model": "qwen2.5:1.5b"}}"#,
        )
        .unwrap();

        let mut cfg = Config::default();
        cfg.ai.sidecar_config = Some(path.clone());
        let (base_url, model) = narrator_target(&cfg);
        assert_eq!(base_url, "http://127.0.0.1:1");
        assert_eq!(model, "qwen2.5:1.5b");

        let bundle = config_bundle(&cfg);
        let narrator = bundle.sidecar.as_ref().unwrap()["narrator"].clone();
        assert_eq!(narrator["model"], "qwen2.5:1.5b");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn narrator_target_ignores_blank_strings() {
        let dir = std::env::temp_dir().join(format!("skyfall-api-blank-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sidecar.json");
        std::fs::write(&path, r#"{"narrator": {"base_url": "  ", "model": ""}}"#).unwrap();
        let mut cfg = Config::default();
        cfg.ai.sidecar_config = Some(path.clone());
        let (base_url, model) = narrator_target(&cfg);
        assert_eq!(base_url, ai::DEFAULT_BASE_URL, "blank falls back");
        assert_eq!(model, ai::DEFAULT_MODEL, "blank falls back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_bundle_round_trips_core_and_sidecar() {
        let dir = std::env::temp_dir().join(format!("skyfall-api-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let core_path = dir.join("skyfall.toml");
        let sidecar_path = dir.join("sidecar.json");

        let mut cfg = Config::default();
        cfg.ai.sidecar_config = Some(sidecar_path.clone());

        let mut bundle = config_bundle(&cfg);
        bundle.core.sampling_interval_ms = 500;
        bundle.core.top_processes = 7;
        bundle.sidecar = Some(serde_json::json!({"narrator": {"model": "qwen2.5:7b"}}));

        let saved = save_bundle(&bundle, &core_path).unwrap();
        assert_eq!(saved.core.sampling_interval_ms, 500);

        // Both files landed and re-read.
        let from_disk = Config::load(&core_path).unwrap();
        assert_eq!(from_disk.top_processes, 7);
        assert_eq!(from_disk.source_path.as_deref(), Some(core_path.as_path()));
        let side: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        assert_eq!(side["narrator"]["model"], "qwen2.5:7b");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn desktop_notification_toggle_survives_a_save() {
        // The dialog PUTs the whole config, so a field the form forgets would be
        // silently reset on every save of an unrelated setting.
        let dir = std::env::temp_dir().join(format!("skyfall-api-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let core_path = dir.join("skyfall.toml");

        let mut cfg = Config {
            storage_path: std::path::PathBuf::from(":memory:"),
            ..Default::default()
        };
        cfg.ai.sidecar_config = Some(dir.join("sidecar.json"));

        let mut bundle = config_bundle(&cfg);
        assert!(
            !bundle.core.desktop_notifications,
            "quiet until the user asks for popups"
        );
        bundle.core.desktop_notifications = true;
        bundle.core.top_processes = 9;

        save_bundle(&bundle, &core_path).unwrap();
        let from_disk = Config::load(&core_path).unwrap();
        assert!(from_disk.desktop_notifications, "popup opt-in stuck");
        assert_eq!(from_disk.top_processes, 9);

        // And turning it back off sticks too.
        let mut bundle = config_bundle(&from_disk);
        bundle.core.desktop_notifications = false;
        save_bundle(&bundle, &core_path).unwrap();
        assert!(!Config::load(&core_path).unwrap().desktop_notifications);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn health_reports_version_and_uptime() {
        let h = Health::new(std::time::Instant::now() - std::time::Duration::from_secs(5));
        assert!(h.ok);
        assert_eq!(h.version, VERSION);
        assert!((4..=6).contains(&h.uptime_secs), "uptime {}", h.uptime_secs);
    }

    #[test]
    fn bundle_reports_the_effective_narrator_even_without_a_sidecar_file() {
        // Nothing on disk yet: the dialog must show what the sidecar will really
        // run (narrator on, default endpoint/model), not a blank/off form.
        let mut cfg = Config::default();
        cfg.ai.sidecar_config =
            Some(std::env::temp_dir().join(format!("skyfall-absent-{}.json", std::process::id())));
        let bundle = config_bundle(&cfg);
        let n = &bundle.sidecar.expect("sidecar block is always present")["narrator"];
        assert_eq!(
            n["enabled"], true,
            "the sidecar defaults the narrator to on"
        );
        assert_eq!(n["base_url"], ai::DEFAULT_BASE_URL);
        assert_eq!(n["model"], ai::DEFAULT_MODEL);
    }

    #[test]
    fn narrator_target_reads_blanks_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("skyfall-api-blank2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sidecar.json");
        // Exactly what a half-filled Settings form would produce.
        std::fs::write(
            &path,
            r#"{"narrator": {"enabled": false, "base_url": "  ", "model": ""}}"#,
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.ai.sidecar_config = Some(path.clone());
        let (base_url, model) = narrator_target(&cfg);
        assert_eq!(base_url, ai::DEFAULT_BASE_URL, "blank falls back");
        assert_eq!(model, ai::DEFAULT_MODEL, "blank falls back");

        // ...and the dialog shows the same, instead of blank fields it would
        // then save back over the real values.
        let n = &config_bundle(&cfg).sidecar.unwrap()["narrator"];
        assert_eq!(n["enabled"], false, "an explicit off is respected");
        assert_eq!(n["base_url"], ai::DEFAULT_BASE_URL);
        assert_eq!(n["model"], ai::DEFAULT_MODEL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ai_toggle_and_model_survive_a_save() {
        // The reported bug: neither the AI on/off setting nor the model stayed
        // saved, because the bundle's sidecar half had nowhere to go.
        let dir = std::env::temp_dir().join(format!("skyfall-api-ai-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let core_path = dir.join("skyfall.toml");
        let sidecar_path = dir.join("skyfall_ai.json");

        let mut cfg = Config::default();
        cfg.ai.sidecar_config = Some(sidecar_path.clone());
        cfg.storage_path = std::path::PathBuf::from(":memory:");
        let mut bundle = config_bundle(&cfg);
        bundle.sidecar.as_mut().unwrap()["narrator"] = serde_json::json!({
            "enabled": false,
            "base_url": "http://127.0.0.1:11434",
            "model": "qwen2.5:7b",
            "api_key": null,
        });

        let saved = save_bundle(&bundle, &core_path).unwrap();
        let narrator = &saved.sidecar.as_ref().unwrap()["narrator"];
        assert_eq!(narrator["enabled"], false);
        assert_eq!(narrator["model"], "qwen2.5:7b");

        // Re-read from disk the way the next launch does.
        let reloaded = Config::load(&core_path).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        assert_eq!(written["narrator"]["enabled"], false, "AI off stuck");
        assert_eq!(written["narrator"]["model"], "qwen2.5:7b", "model stuck");
        assert_eq!(narrator_target(&reloaded).1, "qwen2.5:7b");

        // The core TOML records the path, so the sidecar reads the same file.
        let toml_text = std::fs::read_to_string(&core_path).unwrap();
        assert!(
            toml_text.contains("sidecar_config"),
            "path must be persisted too: {toml_text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unset_sidecar_path_is_pinned_to_a_real_file_on_save() {
        // The default that caused the bug: with no `sidecar_config` configured,
        // saving stamped nothing and the narrator settings went nowhere.
        let dir = std::env::temp_dir().join(format!("skyfall-api-unset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let core_path = dir.join("skyfall.toml");

        let mut cfg = Config::default();
        cfg.ai.sidecar_config = None;
        let saved = persisted_core(&cfg, &core_path);
        assert_eq!(
            saved.ai.sidecar_config.as_deref(),
            Some(crate::config::default_sidecar_config_path().as_path())
        );
        assert!(
            saved.to_toml().unwrap().contains("sidecar_config"),
            "the default path must be written out, not left implicit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_bundle_never_persists_a_blank_endpoint() {
        // A cleared field is not "unset" to the sidecar: `str(cfg.get("base_url",
        // default))` would make it post to an empty host.
        let dir =
            std::env::temp_dir().join(format!("skyfall-api-blanksave-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let core_path = dir.join("skyfall.toml");
        let sidecar_path = dir.join("sidecar.json");

        let mut cfg = Config::default();
        cfg.ai.sidecar_config = Some(sidecar_path.clone());
        cfg.storage_path = std::path::PathBuf::from(":memory:");
        let mut bundle = config_bundle(&cfg);
        bundle.sidecar.as_mut().unwrap()["narrator"] =
            serde_json::json!({"enabled": true, "base_url": "  ", "model": ""});

        save_bundle(&bundle, &core_path).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        assert_eq!(written["narrator"]["base_url"], ai::DEFAULT_BASE_URL);
        assert_eq!(written["narrator"]["model"], ai::DEFAULT_MODEL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn untouched_sidecar_sections_survive_a_save() {
        let dir = std::env::temp_dir().join(format!("skyfall-api-keep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let core_path = dir.join("skyfall.toml");
        let sidecar_path = dir.join("sidecar.json");

        let mut cfg = Config::default();
        cfg.ai.sidecar_config = Some(sidecar_path.clone());
        cfg.storage_path = std::path::PathBuf::from(":memory:");
        let mut bundle = config_bundle(&cfg);
        // Rules the dialog never shows.
        bundle.sidecar.as_mut().unwrap()["rules"] =
            serde_json::json!({"cpu_spike": {"metric": "system.cpu_total", "gt": 50.0}});

        save_bundle(&bundle, &core_path).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar_path).unwrap()).unwrap();
        assert_eq!(written["rules"]["cpu_spike"]["gt"], 50.0);
        assert!(written["narrator"].is_object(), "narrator added alongside");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
