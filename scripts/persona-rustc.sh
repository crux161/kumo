#!/bin/sh
#j917
#j920
#j924
# SH7-d3d: fetch the Linux rustc tree the persona shim runs, pinned by sha256, into
# build/persona-rustc/tree (never committed: the driver alone is 290 MiB). The SH7-d3 cut
# (ratified 2026-11-14) chose upstream musl rustc via musl's own ld-musl: the pinned nightly's
# aarch64-unknown-linux-musl `rustc` component, unmodified, plus Alpine's libgcc_s (the driver
# NEEDs it) from the same pinned image persona/ is built from. Needs network (curl) and docker
# (OrbStack). Re-running with the tree present only re-checks it. — TEAL
#
#   tree/opt/rust/bin/rustc
#   tree/opt/rust/lib/librustc_driver-814edb8bd1ddf471.so
#   tree/opt/rust/lib/rustlib/aarch64-unknown-none/lib/  (core, alloc and compiler_builtins rlibs and metadata)
#   tree/lib/libgcc_s.so.1
#   tree/opt/rust/lib/rustlib/aarch64-unknown-linux-musl/bin/rust-lld  (j924, SH7-f: the linker
#       rustc spawns for an aarch64-unknown-kumo executable, from the same rustc component)
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
OUT="$ROOT/build/persona-rustc"
TREE="$OUT/tree"
DATE=2026-06-22
TARBALL=rustc-nightly-aarch64-unknown-linux-musl.tar.xz
TARGET=aarch64-unknown-none
STD_TARBALL=rust-std-nightly-$TARGET.tar.xz
STD_SHA=eca05287f4ea31d0e803fa87a595a0d151d8853a39aec14be97733a8a6e641a7
TARGET_LIB=lib/rustlib/$TARGET/lib
TARBALL_SHA=e61f647e7566a0da358e18b6f182c3c7385b82a52d8777c29b841a8130b653cd
DRIVER=librustc_driver-814edb8bd1ddf471.so
LLD=opt/rust/lib/rustlib/aarch64-unknown-linux-musl/bin/rust-lld
IMAGE="alpine@sha256:49340b346798875b5592e5d420241f2f9cfba5f47e62ebb1446a1b34734e898f" # alpine:3.22, arm64
PINS="c4742590a4a083a1343941f00f0730edbdfe780b7a16e5f2d88494c9b6063f9c  opt/rust/bin/rustc
b7b785e44c682f134c1ab90589604cfb6a038d94b56d3287063fa701ab5daff0  opt/rust/lib/$DRIVER
2d945d6ce360769b41fbffc652c2440014bbd69416f13fe206fbac19d32c9431  lib/libgcc_s.so.1
4e89dab3b4938053d0a0b92bd34f3b1c5189d05bf18d46f7c26356b61c8a6c1d  opt/rust/$TARGET_LIB/liballoc-e7dd7d8211fee4be.rlib
7d095451ef66c054c807c7d990a4f49c2d9cb4d8c477f29b5fde8b369089996c  opt/rust/$TARGET_LIB/liballoc-e7dd7d8211fee4be.rmeta
d66b25a7acfe706557eda67001170cdeff1aec1f34b6886b33f7e6362e80ff5e  opt/rust/$TARGET_LIB/libcompiler_builtins-d1e347987669791e.rlib
6e13fee72d9708b5412ff440b9d3bb03935045a7d01cbca1724473e86937a812  opt/rust/$TARGET_LIB/libcompiler_builtins-d1e347987669791e.rmeta
f743f925e93779c189dc5574dc919002409fe5ce7c83836ed3f8bad6a26eed29  opt/rust/$TARGET_LIB/libcore-bf137cc3142afd85.rlib
51696641966b07d28020dca5628f211b81221690b6c547687c09613845d3857d  opt/rust/$TARGET_LIB/libcore-bf137cc3142afd85.rmeta
236909a842ad8970e3f7ab01a9077d254a003cfb047221e5219f211a18919520  $LLD"

check() {
    (cd "$TREE" && printf '%s\n' "$PINS" | shasum -a 256 -c -)
}

if [ -d "$TREE" ] && check >/dev/null 2>&1; then
    check
    exit 0
fi
mkdir -p "$OUT/dl" "$TREE/opt/rust/bin" "$TREE/opt/rust/lib" "$TREE/lib"
fetch() {
    archive="$OUT/dl/$1"
    if ! printf '%s  %s\n' "$2" "$archive" | shasum -a 256 -c - >/dev/null 2>&1; then
        curl -fL --retry 3 -o "$archive" "https://static.rust-lang.org/dist/$DATE/$1"
        printf '%s  %s\n' "$2" "$archive" | shasum -a 256 -c -
    fi
}
fetch "$TARBALL" "$TARBALL_SHA"
fetch "$STD_TARBALL" "$STD_SHA"
STAGE="$OUT/extract"
rm -rf "$STAGE"
mkdir -p "$STAGE"
tar -xJf "$OUT/dl/$TARBALL" -C "$STAGE" \
    "rustc-nightly-aarch64-unknown-linux-musl/rustc/bin/rustc" \
    "rustc-nightly-aarch64-unknown-linux-musl/rustc/lib/$DRIVER" \
    "rustc-nightly-aarch64-unknown-linux-musl/rustc/${LLD#opt/rust/}"
cp "$STAGE/rustc-nightly-aarch64-unknown-linux-musl/rustc/bin/rustc" "$TREE/opt/rust/bin/rustc"
cp "$STAGE/rustc-nightly-aarch64-unknown-linux-musl/rustc/lib/$DRIVER" "$TREE/opt/rust/lib/$DRIVER"
mkdir -p "$TREE/$(dirname "$LLD")"
cp "$STAGE/rustc-nightly-aarch64-unknown-linux-musl/rustc/${LLD#opt/rust/}" "$TREE/$LLD"
STD_LIB="rust-std-nightly-$TARGET/rust-std-$TARGET/$TARGET_LIB"
tar -xJf "$OUT/dl/$STD_TARBALL" -C "$STAGE" "$STD_LIB"
mkdir -p "$TREE/opt/rust/$TARGET_LIB"
cp "$STAGE/$STD_LIB/"*.rlib "$STAGE/$STD_LIB/"*.rmeta "$TREE/opt/rust/$TARGET_LIB/"
rm -rf "$STAGE"

NAME=kumo-persona-rustc-$$
docker run -d --name "$NAME" --platform linux/arm64 "$IMAGE" sleep 300 >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT INT TERM
docker exec "$NAME" apk add --no-cache libgcc=14.2.0-r6 >/dev/null
docker cp -L "$NAME:/usr/lib/libgcc_s.so.1" "$TREE/lib/libgcc_s.so.1"
check
