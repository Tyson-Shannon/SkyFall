"""Narration must never run on the sidecar's stdin loop.

A CPU-only 3B verdict takes ~90s, and `narrate()` makes one call per event. Run
inline that blocks reading stdin, the core's 64KB pipe fills, and the whole
collector freezes. These tests pin the decoupling: a slow narrator may delay its
own verdict, but it must not delay event emission or snapshot ingestion.
"""

import json
import threading
import time

from skyfall_ai.events import AnomalyEvent
from skyfall_ai.narrator import NarrationWorker, Narrator
from skyfall_ai.protocol import Emitter


def mk_event(ts_ms=1000, scope="global"):
    return AnomalyEvent(ts_ms=ts_ms, kind="rule", metric="system.cpu_total",
                        scope=scope, severity="high",
                        message="CPU at 95%", dossier="CPU 95% RAM 40%")


def verdict_json() -> str:
    verdict = {
        "severity": "high",
        "headline": "CPU is wildly hot",
        "root_cause": "rustc compiling many crates",
        "explanation": "The package is throttling.",
        "recommended_action": "Clear the radiators.",
        "confidence": 0.9,
    }
    return "```json\n" + json.dumps(verdict) + "\n```"


class _Collector:
    """Thread-safe Emitter stand-in that records every NDJSON object."""

    def __init__(self) -> None:
        self.lines: list[dict] = []
        self._lock = threading.Lock()

    def __call__(self, obj: dict) -> None:
        with self._lock:
            self.lines.append(obj)

    def of_type(self, kind: str) -> list[dict]:
        with self._lock:
            return [x for x in self.lines if x.get("type") == kind]


def slow_transport(delay_s: float):
    def transport(base_url, model, api_key, prompt, timeout_s):
        time.sleep(delay_s)
        return verdict_json()
    return transport


def test_submit_returns_immediately_while_llm_is_slow() -> None:
    """The regression: submit() must not block for the length of a verdict."""
    narrator = Narrator({"enabled": True, "model": "slow", "throttle_s": 0.0})
    narrator._transport = slow_transport(2.0)
    got = _Collector()
    worker = NarrationWorker(narrator, got)
    worker.start()
    try:
        t0 = time.monotonic()
        worker.submit([mk_event()])
        submit_cost = time.monotonic() - t0
        # The verdict takes 2s; submitting must be effectively free.
        assert submit_cost < 0.25, f"submit blocked for {submit_cost:.2f}s"
        # ...and the verdict still arrives, just later.
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline and not got.of_type("narrative"):
            time.sleep(0.05)
        assert got.of_type("narrative"), "verdict never arrived"
    finally:
        worker.stop(drain_timeout=0.1)


def test_worker_keeps_consuming_while_a_verdict_is_in_flight() -> None:
    """A burst must not stall behind one slow call."""
    narrator = Narrator({"enabled": True, "model": "slow", "throttle_s": 0.0})
    narrator._transport = slow_transport(0.4)
    got = _Collector()
    worker = NarrationWorker(narrator, got)
    worker.start()
    try:
        for i in range(5):
            worker.submit([mk_event(ts_ms=1000 + i, scope=f"scope{i}")])
        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline and len(got.of_type("narrative")) < 5:
            time.sleep(0.05)
        assert len(got.of_type("narrative")) == 5
    finally:
        worker.stop(drain_timeout=0.1)


def test_queue_overflow_drops_oldest_and_keeps_newest() -> None:
    """A backlog must never block detection; newest evidence is the valuable one."""
    narrator = Narrator({"enabled": True, "model": "slow", "throttle_s": 0.0})
    narrator._transport = slow_transport(0.05)
    worker = NarrationWorker(narrator, _Collector(), maxsize=2)
    # Not started: nothing drains, so the queue is guaranteed to fill.
    for i in range(10):
        worker.submit([mk_event(ts_ms=1000 + i, scope=f"s{i}")])
    assert worker._q.qsize() == 2
    scopes = [b[0].scope for b in list(worker._q.queue)]
    assert scopes == ["s8", "s9"], f"kept {scopes}, expected the two newest"


def test_stop_is_prompt_even_mid_verdict() -> None:
    """Quitting must not wait out a 90s CPU-only verdict."""
    narrator = Narrator({"enabled": True, "model": "slow", "throttle_s": 0.0})
    narrator._transport = slow_transport(3.0)
    worker = NarrationWorker(narrator, _Collector())
    worker.start()
    worker.submit([mk_event()])
    time.sleep(0.2)  # ensure the worker is inside the call
    t0 = time.monotonic()
    worker.stop(drain_timeout=0.5)
    assert time.monotonic() - t0 < 1.0


def test_emitter_keeps_each_object_on_one_line_under_concurrency() -> None:
    """Two writers share stdout; the Rust NDJSON reader must never see a torn line."""
    import io

    buf = io.StringIO()
    emitter = Emitter(buf)
    payloads = [{"type": "narrative", "verdict": {"root_cause": "x" * 200, "i": i}}
                for i in range(60)]

    def hammer(chunk):
        for obj in chunk:
            emitter(obj)

    threads = [threading.Thread(target=hammer, args=(payloads[a::2],))
               for a in range(2)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    lines = [ln for ln in buf.getvalue().splitlines() if ln.strip()]
    assert len(lines) == 60, f"{len(lines)} lines, expected 60"
    for ln in lines:
        assert json.loads(ln)["type"] == "narrative"
