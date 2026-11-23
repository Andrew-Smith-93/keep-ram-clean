# keep-ram-clean

A tiny (~2 MiB RSS), single-binary **Rust** daemon and CLI for configurable Linux **RAM, tmpfs, swap, and memory-pressure management**.

`keep-ram-clean` monitors your system using native Linux kernel telemetry (`/proc/meminfo`, `/proc/pressure/memory` PSI, `/proc/swaps`, `/proc/mounts`) and triggers safe, modular cleanup actions **only when you specify** — via memory/PSI thresholds, periodic intervals, cron schedules, or on-demand signals.

---

## Try v2.3.1 without installing it

On **Debian 12 (Bookworm), amd64**, download the `.deb`, `.provenance`, and
`.sha256` files from the [v2.3.1 release](https://github.com/Andrew-Smith-93/keep-ram-clean/releases/tag/v2.3.1)
into the same directory. Run as your normal user:

```sh
(
  set -eu
  sha256sum --strict -c keep-ram-clean_2.3.1-1_amd64.sha256
  trial_dir=$(mktemp -d)
  dpkg-deb -x keep-ram-clean_2.3.1-1_amd64.deb "$trial_dir"
  bin="$trial_dir/usr/bin/keep-ram-clean"
  "$bin" --version
  "$bin" status --help
  "$bin" pressure-watch --help
  printf '%s\n' '# Disposable Watch Settings trial' > "$trial_dir/settings.toml"
  printf 'Extracted files: %s\n' "$trial_dir"
  "$bin" --config "$trial_dir/settings.toml" status --watch --view config
)
```

In Settings, select a field with `j`/`k` and press Enter to see presets. Press `m` for manual entry, or `q` to exit. Saving in this trial only
edits the temporary file; there is no installation or daemon reload. You can
delete the temporary directory after the trial.

To try monitoring a dedicated workload you own, use the
[report-only workflow below](#opt-in-memory-pressure-relief) with the extracted
binary, without `--terminate`. The help command above does not monitor or signal
any workload.

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

See the [Watch and daemon glossary](#watch-and-daemon-glossary) for the labels in Watch and Settings.

In `status --watch`, memory, swap devices, pressure, consumers, and cleanup
activity are shown in tables that stack when columns are too narrow. In a short
interactive terminal, use `j`/`k` when the scroll footer appears.
Press `s` for Settings or start there with
`keep-ram-clean status --watch --view config`. Settings shows this Watch
process's configuration, not the daemon's in-memory settings.
Read and edit views use the same two-column grid on normal terminals. On
smaller screens, use `[` and `]` to move between pages. In the editor, `>`
marks the selected field and `*` marks a staged change. Press `q` to quit
Settings, or `s` or a standalone Esc to return to Watch. Arrow, Page,
and function keys are not Settings shortcuts.

Settings previews a system preset for **Available floor**. It uses Linux's
reported physical RAM, not current load, free RAM, process data, or cgroup
limits. The recommendation is 4% of RAM rounded up in 64 MiB steps, with a
128 MiB minimum and the existing 1536 MiB built-in value as its ceiling.
For example, it suggests 192 MiB at 4 GiB and 704 MiB at 16 GiB. It does not
change other triggers or enable any cleanup actions. If RAM size cannot be
determined, no preset is suggested.

A filled dot marks a **Saved** value from the file. An open dot marks a
**Built-in default** used when no value was saved. A **Recommended preset**
is only a suggestion; it never replaces either until you choose and save it.
To edit an existing configuration you own, select a row with `j`/`k` and
press Enter to open **Presets**. Choose a common value or **Off/On** with
`j`/`k` and press Enter to stage it. **Available floor**
also lists the system recommendation. For an exact value, press `m` to switch
to **Manual**, type the value, and press Enter to stage it. Esc or `p`
returns from Manual to Presets without staging that input. Neither choice
writes anything until review and save. Use `w` to review the exact file, the
original/proposed values, and their sources. Visit each change with `j`, then
press Enter to save the batch. After saving, the file holds an explicit value;
the tool does not recalculate saved values as hardware changes. Without an
editable file, Enter explains why editing is blocked or how to set up a file. You can explicitly create a
configuration with `keep-ram-clean init-config -o PATH` and open Watch with
`keep-ram-clean --config PATH status --watch --view config`.
Esc cancels the current step; discarding the draft or quitting never saves.
Built-in defaults, symlinks, and files you do not own are not editable here.
Save does not signal or reload the daemon. A later reload or restart can use
the saved values and change when and how cleanup runs. Choosing a higher
available-RAM floor than your manual setting (including zero, which disables
this trigger) can make the floor trigger sooner.

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

## Watch and daemon glossary

### Memory and pressure

| Label | Meaning |
| --- | --- |
| **RAM, Available, Used** | RAM is physical working memory. Linux's `MemAvailable` estimates what new apps can use without swapping, including some reclaimable memory. **Free** counts only currently unused RAM. Watch shows **Used** as total minus available, not total minus free. |
| **Cache, Reclaimable slab, Dirty** | Cache holds file data for faster access. Reclaimable slab is kernel bookkeeping that Linux may be able to reclaim. Dirty memory contains changes not yet written back. These are separate indicators, not amounts to add to Available. |
| **Swap, PRIO** | Swap gives Linux somewhere to move memory pages out of their normal RAM allocation. A swap entry can be a file, partition, or compressed RAM device. PRIO is the entry's priority; higher priorities are preferred. Swap usage by itself does not mean the machine is stalled now. |
| **zram, zswap** | zram is a compressed block device in RAM that can be used as swap. zswap is a compressed RAM cache in front of backing swap. They're different: zram can be a swap device; zswap isn't a separate device. |
| **tmpfs** | A memory-backed filesystem (a temporary file store). It grows as files are added, up to the mount's limit, and may use swap. The percentage is usage of that mount, not a percentage of all RAM. |
| **PSI** | Pressure Stall Information measures time tasks are held up waiting for a resource. For memory PSI, **some** means at least one task was stalled; **full** means all non-idle tasks were stalled at once. These are time percentages, not RAM usage or percentages of processes. |
| **avg10 / avg60 / avg300** | Linux's PSI averages over 10, 60, and 300 seconds, respectively. Watch's `10/60s` row shows the first two; the daemon's PSI thresholds use `avg10`. These windows aren't the Watch refresh or daemon polling interval. If PSI is unavailable, it isn't a zero-pressure reading. |
| **PID, RSS, Load / CPUs** | PID is a process ID. RSS (Resident Set Size) is memory resident in RAM for that process; it can include shared pages, so summing rows can count shared memory more than once. Load averages (1/5/15 minutes) count runnable and uninterruptible tasks on average, not CPU utilization. Load per CPU divides the 1-minute load average by the CPU count. |
| **MiB, GiB, pp** | 1 MiB is 1,048,576 bytes; 1 GiB is 1,024 MiB. `pp` means percentage points: 80% down to 75% is a drop of 5 pp. |

Linux references: [/proc and memory counters](https://docs.kernel.org/filesystems/proc.html),
[PSI](https://docs.kernel.org/accounting/psi.html), [zram](https://docs.kernel.org/admin-guide/blockdev/zram.html),
[zswap](https://docs.kernel.org/admin-guide/mm/zswap.html), and [tmpfs](https://docs.kernel.org/filesystems/tmpfs.html).

### Daemon and cleanup decisions

- **Daemon / service / status:** The daemon is the running process that checks conditions and can carry out enabled cleanup. A systemd service manages how that process runs. Status and Watch display telemetry and the daemon's recorded activity; opening Watch is not the same as starting the daemon. A cleanup reason says why a run was requested; it doesn't tell you which actions succeeded. Check the individual step results and measured RAM/swap changes.
- **Thresholds / Available floor:** RAM used, swap used, tmpfs used, and PSI trigger at or above their enabled limits. The Available floor triggers at or below its limit; raising it can trigger cleanup sooner. Swap can be checked by percentage or amount. A zero threshold disables that particular trigger. Watch's trigger values come from the config it reads, not a report of the daemon's in-memory settings.
- **Poll interval / Periodic interval / Schedule:** Poll interval is how often the daemon checks for work. A periodic interval requests cleanup after a configured number of minutes; a wall-clock or cron schedule requests it at configured times. Reactive means responding to measured thresholds rather than a timer. Neither is the Watch display refresh rate.
- **Cooldown / Re-arm margin (hysteresis):** Cooldown limits repeat reactive runs; some critical conditions use a shorter cooldown. Re-arm margin makes certain percentage triggers wait for the reading to fall below the threshold by a configured margin before they can trigger again. It reduces repeat runs while readings hover near a limit.
- **CPU load limit / Notifications:** CPU load limit is a load-per-CPU ratio used for load-sensitive cleanup decisions, not a CPU-use percentage. Desktop notices can be filtered by a minimum amount of RAM freed or limited to runs marked critical. These settings control notices, not whether cleanup triggers.
- **Drop caches / Drop critical-only / Cgroup reclaim / Minimum tmpfs age:** Drop caches asks the kernel to discard eligible caches: level 0 is off, 1 is page cache, 2 is reclaimable slab, and 3 is both. Drop critical-only restricts that action to runs classified as critical. Cgroup reclaim asks the kernel to reclaim memory from a Linux control group; the configured MiB is a request, not a guarantee. Minimum tmpfs age is how long a matching item must age before it can be considered for cleanup.
- **Dry-run / report-only:** `run --dry-run` previews cleanup without applying it. The independent `pressure-watch` command reports by default; this does not change the ordinary daemon's cleanup settings.

Kernel references: [drop_caches](https://docs.kernel.org/admin-guide/sysctl/vm.html#drop-caches)
and [cgroup v2 memory.reclaim](https://docs.kernel.org/admin-guide/cgroup-v2.html).

### Where Settings values come from

| Label | Meaning |
| --- | --- |
| **Saved** | The value is explicitly set in the TOML file Watch read. A saved value doesn't prove the daemon has loaded it. |
| **Built-in default** | The program supplies the value because it was not set in the file. It is not a hardware recommendation. |
| **Preset / Recommended preset** | A ready-made choice. The Recommended preset for Available floor uses detected RAM capacity as a starting point. Choosing it stages a value; nothing is written until you review and save. |
| **Manual** | A value you type instead of choosing a preset. |
| **Staged / Preset draft / Manual draft** | A pending edit in Watch. It has not been written to the file and is not active in the daemon. |

Saving in Watch writes the file but does not signal the daemon. Even if Watch
and the daemon report the same config path, a daemon can still be using values
from an earlier successful load. A failed reload leaves the previous values in use.

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
