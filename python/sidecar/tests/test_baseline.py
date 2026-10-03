from tests.conftest import minute, snap

from skyfall_ai.detection import BaselineZ


def run_baseline(seq):
    """Feed `seq` of (cpu) values at one-minute cadence through a warm baseline."""
    z = BaselineZ(window_minutes=8, warmup_minutes=2)
    events = []
    for m, cpu in enumerate(seq):
        events += z.process(snap(minute(m), cpu=cpu))
    return events


def test_warmup_produces_no_events() -> None:
    assert run_baseline([10.0, 10.0]) == []


def test_z_spike_fires_immediately() -> None:
    events = run_baseline([10.0, 10.0, 60.0, 60.0, 60.0, 60.0])
    fired = [e for e in events if e.kind == "baseline"]
    assert len(fired) == 1
    assert fired[0].severity == "critical"  # |z| far above 8 escalates
    assert "z=" in fired[0].message


def test_drift_above_z3() -> None:
    events = run_baseline([10.0, 10.0, 20.0, 30.0, 40.0, 50.0, 60.0])
    # Debouncer cooldown (600s) means at most a couple of events in this window.
    fired = [e for e in events if e.kind == "baseline"]
    assert 1 <= len(fired) <= 2


def test_same_value_stays_quiet() -> None:
    assert run_baseline([42.0] * 12) == []