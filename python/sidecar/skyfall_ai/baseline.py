"""Per-minute baseline statistics.

Keeps the mean value of each metric, bucketed per minute, over a rolling window
(default 7 days, matching ROADMAP). Memory is O(metrics * window) floats — a few
hundred KB at consumer scale. "Off for you" means a sustained |z| > 3 against the
mean/std this learns.

Welford-style stats are overkill here; a deque of per-minute means and numpy mean/std
over the resident window is accurate and fast enough for one value per metric/minute.
"""

from __future__ import annotations

from collections import deque
from typing import Optional


class Baseline:
    def __init__(self, window_minutes: int = 7 * 24 * 60, warmup_minutes: int = 30):
        self.window = max(1, min(window_minutes, 30 * 24 * 60))
        self.warmup = max(1, min(warmup_minutes, self.window))
        self._minutes: dict[str, deque[float]] = {}
        self._buckets: dict[str, list[float]] = {}
        self._cur_minute: int = -1

    def reset(self, scope: str) -> None:
        """Forget everything learned about one metric scope (e.g. app uninstall)."""
        self._minutes.pop(scope, None)
        self._buckets.pop(scope, None)

    def ready(self, scope: str) -> bool:
        return len(self._minutes.get(scope, ())) >= self.warmup

    def count(self, scope: str) -> int:
        return len(self._minutes.get(scope, ()))

    def stats(self, scope: str) -> Optional[tuple[float, float]]:
        values = self._minutes.get(scope)
        if not values:
            return None
        n = len(values)
        mean = sum(values) / n
        if n > 1:
            var = sum((v - mean) ** 2 for v in values) / (n - 1)
            std = var ** 0.5
        else:
            std = 0.0
        return mean, std

    def observe(self, ts_ms: int, scope: str, value: float) -> Optional[float]:
        """Feed one sample; returns the z-score of the just-completed minute once
        the baseline is warm, else None."""
        minute = ts_ms // 60_000
        if minute != self._cur_minute:
            self._flush()
            self._cur_minute = minute
        self._buckets.setdefault(scope, []).append(float(value))
        if not self.ready(scope):
            return None
        stats = self.stats(scope)
        if stats is None:
            return None
        mean, std = stats
        z = (float(value) - mean) / (std if std > 1e-9 else 1e-9)
        return z

    def _flush(self) -> None:
        if not self._buckets:
            return
        for scope, values in self._buckets.items():
            self._minutes.setdefault(scope, deque(maxlen=self.window)).append(
                sum(values) / len(values)
            )
        self._buckets = {}


class Sustained:
    """Require `need` of the trailing `over` observations to consider a condition on.

    Smoothes sampling jitter so a single 500 ms spike under a 3 s sampling cadence
    does not raise an alert, matching the ROADMAP's "sustained" rule wording.
    A miss (condition turns off) resets the streak; a real recovery below the
    hysteresis floor is expressed by callers via `reset()`.
    """

    def __init__(self, need: int = 3, over: int = 5):
        self.need = max(1, need)
        self.over = max(over, need)
        self._hits: int = 0
        self._window: int = 0

    def update(self, on: bool) -> bool:
        if on:
            self._hits = min(self._hits + 1, self.over)
        else:
            self._hits = 0
        self._window = min(self._window + 1, self.over)
        return self._hits >= self.need and self._window >= self.need

    def reset(self) -> None:
        self._hits = 0
        self._window = 0


class Debouncer:
    """Coalesce events per scope: fire on a fresh anomaly, re-fire after a cooldown,
    and reset the moment the condition recovers (hysteresis)."""

    def __init__(self, cooldown_s: float = 300.0):
        self.cooldown_s = max(0.0, cooldown_s)
        self._states: dict[str, bool] = {}
        self._last_fired: dict[str, float] = {}

    def should_fire(self, scope: str, ts_ms: int, anomalous: bool) -> bool:
        sec = ts_ms / 1000.0
        was_active = self._states.get(scope, False)
        if not anomalous:
            self._states[scope] = False
            return False
        if not was_active:
            self._states[scope] = True
            self._last_fired[scope] = sec
            return True
        # Still anomalous: only remind after the cooldown has elapsed.
        if sec - self._last_fired.get(scope, -1e18) >= self.cooldown_s:
            self._last_fired[scope] = sec
            return True
        return False