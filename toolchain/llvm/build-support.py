#!/usr/bin/env python3
#J948_r14_001
"""Build LLVM's Support and Demangle libraries (llvmorg-22.1.8) for native KUMO.

The sources are the measured closure pinned in toolchain/llvm/SOURCE.lock; toolchain/llvm/kumo.patch
ports LLVM's Unix layer to KUMO on a staged copy; gen-config.py renders the CMake config headers.
Everything compiles against the installed libc++ layer (toolchain/cxx/build-libcxx.py) and relibc,
and the proof links against the resulting archives alone. — TEAL
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "toolchain/llvm"
CXX = ROOT / "toolchain/cxx"
# LLVM's own tools run deep (parsers, the demangler, CommandLine): give thread zero a real stack.
MAIN_STACK = 4 * 1024 * 1024


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(command, *, capture=False, cwd=None):
    command = list(map(str, command))
    if capture:
        return subprocess.check_output(command, text=True, cwd=cwd)
    subprocess.run(command, check=True, cwd=cwd)
    return ""


def units(source):
    """Support's portable sources plus BLAKE3's portable and NEON paths (no x86 SIMD), and Demangle."""
    support = source / "llvm/lib/Support"
    found = sorted(support.glob("*.cpp")) + sorted(support.glob("*.c"))
    found += [support / "BLAKE3" / name for name in ("blake3.c", "blake3_dispatch.c", "blake3_portable.c", "blake3_neon.c")]
    demangle = sorted((source / "llvm/lib/Demangle").glob("*.cpp"))
    return found, demangle


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/llvm-support")
    parser.add_argument("--libcxx-out", type=Path, default=ROOT / "target/cxx-libcxx")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    args = parser.parse_args()
    sysroot, out = args.sysroot.resolve(), args.out.resolve()
    libcxx_out, eh_out = args.libcxx_out.resolve(), args.eh_out.resolve()
    libc = sysroot / "lib/libc.a"
    run([sys.executable, CXX / "build-libcxx.py", "--sysroot", sysroot, "--out", libcxx_out, "--eh-out", eh_out])
    layer = libcxx_out / "install"
    for needed in ("include/c++/v1/__config_site", "lib/libc++.a", "lib/libc++abi.a", "lib/libunwind.a"):
        if not (layer / needed).is_file():
            raise RuntimeError(f"installed libc++ layer lacks {needed}")

    lock_path = TOOL / "SOURCE.lock"
    lock = json.loads(lock_path.read_text())
    vendor = TOOL / "vendor"
    for name, pin in lock["files"].items():
        data = (vendor / name).read_bytes()
        blob = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
        if hashlib.sha256(data).hexdigest() != pin["sha256"] or blob != pin["git_blob"]:
            raise RuntimeError(f"source pin mismatch: {name}")
    source = out / "source"
    if source.exists():
        shutil.rmtree(source)
    for name in lock["files"]:
        destination = source / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(vendor / name, destination)
    # Inside this repository `git apply` resolves paths from the repository root, not the cwd,
    # and would skip a staged tree silently; name the stage as the patch directory then, as
    # scripts/relibc-check.sh does. Outside it, plain `git apply` from the stage works. — TEAL
    if source.is_relative_to(ROOT):
        run(["git", "-C", ROOT, "apply", f"--directory={source.relative_to(ROOT)}", TOOL / "kumo.patch"])
    else:
        run(["git", "apply", TOOL / "kumo.patch"], cwd=source)
    if "__kumo__" not in (source / "llvm/lib/Support/Unix/Path.inc").read_text():
        raise RuntimeError("kumo.patch did not apply to the staged LLVM sources")
    generated = out / "generated"
    if generated.exists():
        shutil.rmtree(generated)
    runpy.run_path(str(TOOL / "gen-config.py"))["generate"](source / "llvm/include/llvm/Config", generated)

    out.mkdir(parents=True, exist_ok=True)
    common = ["--target=aarch64-unknown-none", f"--sysroot={sysroot}", "-D__kumo__", "-O2", "-ffreestanding",
              "-fno-pic", "-fno-pie", "-fno-stack-protector", "-ffunction-sections", "-fdata-sections",
              "-D_GNU_SOURCE", "-D__STDC_CONSTANT_MACROS", "-D__STDC_FORMAT_MACROS", "-D__STDC_LIMIT_MACROS",
              "-DLLVM_BUILD_STATIC", "-I", generated, "-I", source / "llvm/include",
              "-I", source / "third-party/siphash/include"]
    cxx = ["-nostdinc++", "-isystem", layer / "include/c++/v1", "-isystem", sysroot / "include", "-std=c++17",
           "-fexceptions", "-frtti", "-funwind-tables"]
    c = ["-isystem", sysroot / "include", "-std=c11"]
    objects = out / "objects"
    objects.mkdir(exist_ok=True)

    def compile_unit(path):
        rel = path.relative_to(source)
        obj = objects / (str(rel).replace("/", "_") + ".o")
        lang = cxx if path.suffix == ".cpp" else c
        driver = "clang++" if path.suffix == ".cpp" else "clang"
        run([driver, *common, "-I", path.parent, *lang, "-MD", "-MF", f"{obj}.d", "-c", path, "-o", obj])
        return obj

    support_units, demangle_units = units(source)
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        support_objects = list(pool.map(compile_unit, support_units))
        demangle_objects = list(pool.map(compile_unit, demangle_units))
    libdir = out / "install/lib"
    libdir.mkdir(parents=True, exist_ok=True)
    support_archive, demangle_archive = libdir / "libLLVMSupport.a", libdir / "libLLVMDemangle.a"
    for archive, members in ((support_archive, support_objects), (demangle_archive, demangle_objects)):
        archive.unlink(missing_ok=True)
        run(["llvm-ar", "rcs", archive, *members])

    smoke = TOOL / "support-smoke.cpp"
    smoke_obj = out / "smoke.o"
    run(["clang++", *common, *cxx, "-MD", "-MF", f"{smoke_obj}.d", "-c", smoke, "-o", smoke_obj])
    crt = out / "crt0.o"
    run(["clang", "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S", "-o", crt])
    libcxx_build = runpy.run_path(str(CXX / "build-libcxx.py"))
    script = libcxx_build["stack_link_script"](out)
    elf = out / "llvm-support-smoke"
    run(["ld.lld", "-static", "--gc-sections", "--eh-frame-hdr", "-z", "noexecstack",
         "-T", CXX / "eh-sections.ld", "-T", script, f"-Map={out / 'link.map'}", "-o", elf, crt, smoke_obj,
         "--start-group", support_archive, demangle_archive, layer / "lib/libc++.a", layer / "lib/libc++abi.a",
         layer / "lib/libunwind.a", libc, "--end-group"])
    libcxx_build["set_stack_size"](elf, MAIN_STACK)
    undefined = run(["llvm-nm", "--undefined-only", elf], capture=True)
    (out / "undefined.txt").write_text(undefined)
    if undefined.strip():
        raise RuntimeError(f"unresolved symbols in the LLVM Support smoke:\n{undefined}")

    manifest = {
        "llvm_revision": lock["revision"],
        "llvm_ref": lock["ref"],
        "scope": "LLVM Support + Demangle for aarch64-unknown-kumo against the installed libc++ layer; "
                 "Unix layer ported by toolchain/llvm/kumo.patch; config rendered by gen-config.py",
        "units": {"support": len(support_units), "demangle": len(demangle_units)},
        "clang": run(["clang++", "--version"], capture=True).strip(),
        "inputs_sha256": {str(path): sha(path) for path in (
            Path(__file__).resolve(), lock_path, TOOL / "kumo.patch", TOOL / "gen-config.py", smoke,
            CXX / "build-libcxx.py", libc, layer / "lib/libc++.a", layer / "include/c++/v1/__config_site")},
        "generated_sha256": {str(path.relative_to(generated)): sha(path)
                             for path in sorted(generated.rglob("*")) if path.is_file()},
        "archives_sha256": {path.name: sha(path) for path in (support_archive, demangle_archive)},
        "main_stack_bytes": MAIN_STACK,
        "elf_sha256": sha(elf),
        "elf_bytes": elf.stat().st_size,
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"SH9-d1 LLVM Support compile/link green: {elf} "
          f"({len(support_units)} Support + {len(demangle_units)} Demangle units)", flush=True)


if __name__ == "__main__":
    main()
