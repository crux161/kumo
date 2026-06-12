#!/bin/sh
set -eu

ESP="${KUMO_ESP:-}"

echo "==> building KUMO images (aarch64 thinkpad-x13s-gen1 + x86_64 generic-uefi-x86_64)"

# ---- aarch64 / ThinkPad X13s ----
cargo xtask image --arch aarch64 --hardware thinkpad-x13s-gen1

AARCH64_DIR="build/images/thinkpad-x13s-gen1"
AARCH64_KERNEL="${AARCH64_DIR}/EFI/KUMO/kernel/kumo-kernel.elf"
AARCH64_INITRD="${AARCH64_DIR}/EFI/KUMO/initrd.img"
AARCH64_PLAN="${AARCH64_DIR}/kumo-image-plan-thinkpad-x13s-gen1.txt"

# ---- x86_64 / generic UEFI ----
cargo xtask image --arch x86_64 --hardware generic-uefi-x86_64

X86_64_DIR="build/images/generic-uefi-x86_64"
X86_64_KERNEL="${X86_64_DIR}/EFI/KUMO/kernel/kumo-kernel.elf"
X86_64_INITRD="${X86_64_DIR}/EFI/KUMO/initrd.img"
X86_64_PLAN="${X86_64_DIR}/kumo-image-plan-generic-uefi-x86_64.txt"

echo ""
echo "==> aarch64 (ThinkPad X13s) build products:"
ls -la "$AARCH64_KERNEL" "$AARCH64_INITRD" 2>/dev/null || echo "    (some files missing)"
if [ -f "$AARCH64_PLAN" ]; then
    echo "    kernel fingerprint: $(grep kernel_fingerprint "$AARCH64_PLAN" || echo unknown)"
    echo "    initrd fingerprint: $(grep initrd_fingerprint "$AARCH64_PLAN" || echo unknown)"
fi

echo ""
echo "==> x86_64 (generic UEFI) build products:"
if [ -f "$X86_64_KERNEL" ]; then
    ls -la "$X86_64_KERNEL" "$X86_64_INITRD" 2>/dev/null || echo "    (some files missing)"
    if [ -f "$X86_64_PLAN" ]; then
        echo "    kernel fingerprint: $(grep kernel_fingerprint "$X86_64_PLAN" || echo unknown)"
        echo "    initrd fingerprint: $(grep initrd_fingerprint "$X86_64_PLAN" || echo unknown)"
    fi
else
    echo "    x86_64 kernel not staged (Sora/initrd aarch64-only; image pipeline WIP)"
    ls "$X86_64_DIR" 2>/dev/null || echo "    (no x86_64 image dir)"
fi

# ---- deploy to ESP ----
if [ -z "$ESP" ]; then
    echo ""
    echo "==> KUMO_ESP not set — skipping deploy"
    echo "    set KUMO_ESP=/path/to/esp to copy kernel + initrd, e.g.:"
    echo "    KUMO_ESP=/mnt/esp ./build.sh"
    exit 0
fi

if ! mountpoint -q "$ESP" 2>/dev/null; then
    echo "==> WARNING: $ESP does not appear to be a mountpoint"
fi

ESP_KUMO="${ESP}/EFI/KUMO"
mkdir -p "$ESP_KUMO/kernel"

if [ -f "$AARCH64_KERNEL" ]; then
    echo "==> deploying aarch64 to $ESP_KUMO"
    cp -v "$AARCH64_KERNEL" "$ESP_KUMO/kernel/kumo-kernel.elf"
    cp -v "$AARCH64_INITRD" "$ESP_KUMO/initrd.img"
    cp -v "$AARCH64_PLAN" "$ESP_KUMO/kumo-image-plan-aarch64.txt"
fi

echo "==> deploy complete — safe to unmount $ESP and reboot"
