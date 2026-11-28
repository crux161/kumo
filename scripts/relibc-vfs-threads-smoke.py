#!/usr/bin/env python3
#J954_r20_001
"""PROPOSAL/027 P2a: concurrent VFS calls from many threads of one relibc process, on UEFI QEMU.

Builds toolchain/relibc/smoke/vfs-threads.cpp against the installed libc++ layer and relibc, boots
it through Sora's `relibc` word, and checks its records in order. `--stage DIR` stages the same ELF
for a metal run instead. — TEAL
"""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "P2a VFS threads"
COMMANDS = b"dmesg -n 6\nrelibc p2a\necho p2a-after-launch\n"
RECORDS = (
    "P2a: 8 files of distinct sizes and bytes on the volume -> OK",
    "P2a: 8 threads x 200 rounds: wrong stat 0, wrong read 0, wrong listing 0, failed calls 0",
    "P2a: concurrent stat/open/read/readdir on one session, no crossed replies -> OK",
    "P2a: files removed -> OK",
    LABEL + " green",
)


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


# The `relibc` word now launches on a mount-scoped fatfs (it hands out the session-control
# channel); a per-launch server says only "fatfs: serving".
MOUNT = b"fatfs: serving a mount"


def check_records(transcript):
    before, proof = transcript.split((LABEL + " begin").encode(), 1)
    if MOUNT not in before.rsplit(b"relibc: C runtime smoke", 1)[-1]:
        raise RuntimeError("the relibc launch was not served by a mount-scoped fatfs")
    records = [record.encode() for record in RECORDS]
    positions = [proof.find(record) for record in records]
    if any(proof.count(record) != 1 for record in records) or positions != sorted(positions):
        raise RuntimeError("P2a records are missing, duplicated, or out of order")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage a separate ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/relibc-vfs-threads")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r20/p2a")
    parser.add_argument("--timeout", type=float, default=180)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/cxx/build-libcxx.py", "--sysroot", args.sysroot, "--out", args.out,
             "--eh-out", args.eh_out, "--smoke", ROOT / "toolchain/relibc/smoke/vfs-threads.cpp",
             "--elf-name", "relibc-vfs-threads"]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "relibc-vfs-threads"
    for name in ("relibc-vfs-threads", "manifest.json", "undefined.txt", "link.map"):
        shutil.copyfile(out / name, evidence / name)
    result = {"elf_sha256": sha(elf), "libc_sha256": sha(args.sysroot.resolve() / "lib/libc.a"), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=elf, label=LABEL,
                                         green=(LABEL + " green").encode(), tag="p2a", commands=COMMANDS)
            (evidence / "qemu-serial.log").write_bytes(transcript)
            check_records(transcript)
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("P2a VFS threads QEMU green", flush=True)
        else:
            stage = args.stage.resolve()
            helpers["stage_image"](args.image.resolve(), stage, elf, COMMANDS)
            result["staged_tree"] = str(stage)
            print(f"P2a VFS threads staged: {stage}", flush=True)
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
