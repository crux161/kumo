#!/usr/bin/env bash
# mkiso.sh - build a bootable KUMO ISO product.
#
#   scripts/mkiso.sh amd64   [out.iso]            (default)
#   scripts/mkiso.sh aarch64 [out.iso] [esp_dir]
#
# amd64:  a UEFI-bootable GRUB rescue ISO. GRUB loads the x86_64 KUMO kernel via
#         Multiboot2 (the kernel ships as a flat 64-bit image with an explicit
#         Multiboot2 address tag). Boot it on real
#         amd64 hardware (dd to a USB key) or in QEMU with OVMF. This is the
#         unambiguous "does KUMO boot via GRUB" pathway.
#         Needs: x86_64-elf-grub (mkrescue), mtools, xorriso.
#           macOS:  brew install x86_64-elf-grub mtools xorriso
#
# aarch64: a UEFI El Torito ISO wrapping the staged EFI/ tree (Nijigumo bootloader).
#         Needs: mtools, xorriso.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${1:-amd64}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: missing '$1' ($2)" >&2; exit 1; }; }

build_amd64() {
  local out="${1:-$ROOT/build/kumo-amd64.iso}"
  local mkrescue
  mkrescue="$(command -v x86_64-elf-grub-mkrescue || command -v grub-mkrescue || true)"
  [ -n "$mkrescue" ] || { echo "error: need x86_64-elf-grub-mkrescue (brew install x86_64-elf-grub)" >&2; exit 1; }
  need mformat "brew install mtools"
  need xorriso "brew install xorriso"

  # 1. Build the x86_64 Multiboot kernel, native-user initrd, and flattened image.
  ( cd "$ROOT" && scripts/x86-multiboot.sh build )
  local elf="$ROOT/target/x86_64-unknown-none/release/kumo-kernel"
  local initrd="$ROOT/target/x86_64-unknown-none/release/kumo-initrd.img"

  # 2. Stage the ISO root: kernel + initrd module + a GRUB Multiboot2 menu entry.
  local work; work="$(mktemp -d)"
  trap "rm -rf '$work'" RETURN
  mkdir -p "$work/boot/grub"
  cp "$elf.bin" "$work/boot/kumo-kernel"
  cp "$initrd" "$work/boot/kumo-initrd.img"
  cat > "$work/boot/grub/grub.cfg" <<'CFG'
set timeout=3
set default=0
# Mirror GRUB's console to serial too, so a headless boot still shows the menu.
serial --unit=0 --speed=115200
terminal_input console serial
terminal_output console serial
menuentry "KUMO (Ziwei) x86_64 - Multiboot" {
    insmod multiboot2
    insmod mmap
    # A Multiboot2 module is allowed at physical zero, but Rust references may not use a
    # null data pointer. Keep the null page out of GRUB's module allocator; KUMO applies its
    # broader legacy-megabyte reservation when it normalizes the firmware map.
    cutmem 0 4K
    set gfxpayload=text
    multiboot2 /boot/kumo-kernel
    module2 /boot/kumo-initrd.img kumo-initrd
    boot
}
CFG

  # 3. Build the UEFI-bootable rescue ISO.
  mkdir -p "$(dirname "$out")"
  "$mkrescue" -o "$out" "$work" >/dev/null 2>&1
  echo "wrote $out ($(du -h "$out" | cut -f1))"
  echo "  - verified QEMU path:   cargo xtask x86-uefi-smoke --arch x86_64"
  echo "  - or dd to a USB key:   sudo dd if=$out of=/dev/rdiskN bs=4m   (then boot it)"
}

build_aarch64() {
  local out="${1:-$ROOT/build/kumo-arm64.iso}"
  local esp_dir="${2:-$ROOT/build/images/thinkpad-x13s-gen1}"
  need xorriso "brew install xorriso"
  need mformat "brew install mtools"
  [ -f "$esp_dir/EFI/BOOT/BOOTAA64.EFI" ] || {
    echo "error: $esp_dir/EFI/BOOT/BOOTAA64.EFI missing (run: cargo xtask image --arch aarch64 --hardware x13s)" >&2
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
  [ "$mib" -lt 32 ] || { echo "error: ARM64 ISO EFI payload exceeds the 31 MiB El Torito limit" >&2; exit 1; }
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
  echo "wrote $out ($(du -h "$out" | cut -f1))"
}

case "$ARCH" in
  amd64|x86_64|x86-64) build_amd64 "${2:-}" ;;
  aarch64|arm64)       build_aarch64 "${2:-}" "${3:-}" ;;
  *) echo "usage: scripts/mkiso.sh {amd64|aarch64} [out.iso] [esp_dir]" >&2; exit 1 ;;
esac
