#!/usr/bin/env python3
#J940_r06_001
"""Build the pinned libc++ read-only filesystem status slice for native KUMO."""
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


def source_dependencies(depfile, stage):
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
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-fs-status")
    parser.add_argument("--path-out", type=Path, default=ROOT / "target/cxx-fs-path")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    args = parser.parse_args()
    sysroot, out = args.sysroot.resolve(), args.out.resolve()
    path_out, eh_out = args.path_out.resolve(), args.eh_out.resolve()
    subprocess.run([sys.executable, TOOL / "build-fs-path.py", "--sysroot", sysroot,
                    "--out", path_out, "--eh-out", eh_out], check=True)
    lock_path = TOOL / "SOURCE.lock"
    lock = json.loads(lock_path.read_text())
    source = path_out / "source"
    status_source = source / "libcxx/src/filesystem/operations.cpp"
    if not status_source.is_file():
        raise RuntimeError(f"missing pinned operations.cpp: {status_source}")
    libc = sysroot / "lib/libc.a"
    if not libc.is_file():
        raise RuntimeError(f"missing native relibc sysroot: {libc}")
    out.mkdir(parents=True, exist_ok=True)
    overlay = out / "overlay"
    overlay.mkdir(parents=True, exist_ok=True)
    sendfile = overlay / "sys/sendfile.h"
    sendfile.parent.mkdir(parents=True, exist_ok=True)
    # operations.cpp must compile its unrelated copy_file section; KUMO has no sendfile.
    # This declaration is only for that dead section, which --gc-sections must drop. — KESTREL
    sendfile.write_text("""#ifndef KUMO_SH9_C2_SEND_FILE_DECLARATION
#define KUMO_SH9_C2_SEND_FILE_DECLARATION
#include <sys/types.h>
#include <stddef.h>
#ifdef __cplusplus
extern \"C\" {
#endif
ssize_t sendfile(int out_fd, int in_fd, off_t *offset, size_t count);
#ifdef __cplusplus
}
#endif
#endif
""")
    flags = ["--target=aarch64-unknown-none", f"--sysroot={sysroot}", "-D__kumo__", "-nostdinc++",
             "-isystem", overlay, "-isystem", source / "overlay", "-isystem", source / "libcxx/include",
             "-isystem", sysroot / "include", "-I", source / "libcxx/src", "-I", source / "libcxx/src/include",
             "-std=c++23", "-O2", "-ffreestanding", "-fno-pic", "-fno-pie", "-fno-stack-protector",
             "-fexceptions", "-frtti", "-funwind-tables", "-ffunction-sections", "-fdata-sections"]
    operations_obj = out / "operations.o"
    operations_dep = out / "operations.d"
    run(["clang++", *flags, "-D_LIBCPP_BUILDING_LIBRARY", "-MD", "-MF", operations_dep,
         "-c", status_source, "-o", operations_obj])
    smoke = TOOL / "fs-status-smoke.cpp"
    smoke_obj = out / "smoke.o"
    smoke_dep = out / "smoke.d"
    run(["clang++", *flags, "-MD", "-MF", smoke_dep, "-c", smoke, "-o", smoke_obj])
    support_objects = []
    support_deps = []
    for name, relative in (
        ("error_category", "libcxx/src/error_category.cpp"),
        ("system_error", "libcxx/src/system_error.cpp"),
        ("filesystem_error", "libcxx/src/filesystem/filesystem_error.cpp"),
        ("memory", "libcxx/src/memory.cpp"),
    ):
        obj = out / f"{name}.o"
        dep = out / f"{name}.d"
        run(["clang++", *flags, "-D_LIBCPP_BUILDING_LIBRARY", "-MD", "-MF", dep,
             "-c", source / relative, "-o", obj])
        support_objects.append(obj)
        support_deps.append(dep)
    archive = out / "libkumo-cxx-fs-status.a"
    archive.unlink(missing_ok=True)
    run(["llvm-ar", "rcs", archive, operations_obj, *support_objects])
    crt = out / "crt0.o"
    run(["clang", "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S", "-o", crt])
    eh_archive = eh_out / "libkumo-cxx-eh.a"
    unwind_archive = eh_out / "unwind/libunwind.a"
    path_archive = path_out / "libkumo-cxx-fs-path.a"
    if not eh_archive.is_file() or not unwind_archive.is_file() or not path_archive.is_file():
        raise RuntimeError("SH9-b1 EH/unwind and SH9-c1 path archives were not built")
    elf = out / "cxx-fs-status-smoke"
    link_map = out / "link.map"
    run(["ld.lld", "-static", "--gc-sections", "--eh-frame-hdr", "-z", "noexecstack",
         "-T", TOOL / "eh-sections.ld", "-T", ROOT / "toolchain/relibc/smoke/link.ld",
         f"-Map={link_map}", "-o", elf, crt, smoke_obj, "--start-group", archive, path_archive,
         eh_archive, unwind_archive, libc, "--end-group"])
    undefined = run(["llvm-nm", "--undefined-only", elf], capture=True)
    (out / "undefined.txt").write_text(undefined)
    if undefined.strip():
        raise RuntimeError(f"unresolved symbols in filesystem status smoke:\n{undefined}")
    if "sendfile" in run(["llvm-nm", elf], capture=True):
        raise RuntimeError("the unsupported sendfile declaration escaped dead-section collection")

    inputs = {Path(__file__).resolve(), (TOOL / "build-fs-path.py").resolve(), smoke.resolve(), lock_path.resolve(),
              (TOOL / "eh-config-site.h").resolve(), (TOOL / "eh-sections.ld").resolve(),
              (ROOT / "toolchain/relibc/smoke/link.ld").resolve(), (ROOT / "toolchain/relibc/smoke/crt0.S").resolve(),
              libc.resolve(), eh_archive.resolve(), unwind_archive.resolve(), path_archive.resolve(), archive.resolve(),
              sendfile.resolve()}
    for depfile in (operations_dep, smoke_dep, *support_deps):
        inputs.update(source_dependencies(depfile, source))
    manifest = {
        "llvm_revision": lock["revision"],
        "llvm_ref": lock["ref"],
        "scope": "libc++ filesystem status/exists/file-kind queries only; read-only; no new VFS or kernel calls",
        "operations_build_shim": "generated sendfile declaration for upstream copy_file code; final smoke ELF contains no sendfile reference",
        "clang": run(["clang++", "--version"], capture=True).strip(),
        "inputs_sha256": {str(path): sha(path) for path in sorted(inputs) if path.is_file()},
        "archive_sha256": sha(archive),
        "elf_sha256": sha(elf),
        "elf_bytes": elf.stat().st_size,
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"SH9-c2 libc++ filesystem status compile/link green: {elf}", flush=True)


if __name__ == "__main__":
    main()
