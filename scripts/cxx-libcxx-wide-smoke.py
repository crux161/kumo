#!/usr/bin/env python3
#J946_r12_001
"""Build and prove the pinned wide-character libc++ (SH9-c7) on a native KUMO image."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "SH9-c7 wide libc++"
GREEN = (LABEL + " green").encode()
COMMANDS = b"dmesg -n 6\nrelibc sh9-c7\necho sh9-c7-after-launch\n"
RECORDS = tuple(line.encode() for line in (
    "SH9-c7: wstring and wide numeric conversions -> OK",
    "SH9-c7: wide streams and wcout -> OK",
    "SH9-c7: wchar_t ctype, collate (C byname) and codecvt -> OK",
    "SH9-c7: wregex and wide format -> OK",
    "SH9-c7 wide libc++ green",
))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_records(transcript):
    """Every proof record exactly once, in order, after the begin marker."""
    proof = transcript.split((LABEL + " begin").encode(), 1)[1]
    positions = [proof.find(marker) for marker in RECORDS]
    if any(proof.count(marker) != 1 for marker in RECORDS) or positions != sorted(positions):
        raise RuntimeError("wide libc++ records are missing, duplicated, or out of order")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage a separate ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-libcxx-wide")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r12/sh9-c7")
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/cxx/build-libcxx.py", "--sysroot", args.sysroot, "--out", args.out,
             "--eh-out", args.eh_out,
             "--smoke", ROOT / "toolchain/cxx/libcxx-wide-smoke.cpp", "--elf-name", "cxx-libcxx-wide-smoke"]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "cxx-libcxx-wide-smoke"
    for name in ("cxx-libcxx-wide-smoke", "manifest.json", "undefined.txt", "install/lib/libc++.a", "link.map"):
        shutil.copyfile(out / name, evidence / Path(name).name)
    result = {"elf_sha256": sha(elf), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=elf,
                                         label=LABEL, green=GREEN, tag="sh9-c7", commands=COMMANDS)
            check_records(transcript)
            (evidence / "qemu-serial.log").write_bytes(transcript)
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("SH9-c7 wide libc++ QEMU green", flush=True)
        else:
            stage = args.stage.resolve()
            helpers["stage_image"](args.image.resolve(), stage, elf, COMMANDS)
            result["staged_tree"] = str(stage)
            print(f"SH9-c7 wide libc++ staged: {stage}", flush=True)
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
