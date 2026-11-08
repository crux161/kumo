#!/usr/bin/env python3
"""Stage the NET7b wire-proof image into the running pxehost's TFTP root.

Appends one line to `etc/autoexec` inside the built initrd (a KUMORD01 container: magic,
version, u32 count, then 80-byte entries of a 64-byte name plus u64 offset/size), mirrors
the arm64 ESP tree into netboot/tftp, and proves the staged initrd differs from the build
in that one entry alone. Sora runs the manifest input-less at boot and reads at most 256 B.

Usage: scripts/net7b-stage.py ["net netprobe wire 192.168.0.106 7701 24"]
Build the image with the console level you want first (KUMO_CONSOLE_LOGLEVEL=5 for proofs).
"""

import hashlib
import pathlib
import shutil
import struct
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
IMAGE = ROOT / 'build/images/orange-pi-5-plus'
TFTP = ROOT / 'netboot/tftp'
DEFAULT = 'net netprobe wire 192.168.0.106 7701 24'
AUTOEXEC = b'etc/autoexec'


def entries(data):
    assert data[:8] == b'KUMORD01' and struct.unpack_from('<I', data, 8)[0] == 1, 'not a KUMORD01 initrd'
    count = struct.unpack_from('<I', data, 12)[0]
    for index in range(count):
        entry = 16 + index * 80
        name = data[entry:entry + 64].split(b'\0', 1)[0]
        offset, size = struct.unpack_from('<QQ', data, entry + 64)
        yield name, data[offset:offset + size]


def main():
    line = (sys.argv[1] if len(sys.argv) > 1 else DEFAULT).encode() + b'\n'
    built = (IMAGE / 'EFI/KUMO/initrd.img').read_bytes()
    items = []
    appended = False
    for name, data in entries(built):
        if name == AUTOEXEC:
            data = data + line
            appended = True
            assert len(data) <= 256, f'autoexec would be {len(data)} bytes (Sora reads 256)'
        items.append((name, data))
    assert appended, 'the built initrd has no etc/autoexec entry'

    count = struct.unpack_from('<I', built, 12)[0]
    out = bytearray(built[:12] + struct.pack('<I', count) + bytes(80 * count))
    for index, (name, data) in enumerate(items):
        out[16 + index * 80:16 + index * 80 + 64] = name.ljust(64, b'\0')
        struct.pack_into('<QQ', out, 16 + index * 80 + 64, len(out), len(data))
        out.extend(data)

    (TFTP / 'EFI/KUMO/kernel').mkdir(parents=True, exist_ok=True)
    (TFTP / 'EFI/BOOT/KUMO').mkdir(parents=True, exist_ok=True)
    (TFTP / 'EFI/KUMO/initrd.img').write_bytes(out)
    shutil.copy2(IMAGE / 'EFI/KUMO/nijigumo.conf', TFTP / 'EFI/KUMO/nijigumo.conf')
    shutil.copy2(IMAGE / 'EFI/KUMO/kernel/kumo-kernel.elf', TFTP / 'EFI/KUMO/kernel/kumo-kernel.elf')
    shutil.copy2(IMAGE / 'bootaa64.efi', TFTP / 'bootaa64.efi')
    shutil.copy2(IMAGE / 'EFI/BOOT/BOOTAA64.EFI', TFTP / 'EFI/BOOT/BOOTAA64.EFI')
    shutil.copy2(IMAGE / 'EFI/BOOT/KUMO/initrd.img', TFTP / 'EFI/BOOT/KUMO/initrd.img')
    shutil.copy2(IMAGE / 'EFI/BOOT/KUMO/nijigumo.conf', TFTP / 'EFI/BOOT/KUMO/nijigumo.conf')

    staged = (TFTP / 'EFI/KUMO/initrd.img').read_bytes()
    assert line.rstrip(b'\n') in staged, 'the manifest line is not in the staged initrd'
    built_entries = dict(entries(built))
    staged_entries = dict(entries(staged))
    assert staged_entries.keys() == built_entries.keys()
    changed = [name for name in staged_entries if staged_entries[name] != built_entries[name]]
    assert changed == [AUTOEXEC], f'unexpected initrd changes: {changed}'

    print('initrd built ', len(built), 'bytes', hashlib.sha256(built).hexdigest())
    print('initrd staged', len(staged), 'bytes', hashlib.sha256(staged).hexdigest())
    print('autoexec appends:', line.decode().strip())
    print('stage ok: only etc/autoexec differs; TFTP root ready')


if __name__ == '__main__':
    main()
