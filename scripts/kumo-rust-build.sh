#!/bin/sh
#j857
# SH0 (PLAN/022): build the multi-file, multi-crate sdk-smoke against the prebuilt
# sysroot, with no -Z build-std. The sysroot carries the link contract (start.o,
# link.ld, the kumo shim, relibc's libc.a), so the consumer's whole Rust toolchain
# dependency is `--sysroot` plus those link arguments.
#
#     scripts/kumo-rust-build.sh
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOOLCHAIN="nightly-2026-06-22"
SYSROOT="${KUMO_SYSROOT:-$ROOT/build/sysroot/aarch64-unknown-kumo}"
CRATE="${KUMO_SDK_SMOKE_CRATE:-$ROOT/toolchain/rust/sdk-smoke}"
TARGET_DIR="${KUMO_SDK_SMOKE_TARGET:-$ROOT/target/sdk-smoke}"

fail() { printf 'kumo-rust-build: %s\n' "$1" >&2; exit 1; }

[ -f "$SYSROOT/manifest.json" ] ||
    fail "no sysroot at $SYSROOT; run scripts/rust-sysroot.sh (or: cargo xtask sysroot)"
[ -f "$SYSROOT/share/kumo/libc.a" ] || fail "sysroot lacks share/kumo/libc.a"

export RUSTFLAGS="--sysroot=$SYSROOT \
-C link-arg=-T$SYSROOT/share/kumo/link.ld \
-C link-arg=$SYSROOT/share/kumo/start.o \
-C link-arg=$SYSROOT/share/kumo/libkumo_shim.a \
-C link-arg=$SYSROOT/share/kumo/libc.a \
-C link-arg=-L$SYSROOT/share/kumo \
-C link-arg=-z -C link-arg=muldefs"
export CARGO_TARGET_DIR="$TARGET_DIR"

cd "$CRATE"
cargo +"$TOOLCHAIN" build --release \
    --target "$ROOT/targets/aarch64-unknown-kumo.json" -Zjson-target-spec

out="$TARGET_DIR/aarch64-unknown-kumo/release/sdk-smoke"
[ -f "$out" ] || fail "sdk-smoke binary missing at $out"
printf 'KUMO sdk-smoke: %s\n' "$out"
