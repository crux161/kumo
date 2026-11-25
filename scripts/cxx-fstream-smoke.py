#!/usr/bin/env python3
#J942_r08_001
"""Build and prove the pinned libc++ read-only file stream slice on a native KUMO image."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "SH9-c4 libc++ file streams"
GREEN = (LABEL + " green").encode()
COMMANDS = b"dmesg -n 6\nrelibc sh9-c4\necho sh9-c4-after-launch\n"
RECORDS = tuple(line.encode() for line in (
    "SH9-c4: ifstream reads a text fixture -> OK",
    "SH9-c4: binary read, seek and tell -> OK",
    "SH9-c4: missing-file failure and ios_base::failure -> OK",
    "SH9-c4: classic-locale number and time facets -> OK",
    "SH9-c4 libc++ file streams green",
))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_records(transcript):
    """Every proof record exactly once, in order, after the begin marker."""
    proof = transcript.split((LABEL + " begin").encode(), 1)[1]
    positions = [proof.find(marker) for marker in RECORDS]
    if any(proof.count(marker) != 1 for marker in RECORDS) or positions != sorted(positions):
        raise RuntimeError("file stream records are missing, duplicated, or out of order")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage a separate ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-fstream")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r08/sh9-c4")
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/cxx/build-fstream.py", "--sysroot", args.sysroot, "--out", args.out,
             "--eh-out", args.eh_out]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "cxx-fstream-smoke"
    for name in ("cxx-fstream-smoke", "manifest.json", "undefined.txt", "libkumo-cxx-locale.a", "link.map"):
        shutil.copyfile(out / name, evidence / name)
    result = {"elf_sha256": sha(elf), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=elf,
                                         label=LABEL, green=GREEN, tag="sh9-c4", commands=COMMANDS)
            check_records(transcript)
            (evidence / "qemu-serial.log").write_bytes(transcript)
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("SH9-c4 libc++ file streams QEMU green", flush=True)
        else:
            stage = args.stage.resolve()
            helpers["stage_image"](args.image.resolve(), stage, elf, COMMANDS)
            result["staged_tree"] = str(stage)
            print(f"SH9-c4 libc++ file streams staged: {stage}", flush=True)
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
