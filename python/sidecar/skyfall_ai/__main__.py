"""Entry point: `python -m skyfall_ai`.

Listens on stdin for NDJSON objects from the Rust core:
  {"type": "hello", ...}        -> reply with detector roster
  {"type": "snapshot", ...}     -> run detection, emit zero+ events
  {"type": "shutdown"}          -> clean exit 0

Usage:  python -m skyfall_ai [--config path/to/skyfall_ai.conf.json] [--db skyfall.db]

`--db` points at the core's SQLite history so the narrator can attach trends and
co-moving metrics to an alert (read-only; omitting it just disables that context).
"""

from __future__ import annotations

import argparse
import sys
from typing import Any

from .engine import Engine, active_detectors, load_config
from .events import AnomalyEvent
from .history import HistoryReader
from .models import Snapshot
from .narrator import NarrationWorker, Narrator
from .protocol import Emitter, iter_stdin_objects
from . import __version__


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="skyfall_ai")
    ap.add_argument("--config", help="path to JSON config (optional)", default=None)
    ap.add_argument("--db", help="path to the core SQLite history (read-only)",
                    default=None)
    ap.add_argument("--history-min", type=int, default=10,
                    help="trend window in minutes (default 10)")
    ap.add_argument("--narrate-drain-s", type=float, default=2.0,
                    help="seconds to wait for in-flight narration on shutdown")
    args = ap.parse_args(argv)

    try:
        config = load_config(args.config)
    except SystemExit as exc:
        print(str(exc), file=sys.stderr)
        return 2

    emitter = Emitter()
    history = HistoryReader(args.db, window_min=args.history_min)
    engine = Engine(config, history=history)
    narrator = Narrator(config.get("narrator"))
    detectors = active_detectors()
    detectors["narrator"] = narrator.status()
    if history.available():
        detectors["history"] = f"read-only trends ({args.history_min}m)"
    emitter({"type": "hello",
             "version": __version__,
             "detectors": detectors})
    # Narration runs off the read path: a verdict can take ~90s on CPU-only
    # hardware, and blocking stdin that long would back-pressure the core.
    worker = NarrationWorker(narrator, emitter)
    worker.start()
    crashed = False
    for obj in iter_stdin_objects():
        msg = obj["type"]
        if msg == "shutdown":
            break
        if msg == "snapshot":
            try:
                snap = Snapshot.from_dict(obj)
                events = engine.process(snap)
                for event in events:
                    emitter(event.as_dict())
                worker.submit(events)
            except Exception as exc:  # pragma: no cover - resilience net
                print(f"skyfall_ai: snapshot error: {exc}", file=sys.stderr)
                crashed = True
                break
        # unknown types ignored for forward compatibility
    worker.stop(drain_timeout=args.narrate_drain_s)
    history.close()
    return 1 if crashed else 0


if __name__ == "__main__":
    raise SystemExit(main())