#!/usr/bin/env bash
# Build committed sources for amd64 in a pinned Debian 12 userspace.
set -euo pipefail
cd "$(dirname "$0")/.."
out="$(realpath -m "${1:-dist/bookworm}")"
image='rust:1.90.0-bookworm@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f'
rev="$(git rev-parse HEAD)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/src" "$work/out" "$work/vendor"
git archive "$rev" | tar -x -C "$work/src"
# Cargo verifies registry checksums; the isolated build has no network or home access.
(cd "$work/src" && cargo vendor --locked "$work/vendor" >/dev/null)
mkdir -p "$work/src/.cargo"
cat > "$work/src/.cargo/config.toml" <<'EOF'
[source.crates-io]
replace-with = "vendored"
[source.vendored]
directory = "/vendor"
EOF
docker run --rm --platform linux/amd64 --network=none --cap-drop=ALL \
    --security-opt=no-new-privileges --read-only --user "$(id -u):$(id -g)" \
    --tmpfs /tmp:rw,exec,mode=1777 --workdir /src \
    -v "$work/src:/src:ro" -v "$work/vendor:/vendor:ro" -v "$work/out:/out" \
    -e HOME=/tmp -e CARGO_HOME=/tmp/cargo -e CARGO_TARGET_DIR=/tmp/target \
    -e CARGO_NET_OFFLINE=true -e "KRC_BUILD_REVISION=$rev" -e "KRC_BUILD_IMAGE=$image" \
    "$image" bash -euc 'source /etc/os-release; test "$ID/$VERSION_CODENAME" = debian/bookworm
      scripts/build-deb.sh /out
      KRC_MAX_GLIBC=2.36 scripts/verify-deb.sh /out/*.deb'
mkdir -p "$out"
cp "$work/out/"* "$out/"
printf 'Verified committed source %s in %s\n' "$rev" "$out"
