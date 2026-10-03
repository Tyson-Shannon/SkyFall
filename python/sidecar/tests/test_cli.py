"""End-to-end CLI test: run the sidecar as `python -m skyfall_ai` and feed it
NDJSON over stdin exactly as the Rust core will.

Each run passes a hermetic temp config so tests never depend on a running Ollama
or on the sample `skyfall_ai.conf.json`; `test_narrator_over_http` spins its own
tiny OpenAI-compatible mock server instead."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SIDECAR = Path(__file__).resolve().parent.parent

CONF = json.loads((SIDECAR / "skyfall_ai.conf.json").read_text())


def write_conf(tmp, overrides: dict | None) -> Path:
    cfg = dict(CONF)
    if overrides:
        cfg.update(overrides)
    path = tmp / "skyfall_ai_test_conf.json"
    path.write_text(json.dumps(cfg))
    return path


def run(input_lines: str, overrides: dict | None = None,
        tmp: Path | None = None) -> subprocess.CompletedProcess:
    env = os.environ.copy()
    env["PYTHONPATH"] = str(SIDECAR)
    cmd = [sys.executable, "-m", "skyfall_ai"]
    if overrides or tmp is not None:
        path = write_conf(tmp or SIDECAR, overrides or {"narrator": {"enabled": False}})
        cmd += ["--config", str(path)]
    return subprocess.run(
        cmd, input=input_lines, capture_output=True, text=True, timeout=90,
        env=env, cwd=SIDECAR,
    )


def snap_line(m: int, cpu: float) -> str:
    return json.dumps({
        "type": "snapshot",
        "ts_ms": m * 60_000,
        "system": {"cpu_total": cpu, "ram_percent": 40.0, "load1": 0.5},
        "gpus": [], "disks": [], "temps": [],
        "processes": [],
    }) + "\n"


def parsed_stdout(p: subprocess.CompletedProcess) -> list[dict]:
    return [json.loads(l) for l in p.stdout.splitlines() if l.strip()]


def test_hello_handshake() -> None:
    p = run('{"type": "shutdown"}\n')
    lines = parsed_stdout(p)
    assert p.returncode == 0
    assert lines[0]["type"] == "hello"
    assert lines[0]["detectors"]["rules"] == "on"
    assert "narrator" in lines[0]["detectors"]


def test_snapshot_triggers_rule_event() -> None:
    feed = "".join([snap_line(m, 99.0) for m in range(8)]
                   + ['{"type": "shutdown"}\n'])
    p = run(feed)
    assert p.returncode == 0, p.stderr
    events = [l for l in parsed_stdout(p) if l["type"] == "event"]
    assert events, "expected at least one rule event"
    ev = events[0]
    assert ev["kind"] == "rule" and ev["metric"] == "system.cpu_total"
    assert ev["message"] and ev["dossier"]


def test_malformed_json_is_tolerated() -> None:
    feed = ("this is not json\n"
            + "".join(snap_line(m, 99.0) for m in range(8))
            + '{"type": "shutdown"}\n')
    p = run(feed)
    assert p.returncode == 0
    events = [l for l in parsed_stdout(p) if l["type"] == "event"]
    assert events


def test_missing_fields_do_not_crash() -> None:
    feed = ('{"type": "snapshot", "ts_ms": 1000}\n'
            '{"type": "shutdown"}\n')
    p = run(feed)
    assert p.returncode == 0
    assert all(l.startswith("{") for l in p.stdout.splitlines() if l.strip())


def test_missing_config_path_falls_back_to_the_packaged_config(tmp_path: Path) -> None:
    """The core always passes `--config` so Settings has somewhere to save the
    narrator settings, which means the file usually does not exist yet. A missing
    file must land on the packaged config, not on the bare defaults — those ship
    no rules, so detection would silently go quiet.
    """
    from skyfall_ai.engine import load_config, packaged_config

    missing = tmp_path / "never-written.json"
    assert not missing.exists()
    cfg = load_config(missing)
    assert cfg["rules"] == CONF["rules"], "tuned rules must survive"
    assert cfg["narrator"]["model"] == CONF["narrator"]["model"]

    # An explicit `~` still resolves (the core expands it, a hand-edited config
    # may not).
    assert load_config(Path("~/definitely-not-here.json"))["rules"] == CONF["rules"]

    # A file that does exist wins outright.
    written = tmp_path / "ai.json"
    written.write_text(json.dumps({"narrator": {"enabled": False, "model": "custom"}}))
    cfg = load_config(written)
    assert cfg["narrator"]["enabled"] is False
    assert cfg["narrator"]["model"] == "custom"
    assert packaged_config().exists(), "packaged config ships with the package"


# ---- narrator over the wire -------------------------------------------------

class _MockLlm(BaseHTTPRequestHandler):
    calls = 0

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        _MockLlm.calls += 1
        verdict = ('{"severity":"high","headline":"CPU is wildly hot",'
                   '"explanation":"The package is throttling.",'
                   '"recommended_action":"Clear the radiators.",'
                   '"confidence":0.9}')
        body = json.dumps({"choices": [{"message": {"content": verdict}}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_: object) -> None:  # silence
        pass


def test_narrator_emits_narrative_over_http() -> None:
    server = ThreadingHTTPServer(("127.0.0.1", 0), _MockLlm)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = server.server_address[1]
        overrides = {
            "narrator": {
                "enabled": True,
                "base_url": f"http://127.0.0.1:{port}",
                "model": "qwen2.5:7b",
                "timeout_s": 5.0,
                "throttle_s": 0.0,
            }
        }
        feed = "".join([snap_line(m, 99.0) for m in range(8)]
                       + ['{"type": "shutdown"}\n'])
        p = run(feed, overrides=overrides)
        assert p.returncode == 0, p.stderr
        lines = parsed_stdout(p)
        narratives = [l for l in lines if l["type"] == "narrative"]
        assert narratives, "expected a narrative for the rule event"
        assert narratives[0]["verdict"]["headline"] == "CPU is wildly hot"
        assert _MockLlm.calls >= 1
    finally:
        server.shutdown()
        server.server_close()

class _SlowLlm(BaseHTTPRequestHandler):
    """Answers slowly, the way a CPU-only 3B model does on a cold load."""

    calls = 0

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        _SlowLlm.calls += 1
        time.sleep(6.0)
        verdict = ('{"severity":"high","headline":"slow but correct",'
                   '"explanation":"deliberate delay",'
                   '"recommended_action":"wait",'
                   '"confidence":0.9}')
        body = json.dumps({"choices": [{"message": {"content": verdict}}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_: object) -> None:  # silence
        pass


def test_slow_llm_never_blocks_snapshot_ingestion(tmp_path: Path) -> None:
    """The regression that froze the whole collector.

    A verdict takes ~90s on a CPU-only machine. Run inline, the sidecar stops
    reading stdin for that whole time: the core keeps writing, its 64KB pipe
    fills, `Sidecar::feed` blocks, and sampling, history writes and the dashboard
    all stop. Here a 6s verdict must not delay *detection* of a later, different
    rule, and shutdown must not wait it out either.
    """
    server = ThreadingHTTPServer(("127.0.0.1", 0), _SlowLlm)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        port = server.server_address[1]
        overrides = {
            # Two rules on different metrics so throttle cannot mask the second.
            "rules": {
                "cpu_spike": {"metric": "system.cpu_total", "gt": 10.0, "need": 1,
                              "of": 2, "cooldown_s": 0.0, "severity": "high"},
                "ram_heavy": {"metric": "system.ram_percent", "gt": 10.0, "need": 1,
                              "of": 2, "cooldown_s": 0.0, "severity": "medium"},
            },
            "narrator": {
                "enabled": True,
                "base_url": f"http://127.0.0.1:{port}",
                "model": "slow",
                "timeout_s": 30.0,
                "throttle_s": 0.0,
            },
        }
        cpu = json.dumps({
            "type": "snapshot", "ts_ms": 1,
            "system": {"cpu_total": 95.0, "ram_percent": 40.0, "load1": 0.5},
            "gpus": [], "disks": [], "temps": [], "processes": [],
        }) + "\n"
        ram = json.dumps({
            "type": "snapshot", "ts_ms": 2,
            "system": {"cpu_total": 95.0, "ram_percent": 88.0, "load1": 0.5},
            "gpus": [], "disks": [], "temps": [], "processes": [],
        }) + "\n"
        feed = cpu + cpu + ram + ram + '{"type": "shutdown"}\n'

        t0 = time.monotonic()
        p = run(feed, overrides=overrides, tmp=tmp_path)
        elapsed = time.monotonic() - t0

        assert p.returncode == 0, p.stderr
        lines = parsed_stdout(p)
        kinds = [l["type"] for l in lines]
        # The RAM alert is detected after the slow CPU verdict is already in
        # flight, so its presence proves ingestion was never blocked.
        assert kinds.count("event") >= 2, f"expected 2 events, got {kinds}"
        ram_alert = [l for l in lines
                     if l["type"] == "event" and l.get("metric") == "system.ram_percent"]
        assert ram_alert, "RAM rule never fired: ingestion was blocked by narration"
        # Shutdown drains briefly rather than waiting out the 6s call.
        assert elapsed < 6.0 + 3.0, f"shutdown waited for the verdict ({elapsed:.1f}s)"
    finally:
        server.shutdown()
        server.server_close()
