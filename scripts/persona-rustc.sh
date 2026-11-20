#!/bin/sh
#j917
# SH7-d3d: fetch the Linux rustc tree the persona shim runs, pinned by sha256, into
# build/persona-rustc/tree (never committed: the driver alone is 290 MiB). The SH7-d3 cut
# (ratified 2026-11-14) chose upstream musl rustc via musl's own ld-musl: the pinned nightly's
# aarch64-unknown-linux-musl `rustc` component, unmodified, plus Alpine's libgcc_s (the driver
# NEEDs it) from the same pinned image persona/ is built from. Needs network (curl) and docker
# (OrbStack). Re-running with the tree present only re-checks it. — TEAL
#
#   tree/opt/rust/bin/rustc
#   tree/opt/rust/lib/librustc_driver-814edb8bd1ddf471.so
#   tree/lib/libgcc_s.so.1
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
OUT="$ROOT/build/persona-rustc"
TREE="$OUT/tree"
DATE=2026-06-22
TARBALL=rustc-nightly-aarch64-unknown-linux-musl.tar.xz
URL="https://static.rust-lang.org/dist/$DATE/$TARBALL"
TARBALL_SHA=e61f647e7566a0da358e18b6f182c3c7385b82a52d8777c29b841a8130b653cd
DRIVER=librustc_driver-814edb8bd1ddf471.so
IMAGE="alpine@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f" # alpine:3.22, arm64
PINS="c4742590a4a083a1343941f00f0730edbdfe780b7a16e5f2d88494c9b6063f9c  opt/rust/bin/rustc
b7b785e44c682f134c1ab90589604cfb6a038d94b56d3287063fa701ab5daff0  opt/rust/lib/$DRIVER
2d945d6ce360769b41fbffc652c2440014bbd69416f13fe206fbac19d32c9431  lib/libgcc_s.so.1"

check() {
    (cd "$TREE" && printf '%s\n' "$PINS" | shasum -a 256 -c -)
}

if [ -d "$TREE" ] && check >/dev/null 2>&1; then
    check
    exit 0
fi
mkdir -p "$OUT/dl" "$TREE/opt/rust/bin" "$TREE/opt/rust/lib" "$TREE/lib"
if ! printf '%s  %s\n' "$TARBALL_SHA" "$OUT/dl/$TARBALL" | shasum -a 256 -c - >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$OUT/dl/$TARBALL" "$URL"
    printf '%s  %s\n' "$TARBALL_SHA" "$OUT/dl/$TARBALL" | shasum -a 256 -c -
fi
STAGE="$OUT/extract"
rm -rf "$STAGE"
mkdir -p "$STAGE"
tar -xJf "$OUT/dl/$TARBALL" -C "$STAGE" \
    "rustc-nightly-aarch64-unknown-linux-musl/rustc/bin/rustc" \
    "rustc-nightly-aarch64-unknown-linux-musl/rustc/lib/$DRIVER"
cp "$STAGE/rustc-nightly-aarch64-unknown-linux-musl/rustc/bin/rustc" "$TREE/opt/rust/bin/rustc"
cp "$STAGE/rustc-nightly-aarch64-unknown-linux-musl/rustc/lib/$DRIVER" "$TREE/opt/rust/lib/$DRIVER"
rm -rf "$STAGE"

NAME=kumo-persona-rustc-$$
docker run -d --name "$NAME" --platform linux/arm64 "$IMAGE" sleep 300 >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT INT TERM
docker exec "$NAME" apk add --no-cache libgcc=14.2.0-r6 >/dev/null
docker cp -L "$NAME:/usr/lib/libgcc_s.so.1" "$TREE/lib/libgcc_s.so.1"
check
