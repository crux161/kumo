#!/bin/sh
set -eu

ESP="${KUMO_ESP:-}"

echo "==> building KUMO images (aarch64 thinkpad-x13s-gen1, aarch64 pi5 + x86_64 generic-uefi-x86_64)"

# ---- aarch64 / ThinkPad X13s ----
echo "==> [1/3] Building aarch64 (ThinkPad X13s)..."
cargo xtask image --arch aarch64 --hardware thinkpad-x13s-gen1

AARCH64_DIR="build/images/thinkpad-x13s-gen1"
AARCH64_KERNEL="${AARCH64_DIR}/EFI/KUMO/kernel/kumo-kernel.elf"
AARCH64_INITRD="${AARCH64_DIR}/EFI/KUMO/initrd.img"
AARCH64_PLAN="${AARCH64_DIR}/kumo-image-plan-thinkpad-x13s-gen1.txt"

# ---- aarch64 / Raspberry Pi 5 ----
echo "==> [2/3] Building aarch64 (Raspberry Pi 5)..."
if [ -x "scripts/mk-pi5-img.sh" ]; then
    ./scripts/mk-pi5-img.sh
else
    # Fallback to xtask if the script isn't executable or present
    cargo xtask image --arch aarch64 --hardware bcm2712-rpi5
fi

# ---- x86_64 / generic UEFI ----
echo "==> [3/3] Building x86_64 (Generic UEFI)..."
cargo xtask image --arch x86_64 --hardware generic-uefi-x86_64

X86_64_DIR="build/images/generic-uefi-x86_64"
X86_64_KERNEL="${X86_64_DIR}/EFI/KUMO/kernel/kumo-kernel.elf"
X86_64_INITRD="${X86_64_DIR}/EFI/KUMO/initrd.img"
X86_64_PLAN="${X86_64_DIR}/kumo-image-plan-generic-uefi-x86_64.txt"

echo ""
echo "==> aarch64 (ThinkPad X13s) build products:"
if [ -f "$AARCH64_KERNEL" ]; then
    ls -la "$AARCH64_KERNEL" "$AARCH64_INITRD" 2>/dev/null || echo "    (some files missing)"
    if [ -f "$AARCH64_PLAN" ]; then
        echo "    kernel fingerprint: $(grep kernel_fingerprint "$AARCH64_PLAN" || echo unknown)"
        echo "    initrd fingerprint: $(grep initrd_fingerprint "$AARCH64_PLAN" || echo unknown)"
    fi
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
if [ -n "$ESP" ]; then
    if ! mountpoint -q "$ESP" 2>/dev/null && [ ! -d "$ESP/EFI" ]; then
        echo "Warning: $ESP does not look like a mounted EFI partition."
    else
        echo "==> Deploying to ESP at $ESP"
        cp -r "$AARCH64_DIR/EFI/"* "$ESP/EFI/"
        sync
        echo "    Deploy complete."
    fi
fi

# ---- Open output directory ----
if command -v open >/dev/null 2>&1; then
    echo "==> Opening build/images..."
    open build/images
fi
