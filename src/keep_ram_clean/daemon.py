"""Background daemon loop with hardened IPC, signal handling, and hot config reload."""

from __future__ import annotations

import html
import json
import logging
import logging.handlers
import os
import shutil
import signal
import stat
import subprocess
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Dict, Optional

from . import __version__
from .cleaners import CleanupReport, run_cleanup
from .config import Config, default_runtime_dir, load_config
from .metrics import collect_snapshot, read_proc_cmdline, read_proc_starttime
from .triggers import TriggerEngine

logger = logging.getLogger("keep_ram_clean.daemon")

TRUSTED_BIN_PATH = "/usr/bin:/bin"


def _is_safe_log_path(log_path: Path) -> bool:
    """Reject symlinked log targets or untrusted/non-root ancestors when running as root."""
    is_root = os.geteuid() == 0
    if is_root and not log_path.is_absolute():
        return False
    abs_path = Path(os.path.abspath(str(log_path)))
    try:
        if abs_path.exists() or abs_path.is_symlink():
            st = os.lstat(abs_path)
            if not stat.S_ISREG(st.st_mode):
                return False
            if is_root and (st.st_uid != 0 or (st.st_mode & 0o022)):
                return False

        # Verify every existing ancestor up to /
        for ancestor in abs_path.parents:
            if not ancestor.exists() and not ancestor.is_symlink():
                continue
            ast = os.lstat(ancestor)
            if not stat.S_ISDIR(ast.st_mode):
                return False
            if is_root and (ast.st_uid != 0 or (ast.st_mode & 0o022)):
                return False
    except OSError:
        return False
    return True


def configure_logging(config: Config) -> None:
    level_name = config.logging.level.upper()
    level = getattr(logging, level_name, logging.INFO)

    root = logging.getLogger("keep_ram_clean")
    root.setLevel(level)
    for handler in list(root.handlers):
        root.removeHandler(handler)
        try:
            handler.close()
        except Exception:
            pass

    fmt = logging.Formatter(
        "%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )

    stderr_handler = logging.StreamHandler()
    stderr_handler.setFormatter(fmt)
    root.addHandler(stderr_handler)

    if config.logging.file:
        log_path = Path(config.logging.file).expanduser()
        if not _is_safe_log_path(log_path):
            root.warning("Refusing unsafe log file path: %s", log_path)
            return
        try:
            log_path.parent.mkdir(parents=True, exist_ok=True, mode=0o750)
            file_handler = logging.handlers.RotatingFileHandler(
                log_path,
                maxBytes=config.logging.max_bytes,
                backupCount=config.logging.backup_count,
                encoding="utf-8",
            )
            file_handler.setFormatter(fmt)
            root.addHandler(file_handler)
        except OSError as exc:
            root.warning("Could not open log file %s: %s", log_path, exc)


def _send_notification(config: Config, report: CleanupReport, critical: bool) -> None:
    ncfg = config.notifications
    if not ncfg.enabled:
        return
    if os.geteuid() == 0:
        logger.debug("Skipping desktop notification in root/system mode.")
        return
    if ncfg.on_critical_only and not critical:
        return
    if report.ram_freed_mb < ncfg.min_freed_mb and not critical:
        return

    notify_bin = shutil.which("notify-send", path=TRUSTED_BIN_PATH)
    if not notify_bin:
        return

    urgency = "critical" if critical else "normal"
    title = "keep-ram-clean: Memory Cleanup"
    raw_body = (
        f"Trigger: {', '.join(report.reasons)}\n"
        f"Available RAM: {report.post_available_mb} MiB "
        f"({report.ram_freed_mb:+d} MiB)\n"
        f"Swap Used: {report.post_swap_used_mb} MiB "
        f"({-report.swap_freed_mb:+d} MiB)"
    )
    body = html.escape(raw_body)
    try:
        subprocess.run(
            [notify_bin, "-u", urgency, "-a", "keep-ram-clean", "--", title, body],
            check=False,
            capture_output=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        logger.debug("Desktop notification failed: %s", exc)


def _ensure_secure_runtime_dir(path: Path) -> None:
    """Create and verify runtime directory ownership, permissions, and non-symlink status."""
    is_root = os.geteuid() == 0
    mode = 0o755 if is_root else 0o700
    path.mkdir(parents=True, exist_ok=True, mode=mode)
    fd = os.open(str(path), os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        st = os.fstat(fd)
        if st.st_uid != os.geteuid():
            raise PermissionError(
                f"Runtime directory {path} is not owned by uid {os.geteuid()}"
            )
        if (st.st_mode & 0o022) != 0 or (not is_root and (st.st_mode & 0o077) != 0):
            os.fchmod(fd, mode)
    finally:
        os.close(fd)


def _atomic_write_text(directory: Path, filename: str, content: str, mode: int) -> None:
    """Atomically write a file inside a verified directory without following symlinks."""
    fd, tmp_name = tempfile.mkstemp(prefix=f".{filename}.", dir=str(directory))
    fd_owned = True
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fd_owned = False
            fh.write(content)
        os.replace(tmp_name, directory / filename)
    except Exception:
        if fd_owned:
            try:
                os.close(fd)
            except OSError:
                pass
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise


class RamCleanDaemon:
    """Long-running memory management daemon."""

    def __init__(
        self,
        config: Config,
        config_path: Optional[Path] = None,
        runtime_dir: Optional[Path] = None,
        dry_run: bool = False,
    ) -> None:
        self.config = config
        self.config_path = config_path or config.source_path
        self.runtime_dir = runtime_dir or default_runtime_dir()
        self.dry_run = dry_run
        self.engine = TriggerEngine(config)

        self._wake = threading.Event()
        self._stop_signal: Optional[int] = None
        self._stop_requested = False
        self._reload_requested = False
        self._manual_trigger_requested = False

        self._started_at = time.time()
        self._starttime = read_proc_starttime(os.getpid())
        self._cleanups_run = 0
        self._total_ram_freed_mb = 0
        self._total_swap_freed_mb = 0
        self._last_report: Optional[Dict[str, Any]] = None

    @property
    def pid_file(self) -> Path:
        return self.runtime_dir / "daemon.pid"

    @property
    def state_file(self) -> Path:
        return self.runtime_dir / "state.json"

    def _install_signal_handlers(self) -> None:
        # Keep signal handlers async-signal-safe: only set flags and wake the event loop
        def handle_stop(signum: int, _frame: Any) -> None:
            self._stop_signal = signum
            self._stop_requested = True
            self._wake.set()

        def handle_reload(_signum: int, _frame: Any) -> None:
            self._reload_requested = True
            self._wake.set()

        def handle_trigger(_signum: int, _frame: Any) -> None:
            self._manual_trigger_requested = True
            self._wake.set()

        signal.signal(signal.SIGTERM, handle_stop)
        signal.signal(signal.SIGINT, handle_stop)
        signal.signal(signal.SIGHUP, handle_reload)
        signal.signal(signal.SIGUSR1, handle_trigger)

    def _write_state(self, status: str = "running") -> None:
        try:
            _ensure_secure_runtime_dir(self.runtime_dir)
            payload = {
                "version": __version__,
                "pid": os.getpid(),
                "starttime": self._starttime,
                "status": status,
                "uid": os.geteuid(),
                "dry_run": self.dry_run,
                "config_path": str(self.config_path) if self.config_path else None,
                "started_at": self._started_at,
                "updated_at": time.time(),
                "cleanups_run": self._cleanups_run,
                "total_ram_freed_mb": self._total_ram_freed_mb,
                "total_swap_freed_mb": self._total_swap_freed_mb,
                "last_cleanup": self._last_report,
            }
            mode = 0o644 if os.geteuid() == 0 else 0o600
            _atomic_write_text(
                self.runtime_dir, "state.json", json.dumps(payload, indent=2), mode
            )
        except OSError as exc:
            logger.debug("Failed writing daemon state: %s", exc)

    def _write_pid(self) -> None:
        _ensure_secure_runtime_dir(self.runtime_dir)
        mode = 0o644 if os.geteuid() == 0 else 0o600
        _atomic_write_text(self.runtime_dir, "daemon.pid", f"{os.getpid()}\n", mode)

    def _cleanup_runtime_files(self) -> None:
        try:
            st = os.lstat(self.pid_file)
            if stat.S_ISREG(st.st_mode) and self.pid_file.read_text().strip() == str(os.getpid()):
                self.pid_file.unlink()
        except OSError:
            pass
        self._write_state(status="stopped")

    def _reload_config(self) -> None:
        self._reload_requested = False
        logger.info("Received SIGHUP, reloading configuration...")
        try:
            new_cfg = load_config(self.config_path)
            self.config = new_cfg
            self.engine.update_config(new_cfg)
            configure_logging(new_cfg)
            self._write_state(status="running")
            logger.info("Reloaded configuration from %s", new_cfg.source_path or "defaults")
        except Exception as exc:
            logger.error("Config reload failed (keeping previous config): %s", exc)

    def _execute_cleanup(self, reasons: list[str], critical: bool) -> None:
        logger.info(
            "Triggering cleanup (%s)%s",
            "; ".join(reasons),
            " [CRITICAL]" if critical else "",
        )
        report = run_cleanup(
            self.config,
            reasons=reasons,
            dry_run=self.dry_run,
            critical=critical,
        )
        self.engine.record_cleanup()
        self._cleanups_run += 1
        if report.ram_freed_mb > 0:
            self._total_ram_freed_mb += report.ram_freed_mb
        if report.swap_freed_mb > 0:
            self._total_swap_freed_mb += report.swap_freed_mb

        for step in report.steps:
            logger.info("  [%s] (%s) %s", step.name, step.status, step.summary)
            for detail in step.details:
                logger.debug("    - %s", detail)

        logger.info(
            "Cleanup finished in %.2fs | Available RAM: %d -> %d MiB (%+d MiB) | "
            "Swap: %d -> %d MiB (%+d MiB)",
            report.duration_seconds,
            report.pre_available_mb,
            report.post_available_mb,
            report.ram_freed_mb,
            report.pre_swap_used_mb,
            report.post_swap_used_mb,
            -report.swap_freed_mb,
        )

        self._last_report = {
            "finished_at": report.finished_at,
            "duration_seconds": report.duration_seconds,
            "reasons": report.reasons,
            "ram_freed_mb": report.ram_freed_mb,
            "swap_freed_mb": report.swap_freed_mb,
            "steps": [
                {"name": s.name, "status": s.status, "summary": s.summary}
                for s in report.steps
            ],
        }
        self._write_state(status="running")
        _send_notification(self.config, report, critical=critical)

    def _poll_once(self) -> None:
        t = self.config.triggers
        snap = collect_snapshot(
            tmpfs_paths=self.config.tmpfs.paths,
            include_swaps=False,
            include_tmpfs=bool(self.config.tmpfs.enabled and t.tmpfs_used_percent > 0),
            include_psi=bool(t.psi_some_avg10 > 0 or t.psi_full_avg10 > 0),
            include_load=bool(t.max_load_avg_ratio > 0),
        )
        decision = self.engine.evaluate(snap)
        if decision.deferred:
            logger.info(
                "Deferred cleanup (%s): %s",
                "; ".join(decision.reasons),
                decision.defer_reason,
            )
        elif decision.should_run:
            self._execute_cleanup(decision.reasons, critical=decision.critical)

    def run(self) -> int:
        configure_logging(self.config)
        self._install_signal_handlers()
        self._write_pid()
        self._write_state(status="running")

        logger.info(
            "keep-ram-clean daemon v%s started (pid=%d, uid=%d, poll=%ds, cooldown=%ds, config=%s)",
            __version__,
            os.getpid(),
            os.geteuid(),
            self.config.triggers.poll_interval_seconds,
            self.config.triggers.cooldown_seconds,
            self.config.source_path or "built-in defaults",
        )
        if os.geteuid() != 0:
            logger.info(
                "Running in non-root user mode: kernel reclaim and swap flush will be skipped."
            )

        try:
            while not self._stop_requested:
                # Clear wake event BEFORE checking flags to eliminate lost-wakeup races
                self._wake.clear()
                try:
                    if self._reload_requested:
                        self._reload_config()

                    if self._manual_trigger_requested:
                        self._manual_trigger_requested = False
                        logger.info("Received SIGUSR1, running on-demand cleanup...")
                        self._execute_cleanup(["On-demand signal (SIGUSR1)"], critical=False)
                    elif not self._stop_requested:
                        self._poll_once()
                except Exception:
                    logger.exception("Unhandled error in daemon poll cycle (continuing)")

                if not (
                    self._stop_requested
                    or self._reload_requested
                    or self._manual_trigger_requested
                ):
                    self._wake.wait(timeout=self.config.triggers.poll_interval_seconds)

            if self._stop_signal is not None:
                logger.info("Received signal %s, shutting down gracefully...", self._stop_signal)
        finally:
            self._cleanup_runtime_files()
            logger.info("keep-ram-clean daemon stopped.")
        return 0


def _verify_daemon_process(pid: int, expected_starttime: Optional[int]) -> bool:
    """Confirm `pid` is still the same keep-ram-clean daemon process."""
    if expected_starttime is None:
        return False
    current_st = read_proc_starttime(pid)
    if current_st is None or current_st != expected_starttime:
        return False
    cmdline = read_proc_cmdline(pid)
    if cmdline and ("keep-ram-clean" not in cmdline and "keep_ram_clean" not in cmdline):
        return False
    return True


def _is_trusted_state_file(rdir: Path, state_path: Path) -> bool:
    """Ensure runtime dir and state.json are not symlinks, owned by user/root, and not group/world-writable."""
    allowed_uids = {0, os.geteuid()}
    try:
        dst = os.lstat(rdir)
        if not stat.S_ISDIR(dst.st_mode) or dst.st_uid not in allowed_uids or (dst.st_mode & 0o022):
            return False
        fst = os.lstat(state_path)
        if not stat.S_ISREG(fst.st_mode) or fst.st_uid not in allowed_uids or (fst.st_mode & 0o022):
            return False
    except OSError:
        return False
    return True


def read_daemon_state(runtime_dir: Optional[Path] = None) -> Optional[Dict[str, Any]]:
    """Read the daemon's runtime state JSON and verify whether its PID is alive and genuine."""
    candidates = [runtime_dir] if runtime_dir else [
        default_runtime_dir(),
        Path("/run/keep-ram-clean"),
    ]
    for rdir in candidates:
        if rdir is None:
            continue
        state_path = rdir / "state.json"
        if not state_path.exists() or not _is_trusted_state_file(rdir, state_path):
            continue
        try:
            data = json.loads(state_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if not isinstance(data, dict):
            continue

        pid = data.get("pid")
        expected_st = data.get("starttime")
        alive = False
        if (
            data.get("status") == "running"
            and isinstance(pid, int)
            and pid > 1
            and isinstance(expected_st, int)
        ):
            try:
                os.kill(pid, 0)
                alive = _verify_daemon_process(pid, expected_st)
            except ProcessLookupError:
                alive = False
            except PermissionError:
                alive = _verify_daemon_process(pid, expected_st)
        data["alive"] = alive
        data["state_file"] = str(state_path)
        return data
    return None


def signal_daemon(sig: signal.Signals, runtime_dir: Optional[Path] = None) -> tuple[bool, str]:
    """Send a signal (SIGHUP / SIGUSR1) to the verified running daemon using pidfd."""
    state = read_daemon_state(runtime_dir=runtime_dir)
    if not state or not state.get("alive"):
        return False, "No running keep-ram-clean daemon found."
    pid = int(state["pid"])
    expected_st = state.get("starttime")
    if not isinstance(expected_st, int):
        return False, f"Daemon state missing starttime for PID {pid}; refusing to signal."
    try:
        if hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal"):
            try:
                pidfd = os.pidfd_open(pid, 0)
            except OSError:
                if not _verify_daemon_process(pid, expected_st):
                    return False, f"PID {pid} no longer matches keep-ram-clean daemon."
                os.kill(pid, sig)
                return True, f"Sent {sig.name} to daemon (PID {pid})."
            try:
                if not _verify_daemon_process(pid, expected_st):
                    return False, f"PID {pid} no longer matches keep-ram-clean daemon."
                signal.pidfd_send_signal(pidfd, sig)
            finally:
                os.close(pidfd)
        else:
            if not _verify_daemon_process(pid, expected_st):
                return False, f"PID {pid} no longer matches keep-ram-clean daemon."
            os.kill(pid, sig)
        return True, f"Sent {sig.name} to daemon (PID {pid})."
    except PermissionError:
        return (
            False,
            f"Permission denied signalling daemon PID {pid} (try running with sudo).",
        )
    except OSError as exc:
        return False, f"Failed to signal PID {pid}: {exc}"
