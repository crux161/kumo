#!/usr/bin/env python3
#J956_r22_001
"""Build and prove PROPOSAL/027 P2c (granted children's stdio redirected to files) on a native KUMO image."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "P2c spawn redirects"
GREEN = (LABEL + " green").encode()
COMMANDS = b"dmesg -n 6\nrelibc p2c\necho p2c-after-launch\n"
RECORDS = tuple(line.encode() for line in (
    "P2c: dup'd descriptors share one offset and outlive each other's close -> OK",
    "P2c: ExecuteAndWait <IN >OUT 2>ERR -> exit 7, both files exact -> OK",
    "P2c: ExecuteAndWait >BOTH 2>&1 -> stdout then stderr in one file -> OK",
    "P2c: posix_spawn O_TRUNC truncates once, O_APPEND appends twice -> OK",
    "P2c: a missing input and a missing directory fail posix_spawn with ENOENT -> OK",
    "child says hi: PING FROM A FILE",
    "P2c: a read-only grant refuses a writable redirect (EACCES) and reads its input -> OK",
    "P2c: without a grant a file redirect is still refused -> OK",
    "P2c: files removed -> OK",
    "P2c spawn redirects green",
))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_records(transcript):
    """Every record exactly once and in order after the begin marker. Only the read-only case's
    child writes to the console, so its greeting appears once: every other child wrote to files."""
    proof = transcript.split((LABEL + " begin").encode(), 1)[1]
    positions = [proof.find(marker) for marker in RECORDS]
    if any(proof.count(marker) != 1 for marker in RECORDS) or positions != sorted(positions):
        raise RuntimeError("P2c records are missing, duplicated, or out of order")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage a separate ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/llvm-redirect")
    parser.add_argument("--libcxx-out", type=Path, default=ROOT / "target/cxx-libcxx")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r22/p2c")
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/llvm/build-redirect.py", "--sysroot", args.sysroot, "--out", args.out,
             "--libcxx-out", args.libcxx_out, "--eh-out", args.eh_out]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "llvm-redirect-smoke"
    for name in ("llvm-redirect-smoke", "manifest.json", "undefined.txt", "install/lib/libLLVMSupport.a", "install/lib/libLLVMDemangle.a", "link.map"):
        shutil.copyfile(out / name, evidence / Path(name).name)
    result = {"elf_sha256": sha(elf), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=elf,
                                         label=LABEL, green=GREEN, tag="p2c", commands=COMMANDS)
            check_records(transcript)
            (evidence / "qemu-serial.log").write_bytes(transcript)
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("P2c spawn redirects QEMU green", flush=True)
        else:
            stage = args.stage.resolve()
            helpers["stage_image"](args.image.resolve(), stage, elf, COMMANDS)
            result["staged_tree"] = str(stage)
            print(f"P2c spawn redirects staged: {stage}", flush=True)
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
