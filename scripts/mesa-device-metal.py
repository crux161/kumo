#!/usr/bin/env python3
#j765
#j768
#j769
#j770
#j772
"""Build live Mesa device and static GLES clients, stage them in the Orange Pi GPU slot,
and check a serial log of the hardware run.

  --proof NAME  select the native device, sync, heap, GL lifecycle, query or compute client.
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
    "gl2-b": dict(
        cases="gl-clear", runtime="device-metal", defines=["-DKUMO_GL_CLEAR_MAIN=1"],
        out="gl2-b-metal", gl=True, begin="GL2-b metal clear begin",
        lines=["GL2-b context: current GLES context over the native GPU session",
               "GL2-b target: complete owned 64x48 RGBA8 frontend framebuffer",
               "GL2-b baseline: public glReadPixels observed pixels different from the requested clear color",
               "GL2-b clear: public glClear and native glFinish complete, expected RGBA8 0xff996633",
               "GL2-b readback: 0 of 3072 pixels differ; 0 of 32 readback guards changed (Mali clear color 0xff996633)",
               "GL2-b samples: first=0xff996633 last=0xff996633",
               "GL2-b destroy: context, owned framebuffer drawable and display released"],
        green="GL2-b metal clear green: Mali glClear replaced all 3072 RGBA8 pixels, glReadPixels guards intact, native completion and clean teardown",
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    "gl2-a": dict(
        cases="gl-compute", runtime="device-metal", defines=["-DKUMO_GL_COMPUTE_MAIN=1"],
        out="gl2-a-metal", gl=True, begin="GL2-a metal compute begin",
        lines=["GL2-a context: current GLES 3.1-capable context over the native GPU session",
               "GL2-a compiler: GLSL ES 310 compute source compiled and linked at runtime",
               "GL2-a buffer: 64 output words and 16 guard words initialized to 0xdeadbeef",
               "GL2-a dispatch: four groups of sixteen invocations, memory barrier and native glFinish complete",
               "GL2-a readback: 0 of 64 output words differ; 0 of 16 guards changed (Mali shader results)",
               "GL2-a destroy: shader, program, SSBO, context, drawable and display released"],
        green="GL2-a metal compute green: runtime-compiled GLES compute wrote all 64 SSBO words, guards intact, native completion and clean teardown",
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    "gl1-f": dict(
        cases="gl-client", runtime="device-metal", defines=["-DKUMO_GL_CLIENT_MAIN=1"],
        out="gl1-f-metal", gl=True,
        begin="GL1-f metal client begin",
        lines=["GL1-f renderer: Mali-G610 MC4 (Panfrost)", "GL1-f version: OpenGL ES ",
               "GL1-f finish: public glFinish returned with GL_NO_ERROR and the native device live",
               "GL1-f destroy: context unbound, drawable and display destroyed"],
        green="GL1-f metal client green: public GLES2 renderer names Mali-G610, glFinish completed, all frontend resources destroyed",
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point 1 failed"], after=None),
    "gl1-e": dict(
        define=9, out="gl1-e-metal", gl=True,
        begin="GL1-e metal frontend begin",
        lines=["GL1-e drawable: owned 64x48 RGBA8 color and Z24/S8 depth/stencil attachments",
               "GL1-e current: Mesa's GLES2 state-tracker context made current; glthread and threaded pipe off",
               "GL1-e front: validate supplied the attachments; native flush completed before the callback; callback refusal propagated",
               "GL1-e destroy: current context unbound, drawable and display released, session context recreation refused"],
        green="GL1-e metal frontend green: Mesa context made current on Mali, color/depth validated, front flush completed, context and display destroyed",
        driver=[], absent=[], after=None),
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
    # G7q-a (J754): the first heap of a fresh session lands at the window's start, context
    # first, so its chunks and head are fixed. — TIMBERDOODLE
    "g7q-a": dict(
        define=3, out="g7q-a", begin="G7q-a metal heap begin",
        lines=["G7q-a heap: 3 chunks of 131072 bytes, context 0x30000000, head 0x30041000",
               "G7q-a walk: the GPU followed 3 links from the head through 0x30021000 and 0x30001000",
               "G7q-a context: the GPU read its 32 bytes at 0x30000000, all zero",
               "G7q-a destroy: 397312 bytes returned",
               "G7q-a destroy: the load faulted"],
        green=("G7q-a metal heap green: HeapCreate placed and linked the chunks the GPU walked, the "
               "context read zero, and HeapDestroy unmapped them"),
        # drv-gpu logs the destroy just before the client's line, so the window opens earlier.
        after="G7q-a context: the GPU read its 32 bytes",
        driver=["drv-gpu: client heap 1 destroyed; 4 pieces unmapped",
                "drv-gpu: AS1 fault at VA=0x0000000030041000: TRANSLATION_FAULT",
                "drv-gpu: containment reset=ok"],
        absent=[]),
    # G7q-c (J757): the same walk with the heap created and destroyed through the C bridge.
    "g7q-c": dict(
        define=4, out="g7q-c-metal", begin="G7q-c metal heap begin",
        lines=["G7q-c heap: 3 chunks of 131072 bytes, context 0x30000000, head 0x30041000",
               "G7q-c walk: the GPU followed 3 links from the head through 0x30021000 and 0x30001000",
               "G7q-c context: the GPU read its 32 bytes at 0x30000000, all zero",
               "G7q-c destroy: 397312 bytes returned",
               "G7q-c destroy: the load faulted"],
        green=("G7q-c metal heap green: pan_kmod_kumo_heap_create placed and linked the chunks the GPU "
               "walked, and pan_kmod_kumo_heap_destroy unmapped them"),
        after="G7q-c context: the GPU read its 32 bytes",
        driver=["drv-gpu: client heap 1 destroyed; 4 pieces unmapped",
                "drv-gpu: AS1 fault at VA=0x0000000030041000: TRANSLATION_FAULT",
                "drv-gpu: containment reset=ok"],
        absent=[]),
    # GL1-a (J758): command streams from writable no-execute bindings, Gallium's and panvk's
    # CS pools, cached and uncached, with one line reused. — TIMBERDOODLE
    "gl1-a": dict(
        define=5, out="gl1-a", begin="GL1-a metal streams begin",
        lines=["GL1-a streams: two writable no-execute BOs (GPU-cached at 0x20100000, uncached at 0x20200000)",
               "GL1-a cached: a stream in the cached NX BO ran and {0x7f1a0001, status 0} landed",
               "GL1-a rewrite: the same line rewritten ran its new words; {0x7f1a0002, status 0} landed",
               "GL1-a uncached: a stream in the uncached NX BO ran and {0x7f1a0003, status 0} landed",
               "GL1-a refusal: a stream crossing its binding's end was refused"],
        green=("GL1-a metal streams green: Submit ran streams from cached and uncached writable no-execute "
               "bindings, a rewritten line ran its new words, and a straddling stream was refused"),
        after=None, driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"]),
    # GL1-b (J759): Gallium's screen over the native device; the client links Gallium. — TIMBERDOODLE
    "gl1-b": dict(
        define=6, out="gl1-b-metal", gl=True, begin="GL1-b metal screen begin",
        lines=["GL1-b screen: Mali-G610 MC4 (Panfrost) from Mesa, over KUMO's native device with no DRM fd",
               "GL1-b caps: GLSL", "GL1-b resource: a 64x64 RGBA8 render-target texture created",
               "GL1-b destroy: the texture and the screen are gone"],
        green=("GL1-b metal screen green: Gallium's panfrost screen came up on Mali over the native device, "
               "its caps read, a texture made, and everything destroyed"),
        after=None, driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"]),
    "gl1-c": dict(
        define=7, out="gl1-c-metal", gl=True, begin="GL1-c metal context begin",
        lines=["GL1-c initialize: creating Gallium's native CSF context",
               "GL1-c context: Gallium created its CSF context; the native initialization submission completed",
               "GL1-c admission: a second context was refused while the first remained live",
               "GL1-c cleanup: native completion observed, heap released, context destroyed",
               "GL1-c destroy: the screen and device are gone; the session-owned group was not reused"],
        green=("GL1-c metal context green: Gallium's native initialization completed, a second context was refused, "
               "and the heap, context and screen were destroyed"),
        after=None, driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point 1 failed"]),
    # GL1-d (J765): a clear through Gallium's batch, its native fence and the CPU readback. On
    # metal the poll may find the batch complete or pending; both lines start alike. — TIMBERDOODLE
    "gl1-d": dict(
        define=8, out="gl1-d-metal", gl=True, begin="GL1-d metal batch begin",
        lines=["GL1-d target: a linear 64x64 RGBA8 render target, filled from the CPU with 0xdeadbeef",
               "GL1-d batch: pipe->clear flushed as one native submission; the fence snapshots the context binary",
               "GL1-d poll: a zero-timeout fence_finish found the batch",
               "GL1-d fence: fence_finish completed natively, ",
               "GL1-d readback: 0 of 4096 pixels differ from 0xff996633 (the GPU's clear color)",
               "GL1-d state: native GROUP_STATE reports the group usable, so no reset",
               "GL1-d destroy: fence, texture, context and screen are gone; every BO left through a deferred unmap"],
        green=("GL1-d metal batch green: Mali cleared the render target through Gallium, the native fence completed, "
               "the CPU read the clear color back, and everything was destroyed"),
        after=None, driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point 1 failed",
                                       "drv-gpu: client point 2 failed"]),
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
    # Without a failure window, `absent` covers the whole proof: GL1-a must fault nowhere.
    last = next(i for i, line in enumerate(window) if proof["green"] in line)
    first = next(i for i, line in enumerate(window) if proof["after"] in line) if proof["after"] else 0
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
    defines = proof["defines"] if "defines" in proof else [f"-DKUMO_METAL_PROOF={proof['define']}"]
    elf = module.build(proof.get("cases", "device-metal"), ROOT / "build/proofs" / proof["out"],
                       TOOL / "device-metal-crt0.S", defines=defines, gl=proof.get("gl", False),
                       runtime=proof.get("runtime"))
    print(f"{args.proof} metal client compile/link green: {elf} sha256 {hashlib.sha256(elf.read_bytes()).hexdigest()}")
    if args.stage:
        stage(args.stage, elf)


if __name__ == "__main__":
    main()
