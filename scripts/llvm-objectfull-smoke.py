#!/usr/bin/env python3
#J960_r26_001
"""Build and prove LLVM Object complete closure and prerequisites (SH9-d8) on native KUMO."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "SH9-d8 LLVM Object closure"
GREEN = (LABEL + " green").encode()
COMMANDS = b"dmesg -n 6\nrelibc sh9-d8\necho sh9-d8-after-launch\n"
RECORDS = tuple(line.encode() for line in (
    'SH9-d8: compiler-rt cache builtin runs through LLVM on an unaligned private range -> OK',
    'SH9-d8: MC lexer identifiers, directives, integers, strings and unterminated-string error -> OK',
    'SH9-d8: generic binary factory reads ELF64LE, ELF32BE, COFF, Mach-O and Wasm -> OK',
    'SH9-d8: generic bitcode factory exposes KUMO IR object symbols and definitions -> OK',
    'SH9-d8: TextAPI YAML v4 and JSON v5 round trips, weak exports, TAPI object adapter and invalid input -> OK',
    'SH9-d8: native GNU archive bytes repeat exactly, mixed ELF/bitcode members and symbol index read back -> OK',
    'SH9-d8: generic factories reject truncated ELF, COFF, Mach-O, Wasm and unknown magic -> OK',
    'SH9-d8: native archive stream saved and loaded through file factory, missing file refused and cleanup -> OK',
    'SH9-d8 LLVM Object closure green',
))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_records(transcript):
    """Every proof record exactly once, in order, within the launch window."""
    begin, end = (LABEL + " begin").encode(), b"sh9-d8-after-launch"
    if transcript.count(begin) != 1 or transcript.count(end) != 1:
        raise RuntimeError("expected one Object proof window")
    start, stop = transcript.index(begin) + len(begin), transcript.index(end)
    if stop <= start:
        raise RuntimeError("Object launch returned before its begin record")
    proof = transcript[start:stop]
    positions = [proof.find(marker) for marker in RECORDS]
    if (any(proof.count(marker) != 1 for marker in RECORDS) or positions != sorted(positions)
            or b"TOWER" in proof or b"FAILED" in proof):
        raise RuntimeError("LLVM Object closure records are missing, duplicated, or out of order")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage a separate ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/llvm-objectfull")
    parser.add_argument("--libcxx-out", type=Path, default=ROOT / "target/cxx-libcxx")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r26/sh9-d8")
    parser.add_argument("--tblgen", type=Path, help="matching LLVM 22.1.8 llvm-tblgen")
    parser.add_argument("--timeout", type=float, default=180)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/llvm/build-objectfull.py", "--sysroot", args.sysroot, "--out", args.out,
             "--libcxx-out", args.libcxx_out, "--eh-out", args.eh_out]
    if args.tblgen:
        build += ["--tblgen", args.tblgen]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "llvm-objectfull-smoke"
    manifest = json.loads((out / "manifest.json").read_text())
    for name in ("llvm-objectfull-smoke", "manifest.json", "undefined.txt", "link.map"):
        shutil.copyfile(out / name, evidence / name)
    for name in manifest["archives_sha256"]:
        shutil.copyfile(out / "install/lib" / name, evidence / name)
    shutil.copyfile(out / "install/lib/libkumo-builtins.a", evidence / "libkumo-builtins.a")
    for name in ("fixtures", "ir-fixtures", "format-fixtures", "generated"):
        shutil.copytree(out / name, evidence / name, dirs_exist_ok=True)
    runtime = evidence / "runtime"
    runtime.mkdir(exist_ok=True)
    for name in ("libc++.a", "libc++abi.a", "libunwind.a"):
        shutil.copyfile(args.libcxx_out.resolve() / "install/lib" / name, runtime / name)
    shutil.copyfile(args.sysroot.resolve() / "lib/libc.a", runtime / "libc.a")
    result = {"elf_sha256": sha(elf), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=elf,
                                         label=LABEL, green=GREEN, tag="sh9-d8", commands=COMMANDS)
            check_records(transcript)
            (evidence / "qemu-serial.log").write_bytes(transcript)
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("SH9-d8 LLVM Object closure QEMU green", flush=True)
        else:
            stage = args.stage.resolve()
            helpers["stage_image"](args.image.resolve(), stage, elf, COMMANDS)
            result["staged_tree"] = str(stage)
            print(f"SH9-d8 LLVM Object closure staged: {stage}", flush=True)
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
