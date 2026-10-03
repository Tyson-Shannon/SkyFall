#!/usr/bin/env bash
# Force a SkyFall alert and wait for the AI-written explanation.
#
# The README's old "force an alert" recipe only produced rule alerts, and used
# `storage_path = ":memory:"`, which silently disables the whole history dossier
# (the narrator gets no evidence, so there is nothing to explain). This script
# uses a file-backed database, seeds a realistic trend, and blocks until a
# `root_cause` actually shows up in the `narratives` row joined to the alert.
#
# There is no HTTP API any more, so this reads the same SQLite database the UI
# reads instead of polling a dashboard endpoint.
#
#   ./shell/force_alert.sh              # full stack: core + sidecar + real model
#   ./shell/force_alert.sh --mock-llm   # same pipeline, fake LLM (instant, no Ollama)
#   ./shell/force_alert.sh --no-ai      # rule alerts only, no narration
#   ./shell/force_alert.sh --keep       # leave the app running when done
#
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MODE="full"
KEEP=0
MODEL="${SKYFALL_TEST_MODEL:-qwen2.5:3b}"
TIMEOUT="${SKYFALL_TEST_TIMEOUT:-300}"
# Desktop popups are opt-in (`desktop_notifications = false` by default), so ask
# for one whenever this run is the desktop shell: the point of SKYFALL_DESKTOP=1
# is to watch the notification arrive.
DESKTOP_NOTIFICATIONS=false
[ "${SKYFALL_DESKTOP:-0}" = "1" ] && DESKTOP_NOTIFICATIONS=true

for arg in "$@"; do
  case "$arg" in
    --mock-llm) MODE="mock" ;;
    --no-ai)    MODE="noai" ;;
    --keep)     KEEP=1 ;;
    --help|-h)  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg (try --help)"; exit 2 ;;
  esac
done

WORK="$(mktemp -d /tmp/skyfall-forcealert.XXXXXX)"
DB="$WORK/history.db"
AI_JSON="$WORK/sidecar.json"
TOML="$WORK/skyfall.toml"
LOG="$WORK/core.log"
MOCK_PID=""; APP_PID=""

cleanup() {
  if [ "$KEEP" = "1" ]; then
    echo
    echo "still running: SkyFall  (log: $LOG)"
    echo "database:      $DB"
    echo "kill with:     pkill -f skyfall"
    return
  fi
  for p in "$APP_PID" "$MOCK_PID"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  # only stop load generators we started
  [ -f "$WORK/load.pids" ] && while read -r p; do kill "$p" 2>/dev/null; done < "$WORK/load.pids"
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

say()  { printf '\n\033[1m%s\033[0m\n' "$*"; }
info() { printf '  %s\n' "$*"; }
bad()  { printf '  \033[31m%s\033[0m\n' "$*"; }
good() { printf '  \033[32m%s\033[0m\n' "$*"; }

# --------------------------------------------------------------- preflight --
say "Preflight"

BIN_CORE="$ROOT/target/debug/skyfall"
BIN_DESKTOP="$ROOT/target/debug/skyfall-desktop"
if [ ! -x "$BIN_CORE" ]; then
  bad "core binary missing - run: cargo build"
  exit 1
fi
if [ ! -d "$ROOT/python/sidecar/skyfall_ai" ]; then
  bad "python sidecar missing"
  exit 1
fi
info "core:      $BIN_CORE"
info "workdir:   $WORK"

if [ "$MODE" = "full" ]; then
  if ! command -v ollama >/dev/null 2>&1; then
    bad "Ollama is not installed. Install it, or rerun with --mock-llm."
    exit 1
  fi
  if ! curl -s -m 3 http://localhost:11434/api/tags >/dev/null 2>&1; then
    bad "Ollama is installed but not running. Start it (systemctl --user start ollama), or rerun with --mock-llm."
    exit 1
  fi
  HAVE_MODEL="$(curl -s -m 5 http://localhost:11434/api/tags \
    | python3 -c "import sys,json;print('yes' if any('$MODEL' in m['name'] for m in json.load(sys.stdin)['models']) else 'no')" 2>/dev/null)"
  if [ "$HAVE_MODEL" != "yes" ]; then
    bad "model '$MODEL' is not installed."
    info "Easiest: open SkyFall, gear icon -> AI explanations -> Install."
    info "Or run: ollama pull $MODEL"
    exit 1
  fi
  good "Ollama running, model '$MODEL' installed"
fi

# ------------------------------------------------------- seed fake history --
# The narrator can only name a cause if it has evidence. A cold database gives it
# a single sample and nothing to compare against, so seed a believable ramp that
# ends just before the real load starts.
say "Seeding history (so the AI has a trend to reason about)"
python3 - "$DB" <<'PY'
import os, sqlite3, sys, time
db = sys.argv[1]
if os.path.exists(db):
    os.remove(db)
c = sqlite3.connect(db)
c.executescript("""
CREATE TABLE system_metrics (ts INTEGER PRIMARY KEY, cpu_total REAL, ram_pct REAL,
 ram_used INTEGER, swap_used INTEGER, load1 REAL, load5 REAL, load15 REAL,
 net_rx_bps INTEGER, net_tx_bps INTEGER);
CREATE TABLE process_metrics (ts INTEGER, pid INTEGER, name TEXT, cpu REAL,
 ram_kb INTEGER, disk_r_bps INTEGER, disk_w_bps INTEGER, PRIMARY KEY (ts,pid));
""")
now = int(time.time() * 1000)
# calm -> busy ramp over the last 3 minutes
for i in range(7):
    ts = now - (7 - i) * 25000
    frac = i / 6.0
    c.execute(
        "INSERT OR REPLACE INTO system_metrics VALUES (?,?,?,0,0,?,0,0,0,?)",
        (ts, 6 + 8 * frac, 45 + 6 * frac, 0.3 + 1.4 * frac, 200000 + 400000 * frac))
# a build-ish workload already winding up
for pid, name, cpu, ram, w in [
    (4242, "rustc", 71.0, 910000, 26_000_000),
    (4243, "cc1plus", 58.0, 420000, 0),
    (5150, "cargo", 12.0, 180000, 3_000_000),
    (99, "bash", 1.5, 4000, 0),
]:
    c.execute("INSERT OR REPLACE INTO process_metrics VALUES (?,?,?,?,?,0,?)",
              (now, pid, name, cpu, ram, w))
c.commit(); c.close()
print("  seeded 7 system samples + 4 processes")
PY

# ------------------------------------------------------------------ config --
say "Writing test config"
NARRATOR_BLOCK=""
if [ "$MODE" = "full" ]; then
  # timeout_s must cover Ollama's cold model load: measured ~93s for the FIRST
  # qwen2.5:3b call on a CPU-only laptop. throttle_s low so repeat runs are fast.
  NARRATOR_BLOCK=",\"narrator\":{\"enabled\":true,\"base_url\":\"http://127.0.0.1:11434\",
    \"model\":\"$MODEL\",\"timeout_s\":300.0,\"throttle_s\":0.0}"
elif [ "$MODE" = "mock" ]; then
  NARRATOR_BLOCK=",\"narrator\":{\"enabled\":true,\"base_url\":\"http://127.0.0.1:18781\",
    \"model\":\"mock\",\"timeout_s\":30.0,\"throttle_s\":0.0}"
fi

cat > "$AI_JSON" <<EOF
{
  "baseline": {"window_minutes": 10080, "warmup_minutes": 10080},
  "creep": {"enabled": false},
  "rules": {
    "cpu_spike": {"metric": "system.cpu_total", "gt": 20.0, "need": 2, "of": 3,
                  "hysteresis": 0.0, "severity": "high", "cooldown_s": 600.0}
  },
  "ml": {"enabled": false}$NARRATOR_BLOCK
}
EOF

cat > "$TOML" <<EOF
sampling_interval_ms = 700
idle_interval_ms = 700
idle_cpu_threshold = 99.0
storage_interval_ms = 500
storage_path = "$DB"
top_processes = 12
per_core_cpu = false
per_process_io = true
echo_stdout = false
# Popups are opt-in; the desktop run below is pointless without one.
desktop_notifications = $DESKTOP_NOTIFICATIONS

[ai]
enabled = true
command = "python3"
module = "skyfall_ai"
sidecar_config = "$AI_JSON"
EOF
info "threshold: cpu > 20% for 2 of 3 samples (fires on almost anything)"
info "one alert only: rule cooldown 600s, baseline_z held in warmup"
info "storage:    $DB  (file-backed, so the dossier has history)"
[ "$MODE" = "full" ] && info "narrator:   on, $MODEL, 300s timeout"

# ------------------------------------------------------------ mock the LLM --
if [ "$MODE" = "mock" ]; then
  cat > "$WORK/mock.py" <<'PY'
import json, threading
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("content-length", 0)); self.rfile.read(n)
        out = {"choices": [{"message": {"content": json.dumps({
            "severity": "high",
            "headline": "MOCK: CPU saturation from busy loops",
            "root_cause": "MOCK: many busy shell loops pegging all cores",
            "explanation": "MOCK: CPU rose with load while disk writes stayed flat, "
                           "which points at compute rather than I/O.",
            "recommended_action": "MOCK: stop the busy loops",
            "confidence": 0.95})}}]}
        b = json.dumps(out).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(b)))
        self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
threading.Thread(target=HTTPServer(("127.0.0.1", 18781), H).serve_forever, daemon=True).start()
import time
while True: time.sleep(3600)
PY
  python3 "$WORK/mock.py" & MOCK_PID=$!
  sleep 1
  info "mock LLM listening on 127.0.0.1:18781"
fi

# ------------------------------------------------------------------- start --
say "Starting SkyFall"
if [ -x "$BIN_DESKTOP" ] && [ "${SKYFALL_DESKTOP:-0}" = "1" ]; then
  info "using the Tauri desktop shell (native notifications)"
  DISPLAY="${DISPLAY:-:0}" "$BIN_DESKTOP" --config "$TOML" run >"$LOG" 2>&1 & APP_PID=$!
else
  PYTHONPATH="$ROOT/python/sidecar" "$BIN_CORE" --config "$TOML" run >"$LOG" 2>&1 & APP_PID=$!
  info "using the core (headless). Set SKYFALL_DESKTOP=1 for native notifications."
fi

# No health endpoint to poll any more, so wait for the collector's own banner.
for _ in $(seq 1 40); do
  kill -0 "$APP_PID" 2>/dev/null || break
  grep -q "sampling every" "$LOG" 2>/dev/null && break
  sleep 0.5
done
if ! kill -0 "$APP_PID" 2>/dev/null; then
  bad "skyfall exited immediately - log follows"; tail -20 "$LOG"; exit 1
fi
if ! grep -q "sampling every" "$LOG"; then
  bad "collector never started - log follows"; tail -20 "$LOG"; exit 1
fi
good "collector running (pid $APP_PID)"

# Confirm the narrator actually attached, before we wait on it.
HELLO="$(grep -o '"narrator":"[^"]*"' "$LOG" | head -1)"
if [ -n "$HELLO" ]; then
  if [[ "$HELLO" == *'"on'* ]]; then good "narrator: $HELLO"
  else bad "narrator NOT running: $HELLO"; info "AI output will not appear."; fi
fi

# --------------------------------------------------------------- add load --
# Deliberately NOT all cores. Ollama needs CPU to answer, and a machine pegged
# by our own load generators will starve it into a timeout: a verdict that takes
# ~90s on a quiet CPU-only laptop will not finish in 300s next to 20 spinners.
# Target roughly a third of the box, which still clears the 20% threshold.
say "Generating CPU load"
NCPU="$(nproc 2>/dev/null || echo 4)"
N=$(( NCPU / 3 )); [ "$N" -lt 2 ] && N=2
: > "$WORK/load.pids"
for _ in $(seq 1 "$N"); do
  ( timeout 400 bash -c 'while :; do :; done' & echo $! >> "$WORK/load.pids" ) 2>/dev/null
done
info "$N busy loops on $NCPU cores (about $(( N * 100 / NCPU ))% CPU)"

stop_load() {
  [ -s "$WORK/load.pids" ] || return 0
  while read -r p; do kill "$p" 2>/dev/null; done < "$WORK/load.pids"
  : > "$WORK/load.pids"
  info "load released, CPU is free for the model"
}

count_alerts() {
  python3 - "$DB" <<'PY' 2>/dev/null || echo 0
import sqlite3, sys
try:
    c = sqlite3.connect("file:%s?mode=ro" % sys.argv[1], uri=True, timeout=1)
    print(c.execute("SELECT count(*) FROM alerts").fetchone()[0])
except Exception:
    print(0)
PY
}

show_verdict() {
  python3 - "$DB" <<'PY' 2>/dev/null
import sqlite3, sys
c = sqlite3.connect("file:%s?mode=ro" % sys.argv[1], uri=True, timeout=1)
r = c.execute(
    "SELECT n.headline, n.root_cause, n.recommended_action, n.explanation,"
    "       n.confidence, a.dossier "
    "FROM alerts a JOIN narratives n ON n.alert_id = a.id "
    "WHERE n.root_cause IS NOT NULL AND n.root_cause != '' "
    "ORDER BY a.ts DESC, a.id DESC LIMIT 1").fetchone()
if not r:
    raise SystemExit(0)
for k, v in zip(("HEADLINE", "CAUSE", "ACTION", "EXPLAIN", "CONF", "DOSSIER"), r):
    if v is not None:
        print("%s\t%s" % (k, v))
PY
}

# ------------------------------------------------- phase 1: trip the rule --
say "Phase 1/2: waiting for the alert to be recorded"
FIRED=0
for _ in $(seq 1 45); do
  if [ "$(count_alerts)" -gt 0 ]; then FIRED=1; break; fi
  sleep 1
done
if [ "$FIRED" != "1" ]; then
  bad "the rule never fired"
  echo
  info "  the threshold is cpu > 20% for 2 of 3 samples, so this is a detection problem"
  info "  log tail:"
  tail -12 "$LOG" | sed 's/^/    /'
  exit 1
fi
good "alert stored: $(count_alerts) row(s) in the feed"

if [ "$MODE" = "noai" ]; then
  say "Rule alert only (--no-ai), nothing further to wait for"
  say "Log tail"
  grep -E "alert \[" "$LOG" | tail -4 | sed 's/^/  /'
  exit 0
fi

# ------------------------------------- phase 2: let the model do its job --
# The alert is captured with the load in the dossier already; now give Ollama a
# quiet machine, which is the difference between ~90s and a timeout.
stop_load
say "Phase 2/2: waiting for the AI explanation (up to ${TIMEOUT}s)"
info "narrating on a quiet CPU; the model is loaded for the call and released after (keep_alive_s=0)"
DEADLINE=$(( $(date +%s) + TIMEOUT ))
FOUND=0
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  OUT="$(show_verdict)"
  if [ -n "$OUT" ]; then FOUND=1; break; fi
  sleep 3
done

if [ "$FOUND" = "1" ]; then
  say "AI explanation"
  printf '%s\n' "$OUT" | while IFS=$'\t' read -r k v; do
    case "$k" in
      HEADLINE) printf '  \033[1m%s\033[0m\n' "$v" ;;
      CAUSE)    printf '  likely cause: %s\n' "$v" ;;
      ACTION)   printf '  suggested:    %s\n' "$v" ;;
      EXPLAIN)  printf '  why:          %s\n' "$v" ;;
      CONF)     printf '  confidence:   %s\n' "$v" ;;
      DOSSIER)   printf '  evidence:     %s\n' "$v" ;;
    esac
  done
  echo
  good "end-to-end OK: snapshot -> rule -> history dossier -> LLM -> SQLite alerts+narratives"
  say "Log tail"
  grep -E "alert \[|narrative \[" "$LOG" | tail -4 | sed 's/^/  /'
  exit 0
fi

bad "alert stored, but no AI explanation after ${TIMEOUT}s"
echo
info "sidecar status and log tail:"
grep -o '"narrator":"[^"]*"' "$LOG" | tail -1 | sed 's/^/    narrator: /'
grep -iE "narrator|Traceback|Error|skyfall_ai:" "$LOG" | tail -8 | sed 's/^/    /'
echo
info "what to check:"
info "  'not installed' -> ollama pull $MODEL"
info "  'call failed'    -> still too slow; raise narrator.timeout_s (currently 300s)"
info "  'disabled'       -> 3 consecutive failures; see the log lines above"
info "  full log:        $LOG   (rerun with --keep to hold on to it)"
exit 1
