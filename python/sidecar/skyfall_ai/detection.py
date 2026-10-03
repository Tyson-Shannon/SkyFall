"""Detectors: hard rules, baseline z-scores, and creep (regression / IsolationForest).

Each detector owns its state (sustained windows, debouncers, per-minute buckets)
and implements `process(snapshot) -> list[AnomalyEvent]`. Missing numpy means the
whole engine refuses to start; a missing scikit-learn only disables the Isolation
Forest pass (the rest still runs, see ROADMAP "ML creep" degradation).
"""

from __future__ import annotations

from collections import deque
from typing import Any, Iterator, Optional

from .baseline import Baseline, Debouncer, Sustained
from .events import AnomalyEvent, severity_for_z

DEFAULT_RULES: dict[str, dict[str, Any]] = {
    "cpu_spike": {
        "metric": "system.cpu_total", "gt": 90.0, "need": 4, "of": 5,
        "hysteresis": 10.0, "severity": "medium", "cooldown_s": 300.0,
    },
    "memory_pressure": {
        "metric": "system.ram_percent", "gt": 95.0, "need": 3, "of": 5,
        "hysteresis": 3.0, "severity": "high", "cooldown_s": 600.0,
    },
    "disk_low": {
        "metric": "disk.percent", "gt": 90.0, "need": 3, "of": 5,
        "hysteresis": 2.0, "severity": "high", "cooldown_s": 600.0,
    },
    "gpu_busy": {
        "metric": "gpu.utilization", "gt": 95.0, "need": 4, "of": 5,
        "hysteresis": 10.0, "severity": "medium", "cooldown_s": 300.0,
    },
    "thermal": {
        "metric": "temp.celsius", "gt": 85.0, "need": 3, "of": 5,
        "hysteresis": 5.0, "severity": "critical", "cooldown_s": 300.0,
    },
}


def metric_feed(snap) -> Iterator[tuple[str, str, float, str]]:
    """Yield (base_metric, scope, value, label) for every measurable surface."""
    if snap.cpu_total is not None:
        yield ("system.cpu_total", "global", snap.cpu_total, "CPU")
    if snap.ram_percent is not None:
        yield ("system.ram_percent", "global", snap.ram_percent, "Memory")
    if snap.swap_used:
        yield ("system.swap", "global", snap.swap_used, "Swap used")
    for i, d in enumerate(snap.disks):
        mount = d.mount_point or f"disk{i}"
        yield ("disk.percent", f"disk:{mount}", d.percent, f"Disk {mount}")
    for g in snap.gpus:
        if g.utilization is not None:
            yield ("gpu.utilization", f"gpu:{g.id}", g.utilization,
                   f"GPU {g.model or g.id}")
        if g.temperature is not None:
            yield ("gpu.temperature", f"gpu:{g.id}", g.temperature,
                   f"GPU temp {g.model or g.id}")
    for t in snap.temps:
        yield ("temp.celsius", f"temp:{t.label}", t.temperature, t.label)


class Rules:
    """Hard thresholds with hysteresis, sustained windows, and per-scope debounce."""

    def __init__(self, rules: Optional[dict[str, dict[str, Any]]] = None):
        self.rules = rules or DEFAULT_RULES
        self._windows: dict[str, Sustained] = {}
        self._debouncer = Debouncer()

    def process(self, snap) -> list[AnomalyEvent]:
        events: list[AnomalyEvent] = []
        feed = list(metric_feed(snap))
        for name, rule in self.rules.items():
            base = rule["metric"]
            gt = float(rule["gt"])
            hyst = float(rule.get("hysteresis", 5.0))
            for fmetric, scope, value, label in feed:
                if fmetric != base:
                    continue
                key = f"{name}:{scope}"
                window = self._windows.setdefault(
                    key, Sustained(int(rule.get("need", 3)), int(rule.get("of", 5)))
                )
                on = window.update(value > gt)
                if value <= gt - hyst:
                    window.reset()
                if not self._debouncer.should_fire(key, snap.ts_ms, on and value > gt - hyst):
                    continue
                unit = "\u00b0C" if base.endswith("celsius") else "%"
                events.append(AnomalyEvent(
                    ts_ms=snap.ts_ms,
                    kind="rule",
                    metric=base,
                    scope=scope,
                    severity=rule.get("severity", "medium"),
                    message=f"{label} at {value:.0f}{unit} sustained > {gt:.0f}{unit}",
                    dossier=build_dossier(snap),
                ))
        return events


class BaselineZ:
    """Supports the ROADMAP "baseline anomaly": each metric's minute mean/std over
    a rolling window; a |z| > 3 means *off for you*.

    Because the rolling window absorbs any persistent shift within a few minutes,
    we do not demand a sustained window here — the debouncer's cooldown handles
    re-alerting instead. A hard step fires immediately (huge z before the window
    catches up); a slow drift fires the moment it crosses 3 sigma.
    """

    Z_LIMIT = 3.0

    def __init__(self, window_minutes: int = 7 * 24 * 60, warmup_minutes: int = 30):
        self.baseline = Baseline(window_minutes=window_minutes, warmup_minutes=warmup_minutes)
        self._debouncer = Debouncer(cooldown_s=600.0)

    def process(self, snap) -> list[AnomalyEvent]:
        events: list[AnomalyEvent] = []
        for base, scope, value, label in metric_feed(snap):
            if value is None:
                continue
            z = self.baseline.observe(snap.ts_ms, f"{base}:{scope}", value)
            if z is None or abs(z) <= self.Z_LIMIT:
                continue
            key = f"{base}:{scope}"
            if not self._debouncer.should_fire(key, snap.ts_ms, True):
                continue
            stats = self.baseline.stats(f"{base}:{scope}") or (0.0, 0.0)
            delta = value - stats[0]
            events.append(AnomalyEvent(
                ts_ms=snap.ts_ms,
                kind="baseline",
                metric=base,
                scope=scope,
                severity=severity_for_z(z),
                message=(
                    f"{label} {value:.1f} deviates {delta:+.1f} from your baseline "
                    f"(mean {stats[0]:.1f}, z={z:.1f})"
                ),
                delta_vs_baseline=delta,
                dossier=build_dossier(snap),
            ))
        return events


def _slope(xs: list[float], ys: list[float]) -> Optional[float]:
    """Least-squares slope of ys-per-xs; None when underdetermined."""
    n = len(xs)
    if n < 3:
        return None
    mx = sum(xs) / n
    my = sum(ys) / n
    num = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    den = sum((x - mx) ** 2 for x in xs)
    if den <= 1e-9:
        return None
    return num / den


class Creep:
    """Slow trends your baseline z-score can't see: growing process RSS (memory
    leaks) and disk-fill creep, via per-minute regression slopes."""

    def __init__(self,
                 window_minutes: int = 240,
                 min_points: int = 8,
                 rss_kb_per_min: float = 15_000.0,
                 disk_pct_per_min: float = 0.5,
                 cooldown_s: float = 900.0):
        self.window = max(min_points, min(window_minutes, 24 * 60))
        self.min_points = min_points
        self.rss_kb_per_min = rss_kb_per_min
        self.disk_pct_per_min = disk_pct_per_min
        self._debouncer = Debouncer(cooldown_s=cooldown_s)
        # (kind, scope) -> deque[(minute_idx, mean_value)]
        self._series: dict[tuple[str, str], deque[tuple[int, float]]] = {}
        self._buckets: dict[tuple[str, str], list[float]] = {}
        self._cur_minute = -1

    def process(self, snap) -> list[AnomalyEvent]:
        minute = snap.ts_ms // 60_000
        if minute != self._cur_minute:
            self._flush()
            self._cur_minute = minute

        for p in snap.processes:
            self._observe(("ram", f"proc:{p.pid}"), minute, float(p.ram_kb))
        for d in snap.disks:
            self._observe(("disk", f"disk:{d.mount_point}"), minute, float(d.percent))

        events: list[AnomalyEvent] = []
        events += self._check_ram(snap)
        events += self._check_disk(snap)
        return events

    def _observe(self, key: tuple[str, str], minute: int, value: float) -> None:
        if value is None:
            return
        self._buckets.setdefault(key, []).append(value)
        # Keep the bucket fresh-bound to the actual minute observed.
        self._series.setdefault(key, deque(maxlen=self.window))

    def _flush(self) -> None:
        if not self._buckets:
            return
        for key, values in self._buckets.items():
            self._series.setdefault(key, deque(maxlen=self.window)).append(
                (self._cur_minute, sum(values) / len(values))
            )
        self._buckets = {}

    def _check_ram(self, snap) -> list[AnomalyEvent]:
        events: list[AnomalyEvent] = []
        for key, series in self._series.items():
            kind, scope = key
            if kind != "ram":
                continue
            if len(series) < self.min_points:
                continue
            xs = [m for m, _ in series]
            ys = [v for _, v in series]
            slope = _slope(xs, ys)
            if slope is None or slope < self.rss_kb_per_min:
                continue
            if self._debouncer.should_fire(scope, snap.ts_ms, True):
                events.append(AnomalyEvent(
                    ts_ms=snap.ts_ms, kind="creep", metric="process.ram_kb",
                    scope=scope,
                    severity="high",
                    message=(
                        f"{scope} RSS growing ~{slope / 1024.0:.0f} MB/min over the last "
                        f"{len(series)} min — possible memory leak"
                    ),
                    dossier=build_dossier(snap),
                ))
        return events

    def _check_disk(self, snap) -> list[AnomalyEvent]:
        events: list[AnomalyEvent] = []
        for key, series in self._series.items():
            kind, scope = key
            if kind != "disk":
                continue
            if len(series) < self.min_points:
                continue
            xs = [m for m, _ in series]
            ys = [v for _, v in series]
            slope = _slope(xs, ys)
            if slope is None or slope < self.disk_pct_per_min or not ys:
                continue
            if self._debouncer.should_fire(scope, snap.ts_ms, True):
                remaining_h = (100.0 - ys[-1]) / slope / 60.0
                events.append(AnomalyEvent(
                    ts_ms=snap.ts_ms, kind="creep", metric="disk.percent",
                    scope=scope,
                    severity="high" if remaining_h < 48 else "medium",
                    message=(
                        f"{scope} filling ~{slope:.2f}%/min — estimated full in "
                        f"{remaining_h:.0f} h"
                    ),
                    delta_vs_baseline=slope,
                    dossier=build_dossier(snap),
                ))
        return events


def build_dossier(snap, history=None) -> str:
    """A context dump for the notification / LLM layer (phase 5/6).

    Phase 6 splits the dossier in two: the *state* line (what is hot right now)
    and — when a :class:`~skyfall_ai.history.HistoryReader` supplied context —
    a *causal* block with trends, correlated movement, and the processes that
    actually burned resources over the window. The causal block is what lets
    the narrator name a root cause instead of restating the metric.
    """
    bits = [f"CPU {snap.cpu_total:.0f}%  RAM {snap.ram_percent:.0f}%  load {snap.load1:.2f}"]
    top = snap.top_procs[:4]
    if top:
        bits.append("top CPU: " + ", ".join(f"{p.name} {p.cpu_percent:.0f}%" for p in top))
    if snap.temps:
        bits.append("temps: " + ", ".join(f"{t.label} {t.temperature:.0f}C" for t in snap.temps[:5]))
    if snap.gpus:
        bits.append("gpu: " + "; ".join(
            f"{g.model} {g.utilization:.0f}%" for g in snap.gpus if g.utilization is not None
        ) or "gpu: idle")
    if snap.disks:
        bits.append("disk: " + ", ".join(
            f"{d.mount_point} {d.percent:.0f}%" for d in snap.disks[:4]
        ))
    state = " · ".join(bits)
    if history is None:
        return state
    rendered = history.render() if hasattr(history, "render") else str(history)
    return f"{state}\n{rendered}" if rendered else state