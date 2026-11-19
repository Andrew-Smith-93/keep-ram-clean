"""Zero-dependency Linux memory, PSI, swap, tmpfs, and process telemetry."""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Tuple

MAX_CMDLINE_BYTES = 4096


def _unescape_proc_path(raw: str) -> str:
    """Unescape standard octal escapes used in /proc/mounts and /proc/swaps."""
    return (
        raw.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
    )


@dataclass(frozen=True)
class MemoryInfo:
    total_mb: int
    available_mb: int
    used_mb: int
    buffers_mb: int
    cached_mb: int
    slab_reclaimable_mb: int
    dirty_mb: int
    swap_total_mb: int
    swap_free_mb: int
    swap_used_mb: int

    @property
    def used_percent(self) -> float:
        if self.total_mb <= 0:
            return 0.0
        return round((self.used_mb / self.total_mb) * 100.0, 2)

    @property
    def swap_used_percent(self) -> float:
        if self.swap_total_mb <= 0:
            return 0.0
        return round((self.swap_used_mb / self.swap_total_mb) * 100.0, 2)


@dataclass(frozen=True)
class PsiMemory:
    available: bool = False
    some_avg10: float = 0.0
    some_avg60: float = 0.0
    some_avg300: float = 0.0
    full_avg10: float = 0.0
    full_avg60: float = 0.0
    full_avg300: float = 0.0


@dataclass(frozen=True)
class SwapDevice:
    path: str
    swap_type: str
    size_mb: int
    used_mb: int
    priority: int

    @property
    def is_zram(self) -> bool:
        return "zram" in Path(self.path).name.lower()


@dataclass(frozen=True)
class TmpfsMount:
    mountpoint: str
    total_mb: int
    used_mb: int
    free_mb: int

    @property
    def used_percent(self) -> float:
        if self.total_mb <= 0:
            return 0.0
        return round((self.used_mb / self.total_mb) * 100.0, 2)


@dataclass(frozen=True)
class ProcessInfo:
    pid: int
    uid: int
    comm: str
    cmdline: str
    rss_mb: float
    starttime: Optional[int] = None


@dataclass(frozen=True)
class SystemSnapshot:
    memory: MemoryInfo
    psi: PsiMemory
    swaps: List[SwapDevice] = field(default_factory=list)
    tmpfs_mounts: List[TmpfsMount] = field(default_factory=list)
    load_avg_1m: float = 0.0
    load_avg_5m: float = 0.0
    load_avg_15m: float = 0.0
    cpu_count: int = 1

    @property
    def load_ratio(self) -> float:
        return round(self.load_avg_1m / max(self.cpu_count, 1), 2)

    def max_tmpfs_used_percent(self, paths: Optional[List[str]] = None) -> Tuple[float, Optional[str]]:
        norm_targets = [os.path.realpath(p) for p in paths] if paths else None
        best_pct = 0.0
        best_mount: Optional[str] = None
        for m in self.tmpfs_mounts:
            if norm_targets is not None:
                mp_prefix = m.mountpoint.rstrip("/") + "/"
                if not any(
                    t == m.mountpoint or t.startswith(mp_prefix) for t in norm_targets
                ):
                    continue
            if m.used_percent > best_pct:
                best_pct = m.used_percent
                best_mount = m.mountpoint
        return best_pct, best_mount


def parse_meminfo(text: str) -> MemoryInfo:
    values_kb: Dict[str, int] = {}
    for line in text.splitlines():
        if ":" not in line:
            continue
        key, rest = line.split(":", 1)
        parts = rest.strip().split()
        if parts:
            try:
                values_kb[key.strip()] = int(parts[0])
            except ValueError:
                pass

    def mb(key: str) -> int:
        return values_kb.get(key, 0) // 1024

    total = mb("MemTotal")
    if "MemAvailable" in values_kb:
        avail = mb("MemAvailable")
    elif "MemFree" in values_kb:
        avail = mb("MemFree") + mb("Buffers") + mb("Cached")
    else:
        avail = 0
    used = max(0, total - avail)
    stotal = mb("SwapTotal")
    sfree = mb("SwapFree")
    sused = max(0, stotal - sfree)

    return MemoryInfo(
        total_mb=total,
        available_mb=avail,
        used_mb=used,
        buffers_mb=mb("Buffers"),
        cached_mb=mb("Cached"),
        slab_reclaimable_mb=mb("SReclaimable"),
        dirty_mb=mb("Dirty"),
        swap_total_mb=stotal,
        swap_free_mb=sfree,
        swap_used_mb=sused,
    )


def read_meminfo() -> MemoryInfo:
    return parse_meminfo(Path("/proc/meminfo").read_text(encoding="utf-8"))


def parse_psi(text: str) -> PsiMemory:
    vals: Dict[str, Dict[str, float]] = {}
    for line in text.splitlines():
        parts = line.strip().split()
        if not parts:
            continue
        kind = parts[0]
        metrics: Dict[str, float] = {}
        for token in parts[1:]:
            if "=" in token:
                k, v = token.split("=", 1)
                try:
                    metrics[k] = float(v)
                except ValueError:
                    pass
        vals[kind] = metrics

    some = vals.get("some", {})
    full = vals.get("full", {})
    return PsiMemory(
        available=bool(some or full),
        some_avg10=some.get("avg10", 0.0),
        some_avg60=some.get("avg60", 0.0),
        some_avg300=some.get("avg300", 0.0),
        full_avg10=full.get("avg10", 0.0),
        full_avg60=full.get("avg60", 0.0),
        full_avg300=full.get("avg300", 0.0),
    )


def read_psi() -> PsiMemory:
    try:
        return parse_psi(Path("/proc/pressure/memory").read_text(encoding="utf-8"))
    except OSError:
        return PsiMemory(available=False)


def parse_swaps(text: str) -> List[SwapDevice]:
    devices: List[SwapDevice] = []
    for line in text.splitlines()[1:]:
        parts = line.split()
        if len(parts) < 5:
            continue
        dev_path = _unescape_proc_path(parts[0])
        if not dev_path.startswith("/"):
            continue
        try:
            devices.append(
                SwapDevice(
                    path=dev_path,
                    swap_type=parts[1],
                    size_mb=int(parts[2]) // 1024,
                    used_mb=int(parts[3]) // 1024,
                    priority=int(parts[4]),
                )
            )
        except ValueError:
            continue
    return devices


def read_swaps() -> List[SwapDevice]:
    try:
        return parse_swaps(Path("/proc/swaps").read_text(encoding="utf-8"))
    except OSError:
        return []


def parse_tmpfs_mountpoints(mounts_text: str) -> List[str]:
    points: List[str] = []
    for line in mounts_text.splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[2] == "tmpfs":
            points.append(_unescape_proc_path(parts[1]))
    return points


def _covering_tmpfs_mountpoint(target: str, mounts_text: str) -> Optional[str]:
    """Return the deepest active mountpoint covering `target` if it is tmpfs, else None."""
    best_mp = ""
    best_fstype = ""
    for line in mounts_text.splitlines():
        parts = line.split()
        if len(parts) < 3:
            continue
        mp = _unescape_proc_path(parts[1])
        fstype = parts[2]
        if target == mp or target.startswith(mp.rstrip("/") + "/"):
            if len(mp) >= len(best_mp):
                best_mp = mp
                best_fstype = fstype
    return best_mp if best_fstype == "tmpfs" else None


def is_mount_tmpfs(target: str, mounts_text: str) -> bool:
    """Return True if the active (deepest, last-mounted) filesystem covering `target` is tmpfs."""
    return _covering_tmpfs_mountpoint(target, mounts_text) is not None


def is_path_on_tmpfs(path: str) -> bool:
    """Return True if the deepest active mountpoint covering `path` is tmpfs."""
    target = os.path.realpath(path)
    try:
        mounts_text = Path("/proc/mounts").read_text(encoding="utf-8")
    except OSError:
        return False
    return is_mount_tmpfs(target, mounts_text)


def read_tmpfs_mounts(filter_paths: Optional[List[str]] = None) -> List[TmpfsMount]:
    try:
        mounts_text = Path("/proc/mounts").read_text(encoding="utf-8")
    except OSError:
        return []

    if filter_paths is not None:
        mountpoints = []
        for p in filter_paths:
            mp = _covering_tmpfs_mountpoint(os.path.realpath(p), mounts_text)
            if mp:
                mountpoints.append(mp)
    else:
        mountpoints = parse_tmpfs_mountpoints(mounts_text)

    results: List[TmpfsMount] = []
    seen = set()
    for mp in mountpoints:
        if mp in seen:
            continue
        seen.add(mp)
        try:
            st = os.statvfs(mp)
            total = (st.f_blocks * st.f_frsize) // (1024 * 1024)
            free = (st.f_bavail * st.f_frsize) // (1024 * 1024)
            used = max(0, total - free)
            results.append(TmpfsMount(mountpoint=mp, total_mb=total, used_mb=used, free_mb=free))
        except OSError:
            continue
    return results


def read_load_avg() -> Tuple[float, float, float]:
    try:
        return os.getloadavg()
    except OSError:
        return (0.0, 0.0, 0.0)


def read_proc_starttime(pid: int) -> Optional[int]:
    """Read process start time (field 22 of /proc/<pid>/stat) to defeat PID reuse."""
    try:
        raw = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return None
    rparen = raw.rfind(")")
    if rparen == -1:
        return None
    fields_after_comm = raw[rparen + 2 :].split()
    if len(fields_after_comm) <= 19:
        return None
    try:
        return int(fields_after_comm[19])
    except ValueError:
        return None


def read_proc_cmdline(pid: int, max_bytes: int = MAX_CMDLINE_BYTES) -> str:
    """Read bounded /proc/<pid>/cmdline string."""
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as fh:
            raw_cmd = fh.read(max_bytes)
        if raw_cmd:
            return raw_cmd.replace(b"\x00", b" ").decode("utf-8", errors="ignore").strip()
    except OSError:
        pass
    return ""


def list_processes(
    min_rss_mb: float = 0.0,
    uid_filter: Optional[int] = None,
    require_starttime: bool = False,
) -> List[ProcessInfo]:
    """Enumerate running processes with RSS memory usage from /proc."""
    procs: List[ProcessInfo] = []
    proc_root = Path("/proc")
    try:
        entries = list(proc_root.iterdir())
    except OSError:
        return procs

    for entry in entries:
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        try:
            status_text = (entry / "status").read_text(encoding="utf-8", errors="ignore")
        except OSError:
            continue

        comm = ""
        rss_kb = 0
        uid = -1
        for line in status_text.splitlines():
            if line.startswith("Name:"):
                comm = line.split(":", 1)[1].strip()
            elif line.startswith("VmRSS:"):
                parts = line.split(":", 1)[1].strip().split()
                if parts:
                    try:
                        rss_kb = int(parts[0])
                    except ValueError:
                        pass
            elif line.startswith("Uid:"):
                parts = line.split(":", 1)[1].strip().split()
                if parts:
                    try:
                        uid = int(parts[0])
                    except ValueError:
                        pass

        rss_mb = round(rss_kb / 1024.0, 1)
        if rss_mb < min_rss_mb:
            continue
        if uid_filter is not None and uid != uid_filter:
            continue

        starttime = read_proc_starttime(pid)
        if require_starttime and starttime is None:
            continue
        cmdline = read_proc_cmdline(pid) or comm

        procs.append(
            ProcessInfo(
                pid=pid,
                uid=uid,
                comm=comm,
                cmdline=cmdline,
                rss_mb=rss_mb,
                starttime=starttime,
            )
        )
    return procs


def top_memory_processes(limit: int = 5) -> List[ProcessInfo]:
    if limit <= 0:
        return []
    procs = list_processes()
    procs.sort(key=lambda p: p.rss_mb, reverse=True)
    return procs[:limit]


def collect_snapshot(
    tmpfs_paths: Optional[List[str]] = None,
    include_swaps: bool = True,
    include_tmpfs: bool = True,
    include_psi: bool = True,
    include_load: bool = True,
) -> SystemSnapshot:
    l1, l5, l15 = read_load_avg() if include_load else (0.0, 0.0, 0.0)
    return SystemSnapshot(
        memory=read_meminfo(),
        psi=read_psi() if include_psi else PsiMemory(available=False),
        swaps=read_swaps() if include_swaps else [],
        tmpfs_mounts=read_tmpfs_mounts(filter_paths=tmpfs_paths) if include_tmpfs else [],
        load_avg_1m=round(l1, 2),
        load_avg_5m=round(l5, 2),
        load_avg_15m=round(l15, 2),
        cpu_count=os.cpu_count() or 1,
    )
