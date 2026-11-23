# Changelog

All notable changes to `keep-ram-clean` will be documented in this file.

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
