# Changelog

All notable changes to `keep-ram-clean` will be documented in this file.

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
