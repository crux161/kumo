#!/usr/bin/env bash
# mk-pi5-img.sh - build a flashable raw SD-card image (.img) for the Raspberry Pi 5 from
# KUMO's staged aarch64 EFI tree. Raspberry Pi Imager ("Use custom") writes a raw .img to
# the card; this produces an MBR + FAT32 image it accepts directly.
#
# IMPORTANT: the Pi 5 has no built-in UEFI. To actually run KUMO's BOOTAA64.EFI you must
# add the community EDK2 RPi5 UEFI firmware (github.com/pftf/RPi5 release zip) to the FAT
# partition. Pass its extracted directory as the 2nd argument and it is overlaid into the
# image; without it the image is a valid FAT card but the Pi firmware has no UEFI to chain
# into. (KUMO cannot redistribute that firmware here.)
#
# macOS only (uses hdiutil + diskutil). Operates solely on the image FILE — it never
# touches a physical disk: the disk identifier comes from attaching our own image.
#
# Usage: scripts/mk-pi5-img.sh [esp_dir] [rpi5_firmware_dir]
#   esp_dir            default build/images/raspberry-pi-5  (run `cargo xtask image
#                      --arch aarch64 --hardware rpi5` first)
#   rpi5_firmware_dir  optional; extracted pftf/RPi5 release (RPI_EFI.fd, *.dtb, config.txt,
#                      overlays/, etc.) overlaid into the FAT partition
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ESP_DIR="${1:-$ROOT/build/images/raspberry-pi-5}"
FW_DIR="${2:-}"
OUT="$ROOT/build/kumo-rpi5.img"

[ "$(uname)" = "Darwin" ] || { echo "this script is macOS-only (hdiutil/diskutil)"; exit 1; }
[ -f "$ESP_DIR/EFI/BOOT/BOOTAA64.EFI" ] || {
  echo "error: $ESP_DIR/EFI/BOOT/BOOTAA64.EFI missing." >&2
  echo "       run: cargo xtask image --arch aarch64 --hardware rpi5" >&2
  exit 1
}

# Size: content + firmware + 64 MiB slack, min 256 MiB (UEFI vars + headroom).
esp_kib="$(du -sk "$ESP_DIR" | cut -f1)"
fw_kib=0
[ -n "$FW_DIR" ] && [ -d "$FW_DIR" ] && fw_kib="$(du -sk "$FW_DIR" | cut -f1)"
mib=$(( (esp_kib + fw_kib + 65536 + 1023) / 1024 )); [ "$mib" -lt 256 ] && mib=256

echo "creating ${mib} MiB raw image: $OUT"
mkdir -p "$(dirname "$OUT")"
dd if=/dev/zero of="$OUT" bs=1m count="$mib" status=none

DISK=""
cleanup() { [ -n "$DISK" ] && hdiutil detach "$DISK" >/dev/null 2>&1 || true; }
trap cleanup EXIT

DISK="$(hdiutil attach -nomount -imagekey diskimage-class=CRawDiskImage "$OUT" | head -1 | awk '{print $1}')"
case "$DISK" in
  /dev/disk[0-9]*) ;;  # sanity: must be an image-backed /dev/diskN
  *) echo "error: unexpected attach result '$DISK'; aborting" >&2; exit 1 ;;
esac
echo "attached image as $DISK"

diskutil partitionDisk "$DISK" MBR "MS-DOS FAT32" KUMO 100% >/dev/null
VOL="/Volumes/KUMO"
[ -d "$VOL" ] || { echo "error: FAT volume did not mount at $VOL" >&2; exit 1; }

# COPYFILE_DISABLE stops macOS writing AppleDouble (._*) sidecar files onto the FAT card.
export COPYFILE_DISABLE=1
cp -R "$ESP_DIR/EFI" "$VOL/"
if [ -n "$FW_DIR" ]; then
  echo "overlaying RPi5 UEFI firmware from $FW_DIR"
  cp -R "$FW_DIR"/* "$VOL/"
fi
# Strip macOS metadata the OS sprinkles onto mounted FAT volumes.
dot_clean -m "$VOL" 2>/dev/null || true
rm -rf "$VOL/.fseventsd" "$VOL/.Spotlight-V100" "$VOL/.Trashes" "$VOL"/._* 2>/dev/null || true
sync

hdiutil detach "$DISK" >/dev/null; DISK=""
echo "wrote $OUT ($(du -h "$OUT" | cut -f1))"
echo
echo "flash it with Raspberry Pi Imager -> 'Use custom' -> $OUT"
[ -z "$FW_DIR" ] && cat <<'NOTE'
NOTE: no firmware overlaid. The Pi 5 will not boot KUMO until the pftf/RPi5 EDK2 UEFI
firmware is on this FAT partition. Either re-run with the extracted firmware dir as arg 2,
or after flashing, copy the pftf release contents onto the card's FAT partition next to EFI/.
NOTE
