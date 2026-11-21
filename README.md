# keep-ram-clean

A tiny (~2 MiB RSS), single-binary **Rust** daemon and CLI for configurable Linux **RAM, tmpfs, swap, and memory-pressure management**.

`keep-ram-clean` monitors your system using native Linux kernel telemetry (`/proc/meminfo`, `/proc/pressure/memory` PSI, `/proc/swaps`, `/proc/mounts`) and triggers safe, modular cleanup actions **only when you specify** — via memory/PSI thresholds, periodic intervals, cron schedules, or on-demand signals.

---

## Key Features

- **Ultra-Lightweight Native Rust Binary**
  - ~2 MiB resident RAM, `0.0%` idle CPU, ~2 ms CLI startup, zero runtime dependencies.
  - Best-effort `mlockall()` in root daemon mode keeps the daemon resident even during severe memory thrashing.
  - Linear-time `regex` and glob matching (immune to ReDoS).
- **Flexible User-Defined Triggers**
  - **Reactive thresholds** with cooldown & hysteresis: RAM %, minimum available MiB, swap %, swap MiB, and `tmpfs` usage %.
  - **Linux PSI (Pressure Stall Information)**: Triggers on actual kernel memory contention (`/proc/pressure/memory` `some`/`full` `avg10`), avoiding false alarms from healthy Linux page cache.
  - **Proactive schedules**: Fixed intervals (`interval_minutes`) or daily/cron wall-clock schedules (`"03:00"`, `"0 */4 * * *"`).
  - **On-demand control**: `keep-ram-clean run`, `keep-ram-clean trigger` (`SIGUSR1`), and live hot-reload via `keep-ram-clean reload` (`SIGHUP`).
- **Safe, Modern Linux Memory Reclamation**
  - **cgroup v2 `memory.reclaim` (Linux 6.1+ root cgroup)**: Gently reclaims cold pages without the cold-cache I/O storm caused by blindly running `drop_caches=3`.
  - **Configurable `drop_caches` & `compact_memory`**: Defaults to critical-only slab/dentry reclamation (`drop_caches_level = 2`, `drop_caches_on_critical_only = true`).
  - **Smart Swap Flushing**: Automatically detects and **skips `zram`** devices, checks RAM headroom + safety margin, checks CPU load, preserves swap priorities (`swapon -p`), and verifies swapfiles are writable before cycling.
  - **Open-File-Safe `tmpfs` Cleaner**: Verifies targets are actually `tmpfs` mounts, enforces glob allowlists/denylists, nested `mtime` checks, and scans `/proc/*/fd` so active files are never deleted.
  - **Graceful Process Watchdog (Opt-in)**: Linear-time regex + RSS thresholds with `notify`, `sigterm`, or `graceful` (SIGTERM → wait → optional SIGKILL) pinned via Linux `pidfd`.
  - **Built-in `doctor` Command**: Audits PSI, cgroup v2 reclaim, MGLRU, zram/zswap, `vm.swappiness`, `vm.vfs_cache_pressure`, monitored `tmpfs` paths, and top memory consumers.
- **Dry-Run & JSON Modes**: Preview every action safely with `--dry-run`, or integrate with scripts/bars using `--json` on `status`, `run`, and `doctor`.

---

## Build & Installation

Requires Rust 1.75+ (`cargo`).

### System-Wide (Recommended — enables kernel reclaim & swap management)

```bash
cargo build --release
sudo ./scripts/install.sh --system
```

This installs `/usr/local/bin/keep-ram-clean`, creates `/etc/keep-ram-clean/config.toml`, and enables `keep-ram-clean.service` under `systemd`.

### User Mode (Non-root — `tmpfs`, process watchdog & user caches)

```bash
cargo build --release
./scripts/install.sh --user
```

This installs `~/.local/bin/keep-ram-clean`, creates `~/.config/keep-ram-clean/config.toml`, and enables the systemd user service.

### Uninstall

```bash
./scripts/uninstall.sh --user              # or: sudo ./scripts/uninstall.sh --system
# Add --purge-config to also remove config files
```

---

## Usage

```bash
# Check live RAM, PSI, swap, tmpfs, top processes, and daemon status
keep-ram-clean status [--top N] [--watch] [--interval SECS] [--json]

# Audit Linux kernel VM configuration & memory health
keep-ram-clean doctor [--json]

# Preview what a cleanup would do (safe dry-run)
keep-ram-clean run --dry-run -v

# Run a one-shot cleanup (use sudo for kernel reclaim & swap flush)
sudo keep-ram-clean run [--only tmpfs,kernel,swap] [--critical] [--json]

# Start the daemon in the foreground
keep-ram-clean daemon [--dry-run] [--config PATH]

# Trigger an immediate cleanup or hot-reload config on a running daemon
keep-ram-clean trigger
keep-ram-clean reload

# Generate a starter configuration file
keep-ram-clean init-config [-o PATH] [-f]
```

`-c` / `--config PATH` is accepted both before and after subcommands (`run`, `daemon`, `status`, `doctor`).

---

## Security & Privilege Model

- **Root config isolation**: When running as `root`, `keep-ram-clean` only auto-loads `/etc/keep-ram-clean/config.toml` and enforces that any loaded config file (including `--config`) is a regular file owned by `uid 0`, not group/world-writable, with all ancestor directories up to `/` also root-owned and non-group/world-writable.
- **Symlink & mount-boundary safety**: Both `tmpfs` and `user_caches` cleaners traverse directories using `openat` + `O_NOFOLLOW`, enforce a depth cap (`64`), never cross filesystem (`st_dev`) boundaries, and skip files held open by running processes.
- **PID-reuse immunity**: Daemon signalling (`trigger` / `reload`) and the process watchdog pin processes with Linux `pidfd` (`pidfd_open` + `pidfd_send_signal`) and verify `/proc/<pid>/stat` start times.
- **Sandboxed systemd units**: The system unit pins `PATH`, limits capabilities (`CAP_SYS_ADMIN`, `CAP_SYS_PTRACE`, `CAP_KILL`, `CAP_DAC_OVERRIDE`, `CAP_FOWNER`), blocks `@mount`, enables `PrivateNetwork=yes`, `ProtectSystem=full`, `ProtectHome=yes`, `LogsDirectory=keep-ram-clean`, and `MemoryDenyWriteExecute=yes`.
- See [SECURITY.md](SECURITY.md) for vulnerability reporting.

---

## Running Tests

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

## License

MIT — see [LICENSE](LICENSE).
