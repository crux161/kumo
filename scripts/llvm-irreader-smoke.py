#!/usr/bin/env python3
#J959_r25_001
"""Build and prove LLVM IR readers + native llvm-dis and prerequisites (SH9-d7) on native KUMO."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import struct
import time
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "SH9-d7 LLVM IR readers"
GREEN = (LABEL + " green").encode()
COMMANDS = b"dmesg -n 6\nrelibc sh9-d7\necho sh9-d7-after-launch\n"
RECORDS = tuple(line.encode() for line in (
    'SH9-d7: textual IR parses SSA, globals, attributes, intrinsics and debug metadata -> OK',
    'SH9-d7: printed IR reparses, malformed syntax and undefined values report diagnostics -> OK',
    'SH9-d7: host LLVM 22 bitcode reads eagerly with matching IR and debug semantics -> OK',
    'SH9-d7: lazy functions and metadata materialize, bad magic and truncated bitcode return errors -> OK',
    'SH9-d7: IRReader dispatches text and bitcode in memory and on RAM FAT, missing file diagnosed -> OK',
    'SH9-d7: upstream native llvm-dis reads bitcode and writes reparsable annotated IR in a granted child -> OK',
    'SH9-d7: native llvm-dis reports LLVM 22 version and missing-input failure, temporary files removed -> OK',
    'SH9-d7 LLVM IR readers green',
))


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_records(transcript):
    """Every proof record exactly once, in order, within the launch window."""
    begin, end = (LABEL + " begin").encode(), b"sh9-d7-after-launch"
    if transcript.count(begin) != 1 or transcript.count(end) != 1:
        raise RuntimeError("expected one IR-reader proof window")
    start, stop = transcript.index(begin) + len(begin), transcript.index(end)
    if stop <= start:
        raise RuntimeError("IR-reader launch returned before its begin record")
    proof = transcript[start:stop]
    positions = [proof.find(marker) for marker in RECORDS]
    if (any(proof.count(marker) != 1 for marker in RECORDS) or positions != sorted(positions)
            or b"TOWER" in proof or b"FAILED" in proof):
        raise RuntimeError("LLVM IR readers records are missing, duplicated, or out of order")



def make_volume(tool, destination):
    """The proof's own RAM volume; sufficient space for a native host tool. — TEAL"""
    programs = {name: Path(shutil.which(name)).absolute() if shutil.which(name) else None
                for name in ("mformat", "mcopy", "mmd")}
    if not all(programs.values()):
        raise RuntimeError("mtools (mformat, mcopy, mmd) is required for the disposable proof volume")
    with destination.open("wb") as file:
        file.truncate(40 * 1024 * 1024)
    def run(name, *arguments):
        subprocess.run([str(programs[name]), "-i", str(destination), *map(str, arguments)], check=True)
    run("mformat", "-F", "-c", "1", "-N", "0x53483937", "-v", "SH9D7", "::")
    # KUMO's RAM fixture has no BIOS boot code. Match its marker in both BPBs. — TEAL
    with destination.open("r+b") as file:
        header = file.read(512)
        backup = struct.unpack_from("<H", header, 0x32)[0]
        for offset in (0, backup * 512):
            file.seek(offset)
            file.write(b"\xeb\xfe\x90")
    run("mmd", "::/EFI", "::/EFI/BOOT")
    hello = destination.with_name("hello.txt")
    hello.write_bytes(b"hello!")
    esp = destination.with_name("esp.txt")
    esp.write_bytes(b"esp-ok")
    run("mcopy", hello, "::/HELLO.TXT")
    run("mcopy", esp, "::/EFI/BOOT/BOOTAA64.EFI")
    run("mcopy", tool, "::/DIS.ELF")
    extracted = destination.with_name("dis-readback.elf")
    run("mcopy", "::/DIS.ELF", extracted)
    if sha(extracted) != sha(tool):
        raise RuntimeError("native llvm-dis bytes changed in the proof FAT volume")
    extracted.unlink()
    return {"sha256": sha(destination), "bytes": destination.stat().st_size,
            "tool_sha256": sha(tool), "mtools": {name: {"path": str(path), "sha256": sha(path)}
                                                  for name, path in programs.items()}}


def stage_image(source, destination, elf, volume, commands=COMMANDS):
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    helpers["stage_image"](source, destination, elf, commands)
    path = destination / "EFI/KUMO/initrd.img"
    original = path.read_bytes()
    count = struct.unpack_from("<I", original, 12)[0]
    out = bytearray(original[:16 + 80 * count])
    found = False
    for index in range(count):
        entry = 16 + index * 80
        name = original[entry:entry + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", original, entry + 64)
        data = original[offset:offset + size]
        if name == b"bin/fat32.img":
            data = volume.read_bytes()
            found = True
        struct.pack_into("<QQ", out, entry + 64, len(out), len(data))
        out.extend(data)
    if not found:
        raise RuntimeError("proof image has no RAM FAT volume to replace")
    path.write_bytes(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--run", action="store_true", help="boot the proof once under UEFI QEMU")
    action.add_argument("--stage", type=Path, metavar="DIR", help="stage a separate ESP for a later metal run")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/llvm-irreader")
    parser.add_argument("--libcxx-out", type=Path, default=ROOT / "target/cxx-libcxx")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r25/sh9-d7")
    parser.add_argument("--tblgen", type=Path, help="matching LLVM 22.1.8 llvm-tblgen")
    parser.add_argument("--timeout", type=float, default=240)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/llvm/build-irreader.py", "--sysroot", args.sysroot, "--out", args.out,
             "--libcxx-out", args.libcxx_out, "--eh-out", args.eh_out]
    if args.tblgen:
        build += ["--tblgen", args.tblgen]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "llvm-irreader-smoke"
    manifest = json.loads((out / "manifest.json").read_text())
    for name in ("llvm-irreader-smoke", "llvm-dis", "llvm-dis.map", "manifest.json", "undefined.txt", "link.map"):
        shutil.copyfile(out / name, evidence / name)
    for name in manifest["archives_sha256"]:
        shutil.copyfile(out / "install/lib" / name, evidence / name)
    shutil.copytree(out / "fixtures", evidence / "fixtures", dirs_exist_ok=True)
    runtime = evidence / "runtime"
    runtime.mkdir(exist_ok=True)
    for name in ("libc++.a", "libc++abi.a", "libunwind.a"):
        shutil.copyfile(args.libcxx_out.resolve() / "install/lib" / name, runtime / name)
    shutil.copyfile(args.sysroot.resolve() / "lib/libc.a", runtime / "libc.a")
    volume = evidence / "proof-fat32.img"
    volume_manifest = make_volume(evidence / "llvm-dis", volume)
    (evidence / "volume.json").write_text(json.dumps(volume_manifest, indent=2) + "\n")
    shutil.copytree(out / "generated", evidence / "generated", dirs_exist_ok=True)
    result = {"elf_sha256": sha(elf), "fat_sha256": sha(volume), "native_tool_sha256": sha(evidence / "llvm-dis"), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        if args.run:
            prepared = evidence / f"prepared-image-{time.time_ns()}"
            stage_image(args.image.resolve(), prepared, elf, volume)
            transcript = helpers["boot"](prepared, args.timeout, elf=elf,
                                         label=LABEL, green=GREEN, tag="sh9-d7", commands=COMMANDS)
            check_records(transcript)
            if b"sector 0 BPB  via sora   OK" not in transcript:
                raise RuntimeError("RAM FAT boot probe did not pass")
            (evidence / "qemu-serial.log").write_bytes(transcript)
            result["boots"]["qemu"] = "PASS"
            result["serial_sha256"] = sha(evidence / "qemu-serial.log")
            print("SH9-d7 LLVM IR readers QEMU green", flush=True)
        else:
            stage = args.stage.resolve()
            stage_image(args.image.resolve(), stage, elf, volume)
            result["staged_tree"] = str(stage)
            print(f"SH9-d7 LLVM IR readers staged: {stage}", flush=True)
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
