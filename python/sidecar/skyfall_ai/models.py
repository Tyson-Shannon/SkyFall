"""Parsed snapshots and the metric surface the detectors consume.

Field names mirror the Rust `Snapshot` serialization so no mapping is needed on
the wire; parsing is tolerant of missing keys so older cores never crash the
sidecar.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Optional


@dataclass
class Gpu:
    id: str
    model: str = ""
    utilization: Optional[float] = None
    temperature: Optional[float] = None
    memory_used: Optional[int] = None
    memory_total: Optional[int] = None
    power_watts: Optional[float] = None
    clock_mhz: Optional[float] = None


@dataclass
class Disk:
    mount_point: str = ""
    percent: float = 0.0
    used: int = 0
    total: int = 0


@dataclass
class Temp:
    label: str = ""
    temperature: float = 0.0


@dataclass
class Proc:
    pid: int = 0
    name: str = ""
    cpu_percent: float = 0.0
    ram_kb: int = 0


@dataclass
class Snapshot:
    ts_ms: int = 0
    system: dict[str, Any] = field(default_factory=dict)
    gpus: list[Gpu] = field(default_factory=list)
    disks: list[Disk] = field(default_factory=list)
    temps: list[Temp] = field(default_factory=list)
    processes: list[Proc] = field(default_factory=list)

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> "Snapshot":
        system = raw.get("system") or {}
        return cls(
            ts_ms=raw.get("ts_ms", 0),
            system=system,
            gpus=[parse_gpu(g) for g in raw.get("gpus") or []],
            disks=[parse_disk(d) for d in raw.get("disks") or []],
            temps=[Temp(t.get("label", ""), t.get("temperature", 0.0))
                   for t in raw.get("temps") or []],
            processes=[parse_proc(p) for p in raw.get("processes") or []],
        )

    # Convenience accessors ---------------------------------------------------
    @property
    def cpu_total(self) -> float:
        return float(self.system.get("cpu_total") or 0.0)

    @property
    def ram_percent(self) -> float:
        return float(self.system.get("ram_percent") or 0.0)

    @property
    def swap_used(self) -> float:
        return float(self.system.get("swap_used") or 0.0)

    @property
    def load1(self) -> float:
        return float(self.system.get("load1") or 0.0)

    @property
    def ram_used(self) -> int:
        return int(self.system.get("ram_used") or 0)

    @property
    def top_procs(self) -> list[Proc]:
        return sorted(
            self.processes, key=lambda p: (p.cpu_percent, p.ram_kb), reverse=True
        )


def parse_gpu(raw: dict[str, Any]) -> Gpu:
    return Gpu(
        id=raw.get("id", ""),
        model=raw.get("model", ""),
        utilization=opt_float(raw.get("utilization")),
        temperature=opt_float(raw.get("temperature")),
        memory_used=raw.get("memory_used"),
        memory_total=raw.get("memory_total"),
        power_watts=opt_float(raw.get("power_watts")),
        clock_mhz=opt_float(raw.get("clock_mhz")),
    )


def parse_disk(raw: dict[str, Any]) -> Disk:
    return Disk(
        mount_point=raw.get("mount_point", ""),
        percent=float(raw.get("percent") or 0.0),
        used=int(raw.get("used") or 0),
        total=int(raw.get("total") or 0),
    )


def parse_proc(raw: dict[str, Any]) -> Proc:
    return Proc(
        pid=int(raw.get("pid") or 0),
        name=raw.get("name", ""),
        cpu_percent=float(raw.get("cpu_percent") or 0.0),
        ram_kb=int(raw.get("ram_kb") or 0),
    )


def opt_float(v: Any) -> Optional[float]:
    if v is None:
        return None
    try:
        return float(v)
    except (TypeError, ValueError):
        return None