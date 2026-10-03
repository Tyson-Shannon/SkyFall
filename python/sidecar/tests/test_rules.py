from tests.conftest import minute, snap

from skyfall_ai.detection import Rules

RULES = {
    "cpu": {"metric": "system.cpu_total", "gt": 90.0, "need": 4, "of": 5,
            "hysteresis": 10.0, "severity": "medium", "cooldown_s": 300.0},
}


def test_rule_sustained_fires() -> None:
    r = Rules(RULES)
    fired: list = []
    for m in range(7):
        fired += r.process(snap(minute(m), cpu=99.0))
    assert len(fired) == 1  # fires once (cooldown suppresses repeats at 60s)
    assert fired[0].kind == "rule"
    assert fired[0].metric == "system.cpu_total"
    assert "99" in fired[0].message


def test_rule_does_not_fire_below_threshold() -> None:
    r = Rules(RULES)
    events = []
    for m in range(10):
        events += r.process(snap(minute(m), cpu=50.0))
    assert events == []


def test_rule_single_spike_ignored() -> None:
    r = Rules(RULES)
    events = []
    for m in range(6):
        cpu = 99.0 if m == 1 else 20.0
        events += r.process(snap(minute(m), cpu=cpu))
    assert events == []  # need 4-of-5, only one hot sample


def test_rule_hysteresis_requires_real_recovery() -> None:
    r = Rules(RULES)
    # fire, dip to 89 (still > gt-hysteresis=80, no recovery), fire again later
    events = []
    for m in range(10):
        cpu = 99.0 if m < 6 else 89.0
        events += r.process(snap(minute(m), cpu=cpu))
    first = [e for e in events if e.ts_ms == minute(3)]
    assert first, "expected first event at the 4th sustained sample"


def test_rule_scoped_per_gpu() -> None:
    r = Rules()
    events = []
    for m in range(6):
        events += r.process(snap(
            minute(m), cpu=20.0,
            gpus=[{"id": "card2", "model": "Arc A370M", "utilization": 99.0}]))
    u = [e for e in events if e.metric == "gpu.utilization"]
    assert u and u[0].scope == "gpu:card2"