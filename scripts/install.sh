#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

MODE="user"
ENABLE_SERVICE=1

usage() {
    cat <<EOF
Usage: $(basename "$0") [--user | --system] [--no-service]

Options:
  --user         Install for current user (~/.local/bin, ~/.config/keep-ram-clean) [default]
  --system       Install system-wide (requires root; /usr/local/bin, /etc/keep-ram-clean)
  --no-service   Install binary and config only; do not install/enable systemd service
  -h, --help     Show this help message
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --user) MODE="user"; shift ;;
        --system) MODE="system"; shift ;;
        --no-service) ENABLE_SERVICE=0; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage; exit 2 ;;
    esac
done

BINARY="$PROJECT_DIR/target/release/keep-ram-clean"
if [[ ! -x "$BINARY" ]]; then
    if ! command -v cargo >/dev/null 2>&1; then
        echo "Error: Prebuilt binary not found at $BINARY and 'cargo' is not in PATH." >&2
        echo "Build first with: cargo build --release" >&2
        exit 1
    fi
    echo "[0/3] Building release binary with cargo..."
    (cd "$PROJECT_DIR" && cargo build --release)
fi

if [[ "$MODE" == "system" ]]; then
    if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
        echo "Error: --system installation requires root (run with sudo)." >&2
        exit 1
    fi

    echo "[1/3] Installing keep-ram-clean binary to /usr/local/bin/keep-ram-clean..."
    install -d -m 0755 -o root -g root /usr/local/bin
    install -m 0755 -o root -g root "$BINARY" /usr/local/bin/keep-ram-clean

    echo "[2/3] Installing default config to /etc/keep-ram-clean/config.toml..."
    install -d -m 0755 -o root -g root /etc/keep-ram-clean
    if [[ ! -f /etc/keep-ram-clean/config.toml ]]; then
        install -m 0644 -o root -g root "$PROJECT_DIR/config/config.example.toml" /etc/keep-ram-clean/config.toml
    else
        echo "  -> Existing /etc/keep-ram-clean/config.toml preserved."
    fi

    if [[ "$ENABLE_SERVICE" -eq 1 ]] && command -v systemctl >/dev/null 2>&1; then
        echo "[3/3] Installing and enabling systemd service..."
        install -m 0644 -o root -g root "$PROJECT_DIR/systemd/keep-ram-clean.service" /etc/systemd/system/keep-ram-clean.service
        systemctl daemon-reload
        systemctl enable --now keep-ram-clean.service
        echo "Done! Check status with: keep-ram-clean status"
    else
        echo "[3/3] Skipping systemd service installation."
    fi
else
    echo "[1/3] Installing keep-ram-clean binary to $HOME/.local/bin/keep-ram-clean..."
    install -d -m 0755 "$HOME/.local/bin"
    install -m 0755 "$BINARY" "$HOME/.local/bin/keep-ram-clean"

    XDG_BASE="${XDG_CONFIG_HOME:-$HOME/.config}"
    if [[ "$XDG_BASE" != /* ]]; then
        XDG_BASE="$HOME/.config"
    fi
    CONFIG_DIR="$XDG_BASE/keep-ram-clean"
    echo "[2/3] Installing default config to $CONFIG_DIR/config.toml..."
    install -d -m 0700 "$CONFIG_DIR"
    if [[ ! -f "$CONFIG_DIR/config.toml" ]]; then
        install -m 0600 "$PROJECT_DIR/config/config.example.toml" "$CONFIG_DIR/config.toml"
    else
        echo "  -> Existing $CONFIG_DIR/config.toml preserved."
    fi

    if [[ "$ENABLE_SERVICE" -eq 1 ]] && command -v systemctl >/dev/null 2>&1; then
        echo "[3/3] Installing and enabling systemd user service..."
        UNIT_DIR="$XDG_BASE/systemd/user"
        install -d -m 0700 "$UNIT_DIR"
        install -m 0644 "$PROJECT_DIR/systemd/keep-ram-clean-user.service" "$UNIT_DIR/keep-ram-clean.service"
        if systemctl --user daemon-reload 2>/dev/null && systemctl --user enable --now keep-ram-clean.service 2>/dev/null; then
            echo "Done! Check status with: keep-ram-clean status"
        else
            echo "Warning: Could not contact user systemd D-Bus session (e.g. headless/SSH)." >&2
            echo "Unit file installed to $UNIT_DIR/keep-ram-clean.service; enable it from a desktop/login session." >&2
        fi
    else
        echo "[3/3] Skipping systemd user service installation."
    fi

    if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
        echo "Note: $HOME/.local/bin is not in your PATH. Add it to invoke 'keep-ram-clean' directly."
    fi
fi
