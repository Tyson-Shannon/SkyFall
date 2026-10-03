"""Detector orchestration and configuration.

The engine turns a raw snapshot into zero or more AnomalyEvents. Config is a
nested dict merged over built-in defaults, loadable from a JSON file via
`--config`. The Rust core sends the same JSON snapshot object the dashboard
sees, so the sidecar needs no independent data plumbing.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Optional

from .detection import DEFAULT_RULES, BaselineZ, Creep, Rules, build_dossier
from .events import AnomalyEvent
from .isolation import MlBlip, available
from .models import Snapshot
from .narrator import (
    DEFAULT_KEEP_ALIVE_S,
    DEFAULT_MAX_PER_BURST,
    DEFAULT_MODEL,
    DEFAULT_TIMEOUT_S,
)


def default_config() -> dict[str, Any]:
    return {
        "baseline": {"window_minutes": 7 * 24 * 60, "warmup_minutes": 30},
        "creep": {
            "window_minutes": 240, "min_points": 8,
            "rss_kb_per_min": 15000.0, "disk_pct_per_min": 0.5,
            "cooldown_s": 900.0,
        },
        "rules": {},
        "ml": {"fit_points": 300, "min_points": 120, "percentile": 3.0,
               "need": 3, "over": 5},
        "narrator": {
            "enabled": True,
            "base_url": "http://localhost:11434",
            "model": DEFAULT_MODEL,
            "api_key": None,
            "timeout_s": DEFAULT_TIMEOUT_S,
            "throttle_s": 300.0,
            # Release the model as soon as a verdict is in: a resident runner
            # costs gigabytes of RAM for a verdict nobody is waiting on.
            "keep_alive_s": DEFAULT_KEEP_ALIVE_S,
            # One snapshot can trip a dozen sensors at once; the throttle is per
            # sensor, so without a cap a single bad moment costs a dozen verdicts.
            "max_per_burst": DEFAULT_MAX_PER_BURST,
        },
    }


def packaged_config() -> Path:
    """The config shipped next to the package (tuned rules, default narrator)."""
    return Path(__file__).resolve().parent.parent / "skyfall_ai.conf.json"


def load_config(path: Optional[Path | str] = None) -> dict[str, Any]:
    """Load user config (JSON) on top of defaults; a missing file is fine.

    The core always passes `--config` so Settings has somewhere to save the
    narrator settings, which means the path usually does not exist yet on a fresh
    install. Falling back to the packaged config keeps detection identical to the
    no-flag case instead of silently downgrading to the bare defaults (which ship
    no rules).
    """
    cfg = default_config()
    p = Path(path).expanduser() if path is not None else packaged_config()
    if not p.exists() and path is not None:
        p = packaged_config()
    if p.exists():
        try:
            user = json.loads(p.read_text())
        except (OSError, ValueError) as exc:
            raise SystemExit(f"bad sidecar config {p}: {exc}") from exc
        if isinstance(user, dict):
            # The narrator block needs a key-level merge: the desktop settings
            # dialog only knows enabled/base_url/model/api_key, and a top-level
            # `update` would drop timeout_s/throttle_s/keep_alive_s along with it.
            # Snapshot the defaults first — `update` is about to overwrite them.
            narrator = dict(cfg.get("narrator") or {})
            cfg.update(user)
            narrator.update(user.get("narrator") or {})
            cfg["narrator"] = narrator
    return cfg


class Engine:
    def __init__(self, config: Optional[dict[str, Any]] = None,
                 history: Optional[Any] = None):
        self.config = config if config is not None else load_config()
        # Optional HistoryReader used to enrich dossiers with causal context.
        self.history = history

        base = self.config.get("baseline", {})
        self.baseline_z = BaselineZ(
            window_minutes=base.get("window_minutes", 7 * 24 * 60),
            warmup_minutes=base.get("warmup_minutes", 30),
        )

        user_rules = self.config.get("rules")
        self.rules = Rules(user_rules if isinstance(user_rules, dict) and user_rules
                           else DEFAULT_RULES)
        self.baseline_z = BaselineZ(
            window_minutes=base.get("window_minutes", 7 * 24 * 60),
            warmup_minutes=base.get("warmup_minutes", 30),
        )

        c = self.config.get("creep", {})
        self.creep = Creep(
            window_minutes=c.get("window_minutes", 240),
            min_points=c.get("min_points", 8),
            rss_kb_per_min=c.get("rss_kb_per_min", 15_000.0),
            disk_pct_per_min=c.get("disk_pct_per_min", 0.5),
            cooldown_s=c.get("cooldown_s", 900.0),
        )

        m = self.config.get("ml", {})
        self.ml = MlBlip(
            fit_points=m.get("fit_points", 300),
            min_points=m.get("min_points", 120),
            percentile=m.get("percentile", 3.0),
            need=m.get("need", 3),
            over=m.get("over", 5),
        )

    def process(self, snap: Snapshot) -> list[AnomalyEvent]:
        if not snap.ts_ms:
            return []
        events: list[AnomalyEvent] = []
        events += self.rules.process(snap)
        events += self.baseline_z.process(snap)
        events += self.creep.process(snap)
        events += self.ml.process(snap)
        # Phase 6: enrich every event's dossier with correlated history (trends,
        # co-moving metrics, resource hogs) so the narrator can infer a cause.
        if self.history is not None:
            try:
                ctx = self.history.read(now_ms=snap.ts_ms)
            except Exception:  # never let context gathering break detection
                ctx = None
            if ctx is not None and getattr(ctx, "available", False):
                enriched = build_dossier(snap, ctx)
                for e in events:
                    e.dossier = enriched
        events.sort(key=lambda e: e.ts_ms)
        return events


def active_detectors() -> dict[str, str]:
    ml = "on" if available() else "off (scikit-learn not installed)"
    return {"rules": "on", "baseline_z": "on", "creep": "on", "ml": ml}