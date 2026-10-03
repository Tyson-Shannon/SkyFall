#!/usr/bin/env bash
# Measure what one AI verdict actually costs on *this* machine.
#
#   ./shell/bench_narration.sh
#   ./shell/bench_narration.sh --model qwen2.5:1.5b --runs 5
#   ./shell/bench_narration.sh --keep-alive 45      # warm runs (no reload)
#
# Every verdict costs three very different things: loading the model, reading the
# prompt (prefill) and writing the answer (decode). On a CPU-only laptop prefill
# and decode dominate, which is why "make it faster" is mostly about the prompt
# and the output length, not about the model size. This prints the split Ollama
# itself reports, so those claims are numbers instead of memories.
#
# Read-only: it writes nothing outside a temp dir, and the final run always
# unloads the model so a benchmark never leaves 2 GB resident.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

MODEL="${SKYFALL_TEST_MODEL:-qwen2.5:3b}"
RUNS=3
KEEP_ALIVE=0
GAP=5

while [ $# -gt 0 ]; do
  case "$1" in
    --model)      MODEL="$2"; shift 2 ;;
    --runs)       RUNS="$2"; shift 2 ;;
    --keep-alive) KEEP_ALIVE="$2"; shift 2 ;;
    --gap)        GAP="$2"; shift 2 ;;
    -h|--help)    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (try --help)"; exit 2 ;;
  esac
done

command -v ollama >/dev/null 2>&1 && OLLAMA_BIN="$(command -v ollama)" \
  || OLLAMA_BIN=/snap/ollama/current/bin/ollama

PYTHONPATH="$ROOT/python/sidecar" \
SKYFALL_BENCH_MODEL="$MODEL" SKYFALL_BENCH_RUNS="$RUNS" \
SKYFALL_BENCH_KEEP="$KEEP_ALIVE" SKYFALL_BENCH_GAP="$GAP" \
SKYFALL_BENCH_PS="$OLLAMA_BIN" \
python3 - <<'PY'
import json, os, subprocess, sys, time, urllib.request

from skyfall_ai.events import AnomalyEvent
from skyfall_ai.narrator import SYSTEM_PROMPT, MAX_TOKENS, Narrator

MODEL = os.environ["SKYFALL_BENCH_MODEL"]
RUNS = int(os.environ["SKYFALL_BENCH_RUNS"])
KEEP = float(os.environ["SKYFALL_BENCH_KEEP"])
GAP = float(os.environ["SKYFALL_BENCH_GAP"])
OLLAMA = os.environ["SKYFALL_BENCH_PS"]
URL = "http://localhost:11434"

# A synthetic-but-realistic dossier: a busy snapshot with a full causal block, so
# the prompt is the size a real alert produces (measured ~800 tokens) rather than
# a flattering 50. Kept here so runs stay comparable as the narrator changes.
STATE = ("CPU 36%  RAM 53%  load 1.55 · top CPU: bash 100%, bash 100%, bash 100%, "
         "bash 100% · temps: dell_ddv Video 47C, dell_ddv Ambient 38C, "
         "dell_ddv Unknown 28C, dell_ddv SODIMM 40C, dell_ddv HDD 39C · "
         "gpu: Iris Xe Graphics (Raptor Lake-P) 20%; Arc A370M (DG2) 0% · "
         "disk: / 24%, /boot/efi 3%")
CAUSAL = ("trends: cpu_total 36% (+9% vs 10m ago 27%) sustained 240s; "
          "ram_percent 53% (+2% vs 10m ago 51%) sustained 900s; "
          "load1 1.55 (+0.80 vs 10m ago 0.75)\n"
          "also elevated right now: cpu_iowait 12%, disk_write_bps 4.1MB/s\n"
          "rising together: cpu_total, disk_read_bps, load1\n"
          "resource hogs over window: bash(1512) cpu~98% peak 100% ram 4MB; "
          "bash(1513) cpu~97% peak 100% ram 4MB; cargo(1490) cpu~64% peak 88% "
          "ram 210MB; rustc(1501) cpu~51% peak 77% ram 180MB")

EVENT = AnomalyEvent(ts_ms=1, kind="rule", metric="system.cpu_total",
                     scope="global", severity="high",
                     message="CPU at 36% sustained > 20%",
                     dossier=STATE + "\n" + CAUSAL)
USER = Narrator._prompt(EVENT)


def resident():
    """What Ollama is holding in RAM right now, or nothing."""
    try:
        out = subprocess.run([OLLAMA, "ps"], capture_output=True,
                             text=True, timeout=10).stdout
    except (OSError, subprocess.SubprocessError):
        return "(ollama ps unavailable)"
    rows = [l.strip() for l in out.splitlines()[1:] if l.strip()]
    return " | ".join(rows) if rows else "(no model resident)"


def chat(keep_alive_s, user):
    payload = {
        "model": MODEL, "stream": False, "keep_alive": f"{keep_alive_s:g}s",
        "messages": [{"role": "system", "content": SYSTEM_PROMPT},
                     {"role": "user", "content": user}],
        "options": {"temperature": 0.2, "num_predict": MAX_TOKENS},
    }
    req = urllib.request.Request(
        f"{URL}/api/chat", data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"}, method="POST")
    t0 = time.monotonic()
    with urllib.request.urlopen(req, timeout=600) as resp:
        body = json.loads(resp.read())
    return body, time.monotonic() - t0


def variant(i):
    """Run i's prompt: the same story, nudged so runs cannot share a full prefix.

    Identical prompts let Ollama serve the whole thing from its prefix cache
    (prefill collapses to ~0.04s), which flatters the numbers and hides the real
    cost of a *new* alert.
    """
    ev = AnomalyEvent(ts_ms=1000 * i, kind="rule", metric="system.cpu_total",
                      scope="global", severity="high",
                      message=f"CPU at {36 + i}% sustained > 20%",
                      dossier=STATE.replace("CPU 36%", f"CPU {36 + i}%")
                      + "\n" + CAUSAL)
    return Narrator._prompt(ev)


def loaded():
    """Is a runner resident right now? Decides whether a run is a cold one."""
    return "cold" if resident().startswith("(no model") else "warm"


def unload():
    """Force a cold runner.

    `keep_alive: 0` only asks: Ollama unloads after the response, but a request
    that arrives first is served by the runner that was on its way out (it then
    sits in `Stopping...` and never appears cold). To measure a genuinely cold
    verdict, stop the model explicitly and wait for it to go.
    """
    subprocess.run([OLLAMA, "stop", MODEL], capture_output=True, timeout=30)
    for _ in range(40):
        if loaded() == "cold":
            return True
        time.sleep(0.5)
    return False


def burst(gap_s):
    """`gap 0` is a burst (verdicts seconds apart, model reused); otherwise every
    run is isolated, so it must start from a cold runner."""
    return gap_s <= 0


print(f"model {MODEL} · {RUNS} run(s) · keep_alive {KEEP:g}s · gap {GAP:g}s")
print(f"prompt ~{len(SYSTEM_PROMPT) + len(USER)}B "
      f"({len(SYSTEM_PROMPT)}B system + {len(USER)}B dossier), "
      f"num_predict {MAX_TOKENS}")
print("resident before: " + resident())
print("\nrun  state   load    prefill        decode         wall     verdict")
print("-" * 64)

runs = []
for i in range(1, RUNS + 1):
    if not burst(GAP):
        if i > 1 and GAP:
            time.sleep(GAP)
        if not unload():
            print(f"  {i}  could not unload {MODEL}; is ollama serve running?")
            sys.exit(1)
    state = loaded()
    # The last run always unloads: a benchmark must not leave a model resident.
    keep = KEEP if i < RUNS else 0
    try:
        body, wall = chat(keep, variant(i))
    except Exception as exc:
        print(f"  {i}  failed: {exc}")
        sys.exit(1)
    s = lambda k: body.get(k, 0) / 1e9
    pre, dec = s("prompt_eval_duration"), s("eval_duration")
    pre_tok, dec_tok = body.get("prompt_eval_count", 0), body.get("eval_count", 0)
    # Ollama's own total vs what the caller actually waited: the difference is
    # runner startup and scheduling that none of its counters include.
    other = max(0.0, wall - s("total_duration"))
    runs.append((s("load_duration"), pre, dec, wall))
    print(f"  {i}  {state}  {s('load_duration'):5.2f}s  "
          f"{pre:5.2f}s {pre_tok / max(pre, 1e-9):4.0f} tok/s  "
          f"{dec:5.2f}s {dec_tok / max(dec, 1e-9):4.0f} tok/s  "
          f"{wall:5.2f}s   {dec_tok:3d} tok"
          + (f"   (+{other:.2f}s outside ollama's counters)" if other > 0.5 else ""))

n = len(runs)
mean = [sum(r[i] for r in runs) / n for i in range(4)]
print("-" * 64)
print(f"      mean {mean[0]:5.2f}s  {mean[1]:5.2f}s          "
      f"{mean[2]:5.2f}s          {mean[3]:5.2f}s")
cold = [r for r in runs if r[0] > 0.05]
if cold:
    c = [sum(r[i] for r in cold) / len(cold) for i in range(4)]
    print(f"cold runs only ({len(cold)}): load {c[0]:.2f}s  prefill {c[1]:.2f}s  "
          f"decode {c[2]:.2f}s  wall {c[3]:.2f}s")
print("\nresident after:  " + resident())
if mean[1] > mean[2]:
    print("prefill dominates -> shorten the prompt/dossier first")
elif mean[2] > mean[1]:
    print("decode dominates -> shorter verdicts, or a smaller model, first")
else:
    print("prefill and decode are balanced -> both are worth trimming")
PY