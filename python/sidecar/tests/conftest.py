"""Reusable synthetic snapshot builders for the test suite."""

from __future__ import annotations

from skyfall_ai.models import Disk, Gpu, Proc, Snapshot, Temp


def snap(ts_ms: int, *, cpu: float | None = 15.0, ram: float | None = 40.0,
         gpus=(), disks=(), temps=(), procs=(), **system) -> Snapshot:
    sysd: dict = {
        "cpu_total": cpu,
        "ram_percent": ram,
        "ram_used": 0,
        "swap_used": 0,
        "load1": 0.5,
        "net_rx_bps": 0,
    }
    sysd.update(system)
    return Snapshot(
        ts_ms=ts_ms,
        system=sysd,
        gpus=[Gpu(**g) for g in gpus],
        disks=[Disk(**d) for d in disks],
        temps=[Temp(**t) for t in temps],
        processes=[Proc(**p) for p in procs],
    )


def minute(m: int) -> int:
    return m * 60_000