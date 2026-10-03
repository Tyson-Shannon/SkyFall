# SkyFall
<div align="center">
  <img width="313.5" height="313.5" alt="FileFlix Logo" src="https://github.com/user-attachments/assets/80a0df57-cdb0-43a6-a427-5276eafefe67"
 />
</div>
</br>

A smart, cross-platform system monitor that watches CPU, GPU, RAM, disk, network, and
per-process activity, detects when things look *off for you*, explains *why* with context,
and alerts you in a live dashboard — plus native desktop notifications if you opt in.

- **Stack:** Rust core + Tauri shell + Python AI sidecar
- **Platforms:** Windows / macOS / Linux
- **AI model:** Hybrid — statistical/ML anomaly detection over your own baseline, then a
  local LLM (Ollama) narrates the anomaly with context; cloud LLM pluggable
- **UI:** systray HUD + opt-in native notifications (Tauri, phase 6) and an embedded
  dashboard (live charts, process table, history, alert feed)

```
┌─────────────────────────────  App (Rust binary + Python sidecar)  ─────────────────────┐
│                                                                                        │
│  ┌─ Rust core ──────────────────────────────────┐   ┌─ Python AI sidecar ────────────┐ │
│  │ sysinfo + GPU providers → SQLite             │   │ anomaly detection              │ │
│  │ Tauri IPC commands + dashboard assets       │──▶│ (rules + z-scores + IsoForest) │ │
│  │ spawns Python child, stdio NDJSON            │◀──│ LLM contextualizer             │ │
│  └──────────────────────────────────────────────┘   └────────────────────────────────┘ │
│                        │ tray + native notifications + dashboard window (Tauri shell)   │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

## Status

| Phase | Deliverable | Status |
|-------|-------------|--------|
| 1 | Rust core: collector, snapshot types, SQLite storage, config, CLI (`collect` / `run`) | ✅ |
| 2 | GPU providers: NVIDIA (NVML), Windows (PDH+DXGI), Linux sysfs, composite dedupe | ✅ |
| 3 | Dashboard v1: no-dependency HTML/JS/CSS UI, Tauri IPC + event streaming | ✅ |
| 4 | Python AI sidecar: NDJSON protocol, rules → baseline → IsolationForest, alert trigger | ✅ |
| 5 | LLM contextualizer: Ollama default, cloud pluggable, alert-feed UI | ✅ |
| 6 | Tauri shell: tray, native notifications, autostart, settings | ✅ |
| 7 | Packaging: bundled Python runtime, installers | planned |
| 8 | Hardening: rollups, anomaly tuning UI, privacy | planned |

Full plan, architecture notes, and cross-platform gotchas live in [`ROADMAP.md`](ROADMAP.md);
a component-by-component tour of how a snapshot becomes an alert lives in
[`COMPONENTS.md`](COMPONENTS.md).

## Repo layout

```
SkyFall/
├── README.md
├── ROADMAP.md
├── COMPONENTS.md
├── Cargo.toml                     # workspace
├── crates/
│   └── core/                      # collector, storage, UI-facing queries, GPU providers
│       ├── src/
│       │   ├── collect.rs         # sysinfo collector + per-process/net deltas
│       │   ├── metrics.rs         # snapshot types (serde)
│       │   ├── storage.rs         # SQLite history + alerts (WAL)
│       │   ├── api.rs             # transport-free view layer the UI commands call
│       │   ├── config.rs          # TOML config + defaults
│       │   ├── gpu/               # providers (nvml, sysfs, pdh) + composite
│       │   └── main.rs            # CLI: collect / run (spawns sidecar)
│       └── assets/dashboard/      # dashboard assets (HTML/JS/CSS), bundled by Tauri
│   └── app/                       # Tauri 2 desktop shell (tray, window, notifications, autostart)
│       ├── tauri.conf.json        # window/tray/bundle config
│       ├── capabilities/          # Tauri v2 permissions (notification, autostart)
│       ├── icons/                 # tray + bundle icons
│       └── src/                   # shell: Runner reuse, tray menu, notification pump
├── python/
│   └── sidecar/                   # anomalydetection sidecar + tests (phase 4)
├── shell/                         # force_alert.sh (end-to-end alert), bench_narration.sh,
│                                 #   plus installers to come (phase 7)
└── config/
    └── skyfall.toml               # sample configuration
```

## Build & run

Requires Rust 1.85+ (edition 2021). GPU/RAM history needs nothing beyond the toolchain.

```sh
# one-shot snapshot to stdout
cargo run -p skyfall-core -- collect

# continuous collection + AI sidecar (Ctrl+C to quit)
cargo run -p skyfall-core -- run

# with a custom config
cargo run -p skyfall-core -- --config config/skyfall.toml run

# tests / clippy / Windows cross-check
cargo test -p skyfall-core
cargo clippy --all-targets
cargo check --target x86_64-pc-windows-msvc -p skyfall-core

# desktop shell (Tauri 2): tray + window + native notifications + autostart
cargo run -p skyfall-app --bin skyfall-desktop
cargo run -p skyfall-app --bin skyfall-desktop -- --config config/skyfall.toml
```

The desktop shell (`crates/app`) is a thin Tauri 2 wrapper around the same core: it spawns
`Runner` (collector + sidecar), points a webview at the bundled dashboard assets, shows a
system tray (Open SkyFall / Quit, close-to-tray), pumps every `AppEvent::Alert` into a native
notification when `[notifications] desktop_notifications = true`, and exposes
launch-at-login.

There is **no local website**. SkyFall never binds a port: the dashboard is a static asset
bundle compiled into the app, and the core talks to it over Tauri IPC — commands such as
`snapshot`, `history`, `alerts`, `get_config`, `ai_status` for queries, plus a
`skyfall://snapshot` event that pushes every sample as it happens. The window shows live
gauges and charts, GPU cards, disk bars, temperatures, and a sortable process table; history
is backfilled from SQLite on first sample and then updated from the event stream.

Run `skyfall-desktop` (or `cargo run -p skyfall-app --bin skyfall-desktop`) to get that UI. The
headless `skyfall-core -- run` binary still works for collection-only and scripting.

### Force an alert

The bundled script does this end to end and blocks until the AI verdict is stored,
so there is nothing to eyeball:

```sh
./shell/force_alert.sh              # real model: qwen2.5:3b via Ollama
./shell/force_alert.sh --mock-llm   # same pipeline, fake LLM (seconds, no Ollama)
./shell/force_alert.sh --no-ai      # rule alerts only, no narration
SKYFALL_DESKTOP=1 ./shell/force_alert.sh   # same, but with native notifications
```

It seeds a short history ramp, trips a `cpu > 20%` rule, releases the load, and prints
the `root_cause` it stored. Expect ~15 s for the default `qwen2.5:3b` on a CPU-only machine
(up to ~90 s on 7B) — the model is loaded for the call and released again afterwards
(`keep_alive_s = 0`).

Three things the old hand-rolled recipe got wrong, all of which silently suppress AI
output rather than erroring:

- **`storage_path = ":memory:"` disables the causal dossier.** The narrator attaches
  history-derived trends and resource hogs from the SQLite file; with an in-memory
  database there is no evidence, so there is nothing to explain. Use a real file.
- **The narrator used to run inline**, so one slow verdict stopped the sidecar reading
  stdin, the core's 64KB pipe filled, and sampling froze until the model answered. It is
  now on a background worker, so detection stays real-time and alerts are stored first
  and annotated when the verdict lands.
- **Saturating every core starves the model.** The script loads about a third of the
  machine, which still clears the threshold, then stops the load before waiting.

If you prefer to do it by hand, the essentials are: a file-backed `storage_path`, a
sidecar config with `"narrator": {"enabled": true, ...}`, a rule with a low threshold
(e.g. `"gt": 20.0, "need": 2, "of": 3`), and a `timeout_s` big enough for a cold
CPU-only load (300 is a safe test value). The core always passes `--db` **and** `--config`
to the sidecar, so keep the two storage paths consistent — the sidecar reads its
detector/narrator knobs from the file the core hands it, not from a stale copy.

### Measure the AI cost

```sh
./shell/bench_narration.sh                          # default model, isolated alerts
./shell/bench_narration.sh --gap 0 --keep-alive 45  # burst: verdicts seconds apart
./shell/bench_narration.sh --model qwen2.5:1.5b --runs 5
```

It prints the load / prefill / decode split Ollama reports, with tok/s per phase, so
"local LLMs are slow" becomes a number you can act on (smaller model, shorter dossier,
fewer verdicts). Every run starts from an explicitly unloaded model unless you ask for a
burst, each dossier differs so the prompt cache cannot flatter the numbers, and the last
run always unloads — a benchmark never leaves 2 GB resident.

### Python AI sidecar

The sidecar is optional but enabled by default. It reads snapshots as NDJSON on stdin and
emits `AnomalyEvent`s as NDJSON on stdout; the Rust core persists those to the `alerts`
table and logs them. Graphics of the engine:

- **Rules** — hard thresholds with hysteresis (CPU/RAM/disk/GPU/temp).
- **Baseline** — rolling per-minute mean/std per metric, sustained `|z| > 3` is "off for you".
- **ML creep** — per-process RSS + disk-fill regression slopes (fast, numpy-only); optional
  IsolationForest when `scikit-learn` is installed.

```sh
cd python/sidecar
python3 -m venv .venv && .venv/bin/pip install -r requirements.txt
.venv/bin/pytest tests/                      # run the suite
# drive it manually:
echo '{"type":"snapshot","ts_ms":1,"system":{...}}' | .venv/bin/python -m skyfall_ai
```

The Rust core locates the sidecar via `python3 -m skyfall_ai` with `PYTHONPATH` pointing at
`python/sidecar`; override the interpreter with `[ai] command` in the config.

### LLM contextualizer (phase 5)

After an anomaly, the sidecar sends a curated dossier to the configured LLM and gets a
structured verdict back: `{severity, headline, root_cause, explanation,
recommended_action, confidence}`. The Rust core persists it in a `narratives` table joined
to the originating alert, and the `alerts` command serves the feed to the UI.

Two wire formats, picked automatically: any OpenAI-compatible endpoint gets
`/v1/chat/completions`; **Ollama is detected** (a cached `GET /api/tags` probe, one per
endpoint, 3 s timeout, and a failed probe means "not Ollama", not "broken") and gets its
native `/api/chat` instead — required, because that is the only route that honours
`keep_alive`.

- **Default = local Ollama** (`base_url http://localhost:11434`, model `qwen2.5:3b`) — private
  and free; point `narrator.base_url` at any OpenAI-compatible provider and set `api_key`.
- **3B on purpose** — consumer laptops are usually CPU-only or have weak integrated
  graphics. A verdict needs ~10 tok/s to finish inside the timeout, and a 7B model on a
  CPU cannot sustain that: on an i7-13700H with no discrete GPU an isolated `qwen2.5:3b`
  verdict measures **~14 s**, while 7B measures **~90 s**. `qwen2.5:3b` is the
  default; Settings has one-click **Fast · 1.5b**, **Balanced · 3b** and **Large · 7b**
  presets (they just fill the model field, so a custom tag still works). Measure the
  trade-off on your own machine with `./shell/bench_narration.sh --model qwen2.5:1.5b`.
- **The model is released after every verdict** (`keep_alive_s`, default `0`) — Ollama
  otherwise keeps a model resident for 5 minutes after the last request *and renews that
  on every call*, which for a monitor that narrates on every alert means a `llama-server`
  runner holding the whole model in RAM (2.2 GB for 3b) almost permanently. SkyFall asks
  Ollama to unload it the moment a verdict lands, so the memory comes back between
  alerts; the cost is a cold model load per verdict, which is affordable because
  narration runs on a background thread. Raise `keep_alive_s` to trade RAM back for
  latency. Note this needs Ollama's native route — `/v1/chat/completions` silently drops
  `keep_alive` — so the narrator probes `/api/tags` once per endpoint and uses
  `/api/chat` when it is talking to Ollama.
- **A burst costs at most 3 verdicts, not one per sensor** (`max_per_burst`, default 3) —
  `throttle_s` is keyed on `(kind, scope)`, and a scope is a *sensor*, so one snapshot in
  which 32 `coretemp` sensors cross 85 °C is 32 separate throttle keys. Two guards fix
  that: alerts carrying **identical evidence** (same detector, metric and machine-wide
  state line, differing only in which sensor tripped) are collapsed into one verdict that
  names everything it covered and speaks for the *worst* sensor of the group; what is left
  is ranked by severity and capped. Alerts past the cap are not lost — they keep their
  detector verdict in the feed and become narratable again once `throttle_s` passes.
- **Warm only inside a burst** — when one snapshot does produce several verdicts, the model
  is held for ~45 s between them (a cached prompt prefix plus no reload) and released by the
  last one, so a burst ends with nothing resident, exactly like a single alert.
- **What a verdict actually costs** on an i7-13700H with no discrete GPU, `qwen2.5:3b`
  (~800 prompt tokens): `1.82 s` model load, `5.73 s` prefill, `6.52 s` decode for a
  ~150-token verdict — `14.08 s` wall, decoding at ~23 tok/s. Decode dominates, so if you
  want it faster: a smaller model, a shorter dossier, or fewer verdicts
  (`max_per_burst`). Inside a burst the load disappears and prefill drops to ~3.6 s, so
  verdicts cost ~9-11 s each instead of ~14 s. `./shell/bench_narration.sh` prints that
  split for any model, with an explicit unload between runs so cold numbers are cold.
- **The model installs itself** — typing a model name is not enough on a fresh machine,
  because Ollama ships with zero models. The `ai_status` command reports whether the endpoint
  is reachable and whether the model is present, and `ai_pull` streams the download as NDJSON
  events, which the settings dialog renders as a progress bar (bytes, percent). A model that
  is missing is reported as *"not installed — install it in Settings"*, and narration
  **pauses** rather than disabling itself, re-probing every
  5 minutes so a newly installed model is picked up without a restart.
- **Trigger-only** — the model doesn't review every sample, only fired alerts. Every alert
  is still stored and shown with its detector verdict; throttling and the burst cap only
  decide which ones get an AI write-up. `throttle_s` (default 300) is keyed on
  `(kind, scope)`, and 3 consecutive call failures auto-disable narration for the session.
- **Root cause (phase 6)** — the narrator returns a `root_cause` field alongside
  `headline`/`explanation`. To make that honest rather than guesswork, the sidecar opens the
  core's SQLite history **read-only** (`--db`, passed automatically) and enriches every
  dossier with a causal block: per-metric **trends** (current vs. a 10-minute baseline, with
  direction), **co-moving metrics** (what rose together), and **resource hogs** (the processes
  that actually burned CPU/IO over the window, including disk throughput). The prompt tells the
  model to attribute the cause to that evidence and to say so plainly when the evidence is
  insufficient. Degrades cleanly: no DB → no causal block → the old behavior.

### Desktop shell (phase 6)

`crates/app` is a Tauri 2 shell around the unchanged core:

- **Same dashboard, no server** — the webview loads the bundled assets straight from disk
  (`frontendDist`), so the UI is identical anywhere with no port, no host/port config, and no
  chance of another process grabbing the socket.
- **System tray** — a tray icon with *Open SkyFall* / *Quit*; the window close button
  minimizes to tray instead of quitting; left-click on the tray reopens the window.
- **Native notifications** — every `AppEvent::Alert` the core broadcasts is rendered through
  `tauri-plugin-notification` (title = severity, body = the anomaly message). Opt-in via
  `desktop_notifications` (**off** by default): the popup is the only thing the flag withholds, so
  alerts are still written to the database and listed in the dashboard either way. The pump re-reads
  the setting per alert, so toggling it applies without a restart.
- **Launch at login** — `tauri-plugin-autostart` (macOS LaunchAgent, Windows registry /
  Linux `.desktop` autostart), toggled from the dashboard's settings modal.
- **Settings modal** — the dashboard gear button opens a dialog bound to the
  `get_config`/`put_config` commands. Every control carries a plain-language label and a sentence
  explaining the trade-off ("how often to take a reading while busy", "what identifies a
  runaway download"), because the raw key names (`sampling_interval_ms`,
  `idle_cpu_threshold`) mean nothing to most people. The dialog only edits the fields it
  shows and sends back the rest of the bundle, so saving cannot reset `storage_path`,
  `ai`, or the non-narrator sidecar sections.
- **Model install** — the AI fieldset shows live model status from the `ai_status`
  command and, when the model is missing, an **Install** button that streams `ai_pull`
  progress events into a progress bar (real bytes and percent, resumable by re-clicking,
  cancellable). Progress is clamped to its high-water mark because Ollama announces one blob
  at a time.
- **Config hot-reload** — `Config` gained `source_path` + `save()`; `Runner` exposes
  `set_config` / `save_config` / `reload` so the collector, sidecar, and storage cadence can
  change without a restart.

## Configuration

All keys optional — see `crates/core/src/config.rs` and `python/sidecar/skyfall_ai.conf.json`
for the AI side of things. Sample: `config/skyfall.toml`.

Older configs that still contain a `[server]` section load fine: the retired table is dropped
on read with a log line, and it disappears the next time the config is saved.

The narrator's on/off, endpoint, model, and API key live in the sidecar JSON, not in the TOML
— that is the file the Python sidecar reads. It defaults to `~/.config/skyfall/skyfall_ai.json`
(override with `sidecar_config` or `SKYFALL_AI_CONFIG`); if that file does not exist yet the
sidecar uses the packaged `python/sidecar/skyfall_ai.conf.json`, and the first save from
Settings creates it with the tuned rules intact.

| Key | Default | Meaning |
|-----|---------|---------|
| `sampling_interval_ms` | 3000 | active sampling interval (higher load → faster) |
| `idle_interval_ms` | 15000 | interval used below the CPU idle threshold |
| `idle_cpu_threshold` | 10.0 | global CPU% below which idle sampling applies |
| `storage_interval_ms` | 60000 | persist a snapshot at most this often |
| `storage_path` | `./skyfall.db` | SQLite history (`~` expands) |
| `top_processes` | 25 | rank-limited process table stored per snapshot |
| `per_core_cpu` | true | include per-core CPU array |
| `per_process_io` | true | collect per-process disk I/O deltas |
| `echo_stdout` | true | print raw snapshots as NDJSON to stdout |
| `desktop_notifications` | false | pop up a desktop notification per alert (alerts are always recorded and listed) |
| `[ai] enabled` | true | run the Python anomaly-detection sidecar |
| `[ai] command` | `python3` | sidecar interpreter (must have numpy) |
| `[ai] module` | `skyfall_ai` | module given to `command -m` |
| `[ai] sidecar_config` | `~/.config/skyfall/skyfall_ai.json` | sidecar JSON config — where Settings saves the narrator settings |
| `[ai.narrator] enabled` | true | ask the LLM to narrate fired alerts |
| `[ai.narrator] base_url` | `http://localhost:11434` | LLM endpoint; Ollama is detected and served over its native `/api/chat`, anything else over `/v1/chat/completions` |
| `[ai.narrator] model` | `qwen2.5:3b` | LLM model id (must be pulled; Settings can do it) |
| `[ai.narrator] api_key` | — | Bearer token for cloud providers |
| `[ai.narrator] timeout_s` | 120 | per-call HTTP timeout (covers Ollama's cold model load) |
| `[ai.narrator] throttle_s` | 300 | min gap between narratives for the same alert kind/scope |
| `[ai.narrator] keep_alive_s` | 0 | how long the endpoint keeps the model resident after a verdict (`0` unloads it; Ollama only) |
| `[ai.narrator] max_per_burst` | 3 | most verdicts one batch of alerts may cost (`0` = no cap) |

## Architecture notes

- **Collector** (`collect.rs`): sysinfo 0.39 for CPU (global + per-core), RAM/swap, disks,
  temps, per-process CPU/RAM/disk-I/O deltas, and network throughput deltas. Zero-copy CPU
  sampling is primed on construction so the first reported percentages are real.
- **GPU providers** (`gpu/`): a trait + composite. NVIDIA via NVML (dynamic load — its
  absence is "no NVIDIA", never a crash); Windows AMD/Intel via PDH "GPU Engine" counters
  matched to DXGI adapter LUIDs (temps/power are N/A without vendor SDKs, which is
  documented); Linux AMD via sysfs counters + hwmon; Linux Intel utilization is *derived*
  from the RC6 residency delta (no root needed). Devices dedupe by stable id (PCI BDF).
- **Storage** (`storage.rs`): SQLite in WAL mode; atomic per-snapshot transactions into
  `system_metrics`, `process_metrics`, `disk_metrics`, `gpu_metrics`, plus the `alerts`
  table the sidecar feeds. History pulls upsample-bucket the series for the dashboard.
- **Dashboard** (`crates/core/assets/dashboard/` + `crates/app`): zero frontend deps —
  vanilla JS rendering into `<canvas>` (hover crosshairs included). The assets are loaded as
  Tauri's `frontendDist`, so there is no HTTP server, no embedding code, and no working-directory
  dependency. The shell pushes each sample to the page with the `skyfall://snapshot` event; the
  alert feed pulls `alerts` every 5 s and renders severity badges, the LLM headline, and
  expandable explanation/recommended-action.
- **Shutdown** (`main.rs`): `ctrl_c` is subscribed once for the whole run so a SIGINT that
  lands mid-tick (or during a spinning loop) is never dropped; on it, the core sends a
  `shutdown` line to the sidecar child, finalizes the WAL, and exits cleanly.
- **Alert context** (`history.py` + `detection.py`): a single snapshot only says *what* is hot.
  The sidecar re-reads the last 10 minutes of `system_metrics` / `process_metrics` through a
  read-only SQLite connection to derive direction, co-movement, and per-process attribution —
  the evidence a root-cause claim needs. It is strictly advisory and never writes to the DB.

## Not yet built (see ROADMAP)

- Consumer packaging: bundled Python runtime, installers (phase 7)
- History rollups, anomaly tuning UI, privacy docs (phase 8)

## Screenshots
<img width="1902" height="1135" alt="image" src="https://github.com/user-attachments/assets/ca276651-344f-4082-8d9c-db5ab7c517e8" />
