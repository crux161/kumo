#!/usr/bin/env python3
#j747
#j753
"""G7o-f and G7p-e: build the live Mesa device client, stage it as an Orange Pi image's GPU
client, and check a serial log of its metal run.

  --proof NAME  g7o-f (the default), g7p-e-fatal or g7p-e-timeout: the cases the client runs.
  --stage DIR   rebuild the Orange Pi image with KUMO_CONSOLE_LOGLEVEL=5, then copy it to DIR
                (which must not exist) with only bin/gpu-client replaced. Never installs or
                serves it; that is the operator's pxehost step. Rebuild the default image
                after the metal run.
  --check LOG   check a tio log of the board's boot for the proof's live lines.

Since J748 the default console shows WARNING and worse. The proof lines are NOTICE, so a
default image would boot the proof without printing it live. A boot whose begin line never
appears fails --check.

No download, image installation or hardware reset. See toolchain/mesa/README.md.
"""
import argparse
import hashlib
import importlib.util
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / "toolchain/mesa"
IMAGE = ROOT / "build/images/orange-pi-5-plus"
SLOT = b"bin/gpu-client"
# Per proof: the build define, its output, and the live lines that must each appear exactly
# once, in this order. Driver lines interleave with the client's, so `driver` lines must each
# appear once between `after` and the green line, in any order, and `absent` never in that
# window. — TIMBERDOODLE
SYNC = ["G7p-e device: gpu_id=", "G7p-e group: native group on VM", "G7p-e A: point 1 submitted natively",
        "G7p-e B: point 2 captured A's unobserved fence", "G7p-e ARM: points 1 and 2 complete"]
PROOFS = {
    "g7o-f": dict(
        define=0, out="g7o-f", begin="G7o-f metal device begin",
        lines=["G7o-f device: gpu_id=", "G7o-f BOs: 5 live", "G7o-f VM: 5 immediate binds",
               "G7o-f GPU: CS sync stores through Mesa bindings landed", "G7o-f VM: data BO unbound",
               "G7o-f W^X: CS store aimed at the executable binding", "G7o-f W^X: store refused"],
        green=("G7o-f metal device green: live props, Mesa BOs and VM, GPU stores through Mesa bindings, "
               "live rebind, W^X store faulted and contained"),
        after=None, driver=[], absent=[]),
    "g7p-e-fatal": dict(
        define=1, out="g7p-e-fatal", begin="G7p-e metal sync begin (fatal)",
        lines=SYNC + ["G7p-e fatal: point 3 stores into the executable binding 0x20100080",
                      "G7p-e fatal: ARM saw the session close"],
        green=("G7p-e metal sync green (fatal): native binary and timeline syncs, a dependency checked "
               "against the GPU, ARM completions, wait-pending and FATAL group state on Mali"),
        after="G7p-e fatal: point 3 stores",
        driver=["drv-gpu: client point 3 failed:",
                "drv-gpu: AS1 fault at VA=0x0000000020100080: PERMISSION_FAULT Write",
                "drv-gpu: containment reset=ok"],
        absent=[]),
    "g7p-e-timeout": dict(
        define=2, out="g7p-e-timeout", begin="G7p-e metal sync begin (timeout)",
        lines=SYNC + ["G7p-e timeout: point 3 waits on a word nobody writes",
                      "G7p-e timeout: ARM saw the session close"],
        green=("G7p-e metal sync green (timeout): native binary and timeline syncs, a dependency checked "
               "against the GPU, ARM completions, wait-pending and TIMED_OUT group state on Mali"),
        after="G7p-e timeout: point 3 waits",
        driver=["drv-gpu: client point 3 failed: Stalled", "drv-gpu: containment reset=ok"],
        absent=["drv-gpu: AS1 fault"]),
}


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
    # The console level is a kernel build constant (J748), so the copy is taken from an image
    # built for it, and the kernel identity check below covers that build. — TIMBERDOODLE
    subprocess.run(["cargo", "xtask", "image", "--arch", "aarch64", "--hardware", "orange-pi-5-plus"],
                   cwd=ROOT, check=True, env={**os.environ, "KUMO_CONSOLE_LOGLEVEL": "5"})
    shutil.copytree(IMAGE, destination)
    (destination / "STAGE-INFO.txt").write_text(
        f"KUMO_CONSOLE_LOGLEVEL=5\ngpu-client sha256 {hashlib.sha256(elf.read_bytes()).hexdigest()}\n")
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


def check(log, proof):
    text = log.read_bytes().decode(errors="replace").replace("\r", "")
    # Live lines only: a dmesg replay repeats a line behind a second, kernel timestamp.
    live = [line for line in text.splitlines() if not re.match(r"^\[[0-9:.]+\] \[ *[0-9]+\.[0-9]+\]", line)]
    # A log may hold several boots: judge only the last, from its own PXE start.
    boot = max((i for i, line in enumerate(live) if "Station IP address" in line), default=0)
    live = live[boot:]
    begin = proof["begin"]
    start = next((i for i, line in enumerate(live) if begin in line), None)
    assert start is not None, f"{begin!r} never appeared live; was the image built with KUMO_CONSOLE_LOGLEVEL=5?"
    window = live[start:]
    position = 0
    for expected in [begin, *proof["lines"], proof["green"]]:
        hits = [i for i, line in enumerate(window) if expected in line]
        assert len(hits) == 1, f"{expected!r} appears {len(hits)} times"
        assert hits[0] >= position, f"{expected!r} is out of order"
        position = hits[0]
    if proof["after"]:
        first = next(i for i, line in enumerate(window) if proof["after"] in line)
        last = next(i for i, line in enumerate(window) if proof["green"] in line)
        failure = window[first:last]
        for expected in proof["driver"]:
            hits = [line for line in failure if expected in line]
            assert len(hits) == 1, f"{expected!r} appears {len(hits)} times in the failure window"
        for unwanted in proof["absent"]:
            assert not any(unwanted in line for line in failure), f"{unwanted!r} in the failure window"
    for bad in ("TOWER", "FAILED", "ABEND"):
        assert not any(bad in line for line in live), f"{bad} in the boot"
    for line in window:
        if line.split("] ", 1)[-1].startswith(("drv-gpu: AS1 fault", "drv-gpu: client point", "drv-gpu: containment",
                                               "drv-gpu: CS status", "drv-gpu: MMU rawstat", "drv-gpu: reset",
                                               "gpu-client: client exited", "G7p-e diag", "G7o-f diag")):
            print(line)
    name = begin.split(" metal")[0]
    print(f"{name} metal check green ({proof['out']}): every proof line once, in order; no TOWER, FAILED or ABEND")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--proof", choices=sorted(PROOFS), default="g7o-f")
    parser.add_argument("--stage", type=Path)
    parser.add_argument("--check", type=Path)
    args = parser.parse_args()
    proof = PROOFS[args.proof]
    if args.check:
        check(args.check, proof)
        return
    spec = importlib.util.spec_from_file_location("device_smoke", ROOT / "scripts/mesa-device-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    elf = module.build("device-metal", ROOT / "build/proofs" / proof["out"], TOOL / "device-metal-crt0.S",
                       defines=[f"-DKUMO_METAL_PROOF={proof['define']}"])
    print(f"{args.proof} metal client compile/link green: {elf} sha256 {hashlib.sha256(elf.read_bytes()).hexdigest()}")
    if args.stage:
        stage(args.stage, elf)


if __name__ == "__main__":
    main()
