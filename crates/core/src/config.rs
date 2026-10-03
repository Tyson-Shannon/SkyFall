use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Runtime configuration. Loaded from TOML; sensible defaults for consumer use.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// How often to collect a fresh snapshot while the system is active.
    pub sampling_interval_ms: u64,
    /// Sampling interval used while the system is idle (battery/heat friendly).
    pub idle_interval_ms: u64,
    /// Minimum CPU load (0-100) below which the idle interval is used.
    pub idle_cpu_threshold: f32,
    /// Wall-clock resolution for storing samples into the history DB.
    pub storage_interval_ms: u64,
    /// Where the SQLite history lives. `:memory:` is supported.
    pub storage_path: PathBuf,
    /// Maximum number of processes stored per snapshot, ranked by CPU% then RAM.
    pub top_processes: usize,
    /// Include per-core CPU values in snapshots.
    pub per_core_cpu: bool,
    /// Collect per-process disk I/O deltas (slightly more expensive).
    pub per_process_io: bool,
    /// Emit raw snapshots to stdout as NDJSON in addition to storage.
    pub echo_stdout: bool,
    /// Raise a desktop popup for each alert. Off by default: alerts are always
    /// recorded and shown in the dashboard, popups are opt-in.
    pub desktop_notifications: bool,
    /// Python anomaly-detection sidecar (phase 4).
    pub ai: AiConfig,
    /// Where this config was loaded from / will be saved to (never serialized).
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

/// Sidecar (skyfall_ai) settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// Spawn and feed the Python sidecar, and persist its alerts.
    pub enabled: bool,
    /// Interpreter used to launch the sidecar.
    pub command: String,
    /// Module handed to `command -m` (the published package name).
    pub module: String,
    /// Path to the sidecar's JSON config. `None` falls back to
    /// [`default_sidecar_config_path`], which is where Settings writes the
    /// narrator settings — so this must always resolve to a real location.
    pub sidecar_config: Option<PathBuf>,
}

impl AiConfig {
    /// The configured path, or the default. Kept unexpanded (`~/…`) because this
    /// is what gets written back to the TOML.
    pub fn config_path_raw(&self) -> PathBuf {
        self.sidecar_config
            .clone()
            .unwrap_or_else(default_sidecar_config_path)
    }

    /// [`Self::config_path_raw`] with `~` expanded, for filesystem and argv use.
    pub fn config_path(&self) -> PathBuf {
        crate::storage::expand_tilde(&self.config_path_raw())
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            enabled: true,
            command: "python3".to_string(),
            module: "skyfall_ai".to_string(),
            sidecar_config: Some(default_sidecar_config_path()),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sampling_interval_ms: 3000,
            idle_interval_ms: 15000,
            idle_cpu_threshold: 10.0,
            storage_interval_ms: 60_000,
            storage_path: PathBuf::from("skyfall.db"),
            top_processes: 25,
            per_core_cpu: true,
            per_process_io: true,
            echo_stdout: true,
            desktop_notifications: false,
            ai: AiConfig::default(),
            source_path: None,
        }
    }
}

impl Config {
    /// Load config from a path, falling back to defaults if the file does not exist.
    pub fn load(path: &std::path::Path) -> Result<Config> {
        let mut cfg = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading config {}", path.display()))?;
            toml::from_str::<Config>(&strip_retired_keys(&raw))
                .with_context(|| format!("parsing config {}", path.display()))?
        } else {
            log::warn!("config not found at {}, using defaults", path.display());
            Config::default()
        };
        cfg.source_path = Some(path.to_path_buf());
        Ok(cfg)
    }

    /// Default config file location, honouring `SKYFALL_CONFIG`.
    ///
    /// `~/.config/skyfall/skyfall.toml` on Unix, `%APPDATA%\skyfall\skyfall.toml`
    /// on Windows. Falls back to `./skyfall.toml` if the home dir is unknown.
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("SKYFALL_CONFIG") {
            return PathBuf::from(p);
        }
        config_dir().join("skyfall.toml")
    }

    /// Serialize to TOML. The transient `source_path` is skipped by serde.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serializing config to TOML")
    }

    /// Write this config to `path`, creating parent directories as needed.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, self.to_toml()?).with_context(|| format!("writing {}", path.display()))
    }

    /// The path to write config changes to (source file, or the default location).
    pub fn effective_path(&self) -> PathBuf {
        self.source_path.clone().unwrap_or_else(Self::default_path)
    }

    /// The interval to use given the current global CPU load.
    pub fn effective_interval(&self, global_cpu: f32) -> u64 {
        if global_cpu < self.idle_cpu_threshold {
            self.idle_interval_ms
        } else {
            self.sampling_interval_ms
        }
    }
}

/// Drop the `[server]` table from a raw config before parsing.
///
/// The dashboard listener is gone, but `Config` denies unknown keys so a config
/// written by an older build would otherwise fail to load at all — which would
/// brick the app for every existing user. Stripping the retired table keeps old
/// configs readable; the next save from Settings drops it for good.
fn strip_retired_keys(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut skipping = false;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            skipping = trimmed.starts_with("[server]");
            if skipping {
                log::info!(
                    "ignoring retired [server] config section (the local dashboard was removed)"
                );
            }
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Directory the app keeps its configuration in.
fn config_dir() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(d) => PathBuf::from(d).join("skyfall"),
        Err(_) => {
            let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".config").join("skyfall")
        }
    }
}

/// Default sidecar JSON config location, honouring `SKYFALL_AI_CONFIG`.
///
/// Sits next to `skyfall.toml`. This has to exist: Settings saves the narrator
/// settings (on/off, model, endpoint) into this file, so without a default the
/// AI toggle and model could never be persisted.
pub fn default_sidecar_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("SKYFALL_AI_CONFIG") {
        return PathBuf::from(p);
    }
    config_dir().join("skyfall_ai.json")
}

/// Read the sidecar's JSON config, if `path` exists.
pub fn read_sidecar_config(path: &std::path::Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Write (replacing) the sidecar's JSON config, creating parent dirs.
pub fn write_sidecar_config(path: &std::path::Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.sampling_interval_ms, 3000);
        assert_eq!(c.top_processes, 25);
        assert!(c.per_core_cpu);
        assert!(c.per_process_io);
        assert_eq!(c.storage_path, PathBuf::from("skyfall.db"));
        assert!(
            !c.desktop_notifications,
            "popups are opt-in: alerts are recorded either way"
        );
    }

    #[test]
    fn desktop_notifications_round_trip_through_toml() {
        // An old config with no key keeps the quiet default rather than failing
        // to parse, and turning it on survives a save.
        let quiet = Config::load(Path::new("/nonexistent/skyfall.toml")).unwrap();
        assert!(!quiet.desktop_notifications);

        let parsed = toml::from_str::<Config>("desktop_notifications = true\n").unwrap();
        assert!(parsed.desktop_notifications);
        assert!(Config::default()
            .to_toml()
            .unwrap()
            .contains("desktop_notifications"));
    }

    #[test]
    fn effective_interval_switches_on_load() {
        let c = Config::default();
        assert_eq!(c.effective_interval(5.0), c.idle_interval_ms);
        assert_eq!(c.effective_interval(50.0), c.sampling_interval_ms);
        assert_eq!(
            c.effective_interval(c.idle_cpu_threshold),
            c.sampling_interval_ms
        );
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let c = Config::load(Path::new("/nonexistent/skyfall.toml")).unwrap();
        assert_eq!(c.sampling_interval_ms, 3000);
    }

    #[test]
    fn load_parses_toml() {
        let dir = std::env::temp_dir();
        let path = dir.join("skyfall_test_config.toml");
        std::fs::write(
            &path,
            "sampling_interval_ms = 500\ntop_processes = 5\nper_core_cpu = false\n",
        )
        .unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.sampling_interval_ms, 500);
        assert_eq!(c.top_processes, 5);
        assert!(!c.per_core_cpu);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn rejects_unknown_keys() {
        let c = toml::from_str::<Config>("not_a_real_key = 1");
        assert!(c.is_err());
    }

    #[test]
    fn legacy_server_section_is_ignored_not_fatal() {
        // An older config (written when the app served a localhost dashboard) must
        // still load: `deny_unknown_fields` would otherwise reject the whole file.
        let legacy = "sampling_interval_ms = 500\n\n[server]\nenabled = true\nhost = \"0.0.0.0\"\nport = 9099\n\n[ai]\nenabled = false\n";
        let parsed = toml::from_str::<Config>(&strip_retired_keys(legacy))
            .expect("legacy [server] section must not break parsing");
        assert_eq!(parsed.sampling_interval_ms, 500);
        assert!(!parsed.ai.enabled, "sections after [server] still parse");

        // Same through the file-loading path.
        let dir = std::env::temp_dir();
        let path = dir.join("skyfall_legacy_server_test.toml");
        std::fs::write(
            &path,
            "top_processes = 7\n\n[server]\nenabled = true\nhost = \"127.0.0.1\"\nport = 8080\n",
        )
        .unwrap();
        let c = Config::load(&path).unwrap();
        assert_eq!(c.top_processes, 7);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn written_config_has_no_server_section() {
        let toml_text = Config::default().to_toml().unwrap();
        assert!(!toml_text.contains("[server]"), "{toml_text}");
        assert!(!toml_text.contains("8080"), "{toml_text}");
    }

    #[test]
    fn ai_section_defaults_and_override() {
        let c = Config::default();
        assert!(c.ai.enabled);
        assert_eq!(c.ai.command, "python3");
        assert_eq!(c.ai.module, "skyfall_ai");
        assert!(
            c.ai.sidecar_config.is_some(),
            "the narrator settings need a path to be saved into"
        );

        let parsed = toml::from_str::<Config>(
            "[ai]\nenabled = false\ncommand = \"python\"\nsidecar_config = \"~/.config/skyfall/sidecar.json\"\n",
        )
        .unwrap();
        assert!(!parsed.ai.enabled);
        assert_eq!(parsed.ai.command, "python");
        assert_eq!(
            parsed.ai.sidecar_config.as_deref(),
            Some(std::path::Path::new("~/.config/skyfall/sidecar.json"))
        );
        assert_eq!(parsed.ai.module, "skyfall_ai");
    }

    #[test]
    fn unset_sidecar_config_still_resolves_to_a_writable_path() {
        // The regression: `sidecar_config` defaulted to None, so the Settings
        // write was discarded and the AI toggle/model could not be saved.
        let ai = AiConfig {
            sidecar_config: None,
            ..AiConfig::default()
        };
        let path = ai.config_path();
        assert_eq!(path, ai.config_path_raw(), "no `~` to expand here");
        assert!(
            path.ends_with("skyfall_ai.json"),
            "{} should be the sidecar config",
            path.display()
        );
    }

    #[test]
    fn sidecar_config_path_expands_tilde() {
        let Some(home) = std::env::var_os("HOME") else {
            return; // nothing to expand against
        };
        let ai = AiConfig {
            sidecar_config: Some(PathBuf::from("~/.config/skyfall/ai.json")),
            ..AiConfig::default()
        };
        assert_eq!(
            ai.config_path(),
            PathBuf::from(home).join(".config/skyfall/ai.json")
        );
        // The raw form stays portable for the TOML.
        assert_eq!(
            ai.config_path_raw(),
            PathBuf::from("~/.config/skyfall/ai.json")
        );
    }

    #[test]
    fn sidecar_config_round_trips_through_disk() {
        let path = std::env::temp_dir().join(format!("skyfall-ai-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(
            read_sidecar_config(&path).is_none(),
            "missing file reads as none"
        );

        let value = serde_json::json!({"narrator": {"enabled": false, "model": "qwen2.5:7b"}});
        write_sidecar_config(&path, &value).unwrap();
        let back = read_sidecar_config(&path).unwrap();
        assert_eq!(back["narrator"]["enabled"], false);
        assert_eq!(back["narrator"]["model"], "qwen2.5:7b");

        // Replacing, not merging: the dialog always sends the whole block.
        write_sidecar_config(&path, &serde_json::json!({"narrator": {"enabled": true}})).unwrap();
        assert!(read_sidecar_config(&path).unwrap()["narrator"]["model"].is_null());

        let _ = std::fs::remove_file(&path);
    }
}
