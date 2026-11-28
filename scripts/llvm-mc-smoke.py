#!/usr/bin/env python3
#J952_r18_001
"""Build and prove LLVM MC + DWARF low-level and prerequisites (SH9-d5) on native KUMO."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "SH9-d5 LLVM MC"
GREEN = (LABEL + " green").encode()
COMMANDS = b"dmesg -n 6\nrelibc sh9-d5\necho sh9-d5-after-launch\n"
RECORDS = tuple(line.encode() for line in (
    'SH9-d5: MC context, ELF section uniquing and symbol attributes -> OK',
    'SH9-d5: absolute and alias expressions, relocation addends and invalid arithmetic -> OK',
    'SH9-d5: instruction operands and data-fixup representation -> OK',
    'SH9-d5: native assembly text labels, integers, bytes, expressions, LEB and alignment -> OK',
    'SH9-d5: ELF suffix deduplication and COFF string-table offsets and bytes -> OK',
    'SH9-d5: DWARF expressions, CFI decoding and scaling, unwind rows and truncated input -> OK',
    'SH9-d5: emitted assembly file round trip on the RAM volume -> OK',
    'SH9-d5 LLVM MC green',
))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_records(transcript):
    """Every proof record exactly once, in order, within the launch window."""
    begin, end = (LABEL + " begin").encode(), b"sh9-d5-after-launch"
    if transcript.count(begin) != 1 or transcript.count(end) != 1:
        raise RuntimeError("expected one MC proof window")
    start, stop = transcript.index(begin) + len(begin), transcript.index(end)
    if stop <= start:
        raise RuntimeError("MC launch returned before its begin record")
    proof = transcript[start:stop]
    positions = [proof.find(marker) for marker in RECORDS]
    if (any(proof.count(marker) != 1 for marker in RECORDS) or positions != sorted(positions)
            or b"TOWER" in proof or b"FAILED" in proof):
        raise RuntimeError("LLVM MC records are missing, duplicated, or out of order")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage a separate ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/llvm-mc")
    parser.add_argument("--libcxx-out", type=Path, default=ROOT / "target/cxx-libcxx")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r18/sh9-d5")
    parser.add_argument("--tblgen", type=Path, help="matching LLVM 22.1.8 llvm-tblgen")
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/llvm/build-mc.py", "--sysroot", args.sysroot, "--out", args.out,
             "--libcxx-out", args.libcxx_out, "--eh-out", args.eh_out]
    if args.tblgen:
        build += ["--tblgen", args.tblgen]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "llvm-mc-smoke"
    for name in ("llvm-mc-smoke", "manifest.json", "undefined.txt", "install/lib/libLLVMMC.a", "install/lib/libLLVMDebugInfoDWARFLowLevel.a", "install/lib/libLLVMTargetParser.a", "install/lib/libLLVMBinaryFormat.a", "install/lib/libLLVMSupport.a", "install/lib/libLLVMDemangle.a", "link.map"):
        shutil.copyfile(out / name, evidence / Path(name).name)
    shutil.copytree(out / "generated", evidence / "generated", dirs_exist_ok=True)
    result = {"elf_sha256": sha(elf), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=elf,
                                         label=LABEL, green=GREEN, tag="sh9-d5", commands=COMMANDS)
            check_records(transcript)
            (evidence / "qemu-serial.log").write_bytes(transcript)
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("SH9-d5 LLVM MC QEMU green", flush=True)
        else:
            stage = args.stage.resolve()
            helpers["stage_image"](args.image.resolve(), stage, elf, COMMANDS)
            result["staged_tree"] = str(stage)
            print(f"SH9-d5 LLVM MC staged: {stage}", flush=True)
    except BaseException as error:
        result["passed"] = False
        result["error"] = str(error)
        raise
    else:
        result["passed"] = True
    finally:
        (evidence / "proof.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
