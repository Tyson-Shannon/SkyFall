"""skyfall_ai — anomaly-detection sidecar for the SkyFall Rust core.

Reads system snapshots (NDJSON objects on stdin), decides whether things look
*off for you*, and emits AnomalyEvents (NDJSON on stdout). Designed to run as a
child process spawned by the Rust collector.
"""

__version__ = "0.1.0"

from .engine import Engine
from .events import AnomalyEvent
from .models import Snapshot

__all__ = ["AnomalyEvent", "Engine", "Snapshot", "__version__"]