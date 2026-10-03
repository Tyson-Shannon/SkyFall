"""Output event type shared across detectors."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Optional


@dataclass
class AnomalyEvent:
    ts_ms: int
    kind: str            # "rule" | "baseline" | "creep" | "ml"
    metric: str          # e.g. "system.cpu_total"
    scope: str           # e.g. "global", "gpu:0000:01:00.0", "proc:123"
    severity: str        # low | medium | high | critical
    message: str         # human-readable, notification-ready line
    delta_vs_baseline: Optional[float] = None
    dossier: str = ""    # multi-line context window for the LLM layer (phase 5)

    def as_dict(self) -> dict[str, Any]:
        return {
            "type": "event",
            "ts_ms": self.ts_ms,
            "kind": self.kind,
            "metric": self.metric,
            "scope": self.scope,
            "severity": self.severity,
            "delta_vs_baseline": self.delta_vs_baseline,
            "message": self.message,
            "dossier": self.dossier,
        }


def severity_for_z(z: float) -> str:
    az = abs(z)
    if az > 8:
        return "critical"
    if az > 5:
        return "high"
    return "medium"