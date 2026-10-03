from tests.conftest import minute, snap

from skyfall_ai.detection import Creep


def leak_feed(n=12, rate_kb=50_000, base=100_000):
    """Per-minute RSS growing by rate_kb/min — classic memory leak shape."""
    c = Creep(window_minutes=12, min_points=4, rss_kb_per_min=15_000.0,
              disk_pct_per_min=0.5)
    events = []
    for m in range(n):
        events += c.process(snap(
            minute(m), procs=[{"pid": 4242, "name": "leaky", "cpu_percent": 2.0,
                               "ram_kb": base + rate_kb * m}]))
    return c, events


def test_ram_leak_detected() -> None:
    _, events = leak_feed()
    creep = [e for e in events if e.kind == "creep" and e.metric == "process.ram_kb"]
    assert creep
    assert creep[0].scope == "proc:4242"
    assert "leak" in creep[0].message.lower()
    assert "49" in creep[0].message  # ~49 MB/min from 50_000 KB/min


def test_steady_rss_no_alarm() -> None:
    c, events = leak_feed(rate_kb=0)
    assert [e for e in events if e.kind == "creep"] == []


def test_disk_fill_slope() -> None:
    c = Creep(window_minutes=12, min_points=4, disk_pct_per_min=0.5)
    events = []
    for m in range(10):
        events += c.process(snap(
            minute(m),
            disks=[{"mount_point": "/dev/root", "percent": 10.0 + 1.5 * m,
                    "used": 0, "total": 0}]))
    creep = [e for e in events if e.kind == "creep" and e.metric == "disk.percent"]
    assert creep
    assert "filling" in creep[0].message.lower()


def test_slow_slope_below_threshold() -> None:
    c = Creep(window_minutes=12, min_points=4, disk_pct_per_min=0.5)
    events = []
    for m in range(10):
        events += c.process(snap(
            minute(m),
            disks=[{"mount_point": "/dev/root", "percent": 40.0 + 0.02 * m,
                    "used": 0, "total": 0}]))
    assert [e for e in events if e.kind == "creep"] == []