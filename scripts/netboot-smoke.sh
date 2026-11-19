#!/usr/bin/env bash
#j904
#j906
# netboot-smoke.sh - boot the generated AMD64 netboot tree end-to-end in QEMU and assert
# KUMO's serial POST, without the physical test machine.
#
# It reproduces a legacy BIOS PXE client (SeaBIOS + the iPXE e1000 option ROM), which
# presents DHCP option 93 arch 0x0000 -- exactly like an Intel Boot Agent -- and boots
# the tree's i386-pc GRUB over QEMU's built-in TFTP/DHCP. This exercises the real
# transport: GRUB fetching its config, modules, kernel, and KUMORD01 initrd over TFTP,
# then Multiboot2-booting KUMO.
#
# --uefi (j906, PLAN/025 X3) reproduces the NUC7i5DNHE instead: OVMF's own UEFI PXE (no iPXE
# ROM) boots the tree's bootx64.efi = Nijigumo, which fetches amd64/EFI/KUMO/* over TFTP and
# hands KUMO a BootInfo with the GOP framebuffer. It also requires the write-combining
# console checkpoint and saves the glass as <tftp-root>/../netboot-smoke-glass.ppm.
#
# Usage: scripts/netboot-smoke.sh [--uefi] [tftp-root] [timeout-seconds]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MODE=bios
if [ "${1:-}" = "--uefi" ]; then
  MODE=uefi
  shift
fi
TREE="${1:-$ROOT/netboot/tftp}"
SECS="${2:-90}"
BIOS_NBP="boot/grub/i386-pc/core.0"

command -v qemu-system-x86_64 >/dev/null 2>&1 || { echo "error: qemu-system-x86_64 not found" >&2; exit 1; }
TREE="$(cd "$TREE" && pwd)"
if [ "$MODE" = uefi ]; then
  if [ ! -s "$TREE/bootx64.efi" ] || [ ! -s "$TREE/amd64/EFI/KUMO/kernel/kumo-kernel.elf" ]; then
    echo "error: $TREE has no Nijigumo bootx64.efi + amd64/EFI/KUMO payloads (regenerate with scripts/refresh-netboot.sh --target nuc)" >&2
    exit 1
  fi
  CODE=""; VARS=""
  for d in /opt/homebrew/share/qemu /usr/local/share/qemu /usr/share/qemu "$(dirname "$(command -v qemu-system-x86_64)")/../share/qemu"; do
    if [ -f "$d/edk2-x86_64-code.fd" ] && [ -f "$d/edk2-i386-vars.fd" ]; then
      CODE="$d/edk2-x86_64-code.fd"; VARS="$d/edk2-i386-vars.fd"; break
    fi
  done
  CODE="${KUMO_OVMF_CODE:-$CODE}"; VARS="${KUMO_OVMF_VARS:-$VARS}"
  [ -n "$CODE" ] && [ -n "$VARS" ] || { echo "error: OVMF (edk2-x86_64-code.fd + edk2-i386-vars.fd) not found; set KUMO_OVMF_CODE/KUMO_OVMF_VARS" >&2; exit 1; }
fi
if [ "$MODE" = bios ] && [ ! -s "$TREE/$BIOS_NBP" ]; then
  echo "error: $TREE/$BIOS_NBP missing (regenerate with scripts/refresh-netboot.sh --target nuc; needs i686-elf-grub)" >&2
  exit 1
fi

if [ "$MODE" = bios ]; then
# Locate QEMU's iPXE BIOS option ROM for e1000.
ROM=""
for d in /opt/homebrew/share/qemu /usr/local/share/qemu /usr/share/qemu "$(dirname "$(command -v qemu-system-x86_64)")/../share/qemu"; do
  if [ -f "$d/pxe-e1000.rom" ]; then ROM="$d/pxe-e1000.rom"; break; fi
done
[ -n "$ROM" ] || { echo "error: pxe-e1000.rom not found in QEMU share dirs" >&2; exit 1; }

fi

WORK="$(mktemp -d)"
SERIAL="$WORK/serial.log"
trap 'rm -rf "$WORK"' EXIT

if [ "$MODE" = uefi ]; then
  echo "==> Netboot smoke: UEFI PXE (OVMF, virtio-net, no option ROM) -> Nijigumo bootx64.efi -> KUMO"
  echo "    tree=$TREE  firmware=$CODE  timeout=${SECS}s"
  cp "$VARS" "$WORK/vars.fd"
  # The monitor socket path is relative (cwd = WORK): macOS caps a unix socket at 104 bytes.
  (cd "$WORK" && exec qemu-system-x86_64 \
    -machine q35 -cpu max,+x2apic -m 1088 \
    -drive "if=pflash,unit=0,format=raw,readonly=on,file=$CODE" \
    -drive "if=pflash,unit=1,format=raw,file=$WORK/vars.fd" \
    -netdev "user,id=net0,tftp=$TREE,bootfile=bootx64.efi" \
    -device "virtio-net-pci,netdev=net0,romfile=" \
    -display none \
    -serial "file:$SERIAL" \
    -monitor unix:monitor.sock,server,nowait \
    -no-reboot >/dev/null 2>&1) &
  QPID=$!
else
echo "==> Netboot smoke: BIOS PXE (arch 0x0000) -> i386-pc GRUB -> KUMO"
echo "    tree=$TREE  rom=$ROM  timeout=${SECS}s"

# CPU features match cargo xtask x86-smoke (`-cpu max,+x2apic`) so that any POST
# difference is attributable to the boot method (PXE vs -kernel), not the machine model.
qemu-system-x86_64 \
  -machine pc -cpu max,+x2apic -m 1088 \
  -netdev "user,id=net0,tftp=$TREE,bootfile=$BIOS_NBP" \
  -device "e1000,netdev=net0,romfile=$ROM" \
  -boot n \
  -display none \
  -serial "file:$SERIAL" \
  -no-reboot >/dev/null 2>&1 &
QPID=$!
fi

# KUMO ends the POST with a `hlt` loop, so QEMU keeps running after the transcript is
# complete. Poll the serial log and stop as soon as a terminal marker appears (or the
# hard timeout elapses), instead of always waiting out the full timeout.
deadline=$(( $(date +%s) + SECS ))
while kill -0 "$QPID" 2>/dev/null; do
  if tr -d '\000' < "$SERIAL" 2>/dev/null | grep -qaE 'HALT|FAIL|PREEMPT SCHED|USER ELF'; then
    break
  fi
  [ "$(date +%s)" -ge "$deadline" ] && break
  sleep 1
done
GLASS="$(dirname "$TREE")/netboot-smoke-glass.ppm"
if [ "$MODE" = uefi ] && command -v nc >/dev/null 2>&1; then
  sleep 1
  rm -f "$GLASS"
  (cd "$WORK" && printf 'screendump %s\n' "$GLASS" | nc -U -w 2 monitor.sock >/dev/null 2>&1) || true
  sleep 1
fi
kill "$QPID" 2>/dev/null || true
wait "$QPID" 2>/dev/null || true

# Strip NULs + ANSI escapes for matching and display.
CLEAN="$(tr -d '\000' < "$SERIAL" | sed $'s/\x1b\\[[0-9;=?]*[A-Za-z]//g')"

echo "----- KUMO POST (netboot) -----"
printf '%s\n' "$CLEAN" | grep -aE 'MUREX|Check|TOWER|HALT|EXCEPTION|FAIL|first light reached' || true
echo "-------------------------------"

grep_c() { printf '%s\n' "$CLEAN" | grep -qa "$1"; }
grep_e() { printf '%s\n' "$CLEAN" | grep -qaE "$1"; }

# KUMO's POST ends with "... first light reached; HALTING." on success (a hlt loop, not a
# crash), and prints "TOWER-x86 ... fatal exception" or a "   FAIL" check on error.
if ! grep_c 'KUMO x86_64 first light'; then
  echo "netboot-smoke: FAIL - KUMO never reached first light over netboot (delivery/GRUB problem)" >&2
  exit 1
elif grep_c 'fatal exception' || grep_e '   FAIL'; then
  echo "netboot-smoke: PARTIAL - KUMO booted over netboot but faulted during POST (see above)" >&2
  exit 2
elif [ "$MODE" = uefi ] && ! { grep_c 'BOOTX64.EFI loaded' && grep_c 'tftp         : amd64/EFI/KUMO/kernel/kumo-kernel.elf' \
    && grep_c 'first light (Nijigumo/UEFI)' && grep_e 'FRAMEBUFFER CONSOLE Check .* write-combining .* OK'; }; then
  echo "netboot-smoke: FAIL - KUMO reached first light, but not as Nijigumo over TFTP with a write-combined glass" >&2
  exit 1
elif grep_c 'first light reached'; then
  if [ "$MODE" = uefi ]; then
    echo "netboot-smoke: PASS - UEFI PXE -> Nijigumo -> KUMO completed its POST on COM1 and the glass"
    [ -s "$GLASS" ] && echo "    glass: $GLASS"
  else
    echo "netboot-smoke: PASS - KUMO booted over netboot and completed its POST"
  fi
  exit 0
else
  echo "netboot-smoke: PARTIAL - KUMO booted over netboot but POST did not complete within ${SECS}s" >&2
  exit 2
fi
