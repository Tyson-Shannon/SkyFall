from tests.conftest import minute, snap

from skyfall_ai.baseline import Baseline, Debouncer, Sustained


def test_sustained_needs_need_of_over() -> None:
    s = Sustained(need=3, over=5)
    fired_at = [s.update(True) for _ in range(5)]
    assert fired_at == [False, False, True, True, True]


def test_sustained_resets_on_miss() -> None:
    s = Sustained(need=3, over=5)
    assert s.update(True) is False
    assert s.update(True) is False
    assert s.update(True) is True
    assert s.update(False) is False
    assert s.update(True) is False  # streak was reset by the miss


def test_debouncer_recovery_and_cooldown() -> None:
    d = Debouncer(cooldown_s=100.0)
    t0 = 1_700_000_000_000
    assert d.should_fire("x", t0, True) is True        # fresh anomaly fires
    assert d.should_fire("x", t0 + 50_000, True) is False   # within cooldown
    assert d.should_fire("x", t0 + 150_000, True) is True   # re-fire after cooldown
    assert d.should_fire("x", t0 + 200_000, False) is False  # recovery resets
    assert d.should_fire("x", t0 + 250_000, True) is True    # next anomaly immediate


def test_baseline_warmup_gate() -> None:
    b = Baseline(window_minutes=10, warmup_minutes=3)
    assert b.ready("cpu") is False
    for m in range(5):
        b.observe(minute(m), "cpu", 1.0)
    assert b.ready("cpu") is True
    assert b.count("cpu") == 4  # 4 flushed minute-buckets; 5th still accumulating


def test_baseline_z_on_step_change() -> None:
    b = Baseline(window_minutes=10, warmup_minutes=2)
    for m in range(2):
        b.observe(minute(m), "cpu", 1.0)
    z = b.observe(minute(2), "cpu", 10.0)
    assert z is not None and z > 5.0


def test_baseline_window_trims_old_minutes() -> None:
    b = Baseline(window_minutes=3, warmup_minutes=1)
    for m in range(6):
        b.observe(minute(m), "cpu", 1.0)
    assert b.count("cpu") == 3  # deque(maxlen=3) only latest minutes retained