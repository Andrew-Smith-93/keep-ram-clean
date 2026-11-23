# keep-ram-clean

A tiny (~2 MiB RSS), single-binary **Rust** daemon and CLI for configurable Linux **RAM, tmpfs, swap, and memory-pressure management**.

`keep-ram-clean` monitors your system using native Linux kernel telemetry (`/proc/meminfo`, `/proc/pressure/memory` PSI, `/proc/swaps`, `/proc/mounts`) and triggers safe, modular cleanup actions **only when you specify** — via memory/PSI thresholds, periodic intervals, cron schedules, or on-demand signals.

---

## Try v2.3.0 without installing it

On **Debian 12 (Bookworm), amd64**, download the `.deb`, `.provenance`, and
`.sha256` files from the [v2.3.0 release](https://github.com/Andrew-Smith-93/keep-ram-clean/releases/tag/v2.3.0)
into the same directory. In that directory, run as your normal user:

```sh
(
  set -eu
  sha256sum -c keep-ram-clean_2.3.0-1_amd64.sha256
  trial_dir=$(mktemp -d)
  dpkg-deb -x keep-ram-clean_2.3.0-1_amd64.deb "$trial_dir"
  "$trial_dir/usr/bin/keep-ram-clean" --version
  "$trial_dir/usr/bin/keep-ram-clean" pressure-watch --help
  printf 'Extracted files: %s\n' "$trial_dir"
)
```

This smoke test runs only version and help: no `sudo`, package installation,
services, cleanup, or process signaling. You can delete the temporary directory
afterward. To try monitoring with a dedicated workload you own, use the
[report-only workflow below](#opt-in-memory-pressure-relief) with the extracted
binary, without `--terminate`. No need to create memory pressure on your machine;
without sustained pressure, you won't see a recommendation.

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

## Opt-in memory-pressure relief

`pressure-watch` is separate from ordinary cleanup and is **report-only by default**.
It does not run through the existing system or user services, the cleanup command, or
TOML configuration. It borrows the idea of checking available RAM, available swap,
and kernel OOM scores from [earlyoom](https://github.com/rfjakob/earlyoom), but it
does not select arbitrary victims, act system-wide, or send SIGKILL.

Run in a terminal as the same regular, unprivileged user who owns the workloads.
To discover matching processes automatically, pass the exact canonical path to a
dedicated, user-owned binary. Repeat `--allow-exe` to allow up to 16 executables.

```sh
keep-ram-clean pressure-watch --discover --allow-exe "$HOME/bin/my-worker"
```

Discovery checks running processes only when pressure is sustained. It considers
only processes executing an allowlisted file, and pins that file at watcher startup.
If the binary is replaced, restart the watcher to allow the replacement. To restrict
the scope to already-known processes instead, omit `--discover` and repeat
`--workload-pid PID` up to 16 times alongside the executable allowlist.
Use only dedicated workers whose interruption you can tolerate.
System/package-owned executables, symlinks, wildcard paths,
privileged processes and the watcher's own process and ancestors are rejected.
After previewing the logs, add `--terminate` in a new foreground invocation only
if stopping those explicit workloads is acceptable. Ctrl-C stops monitoring.
Registration is bound to a Linux pidfd for each process. In explicit-PID mode,
restarts are not re-enrolled. Discovery may consider a new instance if it matches
the pinned allowlist, but it must be verified and bound afresh; an existing request
never follows a recycled PID. Missing or unreadable evidence
blocks action. SIGTERM can interrupt work or lose unsaved state, is not a promise
that the process exits, and is never escalated.

Every two seconds the watcher samples `/proc/meminfo`. With swap, at most 10%
available RAM and at most 10% free swap must persist for 20 seconds; without swap
the available RAM limit is 5%. Invalid telemetry resets the timer. It ranks only
registered workloads with at least 64 MiB resident memory by OOM score and then
RSS, and excludes negative OOM adjustments. It reports or attempts at most one
action per 60 seconds in a watch session, and attempts an individual SIGTERM
only once per registered process. These are host memory figures, not cgroup
quotas. Standard pidfd and procfs access are required; there is no numeric PID
signal fallback. No configuration or existing service behavior changes.

## Build & Installation

### Debian packages

Do not use the historical GitHub v2.2.0 amd64 binary as a Debian 12
package: its metadata says libc6 >= 2.34, but its ELF requires GLIBC_2.39.
That release tag predates the current packaging; a new build does not
retroactively correct or authenticate an old asset.

For Debian 12 **amd64**, build the committed checkout in the pinned Bookworm
image; this does not install the package or start either service:

```bash
./scripts/build-bookworm.sh dist/bookworm
(cd dist/bookworm && sha256sum -c *.sha256)
```

See [the build and provenance notes](debian/README.Debian) for requirements,
verification limits, and the separate Debian source-package path. Packages
built directly on another suite are not automatically compatible with Bookworm.

### From source (locked dependency floor: Rust/Cargo 1.85; Bookworm build uses 1.90.0)

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
# Check live RAM, PSI, swap, tmpfs, auto-clean activity, and daemon status
keep-ram-clean status [--top N] [--watch] [--interval SECS] [--json]

# View or change auto-clean trigger & notification settings (auto-reloads daemon)
keep-ram-clean config show
keep-ram-clean config set ram-percent 75
keep-ram-clean config set notify true

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
