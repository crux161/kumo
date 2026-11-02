#!/usr/bin/env python3
#j747
"""G7o-f: build the live Mesa device client, stage it as an Orange Pi image's GPU client, and
check a serial log of its metal run.

  --stage DIR   copy build/images/orange-pi-5-plus to DIR (which must not exist) with only
                bin/gpu-client replaced. Never installs or serves it; that is the operator's
                pxehost step.
  --check LOG   check a tio log of the board's boot for the proof's live lines.

No download, image installation or hardware reset. See toolchain/mesa/README.md.
"""
import argparse
import hashlib
import importlib.util
from pathlib import Path
import re
import shutil
import struct

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / "toolchain/mesa"
OUT = ROOT / "build/proofs/g7o-f"
IMAGE = ROOT / "build/images/orange-pi-5-plus"
SLOT = b"bin/gpu-client"
BEGIN = "G7o-f metal device begin"
GREEN = ("G7o-f metal device green: live props, Mesa BOs and VM, GPU stores through Mesa bindings, "
         "live rebind, W^X store faulted and contained")
# Each must appear exactly once as a live line, in this order. — TIMBERDOODLE
LINES = [BEGIN, "G7o-f device: gpu_id=", "G7o-f BOs: 5 live", "G7o-f VM: 5 immediate binds",
         "G7o-f GPU: CS sync stores through Mesa bindings landed", "G7o-f VM: data BO unbound",
         "G7o-f W^X: CS store aimed at the executable binding", "G7o-f W^X: store refused", GREEN]


def entries(image):
    data = image.read_bytes()
    assert data[:8] == b"KUMORD01" and struct.unpack_from("<I", data, 8)[0] == 1
    count = struct.unpack_from("<I", data, 12)[0]
    for index in range(count):
        entry = 16 + index * 80
        name = data[entry:entry + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", data, entry + 64)
        yield name, data[offset:offset + size]


def stage(destination, elf):
    assert not destination.exists(), f"refusing to replace existing image copy: {destination}"
    shutil.copytree(IMAGE, destination)
    path = destination / "EFI/KUMO/initrd.img"
    original = path.read_bytes()
    count = struct.unpack_from("<I", original, 12)[0]
    out = bytearray(original[:16 + 80 * count])
    replaced = 0
    for index, (name, data) in enumerate(entries(path)):
        if name == SLOT:
            data, replaced = elf.read_bytes(), replaced + 1
        struct.pack_into("<QQ", out, 16 + index * 80 + 64, len(out), len(data))
        out.extend(data)
    assert replaced == 1, "the image has no single GPU client slot"
    path.write_bytes(out)
    # Prove the staged image differs from the built one in that one slot, and nowhere else.
    built, staged = dict(entries(IMAGE / "EFI/KUMO/initrd.img")), dict(entries(path))
    assert built.keys() == staged.keys()
    assert [n for n in built if built[n] != staged[n]] == [SLOT]
    kernel = "EFI/KUMO/kernel/kumo-kernel.elf"
    assert (IMAGE / kernel).read_bytes() == (destination / kernel).read_bytes()
    print(f"staged {destination}: only {SLOT.decode()} differs ({len(staged[SLOT])} bytes); kernel identical")
    print(f"initrd sha256 {hashlib.sha256(out).hexdigest()} ({len(out)} bytes)")


def check(log):
    text = log.read_bytes().decode(errors="replace").replace("\r", "")
    # Live lines only: a dmesg replay repeats a line behind a second, kernel timestamp.
    live = [line for line in text.splitlines() if not re.match(r"^\[[0-9:.]+\] \[ *[0-9]+\.[0-9]+\]", line)]
    # A log may hold several boots: judge only the last, from its own PXE start.
    boot = max((i for i, line in enumerate(live) if "Station IP address" in line), default=0)
    live = live[boot:]
    start = next(i for i, line in enumerate(live) if BEGIN in line)
    window = live[start:]
    position = 0
    for expected in LINES:
        hits = [i for i, line in enumerate(window) if expected in line]
        assert len(hits) == 1, f"{expected!r} appears {len(hits)} times"
        assert hits[0] >= position, f"{expected!r} is out of order"
        position = hits[0]
    for bad in ("TOWER", "FAILED", "ABEND"):
        assert not any(bad in line for line in live), f"{bad} in the boot"
    for line in window:
        if line.split("] ", 1)[-1].startswith(("drv-gpu: AS1 fault", "drv-gpu: client point", "drv-gpu: containment",
                                               "drv-gpu: reset", "gpu-client: client exited")) or "reset=" in line:
            print(line)
    print("G7o-f metal check green: every proof line once, in order; no TOWER, FAILED or ABEND")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--stage", type=Path)
    parser.add_argument("--check", type=Path)
    args = parser.parse_args()
    if args.check:
        check(args.check)
        return
    spec = importlib.util.spec_from_file_location("device_smoke", ROOT / "scripts/mesa-device-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    elf = module.build("device-metal", OUT, TOOL / "device-metal-crt0.S")
    print(f"G7o-f metal client compile/link green: {elf} sha256 {hashlib.sha256(elf.read_bytes()).hexdigest()}")
    if args.stage:
        stage(args.stage, elf)


if __name__ == "__main__":
    main()
