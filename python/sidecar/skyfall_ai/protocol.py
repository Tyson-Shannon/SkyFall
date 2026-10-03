"""NDJSON framing between the Rust core and this sidecar.

Messages are one JSON object per line on both stdin (Rust -> Python) and stdout
(Python -> Rust). Unknown message types are ignored so the protocol can grow
without breaking older binaries.
"""

from __future__ import annotations

import json
import sys
import threading
from typing import Any, Iterator


def parse_line(line: str) -> dict[str, Any]:
    """Parse one JSON-object line; raises `ValueError` on malformed input."""
    obj = json.loads(line)
    if not isinstance(obj, dict):
        raise ValueError(f"expected a JSON object, got {type(obj).__name__}")
    return obj


def iter_stdin_objects() -> Iterator[dict[str, Any]]:
    """Yield the `{"type": ...}` objects arriving on stdin."""
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            obj = parse_line(line)
        except (json.JSONDecodeError, ValueError):
            # Tolerate junk; never let one bad line kill the feed.
            continue
        if isinstance(obj.get("type"), str):
            yield obj


def emit(obj: dict[str, Any], out: Any = None) -> None:
    """Write one NDJSON object to stdout (or a supplied file-like object)."""
    out = out if out is not None else sys.stdout
    out.write(json.dumps(obj, separators=(",", ":")) + "\n")
    out.flush()


def hello() -> dict[str, Any]:
    """Startup handshake: what detectors are live, and why."""
    from .engine import active_detectors

    return {
        "type": "hello",
        "version": __import__("skyfall_ai").__version__,
        "detectors": active_detectors(),
    }


class Emitter:
    """Thread-safe NDJSON emitter.

    Detection replies on the main thread while narration replies on a worker, so
    two writers can reach stdout at once. The lock keeps each object on one
    unbroken line, which the Rust core's NDJSON reader depends on.
    """

    def __init__(self, out: Any = None) -> None:
        self._out = out if out is not None else sys.stdout
        self._lock = threading.Lock()

    def __call__(self, obj: dict[str, Any]) -> None:
        with self._lock:
            emit(obj, self._out)