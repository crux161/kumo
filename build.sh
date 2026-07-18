#!/bin/sh
#j213
#j330
#j467

# build.sh — quick staging, deploy, and boot-media entry point.
#
# Boot-media products:
#   ./build.sh arm64 iso    -> build/kumo-arm64.iso
#   ./build.sh arm64 img    -> build/kumo-arm64.img
#   ./build.sh amd64 iso    -> build/kumo-amd64.iso
#   ./build.sh amd64 img    -> build/kumo-amd64.img
#
# `.iso` selects UEFI optical media. `.img` selects disk-writeable media: a raw MBR/FAT32
# ESP on ARM64 and GRUB's USB-bootable ISO-hybrid layout on AMD64.
#
# Legacy shortcuts remain available:
#   ./build.sh              -> stage the ThinkPad X13s EFI tree
#   ./build.sh x13s         -> same as above
#   ./build.sh all          -> stage every existing architecture/hardware profile
#
# KUMO_ESP=/Volumes/ESP ./build.sh x13s copies the staged X13s EFI tree to a mounted ESP.
# KUMO_PI5_CONSOLE_UART=pl011@0x1c00030000 ./build.sh all explicitly routes the Pi 5 serial sink.
# KUMO_NO_OPEN=1 suppresses opening the output directory on macOS.
set -eu

MODE="${1:-x13s}"
PRODUCT="${2:-}"
ESP="${KUMO_ESP:-}"
PI5_CONSOLE_UART="${KUMO_PI5_CONSOLE_UART:-}"
OPEN_DIR=""
DEPLOY_DIR=""

X13S_DIR="build/images/thinkpad-x13s-gen1"

usage() {
    cat >&2 <<'USAGE'
usage:
  ./build.sh <arm64|amd64> <iso|img>
  ./build.sh [x13s|all]

examples:
  ./build.sh arm64 img
  ./build.sh amd64 iso
  KUMO_PI5_CONSOLE_UART=pl011@0x1c00030000 ./build.sh all
USAGE
}

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
    if [ -n "$PI5_CONSOLE_UART" ]; then
        echo "    explicit serial route: $PI5_CONSOLE_UART"
        cargo xtask image --arch aarch64 --hardware rpi5 --console-uart "$PI5_CONSOLE_UART"
    else
        cargo xtask image --arch aarch64 --hardware rpi5
    fi
    if [ -x "scripts/mk-pi5-img.sh" ]; then
        ./scripts/mk-pi5-img.sh
    fi
}

build_x86() {
    echo "==> Building x86_64 (Generic UEFI staging tree)..."
    cargo xtask image --arch x86_64 --hardware generic-uefi-x86_64
}

report() {
    dir="$1"
    name="$2"
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

report_media() {
    artifact="$1"
    if [ ! -s "$artifact" ]; then
        echo "error: media builder did not produce $artifact" >&2
        exit 1
    fi
    echo ""
    echo "==> Boot-media product:"
    ls -lh "$artifact"
}

build_media() {
    arch="$1"
    format="$2"

    case "$format" in
        iso|img) ;;
        *)
            echo "error: unknown media format '$format' (expected iso or img)" >&2
            usage
            exit 2
            ;;
    esac

    case "$arch" in
        arm64|aarch64)
            artifact="build/kumo-arm64.${format}"
            build_x13s
            if [ "$format" = "iso" ]; then
                echo "==> Building ARM64 UEFI ISO: $artifact"
                ./scripts/mkiso.sh aarch64 "$artifact" "$X13S_DIR"
            else
                echo "==> Building ARM64 raw MBR/FAT32 image: $artifact"
                ./scripts/mkimg.sh arm64 "$artifact" "$X13S_DIR"
            fi
            DEPLOY_DIR="$X13S_DIR"
            ;;
        amd64|x86_64|x86-64)
            artifact="build/kumo-amd64.${format}"
            if [ "$format" = "iso" ]; then
                echo "==> Building AMD64 GRUB/Multiboot2 ISO: $artifact"
                ./scripts/mkiso.sh amd64 "$artifact"
            else
                echo "==> Building AMD64 GRUB/Multiboot2 hybrid image: $artifact"
                ./scripts/mkimg.sh amd64 "$artifact"
            fi
            ;;
        *)
            echo "error: unknown architecture '$arch' (expected arm64 or amd64)" >&2
            usage
            exit 2
            ;;
    esac

    report_media "$artifact"
    OPEN_DIR="build"
}

if [ "$#" -gt 2 ]; then
    usage
    exit 2
fi

case "$MODE" in
    arm64|aarch64|amd64|x86_64|x86-64)
        if [ -z "$PRODUCT" ]; then
            echo "error: architecture builds require an iso or img product" >&2
            usage
            exit 2
        fi
        build_media "$MODE" "$PRODUCT"
        ;;
    x13s)
        if [ -n "$PRODUCT" ]; then
            usage
            exit 2
        fi
        echo "==> Quick build: ThinkPad X13s only (use './build.sh all' for the full sweep)"
        build_x13s
        report "$X13S_DIR" "thinkpad-x13s-gen1"
        OPEN_DIR="$X13S_DIR"
        DEPLOY_DIR="$X13S_DIR"
        ;;
    all)
        if [ -n "$PRODUCT" ]; then
            usage
            exit 2
        fi
        echo "==> Full sweep: X13s + Pi5 + x86_64 + QEMU"
        build_x13s
        build_pi5
        build_x86
        build_qemu
        report "$X13S_DIR" "thinkpad-x13s-gen1"
        report "build/images/generic-uefi-x86_64" "generic-uefi-x86_64"
        report "build/images/qemu-virt-aarch64" "qemu-virt-aarch64"
        OPEN_DIR="build/images"
        DEPLOY_DIR="$X13S_DIR"
        ;;
    *)
        usage
        exit 2
        ;;
esac

# Deploy a staged EFI tree to a mounted ESP/USB when requested. Media-only AMD64 builds do
# not expose a persistent staging directory, so they deliberately skip this convenience path.
if [ -n "$ESP" ]; then
    if [ -z "$DEPLOY_DIR" ]; then
        echo "Warning: this build has no staged EFI tree to deploy; use the generated media."
    elif ! mountpoint -q "$ESP" 2>/dev/null && [ ! -d "$ESP/EFI" ]; then
        echo "Warning: $ESP does not look like a mounted EFI partition; skipping deploy."
    else
        echo "==> Deploying EFI tree from $DEPLOY_DIR to ESP at $ESP"
        cp -r "$DEPLOY_DIR/EFI/"* "$ESP/EFI/"
        sync
        echo "    Deploy complete."
    fi
fi

if [ "${KUMO_NO_OPEN:-0}" != "1" ] && command -v open >/dev/null 2>&1; then
    echo "==> Opening ${OPEN_DIR}..."
    open "$OPEN_DIR"
fi
