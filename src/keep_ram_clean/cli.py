"""Command-line interface for keep-ram-clean."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import signal
import sys
from pathlib import Path
from typing import List, Optional

from . import __version__
from .cleaners import ALL_STEP_NAMES, run_cleanup
from .config import (
    DEFAULT_SYSTEM_CONFIG,
    EXAMPLE_CONFIG_TOML,
    default_user_config,
    load_config,
)
from .daemon import RamCleanDaemon, read_daemon_state, signal_daemon
from .doctor import run_doctor
from .metrics import collect_snapshot, top_memory_processes


class Palette:
    def __init__(self) -> None:
        # Per no-color.org, non-empty NO_COLOR disables color
        use_color = sys.stdout.isatty() and not os.environ.get("NO_COLOR")
        self.BOLD = "\033[1m" if use_color else ""
        self.DIM = "\033[2m" if use_color else ""
        self.GREEN = "\033[0;32m" if use_color else ""
        self.CYAN = "\033[0;36m" if use_color else ""
        self.YELLOW = "\033[1;33m" if use_color else ""
        self.RED = "\033[0;31m" if use_color else ""
        self.NC = "\033[0m" if use_color else ""


def _fmt_ts(ts: object) -> str:
    if not isinstance(ts, (int, float)):
        return "unknown"
    try:
        return dt.datetime.fromtimestamp(ts).strftime("%Y-%m-%d %H:%M:%S")
    except (OverflowError, OSError, ValueError):
        return "unknown"


def cmd_run(args: argparse.Namespace) -> int:
    c = Palette()
    cfg = load_config(Path(args.config) if args.config else None)
    only: Optional[List[str]] = None
    if args.only:
        only = [s.strip() for s in args.only.split(",") if s.strip()]
        invalid = [s for s in only if s not in ALL_STEP_NAMES]
        if invalid:
            print(
                f"Unknown step(s): {', '.join(invalid)}. "
                f"Valid choices: {', '.join(ALL_STEP_NAMES)}",
                file=sys.stderr,
            )
            return 2

    report = run_cleanup(
        cfg,
        reasons=["Manual CLI run"],
        dry_run=args.dry_run,
        critical=args.critical,
        only_steps=only,
    )

    exit_code = 1 if report.has_errors else 0

    if args.json:
        payload = {
            "version": __version__,
            "config_path": str(cfg.source_path) if cfg.source_path else None,
            "dry_run": report.dry_run,
            "critical": report.critical,
            "reasons": report.reasons,
            "started_at": report.started_at,
            "finished_at": report.finished_at,
            "duration_seconds": report.duration_seconds,
            "pre_available_mb": report.pre_available_mb,
            "post_available_mb": report.post_available_mb,
            "ram_freed_mb": report.ram_freed_mb,
            "pre_swap_used_mb": report.pre_swap_used_mb,
            "post_swap_used_mb": report.post_swap_used_mb,
            "swap_freed_mb": report.swap_freed_mb,
            "has_errors": report.has_errors,
            "steps": [
                {
                    "name": s.name,
                    "status": s.status,
                    "summary": s.summary,
                    "bytes_freed": s.bytes_freed,
                    "details": s.details,
                }
                for s in report.steps
            ],
        }
        print(json.dumps(payload, indent=2))
        return exit_code

    mode = f" {c.YELLOW}[DRY-RUN]{c.NC}" if report.dry_run else ""
    print(f"{c.BOLD}{c.CYAN}keep-ram-clean v{__version__}{c.NC}{mode}")
    print(f"Config: {cfg.source_path or 'built-in defaults'}")
    print("-" * 58)
    for step in report.steps:
        if step.status == "ok":
            icon = f"{c.GREEN}✔{c.NC}"
        elif step.status == "dry-run":
            icon = f"{c.CYAN}◎{c.NC}"
        elif step.status == "skipped":
            icon = f"{c.YELLOW}–{c.NC}"
        else:
            icon = f"{c.RED}✖{c.NC}"
        print(f" {icon} {c.BOLD}{step.name:<12}{c.NC} {step.summary}")
        if args.verbose or report.dry_run or step.status == "error":
            for d in step.details:
                print(f"     {c.DIM}• {d}{c.NC}")

    print("-" * 58)
    ram_delta = f"{report.ram_freed_mb:+d} MiB"
    swap_delta = f"{-report.swap_freed_mb:+d} MiB"
    print(
        f"Available RAM : {c.BOLD}{report.post_available_mb} MiB{c.NC} "
        f"(was {report.pre_available_mb} MiB, {ram_delta})"
    )
    print(
        f"Swap Used     : {c.BOLD}{report.post_swap_used_mb} MiB{c.NC} "
        f"(was {report.pre_swap_used_mb} MiB, {swap_delta})"
    )
    print(f"Elapsed       : {report.duration_seconds:.2f}s")
    return exit_code


def cmd_daemon(args: argparse.Namespace) -> int:
    cfg_path = Path(args.config) if args.config else None
    cfg = load_config(cfg_path)
    daemon = RamCleanDaemon(
        config=cfg,
        config_path=cfg_path,
        dry_run=args.dry_run,
    )
    return daemon.run()


def cmd_status(args: argparse.Namespace) -> int:
    if args.top < 0:
        print("Error: --top must be >= 0", file=sys.stderr)
        return 2

    c = Palette()
    cfg = load_config(Path(args.config) if args.config else None)
    snap = collect_snapshot(tmpfs_paths=cfg.tmpfs.paths)
    top_procs = top_memory_processes(limit=args.top)
    daemon_state = read_daemon_state()

    if args.json:
        payload = {
            "version": __version__,
            "memory": {
                "total_mb": snap.memory.total_mb,
                "used_mb": snap.memory.used_mb,
                "available_mb": snap.memory.available_mb,
                "used_percent": snap.memory.used_percent,
                "buffers_mb": snap.memory.buffers_mb,
                "cached_mb": snap.memory.cached_mb,
                "slab_reclaimable_mb": snap.memory.slab_reclaimable_mb,
                "dirty_mb": snap.memory.dirty_mb,
                "swap_total_mb": snap.memory.swap_total_mb,
                "swap_free_mb": snap.memory.swap_free_mb,
                "swap_used_mb": snap.memory.swap_used_mb,
                "swap_used_percent": snap.memory.swap_used_percent,
            },
            "psi": {
                "available": snap.psi.available,
                "some_avg10": snap.psi.some_avg10,
                "some_avg60": snap.psi.some_avg60,
                "some_avg300": snap.psi.some_avg300,
                "full_avg10": snap.psi.full_avg10,
                "full_avg60": snap.psi.full_avg60,
                "full_avg300": snap.psi.full_avg300,
            },
            "load_avg": {
                "1m": snap.load_avg_1m,
                "5m": snap.load_avg_5m,
                "15m": snap.load_avg_15m,
                "cpus": snap.cpu_count,
                "ratio": snap.load_ratio,
            },
            "swaps": [
                {
                    "path": s.path,
                    "type": s.swap_type,
                    "size_mb": s.size_mb,
                    "used_mb": s.used_mb,
                    "priority": s.priority,
                    "is_zram": s.is_zram,
                }
                for s in snap.swaps
            ],
            "tmpfs": [
                {
                    "mountpoint": m.mountpoint,
                    "total_mb": m.total_mb,
                    "used_mb": m.used_mb,
                    "free_mb": m.free_mb,
                    "used_percent": m.used_percent,
                }
                for m in snap.tmpfs_mounts
            ],
            "top_processes": [
                {"pid": p.pid, "comm": p.comm, "rss_mb": p.rss_mb} for p in top_procs
            ],
            "daemon": daemon_state,
        }
        print(json.dumps(payload, indent=2))
        return 0

    mem = snap.memory
    print(f"{c.BOLD}{c.CYAN}keep-ram-clean Status{c.NC}")
    print("-" * 58)
    print(
        f"Physical RAM  : {c.BOLD}{mem.used_mb} / {mem.total_mb} MiB{c.NC} "
        f"({mem.used_percent:.1f}% used, {mem.available_mb} MiB available)"
    )
    print(
        f"Caches / Slab : PageCache {mem.cached_mb} MiB | "
        f"Reclaimable Slab {mem.slab_reclaimable_mb} MiB | Dirty {mem.dirty_mb} MiB"
    )
    print(
        f"Swap          : {c.BOLD}{mem.swap_used_mb} / {mem.swap_total_mb} MiB{c.NC} "
        f"({mem.swap_used_percent:.1f}% used)"
    )
    if snap.swaps:
        for s in snap.swaps:
            tag = " [zram]" if s.is_zram else ""
            print(f"                  • {s.path}{tag}: {s.used_mb}/{s.size_mb} MiB")

    if snap.psi.available:
        print(
            f"Memory PSI    : some avg10={snap.psi.some_avg10:.2f} "
            f"avg60={snap.psi.some_avg60:.2f} | "
            f"full avg10={snap.psi.full_avg10:.2f} avg60={snap.psi.full_avg60:.2f}"
        )
    print(
        f"CPU Load      : {snap.load_avg_1m:.2f} {snap.load_avg_5m:.2f} "
        f"{snap.load_avg_15m:.2f} ({snap.cpu_count} cores, ratio {snap.load_ratio:.2f})"
    )

    if snap.tmpfs_mounts:
        print("Monitored tmpfs:")
        for m in snap.tmpfs_mounts:
            print(
                f"  • {m.mountpoint}: {m.used_mb}/{m.total_mb} MiB ({m.used_percent:.1f}%)"
            )

    if top_procs:
        print(f"Top {len(top_procs)} Memory Consumers:")
        for p in top_procs:
            print(f"  • PID {p.pid:<7} {p.rss_mb:>8.1f} MiB  {p.comm}")

    print("-" * 58)
    if daemon_state and daemon_state.get("alive"):
        print(
            f"Daemon        : {c.GREEN}RUNNING{c.NC} (PID {daemon_state.get('pid', '?')}, "
            f"since {_fmt_ts(daemon_state.get('started_at'))})"
        )
        print(
            f"Cleanups Run  : {daemon_state.get('cleanups_run', 0)} "
            f"(cumulative +{daemon_state.get('total_ram_freed_mb', 0)} MiB RAM, "
            f"-{daemon_state.get('total_swap_freed_mb', 0)} MiB swap)"
        )
        last = daemon_state.get("last_cleanup")
        if isinstance(last, dict):
            print(
                f"Last Cleanup  : {_fmt_ts(last.get('finished_at'))} — "
                f"{'; '.join(last.get('reasons', []))}"
            )
    else:
        print(f"Daemon        : {c.YELLOW}NOT RUNNING{c.NC}")
    return 0


def cmd_doctor(args: argparse.Namespace) -> int:
    c = Palette()
    cfg = load_config(Path(args.config) if args.config else None)
    findings = run_doctor(cfg)

    if args.json:
        payload = {
            "version": __version__,
            "findings": [
                {
                    "check": f.check,
                    "status": f.status,
                    "detail": f.detail,
                    "recommendation": f.recommendation,
                }
                for f in findings
            ],
        }
        print(json.dumps(payload, indent=2))
        return 0

    print(f"{c.BOLD}{c.CYAN}keep-ram-clean Doctor — System Memory Audit{c.NC}")
    print("-" * 58)
    for f in findings:
        if f.status == "ok":
            icon = f"{c.GREEN}✔{c.NC}"
        elif f.status == "warn":
            icon = f"{c.YELLOW}⚠{c.NC}"
        else:
            icon = f"{c.CYAN}ℹ{c.NC}"
        print(f" {icon} {c.BOLD}{f.check}{c.NC}: {f.detail}")
        if f.recommendation:
            print(f"     {c.DIM}→ {f.recommendation}{c.NC}")
    return 0


def cmd_init_config(args: argparse.Namespace) -> int:
    if args.output:
        dest = Path(args.output).expanduser()
    elif os.geteuid() == 0:
        dest = DEFAULT_SYSTEM_CONFIG
    else:
        dest = default_user_config()

    if (dest.exists() or dest.is_symlink()) and not args.force:
        print(
            f"Config already exists at {dest}. Use --force to overwrite.",
            file=sys.stderr,
        )
        return 1

    if dest.is_symlink():
        print(f"Refusing to write through symlink: {dest}", file=sys.stderr)
        return 1

    is_root = os.geteuid() == 0
    dest.parent.mkdir(parents=True, exist_ok=True, mode=0o755 if is_root else 0o700)
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW
    mode = 0o644 if is_root else 0o600
    fd = os.open(str(dest), flags, mode)
    fd_owned = True
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            fd_owned = False
            fh.write(EXAMPLE_CONFIG_TOML)
    except Exception:
        if fd_owned:
            try:
                os.close(fd)
            except OSError:
                pass
        raise
    print(f"Wrote default configuration to {dest}")
    return 0


def cmd_signal(sig: signal.Signals) -> int:
    ok, msg = signal_daemon(sig)
    print(msg, file=sys.stdout if ok else sys.stderr)
    return 0 if ok else 1


def build_parser() -> argparse.ArgumentParser:
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument(
        "-c",
        "--config",
        help="Path to TOML configuration file",
        default=argparse.SUPPRESS,
    )

    parser = argparse.ArgumentParser(
        prog="keep-ram-clean",
        description="Configurable Linux RAM, tmpfs, swap, and memory-pressure management tool & daemon.",
    )
    parser.add_argument(
        "-V", "--version", action="version", version=f"%(prog)s {__version__}"
    )
    parser.add_argument(
        "-c", "--config", help="Path to TOML configuration file", default=None
    )

    sub = parser.add_subparsers(dest="command", required=True)

    p_run = sub.add_parser("run", parents=[common], help="Run a one-shot cleanup immediately")
    p_run.add_argument("--dry-run", action="store_true", help="Preview actions without modifying anything")
    p_run.add_argument(
        "--only",
        help=f"Comma-separated subset of steps to run ({','.join(ALL_STEP_NAMES)})",
        default=None,
    )
    p_run.add_argument("--critical", action="store_true", help="Treat run as critical pressure")
    p_run.add_argument("--json", action="store_true", help="Output report as JSON")
    p_run.add_argument("-v", "--verbose", action="store_true", help="Show detailed step output")
    p_run.set_defaults(func=cmd_run)

    p_daemon = sub.add_parser(
        "daemon", parents=[common], help="Start the background monitoring & cleanup daemon"
    )
    p_daemon.add_argument("--dry-run", action="store_true", help="Log what the daemon would do without modifying anything")
    p_daemon.set_defaults(func=cmd_daemon)

    p_status = sub.add_parser(
        "status", parents=[common], help="Show live RAM, PSI, swap, tmpfs, and daemon status"
    )
    p_status.add_argument("--json", action="store_true", help="Output status as JSON")
    p_status.add_argument("--top", type=int, default=5, help="Number of top memory processes to show")
    p_status.set_defaults(func=cmd_status)

    p_doc = sub.add_parser(
        "doctor", parents=[common], help="Audit kernel VM settings and memory health"
    )
    p_doc.add_argument("--json", action="store_true", help="Output audit findings as JSON")
    p_doc.set_defaults(func=cmd_doctor)

    p_init = sub.add_parser("init-config", help="Generate a documented starter config.toml")
    p_init.add_argument("-o", "--output", help="Destination config path", default=None)
    p_init.add_argument("-f", "--force", action="store_true", help="Overwrite existing config file")
    p_init.set_defaults(func=cmd_init_config)

    p_reload = sub.add_parser("reload", help="Send SIGHUP to running daemon to reload config")
    p_reload.set_defaults(func=lambda _a: cmd_signal(signal.SIGHUP))

    p_trig = sub.add_parser("trigger", help="Send SIGUSR1 to running daemon for immediate cleanup")
    p_trig.set_defaults(func=lambda _a: cmd_signal(signal.SIGUSR1))

    return parser


def main(argv: Optional[List[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return int(args.func(args))
    except (OSError, ValueError) as exc:
        print(f"Error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130
