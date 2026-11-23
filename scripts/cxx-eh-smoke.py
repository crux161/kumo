#!/usr/bin/env python3
#J936_r02_001
"""Build SH9-b1 and prove native C++ cleanup, RTTI, thread isolation and termination."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess

ROOT = Path(__file__).resolve().parent.parent
GREEN = b"SH9-b1 native C++ EH green"
RECORDS = (
    b"SH9-b1: typed catches, RTTI and LIFO cleanup x64 -> OK",
    b"SH9-b1: standard exception message and rethrow -> OK",
    b"SH9-b1: throwing static initializer retries exactly once -> OK",
    b"SH9-b1: allocation failure throws bad_alloc -> OK",
    b"SH9-b1: thread-local catches and exception_ptr x64 -> OK",
    GREEN,
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group()
    action.add_argument("--run", action="store_true", help="three disposable QEMU boots")
    action.add_argument("--stage", type=Path, help="stage the positive proof in a disposable ESP")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/sh9-b1")
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    subprocess.run(["python3", ROOT / "toolchain/cxx/build-eh.py"], check=True)
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    elf = evidence / "cxx-eh-smoke"
    for name in ("cxx-eh-smoke", "manifest.json", "undefined.txt", "libkumo-cxx-eh.a", "link.map"):
        shutil.copyfile(ROOT / "target/cxx-eh" / name, evidence / name)
    shutil.copyfile(ROOT / "target/cxx-eh/unwind/manifest.json", evidence / "unwind-manifest.json")
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    result = {"elf_sha256": hashlib.sha256(elf.read_bytes()).hexdigest(), "boots": {}}
    try:
        if args.run:
            text = helpers["boot"](args.image.resolve(), args.timeout, elf=elf,
                                   label="SH9-b1 native C++ EH", green=GREEN, tag="SH9-b1")
            positions = [text.find(record) for record in RECORDS]
            if any(text.count(record) != 1 for record in RECORDS) or positions != sorted(positions):
                raise RuntimeError("missing, duplicate or out-of-order C++ proof records")
            result["boots"]["positive"] = "PASS"
            for case in ("noexcept", "uncaught"):
                label = f"SH9-b1 {case}"
                helpers["boot"](args.image.resolve(), args.timeout, elf=elf, label=label,
                                green=(label + " green").encode(), tag=f"SH9-b1-{case}")
                result["boots"][case] = "PASS"
            print("SH9-b1 native C++ EH suite green: positive, noexcept, uncaught", flush=True)
        elif args.stage:
            helpers["stage_image"](args.image.resolve(), args.stage.resolve(), elf,
                                   b"dmesg -n 6\nrelibc sh9-b1\necho SH9-b1-after-launch\n")
            print(f"SH9-b1 proof staged: {args.stage.resolve()}")
    finally:
        (evidence / "proof.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
