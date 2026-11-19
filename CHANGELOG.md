# Changelog

All notable changes to `keep-ram-clean` will be documented in this file.

## [1.0.0] - 2026-11-19

### Added
- Configurable Linux RAM, `tmpfs`, swap, and memory-pressure management CLI and background daemon (`keep-ram-clean`).
- Reactive trigger engine supporting RAM %, available MiB, swap %/MiB, `tmpfs` %, Linux PSI (`/proc/pressure/memory`), cooldown, hysteresis, CPU load guardrails, periodic intervals, and daily/cron schedules.
- Safe, modular cleaners: `dir_fd`/`O_NOFOLLOW` `tmpfs` cleaner with open-file protection, `pidfd`-pinned process watchdog, optional user cache cleaner, cgroup v2 `memory.reclaim` + `drop_caches` kernel reclaimer, and zram-aware disk swap flusher.
- `keep-ram-clean status` (text + JSON), `keep-ram-clean doctor` (text + JSON), `keep-ram-clean run --dry-run`, `init-config`, `trigger` (`SIGUSR1`), and `reload` (`SIGHUP`).
- Hardened `systemd` system and user units, plus `scripts/install.sh` and `scripts/uninstall.sh`.
