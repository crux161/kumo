#!/usr/bin/env python3
#J939_r05_001
"""Build and prove the pinned libc++ filesystem path slice on a native KUMO image."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "SH9-c1 libc++ filesystem path"
GREEN = (LABEL + " green").encode()
RECORDS = tuple(line.encode() for line in (
    "SH9-c1: decomposition, extension, and root -> OK",
    "SH9-c1: lexical normalization -> OK",
    "SH9-c1: append and replace_extension -> OK",
    "SH9-c1: path iteration -> OK",
    "SH9-c1 libc++ filesystem path green",
))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, help="stage a separate ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-fs-path")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r05/sh9-c1")
    parser.add_argument("--stage-dir", type=Path, help="destination for --stage (required with --stage)")
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/cxx/build-fs-path.py", "--sysroot", args.sysroot,
             "--out", args.out, "--eh-out", args.eh_out]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "cxx-fs-path-smoke"
    products = ("cxx-fs-path-smoke", "manifest.json", "undefined.txt", "libkumo-cxx-fs-path.a", "link.map")
    for name in products:
        shutil.copyfile(out / name, evidence / name)
    result = {"elf_sha256": sha(elf), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=elf,
                                            label=LABEL, green=GREEN, tag="sh9-c1")
            start, proof = transcript.split((LABEL + " begin").encode(), 1)
            del start
            positions = [proof.find(marker) for marker in RECORDS]
            if any(proof.count(marker) != 1 for marker in RECORDS) or positions != sorted(positions):
                raise RuntimeError("filesystem path records are missing, duplicated, or out of order")
            (evidence / "qemu-serial.log").write_bytes(transcript)
            result["boots"]["qemu"] = "PASS"
            print("SH9-c1 libc++ filesystem path QEMU green", flush=True)
        else:
            if args.stage_dir is None:
                raise RuntimeError("--stage requires --stage-dir")
            stage = args.stage_dir.resolve()
            helpers["stage_image"](args.image.resolve(), stage, elf,
                                    b"dmesg -n 6\nrelibc sh9-c1\necho SH9-c1-after-launch\n")
            result["staged_tree"] = str(stage)
            print(f"SH9-c1 libc++ filesystem path staged: {stage}", flush=True)
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
