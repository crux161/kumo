#!/usr/bin/env python3
#J939_r05_001
"""Build the pinned LLVM libc++ filesystem path slice for native KUMO."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "toolchain/cxx"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(command, *, capture=False):
    command = list(map(str, command))
    if capture:
        return subprocess.check_output(command, text=True)
    subprocess.run(command, check=True)
    return ""


def source_dependencies(depfile, source_root):
    text = depfile.read_text().replace("\\\n", " ")
    paths = text.split(":", 1)[1].split()
    result = set()
    for raw in paths:
        path = Path(raw).resolve()
        if path.is_file():
            if str(path).startswith(("/usr/include/", "/Library/Developer/")) or "include/c++/v1" in str(path):
                raise RuntimeError(f"host C++ header contamination: {path}")
            result.add(path)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-fs-path")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    args = parser.parse_args()
    sysroot, out, eh_out = args.sysroot.resolve(), args.out.resolve(), args.eh_out.resolve()
    lock_path = TOOL / "SOURCE.lock"
    lock = json.loads(lock_path.read_text())
    vendor = TOOL / "vendor"
    for name, pin in lock["files"].items():
        path = vendor / name
        data = path.read_bytes()
        blob = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
        if sha(path) != pin["sha256"] or blob != pin["git_blob"]:
            raise RuntimeError(f"source pin mismatch: {name}")
    libc = sysroot / "lib/libc.a"
    if not libc.is_file():
        raise RuntimeError(f"missing native relibc sysroot: {libc}")

    run([sys.executable, TOOL / "build-eh.py", "--sysroot", sysroot, "--out", eh_out])
    stage = out / "source"
    if stage.exists():
        shutil.rmtree(stage)
    for name in lock["files"]:
        destination = stage / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(vendor / name, destination)
    overlay = stage / "overlay"
    overlay.mkdir(parents=True)
    site = (TOOL / "eh-config-site.h").read_text()
    marker = "#define _LIBCPP_HAS_FILESYSTEM 0"
    if site.count(marker) != 1:
        raise RuntimeError("expected one disabled filesystem config in eh-config-site.h")
    (overlay / "__config_site").write_text(site.replace(marker, "#define _LIBCPP_HAS_FILESYSTEM 1"))
    shutil.copyfile(vendor / "libcxx/vendor/llvm/default_assertion_handler.in", overlay / "__assertion_handler")

    out.mkdir(parents=True, exist_ok=True)
    flags = ["--target=aarch64-unknown-none", f"--sysroot={sysroot}", "-D__kumo__", "-nostdinc++",
             "-isystem", overlay, "-isystem", stage / "libcxx/include", "-isystem", sysroot / "include",
             "-I", stage / "libcxx/src", "-I", stage / "libcxx/src/include", "-std=c++23", "-O2", "-ffreestanding", "-fno-pic", "-fno-pie",
             "-fno-stack-protector", "-fexceptions", "-frtti", "-funwind-tables", "-ffunction-sections",
             "-fdata-sections"]
    path_source = stage / "libcxx/src/filesystem/path.cpp"
    path_object = out / "path.o"
    path_dep = out / "path.d"
    run(["clang++", *flags, "-D_LIBCPP_BUILDING_LIBRARY", "-MD", "-MF", path_dep,
         "-c", path_source, "-o", path_object])
    smoke = TOOL / "fs-path-smoke.cpp"
    smoke_object = out / "smoke.o"
    smoke_dep = out / "smoke.d"
    run(["clang++", *flags, "-MD", "-MF", smoke_dep, "-c", smoke, "-o", smoke_object])
    string_source = stage / "libcxx/src/string.cpp"
    string_object = out / "string.o"
    string_dep = out / "string.d"
    run(["clang++", *flags, "-D_LIBCPP_BUILDING_LIBRARY", "-MD", "-MF", string_dep,
         "-c", string_source, "-o", string_object])
    archive = out / "libkumo-cxx-fs-path.a"
    archive.unlink(missing_ok=True)
    run(["llvm-ar", "rcs", archive, path_object, string_object])
    crt = out / "crt0.o"
    run(["clang", "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S", "-o", crt])
    eh_archive = eh_out / "libkumo-cxx-eh.a"
    unwind_archive = eh_out / "unwind/libunwind.a"
    if not eh_archive.is_file() or not unwind_archive.is_file():
        raise RuntimeError("SH9-b1 exception and unwind archives were not built")
    elf = out / "cxx-fs-path-smoke"
    link_map = out / "link.map"
    run(["ld.lld", "-static", "--gc-sections", "--eh-frame-hdr", "-z", "noexecstack",
         "-T", TOOL / "eh-sections.ld", "-T", ROOT / "toolchain/relibc/smoke/link.ld",
         f"-Map={link_map}", "-o", elf, crt, smoke_object, "--start-group", archive, eh_archive,
         unwind_archive, libc, "--end-group"])
    undefined = run(["llvm-nm", "--undefined-only", elf], capture=True)
    (out / "undefined.txt").write_text(undefined)
    if undefined.strip():
        raise RuntimeError(f"unresolved symbols in filesystem path smoke:\n{undefined}")

    inputs = {Path(__file__).resolve(), smoke.resolve(), lock_path.resolve(), (TOOL / "eh-config-site.h").resolve(),
              (TOOL / "eh-sections.ld").resolve(), (ROOT / "toolchain/relibc/smoke/link.ld").resolve(),
              (ROOT / "toolchain/relibc/smoke/crt0.S").resolve(), libc.resolve(), eh_archive.resolve(),
              unwind_archive.resolve(), archive.resolve()}
    for dep in (path_dep, smoke_dep, string_dep):
        inputs.update(source_dependencies(dep, stage))
    manifest = {
        "llvm_revision": lock["revision"],
        "llvm_ref": lock["ref"],
        "scope": "libc++ filesystem::path lexical operations only; no filesystem syscalls",
        "clang": run(["clang++", "--version"], capture=True).strip(),
        "inputs_sha256": {str(path): sha(path) for path in sorted(inputs) if path.is_file()},
        "archive_sha256": sha(archive),
        "elf_sha256": sha(elf),
        "elf_bytes": elf.stat().st_size,
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"SH9-c1 libc++ filesystem path compile/link green: {elf}", flush=True)


if __name__ == "__main__":
    main()
