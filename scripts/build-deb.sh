#!/usr/bin/env bash
# Build a native convenience package; this is not a Debian archive source build.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
OUT_DIR="${1:-$REPO_DIR/dist}"

OUT_DIR="$(realpath -m "$OUT_DIR")"
cd "$REPO_DIR"
export LC_ALL=C
export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(dpkg-parsechangelog -STimestamp)}"
VERSION="$(dpkg-parsechangelog -SVersion)"
ARCH="$(dpkg --print-architecture)"
PKG_NAME="keep-ram-clean"
DEB_FILE="${PKG_NAME}_${VERSION}_${ARCH}.deb"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_DIR/target}"
RUST_HOST="$(rustc -vV | sed -n 's/^host: //p')"
revision="${KRC_BUILD_REVISION:-$(git rev-parse HEAD 2>/dev/null || echo unknown)}"
if git rev-parse --is-inside-work-tree >/dev/null 2>&1 && [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
    revision="$revision+dirty"
fi

echo "Building ${DEB_FILE}..."
cargo build --release --locked --target "$RUST_HOST" --target-dir "$TARGET_DIR"

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

install -m 0755 "$TARGET_DIR/$RUST_HOST/release/keep-ram-clean" "$STAGE/usr/bin/keep-ram-clean"
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

# Resolve the actual staged ELF using the suite's installed symbols database.
dpkg-shlibdeps -T"$STAGE/substvars" -e"$STAGE/usr/bin/$PKG_NAME"
grep -Eq '^shlibs:Depends=.+' "$STAGE/substvars" || { echo 'Missing ELF dependencies' >&2; exit 1; }
printf 'misc:Depends=\n' >> "$STAGE/substvars"
dpkg-gencontrol -p"$PKG_NAME" -P"$STAGE" -T"$STAGE/substvars" \
    -V"Installed-Size=$INSTALLED_KB" -O > "$STAGE/DEBIAN/control"
rm "$STAGE/substvars"

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
# These are the two artifacts in this build's scoped checksum manifest.
info="${DEB_FILE%.deb}.provenance"
(
    . /etc/os-release
    printf 'source_revision=%s\nos=%s\nsuite=%s\narchitecture=%s\n' "$revision" "$ID" "${VERSION_CODENAME:-unknown}" "$ARCH"
    printf 'image=%s\nsource_date_epoch=%s\n' "${KRC_BUILD_IMAGE:-host}" "$SOURCE_DATE_EPOCH"
    rustc -vV
    cargo --version
    dpkg-query -W dpkg-dev libc6
    sha256sum Cargo.lock
    dpkg-deb -f "$OUT_DIR/$DEB_FILE" Version Depends
) > "$OUT_DIR/$info"
(cd "$OUT_DIR" && sha256sum "$DEB_FILE" "$info" > "${DEB_FILE%.deb}.sha256")
echo "Built: $OUT_DIR/$DEB_FILE"
