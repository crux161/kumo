#!/usr/bin/env bash
#j439
# x86-multiboot.sh - build the x86_64 KUMO kernel and KUMORD01 initrd as Multiboot
# payloads and (by default) boot them under QEMU's built-in Multiboot loader. These are
# the same payloads that scripts/mkiso.sh installs for GRUB's `multiboot2` and `module2`
# commands.
#
# Why a flat binary + a.out kludge: QEMU's `-kernel` (and GRUB Multiboot1) refuse a
# 64-bit ELF ("give a 32bit one"). The a.out address kludge in the kernel's multiboot
# header lets a flat 64-bit image load at 1 MiB; the kernel's `_start` builds long mode.
#
# Usage: scripts/x86-multiboot.sh [build|run]   (default: run)
#   build  - build the ELF + objcopy to a flat .bin
#   run    - build, then boot under QEMU with serial on stdout
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
MODE="${1:-run}"

ELF="target/x86_64-unknown-none/release/kumo-kernel"
BIN="$ELF.bin"
INITRD="target/x86_64-unknown-none/release/kumo-initrd.img"

# Built-in target, no nightly/build-std (like aarch64-unknown-none).
rustup target list --installed | grep -qx x86_64-unknown-none || rustup target add x86_64-unknown-none

cargo xtask x86-initrd --arch x86_64

cargo build -p kernel --bin kumo-kernel \
  --target x86_64-unknown-none --release \
  --no-default-features --features arch_x86_64

# Flatten the ELF64 -> raw image the Multiboot a.out kludge loads.
OBJCOPY="$(command -v llvm-objcopy || command -v rust-objcopy || command -v gobjcopy || command -v objcopy)"
"$OBJCOPY" -O binary "$ELF" "$BIN"
echo "built $BIN ($(wc -c < "$BIN") bytes)"

if [ "$MODE" = "build" ]; then
  exit 0
fi

echo "booting under QEMU (Ctrl-A X to quit)..."
exec qemu-system-x86_64 -kernel "$BIN" -initrd "$INITRD" -cpu qemu64,+x2apic -m 1088 -display none -no-reboot -serial stdio
