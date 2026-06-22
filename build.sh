#!/bin/sh
# build.sh — crux's quick deploy loop.
#
#   ./build.sh            # FAST: build the ThinkPad X13s image, open its product dir (drag EFI/ -> USB)
#   ./build.sh all        # build X13s + Raspberry Pi 5 + x86_64 (the old full sweep)
#   KUMO_ESP=/Volumes/ESP ./build.sh   # also copy the EFI tree straight onto a mounted ESP/USB
#
# After `./build.sh`, the X13s image dir opens in Finder; drag its EFI/ folder onto the USB key's
# EFI System Partition, then boot the ThinkPad. (No need to append `open build` anymore.)
set -eu

MODE="${1:-x13s}"
ESP="${KUMO_ESP:-}"

X13S_DIR="build/images/thinkpad-x13s-gen1"

build_qemu() {
    echo "==> Building QEMU native image (virt, aarch64)..."
    cargo xtask image --arch aarch64 --hardware qemu
}

build_x13s() {
    echo "==> Building aarch64 (ThinkPad X13s Gen 1)..."
    cargo xtask image --arch aarch64 --hardware thinkpad-x13s-gen1
}

build_pi5() {
    echo "==> Building aarch64 (Raspberry Pi 5)..."
    cargo xtask image --arch aarch64 --hardware rpi5
    if [ -x "scripts/mk-pi5-img.sh" ]; then
        ./scripts/mk-pi5-img.sh
    fi
}

build_x86() {
    echo "==> Building x86_64 (Generic UEFI)..."
    cargo xtask image --arch x86_64 --hardware generic-uefi-x86_64
}

report() {
    dir="$1"; name="$2"
    kernel="${dir}/EFI/KUMO/kernel/kumo-kernel.elf"
    initrd="${dir}/EFI/KUMO/initrd.img"
    plan="${dir}/kumo-image-plan-${name}.txt"
    echo ""
    echo "==> ${name} build products:"
    if [ -f "$kernel" ]; then
        ls -la "$kernel" "$initrd" 2>/dev/null || echo "    (some files missing)"
        if [ -f "$plan" ]; then
            echo "    kernel fingerprint: $(grep kernel_fingerprint "$plan" 2>/dev/null || echo unknown)"
            echo "    initrd fingerprint: $(grep initrd_fingerprint "$plan" 2>/dev/null || echo unknown)"
        fi
    else
        echo "    not staged: $kernel"
    fi
}

case "$MODE" in
    x13s)
        echo "==> Quick build: ThinkPad X13s only (use './build.sh all' for Pi5 + x86_64)"
        build_x13s
        report "$X13S_DIR" "thinkpad-x13s-gen1"
        OPEN_DIR="$X13S_DIR"
        ;;
    all)
        echo "==> Full sweep: X13s + Pi5 + x86_64 + qemu"
        build_x13s
        build_pi5
        build_x86
	build_qemu
        report "$X13S_DIR" "thinkpad-x13s-gen1"
        report "build/images/generic-uefi-x86_64" "generic-uefi-x86_64"
	report "build/images/qemu-virt-aarch64" "qemu-virt-aarch64"
        OPEN_DIR="build/images"
        ;;
    *)
        echo "usage: ./build.sh [x13s|all]   (default: x13s)" >&2
        exit 2
        ;;
esac

# ---- deploy to a mounted ESP/USB if KUMO_ESP is set ----
if [ -n "$ESP" ]; then
    if ! mountpoint -q "$ESP" 2>/dev/null && [ ! -d "$ESP/EFI" ]; then
        echo "Warning: $ESP does not look like a mounted EFI partition; skipping deploy."
    else
        echo "==> Deploying X13s EFI tree to ESP at $ESP"
        cp -r "$X13S_DIR/EFI/"* "$ESP/EFI/"
        sync
        echo "    Deploy complete."
    fi
fi

# ---- open the product dir so the EFI/ folder is ready to drag onto the USB key ----
if command -v open >/dev/null 2>&1; then
    echo "==> Opening ${OPEN_DIR}..."
    open "$OPEN_DIR"
fi
