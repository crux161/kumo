#!/usr/bin/env bash
#j904
#j906
# Refresh KUMO's PXE tree for one hardware target and serve it with the KUMO-capable
# pxehost build. With no arguments the target is the Orange Pi 5 Plus.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PXEHOST="${KUMO_PXEHOST_BIN:-$ROOT/build/tools/pxehost-kumo}"
TARGET="${KUMO_PXE_TARGET:-opi5}"
BUILD=1

usage() {
  cat <<'USAGE'
usage: ./scripts/run-pxehost.sh [--target opi5|nuc|all] [--no-build]

Build KUMO for one hardware target, stage it as netboot/tftp, then run pxehost's
proxyDHCP and TFTP services. --no-build restages the existing payloads.

Targets (default: opi5):
  opi5   Orange Pi 5 Plus, arm64 UEFI -> bootaa64.efi (Nijigumo)
  nuc    x86-64 PXE clients, reference rig Intel NUC7i5DNHE
         UEFI -> bootx64.efi (Nijigumo, first light on the screen);
         legacy BIOS -> core.0 (GRUB -> Multiboot2 KUMO, serial only)
  all    serve both from one tree

Only the selected target's boot program is staged. A machine of the other
architecture gets no KUMO loader and falls through to its next boot entry.

Environment:
  KUMO_PXE_TARGET    default target when --target is not given
  KUMO_PXE_IP        IPv4 address to advertise (otherwise auto-detected)
  KUMO_PXEHOST_BIN   alternate KUMO-capable pxehost binary
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --no-build) BUILD=0 ;;
    --target)
      if [ "$#" -lt 2 ]; then
        echo "error: --target needs a value (opi5, nuc or all)" >&2
        exit 2
      fi
      TARGET="$2"
      shift
      ;;
    --target=*) TARGET="${1#--target=}" ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown argument '$1'" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

# refresh-netboot.sh validates and normalizes the target name (opi5plus, x86, ...).
REFRESH_ARGS=(--target "$TARGET")
if [ "$BUILD" -eq 0 ]; then
  REFRESH_ARGS+=(--no-build)
fi

if [ ! -x "$PXEHOST" ]; then
  "$ROOT/scripts/build-pxehost-kumo.sh"
fi

"$ROOT/scripts/refresh-netboot.sh" "${REFRESH_ARGS[@]}"

export PXEHOST_TFTP_ROOT="$ROOT/netboot/tftp"
STAGED_TARGET="$(sed -n 's/^target=//p' "$PXEHOST_TFTP_ROOT/BUILD-INFO.txt")"

# Advertise only what the tree carries. The overlay offers x86-64 UEFI clients
# bootx64.efi regardless; in an opi5-only tree that file is absent, so the TFTP read
# fails and the PC falls through to its next boot entry.
export PXEHOST_BOOTFILE=bootx64.efi
unset PXEHOST_ARM64_BOOTFILE PXEHOST_BIOS_BOOTFILE
if [ -s "$PXEHOST_TFTP_ROOT/bootaa64.efi" ]; then
  export PXEHOST_ARM64_BOOTFILE=bootaa64.efi
fi
if [ -s "$PXEHOST_TFTP_ROOT/boot/grub/i386-pc/core.0" ]; then
  export PXEHOST_BIOS_BOOTFILE=boot/grub/i386-pc/core.0
fi
if [ -n "${KUMO_PXE_IP:-}" ]; then
  export PXEHOST_ADVERTISED_IP="$KUMO_PXE_IP"
fi

echo "==> Starting KUMO PXE host — target: ${STAGED_TARGET:-unknown}"
echo "    TFTP root: $PXEHOST_TFTP_ROOT"
if [ -n "${PXEHOST_ARM64_BOOTFILE:-}" ]; then
  echo "    opi5:      $PXEHOST_ARM64_BOOTFILE (arm64 UEFI, option 93 arch 0x000b)"
fi
if [ -s "$PXEHOST_TFTP_ROOT/$PXEHOST_BOOTFILE" ]; then
  echo "    nuc:       $PXEHOST_BOOTFILE (Nijigumo, x86-64 UEFI) + ${PXEHOST_BIOS_BOOTFILE:-no BIOS NBP}"
else
  # j906: the overlay still offers bootx64.efi to an x86 client; say so before it fails.
  echo "    nuc:       NOT SERVED - an x86 PXE client is offered $PXEHOST_BOOTFILE and the"
  echo "               TFTP read fails ('no such file'); rerun with --target nuc or --target all"
fi
echo "    ports:     UDP 67 (proxyDHCP), 69 (TFTP), 4011 (PXE)"
echo "    Stop PumpKIN or any other DHCP/TFTP service before continuing."
exec "$PXEHOST"
