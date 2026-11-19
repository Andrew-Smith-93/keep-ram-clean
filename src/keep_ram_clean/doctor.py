"""System memory health audit & Linux VM tuning advisor (`keep-ram-clean doctor`)."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional

from .config import Config
from .metrics import collect_snapshot, is_path_on_tmpfs, top_memory_processes


@dataclass
class AuditFinding:
    check: str
    status: str  # "ok", "info", "warn"
    detail: str
    recommendation: str = ""


def _read_sysctl(path: str) -> Optional[str]:
    p = Path(path)
    try:
        return p.read_text(encoding="utf-8").strip()
    except OSError:
        return None


def run_doctor(config: Optional[Config] = None) -> List[AuditFinding]:
    findings: List[AuditFinding] = []
    tmpfs_paths = config.tmpfs.paths if config else ["/tmp"]
    snap = collect_snapshot(tmpfs_paths=tmpfs_paths)

    # 1. PSI support
    if snap.psi.available:
        findings.append(
            AuditFinding(
                "Pressure Stall Information (PSI)",
                "ok",
                f"Enabled (memory some avg10={snap.psi.some_avg10:.2f}, full avg10={snap.psi.full_avg10:.2f})",
            )
        )
    else:
        findings.append(
            AuditFinding(
                "Pressure Stall Information (PSI)",
                "warn",
                "/proc/pressure/memory not available",
                "Boot with kernel parameter 'psi=1' on older kernels for accurate contention-based triggers.",
            )
        )

    # 2. cgroup v2 root memory.reclaim (Linux 6.1+)
    cg_reclaim = Path("/sys/fs/cgroup/memory.reclaim")
    if cg_reclaim.exists():
        findings.append(
            AuditFinding(
                "cgroup v2 memory.reclaim",
                "ok",
                "Supported (proactive cold-page reclamation available without cold-cache shock)",
            )
        )
    else:
        findings.append(
            AuditFinding(
                "cgroup v2 memory.reclaim",
                "info",
                "Not available (root /sys/fs/cgroup/memory.reclaim requires Linux 6.1+ with unified cgroup v2)",
                "Daemon will fall back to drop_caches_level when running as root.",
            )
        )

    # 3. MGLRU (Multi-Gen LRU)
    mglru = _read_sysctl("/sys/kernel/mm/lru_gen/enabled")
    if mglru is not None:
        try:
            enabled = int(mglru, 0) != 0
        except ValueError:
            enabled = mglru not in ("0", "0x0000")
        findings.append(
            AuditFinding(
                "Multi-Gen LRU (MGLRU)",
                "ok" if enabled else "info",
                f"Kernel value: {mglru}",
                "" if enabled else "Enabling MGLRU (CONFIG_LRU_GEN) significantly improves desktop responsiveness under memory pressure.",
            )
        )

    # 4. Swap / zram / zswap check
    has_zram = any(s.is_zram for s in snap.swaps)
    has_disk_swap = any(not s.is_zram for s in snap.swaps)
    zswap_enabled = _read_sysctl("/sys/module/zswap/parameters/enabled") == "Y"
    if has_zram:
        zram_mb = sum(s.size_mb for s in snap.swaps if s.is_zram)
        findings.append(
            AuditFinding(
                "Compressed Swap (zram)",
                "ok",
                f"Active ({zram_mb} MiB total zram); skip_zram=true will avoid wasteful zram flushing.",
            )
        )
    elif zswap_enabled:
        findings.append(
            AuditFinding(
                "Compressed Swap (zswap)",
                "ok",
                "zswap is enabled in kernel.",
            )
        )
    elif snap.memory.swap_total_mb == 0:
        findings.append(
            AuditFinding(
                "Swap Configuration",
                "info",
                "No swap configured.",
                "Consider enabling zram (e.g. zram-generator) to cushion RAM spikes without disk thrashing.",
            )
        )
    else:
        findings.append(
            AuditFinding(
                "Swap Configuration",
                "info",
                f"Disk/file swap active ({snap.memory.swap_total_mb} MiB total, no zram/zswap detected).",
                "Consider zram or zswap for faster, lower-latency swapping under load.",
            )
        )

    # 5. vm.swappiness
    swappiness_raw = _read_sysctl("/proc/sys/vm/swappiness")
    if swappiness_raw is not None:
        try:
            swappiness = int(swappiness_raw)
            if snap.memory.swap_total_mb == 0:
                findings.append(
                    AuditFinding("vm.swappiness", "ok", f"Currently {swappiness} (no swap active)")
                )
            elif has_zram and not has_disk_swap and swappiness < 60:
                findings.append(
                    AuditFinding(
                        "vm.swappiness",
                        "info",
                        f"Currently {swappiness} (zram-only swap active)",
                        "With zram-only swap, higher swappiness (100-180) is often recommended because zram is RAM-speed.",
                    )
                )
            elif has_zram and has_disk_swap:
                findings.append(
                    AuditFinding(
                        "vm.swappiness",
                        "ok",
                        f"Currently {swappiness} (mixed zram + disk swap active)",
                    )
                )
            elif not has_zram and not zswap_enabled and swappiness > 60:
                findings.append(
                    AuditFinding(
                        "vm.swappiness",
                        "warn",
                        f"Currently {swappiness} (disk swap only)",
                        "Consider lowering vm.swappiness to 10-30 for desktop workloads on disk swap.",
                    )
                )
            else:
                findings.append(AuditFinding("vm.swappiness", "ok", f"Currently {swappiness}"))
        except ValueError:
            pass

    # 6. vm.vfs_cache_pressure
    vfs_raw = _read_sysctl("/proc/sys/vm/vfs_cache_pressure")
    if vfs_raw is not None:
        try:
            vfs_val = int(vfs_raw)
            if vfs_val == 0:
                findings.append(
                    AuditFinding(
                        "vm.vfs_cache_pressure",
                        "warn",
                        "Currently 0 (kernel will never reclaim dentry/inode slab caches)",
                        "Set vm.vfs_cache_pressure to 50-100 to prevent unbounded slab growth.",
                    )
                )
            else:
                findings.append(
                    AuditFinding("vm.vfs_cache_pressure", "ok", f"Currently {vfs_val}")
                )
        except ValueError:
            findings.append(AuditFinding("vm.vfs_cache_pressure", "info", f"Currently {vfs_raw}"))

    # 7. Monitored tmpfs paths
    for raw_path in tmpfs_paths:
        if is_path_on_tmpfs(raw_path):
            findings.append(
                AuditFinding(
                    f"{raw_path} Filesystem",
                    "ok",
                    f"Mounted on tmpfs (RAM-backed); stale files in {raw_path} directly consume RAM and will be cleaned.",
                )
            )
        else:
            findings.append(
                AuditFinding(
                    f"{raw_path} Filesystem",
                    "info",
                    f"{raw_path} is on disk (not tmpfs).",
                    f"tmpfs cleanup will safely skip {raw_path} while require_tmpfs=true.",
                )
            )

    # 8. Top consumer note
    top = top_memory_processes(limit=3)
    if top:
        summary = ", ".join(f"{p.comm} ({p.rss_mb:.0f} MiB)" for p in top)
        findings.append(AuditFinding("Top Memory Consumers", "info", summary))

    return findings
