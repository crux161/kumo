#!/usr/bin/env python3
#J957_r23_001
"""PROPOSAL/027 P2d: the session budget (`ruststd sessions`) on UEFI QEMU.

The proof is the image's own Rust std smoke (/SMOKE.ELF on the RAM FAT volume, which its
children are spawned from), so nothing is staged in place of `bin/relibc-smoke`: the
image's copy is extracted and staged back unchanged. Records are kernel-log ring lines
("std: p2d ..."), shown on serial after `dmesg -n 6`. — TEAL
"""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import struct

ROOT = Path(__file__).resolve().parent.parent
LABEL = "P2d session budget"
COMMANDS = b"dmesg -n 6\nruststd sessions\necho p2d-after-launch\n"
RECORDS = tuple(f"std: p2d {line}" for line in (
    "setup /P2D -> OK",
    "64 sessions at once: 7 granted children x 8 granted grandchildren -> OK",
    "the 65th session is refused at spawn with EAGAIN -> OK",
    "56 grandchildren each wrote and read their own file -> OK",
    "sessions come back when their processes exit -> OK",
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
        raise RuntimeError("P2d records are missing, duplicated, or out of order")
    if "std: p2d leaf " in proof:
        raise RuntimeError("a grandchild reported a failure")
    # 63 grants on top of the root's own session before the first one closes: all alive at once.
    fill = proof.split(RECORDS[1], 1)[0]
    events = [line.split("fatfs: session ", 1)[1][:7] for line in fill.splitlines() if "fatfs: session " in line]
    if events.count("granted") != 63 or "closed" in events:
        raise RuntimeError(f"the tree did not hold 64 sessions at once: {events.count('granted')} granted, closed={'closed' in events}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage an ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r23/p2d")
    parser.add_argument("--timeout", type=float, default=600)
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
                                         green=(LABEL + " green").encode(), tag="p2d", commands=COMMANDS)
            (evidence / "qemu-serial.log").write_bytes(transcript)
            check_records(transcript.decode("utf-8", "replace"))
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("P2d session budget QEMU green", flush=True)
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
