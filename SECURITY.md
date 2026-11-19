# Security Policy

## Supported Versions

| Version | Supported |
| :--- | :--- |
| 1.0.x | Yes |

## Security Architecture

Because `keep-ram-clean` can run with root privileges (via `systemd` system service or `sudo`), it enforces strict defensive boundaries:

- **Root configuration trust**: When running as `root`, auto-discovery reads `/etc/keep-ram-clean/config.toml` only. Any loaded config file (including `--config`) must be a regular file owned by `uid 0`, not group/world-writable (`mode & 0o022 == 0`), with all ancestor directories up to `/` also root-owned and non-group/world-writable, read directly from an `O_NOFOLLOW` descriptor.
- **Symlink & mount-boundary protection**: `tmpfs` and `user_caches` cleaners traverse directories via `dir_fd` + `O_NOFOLLOW`, enforce `st_dev` mount boundaries, enforce a depth cap (`MAX_WALK_DEPTH = 64`), and verify file ownership.
- **PID-reuse defense**: Watchdog signalling and daemon IPC (`trigger`/`reload`) pin processes with Linux `pidfd` (`os.pidfd_open` + `signal.pidfd_send_signal`) and verify `/proc/<pid>/stat` start times.
- **Subprocess safety**: System binaries (`swapoff`, `swapon`) are resolved exclusively from trusted system directories with minimal `env`, `--` terminators, and timeouts. Desktop notifications (`notify-send`) are skipped when running as root and HTML-escaped.

## Reporting a Vulnerability

Please report security issues privately through GitHub Security Advisories (or contact the repository maintainer directly) rather than opening a public issue. Include steps to reproduce, affected version, and kernel/systemd version if applicable.
