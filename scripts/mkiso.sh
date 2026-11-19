#!/usr/bin/env bash
#j909
# mkiso.sh - build a bootable KUMO ISO product: a UEFI El Torito ISO wrapping a staged
# Nijigumo EFI/ tree (the same tree `cargo xtask image` stages for the ESP).
#
#   scripts/mkiso.sh amd64   [out.iso] [esp_dir]   (default; esp_dir build/images/generic-uefi-x86_64)
#   scripts/mkiso.sh aarch64 [out.iso] [esp_dir]   (esp_dir build/images/thinkpad-x13s-gen1)
#
# Both architectures boot Nijigumo (BOOTX64.EFI / BOOTAA64.EFI), which loads EFI/KUMO/* and hands
# KUMO a BootInfo. j909 (PLAN/025 X4) retired the amd64 GRUB rescue ISO: the NUC proved Nijigumo
# on metal, and only Nijigumo hands the x86 kernel the GOP framebuffer. GRUB remains only as the
# legacy-BIOS netboot program (scripts/refresh-netboot.sh, core.0).
# Dd an ISO to a USB key, or boot it in QEMU with OVMF/AAVMF (`cargo xtask x86-uefi-smoke`).
# Needs: mtools, xorriso (macOS: brew install mtools xorriso).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${1:-amd64}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: missing '$1' ($2)" >&2; exit 1; }; }

# build_esp_iso <loader file name> <arch label> <out.iso> <esp_dir> <image command hint>
build_esp_iso() {
  local loader="$1" label="$2" out="$3" esp_dir="$4" hint="$5"
  need xorriso "brew install xorriso"
  need mformat "brew install mtools"
  [ -f "$esp_dir/EFI/BOOT/$loader" ] || {
    echo "error: $esp_dir/EFI/BOOT/$loader missing (run: $hint)" >&2
    exit 1
  }
  local work; work="$(mktemp -d)"; trap "rm -rf '$work'" RETURN
  local iso_root="$work/iso-root"; mkdir -p "$iso_root"
  local esp="$work/esp.img"
  local kib mib
  # El Torito stores its 512-byte load count in 16 bits, so its EFI image must stay below
  # 32 MiB. FAT16 is valid UEFI removable-media storage at this size; the raw IMG builder
  # uses a separate 64 MiB standards-compliant FAT32 ESP.
  kib=$(( $(du -sk "$esp_dir" | cut -f1) + 4096 )); mib=$(( (kib + 1023) / 1024 )); [ "$mib" -lt 16 ] && mib=16
  [ "$mib" -lt 32 ] || { echo "error: $label ISO EFI payload exceeds the 31 MiB El Torito limit" >&2; exit 1; }
  dd if=/dev/zero of="$esp" bs=1m count="$mib" status=none
  mformat -i "$esp" ::
  copy_tree() { local d name; for d in "$1"/*; do name="$(basename "$d")"
    if [ -d "$d" ]; then mmd -i "$esp" "$2/$name"; copy_tree "$d" "$2/$name"
    else mcopy -i "$esp" "$d" "$2/$name"; fi; done; }
  mmd -i "$esp" ::/EFI; copy_tree "$esp_dir/EFI" "::/EFI"
  mkdir -p "$(dirname "$out")"
  # Use the FAT16 image both as the UEFI El Torito image and as an appended type-0xef
  # partition, keeping the optical product inspectable with ordinary disk tooling.
  xorriso -as mkisofs -V KUMO -iso-level 3 -full-iso9660-filenames \
    -append_partition 2 0xef "$esp" \
    -e --interval:appended_partition_2:all:: -no-emul-boot \
    -isohybrid-gpt-basdat -partition_cyl_align all \
    -o "$out" "$iso_root" 2>/dev/null
  echo "wrote $out ($(du -h "$out" | cut -f1)) $label UEFI ISO (Nijigumo)"
}

build_amd64() {
  build_esp_iso BOOTX64.EFI amd64 "${1:-$ROOT/build/kumo-amd64.iso}" \
    "${2:-$ROOT/build/images/generic-uefi-x86_64}" \
    "cargo xtask image --arch x86_64 --hardware generic-uefi-x86_64"
  echo "  - verified QEMU path:   cargo xtask x86-uefi-smoke"
  echo "  - or dd to a USB key:   sudo dd if=${1:-$ROOT/build/kumo-amd64.iso} of=/dev/rdiskN bs=4m   (then boot it, Secure Boot off)"
}

build_aarch64() {
  build_esp_iso BOOTAA64.EFI aarch64 "${1:-$ROOT/build/kumo-arm64.iso}" \
    "${2:-$ROOT/build/images/thinkpad-x13s-gen1}" \
    "cargo xtask image --arch aarch64 --hardware x13s"
}

case "$ARCH" in
  amd64|x86_64|x86-64) build_amd64 "${2:-}" "${3:-}" ;;
  aarch64|arm64)       build_aarch64 "${2:-}" "${3:-}" ;;
  *) echo "usage: scripts/mkiso.sh {amd64|aarch64} [out.iso] [esp_dir]" >&2; exit 1 ;;
esac
