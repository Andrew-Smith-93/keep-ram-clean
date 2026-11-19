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
  --no-service   Install CLI and config only; do not install/enable systemd service
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

if ! command -v python3 >/dev/null 2>&1; then
    echo "Error: python3 (>= 3.9) is required." >&2
    exit 1
fi

if ! python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 9) else 1)'; then
    echo "Error: Python 3.9 or newer is required (found $(python3 --version))." >&2
    exit 1
fi

if ! python3 -m pip --version >/dev/null 2>&1; then
    echo "Error: python3 -m pip is required (install python3-pip)." >&2
    exit 1
fi

install_python_pkg() {
    local pip_flags=("$@")
    if python3 -m pip install --help 2>/dev/null | grep -q -- "--break-system-packages"; then
        pip_flags+=("--break-system-packages")
    fi
    if python3 -c 'import setuptools' >/dev/null 2>&1; then
        python3 -m pip install --no-cache-dir --no-build-isolation --no-deps "${pip_flags[@]}" "$PROJECT_DIR"
    else
        echo "Note: setuptools not pre-installed; using pip build isolation..."
        python3 -m pip install --no-cache-dir --no-deps "${pip_flags[@]}" "$PROJECT_DIR"
    fi
}

if [[ "$MODE" == "system" ]]; then
    if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
        echo "Error: --system installation requires root (run with sudo)." >&2
        exit 1
    fi

    echo "[1/3] Installing keep-ram-clean system-wide..."
    install_python_pkg --prefix /usr/local

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
    echo "[1/3] Installing keep-ram-clean for user $(id -un)..."
    if [[ -n "${VIRTUAL_ENV:-}" ]]; then
        echo "Note: Active virtualenv detected ($VIRTUAL_ENV); installing into virtualenv."
        install_python_pkg
    else
        install_python_pkg --user
    fi

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

    if [[ -z "${VIRTUAL_ENV:-}" && ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
        echo "Note: $HOME/.local/bin is not in your PATH. Add it to invoke 'keep-ram-clean' directly."
    fi
fi
