"""Modular, dry-run-safe, race-hardened Linux memory and cache cleanup actions."""

from __future__ import annotations

import errno
import fnmatch
import logging
import os
import pwd
import re
import select
import shutil
import signal
import stat
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, List, Optional, Set, Tuple

from .config import Config
from .metrics import (
    SystemSnapshot,
    collect_snapshot,
    is_path_on_tmpfs,
    list_processes,
    read_meminfo,
    read_proc_starttime,
    read_swaps,
)

logger = logging.getLogger("keep_ram_clean.cleaners")

TRUSTED_SBIN_PATH = "/usr/sbin:/sbin:/usr/bin:/bin"
SWAP_CMD_TIMEOUT_SECONDS = 120
MAX_WALK_DEPTH = 64


@dataclass
class StepResult:
    name: str
    status: str  # "ok", "skipped", "dry-run", "error"
    summary: str
    bytes_freed: int = 0
    details: List[str] = field(default_factory=list)


@dataclass
class CleanupReport:
    started_at: float
    finished_at: float
    dry_run: bool
    reasons: List[str]
    pre_available_mb: int
    post_available_mb: int
    pre_swap_used_mb: int
    post_swap_used_mb: int
    steps: List[StepResult] = field(default_factory=list)
    elapsed_seconds: Optional[float] = None
    critical: bool = False

    @property
    def ram_freed_mb(self) -> int:
        return self.post_available_mb - self.pre_available_mb

    @property
    def swap_freed_mb(self) -> int:
        return self.pre_swap_used_mb - self.post_swap_used_mb

    @property
    def duration_seconds(self) -> float:
        if self.elapsed_seconds is not None:
            return round(max(0.0, self.elapsed_seconds), 2)
        return round(max(0.0, self.finished_at - self.started_at), 2)

    @property
    def has_errors(self) -> bool:
        return any(s.status == "error" for s in self.steps)


def _get_open_paths_under(roots: Iterable[str]) -> Set[str]:
    """Scan /proc/*/fd, /proc/*/cwd, and /proc/*/exe for paths currently held open under `roots`."""
    norm_roots = [os.path.realpath(r).rstrip("/") + "/" for r in roots]
    open_paths: Set[str] = set()
    proc_root = Path("/proc")
    try:
        entries = list(proc_root.iterdir())
    except OSError:
        return open_paths

    warned_eacces = False
    is_root = os.geteuid() == 0

    for entry in entries:
        if not entry.name.isdigit():
            continue
        for link_name in ("cwd", "exe"):
            try:
                target = os.readlink(entry / link_name)
                if any(target.startswith(r) or target + "/" == r for r in norm_roots):
                    open_paths.add(target)
            except PermissionError:
                if is_root and not warned_eacces:
                    logger.warning(
                        "Permission denied reading /proc/<pid>/%s as root "
                        "(ensure CAP_SYS_PTRACE is permitted for skip_open_files).",
                        link_name,
                    )
                    warned_eacces = True
            except OSError:
                pass

        fd_dir = entry / "fd"
        try:
            fds = list(fd_dir.iterdir())
        except PermissionError:
            if is_root and not warned_eacces:
                logger.warning(
                    "Permission denied reading /proc/<pid>/fd as root "
                    "(ensure CAP_SYS_PTRACE is permitted for skip_open_files)."
                )
                warned_eacces = True
            continue
        except OSError:
            continue
        for fd in fds:
            try:
                target = os.readlink(fd)
                if any(target.startswith(r) for r in norm_roots):
                    open_paths.add(target)
            except OSError:
                continue
    return open_paths


def _path_has_open_handle(item_path: str, open_paths: Set[str]) -> bool:
    norm = os.path.normpath(item_path)
    prefix = norm.rstrip("/") + "/"
    for op in open_paths:
        if op == norm or op.startswith(prefix):
            return True
    return False


def _dir_size_and_max_mtime_fd(
    dir_fd: int, root_dev: int, depth: int = 0
) -> Tuple[int, float]:
    """Compute directory size and newest mtime within `root_dev`, bounded by MAX_WALK_DEPTH."""
    if depth >= MAX_WALK_DEPTH:
        return 0, 0.0
    total = 0
    max_mtime = 0.0
    try:
        with os.scandir(dir_fd) as it:
            entries = list(it)
    except OSError:
        return 0, 0.0
    for entry in entries:
        try:
            st = os.lstat(entry.name, dir_fd=dir_fd)
        except OSError:
            continue
        if st.st_mtime > max_mtime:
            max_mtime = st.st_mtime
        if stat.S_ISLNK(st.st_mode):
            total += st.st_size
            continue
        if stat.S_ISDIR(st.st_mode):
            if st.st_dev != root_dev:
                continue
            try:
                sub_fd = os.open(
                    entry.name,
                    os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                    dir_fd=dir_fd,
                )
            except OSError:
                continue
            try:
                sub_st = os.fstat(sub_fd)
                if sub_st.st_dev == root_dev and sub_st.st_ino == st.st_ino:
                    sub_sz, sub_mt = _dir_size_and_max_mtime_fd(
                        sub_fd, root_dev, depth + 1
                    )
                    total += sub_sz
                    if sub_mt > max_mtime:
                        max_mtime = sub_mt
            finally:
                os.close(sub_fd)
        else:
            total += st.st_size
    return total, max_mtime


def _rmtree_fd(
    parent_fd: int,
    name: str,
    root_dev: int,
    expected_ino: int,
    depth: int = 0,
) -> None:
    """Race-safe, depth-bounded recursive directory removal using dir_fd + O_NOFOLLOW."""
    if depth >= MAX_WALK_DEPTH:
        raise OSError(errno.ELOOP, f"Maximum directory depth ({MAX_WALK_DEPTH}) exceeded: {name}")
    dir_fd = os.open(
        name,
        os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
        dir_fd=parent_fd,
    )
    try:
        dir_st = os.fstat(dir_fd)
        if dir_st.st_dev != root_dev or dir_st.st_ino != expected_ino:
            raise PermissionError(f"Refusing to traverse replaced/cross-device dir: {name}")
        with os.scandir(dir_fd) as it:
            entries = list(it)
        for entry in entries:
            try:
                st = os.lstat(entry.name, dir_fd=dir_fd)
            except OSError:
                continue
            if stat.S_ISDIR(st.st_mode) and not stat.S_ISLNK(st.st_mode):
                if st.st_dev != root_dev:
                    continue
                _rmtree_fd(dir_fd, entry.name, root_dev, st.st_ino, depth + 1)
            else:
                os.unlink(entry.name, dir_fd=dir_fd)
    finally:
        os.close(dir_fd)
    os.rmdir(name, dir_fd=parent_fd)


def _open_subpath_nofollow(
    base_dir: Path,
    rel_parts: Tuple[str, ...],
    expected_uid: Optional[int] = None,
) -> Optional[Tuple[int, int]]:
    """Open `base_dir / rel_parts` component-by-component with O_NOFOLLOW and optional UID check."""
    # Refuse empty rel_parts so we never return an FD pointing to base_dir ($HOME) itself
    if not rel_parts:
        return None
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    try:
        cur_fd = os.open(str(base_dir), flags)
    except OSError:
        return None

    try:
        base_st = os.fstat(cur_fd)
        if expected_uid is not None and base_st.st_uid != expected_uid:
            os.close(cur_fd)
            return None
        base_dev = base_st.st_dev

        for part in rel_parts:
            if not part or part in (".", ".."):
                os.close(cur_fd)
                return None
            try:
                nxt_fd = os.open(part, flags, dir_fd=cur_fd)
            except OSError:
                os.close(cur_fd)
                return None
            os.close(cur_fd)
            cur_fd = nxt_fd
            st = os.fstat(cur_fd)
            if st.st_dev != base_dev:
                os.close(cur_fd)
                return None
            if expected_uid is not None and st.st_uid != expected_uid:
                os.close(cur_fd)
                return None
        return cur_fd, base_dev
    except Exception:
        os.close(cur_fd)
        raise


def clean_tmpfs(config: Config, dry_run: bool = False) -> StepResult:
    """Safely remove stale temporary files/directories on tmpfs mounts."""
    cfg = config.tmpfs
    if not cfg.enabled:
        return StepResult("tmpfs", "skipped", "Disabled in configuration")

    eligible_roots: List[Path] = []
    skipped_reasons: List[str] = []
    for raw_path in cfg.paths:
        p = Path(raw_path)
        if not p.is_dir():
            skipped_reasons.append(f"{raw_path}: does not exist")
            continue
        if cfg.require_tmpfs and not is_path_on_tmpfs(str(p)):
            skipped_reasons.append(f"{raw_path}: not a tmpfs mount (require_tmpfs=true)")
            continue
        eligible_roots.append(p)

    if not eligible_roots:
        reason = "; ".join(skipped_reasons) if skipped_reasons else "No eligible tmpfs paths"
        return StepResult("tmpfs", "skipped", reason)

    open_paths: Optional[Set[str]] = None
    now = time.time()
    min_age_secs = cfg.min_age_minutes * 60
    euid = os.geteuid()
    removed_count = 0
    bytes_freed = 0
    details: List[str] = []

    for root in eligible_roots:
        try:
            root_fd = os.open(str(root), os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        except OSError as exc:
            details.append(f"Cannot open {root}: {exc}")
            continue

        root_real = os.path.realpath(str(root))
        try:
            try:
                root_dev = os.fstat(root_fd).st_dev
                with os.scandir(root_fd) as it:
                    entries = list(it)
            except OSError as exc:
                details.append(f"Cannot scan {root}: {exc}")
                continue

            for entry in entries:
                name = entry.name
                if any(fnmatch.fnmatch(name, ex) for ex in cfg.exclude_patterns):
                    continue
                if not any(fnmatch.fnmatch(name, pat) for pat in cfg.patterns):
                    continue

                try:
                    st = os.lstat(name, dir_fd=root_fd)
                except OSError:
                    continue

                if cfg.owner_only and euid != 0 and st.st_uid != euid:
                    continue

                if (now - st.st_mtime) < min_age_secs:
                    continue

                # Defer expensive /proc/*/fd scan until at least one candidate matches
                if cfg.skip_open_files:
                    if open_paths is None:
                        open_paths = _get_open_paths_under(str(p) for p in eligible_roots)
                    canonical_item = os.path.join(root_real, name)
                    if _path_has_open_handle(canonical_item, open_paths):
                        logger.debug("Skipping open path: %s", canonical_item)
                        continue

                item_path = os.path.join(str(root), name)
                is_dir = stat.S_ISDIR(st.st_mode) and not stat.S_ISLNK(st.st_mode)
                if is_dir:
                    if st.st_dev != root_dev:
                        continue
                    try:
                        sub_fd = os.open(
                            name,
                            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                            dir_fd=root_fd,
                        )
                    except OSError:
                        continue
                    try:
                        sub_st = os.fstat(sub_fd)
                        if sub_st.st_dev != root_dev or sub_st.st_ino != st.st_ino:
                            continue
                        size, max_child_mtime = _dir_size_and_max_mtime_fd(sub_fd, root_dev)
                    finally:
                        os.close(sub_fd)
                    # Skip directories with recently modified nested files (active build trees)
                    if max_child_mtime > 0 and (now - max_child_mtime) < min_age_secs:
                        continue
                else:
                    size = st.st_size

                if dry_run:
                    removed_count += 1
                    bytes_freed += size
                    details.append(
                        f"[dry-run] Would remove {item_path} ({size / (1024*1024):.1f} MiB)"
                    )
                    continue

                try:
                    if is_dir:
                        _rmtree_fd(root_fd, name, root_dev, st.st_ino)
                    else:
                        os.unlink(name, dir_fd=root_fd)
                    removed_count += 1
                    bytes_freed += size
                    details.append(f"Removed {item_path} ({size / (1024*1024):.1f} MiB)")
                except OSError as exc:
                    details.append(f"Failed to remove {item_path}: {exc}")
        finally:
            os.close(root_fd)

    mb = bytes_freed / (1024 * 1024)
    status = "dry-run" if dry_run else "ok"
    if removed_count == 0:
        return StepResult("tmpfs", status, "No stale tmpfs items matched", 0, details)
    verb = "Would remove" if dry_run else "Removed"
    return StepResult(
        "tmpfs",
        status,
        f"{verb} {removed_count} stale item(s) ({mb:.1f} MiB)",
        bytes_freed,
        details,
    )


def _signal_fallback_with_starttime(
    pid: int,
    expected_starttime: int,
    sig: signal.Signals,
    grace_seconds: int = 0,
    escalate_sigkill: bool = False,
) -> Tuple[str, Optional[str]]:
    """Fallback signalling when pidfd_open is unavailable (ENOSYS), verified via starttime."""
    if read_proc_starttime(pid) != expected_starttime:
        return "stale", "Process exited or PID was recycled before signalling"
    os.kill(pid, sig)
    if grace_seconds <= 0 and not escalate_sigkill:
        return "sent", None

    deadline = time.monotonic() + max(0, grace_seconds)
    while time.monotonic() < deadline:
        if read_proc_starttime(pid) != expected_starttime:
            return "exited", None
        time.sleep(0.25)

    if read_proc_starttime(pid) != expected_starttime:
        return "exited", None
    if escalate_sigkill:
        os.kill(pid, signal.SIGKILL)
        return "escalated", None
    return "still_running", None


def _signal_via_pidfd(
    pid: int,
    expected_starttime: Optional[int],
    sig: signal.Signals,
    grace_seconds: int = 0,
    escalate_sigkill: bool = False,
) -> Tuple[str, Optional[str]]:
    """Pin process via Linux pidfd to eliminate PID reuse races during signalling/waiting."""
    if expected_starttime is None:
        return "stale", "Missing process starttime; refusing to signal"

    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        return _signal_fallback_with_starttime(
            pid, expected_starttime, sig, grace_seconds, escalate_sigkill
        )

    try:
        pidfd = os.pidfd_open(pid, 0)
    except OSError as exc:
        if exc.errno == errno.ENOSYS:
            return _signal_fallback_with_starttime(
                pid, expected_starttime, sig, grace_seconds, escalate_sigkill
            )
        raise

    try:
        if read_proc_starttime(pid) != expected_starttime:
            return "stale", "Process exited or PID was recycled before signalling"

        signal.pidfd_send_signal(pidfd, sig)
        if grace_seconds <= 0 and not escalate_sigkill:
            return "sent", None

        poller = select.poll()
        poller.register(pidfd, select.POLLIN)
        events = poller.poll(max(0, grace_seconds) * 1000)
        if events:
            return "exited", None
        if escalate_sigkill:
            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
            return "escalated", None
        return "still_running", None
    finally:
        os.close(pidfd)


def run_process_watchdog(config: Config, dry_run: bool = False) -> StepResult:
    """Inspect processes against user-defined RSS watchdog rules."""
    cfg = config.watchdog
    if not cfg.enabled or not cfg.rules:
        return StepResult("watchdog", "skipped", "Disabled or no rules configured")

    euid = os.geteuid()
    min_rule_rss = min(r.max_rss_mb for r in cfg.rules)
    procs = list_processes(
        min_rss_mb=float(min_rule_rss),
        uid_filter=None if euid == 0 else euid,
        require_starttime=True,
    )
    my_pid = os.getpid()
    actions_taken = 0
    details: List[str] = []
    compiled_rules = [(rule, re.compile(rule.pattern)) for rule in cfg.rules]

    for rule, regex in compiled_rules:
        for proc in procs:
            if proc.pid <= 1 or proc.pid == my_pid:
                continue
            if proc.rss_mb < rule.max_rss_mb:
                continue
            if not (regex.search(proc.comm) or regex.search(proc.cmdline)):
                continue

            msg = (
                f"Rule '{rule.name}' matched PID {proc.pid} ({proc.comm}, "
                f"RSS {proc.rss_mb:.1f} MiB >= {rule.max_rss_mb} MiB)"
            )
            if rule.action == "notify":
                details.append(f"ALERT: {msg}")
                actions_taken += 1
                continue

            if dry_run:
                details.append(f"[dry-run] Would apply '{rule.action}' to {msg}")
                actions_taken += 1
                continue

            try:
                if rule.action == "sigkill":
                    outcome, _ = _signal_via_pidfd(proc.pid, proc.starttime, signal.SIGKILL)
                    if outcome != "stale":
                        details.append(f"Sent SIGKILL: {msg}")
                        actions_taken += 1
                elif rule.action == "sigterm":
                    outcome, _ = _signal_via_pidfd(proc.pid, proc.starttime, signal.SIGTERM)
                    if outcome != "stale":
                        details.append(f"Sent SIGTERM: {msg}")
                        actions_taken += 1
                elif rule.action == "graceful":
                    outcome, _ = _signal_via_pidfd(
                        proc.pid,
                        proc.starttime,
                        signal.SIGTERM,
                        grace_seconds=rule.grace_seconds,
                        escalate_sigkill=rule.allow_sigkill,
                    )
                    if outcome == "exited":
                        details.append(f"Gracefully stopped (SIGTERM): {msg}")
                        actions_taken += 1
                    elif outcome == "escalated":
                        details.append(
                            f"Escalated to SIGKILL after {rule.grace_seconds}s: {msg}"
                        )
                        actions_taken += 1
                    elif outcome in ("still_running", "sent"):
                        details.append(
                            f"Sent SIGTERM (still running after {rule.grace_seconds}s; "
                            f"allow_sigkill=false): {msg}"
                        )
                        actions_taken += 1
            except ProcessLookupError:
                details.append(f"Process already exited: PID {proc.pid}")
            except PermissionError:
                details.append(f"Permission denied signalling PID {proc.pid} ({proc.comm})")
            except OSError as exc:
                details.append(f"Failed signalling PID {proc.pid} ({proc.comm}): {exc}")

    status = "dry-run" if dry_run else "ok"
    if actions_taken == 0:
        return StepResult("watchdog", status, "No runaway processes exceeded thresholds")
    return StepResult(
        "watchdog",
        status,
        f"Handled {actions_taken} process rule match(es)",
        0,
        details,
    )


def _resolve_target_users(target_users: List[str]) -> List[Tuple[int, Path]]:
    """Return list of (uid, home_path) tuples for user cache cleanup."""
    targets: List[Tuple[int, Path]] = []
    if target_users:
        for username in target_users:
            try:
                pw = pwd.getpwnam(username)
                targets.append((pw.pw_uid, Path(pw.pw_dir)))
            except KeyError:
                logger.warning("Unknown user in user_caches.target_users: %s", username)
        return targets

    if os.geteuid() == 0:
        sudo_user = os.environ.get("SUDO_USER")
        if sudo_user:
            try:
                pw = pwd.getpwnam(sudo_user)
                return [(pw.pw_uid, Path(pw.pw_dir))]
            except KeyError:
                pass
        return []
    return [(os.geteuid(), Path.home())]


def _prune_old_files_fd(
    dir_fd: int,
    cutoff_ts: float,
    dry_run: bool,
    root_dev: int,
    expected_uid: Optional[int],
    depth: int = 0,
) -> Tuple[int, int]:
    """Race-safe, symlink-safe, depth-bounded recursive pruning of old regular files."""
    if depth >= MAX_WALK_DEPTH:
        return 0, 0
    count = 0
    freed = 0
    try:
        with os.scandir(dir_fd) as it:
            entries = list(it)
    except OSError:
        return 0, 0

    for entry in entries:
        try:
            st = os.lstat(entry.name, dir_fd=dir_fd)
        except OSError:
            continue
        if stat.S_ISLNK(st.st_mode):
            continue
        if expected_uid is not None and st.st_uid != expected_uid:
            continue
        if stat.S_ISDIR(st.st_mode):
            if st.st_dev != root_dev:
                continue
            try:
                sub_fd = os.open(
                    entry.name,
                    os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                    dir_fd=dir_fd,
                )
            except OSError:
                continue
            try:
                sub_st = os.fstat(sub_fd)
                if sub_st.st_dev == root_dev and sub_st.st_ino == st.st_ino:
                    c, b = _prune_old_files_fd(
                        sub_fd, cutoff_ts, dry_run, root_dev, expected_uid, depth + 1
                    )
                    count += c
                    freed += b
            finally:
                os.close(sub_fd)
        elif stat.S_ISREG(st.st_mode):
            if st.st_atime < cutoff_ts and st.st_mtime < cutoff_ts:
                try:
                    if not dry_run:
                        os.unlink(entry.name, dir_fd=dir_fd)
                    count += 1
                    freed += st.st_size
                except OSError:
                    continue
    return count, freed


def clean_user_caches(config: Config, dry_run: bool = False) -> StepResult:
    """Clean optional user-level disk caches safely without following symlinks."""
    cfg = config.user_caches
    if not cfg.enabled:
        return StepResult("user_caches", "skipped", "Disabled in configuration")

    targets = _resolve_target_users(cfg.target_users)
    if not targets:
        return StepResult(
            "user_caches",
            "skipped",
            "Running as root with no target_users configured",
        )

    now = time.time()
    is_root = os.geteuid() == 0
    removed_files = 0
    bytes_freed = 0
    details: List[str] = []

    for uid, home in targets:
        uid_guard: Optional[int] = uid if is_root else None

        if cfg.clean_thumbnails:
            opened = _open_subpath_nofollow(home, (".cache", "thumbnails"), uid_guard)
            if opened is not None:
                dfd, dev = opened
                try:
                    cutoff = now - (cfg.thumbnail_max_age_days * 86400)
                    c, b = _prune_old_files_fd(dfd, cutoff, dry_run, dev, uid_guard)
                    removed_files += c
                    bytes_freed += b
                    if c:
                        details.append(
                            f"{'[dry-run] ' if dry_run else ''}Pruned {c} thumbnail(s) "
                            f"in {home / '.cache' / 'thumbnails'} ({b / (1024*1024):.1f} MiB)"
                        )
                finally:
                    os.close(dfd)

        if cfg.clean_pip_cache:
            opened_cache = _open_subpath_nofollow(home, (".cache",), uid_guard)
            if opened_cache is not None:
                cache_fd, dev = opened_cache
                try:
                    pip_st = os.lstat("pip", dir_fd=cache_fd)
                    if (
                        stat.S_ISDIR(pip_st.st_mode)
                        and not stat.S_ISLNK(pip_st.st_mode)
                        and pip_st.st_dev == dev
                        and (uid_guard is None or pip_st.st_uid == uid_guard)
                    ):
                        pip_fd = os.open(
                            "pip",
                            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                            dir_fd=cache_fd,
                        )
                        try:
                            size, _ = _dir_size_and_max_mtime_fd(pip_fd, dev)
                        finally:
                            os.close(pip_fd)
                        if size > 0:
                            if dry_run:
                                bytes_freed += size
                                details.append(
                                    f"[dry-run] Would purge pip cache {home / '.cache' / 'pip'} "
                                    f"({size / (1024*1024):.1f} MiB)"
                                )
                            else:
                                _rmtree_fd(cache_fd, "pip", dev, pip_st.st_ino)
                                bytes_freed += size
                                details.append(
                                    f"Purged pip cache {home / '.cache' / 'pip'} "
                                    f"({size / (1024*1024):.1f} MiB)"
                                )
                except OSError:
                    pass
                finally:
                    os.close(cache_fd)

        for extra in cfg.extra_cache_dirs:
            if extra.startswith("~/"):
                rel_parts = tuple(p for p in Path(extra[2:]).parts if p not in ("", "."))
                display = home / extra[2:]
            elif not os.path.isabs(extra):
                rel_parts = tuple(p for p in Path(extra).parts if p not in ("", "."))
                display = home / extra
            else:
                extra_p = Path(extra)
                try:
                    rel = extra_p.relative_to(home)
                except ValueError:
                    logger.warning(
                        "Skipping extra_cache_dir outside target home %s: %s", home, extra
                    )
                    continue
                rel_parts = tuple(p for p in rel.parts if p not in ("", "."))
                display = extra_p

            if not rel_parts:
                logger.warning("Skipping unsafe empty/home extra_cache_dir: %s", extra)
                continue

            opened = _open_subpath_nofollow(home, rel_parts, uid_guard)
            if opened is not None:
                dfd, dev = opened
                try:
                    cutoff = now - (cfg.extra_cache_max_age_days * 86400)
                    c, b = _prune_old_files_fd(dfd, cutoff, dry_run, dev, uid_guard)
                    removed_files += c
                    bytes_freed += b
                    if c:
                        details.append(
                            f"{'[dry-run] ' if dry_run else ''}Pruned {c} file(s) "
                            f"in {display} ({b / (1024*1024):.1f} MiB)"
                        )
                finally:
                    os.close(dfd)

    mb = bytes_freed / (1024 * 1024)
    status = "dry-run" if dry_run else "ok"
    if bytes_freed == 0 and removed_files == 0:
        return StepResult("user_caches", status, "User caches already clean", 0, details)
    verb = "Would free" if dry_run else "Freed"
    return StepResult(
        "user_caches",
        status,
        f"{verb} {mb:.1f} MiB from user caches",
        bytes_freed,
        details,
    )


def reclaim_kernel_memory(
    config: Config,
    dry_run: bool = False,
    critical: bool = False,
) -> StepResult:
    """Reclaim kernel memory via cgroup v2 memory.reclaim, drop_caches, and compaction."""
    cfg = config.kernel
    if not cfg.enabled:
        return StepResult("kernel", "skipped", "Disabled in configuration")

    if os.geteuid() != 0 and not dry_run:
        return StepResult(
            "kernel",
            "skipped",
            "Requires root privileges (run via sudo or system systemd service)",
        )

    details: List[str] = []
    actions = 0
    errors = 0

    cgroup_reclaim = Path("/sys/fs/cgroup/memory.reclaim")
    if cfg.use_cgroup_reclaim and cfg.cgroup_reclaim_mb > 0 and cgroup_reclaim.exists():
        target_bytes = cfg.cgroup_reclaim_mb * 1024 * 1024
        if dry_run:
            details.append(
                f"[dry-run] Would write {cfg.cgroup_reclaim_mb}M to {cgroup_reclaim}"
            )
            actions += 1
        else:
            try:
                cgroup_reclaim.write_text(f"{target_bytes}\n", encoding="utf-8")
                details.append(
                    f"Proactively reclaimed up to {cfg.cgroup_reclaim_mb} MiB via cgroup v2 memory.reclaim"
                )
                actions += 1
            except OSError as exc:
                if exc.errno == errno.EAGAIN:
                    details.append(
                        "cgroup v2 memory.reclaim reclaimed available cold pages (EAGAIN)"
                    )
                    actions += 1
                else:
                    details.append(f"cgroup v2 memory.reclaim failed: {exc}")
                    errors += 1

    should_drop = cfg.drop_caches_level > 0 and (
        not cfg.drop_caches_on_critical_only or critical
    )
    if should_drop:
        drop_path = Path("/proc/sys/vm/drop_caches")
        if dry_run:
            details.append(
                f"[dry-run] Would sync and write '{cfg.drop_caches_level}' to {drop_path}"
            )
            actions += 1
        else:
            try:
                os.sync()
                drop_path.write_text(f"{cfg.drop_caches_level}\n", encoding="utf-8")
                details.append(f"Flushed kernel caches (drop_caches={cfg.drop_caches_level})")
                actions += 1
            except OSError as exc:
                details.append(f"drop_caches failed: {exc}")
                errors += 1

    compact_path = Path("/proc/sys/vm/compact_memory")
    if cfg.compact_memory and compact_path.exists():
        if dry_run:
            details.append(f"[dry-run] Would write '1' to {compact_path}")
            actions += 1
        else:
            try:
                compact_path.write_text("1\n", encoding="utf-8")
                details.append("Triggered kernel memory compaction")
                actions += 1
            except OSError as exc:
                details.append(f"Memory compaction failed: {exc}")
                errors += 1

    if errors > 0 and actions == 0:
        return StepResult("kernel", "error", f"Kernel reclaim failed ({errors} error(s))", 0, details)
    status = "dry-run" if dry_run else ("error" if errors > 0 else "ok")
    if actions == 0:
        return StepResult("kernel", "skipped", "No kernel reclaim actions enabled/applicable", 0, details)
    return StepResult(
        "kernel",
        status,
        f"Executed {actions} kernel memory optimization(s)"
        + (f" ({errors} error(s))" if errors else ""),
        0,
        details,
    )


def _resolve_trusted_binary(name: str) -> Optional[str]:
    """Resolve a system binary exclusively from trusted sbin/bin directories."""
    resolved = shutil.which(name, path=TRUSTED_SBIN_PATH)
    if not resolved:
        return None
    try:
        st = os.stat(resolved)
        if os.geteuid() == 0 and (st.st_uid != 0 or (st.st_mode & 0o022)):
            logger.error("Refusing to execute untrusted binary: %s", resolved)
            return None
    except OSError:
        return None
    return resolved


def _is_swapfile_writable(dev_path: str) -> bool:
    """Verify a file-based swap target is on a read-write mount before calling swapoff."""
    try:
        vfs = os.statvfs(dev_path)
        if hasattr(os, "ST_RDONLY") and (vfs.f_flag & os.ST_RDONLY):
            return False
        return os.access(dev_path, os.W_OK)
    except OSError:
        return False


def flush_swap(
    config: Config,
    snapshot: Optional[SystemSnapshot] = None,
    dry_run: bool = False,
) -> StepResult:
    """Safely flush eligible non-zram disk swap back into RAM when headroom is ample."""
    cfg = config.swap
    if not cfg.enabled:
        return StepResult("swap", "skipped", "Disabled in configuration")

    if os.geteuid() != 0 and not dry_run:
        return StepResult(
            "swap",
            "skipped",
            "Requires root privileges (run via sudo or system systemd service)",
        )

    snap = snapshot or collect_snapshot(include_swaps=False, include_tmpfs=False, include_psi=False)
    if cfg.max_load_avg_ratio > 0 and snap.load_ratio > cfg.max_load_avg_ratio:
        return StepResult(
            "swap",
            "skipped",
            f"Deferred: CPU load ratio {snap.load_ratio:.2f} > {cfg.max_load_avg_ratio:.2f}",
        )

    swaps = read_swaps()
    eligible = []
    skipped_zram = 0
    skipped_ro = 0
    for dev in swaps:
        if cfg.skip_zram and dev.is_zram:
            skipped_zram += 1
            continue
        if dev.used_mb < cfg.min_swap_used_mb:
            continue
        if dev.swap_type == "file" and not dry_run and not _is_swapfile_writable(dev.path):
            skipped_ro += 1
            logger.warning(
                "Skipping swapfile %s: mount is read-only (would fail swapon with EROFS)",
                dev.path,
            )
            continue
        eligible.append(dev)

    if not eligible:
        notes = []
        if skipped_zram:
            notes.append(f"{skipped_zram} zram device(s) skipped")
        if skipped_ro:
            notes.append(f"{skipped_ro} read-only swapfile(s) skipped")
        suffix = f" ({'; '.join(notes)})" if notes else ""
        return StepResult(
            "swap",
            "skipped",
            f"No disk swap device exceeds min_swap_used_mb={cfg.min_swap_used_mb}{suffix}",
        )

    mem = read_meminfo()
    total_to_flush = sum(d.used_mb for d in eligible)
    required_avail = total_to_flush + cfg.safety_margin_mb
    if mem.available_mb < required_avail:
        return StepResult(
            "swap",
            "skipped",
            f"Insufficient RAM headroom: {mem.available_mb} MiB available < "
            f"{required_avail} MiB required (swap {total_to_flush} + margin {cfg.safety_margin_mb})",
        )

    swapoff_bin = _resolve_trusted_binary("swapoff")
    swapon_bin = _resolve_trusted_binary("swapon")
    if not dry_run and (not swapoff_bin or not swapon_bin):
        return StepResult("swap", "error", "Trusted swapoff/swapon binaries not found")

    safe_env = {"PATH": TRUSTED_SBIN_PATH, "LC_ALL": "C"}
    details: List[str] = []
    flushed_mb = 0
    succeeded = 0
    failed = 0

    for dev in eligible:
        if dry_run:
            details.append(
                f"[dry-run] Would cycle swap device {dev.path} "
                f"(priority {dev.priority}, {dev.used_mb} MiB used)"
            )
            flushed_mb += dev.used_mb
            succeeded += 1
            continue

        assert swapoff_bin is not None and swapon_bin is not None
        swapon_cmd = [swapon_bin, "-p", str(dev.priority), "--", dev.path]
        try:
            subprocess.run(
                [swapoff_bin, "--", dev.path],
                check=True,
                capture_output=True,
                text=True,
                env=safe_env,
                timeout=SWAP_CMD_TIMEOUT_SECONDS,
            )
            subprocess.run(
                swapon_cmd,
                check=True,
                capture_output=True,
                text=True,
                env=safe_env,
                timeout=SWAP_CMD_TIMEOUT_SECONDS,
            )
            details.append(f"Flushed swap device {dev.path} ({dev.used_mb} MiB)")
            flushed_mb += dev.used_mb
            succeeded += 1
        except (subprocess.SubprocessError, OSError) as exc:
            failed += 1
            details.append(f"Failed cycling {dev.path}: {exc}")
            try:
                subprocess.run(
                    swapon_cmd,
                    check=False,
                    capture_output=True,
                    env=safe_env,
                    timeout=SWAP_CMD_TIMEOUT_SECONDS,
                )
            except (subprocess.SubprocessError, OSError):
                pass

    if failed > 0:
        return StepResult(
            "swap",
            "error",
            f"Swap flush encountered {failed} error(s) ({succeeded} succeeded, {flushed_mb} MiB flushed)",
            flushed_mb * 1024 * 1024,
            details,
        )
    status = "dry-run" if dry_run else "ok"
    verb = "Would flush" if dry_run else "Flushed"
    return StepResult(
        "swap",
        status,
        f"{verb} {flushed_mb} MiB across {succeeded} disk swap device(s)",
        flushed_mb * 1024 * 1024,
        details,
    )


ALL_STEP_NAMES = ("tmpfs", "watchdog", "user_caches", "kernel", "swap")


def run_cleanup(
    config: Config,
    reasons: Optional[List[str]] = None,
    dry_run: bool = False,
    critical: bool = False,
    only_steps: Optional[List[str]] = None,
) -> CleanupReport:
    """Execute the full configured cleanup pipeline and return a structured report."""
    started = time.time()
    start_mono = time.monotonic()
    pre_snap = collect_snapshot(
        tmpfs_paths=config.tmpfs.paths,
        include_swaps=False,
        include_tmpfs=False,
        include_psi=False,
        include_load=True,
    )
    selected = set(only_steps) if only_steps else set(ALL_STEP_NAMES)

    steps: List[StepResult] = []
    cgroup_reclaimed = False
    if "tmpfs" in selected:
        steps.append(clean_tmpfs(config, dry_run=dry_run))
    if "watchdog" in selected:
        steps.append(run_process_watchdog(config, dry_run=dry_run))
    if "user_caches" in selected:
        steps.append(clean_user_caches(config, dry_run=dry_run))
    if "kernel" in selected:
        kres = reclaim_kernel_memory(config, dry_run=dry_run, critical=critical)
        steps.append(kres)
        if kres.status == "ok" and any("memory.reclaim" in d for d in kres.details):
            cgroup_reclaimed = True
    if "swap" in selected:
        # Avoid immediately pulling back anon pages that cgroup v2 memory.reclaim just paged out
        if cgroup_reclaimed and only_steps is None:
            steps.append(
                StepResult(
                    "swap",
                    "skipped",
                    "Skipped to avoid reversing cgroup v2 memory.reclaim in the same pass",
                )
            )
        else:
            steps.append(flush_swap(config, snapshot=pre_snap, dry_run=dry_run))

    post_mem = read_meminfo()
    return CleanupReport(
        started_at=started,
        finished_at=time.time(),
        dry_run=dry_run,
        reasons=reasons or ["Manual invocation"],
        pre_available_mb=pre_snap.memory.available_mb,
        post_available_mb=post_mem.available_mb,
        pre_swap_used_mb=pre_snap.memory.swap_used_mb,
        post_swap_used_mb=post_mem.swap_used_mb,
        steps=steps,
        elapsed_seconds=time.monotonic() - start_mono,
        critical=critical,
    )
