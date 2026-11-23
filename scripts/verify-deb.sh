#!/usr/bin/env bash
# Verify a convenience package in the same build suite without installing/running it.
set -euo pipefail
export LC_ALL=C
deb="$(realpath "${1:?pass a .deb from build-deb.sh}")"
cd "$(dirname "$0")/.."
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
(cd "$(dirname "$deb")" && sha256sum --strict -c "$(basename "${deb%.deb}.sha256")")
dpkg-deb -x "$deb" "$tmp"
elf="$tmp/usr/bin/keep-ram-clean"
expected="$(dpkg-shlibdeps -O -e"$elf")"
[[ "$expected" == shlibs:Depends=?* ]]
[[ "$(dpkg-deb -f "$deb" Depends)" == "${expected#shlibs:Depends=}" ]]
[[ "$(dpkg-deb -f "$deb" Architecture)" == "$(dpkg --print-architecture)" ]]
[[ "$(dpkg-deb -f "$deb" Version)" == "$(dpkg-parsechangelog -SVersion)" ]]
max="$(readelf -W --version-info "$elf" | sed -nE 's/.*Name: GLIBC_([0-9.]+) .*/\1/p' | sort -Vu | tail -1)"
[[ -n "$max" ]]
if [[ -n "${KRC_MAX_GLIBC:-}" ]]; then
    dpkg --compare-versions "$max" le "$KRC_MAX_GLIBC"
fi
printf 'Verified %s: %s; highest required GLIBC_%s\n' "$(basename "$deb")" "${expected#shlibs:Depends=}" "$max"
