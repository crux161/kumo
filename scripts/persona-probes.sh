#!/bin/sh
#j915
# Build the header-free Linux syscall probe with the installed cross clang and pinned rust-lld.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
HOST=$(rustc -vV | sed -n 's/^host: //p')
LLD="$(rustc --print sysroot)/lib/rustlib/$HOST/bin/rust-lld"
OUT="$ROOT/build/persona-probes"
mkdir -p "$OUT"
clang --target=aarch64-linux-musl -O2 -ffreestanding -fno-builtin -fno-stack-protector \
    -c "$ROOT/persona/pipeprobe.c" -o "$OUT/pipeprobe.o"
"$LLD" -flavor gnu -static --build-id=none --entry=_start -z max-page-size=4096 \
    -Ttext=0x10000000 -o "$ROOT/persona/pipeprobe" "$OUT/pipeprobe.o"
shasum -a 256 "$ROOT/persona/pipeprobe"
