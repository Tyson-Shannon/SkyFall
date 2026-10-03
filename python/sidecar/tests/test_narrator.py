import io
import json
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

from skyfall_ai.events import AnomalyEvent
from skyfall_ai.narrator import Narrator, parse_verdict, with_keep_alive


def mk_event(ts_ms=1000, kind="rule", scope="global", metric="system.cpu_total",
             severity="medium", message="CPU at 95%", dossier="CPU 95% RAM 40%"):
    return AnomalyEvent(ts_ms=ts_ms, kind=kind, metric=metric, scope=scope,
                        severity=severity, message=message, dossier=dossier)


def fake_transport(content):
    calls = []

    def transport(base_url, model, api_key, prompt, timeout_s):
        calls.append((base_url, model, api_key, prompt, timeout_s))
        return content

    transport.calls = calls
    return transport


def verdict_json(sev="high", headline="CPU is wildly hot"):
    verdict = {
        "severity": sev,
        "headline": headline,
        "root_cause": "rustc compiling many crates (CPU and disk writes rising together)",
        "explanation": "The package is throttling.",
        "recommended_action": "Clear the radiators.",
        "confidence": 0.9,
    }
    return "```json\n" + json.dumps(verdict) + "\n```"


def test_parse_verdict_strips_fences_and_coerces() -> None:
    v = parse_verdict(verdict_json("HIGH"), fallback_severity="medium")
    assert v["severity"] == "high"  # lowercased + validated
    assert v["headline"] == "CPU is wildly hot"
    assert v["root_cause"].startswith("rustc compiling")
    assert 0.0 < v["confidence"] <= 1.0


def test_parse_verdict_tolerates_missing_root_cause() -> None:
    # Older / smaller models may omit the field; it must degrade to empty.
    v = parse_verdict('{"severity":"high","headline":"h","confidence":0.5}', "high")
    assert v["root_cause"] == ""


def test_parse_verdict_truncates_overlong_root_cause() -> None:
    v = parse_verdict(json.dumps({"root_cause": "x" * 5000}), "low")
    assert len(v["root_cause"]) <= 400


def test_parse_verdict_handles_garbage() -> None:
    v = parse_verdict("sorry, no json here", fallback_severity="medium")
    assert v["severity"] == "medium"
    assert v["confidence"] == 0.0
    assert "unreadable" in v["headline"]


def test_parse_verdict_clamps_confidence_and_severity() -> None:
    v = parse_verdict('{"confidence": 5.0, "severity": "oops"}', "critical")
    assert v["confidence"] == 1.0
    assert v["severity"] == "critical"


def test_narrator_prompt_surfaces_causal_block() -> None:
    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": True, "throttle_s": 0}, transport=tr)
    ev = mk_event(dossier="CPU 95% RAM 40%\ntrends: CPU 95 (+40% vs 10m ago 55)\n"
                            "resource hogs over window: rustc(4242) cpu~80% write 20MB/s")
    n.narrate([ev], now_sec=100.0)
    prompt = tr.calls[0][3]
    assert "Causal context" in prompt
    assert "rustc(4242)" in prompt
    assert "Current state: CPU 95% RAM 40%" in prompt


def test_narrator_calls_transport_and_throttles_per_scope() -> None:
    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": True, "throttle_s": 1000}, transport=tr)
    out = n.narrate([mk_event(ts_ms=1_000)], now_sec=100.0)
    assert len(out) == 1 and out[0]["type"] == "narrative"
    assert out[0]["verdict"]["headline"] == "CPU is wildly hot"
    assert out[0]["alert_ts_ms"] == 1_000
    assert len(tr.calls) == 1

    # same (kind, scope) within throttle window -> no new call, no message
    out2 = n.narrate([mk_event(ts_ms=2_000)], now_sec=100.5)
    assert out2 == [] and len(tr.calls) == 1


def test_narrator_different_scope_passes_throttle() -> None:
    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": True, "throttle_s": 1000}, transport=tr)
    n.narrate([mk_event(ts_ms=1_000, scope="global")], now_sec=100.0)
    out = n.narrate([mk_event(ts_ms=2_000, scope="proc:9")], now_sec=100.5)
    assert len(out) == 1 and len(tr.calls) == 2


def test_narrator_disables_after_consecutive_failures() -> None:
    def boom(*_a, **_k):
        raise ConnectionRefusedError("no ollama here")

    n = Narrator({"enabled": True, "throttle_s": 0}, transport=boom)
    for _ in range(3):
        n.narrate([mk_event(ts_ms=1000)], now_sec=100.0)
    assert n.status().startswith("off")
    n.narrate([mk_event(ts_ms=2000)], now_sec=100.0)
    assert n.narrate([]) == []  # and still returns nothing safely


def test_narrator_disabled_no_calls() -> None:
    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": False, "throttle_s": 0}, transport=tr)
    assert n.narrate([mk_event()]) == []
    assert tr.calls == []
    assert n.status().startswith("off")


def test_narrator_status_shows_model_and_host() -> None:
    n = Narrator({"enabled": True, "base_url": "http://localhost:11434",
                  "model": "qwen2.5:7b"}, transport=fake_transport("x"))
    assert "qwen2.5:7b" in n.status() and "localhost" in n.status()


def test_default_model_is_the_3b_and_timeouts_are_cpu_safe() -> None:
    from skyfall_ai.narrator import (DEFAULT_MODEL, DEFAULT_TIMEOUT_S, MAX_TOKENS)

    assert DEFAULT_MODEL == "qwen2.5:3b"
    # The verdict must comfortably fit, and CPU-only 3B needs the headroom.
    assert MAX_TOKENS >= 200
    assert DEFAULT_TIMEOUT_S * (10.0) >= MAX_TOKENS, \
        "default timeout must allow >=10 tok/s, else every call times out"
    n = Narrator({"enabled": True}, transport=fake_transport("x"))
    assert n.model == "qwen2.5:3b"
    assert n.timeout_s == DEFAULT_TIMEOUT_S


def test_missing_model_pauses_instead_of_tripping_the_breaker() -> None:
    """A model that was never pulled is a setup gap, not a dead endpoint."""
    from skyfall_ai.narrator import ModelNotInstalled

    calls = []

    def missing(base_url, model, api_key, prompt, timeout_s):
        calls.append(model)
        raise ModelNotInstalled(model)

    n = Narrator({"enabled": True, "throttle_s": 0.0}, transport=missing)
    for _ in range(5):
        assert n.narrate([mk_event()]) == []
    # Called once, then paused: no per-tick hammering and no self-disable.
    assert len(calls) == 1
    assert n._available is True, "must NOT disable the narrator for the session"
    assert "not installed" in n.status()


def test_missing_model_recovers_once_it_is_installed() -> None:
    from skyfall_ai.narrator import MODEL_RECHECK_S, ModelNotInstalled

    state = {"installed": False}

    def flaky(base_url, model, api_key, prompt, timeout_s):
        if not state["installed"]:
            raise ModelNotInstalled(model)
        return verdict_json()

    n = Narrator({"enabled": True, "throttle_s": 0.0}, transport=flaky)
    assert n.narrate([mk_event()], now_sec=100.0) == []
    # While paused we must not keep calling...
    assert n.narrate([mk_event()], now_sec=100.0 + MODEL_RECHECK_S - 1) == []
    # ...but after the recheck window we must re-probe and recover.
    state["installed"] = True
    out = n.narrate([mk_event()], now_sec=100.0 + MODEL_RECHECK_S + 1)
    assert len(out) == 1
    assert out[0]["verdict"]["headline"] == "CPU is wildly hot"
    assert "on" in n.status()


def test_real_transport_maps_404_to_model_not_installed() -> None:
    """The HTTP path must distinguish 'model absent' from other 4xx/5xx."""
    import io
    import urllib.error

    from skyfall_ai.narrator import ModelNotInstalled, post_openai

    def raise_404(*_a, **_k):
        raise urllib.error.HTTPError(
            "http://x/v1/chat/completions", 404, "Not Found", {}, io.BytesIO(b""))

    orig = urllib.request.urlopen
    urllib.request.urlopen = raise_404
    try:
        try:
            post_openai("http://x", "qwen2.5:3b", None, "p", 1.0)
        except ModelNotInstalled as exc:
            assert "qwen2.5:3b" in str(exc)
        else:
            raise AssertionError("404 should raise ModelNotInstalled")
    finally:
        urllib.request.urlopen = orig


class _Resp:
    """Minimal urlopen result: bytes in, `.status` out."""

    def __init__(self, payload: bytes, status: int = 200):
        self._buf = io.BytesIO(payload)
        self.status = status

    def read(self):
        return self._buf.read()

    def __enter__(self):
        return self

    def __exit__(self, *_a):
        self._buf.close()
        return False


def _stub_endpoint(ollama: bool):
    """Fake urlopen for one base URL; returns (patch, calls) with the chat body."""
    calls = []

    def fake_urlopen(req, timeout=None):
        url = req.full_url if hasattr(req, "full_url") else str(req)
        calls.append(url)
        if url.endswith("/api/tags"):
            if not ollama:
                raise urllib.error.HTTPError(url, 404, "Not Found", {}, io.BytesIO(b""))
            return _Resp(json.dumps({"models": []}).encode())
        body = json.loads(req.data)
        calls[-1] = (url, body)
        if ollama:
            return _Resp(json.dumps({"message": {"content": verdict_json()}}).encode())
        return _Resp(json.dumps(
            {"choices": [{"message": {"content": verdict_json()}}]}).encode())

    return fake_urlopen, calls


def _chat_body(base_url: str, keep_alive_s, ollama: bool):
    fake_urlopen, calls = _stub_endpoint(ollama)
    orig = urllib.request.urlopen
    urllib.request.urlopen = fake_urlopen
    try:
        with_keep_alive(keep_alive_s)(base_url, "qwen2.5:3b", None, "p", 1.0)
    finally:
        urllib.request.urlopen = orig
    chat = [c for c in calls if isinstance(c, tuple)]
    assert len(chat) == 1, f"expected one chat request, saw {calls}"
    return chat[0]


def test_narration_releases_the_model_instead_of_pinning_it() -> None:
    """The regression behind "why is a 2 GB llama-server running?".

    Ollama keeps a model resident for 5 minutes after the last request and renews
    that on every call, so narrating alerts left a `llama-server` runner holding
    the whole model in RAM almost permanently. We ask for it to be released as
    soon as the verdict is in — which only works on Ollama's native route, since
    `/v1/chat/completions` silently drops `keep_alive` (verified against a live
    ollama: the runner survived with "4 minutes from now").
    """
    from skyfall_ai.narrator import DEFAULT_KEEP_ALIVE_S

    assert DEFAULT_KEEP_ALIVE_S == 0.0, "default must not pin the model"

    url, body = _chat_body("http://ollama-test-1:11434", DEFAULT_KEEP_ALIVE_S, ollama=True)
    assert url.endswith("/api/chat"), f"keep_alive needs the native route, got {url}"
    assert body["keep_alive"] == "0s", "unload as soon as the verdict is in"
    assert body["options"]["num_predict"] >= 200
    assert body["stream"] is False

    # A user who prefers latency over RAM can ask for a warm model.
    _, warm = _chat_body("http://ollama-test-2:11434", 300, ollama=True)
    assert warm["keep_alive"] == "300s"
    _, negative = _chat_body("http://ollama-test-3:11434", -5, ollama=True)
    assert negative["keep_alive"] == "0s", "never negative"

    # Anything that is not Ollama keeps the OpenAI-compatible route it always had.
    url, body = _chat_body("http://cloud-test-1/v1", DEFAULT_KEEP_ALIVE_S, ollama=False)
    assert url.endswith("/v1/chat/completions"), url
    assert "keep_alive" not in body, "no native field on an OpenAI route"


def test_endpoint_probe_happens_once_per_endpoint() -> None:
    from skyfall_ai.narrator import speaks_ollama

    speaks_ollama.cache_clear()
    fake_urlopen, calls = _stub_endpoint(ollama=True)
    orig = urllib.request.urlopen
    urllib.request.urlopen = fake_urlopen
    try:
        transport = with_keep_alive(0.0)
        for _ in range(3):
            transport("http://ollama-test-4:11434", "qwen2.5:3b", None, "p", 1.0)
        assert speaks_ollama("http://ollama-test-4:11434") is True
    finally:
        urllib.request.urlopen = orig
    probes = [c for c in calls if isinstance(c, str) and c.endswith("/api/tags")]
    assert len(probes) == 1, f"probe must be cached, saw {probes}"


def test_a_probe_failure_is_not_a_missing_model() -> None:
    """A 404 from the probe means "not Ollama", never "install the model".

    Misreading it would pause narration for minutes on every cloud provider.
    """
    from skyfall_ai.narrator import speaks_ollama

    speaks_ollama.cache_clear()

    def refuse(*_a, **_k):
        raise urllib.error.URLError("connection refused")

    orig = urllib.request.urlopen
    urllib.request.urlopen = refuse
    try:
        assert speaks_ollama("http://ollama-test-5:11434") is False
    finally:
        urllib.request.urlopen = orig
    # Cached, so the endpoint is not re-probed on every verdict.
    assert speaks_ollama("http://ollama-test-5:11434") is False


def test_keep_alive_defaults_and_is_overridable() -> None:
    from skyfall_ai.narrator import DEFAULT_KEEP_ALIVE_S

    n = Narrator({"enabled": True, "throttle_s": 0.0},
                 transport=fake_transport(verdict_json()))
    assert n.keep_alive_s == DEFAULT_KEEP_ALIVE_S
    assert Narrator({"enabled": True, "keep_alive_s": 120},
                    transport=fake_transport("x")).keep_alive_s == 120.0


def test_partial_narrator_block_keeps_the_other_knobs() -> None:
    """The settings dialog writes only enabled/base_url/model/api_key.

    A top-level config merge would drop timeout_s/throttle_s/keep_alive_s with
    them, silently changing behaviour on the first save.
    """
    from skyfall_ai.engine import default_config, load_config

    packaged = json.loads((Path(__file__).resolve().parent.parent
                           / "skyfall_ai.conf.json").read_text())
    assert "keep_alive_s" in packaged["narrator"], "packaged config documents the knob"

    partial = {"narrator": {"enabled": False, "model": "qwen2.5:1.5b"}}
    narrator = _merge(load_config, partial)["narrator"]
    builtins = default_config()["narrator"]
    assert narrator["enabled"] is False, "the user's choice wins"
    assert narrator["model"] == "qwen2.5:1.5b"
    for key in ("timeout_s", "throttle_s", "keep_alive_s"):
        assert narrator[key] == builtins[key], f"{key} must survive a partial block"


def _merge(load_config, user):
    """`load_config` applied to an in-memory `user` dict, as if it were a file."""
    with tempfile.TemporaryDirectory() as d:
        p = Path(d) / "skyfall_ai.json"
        p.write_text(json.dumps(user))
        return load_config(p)


# ---------------------------------------------------------------- burst cost --
# The throttle is keyed on `(kind, scope)`, and a scope is a *sensor*
# (`temp:gpu1`, `disk:/`, `gpu:0`). One bad moment trips many of them at once, so
# without these guards a single snapshot could buy a dozen verdicts telling the
# same story, each paying its own model load.


def test_burst_is_capped_and_worst_first() -> None:
    from skyfall_ai.narrator import DEFAULT_MAX_PER_BURST

    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": True, "throttle_s": 0.0}, transport=tr)
    assert n.max_per_burst == DEFAULT_MAX_PER_BURST
    out = n.narrate([
        mk_event(ts_ms=1, metric="m.low", severity="low"),
        mk_event(ts_ms=2, metric="m.crit", severity="critical"),
        mk_event(ts_ms=3, metric="m.high", severity="high"),
        mk_event(ts_ms=4, metric="m.med", severity="medium"),
        mk_event(ts_ms=5, metric="m.high2", severity="high"),
    ], now_sec=100.0)
    assert len(out) == DEFAULT_MAX_PER_BURST
    asked = [c[3].split("metric '")[1].split("'")[0] for c in tr.calls]
    assert asked == ["m.crit", "m.high", "m.high2"], asked


def test_overflowed_alert_stays_eligible_after_the_throttle() -> None:
    """Capping must not silently retire an alert: dropped is not throttled."""
    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": True, "throttle_s": 60.0, "max_per_burst": 1},
                 transport=tr)
    lo = mk_event(ts_ms=1, metric="m.low", scope="temp:gpu1", severity="low")
    hi = mk_event(ts_ms=2, metric="m.high", scope="disk:/", severity="high")
    n.narrate([lo, hi], now_sec=100.0)
    assert len(tr.calls) == 1, "only the worst alert is narrated"
    n.narrate([lo], now_sec=101.0)
    assert len(tr.calls) == 2, "the dropped alert must still be narratable"


def test_max_per_burst_zero_means_unlimited() -> None:
    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": True, "throttle_s": 0.0, "max_per_burst": 0},
                 transport=tr)
    events = [mk_event(ts_ms=i, metric=f"m{i}") for i in range(6)]
    assert len(n.narrate(events, now_sec=100.0)) == 6


def test_sibling_sensors_with_identical_evidence_narrate_once() -> None:
    """Five hot temperature probes are one story, not five verdicts."""
    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": True, "throttle_s": 300.0}, transport=tr)
    sensors = [mk_event(ts_ms=1_000, metric="temp.celsius", severity="high",
                        message="GPU at 84C", scope=f"temp:{name}")
               for name in ("gpu1", "gpu2", "gpu3")]
    out = n.narrate(sensors, now_sec=100.0)
    assert len(out) == 1 and len(tr.calls) == 1
    prompt = tr.calls[0][3]
    assert "also affected" in prompt, "the verdict must own the whole group"
    assert "temp:gpu2" in prompt and "temp:gpu3" in prompt


def test_scope_class_decides_whether_one_verdict_covers_both() -> None:
    """Only the sensor *class* is dropped, so different systems stay separate."""
    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": True, "throttle_s": 300.0}, transport=tr)
    out = n.narrate([
        mk_event(ts_ms=1, metric="temp.celsius", scope="temp:gpu1"),
        mk_event(ts_ms=2, metric="temp.celsius", scope="disk:/"),
    ], now_sec=100.0)
    assert len(out) == 2 and len(tr.calls) == 2


def test_the_worst_sensor_is_the_one_that_gets_narrated() -> None:
    """Collapsing 32 hot cores must still speak for the hottest one."""
    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": True, "throttle_s": 300.0}, transport=tr)
    out = n.narrate([
        mk_event(ts_ms=1, metric="temp.celsius", scope="temp:coretemp Core 0",
                 severity="medium"),
        mk_event(ts_ms=2, metric="temp.celsius", scope="temp:coretemp Core 7",
                 severity="critical"),
        mk_event(ts_ms=3, metric="temp.celsius", scope="temp:coretemp Core 9",
                 severity="high"),
    ], now_sec=100.0)
    assert len(out) == 1
    assert out[0]["alert_scope"] == "temp:coretemp Core 7", out[0]["alert_scope"]
    prompt = tr.calls[0][3]
    assert "Core 0" in prompt and "Core 9" in prompt, prompt


def test_fingerprint_memory_is_bounded() -> None:
    """A month-long session must not accumulate fingerprints forever."""
    from skyfall_ai.narrator import FINGERPRINT_MEMORY

    n = Narrator({"enabled": True, "throttle_s": 0.0},
                 transport=fake_transport(verdict_json()))
    for i in range(FINGERPRINT_MEMORY + 20):
        n.narrate([mk_event(ts_ms=i, metric=f"m{i}")], now_sec=100.0 + i)
    assert len(n._fingerprints) <= FINGERPRINT_MEMORY


class _Recorder:
    """Stand-in for a bound transport: records the prompt, returns a verdict."""

    def __init__(self, label, log):
        self.label, self._log = label, log

    def __call__(self, base_url, model, api_key, prompt, timeout_s):
        self._log.append((self.label, prompt))
        return verdict_json()


def _narrator_with_recorders(cfg):
    """A Narrator whose transports are recorders, labelled by keep-alive.

    Patching the factory shows exactly which keep-alive each bound transport was
    built with, and keeps the test off the network.
    """
    from skyfall_ai import narrator as narrator_mod

    log: list = []
    built: list = []
    orig = narrator_mod.with_keep_alive

    def factory(keep_alive_s):
        built.append(keep_alive_s)
        return _Recorder(f"keep_alive={keep_alive_s}", log)

    narrator_mod.with_keep_alive = factory
    try:
        n = Narrator(cfg)
    finally:
        narrator_mod.with_keep_alive = orig
    return n, log, built


def _three_stories():
    return [
        mk_event(ts_ms=1, metric="system.cpu_total", scope="global"),
        mk_event(ts_ms=2, metric="system.ram_percent", scope="global"),
        mk_event(ts_ms=3, metric="disk.percent", scope="disk:/"),
    ]


def test_burst_stays_warm_until_its_last_verdict() -> None:
    """One model load per burst, and nothing resident once it ends."""
    from skyfall_ai.narrator import (DEFAULT_BURST_KEEP_ALIVE_S,
                                     DEFAULT_KEEP_ALIVE_S)

    n, log, built = _narrator_with_recorders(
        {"enabled": True, "throttle_s": 0.0, "max_per_burst": 3})
    assert built == [DEFAULT_KEEP_ALIVE_S, DEFAULT_BURST_KEEP_ALIVE_S], built
    n.narrate(_three_stories(), now_sec=100.0)
    assert [label for label, _ in log] == [
        f"keep_alive={DEFAULT_BURST_KEEP_ALIVE_S}",
        f"keep_alive={DEFAULT_BURST_KEEP_ALIVE_S}",
        f"keep_alive={DEFAULT_KEEP_ALIVE_S}",
    ], "warm between verdicts, unload on the last"


def test_isolated_alert_unloads_immediately() -> None:
    """The common case: one alert, nothing kept resident afterwards."""
    n, log, _built = _narrator_with_recorders({"enabled": True, "throttle_s": 0.0})
    n.narrate([mk_event(ts_ms=1)], now_sec=100.0)
    assert [label for label, _ in log] == ["keep_alive=0.0"]


def test_configured_keep_alive_overrides_burst_warmth() -> None:
    """A user who asked for a resident model has already paid for it."""
    n, log, built = _narrator_with_recorders(
        {"enabled": True, "throttle_s": 0.0, "keep_alive_s": 300.0})
    assert built == [300.0], "no burst transport on top of an explicit keep-alive"
    n.narrate(_three_stories(), now_sec=100.0)
    assert {label for label, _ in log} == {"keep_alive=300.0"}


def test_injected_transport_ignores_the_burst_policy() -> None:
    tr = fake_transport(verdict_json())
    n = Narrator({"enabled": True, "throttle_s": 0.0}, transport=tr)
    assert n._burst_transport is None, "a caller-supplied transport stands alone"
    n.narrate(_three_stories(), now_sec=100.0)
    assert len(tr.calls) == 3


