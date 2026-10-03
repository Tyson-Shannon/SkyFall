"""Tests for the read-only history reader (phase 6 causal context)."""

import sqlite3

import pytest

from skyfall_ai.history import HistoryReader

SCHEMA = """
CREATE TABLE system_metrics (
    ts INTEGER PRIMARY KEY, cpu_total REAL, ram_pct REAL, ram_used INTEGER,
    swap_used INTEGER, load1 REAL, load5 REAL, load15 REAL,
    net_rx_bps INTEGER, net_tx_bps INTEGER
);
CREATE TABLE process_metrics (
    ts INTEGER, pid INTEGER, name TEXT, cpu REAL, ram_kb INTEGER,
    disk_r_bps INTEGER, disk_w_bps INTEGER, PRIMARY KEY (ts, pid)
) WITHOUT ROWID;
"""

NOW = 1_700_000_000_000
WINDOW_MS = 10 * 60_000


def make_db(path, points, procs=()):
    conn = sqlite3.connect(path)
    conn.executescript(SCHEMA)
    for ts, cpu, ram, load1, tx in points:
        conn.execute(
            "INSERT INTO system_metrics (ts, cpu_total, ram_pct, ram_used, swap_used,"
            " load1, load5, load15, net_rx_bps, net_tx_bps)"
            " VALUES (?,?,?,0,0,?,0,0,0,?)",
            (ts, cpu, ram, load1, tx),
        )
    for ts, pid, name, cpu, ram_kb, w in procs:
        conn.execute(
            "INSERT INTO process_metrics (ts, pid, name, cpu, ram_kb, disk_r_bps, disk_w_bps)"
            " VALUES (?,?,?,?,?,0,?)",
            (ts, pid, name, cpu, ram_kb, w),
        )
    conn.commit()
    conn.close()


def test_no_db_path_is_not_available() -> None:
    r = HistoryReader(None)
    assert not r.available()
    h = r.read(now_ms=NOW)
    assert not h.available
    assert h.render() == ""


def test_missing_db_file_is_not_available(tmp_path) -> None:
    r = HistoryReader(str(tmp_path / "nope.db"))
    assert not r.available()
    assert "not found" in r.read(now_ms=NOW).reason


def test_memory_db_reports_unavailable() -> None:
    r = HistoryReader(":memory:")
    assert not r.available()


def test_reports_rising_cpu_trend(tmp_path) -> None:
    db = tmp_path / "h.db"
    make_db(db, [
        (NOW - WINDOW_MS, 20.0, 40.0, 1.0, 0),
        (NOW - WINDOW_MS // 2, 55.0, 41.0, 2.0, 0),
        (NOW, 95.0, 42.0, 4.0, 0),
    ])
    r = HistoryReader(str(db))
    assert r.available()
    h = r.read(now_ms=NOW)
    assert h.available
    cpu = next(t for t in h.trends if t.key == "cpu_total")
    assert cpu.direction == "rising"
    assert cpu.delta > 0
    assert cpu.arrow == "^"
    assert "vs 10m ago" in cpu.render()
    # Ram moved <5% and stayed low, so it is dropped as noise.
    assert not any(t.key == "ram_pct" for t in h.trends)


def test_reports_correlated_movement_and_hogs(tmp_path) -> None:
    db = tmp_path / "h.db"
    make_db(
        db,
        [
            (NOW - WINDOW_MS, 10.0, 30.0, 0.5, 1_000),
            (NOW - WINDOW_MS // 2, 50.0, 60.0, 2.0, 40_000_000),
            (NOW, 92.0, 88.0, 5.0, 90_000_000),
        ],
        procs=[
            (NOW, 4242, "rustc", 78.0, 900_000, 20_000_000),
            (NOW, 99, "bash", 2.0, 4_000, 0),
        ],
    )
    h = HistoryReader(str(db)).read(now_ms=NOW)
    assert h.available
    # Net tx is both elevated (>1MB/s) and rising.
    assert any("NetTx" in c for c in h.correlated)
    # Ram is above its 80% threshold -> elevated.
    assert any("RAM" in e for e in h.elevated)
    # rustc is the top hog and its write rate is surfaced.
    top = h.culprits[0]
    assert top.name == "rustc" and top.pid == 4242
    assert "write" in top.render()
    text = h.render()
    assert "trends:" in text and "resource hogs" in text and "rustc" in text


def test_insufficient_history_degrades(tmp_path) -> None:
    db = tmp_path / "h.db"
    make_db(db, [(NOW, 50.0, 40.0, 1.0, 0)])
    h = HistoryReader(str(db)).read(now_ms=NOW)
    assert not h.available
    assert "not enough history" in h.reason
    assert h.render() == ""


def test_window_bounds_the_query(tmp_path) -> None:
    db = tmp_path / "h.db"
    make_db(db, [
        (NOW - 10 * WINDOW_MS, 99.0, 99.0, 9.0, 0),   # far outside the window
        (NOW - 60_000, 30.0, 40.0, 1.0, 0),
        (NOW, 92.0, 41.0, 1.1, 0),
    ])
    h = HistoryReader(str(db), window_min=10).read(now_ms=NOW)
    cpu = next(t for t in h.trends if t.key == "cpu_total")
    # The 99% spike is outside the window and must not skew the baseline.
    assert cpu.baseline < 50
    assert cpu.direction == "rising"
