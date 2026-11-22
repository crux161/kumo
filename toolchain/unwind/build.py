#!/usr/bin/env python3
#J935_r01_001
"""Build the pinned LLVM unwinder for static native KUMO AArch64 images, offline."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "toolchain/unwind"


def run(command):
    subprocess.run(list(map(str, command)), check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/kumo-unwind")
    args = parser.parse_args()
    sysroot, out = args.sysroot.resolve(), args.out.resolve()
    lock = json.loads((TOOL / "SOURCE.lock").read_text())
    for name, pin in lock["files"].items():
        data = (TOOL / "vendor" / name).read_bytes()
        blob = b"blob " + str(len(data)).encode() + b"\0" + data
        if (hashlib.sha256(data).hexdigest() != pin["sha256"]
                or hashlib.sha1(blob).hexdigest() != pin["git_blob"]):
            raise RuntimeError(f"upstream source pin mismatch: {name}")
    if not (sysroot / "include/stdio.h").is_file():
        raise RuntimeError("missing relibc headers; run scripts/relibc-headers.sh")
    out.mkdir(parents=True, exist_ok=True)
    # Upstream's static-image table discovery needs no dl_iterate_phdr or Linux
    # syscall path. Keep real pthread locks for its process-wide FDE cache, even
    # on today's single core: native threads can preempt each other. — KESTREL
    flags = ["--target=aarch64-unknown-none", f"--sysroot={sysroot}",
             "-isystem", str(sysroot / "include"), "-I", str(TOOL / "vendor/include"),
             "-D__kumo__", "-D_LIBUNWIND_IS_BAREMETAL", "-D_LIBUNWIND_IS_NATIVE_ONLY",
             "-DNDEBUG", "-O2", "-ffreestanding", "-fno-stack-protector",
             "-fvisibility=hidden", "-funwind-tables", "-ffunction-sections", "-fdata-sections"]
    sources = ["libunwind.cpp", "UnwindLevel1.c", "UnwindLevel1-gcc-ext.c",
               "UnwindRegistersRestore.S", "UnwindRegistersSave.S"]
    objects = []
    for name in sources:
        obj = out / (name + ".o")
        extra = (["-std=c++17", "-nostdinc++", "-fno-exceptions", "-fno-rtti"]
                 if name.endswith(".cpp") else ["-std=c99", "-fexceptions"]
                 if name.endswith(".c") else [])
        run(["clang", *flags, *extra, "-MD", "-MF", out / (name + ".d"),
             "-c", TOOL / "vendor/src" / name, "-o", obj])
        objects.append(obj)
    archive = out / "libunwind.a"
    archive.unlink(missing_ok=True)
    run(["llvm-ar", "rcs", archive, *objects])
    inputs = {TOOL / "SOURCE.lock", TOOL / "sections.ld", Path(__file__).resolve()}
    for name in sources:
        dep = (out / (name + ".d")).read_text().replace("\\\n", " ")
        inputs.update(Path(p) for p in dep.split(":", 1)[1].split())
    for path in inputs:
        if str(path).startswith(("/usr/include/", "/Library/Developer/")):
            raise RuntimeError(f"host header contamination: {path}")
    (out / "manifest.json").write_text(json.dumps({
        "llvm": lock["ref"],
        "target": "aarch64-unknown-kumo",
        "clang": subprocess.check_output(["clang", "--version"], text=True).strip(),
        "flags": flags,
        "inputs_sha256": {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
                          for p in sorted(inputs)},
        "archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
    }, indent=2) + "\n")
    print(f"KUMO native libunwind: {archive}", flush=True)


if __name__ == "__main__":
    main()
