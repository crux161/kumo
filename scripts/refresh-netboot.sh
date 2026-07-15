#!/usr/bin/env bash
# refresh-netboot.sh - rebuild and atomically repopulate KUMO's AMD64 UEFI/TFTP root.
#
# Usage:
#   ./scripts/refresh-netboot.sh [output-directory]
#   ./scripts/refresh-netboot.sh --no-build [output-directory]
#
# The default output is netboot/tftp. The output directory is replaced only when it
# contains this script's ownership marker, preventing an accidental wipe of a general
# purpose TFTP root. Point a DHCP server at bootx64.efi after synchronizing this tree.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT="$ROOT/netboot/tftp"
OUTPUT_SET=0
BUILD=1
MARKER=.kumo-netboot-root

usage() {
  cat >&2 <<'USAGE'
usage: ./scripts/refresh-netboot.sh [--no-build] [output-directory]

Rebuild and repopulate an AMD64 UEFI PXE/TFTP tree. The default output directory is
netboot/tftp. Use --no-build to restage already-built kernel and initrd payloads.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --no-build)
      BUILD=0
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --*)
      echo "error: unknown option '$1'" >&2
      usage
      exit 2
      ;;
    *)
      if [ "$OUTPUT_SET" -eq 1 ]; then
        echo "error: only one output directory may be specified" >&2
        usage
        exit 2
      fi
      OUTPUT="$1"
      OUTPUT_SET=1
      ;;
  esac
  shift
done

case "$OUTPUT" in
  /*) ;;
  *) OUTPUT="$ROOT/$OUTPUT" ;;
esac

if [ -e "$OUTPUT" ] && [ ! -d "$OUTPUT" ]; then
  echo "error: output exists but is not a directory: $OUTPUT" >&2
  exit 1
fi

if [ -d "$OUTPUT" ] && [ -n "$(find "$OUTPUT" -mindepth 1 -maxdepth 1 -print -quit)" ] \
  && [ ! -f "$OUTPUT/$MARKER" ]; then
  echo "error: refusing to replace unowned non-empty directory: $OUTPUT" >&2
  echo "       choose an empty directory or add $MARKER after verifying its contents" >&2
  exit 1
fi

MKNETDIR="$(command -v x86_64-elf-grub-mknetdir || command -v grub-mknetdir || true)"
if [ -z "$MKNETDIR" ]; then
  echo "error: need x86_64-elf-grub-mknetdir or grub-mknetdir" >&2
  echo "       macOS: brew install x86_64-elf-grub" >&2
  exit 1
fi

if [ "$BUILD" -eq 1 ]; then
  echo "==> Building AMD64 Multiboot2 kernel and initrd"
  (cd "$ROOT" && ./scripts/x86-multiboot.sh build)
fi

KERNEL="$ROOT/target/x86_64-unknown-none/release/kumo-kernel.bin"
INITRD="$ROOT/target/x86_64-unknown-none/release/kumo-initrd.img"
for payload in "$KERNEL" "$INITRD"; do
  if [ ! -s "$payload" ]; then
    echo "error: missing payload: $payload" >&2
    echo "       rerun without --no-build" >&2
    exit 1
  fi
done

PARENT="$(dirname "$OUTPUT")"
mkdir -p "$PARENT"
PARENT="$(cd "$PARENT" && pwd)"
OUTPUT="$PARENT/$(basename "$OUTPUT")"
STAGE="$(mktemp -d "$PARENT/.kumo-tftp.next.XXXXXX")"
BACKUP=""

cleanup() {
  if [ -n "$STAGE" ] && [ -d "$STAGE" ]; then
    rm -rf "$STAGE"
  fi
  if [ -n "$BACKUP" ] && [ -d "$BACKUP" ] && [ ! -e "$OUTPUT" ]; then
    mv "$BACKUP" "$OUTPUT"
  fi
}
trap cleanup EXIT INT TERM

echo "==> Generating GRUB UEFI network loader"
"$MKNETDIR" \
  --net-directory="$STAGE" \
  --subdir=/boot/grub \
  --install-modules="normal configfile serial terminal multiboot2 mmap net tftp efinet echo boot" \
  --modules="efinet tftp net normal" \
  --locales= --themes= --fonts= >/dev/null

CORE="$STAGE/boot/grub/x86_64-efi/core.efi"
if [ ! -s "$CORE" ]; then
  echo "error: GRUB did not produce $CORE" >&2
  exit 1
fi

mkdir -p "$STAGE/amd64" "$STAGE/EFI/BOOT" "$STAGE/config"
cp "$KERNEL" "$STAGE/amd64/kumo-kernel"
cp "$INITRD" "$STAGE/amd64/kumo-initrd.img"
# bootx64.efi is the concise DHCP filename. Keep the removable-media path too so the
# same generated tree remains easy to inspect and reuse.
cp "$CORE" "$STAGE/bootx64.efi"
cp "$CORE" "$STAGE/EFI/BOOT/BOOTX64.EFI"

cat > "$STAGE/boot/grub/grub.cfg" <<'CFG'
set timeout=3
set default=0

# Mirror GRUB and KUMO output to COM1 for headless machines.
serial --unit=0 --speed=115200
terminal_input console serial
terminal_output console serial

menuentry "KUMO (Ziwei) x86_64 - TFTP/Multiboot2" {
    insmod multiboot2
    insmod mmap
    # Keep the null page out of GRUB's module allocator. KUMO reserves the rest of the
    # legacy megabyte while normalizing the firmware memory map.
    cutmem 0 4K
    set gfxpayload=text
    multiboot2 /amd64/kumo-kernel
    module2 /amd64/kumo-initrd.img kumo-initrd
    boot
}
CFG

GRUB_CHECK="$(command -v x86_64-elf-grub-script-check || command -v grub-script-check || true)"
if [ -n "$GRUB_CHECK" ]; then
  "$GRUB_CHECK" "$STAGE/boot/grub/grub.cfg"
fi

cat > "$STAGE/config/pxehost.env.example" <<EOF
# KUMO's pxehost overlay consumes these settings. The run-pxehost.sh wrapper exports
# them automatically; they are recorded here so the generated tree is self-describing.
PXEHOST_TFTP_ROOT="$OUTPUT"
PXEHOST_BOOTFILE="bootx64.efi"
# PXEHOST_ADVERTISED_IP="192.168.0.107"
EOF

cat > "$STAGE/README.txt" <<EOF
KUMO AMD64 UEFI netboot tree
============================

TFTP root:     $OUTPUT
DHCP bootfile: bootx64.efi
Firmware:      x86-64 UEFI with the network stack enabled
Secure Boot:   disable it for this unsigned development loader

The pxehost proxyDHCP service advertises bootx64.efi to x86-64 UEFI clients. Its TFTP
service provides the GRUB loader, boot/grub/grub.cfg, required GRUB modules, KUMO's
Multiboot2 kernel, and the KUMORD01 initrd. The pxehost environment is under config/.

Run scripts/run-pxehost.sh to refresh this tree and start proxyDHCP/TFTP. Rerun it
whenever the kernel or initrd changes.
EOF

COMMIT="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
cat > "$STAGE/BUILD-INFO.txt" <<EOF
architecture=x86_64
firmware=uefi
boot_protocol=grub-multiboot2-over-tftp
dhcp_bootfile=bootx64.efi
source_commit=$COMMIT
EOF

touch "$STAGE/$MARKER"

if command -v shasum >/dev/null 2>&1; then
  hash_file() { shasum -a 256 "$1"; }
elif command -v sha256sum >/dev/null 2>&1; then
  hash_file() { sha256sum "$1"; }
else
  echo "error: need shasum or sha256sum to write the payload manifest" >&2
  exit 1
fi

MANIFEST="$STAGE/MANIFEST.sha256"
: > "$MANIFEST"
find "$STAGE" -type f ! -name MANIFEST.sha256 -print | LC_ALL=C sort | while IFS= read -r file; do
  relative="${file#"$STAGE"/}"
  digest="$(hash_file "$file" | awk '{print $1}')"
  printf '%s  %s\n' "$digest" "$relative" >> "$MANIFEST"
done

echo "==> Installing refreshed TFTP root"
if [ -d "$OUTPUT" ]; then
  BACKUP="$PARENT/.kumo-tftp.previous.$$"
  mv "$OUTPUT" "$BACKUP"
fi
mv "$STAGE" "$OUTPUT"
STAGE=""
if [ -n "$BACKUP" ]; then
  rm -rf "$BACKUP"
  BACKUP=""
fi

trap - EXIT INT TERM
echo "wrote $OUTPUT"
echo "  DHCP bootfile: bootx64.efi"
echo "  payloads:      amd64/kumo-kernel + amd64/kumo-initrd.img"
echo "  manifest:      MANIFEST.sha256"
