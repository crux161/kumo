#!/usr/bin/env python3
#J955_r21_001
"""PROPOSAL/027 P2b: the child filesystem grant tree (`ruststd grants`) on UEFI QEMU.

The proof is the image's own Rust std smoke (/SMOKE.ELF on the RAM FAT volume, which its
children are spawned from), so nothing is staged in place of `bin/relibc-smoke`: the
image's copy is extracted and staged back unchanged. Records are kernel-log ring lines
("std: p2b ..."), shown on serial after `dmesg -n 6`. — TEAL
"""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import struct

ROOT = Path(__file__).resolve().parent.parent
LABEL = "P2b child grants"
COMMANDS = b"dmesg -n 6\nruststd grants\necho p2b-after-launch\n"
RECORDS = tuple(f"std: p2b {line}" for line in (
    "setup /P2B -> OK",
    "read-write tree: child and grandchild wrote their files, marker inherited -> OK",
    "read-only grant: reads, write refused EACCES, its child gets nothing -> OK",
    "no marker: the child cannot open anything -> OK",
    "3 granted children and the parent do I/O concurrently, no crossed replies -> OK",
    "files removed -> OK",
    LABEL + " green",
))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def initrd_entry(image, wanted):
    data = (image / "EFI/KUMO/initrd.img").read_bytes()
    assert data[:8] == b"KUMORD01"
    for index in range(struct.unpack_from("<I", data, 12)[0]):
        entry = 16 + index * 80
        name = data[entry:entry + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", data, entry + 64)
        if name == wanted:
            return data[offset:offset + size]
    raise RuntimeError(f"{wanted!r} not in the initrd")


def check_records(text):
    proof = text.split(LABEL + " begin", 1)[1]
    positions = [proof.find(record) for record in RECORDS]
    if any(proof.count(record) != 1 for record in RECORDS) or positions != sorted(positions):
        raise RuntimeError("P2b records are missing, duplicated, or out of order")
    if "std: p2b child " in proof:
        raise RuntimeError("a grant-tree child reported a failure")
    # The busy children overlap: all three sessions are open before the first one closes.
    window = proof.split(RECORDS[3], 1)[1].split(RECORDS[4], 1)[0]
    events = [line.split("fatfs: session ", 1)[1][:7] for line in window.splitlines() if "fatfs: session " in line]
    if events[:3] != ["granted"] * 3:
        raise RuntimeError(f"the busy children did not overlap: {events}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage an ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r21/p2b")
    parser.add_argument("--timeout", type=float, default=240)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    image = args.image.resolve()
    unchanged = evidence / "relibc-smoke"
    unchanged.write_bytes(initrd_entry(image, b"bin/relibc-smoke"))
    std_smoke = initrd_entry(image, b"bin/std-smoke")
    result = {"std_smoke_sha256": hashlib.sha256(std_smoke).hexdigest(), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            transcript = helpers["boot"](image, args.timeout, elf=unchanged, label=LABEL,
                                         green=(LABEL + " green").encode(), tag="p2b", commands=COMMANDS)
            (evidence / "qemu-serial.log").write_bytes(transcript)
            check_records(transcript.decode("utf-8", "replace"))
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("P2b child grants QEMU green", flush=True)
        else:
            helpers["stage_image"](image, args.stage.resolve(), unchanged, COMMANDS)
            result["staged_tree"] = str(args.stage.resolve())
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
