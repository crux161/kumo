#!/usr/bin/env bash
#j904
#j906
# refresh-netboot.sh - rebuild and atomically repopulate KUMO's PXE/TFTP root for one
# hardware target (or all of them).
#
# Usage:
#   ./scripts/refresh-netboot.sh [--target opi5|nuc|all] [--no-build] [output-directory]
#
# The default target is opi5 and the default output is netboot/tftp. The tree carries
# ONLY the selected target's boot program and payloads, so a machine that is not the
# target can never netboot a stale KUMO left behind by an earlier refresh. The output
# directory is replaced only when it contains this script's ownership marker,
# preventing an accidental wipe of a general purpose TFTP root.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUTPUT="$ROOT/netboot/tftp"
OUTPUT_SET=0
BUILD=1
TARGET="${KUMO_PXE_TARGET:-opi5}"
MARKER=.kumo-netboot-root

usage() {
  cat >&2 <<'USAGE'
usage: ./scripts/refresh-netboot.sh [--target opi5|nuc|all] [--no-build] [output-directory]

Rebuild and repopulate a PXE/TFTP tree for one hardware target. The default output
directory is netboot/tftp. Use --no-build to restage already-built payloads.

Targets (default: opi5, or $KUMO_PXE_TARGET):
  opi5   Orange Pi 5 Plus (arm64 UEFI, option 93 arch 0x000b) -> bootaa64.efi = Nijigumo
  nuc    x86-64 PXE clients, reference rig Intel NUC7i5DNHE: UEFI (arch 0x0007/0x0009)
         -> bootx64.efi = Nijigumo, payloads in amd64/EFI/KUMO/; legacy BIOS (arch 0x0000)
         -> GRUB i386-pc core.0 -> Multiboot2 KUMO (only when i686-elf-grub is installed)
  all    both of the above in one tree

The nuc kernel is built with KUMO_CONSOLE_LOGLEVEL=7 unless you set it: the NUC has no
serial header, so first light must reach its screen, and the console (glass and COM1)
shows only lines at or above that level.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --no-build)
      BUILD=0
      ;;
    --target)
      if [ "$#" -lt 2 ]; then
        echo "error: --target needs a value (opi5, nuc or all)" >&2
        exit 2
      fi
      TARGET="$2"
      shift
      ;;
    --target=*)
      TARGET="${1#--target=}"
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

case "$TARGET" in
  opi5|opi5plus|orange-pi-5-plus) TARGET=opi5 ;;
  nuc|nuc7|nuc7i5dnhe|x86|x86_64|amd64) TARGET=nuc ;;
  all|both) TARGET=all ;;
  *)
    echo "error: unknown target '$TARGET' (expected opi5, nuc or all)" >&2
    usage
    exit 2
    ;;
esac
WANT_X86=0
WANT_ARM64=0
case "$TARGET" in
  opi5) WANT_ARM64=1 ;;
  nuc) WANT_X86=1 ;;
  all) WANT_X86=1; WANT_ARM64=1 ;;
esac

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

echo "==> Netboot target: $TARGET"

MKNETDIR_BIOS=""
if [ "$WANT_X86" -eq 1 ]; then
  # j906: x86-64 UEFI clients boot Nijigumo, like the Orange Pi; GRUB is only the legacy
  # BIOS network boot program (Intel Boot Agent, DHCP option 93 arch 0x0000), which cannot
  # run a UEFI PE. Optional: without an i386-pc grub the x86 side is UEFI-only.
  MKNETDIR_BIOS="$(command -v i686-elf-grub-mknetdir || command -v i386-elf-grub-mknetdir || command -v grub-mknetdir || true)"
fi

X86_LOGLEVEL="${KUMO_CONSOLE_LOGLEVEL:-7}"
X86_LOGLEVEL_BUILT="$X86_LOGLEVEL"
[ "$BUILD" -eq 1 ] || X86_LOGLEVEL_BUILT=as-built

if [ "$BUILD" -eq 1 ]; then
  if [ "$WANT_X86" -eq 1 ]; then
    # The NUC's only output is its screen, so first light must clear the console level.
    # Scoped to the x86 builds: an `all` tree's Orange Pi image keeps its own level.
    echo "==> [nuc] Building the x86_64 UEFI image (Nijigumo + kernel + initrd), console level $X86_LOGLEVEL"
    (cd "$ROOT" && KUMO_CONSOLE_LOGLEVEL="$X86_LOGLEVEL" cargo xtask image --arch x86_64 --hardware generic-uefi-x86_64 >/dev/null)
    if [ -n "$MKNETDIR_BIOS" ]; then
      echo "==> [nuc] Building the Multiboot2 kernel and initrd for legacy BIOS clients"
      (cd "$ROOT" && KUMO_CONSOLE_LOGLEVEL="$X86_LOGLEVEL" ./scripts/x86-multiboot.sh build)
    fi
  fi
  if [ "$WANT_ARM64" -eq 1 ]; then
    # ARM64: stage the Orange Pi 5 Plus ESP tree (Nijigumo + kernel + initrd + DTB). The
    # same layout the SD image uses, so a netbooted loader finds its payloads at the very
    # paths it already knows.
    echo "==> [opi5] Building ARM64 (Orange Pi 5 Plus) boot assets"
    (cd "$ROOT" && cargo xtask image --arch aarch64 --hardware opi5plus >/dev/null)
  fi
fi

# The opi5 image staging directory produced by `cargo xtask image`.
ARM64_ESP="$ROOT/build/images/orange-pi-5-plus"
ARM64_LOADER="$ARM64_ESP/EFI/BOOT/BOOTAA64.EFI"

# The x86_64 UEFI image staging directory produced by `cargo xtask image`.
X86_ESP="$ROOT/build/images/generic-uefi-x86_64"
X86_LOADER="$X86_ESP/EFI/BOOT/BOOTX64.EFI"
KERNEL="$ROOT/target/x86_64-unknown-none/release/kumo-kernel.bin"
INITRD="$ROOT/target/x86_64-unknown-none/release/kumo-initrd.img"
if [ "$WANT_X86" -eq 1 ]; then
  for payload in "$X86_LOADER" "$X86_ESP/EFI/KUMO/kernel/kumo-kernel.elf" "$X86_ESP/EFI/KUMO/initrd.img"; do
    if [ ! -s "$payload" ]; then
      echo "error: [nuc] missing payload: $payload" >&2
      echo "       rerun without --no-build, or run: cargo xtask image --arch x86_64 --hardware generic-uefi-x86_64" >&2
      exit 1
    fi
  done
  if [ -n "$MKNETDIR_BIOS" ]; then
    for payload in "$KERNEL" "$INITRD"; do
      if [ ! -s "$payload" ]; then
        echo "error: [nuc] missing BIOS payload: $payload" >&2
        echo "       rerun without --no-build, or run: scripts/x86-multiboot.sh build" >&2
        exit 1
      fi
    done
  fi
fi
if [ "$WANT_ARM64" -eq 1 ] && [ ! -s "$ARM64_LOADER" ]; then
  echo "error: [opi5] missing loader: $ARM64_LOADER" >&2
  echo "       rerun without --no-build, or run: cargo xtask image --arch aarch64 --hardware opi5plus" >&2
  exit 1
fi

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

CORE_BIOS=""
BIOS_BOOTFILE=""
if [ "$WANT_X86" -eq 1 ]; then
  # x86-64 UEFI clients (option 93 arch 0x0007/0x0009) boot Nijigumo directly, which then
  # fetches amd64/EFI/KUMO/* over TFTP (its x86 payload prefix; the Orange Pi's loader reads
  # EFI/KUMO/* at the root, so `all` holds both). bootx64.efi is the concise DHCP filename.
  echo "==> [nuc] Staging Nijigumo and payloads from $X86_ESP"
  mkdir -p "$STAGE/amd64/EFI/KUMO"
  cp "$X86_LOADER" "$STAGE/bootx64.efi"
  (cd "$X86_ESP/EFI/KUMO" && tar cf - .) | (cd "$STAGE/amd64/EFI/KUMO" && tar xf -)

  # Stage the i386-pc BIOS network boot program for legacy PXE clients.
  if [ -n "$MKNETDIR_BIOS" ]; then
    echo "==> [nuc] Generating GRUB i386-pc BIOS network loader ($MKNETDIR_BIOS)"
    "$MKNETDIR_BIOS" \
      --net-directory="$STAGE" \
      --subdir=/boot/grub \
      --install-modules="normal configfile serial terminal multiboot2 mmap net tftp pxe echo boot" \
      --modules="pxe tftp net normal" \
      --locales= --themes= --fonts= >/dev/null
    CORE_BIOS="$STAGE/boot/grub/i386-pc/core.0"
    if [ ! -s "$CORE_BIOS" ]; then
      echo "error: BIOS GRUB did not produce $CORE_BIOS" >&2
      exit 1
    fi
    BIOS_BOOTFILE="boot/grub/i386-pc/core.0"
    cp "$KERNEL" "$STAGE/amd64/kumo-kernel"
    cp "$INITRD" "$STAGE/amd64/kumo-initrd.img"

    cat > "$STAGE/boot/grub/grub.cfg" <<'CFG'
set timeout=3
set default=0

# Mirror GRUB and KUMO output to COM1 for headless machines.
serial --unit=0 --speed=115200
terminal_input console serial
terminal_output console serial

menuentry "KUMO (Ziwei) x86_64 - TFTP/Multiboot2 (legacy BIOS)" {
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

    GRUB_CHECK="$(command -v i686-elf-grub-script-check || command -v x86_64-elf-grub-script-check || command -v grub-script-check || true)"
    if [ -n "$GRUB_CHECK" ]; then
      "$GRUB_CHECK" "$STAGE/boot/grub/grub.cfg"
    fi
  else
    echo "==> [nuc] No i386-pc grub found; x86 side is UEFI-only" >&2
    echo "    (legacy BIOS PXE clients need: brew install i686-elf-grub)" >&2
  fi
fi

# ARM64 UEFI clients (option 93 arch 0x000b) boot Nijigumo directly — there is no GRUB
# stage. bootaa64.efi is the concise DHCP filename; the removable-media path is kept so
# the same tree stays inspectable. EFI/KUMO carries the payloads Nijigumo fetches.
if [ "$WANT_ARM64" -eq 1 ]; then
  echo "==> [opi5] Staging Nijigumo and payloads from $ARM64_ESP"
  mkdir -p "$STAGE/EFI/BOOT"
  cp "$ARM64_LOADER" "$STAGE/bootaa64.efi"
  cp "$ARM64_LOADER" "$STAGE/EFI/BOOT/BOOTAA64.EFI"
  if [ -d "$ARM64_ESP/EFI/KUMO" ]; then
    mkdir -p "$STAGE/EFI/KUMO"
    (cd "$ARM64_ESP/EFI/KUMO" && tar cf - .) | (cd "$STAGE/EFI/KUMO" && tar xf -)
  fi
fi

UEFI_BOOTFILE=none
ARM64_BOOTFILE=none
[ "$WANT_X86" -eq 1 ] && UEFI_BOOTFILE=bootx64.efi
[ "$WANT_ARM64" -eq 1 ] && ARM64_BOOTFILE=bootaa64.efi

mkdir -p "$STAGE/config"
{
  echo "# KUMO's pxehost overlay consumes these settings. run-pxehost.sh exports them"
  echo "# automatically; they are recorded here so the generated tree is self-describing."
  echo "# Netboot target: $TARGET"
  echo "PXEHOST_TFTP_ROOT=\"$OUTPUT\""
  if [ "$WANT_X86" -eq 1 ]; then
    echo "PXEHOST_BOOTFILE=\"bootx64.efi\""
    if [ -n "$BIOS_BOOTFILE" ]; then
      echo "# Legacy BIOS PXE clients (option 93 arch 0x0000) are served this i386-pc NBP."
      echo "PXEHOST_BIOS_BOOTFILE=\"$BIOS_BOOTFILE\""
    fi
  fi
  if [ "$WANT_ARM64" -eq 1 ]; then
    echo "PXEHOST_ARM64_BOOTFILE=\"bootaa64.efi\""
  fi
  echo "# PXEHOST_ADVERTISED_IP=\"192.168.0.107\""
} > "$STAGE/config/pxehost.env.example"

cat > "$STAGE/README.txt" <<EOF
KUMO netboot tree - target: $TARGET
=====================================

TFTP root:      $OUTPUT
x86-64 UEFI:    $UEFI_BOOTFILE   (option 93 arch 0x0007/0x0009, e.g. the NUC7i5DNHE)
x86 BIOS:       ${BIOS_BOOTFILE:-none}   (option 93 arch 0x0000, e.g. Intel Boot Agent)
arm64 UEFI:     $ARM64_BOOTFILE   (option 93 arch 0x000b, the Orange Pi 5 Plus)
Secure Boot:    disable it for these unsigned development loaders

The pxehost proxyDHCP service picks the bootfile by DHCP option 93 (client architecture).
"none" means this tree deliberately carries nothing for that architecture: a machine of
that kind falls through to its next boot entry instead of booting a stale KUMO. The
Orange Pi runs Nijigumo, which fetches EFI/KUMO/*; x86-64 UEFI clients run Nijigumo too,
which fetches amd64/EFI/KUMO/* and draws first light on the screen; legacy BIOS clients run
GRUB, which Multiboot2-boots amd64/kumo-kernel + amd64/kumo-initrd.img (serial only).

Run scripts/run-pxehost.sh [--target opi5|nuc|all] to refresh this tree and start
proxyDHCP/TFTP. Rerun it whenever the kernel or initrd changes.
EOF

COMMIT="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
cat > "$STAGE/BUILD-INFO.txt" <<EOF
target=$TARGET
boot_protocol_x86=nijigumo-over-tftp
boot_protocol_x86_bios=$( [ -n "$BIOS_BOOTFILE" ] && echo grub-multiboot2-over-tftp || echo none )
x86_payload_prefix=amd64/
x86_console_loglevel=$( [ "$WANT_X86" -eq 1 ] && echo "$X86_LOGLEVEL_BUILT" || echo none )
boot_protocol_arm64=nijigumo-over-tftp
dhcp_bootfile_uefi=$UEFI_BOOTFILE
dhcp_bootfile_arm64=$ARM64_BOOTFILE
dhcp_bootfile_bios=${BIOS_BOOTFILE:-none}
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
echo "wrote $OUTPUT (target: $TARGET)"
if [ "$WANT_ARM64" -eq 1 ]; then
  echo "  [opi5] arm64 UEFI bootfile:  bootaa64.efi (Nijigumo; payloads in EFI/KUMO/)"
fi
if [ "$WANT_X86" -eq 1 ]; then
  echo "  [nuc]  x86-64 UEFI bootfile: bootx64.efi (Nijigumo; payloads in amd64/EFI/KUMO/)"
  echo "  [nuc]  x86 BIOS bootfile:    ${BIOS_BOOTFILE:-<none: UEFI-only>}"
fi
echo "  manifest: MANIFEST.sha256"
