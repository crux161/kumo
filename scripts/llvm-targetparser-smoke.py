#!/usr/bin/env python3
#J950_r16_001
"""Build and prove LLVM TargetParser + BinaryFormat + Support + Demangle (SH9-d3) on a native KUMO image."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "SH9-d3 LLVM TargetParser"
GREEN = (LABEL + " green").encode()
COMMANDS = b"dmesg -n 6\nrelibc sh9-d3\necho sh9-d3-after-launch\n"
RECORDS = tuple(line.encode() for line in (
    "SH9-d3: KUMO triple, normalization, data layout and conservative host queries -> OK",
    "SH9-d3: generated AArch64 architectures, CPUs and extensions, ARM parsing -> OK",
    "SH9-d3: generated RISC-V CPUs, ISA extensions, ABI and invalid input -> OK",
    "SH9-d3: generated PPC features and X86, LoongArch, CSKY and Xtensa tables -> OK",
    "SH9-d3: subtarget feature lists, wide feature bitsets and AMDGPU names -> OK",
    "SH9-d3: BinaryFormat Mach-O and XCOFF dependencies, unsupported-triple error -> OK",
    "SH9-d3 LLVM TargetParser green",
))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_records(transcript):
    """Every proof record exactly once, in order, within the launch window."""
    begin, end = (LABEL + " begin").encode(), b"sh9-d3-after-launch"
    if transcript.count(begin) != 1 or transcript.count(end) != 1:
        raise RuntimeError("expected one TargetParser proof window")
    start, stop = transcript.index(begin) + len(begin), transcript.index(end)
    if stop <= start:
        raise RuntimeError("TargetParser launch returned before its begin record")
    proof = transcript[start:stop]
    positions = [proof.find(marker) for marker in RECORDS]
    if (any(proof.count(marker) != 1 for marker in RECORDS) or positions != sorted(positions)
            or b"TOWER" in proof or b"FAILED" in proof):
        raise RuntimeError("LLVM TargetParser records are missing, duplicated, or out of order")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage a separate ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/llvm-targetparser")
    parser.add_argument("--libcxx-out", type=Path, default=ROOT / "target/cxx-libcxx")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r16/sh9-d3")
    parser.add_argument("--tblgen", type=Path, help="matching LLVM 22.1.8 llvm-tblgen")
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/llvm/build-targetparser.py", "--sysroot", args.sysroot, "--out", args.out,
             "--libcxx-out", args.libcxx_out, "--eh-out", args.eh_out]
    if args.tblgen:
        build += ["--tblgen", args.tblgen]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "llvm-targetparser-smoke"
    for name in ("llvm-targetparser-smoke", "manifest.json", "undefined.txt", "install/lib/libLLVMTargetParser.a", "install/lib/libLLVMBinaryFormat.a", "install/lib/libLLVMSupport.a", "install/lib/libLLVMDemangle.a", "link.map"):
        shutil.copyfile(out / name, evidence / Path(name).name)
    shutil.copytree(out / "generated/llvm/TargetParser", evidence / "targetparser-tables", dirs_exist_ok=True)
    result = {"elf_sha256": sha(elf), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=elf,
                                         label=LABEL, green=GREEN, tag="sh9-d3", commands=COMMANDS)
            check_records(transcript)
            (evidence / "qemu-serial.log").write_bytes(transcript)
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("SH9-d3 LLVM TargetParser QEMU green", flush=True)
        else:
            stage = args.stage.resolve()
            helpers["stage_image"](args.image.resolve(), stage, elf, COMMANDS)
            result["staged_tree"] = str(stage)
            print(f"SH9-d3 LLVM TargetParser staged: {stage}", flush=True)
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
