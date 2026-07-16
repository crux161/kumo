#!/usr/bin/env bash
# netboot-smoke.sh - boot the generated AMD64 netboot tree end-to-end in QEMU and assert
# KUMO's serial POST, without the physical test machine.
#
# It reproduces a legacy BIOS PXE client (SeaBIOS + the iPXE e1000 option ROM), which
# presents DHCP option 93 arch 0x0000 -- exactly like an Intel Boot Agent -- and boots
# the tree's i386-pc GRUB over QEMU's built-in TFTP/DHCP. This exercises the real
# transport: GRUB fetching its config, modules, kernel, and KUMORD01 initrd over TFTP,
# then Multiboot2-booting KUMO.
#
# Usage: scripts/netboot-smoke.sh [tftp-root] [timeout-seconds]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TREE="${1:-$ROOT/netboot/tftp}"
SECS="${2:-90}"
BIOS_NBP="boot/grub/i386-pc/core.0"

command -v qemu-system-x86_64 >/dev/null 2>&1 || { echo "error: qemu-system-x86_64 not found" >&2; exit 1; }
if [ ! -s "$TREE/$BIOS_NBP" ]; then
  echo "error: $TREE/$BIOS_NBP missing (regenerate with scripts/refresh-netboot.sh; needs i686-elf-grub)" >&2
  exit 1
fi

# Locate QEMU's iPXE BIOS option ROM for e1000.
ROM=""
for d in /opt/homebrew/share/qemu /usr/local/share/qemu /usr/share/qemu "$(dirname "$(command -v qemu-system-x86_64)")/../share/qemu"; do
  if [ -f "$d/pxe-e1000.rom" ]; then ROM="$d/pxe-e1000.rom"; break; fi
done
[ -n "$ROM" ] || { echo "error: pxe-e1000.rom not found in QEMU share dirs" >&2; exit 1; }

WORK="$(mktemp -d)"
SERIAL="$WORK/serial.log"
trap 'rm -rf "$WORK"' EXIT

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
elif grep_c 'first light reached'; then
  echo "netboot-smoke: PASS - KUMO booted over netboot and completed its POST"
  exit 0
else
  echo "netboot-smoke: PARTIAL - KUMO booted over netboot but POST did not complete within ${SECS}s" >&2
  exit 2
fi
