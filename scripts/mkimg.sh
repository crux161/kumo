#!/usr/bin/env bash
# mkimg.sh - build a disk-writeable KUMO image product.
#
#   scripts/mkimg.sh arm64 [out.img] [esp_dir]
#   scripts/mkimg.sh amd64 [out.img]
#
# ARM64 is emitted as a raw MBR disk with a type-0xef FAT32 EFI System Partition at
# sector 2048. AMD64 keeps GRUB's ISO-hybrid layout because its Multiboot2 kernel and initrd
# live in the ISO filesystem; that layout already carries MBR/GPT/ESP records for USB boot.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${1:-}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: missing '$1' ($2)" >&2; exit 1; }; }

build_arm64() {
  local out="${1:-$ROOT/build/kumo-arm64.img}"
  local esp_dir="${2:-$ROOT/build/images/thinkpad-x13s-gen1}"
  local work; work="$(mktemp -d)"
  trap "rm -rf '$work'" RETURN

  need dd "install coreutils"
  need mformat "install mtools"
  need mmd "install mtools"
  need mcopy "install mtools"
  need mdir "install mtools"
  need xorriso "install xorriso"
  [ -f "$esp_dir/EFI/BOOT/BOOTAA64.EFI" ] || {
    echo "error: $esp_dir/EFI/BOOT/BOOTAA64.EFI missing" >&2
    exit 1
  }

  local esp="$work/esp.img"
  local iso_root="$work/iso-root"; mkdir -p "$iso_root"
  dd if=/dev/zero of="$esp" bs=1m count=64 status=none
  mformat -i "$esp" -F ::
  copy_tree() { local d name; for d in "$1"/*; do name="$(basename "$d")"
    if [ -d "$d" ]; then mmd -i "$esp" "$2/$name"; copy_tree "$d" "$2/$name"
    else mcopy -i "$esp" "$d" "$2/$name"; fi; done; }
  mmd -i "$esp" ::/EFI; copy_tree "$esp_dir/EFI" "::/EFI"

  xorriso -as mkisofs -V KUMO -iso-level 3 -full-iso9660-filenames \
    -append_partition 2 0xef "$esp" \
    -e --interval:appended_partition_2:all:: -no-emul-boot \
    -isohybrid-gpt-basdat -partition_cyl_align all \
    -o "$work/kumo-arm64-hybrid.iso" "$iso_root" 2>/dev/null

  mkdir -p "$(dirname "$out")"
  cp "$work/kumo-arm64-hybrid.iso" "$out"

  # Preserve the xorriso-generated MBR (sector 0) and appended ESP (from sector 2048),
  # but remove the intervening ISO9660 area so firmware treats this as a disk, not optical
  # media. Clear xorriso's overlapping ISO partition entry; the type-0xef ESP entry remains.
  dd if=/dev/zero of="$out" bs=512 seek=1 count=2047 conv=notrunc status=none
  dd if=/dev/zero of="$out" bs=1 seek=446 count=16 conv=notrunc status=none

  mdir -i "${out}@@1048576" ::/EFI/BOOT/BOOTAA64.EFI >/dev/null
  echo "wrote $out ($(du -h "$out" | cut -f1)) raw MBR + FAT32 ESP"
}

case "$ARCH" in
  aarch64|arm64) build_arm64 "${2:-}" "${3:-}" ;;
  amd64|x86_64|x86-64) exec "$ROOT/scripts/mkiso.sh" amd64 "${2:-$ROOT/build/kumo-amd64.img}" ;;
  *) echo "usage: scripts/mkimg.sh {arm64|amd64} [out.img] [esp_dir]" >&2; exit 2 ;;
esac
