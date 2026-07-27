#!/bin/sh
#j505
# Compile KUMO's out-of-tree relibc platform scaffold without touching the
# pinned upstream checkout. Runtime syscall/TLS behavior is deliberately absent.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RELIBC="$ROOT/vendor/relibc"
TOOL="$ROOT/toolchain/relibc"
TARGET="$ROOT/targets/aarch64-unknown-kumo.json"
TOOLCHAIN="nightly-2026-06-22"
RELIBC_REV="5f6afb52692e62dae79154f4dbec2d0e79a07602"
DLMALLOC_REV="40c10f6182cb1d2a78b29f85492936dfbeded02e"
OPENLIBM_REV="f5ed774593458e116e69731a3ffa59e2a6408d4e"
RUSTC_REV="91fe22da8084a1c9e993d78d4a56f22ab8396236"
RELIBC_LOCK_SHA256="3702686153d3152537524a1c89d41bfd44de33a5149c195b2ada2f160a406f56"
KUMO_LOCK_SHA256="15797bdbab1fd89a4dc821300663620e1a2712a8213b7b10beff19faf55f3da4"

fail() {
    printf 'relibc-check: %s\n' "$1" >&2
    exit 1
}

command -v git >/dev/null 2>&1 || fail "git is required"
command -v cargo >/dev/null 2>&1 || fail "cargo is required"
command -v clang >/dev/null 2>&1 || fail "clang is required"
command -v shasum >/dev/null 2>&1 || fail "shasum is required"

[ -f "$RELIBC/Cargo.toml" ] ||
    fail "source missing; run: git submodule update --init --recursive vendor/relibc"
[ -f "$RELIBC/dlmalloc-rs/Cargo.toml" ] ||
    fail "dlmalloc missing; run: git submodule update --init --recursive vendor/relibc"
[ -f "$RELIBC/openlibm/LICENSE.md" ] ||
    fail "openlibm missing; run: git submodule update --init --recursive vendor/relibc"

[ "$(git -C "$RELIBC" rev-parse HEAD)" = "$RELIBC_REV" ] ||
    fail "relibc checkout does not match SOURCE.lock"
[ "$(git -C "$RELIBC/dlmalloc-rs" rev-parse HEAD)" = "$DLMALLOC_REV" ] ||
    fail "dlmalloc checkout does not match SOURCE.lock"
[ "$(git -C "$RELIBC/openlibm" rev-parse HEAD)" = "$OPENLIBM_REV" ] ||
    fail "openlibm checkout does not match SOURCE.lock"
[ "$(shasum -a 256 "$RELIBC/Cargo.lock" | awk '{ print $1 }')" = \
    "$RELIBC_LOCK_SHA256" ] || fail "upstream Cargo.lock does not match SOURCE.lock"
[ "$(shasum -a 256 "$TOOL/overlay/Cargo.lock" | awk '{ print $1 }')" = \
    "$KUMO_LOCK_SHA256" ] || fail "KUMO Cargo.lock does not match SOURCE.lock"

rustc_commit="$(rustc +"$TOOLCHAIN" --version --verbose |
    awk '/^commit-hash:/ { print $2 }')"
[ "$rustc_commit" = "$RUSTC_REV" ] ||
    fail "rustc +$TOOLCHAIN is missing or has the wrong commit"

sysroot="$(rustc +"$TOOLCHAIN" --print sysroot)"
rust_vendor="$sysroot/lib/rustlib/src/rust/library/vendor"
[ -d "$rust_vendor" ] ||
    fail "rust-src is missing; install it for $TOOLCHAIN"

method_count="$(grep -Ec '^    (unsafe )?fn ' \
    "$TOOL/overlay/src/platform/kumo.rs")"
[ "$method_count" -eq 129 ] ||
    fail "KUMO backend has $method_count methods; expected 129"

mkdir -p "$ROOT/target"
stage="$(mktemp -d "$ROOT/target/relibc-source.XXXXXX")"
vendor_union="$(mktemp -d "$ROOT/target/relibc-vendor.XXXXXX")"
cleanup() {
    if [ "${KUMO_RELIBC_KEEP_STAGE:-0}" = "1" ]; then
        printf 'relibc-check: kept stage %s and vendor union %s\n' \
            "$stage" "$vendor_union" >&2
        return
    fi
    rm -rf "$stage" "$vendor_union"
}
trap cleanup EXIT HUP INT TERM

git -C "$RELIBC" archive "$RELIBC_REV" | tar -x -C "$stage"
mkdir -p "$stage/dlmalloc-rs"
git -C "$RELIBC/dlmalloc-rs" archive "$DLMALLOC_REV" |
    tar -x -C "$stage/dlmalloc-rs"
mkdir -p "$stage/kumo-deps"
cp -R "$TOOL/vendor/rand_jitter-0.6.1" "$stage/kumo-deps/rand_jitter"
cp -R "$TOOL/overlay/." "$stage/"
stage_rel="${stage#"$ROOT"/}"
git -C "$ROOT" apply --directory="$stage_rel" "$TOOL/kumo.patch"
git -C "$ROOT" apply --directory="$stage_rel/dlmalloc-rs" \
    "$TOOL/dlmalloc-kumo.patch"
git -C "$ROOT" apply --directory="$stage_rel/kumo-deps/rand_jitter" \
    "$TOOL/rand-jitter-kumo.patch"

for source_dir in "$TOOL/vendor" "$ROOT/vendor/cargo" "$rust_vendor"; do
    for crate_dir in "$source_dir"/*; do
        name="${crate_dir##*/}"
        [ -e "$vendor_union/$name" ] || ln -s "$crate_dir" "$vendor_union/$name"
    done
done

cc_bin="${CC_aarch64_unknown_kumo:-$(command -v clang)}"
ar_bin="${AR_aarch64_unknown_kumo:-}"
if [ -z "$ar_bin" ] && command -v llvm-ar >/dev/null 2>&1; then
    ar_bin="$(command -v llvm-ar)"
fi

set -- env \
    CC_aarch64_unknown_kumo="$cc_bin" \
    CFLAGS_aarch64_unknown_kumo="--target=aarch64-unknown-none" \
    CARGO_TARGET_DIR="$ROOT/target/relibc-kumo"
if [ -n "$ar_bin" ]; then
    set -- "$@" AR_aarch64_unknown_kumo="$ar_bin"
fi

"$@" cargo +"$TOOLCHAIN" \
    --config "$TOOL/cargo-sources.toml" \
    --config "source.vendored-sources.directory=\"$vendor_union\"" \
    check \
    --locked \
    --offline \
    -Z json-target-spec \
    -Z build-std=core,alloc \
    --target "$TARGET" \
    --manifest-path "$stage/Cargo.toml" \
    --no-default-features \
    --features no_trace \
    --lib

printf 'KUMO relibc scaffold green: %s methods, target_os=kumo, source %s\n' \
    "$method_count" "$RELIBC_REV"
