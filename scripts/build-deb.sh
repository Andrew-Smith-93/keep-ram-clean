#!/usr/bin/env bash
# Build a Debian-Policy-compliant .deb binary package using cargo + dpkg-deb.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
OUT_DIR="${1:-$REPO_DIR/dist}"

VERSION="$(grep -m1 '^version' "$REPO_DIR/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
DEB_REV="1"
ARCH="$(dpkg --print-architecture)"
PKG_NAME="keep-ram-clean"
DEB_FILE="${PKG_NAME}_${VERSION}-${DEB_REV}_${ARCH}.deb"

echo "Building ${DEB_FILE}..."
(cd "$REPO_DIR" && cargo build --release --locked)

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p \
    "$STAGE/DEBIAN" \
    "$STAGE/usr/bin" \
    "$STAGE/etc/keep-ram-clean" \
    "$STAGE/usr/lib/systemd/system" \
    "$STAGE/usr/lib/systemd/user" \
    "$STAGE/usr/share/doc/${PKG_NAME}" \
    "$STAGE/usr/share/man/man1"

install -m 0755 "$REPO_DIR/target/release/keep-ram-clean" "$STAGE/usr/bin/keep-ram-clean"
install -m 0644 "$REPO_DIR/config/config.example.toml" "$STAGE/etc/keep-ram-clean/config.toml"
install -m 0644 "$REPO_DIR/debian/keep-ram-clean.service" "$STAGE/usr/lib/systemd/system/keep-ram-clean.service"
install -m 0644 "$REPO_DIR/debian/keep-ram-clean-user.service" "$STAGE/usr/lib/systemd/user/keep-ram-clean.service"
install -m 0644 "$REPO_DIR/debian/copyright" "$STAGE/usr/share/doc/${PKG_NAME}/copyright"
install -m 0644 "$REPO_DIR/debian/README.Debian" "$STAGE/usr/share/doc/${PKG_NAME}/README.Debian"
gzip -9n -c "$REPO_DIR/debian/changelog" > "$STAGE/usr/share/doc/${PKG_NAME}/changelog.Debian.gz"
gzip -9n -c "$REPO_DIR/debian/keep-ram-clean.1" > "$STAGE/usr/share/man/man1/keep-ram-clean.1.gz"
chmod 0644 \
    "$STAGE/usr/share/doc/${PKG_NAME}/changelog.Debian.gz" \
    "$STAGE/usr/share/man/man1/keep-ram-clean.1.gz"

INSTALLED_KB="$(du -sk "$STAGE/usr" "$STAGE/etc" | awk '{s+=$1} END {print s}')"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${VERSION}-${DEB_REV}
Section: admin
Priority: optional
Architecture: ${ARCH}
Installed-Size: ${INSTALLED_KB}
Depends: libc6 (>= 2.34)
Maintainer: Andrew Smith <andrewsmith.independent@gmail.com>
Homepage: https://github.com/Andrew-Smith-93/keep-ram-clean
Description: Configurable Linux RAM, tmpfs, swap, and memory-pressure daemon
 keep-ram-clean is a lightweight Rust daemon and CLI that monitors Linux memory
 health via /proc/meminfo, Pressure Stall Information (/proc/pressure/memory),
 /proc/swaps, and /proc/mounts, triggering safe modular cleanup actions only
 when user-specified thresholds or schedules are met.
 .
 Features include cgroup v2 proactive memory.reclaim, zram-aware disk swap
 flushing, open-file-safe tmpfs cleanup, a pidfd-pinned process watchdog, an
 interactive full-width status dashboard, and a kernel VM doctor audit.
EOF

install -m 0644 "$REPO_DIR/debian/conffiles" "$STAGE/DEBIAN/conffiles"
for script in postinst prerm postrm; do
    sed '/^#DEBHELPER#$/d' "$REPO_DIR/debian/$script" > "$STAGE/DEBIAN/$script"
    chmod 0755 "$STAGE/DEBIAN/$script"
done

(cd "$STAGE" && find usr etc -type f -print0 | sort -z | xargs -0 md5sum > DEBIAN/md5sums)
chmod 0644 "$STAGE/DEBIAN/md5sums"
find "$STAGE" -type d -exec chmod 0755 {} +

mkdir -p "$OUT_DIR"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT_DIR/$DEB_FILE"
echo "Built: $OUT_DIR/$DEB_FILE"
dpkg-deb --info "$OUT_DIR/$DEB_FILE"
dpkg-deb --contents "$OUT_DIR/$DEB_FILE"
