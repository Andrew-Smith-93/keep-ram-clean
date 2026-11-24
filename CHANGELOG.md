# Changelog

All notable changes to `keep-ram-clean` will be documented in this file.

## [2.3.2] - 2026-11-24

### Fixed
- Size the default available-RAM floor to usable RAM so low-memory hosts
  no longer trigger idle cleanup just because their total RAM is below the
  old 1536 MiB floor.
- Keep explicit floor values, including zero, unchanged. New sample configs
  leave the floor automatic; Watch and config show report the effective value.

## [2.3.1] - 2026-11-23

### Changed
- Restyled the Watch dashboard with compact, responsive tables for resource
  usage, swap devices, pressure, top consumers, and cleanup activity.
- Shared section dividers and meter rails reduce vertical space without
  hiding readings or removing the meters' top and bottom borders.

### Added
- Watch Settings (`s` or `status --watch --view config`) shows the file or
  built-in source of each editable value. Choose a preset or Off/On first,
  or switch to Manual for an exact value. Review staged changes and save
  atomically without reloading the daemon.
- Settings previews a physical-RAM-based Available floor preset. It does not
  replace explicit values unless selected, reviewed, and saved. Runtime defaults
  and daemon behavior are unchanged.

## [2.3.0] - 2026-11-23

### Added
- Standalone `pressure-watch` command to discover exact allowlisted, user-owned workloads or watch explicitly registered PIDs under sustained memory pressure. It reports by default. Explicit `--terminate` allows SIGTERM only, guarded by process identity and cooldown; existing services are unchanged.

## [2.2.1] - 2026-11-23

### Fixed
- Rebuilt the Debian 12 (Bookworm) amd64 package in a pinned Bookworm environment.
- Derive runtime dependencies from the packaged ELF and include SHA-256 checksums and build provenance.

## [2.2.0] - 2026-11-20

### Added
- **Full-width 2-column consolidated dashboard**: Dynamically spans your full terminal width (`TIOCGWINSZ`) so `status` and `status --watch` fit on a single screen with zero vertical scrolling.
- **Live auto-clean visual state & history**: Daemon publishes `cleaning_now`, `cleaning_reason`, and the last 5 `recent_cleanups` to `state.json`; dashboard displays a live `⚡ AUTO-CLEAN IN PROGRESS` banner and a recent auto-clean activity feed with per-step icons.
- **CLI settings manager**: Added `keep-ram-clean config show` and `keep-ram-clean config set <key> <value>` (validates, saves atomically, and hot-reloads the running daemon via `SIGHUP`).
- **Interactive hotkeys in `status --watch`**: `[c]` Clean now, `[+]/[-]` adjust RAM trigger ±5%, `[n]` toggle desktop notification alerts, `[r]` reload config, `[q]` quit.

## [2.1.0] - 2026-11-20

### Added
- Upgraded terminal UI across `status`, `doctor`, and `run` with rounded Unicode banners, color-coded gauge bars for RAM/swap/`tmpfs`, inline RSS bars for top consumers, PSI status badges (`CALM` / `ELEVATED` / `PRESSURE`), and `● DAEMON RUNNING` / `○ DAEMON STOPPED` status pills (zero new dependencies; respects `NO_COLOR` and non-TTY pipes).
- Live-refreshing dashboard mode: `keep-ram-clean status --watch [--interval N]`.

## [2.0.0] - 2026-11-19

### Changed
- **Full Rust rewrite**: Replaced the Python implementation with a single native Rust binary (~2 MiB RSS vs ~18 MiB Python RSS, ~2ms CLI startup, zero runtime dependencies).
- **ReDoS-proof watchdog**: Uses Rust's linear-time `regex` engine and linear-time glob matcher.
- **Self-pipe signal wakeup & `mlockall`**: Async-signal-safe self-pipe + `poll(2)` eliminates lost wakeups; root daemon calls best-effort `mlockall(MCL_CURRENT | MCL_FUTURE)` to stay resident under severe memory thrashing.
- **100% config & CLI compatibility**: Existing `config.toml` files, systemd units, CLI flags, and JSON schemas work seamlessly.

## [1.0.0] - 2026-11-19

### Added
- Initial Python release of `keep-ram-clean` CLI, background daemon, PSI/cron trigger engine, `doctor` diagnostics, and hardened systemd units.
