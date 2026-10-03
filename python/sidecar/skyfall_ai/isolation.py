"""Optional ML pass (ROADMAP tier 3): IsolationForest on per-minute system
features to catch anomalies a per-metric z-score misses (odd cross-metric
combinations), plus sustained-window validation.

scikit-learn is optional: without it this detector reports "off" and produces
nothing. The rules/z-score/creep detectors never depend on it.
"""

from __future__ import annotations

from collections import deque
from typing import Any, Optional

from .baseline import Debouncer, Sustained
from .events import AnomalyEvent

try:  # scikit-learn is an optional extra
    from sklearn.ensemble import IsolationForest

    SKLEARN = True
except Exception:  # pragma: no cover - environment-specific
    SKLEARN = False


def available() -> bool:
    return SKLEARN


class MlBlip:
    """Fits an IsolationForest over a rolling window of system feature vectors and
    flags rows well below the fitted score distribution (the odd few-percent)."""

    def __init__(self,
                 fit_points: int = 300,
                 min_points: int = 120,
                 percentile: float = 3.0,
                 need: int = 3,
                 over: int = 5,
                 loss_percentile: float = 10.0):
        self.fit_points = max(60, fit_points)
        self.min_points = min(min_points, self.fit_points)
        self.percentile = percentile
        self.loss_bound = loss_percentile  # drop points when the model is stale
        self._sustained = Sustained(need=need, over=over)
        self._debouncer = Debouncer(cooldown_s=300.0)
        self._series: deque[list[float]] = deque(maxlen=fit_points)
        self._minutes: deque[int] = deque(maxlen=fit_points)
        self._model: Optional[Any] = None
        self._threshold: Optional[float] = None
        self._dirty = False

    def process(self, snap) -> list[AnomalyEvent]:
        if not SKLEARN:
            return []
        vec = self._features(snap)
        self._series.append(vec)
        self._minutes.append(snap.ts_ms // 60_000)
        self._dirty = True
        if len(self._series) < self.min_points:
            return []
        self._refit()
        return self._check(snap)

    def _features(self, snap) -> list[float]:
        temps = [t.temperature for t in snap.temps]
        gpu_util = [g.utilization for g in snap.gpus if g.utilization is not None]
        return [
            snap.cpu_total / 100.0,
            snap.ram_percent / 100.0,
            min(5.0, snap.load1) / 5.0,
            min(1.0, snap.system.get("net_rx_bps", 0) / 500e6),
            max(temps) / 100.0 if temps else 0.0,
            max(gpu_util) / 100.0 if gpu_util else 0.0,
        ]

    def _refit(self) -> None:
        if not self._dirty:
            return
        self._dirty = False
        import numpy as np

        arr = np.asarray(list(self._series), dtype=np.float64)
        model = IsolationForest(n_estimators=64, max_samples="auto",
                                contamination="auto", random_state=0)
        model.fit(arr)
        scores = model.decision_function(arr)
        self._model = model
        self._threshold = float(np.percentile(scores, self.percentile))

    def _check(self, snap) -> list[AnomalyEvent]:
        if self._model is None or self._threshold is None:
            return []
        import numpy as np

        vec = self._series[-1]
        score = float(self._model.decision_function([np.asarray(vec)])[0])
        on = self._sustained.update(score < self._threshold)
        if not self._debouncer.should_fire(f"ml:{_scope(snap)}", snap.ts_ms, on):
            return []
        return [AnomalyEvent(
            ts_ms=snap.ts_ms,
            kind="ml",
            metric="system.features",
            scope=_scope(snap),
            severity="medium",
            message=f"Multi-metric signature flagged by IsolationForest (score {score:.3f})",
            delta_vs_baseline=score,
            dossier=build_dossier_shim(snap),
        )]


def _scope(snap) -> str:
    return "global"


def build_dossier_shim(snap) -> str:
    """Small context line; reuse detection.build_dossier without a circular dep."""
    bits = [f"CPU {snap.cpu_total:.0f}% RAM {snap.ram_percent:.0f}% load {snap.load1:.2f}"]
    gpu = [f"{g.model} {g.utilization:.0f}%" for g in snap.gpus if g.utilization is not None]
    if gpu:
        bits.append("gpu: " + ", ".join(gpu))
    return " · ".join(bits)