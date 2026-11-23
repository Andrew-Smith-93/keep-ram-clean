# Security Policy

## Versions to check

If you can reproduce the issue safely, check the
[latest stable release](https://github.com/Andrew-Smith-93/keep-ram-clean/releases/latest).
Please report any affected version you know about, even if you have not checked
the latest release.

## Security Architecture

Because `keep-ram-clean` can run with root privileges (via `systemd` system service or `sudo`), it enforces strict defensive boundaries:

- **Root configuration trust**: When running as `root`, auto-discovery reads `/etc/keep-ram-clean/config.toml` only. Any loaded config file (including `--config`) must be a regular file owned by `uid 0`, not group/world-writable (`mode & 0o022 == 0`), with all ancestor directories up to `/` also root-owned and non-group/world-writable, read directly from an `O_NOFOLLOW` descriptor.
- **Symlink & mount-boundary protection**: `tmpfs` and `user_caches` cleaners traverse directories via `dir_fd` + `O_NOFOLLOW`, enforce `st_dev` mount boundaries, enforce a depth cap (`MAX_WALK_DEPTH = 64`), and verify file ownership.
- **PID-reuse defense**: Watchdog signalling and daemon IPC (`trigger`/`reload`) pin processes with Linux `pidfd` (`os.pidfd_open` + `signal.pidfd_send_signal`) and verify `/proc/<pid>/stat` start times.
- **Subprocess safety**: System binaries (`swapoff`, `swapon`) are resolved exclusively from trusted system directories with minimal `env`, `--` terminators, and timeouts. Desktop notifications (`notify-send`) are skipped when running as root and HTML-escaped.

## Reporting a Vulnerability

Do not disclose vulnerability details in a public issue, pull request, or
discussion. Contact the maintainer privately using the
[maintainer contact already listed in Cargo.toml](Cargo.toml).

Start with a short summary, affected version, and potential impact. If you have
reproduction steps, logs, or artifacts with sensitive details, agree on a
suitable private channel before sharing them. Include kernel or systemd
versions if relevant. Do not send credentials or unrelated private data.

If GitHub shows a **Report a vulnerability** button on the
[repository's Security Advisories page](https://github.com/Andrew-Smith-93/keep-ram-clean/security/advisories),
you may use that private form instead. If the button is absent, use the
maintainer contact above.
