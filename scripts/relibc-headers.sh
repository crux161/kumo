#!/bin/sh
#j511
# Build KUMO's C sysroot: relibc's installed headers plus its static archive.
#
# relibc generates most of its headers from Rust with cbindgen. That tool's whole
# dependency closure is vendored under `toolchain/cbindgen/`, so this runs with no network
# and no globally installed binary — the same offline guarantee `relibc-check.sh` makes.
#
# The headers are generated from the *staged, KUMO-patched* tree, not the pristine one.
# That matters: `kumo.patch` admits `target_os = "kumo"` alongside Linux in the header
# modules, and adds the `__kumo__` cbindgen define, so a constant that differs between
# platforms is emitted under `#if defined(__linux__) || defined(__kumo__)` rather than
# twice with no guard. Compile against this sysroot with `-D__kumo__`.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOOL="$ROOT/toolchain/relibc"
CBINDGEN_DIR="$ROOT/toolchain/cbindgen"
CBINDGEN_VERSION="0.29.2"
STAGE="$ROOT/target/relibc-stage"
SYSROOT="$ROOT/target/kumo-sysroot"
CBINDGEN_ROOT="$ROOT/target/cbindgen-host"
CBINDGEN="$CBINDGEN_ROOT/bin/cbindgen"

fail() {
    printf 'relibc-headers: %s\n' "$1" >&2
    exit 1
}

CBINDGEN_LOCK_SHA256="bd9d7fb419c960466093fb4e50126b8a258d5805640c03ae80cc5a753b4f2ed1"

command -v cargo >/dev/null 2>&1 || fail "cargo is required"
command -v shasum >/dev/null 2>&1 || fail "shasum is required"
[ -d "$CBINDGEN_DIR/vendor" ] ||
    fail "toolchain/cbindgen/vendor is missing; the header generator cannot be built offline"
[ "$(shasum -a 256 "$CBINDGEN_DIR/Cargo.lock" | awk '{ print $1 }')" = \
    "$CBINDGEN_LOCK_SHA256" ] || fail "cbindgen Cargo.lock does not match SOURCE.lock"

# Build the generator once. Its version is pinned, so an existing binary of the right
# version is reused rather than rebuilt on every image.
if ! "$CBINDGEN" --version 2>/dev/null | grep -q "^cbindgen $CBINDGEN_VERSION\$"; then
    cargo install cbindgen \
        --version "$CBINDGEN_VERSION" \
        --offline \
        --root "$CBINDGEN_ROOT" \
        --config "$CBINDGEN_DIR/cargo-sources.toml" \
        --config "source.vendored-sources.directory=\"$CBINDGEN_DIR/vendor\"" \
        >/dev/null 2>&1 ||
        fail "building the vendored cbindgen failed"
fi
"$CBINDGEN" --version >/dev/null 2>&1 || fail "the vendored cbindgen did not produce a binary"

# Stage the KUMO-patched tree and build the archive. `relibc-check.sh` owns every source
# and toolchain identity check, so this script inherits them rather than repeating them.
KUMO_RELIBC_STAGE_OUT="$STAGE" "$ROOT/scripts/relibc-check.sh" >/dev/null ||
    fail "relibc-check.sh failed; the sysroot has nothing to install"

archive="$ROOT/target/relibc-kumo/aarch64-unknown-kumo/release/librelibc.a"
[ -f "$archive" ] || fail "librelibc.a was not produced"

rm -rf "$SYSROOT"
mkdir -p "$SYSROOT/include" "$SYSROOT/lib"
cp "$archive" "$SYSROOT/lib/libc.a"
cp -R "$STAGE/include/." "$SYSROOT/include/"
# relibc links openlibm for the math surface; its headers ship beside it.
cp "$STAGE/openlibm/include"/*.h "$SYSROOT/include/" 2>/dev/null || true
cp "$STAGE/openlibm/src"/*.h "$SYSROOT/include/" 2>/dev/null || true

generated=0
failed=0
for dir in "$STAGE"/src/header/*/; do
    header="$(basename "$dir")"
    case "$header" in
        _*) continue ;;
    esac
    [ -f "$dir/cbindgen.toml" ] || continue
    # `sys_ioctl` becomes `sys/ioctl.h`, matching the Makefile's own `_`-to-`/` rule.
    out="$SYSROOT/include/$(printf '%s' "$header" | tr '_' '/').h"
    mkdir -p "$(dirname "$out")"
    if cat "$dir/cbindgen.toml" "$STAGE/cbindgen.globdefs.toml" |
        "$CBINDGEN" "$dir/mod.rs" --config=/dev/stdin --output "$out" 2>/dev/null; then
        generated=$((generated + 1))
    else
        failed=$((failed + 1))
        printf 'relibc-headers: could not generate %s\n' "$out" >&2
        rm -f "$out"
    fi
done

[ "$failed" -eq 0 ] || fail "$failed header(s) failed to generate"
[ -f "$SYSROOT/include/stdio.h" ] || fail "stdio.h is missing from the sysroot"

# Link the C smoke against the sysroot it just produced. This lives here rather than in
# `relibc-check.sh` because the smoke now `#include`s the generated headers: the archive
# alone is no longer enough to build it.
cc_bin="${CC_aarch64_unknown_kumo:-$(command -v clang)}"
ld_bin="${LD_aarch64_unknown_kumo:-}"
if [ -z "$ld_bin" ] && command -v ld.lld >/dev/null 2>&1; then
    ld_bin="$(command -v ld.lld)"
fi
if [ -z "$ld_bin" ] && [ -x "$(dirname "$cc_bin")/ld.lld" ]; then
    ld_bin="$(dirname "$cc_bin")/ld.lld"
fi
[ -n "$ld_bin" ] || fail "ld.lld is required"

smoke_dir="$ROOT/target/relibc-smoke"
smoke_elf="$smoke_dir/relibc-smoke"
mkdir -p "$smoke_dir"
# `-D__kumo__` is what selects the KUMO arm of every cfg-guarded constant the generator
# emitted. Without it a header offers the Linux and Redox values side by side and neither
# is chosen — see this script's header comment.
cflags="--target=aarch64-unknown-none -D__kumo__ -O2 -ffreestanding \
    -fno-pic -fno-pie -fno-stack-protector -ffunction-sections -fdata-sections \
    --sysroot=$SYSROOT -isystem $SYSROOT/include"
# shellcheck disable=SC2086
"$cc_bin" $cflags -c "$TOOL/smoke/crt0.S" -o "$smoke_dir/crt0.o"
# shellcheck disable=SC2086
"$cc_bin" $cflags -c "$TOOL/smoke/main.c" -o "$smoke_dir/main.o"
"$ld_bin" -static --gc-sections -z noexecstack \
    -T "$TOOL/smoke/link.ld" \
    -o "$smoke_elf" \
    "$smoke_dir/crt0.o" "$smoke_dir/main.o" "$SYSROOT/lib/libc.a"
[ -s "$smoke_elf" ] || fail "relibc smoke ELF is empty"

printf 'KUMO sysroot: %s (%s generated headers, %s total)\n' \
    "$SYSROOT" "$generated" "$(find "$SYSROOT/include" -name '*.h' | wc -l | tr -d ' ')"
printf 'KUMO relibc I-a3 image: %s\n' "$smoke_elf"
