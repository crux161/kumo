#!/usr/bin/env python3
#J941_r07_001
"""Build the pinned libc++ read-only directory iteration slice for native KUMO."""
import argparse
import hashlib
import json
from pathlib import Path
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


def source_dependencies(depfile):
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
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-fs-dir")
    parser.add_argument("--status-out", type=Path, default=ROOT / "target/cxx-fs-status")
    parser.add_argument("--path-out", type=Path, default=ROOT / "target/cxx-fs-path")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    args = parser.parse_args()
    sysroot, out = args.sysroot.resolve(), args.out.resolve()
    status_out, path_out, eh_out = args.status_out.resolve(), args.path_out.resolve(), args.eh_out.resolve()
    # The status build verifies every SOURCE.lock pin, stages the closure (this slice's files
    # included) and builds the path, EH and status archives this one links over. — TEAL
    subprocess.run([sys.executable, TOOL / "build-fs-status.py", "--sysroot", sysroot, "--out", status_out,
                    "--path-out", path_out, "--eh-out", eh_out], check=True)
    lock_path = TOOL / "SOURCE.lock"
    lock = json.loads(lock_path.read_text())
    source = path_out / "source"
    libc = sysroot / "lib/libc.a"
    if not libc.is_file():
        raise RuntimeError(f"missing native relibc sysroot: {libc}")
    out.mkdir(parents=True, exist_ok=True)
    flags = ["--target=aarch64-unknown-none", f"--sysroot={sysroot}", "-D__kumo__", "-nostdinc++",
             "-isystem", source / "overlay", "-isystem", source / "libcxx/include",
             "-isystem", sysroot / "include", "-I", source / "libcxx/src", "-I", source / "libcxx/src/include",
             "-std=c++23", "-O2", "-ffreestanding", "-fno-pic", "-fno-pie", "-fno-stack-protector",
             "-fexceptions", "-frtti", "-funwind-tables", "-ffunction-sections", "-fdata-sections"]
    objects, deps = [], []
    for name in ("directory_iterator", "directory_entry"):
        unit = source / f"libcxx/src/filesystem/{name}.cpp"
        if not unit.is_file():
            raise RuntimeError(f"missing pinned {unit.name}: {unit}")
        obj, dep = out / f"{name}.o", out / f"{name}.d"
        run(["clang++", *flags, "-D_LIBCPP_BUILDING_LIBRARY", "-MD", "-MF", dep, "-c", unit, "-o", obj])
        objects.append(obj)
        deps.append(dep)
    smoke = TOOL / "fs-dir-smoke.cpp"
    smoke_obj, smoke_dep = out / "smoke.o", out / "smoke.d"
    run(["clang++", *flags, "-MD", "-MF", smoke_dep, "-c", smoke, "-o", smoke_obj])
    archive = out / "libkumo-cxx-fs-dir.a"
    archive.unlink(missing_ok=True)
    run(["llvm-ar", "rcs", archive, *objects])
    crt = out / "crt0.o"
    run(["clang", "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S", "-o", crt])
    status_archive = status_out / "libkumo-cxx-fs-status.a"
    path_archive = path_out / "libkumo-cxx-fs-path.a"
    eh_archive = eh_out / "libkumo-cxx-eh.a"
    unwind_archive = eh_out / "unwind/libunwind.a"
    for needed in (status_archive, path_archive, eh_archive, unwind_archive):
        if not needed.is_file():
            raise RuntimeError(f"SH9-b1/c1/c2 archive was not built: {needed}")
    elf = out / "cxx-fs-dir-smoke"
    link_map = out / "link.map"
    run(["ld.lld", "-static", "--gc-sections", "--eh-frame-hdr", "-z", "noexecstack",
         "-T", TOOL / "eh-sections.ld", "-T", ROOT / "toolchain/relibc/smoke/link.ld",
         f"-Map={link_map}", "-o", elf, crt, smoke_obj, "--start-group", archive, status_archive,
         path_archive, eh_archive, unwind_archive, libc, "--end-group"])
    undefined = run(["llvm-nm", "--undefined-only", elf], capture=True)
    (out / "undefined.txt").write_text(undefined)
    if undefined.strip():
        raise RuntimeError(f"unresolved symbols in directory iteration smoke:\n{undefined}")
    symbols = run(["llvm-nm", elf], capture=True)
    if "sendfile" in symbols:
        raise RuntimeError("the unsupported sendfile declaration escaped dead-section collection")
    # Read-only by construction: the proof binary must not even link a mutating call. — TEAL
    mutators = {"mkdir", "mkdirat", "rmdir", "unlink", "unlinkat", "rename", "renameat", "ftruncate",
                "truncate", "symlink", "symlinkat", "link", "linkat", "utimensat", "futimens", "utimes"}
    linked = {line.split()[-1] for line in symbols.splitlines() if line.split()[-2:-1] in (["T"], ["t"])}
    if mutators & linked:
        raise RuntimeError(f"directory iteration smoke links mutating calls: {sorted(mutators & linked)}")

    inputs = {Path(__file__).resolve(), (TOOL / "build-fs-status.py").resolve(), (TOOL / "build-fs-path.py").resolve(),
              smoke.resolve(), lock_path.resolve(), (TOOL / "eh-config-site.h").resolve(),
              (TOOL / "eh-sections.ld").resolve(), (ROOT / "toolchain/relibc/smoke/link.ld").resolve(),
              (ROOT / "toolchain/relibc/smoke/crt0.S").resolve(), libc.resolve(), eh_archive.resolve(),
              unwind_archive.resolve(), path_archive.resolve(), status_archive.resolve(), archive.resolve()}
    for depfile in (*deps, smoke_dep):
        inputs.update(source_dependencies(depfile))
    manifest = {
        "llvm_revision": lock["revision"],
        "llvm_ref": lock["ref"],
        "scope": "libc++ directory_iterator, recursive_directory_iterator and directory_entry refresh over "
                 "relibc opendir/readdir/stat; read-only; no new VFS or kernel calls",
        "clang": run(["clang++", "--version"], capture=True).strip(),
        "inputs_sha256": {str(path): sha(path) for path in sorted(inputs) if path.is_file()},
        "archive_sha256": sha(archive),
        "elf_sha256": sha(elf),
        "elf_bytes": elf.stat().st_size,
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"SH9-c3 libc++ directory iteration compile/link green: {elf}", flush=True)


if __name__ == "__main__":
    main()
