"""Read-only history access for causal context (phase 6).

The core persists every snapshot into SQLite (``system_metrics``,
``process_metrics``, ``disk_metrics``, ``gpu_metrics``). A single-snapshot dossier
can tell the narrator *what* is hot, but not *why* — a CPU at 90% looks identical
whether it is a compile, a stuck spin-loop, or a page-cache thrash.

This module opens the same database read-only and derives the three things a
root-cause analysis needs and a snapshot cannot supply:

1. **Trends** — current value vs. a baseline window ago, with direction and slope,
   so "ramping since 09:14" is distinguishable from "pinned at 95% for an hour".
2. **Correlation** — which *other* metrics moved at the same time. A CPU spike
   that coincides with disk-write saturation is a very different incident from
   one that coincides with rising swap.
3. **Attribution** — which processes were actually burning CPU/IO over the same
   window, not just at the instant the alert fired.

The reader is strictly best-effort: every failure degrades to "no extra context"
rather than breaking detection, and it never writes to the database.
"""

from __future__ import annotations

import os
import sqlite3
import time
from dataclasses import dataclass, field
from typing import Any, Optional

# Columns we surface from system_metrics, with the display label and unit.
_SYSTEM_METRICS = {
    "cpu_total": ("CPU", "%"),
    "ram_pct": ("RAM", "%"),
    "swap_used": ("Swap", "MB"),
    "load1": ("Load1", ""),
    "net_rx_bps": ("NetRx", "MB/s"),
    "net_tx_bps": ("NetTx", "MB/s"),
}

# Thresholds for deciding a metric is "elevated" and thus worth correlating.
_ELEVATED = {
    "cpu_total": 60.0,
    "ram_pct": 80.0,
    "net_tx_bps": 1_000_000.0,
    "net_rx_bps": 1_000_000.0,
    "swap_used": 1.0,
}

# A metric is "moving" when it changed by more than this across the window.
_MOVING = {
    "cpu_total": 10.0,
    "ram_pct": 5.0,
    "net_tx_bps": 500_000.0,
    "net_rx_bps": 500_000.0,
    "swap_used": 1.0,
}


def _mb(v: float) -> float:
    return v / (1024.0 * 1024.0)


def _fmt_bytes_bps(v: float) -> str:
    v = float(v)
    if v >= 1e9:
        return f"{v / 1e9:.1f}GB/s"
    if v >= 1e6:
        return f"{_mb(v):.0f}MB/s"
    if v >= 1e3:
        return f"{v / 1e3:.0f}KB/s"
    return f"{v:.0f}B/s"


def _fmt_value(key: str, v: float) -> str:
    """Format a metric value with the right unit for its key."""
    v = float(v)
    if key.endswith("_bps"):
        return _fmt_bytes_bps(v)
    unit = _SYSTEM_METRICS.get(key, ("", ""))[1]
    if unit == "%":
        return f"{v:.0f}%"
    if key == "swap_used":
        return f"{_mb(v):.0f}MB"
    if key == "load1":
        return f"{v:.1f}"
    return f"{v:.1f}{unit}"


@dataclass
class MetricTrend:
    """One metric's recent behaviour."""
    key: str
    label: str
    current: Optional[float]
    baseline: Optional[float]
    window_min: int
    direction: str          # "rising" | "falling" | "flat" | "unknown"
    delta: Optional[float]
    sustained_s: float      # how long the current excursion has held

    @property
    def arrow(self) -> str:
        return {"rising": "^", "falling": "v", "flat": "-"}.get(self.direction, "?")

    def render(self) -> str:
        if self.current is None:
            return f"{self.label} n/a"
        cur = _fmt_value(self.key, self.current)
        if self.baseline is None or self.delta is None:
            return f"{self.label} {cur}"
        sign = "+" if self.delta >= 0 else ""
        base = _fmt_value(self.key, self.baseline)
        return (f"{self.label} {cur} ({sign}{_fmt_value(self.key, self.delta)} "
                f"vs {self.window_min}m ago {base})")

    def as_dict(self) -> dict[str, Any]:
        return {
            "key": self.key,
            "label": self.label,
            "current": self.current,
            "baseline": self.baseline,
            "window_min": self.window_min,
            "direction": self.direction,
            "delta": self.delta,
            "sustained_s": round(self.sustained_s, 1),
        }


@dataclass
class ProcessBlame:
    """A process that stood out over the analysis window."""
    pid: int
    name: str
    cpu_avg: float
    cpu_peak: float
    ram_kb: int
    read_bps: float = 0.0
    write_bps: float = 0.0

    def render(self) -> str:
        bits = [f"{self.name}({self.pid}) cpu~{self.cpu_avg:.0f}%"]
        if self.cpu_peak > self.cpu_avg * 1.5 + 5:
            bits.append(f"peak {self.cpu_peak:.0f}%")
        if self.ram_kb > 0:
            bits.append(f"ram {self.ram_kb / 1024:.0f}MB")
        if self.write_bps > 10_000:
            bits.append(f"write {_fmt_bytes_bps(self.write_bps)}")
        if self.read_bps > 10_000:
            bits.append(f"read {_fmt_bytes_bps(self.read_bps)}")
        return " ".join(bits)

    def as_dict(self) -> dict[str, Any]:
        return {
            "pid": self.pid,
            "name": self.name,
            "cpu_avg": round(self.cpu_avg, 1),
            "cpu_peak": round(self.cpu_peak, 1),
            "ram_kb": self.ram_kb,
            "read_bps": self.read_bps,
            "write_bps": self.write_bps,
        }


@dataclass
class History:
    """Correlated context for one alert."""
    available: bool = False
    reason: str = ""
    window_min: int = 10
    trends: list[MetricTrend] = field(default_factory=list)
    correlated: list[str] = field(default_factory=list)
    culprits: list[ProcessBlame] = field(default_factory=list)
    elevated: list[str] = field(default_factory=list)

    def render(self) -> str:
        """Multi-line context block handed to the narrator."""
        if not self.available:
            return ""
        lines: list[str] = []
        if self.trends:
            lines.append("trends: " + "; ".join(t.render() for t in self.trends))
        if self.elevated:
            lines.append("also elevated right now: " + ", ".join(self.elevated))
        if self.correlated:
            lines.append("rising together: " + ", ".join(self.correlated))
        if self.culprits:
            lines.append("resource hogs over window: "
                         + "; ".join(p.render() for p in self.culprits[:4]))
        return "\n".join(lines)

    def as_dict(self) -> dict[str, Any]:
        return {
            "available": self.available,
            "reason": self.reason,
            "window_min": self.window_min,
            "trends": [t.as_dict() for t in self.trends],
            "correlated": self.correlated,
            "culprits": [p.as_dict() for p in self.culprits],
            "elevated": self.elevated,
        }


class HistoryReader:
    """Read-only accessor over the core's SQLite history.

    Holds a single long-lived connection opened in read-only URI mode so we
    never contend on the writer's lock (WAL readers are non-blocking anyway).
    If the database is missing or unreadable, every query degrades to empty and
    :meth:`read` returns ``History(available=False, reason=...)``.
    """

    def __init__(self, db_path: Optional[str], window_min: int = 10):
        self.db_path = db_path
        self.window_min = max(1, int(window_min))
        self._conn: Optional[sqlite3.Connection] = None
        self._unavailable = ""
        if db_path and db_path != ":memory:":
            self._connect(db_path)
        elif db_path == ":memory:":
            self._unavailable = "in-memory database (no history)"
        else:
            self._unavailable = "no database path configured"

    def _connect(self, path: str) -> None:
        try:
            if not os.path.exists(path):
                self._unavailable = f"database {path} not found yet"
                return
            uri = f"file:{os.path.abspath(path)}?mode=ro"
            conn = sqlite3.connect(uri, uri=True, timeout=2.0)
            conn.row_factory = sqlite3.Row
            # Cheap liveness probe; a locked/mismatched DB fails here.
            conn.execute("SELECT 1 FROM sqlite_master LIMIT 1").fetchone()
            self._conn = conn
        except sqlite3.Error as exc:
            self._conn = None
            self._unavailable = f"history unavailable: {exc}"

    def close(self) -> None:
        if self._conn is not None:
            try:
                self._conn.close()
            except sqlite3.Error:
                pass
            self._conn = None

    def available(self) -> bool:
        """True when a readable database was found (context may still be thin)."""
        return self._conn is not None

    # -- queries ------------------------------------------------------------

    def _system_window(self, now_ms: int) -> list[sqlite3.Row]:
        if self._conn is None:
            return []
        since = now_ms - self.window_min * 60_000
        try:
            return self._conn.execute(
                "SELECT ts, cpu_total, ram_pct, swap_used, load1, net_rx_bps, net_tx_bps "
                "FROM system_metrics WHERE ts >= ?1 ORDER BY ts ASC",
                (since,),
            ).fetchall()
        except sqlite3.Error:
            return []

    def _trends(self, rows: list[sqlite3.Row], now_ms: int) -> list[MetricTrend]:
        if len(rows) < 2:
            return []
        out: list[MetricTrend] = []
        for key, (label, _unit) in _SYSTEM_METRICS.items():
            series = [(r["ts"], r[key]) for r in rows if r[key] is not None]
            if len(series) < 2:
                continue
            current = float(series[-1][1])
            baseline = float(series[0][1])
            # Baseline = mean of the oldest quarter, to damp single-sample noise.
            head = series[: max(1, len(series) // 4)]
            baseline = sum(v for _, v in head) / len(head)
            delta = current - baseline
            moving = _MOVING.get(key, 1.0)
            if delta > moving:
                direction = "rising"
            elif delta < -moving:
                direction = "falling"
            else:
                direction = "flat"
            sustained = (now_ms - series[0][0]) / 1000.0
            # Skip metrics that never moved: they add noise, not evidence.
            if direction == "flat" and abs(current) < _ELEVATED.get(key, float("inf")):
                continue
            out.append(MetricTrend(
                key=key, label=label, current=current, baseline=baseline,
                window_min=self.window_min, direction=direction, delta=delta,
                sustained_s=sustained,
            ))
        return out

    def _correlations(self, trends: list[MetricTrend]) -> tuple[list[str], list[str]]:
        """Split other metrics into 'elevated' and 'rising together' with the top one."""
        elevated: list[str] = []
        rising: list[str] = []
        if not trends:
            return elevated, rising
        # Classify every metric on its own merits so a multi-metric story keeps
        # all of its parts; the caller already knows which metric fired.
        for t in trends:
            cur = t.current or 0.0
            if cur >= _ELEVATED.get(t.key, float("inf")):
                elevated.append(f"{t.label} {_fmt_value(t.key, cur)}")
            if t.direction == "rising" and t.delta is not None and t.delta > 0:
                rising.append(f"{t.label} {arrow_of(t)}")
        return elevated, rising

    def _culprits(self, now_ms: int, limit: int = 4) -> list[ProcessBlame]:
        if self._conn is None:
            return []
        since = now_ms - self.window_min * 60_000
        try:
            rows = self._conn.execute(
                "SELECT pid, name, AVG(cpu) AS cpu_avg, MAX(cpu) AS cpu_peak, "
                "       MAX(ram_kb) AS ram_kb, AVG(disk_r_bps) AS read_bps, "
                "       AVG(disk_w_bps) AS write_bps, COUNT(*) AS n "
                "FROM process_metrics WHERE ts >= ?1 "
                "GROUP BY pid, name "
                "HAVING n > 0 "
                "ORDER BY cpu_avg DESC, write_bps DESC LIMIT ?2",
                (since, limit),
            ).fetchall()
        except sqlite3.Error:
            return []
        return [
            ProcessBlame(
                pid=r["pid"], name=r["name"] or "?",
                cpu_avg=r["cpu_avg"] or 0.0, cpu_peak=r["cpu_peak"] or 0.0,
                ram_kb=r["ram_kb"] or 0, read_bps=r["read_bps"] or 0.0,
                write_bps=r["write_bps"] or 0.0,
            )
            for r in rows
        ]

    def read(self, now_ms: Optional[int] = None, subject: Optional[str] = None) -> History:
        """Build correlated context for the alert at ``now_ms``.

        ``subject`` (e.g. ``system.cpu_total``) biases the correlation toward the
        metric that actually fired so the narrator sees related movement first.
        """
        if self._conn is None:
            return History(available=False, reason=self._unavailable or "history disabled",
                           window_min=self.window_min)
        now = int(now_ms if now_ms is not None else time.time() * 1000)
        rows = self._system_window(now)
        if len(rows) < 2:
            return History(
                available=False,
                reason="not enough history yet (need a few samples)",
                window_min=self.window_min,
            )
        trends = self._trends(rows, now)
        elevated, rising = self._correlations(trends)
        culprits = self._culprits(now)
        return History(
            available=True,
            window_min=self.window_min,
            trends=trends,
            correlated=rising,
            elevated=elevated,
            culprits=culprits,
        )


def arrow_of(t: MetricTrend) -> str:
    return t.arrow
