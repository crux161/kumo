#!/usr/bin/env python3
#J935_r01_001
"""Build SH9-a's native Rust unwind proof; optionally boot a disposable QEMU image."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess

ROOT = Path(__file__).resolve().parent.parent
GREEN = b"SH9-a native unwind green"
RECORDS = (
    b"SH9-a: payload and LIFO cleanup x64 -> OK",
    b"SH9-a: resume_unwind cleanup -> OK",
    b"SH9-a: concurrent catches and panicking worker join -> OK",
    b"SH9-a: real backtrace frames -> OK",
    GREEN,
)


def output(command):
    return subprocess.check_output(list(map(str, command)), text=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group()
    action.add_argument("--run", action="store_true")
    action.add_argument("--stage", type=Path, help="stage a disposable ESP for metal acceptance")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/sh9-a")
    parser.add_argument("--timeout", type=float, default=120)
    args = parser.parse_args()
    subprocess.run([ROOT / "scripts/rust-std-check.sh", "--unwind"], check=True)
    elf = ROOT / "target/rust-unwind-smoke/aarch64-unknown-kumo/release/unwind-smoke"
    undefined = output(["llvm-nm", "--undefined-only", elf])
    # Rust std retains its optional weak gettid probe (the existing shim owns
    # its syscall fallback). All strong undefined symbols must close. — KESTREL
    if any(line.split() != ["w", "gettid"] for line in undefined.splitlines()):
        raise RuntimeError(f"unresolved symbols: {undefined}")
    shim = ROOT / "target/rust-unwind-stage/stublibs/libkumo_shim.a"
    if "_Unwind_" in output(["llvm-nm", "--defined-only", shim]):
        raise RuntimeError("abort-only unwind stubs leaked into the native proof")
    defined = output(["llvm-nm", "--defined-only", elf])
    for symbol in ("_Unwind_RaiseException", "_Unwind_Resume", "_Unwind_Backtrace", "_Unwind_GetIP"):
        if not any(line.split()[-1:] == [symbol] for line in defined.splitlines()):
            raise RuntimeError(f"missing native unwind entry: {symbol}")
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    proof = evidence / "unwind-smoke"
    shutil.copyfile(elf, proof)
    shutil.copyfile(ROOT / "target/rust-unwind-stage/unwind/manifest.json", evidence / "unwind-manifest.json")
    (evidence / "elf.json").write_text(json.dumps({
        "sha256": hashlib.sha256(proof.read_bytes()).hexdigest(),
        "bytes": proof.stat().st_size,
        "undefined": undefined,
        "headers": output(["llvm-readelf", "--file-header", "--program-headers", proof]),
    }, indent=2) + "\n")
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    if args.run:
        transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=proof,
                                     label="SH9-a native unwind", green=GREEN, tag="SH9-a")
        positions = [transcript.find(record) for record in RECORDS]
        if any(transcript.count(record) != 1 for record in RECORDS) or positions != sorted(positions):
            raise RuntimeError("missing, duplicated, or out-of-order unwind proof records")
        print("SH9-a proof: all cleanup, payload, thread and backtrace records verified")
    elif args.stage:
        helpers["stage_image"](args.image.resolve(), args.stage.resolve(), proof,
                               b"dmesg -n 6\nrelibc sh9-a\necho SH9-a-after-launch\n")
        print(f"SH9-a metal proof staged: {args.stage.resolve()}")


if __name__ == "__main__":
    main()
