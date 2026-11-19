#!/usr/bin/env bash
set -euo pipefail

MODE="user"
PURGE_CONFIG=0

usage() {
    echo "Usage: $(basename "$0") [--user | --system] [--purge-config]"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --user) MODE="user"; shift ;;
        --system) MODE="system"; shift ;;
        --purge-config) PURGE_CONFIG=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [[ "$MODE" == "system" ]]; then
    if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
        echo "Error: --system uninstall requires root." >&2
        exit 1
    fi
    if command -v systemctl >/dev/null 2>&1; then
        systemctl disable --now keep-ram-clean.service 2>/dev/null || true
        rm -f /etc/systemd/system/keep-ram-clean.service
        systemctl daemon-reload
    fi
    rm -f /usr/local/bin/keep-ram-clean
    rm -rf /run/keep-ram-clean
    if [[ "$PURGE_CONFIG" -eq 1 ]]; then
        rm -rf /etc/keep-ram-clean
    fi
else
    XDG_BASE="${XDG_CONFIG_HOME:-$HOME/.config}"
    if [[ "$XDG_BASE" != /* ]]; then
        XDG_BASE="$HOME/.config"
    fi
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user disable --now keep-ram-clean.service 2>/dev/null || true
        rm -f "$XDG_BASE/systemd/user/keep-ram-clean.service"
        systemctl --user daemon-reload 2>/dev/null || true
    fi
    rm -f "$HOME/.local/bin/keep-ram-clean"
    if [[ -n "${XDG_RUNTIME_DIR:-}" && "$XDG_RUNTIME_DIR" == /* ]]; then
        rm -rf "$XDG_RUNTIME_DIR/keep-ram-clean"
    fi
    XDG_STATE="${XDG_STATE_HOME:-$HOME/.local/state}"
    if [[ "$XDG_STATE" == /* ]]; then
        rm -rf "$XDG_STATE/keep-ram-clean"
    fi
    if [[ "$PURGE_CONFIG" -eq 1 ]]; then
        rm -rf "$XDG_BASE/keep-ram-clean"
    fi
fi

echo "keep-ram-clean uninstalled ($MODE mode)."
