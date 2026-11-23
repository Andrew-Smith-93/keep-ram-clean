# Contributing to Keep RAM Clean

You can help with bug reports, documentation, tests, Linux compatibility, or
code. You do not need root access or a running Keep RAM Clean service to start.

## Choose the right place

- Use the [issue chooser](https://github.com/Andrew-Smith-93/keep-ram-clean/issues/new/choose)
  for bugs and feature ideas. Search for an existing issue first.
- Report suspected vulnerabilities using the [security policy](SECURITY.md),
  not a public issue or pull request. Use the [Code of Conduct](CODE_OF_CONDUCT.md)
  for community concerns.
- Small, clear fixes can go straight to a pull request. Open an issue first for
  significant changes, especially those affecting cleanup, privileges, daemon
  defaults, or compatibility, so we can agree on the behavior before a large
  implementation.

## Build and test locally

Use Linux and Rust/Cargo 1.85 or newer. From a checkout:

```sh
cargo build --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
./target/debug/keep-ram-clean --version
./target/debug/keep-ram-clean --help
```

Use temporary files and synthetic data wherever possible. For an interactive
Settings change, point `status --watch --view config` at a disposable file
with `--config PATH`. Check a narrow terminal and `NO_COLOR=1` if you change
the terminal UI. Don't run a live cleanup, restart an installed service,
generate memory pressure, or signal active processes just to test a change.
The ignored real-pidfd signal test is not part of the regular test run and is
for a disposable, isolated PID namespace only.

For packaging changes, see [Debian packages](README.md#debian-packages)
and state which distribution and architecture you verified.

## Open a pull request

1. Work from a branch on the latest `main`. Keep the change focused and
   format any Rust code you touch.
2. Include a regression test when practical. Update documentation, CLI help,
   or the example configuration if user-visible behavior changes.
3. Link any related issue and explain what changed, how you verified it, and
   what you couldn't test. Call out compatibility or safety changes explicitly.
4. Before posting, redact secrets, personal paths, and private workload or
   process data from any logs, config excerpts, screenshots, and test fixtures.

Follow the [Code of Conduct](CODE_OF_CONDUCT.md) when participating.
