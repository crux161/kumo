#!/bin/sh
set -eu

HARDWARE="${1:-thinkpad-x13s-gen1}"
ARCH="${2:-aarch64}"
ESP="${KUMO_ESP:-}"

echo "==> building KUMO image (arch=$ARCH hardware=$HARDWARE)"
cargo xtask image --arch "$ARCH" --hardware "$HARDWARE"

IMAGE_DIR="build/images/${HARDWARE}"
KERNEL_SRC="${IMAGE_DIR}/EFI/KUMO/kernel/kumo-kernel.elf"
INITRD_SRC="${IMAGE_DIR}/EFI/KUMO/initrd.img"
PLAN_SRC="${IMAGE_DIR}/kumo-image-plan-${HARDWARE}.txt"

echo "==> build products:"
ls -la "$KERNEL_SRC" "$INITRD_SRC"

if [ -z "$ESP" ]; then
    echo "==> KUMO_ESP not set — skipping deploy"
    echo "    set KUMO_ESP=/path/to/esp to copy kernel + initrd, e.g.:"
    echo "    KUMO_ESP=/mnt/esp ./build.sh"
    exit 0
fi

if ! mountpoint -q "$ESP" 2>/dev/null; then
    echo "==> WARNING: $ESP does not appear to be a mountpoint"
    echo "    deploy may fail if the path is not accessible"
fi

ESP_KUMO="${ESP}/EFI/KUMO"
mkdir -p "$ESP_KUMO/kernel"

echo "==> deploying to $ESP_KUMO"
cp -v "$KERNEL_SRC" "$ESP_KUMO/kernel/kumo-kernel.elf"
cp -v "$INITRD_SRC" "$ESP_KUMO/initrd.img"
cp -v "$PLAN_SRC" "$ESP_KUMO/"

echo "==> kernel fingerprint: $(grep kernel_fingerprint "$PLAN_SRC" || echo unknown)"
echo "==> initrd fingerprint: $(grep initrd_fingerprint "$PLAN_SRC" || echo unknown)"
echo "==> deploy complete — safe to unmount $ESP and reboot"
