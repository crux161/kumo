#!/usr/bin/env bash
# Refresh KUMO's AMD64 PXE tree and serve it with the KUMO-capable pxehost build.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PXEHOST="${KUMO_PXEHOST_BIN:-$ROOT/build/tools/pxehost-kumo}"
REFRESH_ARG=""

case "${1:-}" in
  "") ;;
  --no-build) REFRESH_ARG=--no-build ;;
  -h|--help)
    cat <<'USAGE'
usage: ./scripts/run-pxehost.sh [--no-build]

Build and refresh KUMO's AMD64 PXE tree, then run pxehost's proxyDHCP and TFTP
services. --no-build restages the existing KUMO kernel and initrd.

Environment:
  KUMO_PXE_IP        IPv4 address to advertise (otherwise auto-detected)
  KUMO_PXEHOST_BIN   alternate KUMO-capable pxehost binary
USAGE
    exit 0
    ;;
  *)
    echo "error: unknown argument '$1'" >&2
    exit 2
    ;;
esac

if [ ! -x "$PXEHOST" ]; then
  "$ROOT/scripts/build-pxehost-kumo.sh"
fi

if [ -n "$REFRESH_ARG" ]; then
  "$ROOT/scripts/refresh-netboot.sh" "$REFRESH_ARG"
else
  "$ROOT/scripts/refresh-netboot.sh"
fi

export PXEHOST_TFTP_ROOT="$ROOT/netboot/tftp"
export PXEHOST_BOOTFILE=bootx64.efi
# ARM64 UEFI clients (option 93 arch 0x000b) boot Nijigumo directly. Left unset the
# sidecar auto-detects bootaa64.efi in the tree; naming it here keeps the intent visible.
export PXEHOST_ARM64_BOOTFILE=bootaa64.efi
if [ -n "${KUMO_PXE_IP:-}" ]; then
  export PXEHOST_ADVERTISED_IP="$KUMO_PXE_IP"
fi

echo "==> Starting KUMO PXE host"
echo "    TFTP root: $PXEHOST_TFTP_ROOT"
echo "    bootfile:  $PXEHOST_BOOTFILE"
if [ -s "$PXEHOST_TFTP_ROOT/$PXEHOST_ARM64_BOOTFILE" ]; then
  echo "    arm64:     $PXEHOST_ARM64_BOOTFILE (aarch64 UEFI / opi5)"
fi
echo "    ports:     UDP 67 (proxyDHCP), 69 (TFTP), 4011 (PXE)"
echo "    Stop PumpKIN or any other DHCP/TFTP service before continuing."
exec "$PXEHOST"
