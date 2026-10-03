# SkyFall — Consumer AI System Monitor

A smart, cross-platform system monitor that watches CPU, GPU, RAM, disk, network, and
per-process activity, detects when things look off *for you*, explains *why* with
context, and alerts you — in a live dashboard, and as a desktop notification if you opt in.

- **Stack:** Rust core + Tauri shell + Python AI sidecar
- **Platforms:** Windows / macOS / Linux
- **AI model:** Hybrid — statistical/ML anomaly detection over your own baseline, then a
  local LLM (Ollama) narrates the anomaly with context; cloud LLM pluggable
- **UI:** systray HUD, native desktop notifications, embedded dashboard (live charts,
  process table, history, alert feed)

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

## Why this split

The Rust collector has ~zero overhead — important for a *monitor*. Python keeps the
ML/LLM loop fast to iterate on. Rust fires notifications; Python decides *when* and
*what to say*.

---

## Components

### 1. Data collection (Rust)

- **`sysinfo`** — CPU per-core, RAM/swap, disks, network, per-process (CPU/mem/disk/name/pid)
  on all three OSes. This is the "task manager" layer.
- **GPU — a trait + per-OS providers, degrading gracefully:**
  - NVIDIA (Linux/Win): `nvml-wrapper` — dynamic load of `libnvidia-ml`/`nvml.dll`, so its
    absence is "no NVIDIA", never a crash. Full util/mem/temp/power/clocks.
  - AMD/Intel on **Windows**: PDH "GPU Engine" counters (util% + VRAM). `gpuviewer-core`
    WDDM backend (DXGI + PDH + D3DKMT) and `hypomnesis` for VRAM. **N/A:** temps/power/
    clocks on non-NVIDIA without vendor SDKs.
- AMD on **Linux**: sysfs `gpu_busy_percent` + `mem_info_*` + `hwmon` (temps/power/clocks).
- Intel on **Linux**: utilization derived from `rc6_residency_ms` delta (no root needed,
  verified); clocks from `gt/gt0/rps_cur_freq_mhz` / `gt_cur_freq_mhz`; temps from
  `hwmon` when the driver exposes them (iGPU often doesn't → N/A).
  - **macOS:** no NVIDIA; Apple Silicon per-GPU utilization is not exposed unprivileged.
    v1 = unified RAM + memory pressure + per-process; optional root `powermetrics`.
- Sampling at 2–5 s while active, throttled when idle (battery/heat friendly).

### 2. Storage

- **SQLite** (`rusqlite`): per-minute rows for system + top-process snapshots, hourly/daily
  rollups, and an `alerts` table. GPU identity = PCI BDF on Windows / stable key elsewhere
  so history survives reboots.

### 3. Anomaly detection (Python sidecar)

Three tiers, all debounced/coalesced so alerts don't spam:

1. **Rules** — hard thresholds with hysteresis that must hold for N of the last 5 samples:
   CPU >90%, RAM >95%, disk >90% full, GPU >95%, temp >85°C (critical), each with a
   per-rule cooldown.
2. **Baseline anomaly** — rolling mean/std of **per-minute averages** per metric *and per
   scope* (global CPU, each disk, each GPU, each temp sensor), default window **7 days**
   (capped at 30) with a 30-minute warmup; sustained `|z| > 3` = "off" *for you* (learns
   your normal).
3. **ML creep** — ordinary least-squares slope over the last 4 hours of per-minute points:
   a process RSS rising ≥15 MB/min or a disk filling ≥0.5%/min, plus optional
   `scikit-learn` IsolationForest on 6-feature machine-wide vectors for weird *combinations*.

Output: `AnomalyEvent { ts_ms, kind, metric, scope, severity, message, dossier }`.

### 4. LLM context layer

- On anomaly, Python curates a **2–4 KB dossier** (window vs baseline, top-process deltas,
  temps, recent events, plus a causal block read from the core's SQLite history) and asks
  the model for a structured verdict: `{severity, headline, root_cause, explanation,
  recommended_action, confidence}`.
- **Default = local Ollama** (`qwen2.5:3b`) for privacy and zero cost; cloud
  (OpenAI/Anthropic) pluggable in config, same client API either way. Ollama is detected
  by a cached `/api/tags` probe and served over its native `/api/chat`, because
  `/v1/chat/completions` silently drops `keep_alive`.
- Calls are **trigger-only** (a local LLM pollutes the exact metrics it watches) and
  controlled so a storm of alerts cannot become a storm of inferences: `throttle_s` per
  `(kind, scope)`, fingerprint dedupe across sibling sensors, a severity-ranked
  `max_per_burst` cap, and `keep_alive_s = 0` so the model is released after every burst.
  `shell/bench_narration.sh` measures the cost of any of those trade-offs.

### 5. UI / alerts (Tauri shell)

- `tauri-plugin-notification` (cross-platform native notifications, macOS permission
  prompt, Windows Focus Assist), `tauri-plugin-tray` (systray HUD), `tauri-plugin-autostart`.
  Desktop notifications are opt-in (`desktop_notifications`, off by default); the alert is
  always stored and shown in the feed regardless.
- **Dashboard** = static HTML/JS/CSS with **zero frontend dependencies** (hand-rolled canvas
  charts, sortable process table, history graphs with range select) bundled into the app and
  driven over Tauri IPC (commands for queries, a `skyfall://snapshot` event for live pushes):
  live gauges, top processes by CPU/RAM/disk, alert feed with LLM write-ups. No local
  server, no port.
- Config in TOML (XDG / AppData).

---

## Repo layout (target)

```
SkyFall/
├── ROADMAP.md
├── COMPONENTS.md                 # component tour: snapshot → alert → verdict
├── Cargo.toml                     # workspace
├── crates/
│   ├── core/                      # collector, storage, UI queries, GPU providers
│   └── app/                       # Tauri shell (phase 6)
├── python/
│   └── sidecar/                   # anomaly detection + LLM (phase 4/5)
├── shell/                         # force_alert.sh, bench_narration.sh, installers (phase 7)
└── config/
    └── skyfall.toml               # defaults
```

---

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


---

## Next Steps
- Test Linux Build
- Test Windows Build
- Package and release v0.1.0