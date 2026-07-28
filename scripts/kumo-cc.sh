#!/bin/sh
#j517
# kumo-cc — compile and link a C program for KUMO.
#
# `DESIGN/008 §4` describes this as "the entire C compiler for KUMO": clang pointed at
# the relibc sysroot. No compiler is built. This script exists so the flags live in ONE
# place — in particular `-D__kumo__`, without which every cfg-guarded constant in the
# generated headers offers its Linux and Redox values side by side and selects neither.
#
#     scripts/kumo-cc.sh <output-elf> <source.c> [more.c ...]
#
# The sysroot must already exist; `scripts/relibc-headers.sh` builds it.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SYSROOT="${KUMO_SYSROOT:-$ROOT/target/kumo-sysroot}"
CRT0="$ROOT/toolchain/relibc/smoke/crt0.S"
LINK_SCRIPT="$ROOT/toolchain/relibc/smoke/link.ld"

fail() {
    printf 'kumo-cc: %s\n' "$1" >&2
    exit 1
}

[ "$#" -ge 2 ] || fail "usage: kumo-cc.sh <output-elf> <source.c> [more.c ...]"
out="$1"
shift

[ -d "$SYSROOT/include" ] || fail "no sysroot at $SYSROOT; run scripts/relibc-headers.sh"
[ -f "$SYSROOT/lib/libc.a" ] || fail "no libc.a in $SYSROOT; run scripts/relibc-headers.sh"

cc_bin="${CC_aarch64_unknown_kumo:-$(command -v clang)}"
[ -n "$cc_bin" ] || fail "clang is required"
ld_bin="${LD_aarch64_unknown_kumo:-}"
if [ -z "$ld_bin" ] && command -v ld.lld >/dev/null 2>&1; then
    ld_bin="$(command -v ld.lld)"
fi
if [ -z "$ld_bin" ] && [ -x "$(dirname "$cc_bin")/ld.lld" ]; then
    ld_bin="$(dirname "$cc_bin")/ld.lld"
fi
[ -n "$ld_bin" ] || fail "ld.lld is required"

work="$(dirname "$out")/kumo-cc-obj"
rm -rf "$work"
mkdir -p "$work" "$(dirname "$out")"

# Freestanding and non-PIC: KUMO userland is statically linked at a fixed base by
# `link.ld`, and there is no dynamic loader yet.
CFLAGS="--target=aarch64-unknown-none -D__kumo__ -O2 -ffreestanding \
    -fno-pic -fno-pie -fno-stack-protector -ffunction-sections -fdata-sections \
    --sysroot=$SYSROOT -isystem $SYSROOT/include"

objects=""
# shellcheck disable=SC2086
"$cc_bin" $CFLAGS -c "$CRT0" -o "$work/crt0.o"
objects="$work/crt0.o"
index=0
for source in "$@"; do
    [ -f "$source" ] || fail "no such source: $source"
    index=$((index + 1))
    object="$work/unit$index.o"
    # shellcheck disable=SC2086
    "$cc_bin" $CFLAGS -c "$source" -o "$object"
    objects="$objects $object"
done

# shellcheck disable=SC2086
"$ld_bin" -static --gc-sections -z noexecstack \
    -T "$LINK_SCRIPT" \
    -o "$out" \
    $objects "$SYSROOT/lib/libc.a"
[ -s "$out" ] || fail "link produced an empty image"

printf 'kumo-cc: %s\n' "$out"
