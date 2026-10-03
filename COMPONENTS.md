**SkyFall is a smart system monitor.** It watches your computer the way Task Manager does, then decides when something looks *wrong for you*, explains why, and tells you — in a dashboard, and as a desktop notification if you turn those on.

---

## What it does

Every few seconds it takes a snapshot of:

- CPU, RAM, disks, network, temperatures  
- top processes  
- GPUs (when the OS/drivers allow it)

Those snapshots go into a local SQLite database. A Python “AI sidecar” looks at them and can fire an **alert**. A local language model (Ollama by default) then writes a short explanation: what happened, likely cause, what you might do.

It is built so the monitor itself stays cheap (Rust), while detection and wording stay easy to change (Python).

---

# How it works (one loop)

Think of a heartbeat:

1. **Collect** — Rust reads OS stats (`sysinfo` + GPU providers).
2. **Show live** — the latest snapshot is pushed to the dashboard (a Tauri event).
3. **Remember** — on a slower cadence (default every 60s) it writes history to SQLite.
4. **Judge** — the same snapshot is sent as one JSON line to a Python child process.
5. **Alert** — if Python says “anomaly,” Rust stores it and can pop a notification.
6. **Explain** — Python asks the LLM in the background (so a slow model does not freeze sampling), then Rust stores the write-up next to the alert.
7. **Sleep** — shorter interval if the machine is busy, longer if idle.

The dashboard is a small set of static files bundled into the app — there is no web server and
no port. The desktop app shows those files in a window, plus a tray icon.

Communication with Python is **NDJSON on stdin/stdout**: one JSON object per line. Rust never depends on Python staying up — if the sidecar dies, collection continues and alerts just stop.

---

## The parts

### 1. Rust core (`crates/core`) — the engine

| File | Role |
|------|------|
| `collect.rs` | Takes one snapshot of the machine |
| `metrics.rs` | The snapshot shape (CPU, RAM, processes, …) |
| `gpu/` | GPU readers: NVIDIA (NVML), Linux sysfs, Windows PDH; missing GPU = skip, not crash |
| `storage.rs` | SQLite history + alerts + LLM write-ups |
| `config.rs` | TOML settings (how often to sample, storage, AI on/off) |
| `sidecar.rs` | Starts Python, feeds snapshots, reads alerts back |
| `api.rs` | The queries the UI asks for (snapshot, history, alerts, config) |
| `runner.rs` | Ties it all together in a background loop |
| `main.rs` | CLI: `collect` (one snapshot) or `run` (keep going) |
| `assets/dashboard/` | HTML/JS/CSS charts, process table, alert feed, settings |

`Runner` is the important glue: both the CLI and the desktop app start it and then just listen.

### 2. Python sidecar (`python/sidecar/skyfall_ai`) — “is this weird?”

| File | Role |
|------|------|
| `__main__.py` | Reads stdin, runs detection, writes events out |
| `engine.py` | Runs all detectors on each snapshot |
| `detection.py` | **Rules** (hard limits) + **baseline z-scores** (“unusual *for you*”) + **creep** (slow RAM leak / disk fill) |
| `isolation.py` | Optional IsolationForest if scikit-learn is installed |
| `history.py` | Read-only look at the last ~10 minutes of SQLite for trends and “who used the CPU” |
| `narrator.py` | Asks the LLM what to say (native Ollama `/api/chat` or any OpenAI-compatible API) and decides how *often* to ask |
| `protocol.py` | NDJSON in/out |

Detectors run in order: rules → baseline → creep → ML. Fired alerts get a **dossier** (trends + related metrics + hog processes). The LLM is only called on alerts, not every sample.

### 3. Desktop shell (`crates/app`) — the “app” wrapper

Tauri 2 around the same `Runner`:

- window showing the bundled dashboard assets  
- system tray (open / quit; close hides to tray)  
- native notifications when an alert fires — opt-in, off by default  
- launch at login  

The same dashboard assets are used everywhere, with no server to start.

### 4. Config and extras

- `config/skyfall.toml` — sampling, storage path, AI, notifications  
- `python/sidecar/skyfall_ai.conf.json` — detector + narrator knobs (the core passes it to the sidecar with `--config`; Settings saves to `~/.config/skyfall/skyfall_ai.json`)  
- `shell/force_alert.sh` — force a real end-to-end alert and wait for the verdict  
- `shell/bench_narration.sh` — measure what a verdict costs (load / prefill / decode, cold vs. burst)  

---

## How often the LLM is asked, and what that costs

A local model is the slowest thing in the loop, and it watches the same machine it is
reasoning about — so the narrator's job is as much *not calling* as calling:

| Control | Default | What it stops |
|---------|---------|---------------|
| `throttle_s` | 300 | A repeat of the same alert kind/scope within 5 minutes |
| fingerprint dedupe | — | 32 identical temperature alerts (one `coretemp` sensor per core) becoming 32 verdicts; they collapse into one that names every sensor it covered and speaks for the worst |
| `max_per_burst` | 3 | One snapshot with many distinct alerts becoming many verdicts; what is left is ranked by severity, and overflow waits for the next `throttle_s` window |
| `keep_alive_s` | 0 | A model sitting resident in RAM for 5 minutes after every verdict, renewed on every call |

Only Ollama is affected by `keep_alive`, and only on its **native** `/api/chat` route —
`/v1/chat/completions` accepts the field and drops it. So the narrator probes `/api/tags`
once per endpoint (cached) and picks the route; a failed probe just means "not Ollama".

Inside a burst the model is deliberately held for ~45 s between verdicts (cached prompt
prefix, no reload) and released by the last one, so a burst ends as clean as a single alert.
Nothing above throws an alert away: an alert that misses the cap is still stored and still
shown, with its detector verdict and no AI prose.

Measured on an i7-13700H with no discrete GPU (`qwen2.5:3b`, ~800 prompt tokens): **14.08 s**
for an isolated verdict (1.82 s load + 5.73 s prefill + 6.52 s decode, ~23 tok/s) and
**~9–11 s** each inside a burst. `./shell/bench_narration.sh` reproduces those numbers for any
model, with an explicit unload between runs so "cold" actually means cold.

---

## Two ways to run it

- **Headless:** `cargo run -p skyfall-core -- run` → collect + detect, no UI  
- **Desktop:** `cargo run -p skyfall-app --bin skyfall-desktop` → tray + window + OS notifications  

Same collector, same DB, same Python sidecar.

---

## Mental model

```
OS stats  →  Rust collector  →  SQLite + live dashboard
                 │
                 ▼  (JSON lines)
           Python detectors  →  alert
                 │
                 ▼  (background)
           LLM “why”  →  stored next to the alert  →  UI / notification
```

Rust measures and stores. Python decides. The LLM explains. The app window is just a view onto that loop.







# PYTHON SIDE CAR INDEPTH

The Python side is **not one model**. Detection is three statistical layers plus one optional sklearn model. The LLM does **not** detect anything; it only writes after an alert already fired.

---

## The only “real” ML: Isolation Forest

**Algorithm:** scikit-learn’s `IsolationForest` (`n_estimators=64`, `contamination="auto"`). Unsupervised. No labels, no “good vs bad” training. It learns what *recent* system snapshots look like, then flags a snapshot whose **combination** of numbers is rare.

**Idea:** isolation forests isolate odd points by randomly splitting features. Typical points take many splits; weird points take few. SkyFall uses `decision_function` (higher = more normal) and treats the **bottom 3% of scores in the rolling window** as the cutoff.

**Input:** one 6-number vector per snapshot, all **machine-wide**, not per process:

| Feature | What it encodes |
|---------|-----------------|
| CPU total | scaled 0–1 |
| RAM % | scaled 0–1 |
| load average (1 min) | capped at 5, then /5 |
| network **receive** rate | capped as if 500 Mbps is “full” |
| hottest temperature | /100, or 0 if none |
| busiest GPU util | /100, or 0 if none |

It keeps the last **300** of those vectors, needs **120** before it will alert, **refits on every sample**, then checks the *current* vector.

**What it is looking for:** weird **joint** states a single-metric z-score misses. Example: CPU not crazy, RAM not crazy, GPU not crazy — but that *mix* (say high GPU + high download + odd temp with low CPU) almost never happened in the last few hours.

It must stay odd for **3 of the last 5** samples, then it is silenced for **5 minutes**. Severity is always `medium`. Scope is always `global`.

**What it is not looking for:**

- Individual processes, names, PIDs, memory leaks, disk I/O, disk fullness  
- Network **upload**, per-core CPU, GPU memory, clocks, power  
- Malware, crashes, errors, “this app is bad”  
- Time-of-day (“you always compile at 5pm”)  
- Slow one-metric drift that still looks like a plausible combo  
- Future prediction / forecasting  

If scikit-learn is missing, this detector is **off**. Rules, z-scores, and creep still run.

---

## The other “ML-ish” detectors (not Isolation Forest)

### 1. Baseline z-score (`BaselineZ`) — classical stats

Per metric, per scope (global CPU, each disk, each GPU, each temp sensor): rolling **mean and standard deviation** of **per-minute averages**, default window **7 days**, **30 minutes warmup**. Alert if **|z| > 3** — “this number is rare *for you*,” not “above 90%.”

**Looking for:** a sudden or fairly sharp step away from *your* normal (CPU usually 8%, now 40%).

**Not looking for:** combinations of metrics; anything in the first half hour; very slow change (the window **absorbs** a leak, so z stays small — that is why creep exists).

### 2. Creep — ordinary least-squares slope

Not sklearn. Fit a straight line to the last ~**4 hours** of per-minute points (needs ≥ **8** minutes).

**Looking for only two things:**

- A process’s **RSS** rising ≥ ~**15 MB/min** → “possible memory leak”  
- A disk’s **used %** rising ≥ **0.5%/min** → “filling, ~X hours until full”

**Not looking for:** CPU creep, GPU VRAM leaks, network, temps, “this process uses a lot of RAM but stably,” or a leak slower than that slope.

### 3. Rules — not ML at all

Fixed ceilings (CPU >90%, RAM >95%, disk >90% full, GPU >95%, temp >85°C) that must **hold for several samples**. These catch “objectively hot,” even if that *is* your baseline (e.g. you always game at 95% GPU — the rule still fires; z-score might not).

---

## How they split the job

| Detector | Question it answers |
|----------|---------------------|
| Rules | Is this *absolutely* too high? |
| Z-score | Is this *unusual for this machine*? |
| Creep | Is RAM or disk *quietly trending up*? |
| Isolation Forest | Is this *mix* of system stats unlike recent mixes? |
| LLM | After any of the above: *what should we say?* |

So the Isolation Forest is a **combo / leftover** net: it does not name a process, does not prove a leak, and does not mean “broken.” It means “this whole-machine signature is an outlier vs the last few hundred samples.” Attribution (which process, which trend) is added later from SQLite history for the narrator, not by the forest itself.
