#!/usr/bin/env python3
#J953_r19_001
"""Build and prove LLVM process launch (SH9-p1: ExecuteAndWait/ExecuteNoWait/Wait) on a native KUMO image."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "SH9-p1 LLVM process launch"
GREEN = (LABEL + " green").encode()
COMMANDS = b"dmesg -n 6\nrelibc sh9-p1\necho sh9-p1-after-launch\n"
RECORDS = tuple(line.encode() for line in (
    "SH9-p1: findProgramByName(SMOKE.ELF) = /SMOKE.ELF -> OK",
    "child says hi: ",
    "SH9-p1: ExecuteAndWait -> exit 7 -> OK",
    "child says hi: ",
    "SH9-p1: ExecuteNoWait + polling Wait -> exit 7 -> OK",
    "child says hi: ",
    "SH9-p1: ExecuteNoWait + blocking Wait -> exit 7 -> OK",
    "SH9-p1: refused: posix_spawn failed: No such file or directory",
    "SH9-p1: refused: posix_spawn failed: Operation not supported",
    "SH9-p1: missing program and file redirect refused cleanly -> OK",
    "SH9-p1 LLVM process launch green",
))
CHILD = b"child says hi: "


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_records(transcript):
    """Every proof record in order after the begin marker; exactly three child greetings, one
    before each launch record (stdin was /dev/null, so nothing follows "hi: ")."""
    proof = transcript.split((LABEL + " begin").encode(), 1)[1]
    position = 0
    for marker in RECORDS:
        found = proof.find(marker, position)
        if found < 0:
            raise RuntimeError(f"LLVM process records missing or out of order at {marker!r}")
        position = found + len(marker)
    if proof.count(CHILD) != 3 or any(proof.count(marker) != 1 for marker in RECORDS if marker != CHILD):
        raise RuntimeError("LLVM process records are duplicated or a child ran an unexpected number of times")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage a separate ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/llvm-process")
    parser.add_argument("--libcxx-out", type=Path, default=ROOT / "target/cxx-libcxx")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r19/sh9-p1")
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/llvm/build-process.py", "--sysroot", args.sysroot, "--out", args.out,
             "--libcxx-out", args.libcxx_out, "--eh-out", args.eh_out]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "llvm-process-smoke"
    for name in ("llvm-process-smoke", "manifest.json", "undefined.txt", "install/lib/libLLVMSupport.a", "install/lib/libLLVMDemangle.a", "link.map"):
        shutil.copyfile(out / name, evidence / Path(name).name)
    result = {"elf_sha256": sha(elf), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=elf,
                                         label=LABEL, green=GREEN, tag="sh9-p1", commands=COMMANDS)
            check_records(transcript)
            (evidence / "qemu-serial.log").write_bytes(transcript)
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("SH9-p1 LLVM process launch QEMU green", flush=True)
        else:
            stage = args.stage.resolve()
            helpers["stage_image"](args.image.resolve(), stage, elf, COMMANDS)
            result["staged_tree"] = str(stage)
            print(f"SH9-p1 LLVM process launch staged: {stage}", flush=True)
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
