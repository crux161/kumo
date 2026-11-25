#!/usr/bin/env python3
#J942_r08_001
"""Build the pinned libc++ locale layer and read-only file streams for native KUMO."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "toolchain/cxx"
# The localized library: locale + iostreams + file streams, and what they call at link time.
UNITS = ("locale", "ios", "ios.instantiations", "fstream", "ostream", "call_once", "new_helpers",
         "string", "memory", "system_error", "error_category")
# The four relibc additions this slice made (kumo.patch, J942_r08_001). The proof must reach
# every one of them through libc++, or it is not proving them. — TEAL
RELIBC_LOCALE_CALLS = ("strtod_l", "strtof_l", "strtold_l", "strftime_l")
MUTATORS = {"mkdir", "mkdirat", "rmdir", "unlink", "unlinkat", "rename", "renameat", "ftruncate",
            "truncate", "symlink", "symlinkat", "link", "linkat", "utimensat", "futimens", "utimes"}


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
    result = set()
    for raw in text.split(":", 1)[1].split():
        path = Path(raw).resolve()
        if path.is_file():
            if str(path).startswith(("/usr/include/", "/Library/Developer/")) or "include/c++/v1" in str(path):
                raise RuntimeError(f"host C++ header contamination: {path}")
            result.add(path)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-fstream")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    args = parser.parse_args()
    sysroot, out, eh_out = args.sysroot.resolve(), args.out.resolve(), args.eh_out.resolve()
    libc = sysroot / "lib/libc.a"
    if not libc.is_file():
        raise RuntimeError(f"missing native relibc sysroot: {libc}")
    exported = run(["llvm-nm", "--defined-only", libc], capture=True)
    for name in RELIBC_LOCALE_CALLS:
        if f" T {name}\n" not in exported:
            raise RuntimeError(f"sysroot relibc lacks {name}: rebuild it with scripts/relibc-headers.sh")

    run([sys.executable, TOOL / "build-eh.py", "--sysroot", sysroot, "--out", eh_out])
    lock_path = TOOL / "SOURCE.lock"
    lock = json.loads(lock_path.read_text())
    vendor = TOOL / "vendor"
    for name, pin in lock["files"].items():
        data = (vendor / name).read_bytes()
        blob = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
        if hashlib.sha256(data).hexdigest() != pin["sha256"] or blob != pin["git_blob"]:
            raise RuntimeError(f"source pin mismatch: {name}")
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
    for marker in ("#define _LIBCPP_HAS_FILESYSTEM 0", "#define _LIBCPP_HAS_LOCALIZATION 0"):
        if site.count(marker) != 1:
            raise RuntimeError(f"expected one '{marker}' in eh-config-site.h")
    # relibc is neither glibc, musl nor a BSD, so libc++ has no ctype table of its own for it;
    # the default rune table is libc++'s supported answer for such a libc. — TEAL
    site = site.replace("#define _LIBCPP_HAS_FILESYSTEM 0", "#define _LIBCPP_HAS_FILESYSTEM 1")
    site = site.replace("#define _LIBCPP_HAS_LOCALIZATION 0",
                        "#define _LIBCPP_HAS_LOCALIZATION 1\n#define _LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE")
    (overlay / "__config_site").write_text(site)
    shutil.copyfile(vendor / "libcxx/vendor/llvm/default_assertion_handler.in", overlay / "__assertion_handler")

    out.mkdir(parents=True, exist_ok=True)
    flags = ["--target=aarch64-unknown-none", f"--sysroot={sysroot}", "-D__kumo__", "-nostdinc++",
             "-isystem", overlay, "-isystem", stage / "libcxx/include", "-isystem", sysroot / "include",
             "-I", stage / "libcxx/src", "-I", stage / "libcxx/src/include", "-std=c++23", "-O2",
             "-ffreestanding", "-fno-pic", "-fno-pie", "-fno-stack-protector", "-fexceptions", "-frtti",
             "-funwind-tables", "-ffunction-sections", "-fdata-sections"]
    objects, deps = [], []
    for unit in UNITS:
        obj, dep = out / f"{unit}.o", out / f"{unit}.d"
        run(["clang++", *flags, "-D_LIBCPP_BUILDING_LIBRARY", "-MD", "-MF", dep,
             "-c", stage / f"libcxx/src/{unit}.cpp", "-o", obj])
        objects.append(obj)
        deps.append(dep)
    smoke = TOOL / "fstream-smoke.cpp"
    smoke_obj, smoke_dep = out / "smoke.o", out / "smoke.d"
    run(["clang++", *flags, "-MD", "-MF", smoke_dep, "-c", smoke, "-o", smoke_obj])
    archive = out / "libkumo-cxx-locale.a"
    archive.unlink(missing_ok=True)
    run(["llvm-ar", "rcs", archive, *objects])
    crt = out / "crt0.o"
    run(["clang", "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S", "-o", crt])
    eh_archive, unwind_archive = eh_out / "libkumo-cxx-eh.a", eh_out / "unwind/libunwind.a"
    if not eh_archive.is_file() or not unwind_archive.is_file():
        raise RuntimeError("SH9-b1 exception and unwind archives were not built")
    elf = out / "cxx-fstream-smoke"
    link_map = out / "link.map"
    run(["ld.lld", "-static", "--gc-sections", "--eh-frame-hdr", "-z", "noexecstack",
         "-T", TOOL / "eh-sections.ld", "-T", ROOT / "toolchain/relibc/smoke/link.ld",
         f"-Map={link_map}", "-o", elf, crt, smoke_obj, "--start-group", archive, eh_archive,
         unwind_archive, libc, "--end-group"])
    undefined = run(["llvm-nm", "--undefined-only", elf], capture=True)
    (out / "undefined.txt").write_text(undefined)
    if undefined.strip():
        raise RuntimeError(f"unresolved symbols in file stream smoke:\n{undefined}")
    linked = {line.split()[-1] for line in run(["llvm-nm", elf], capture=True).splitlines()
              if line.split()[-2:-1] in (["T"], ["t"])}
    missing = [name for name in RELIBC_LOCALE_CALLS if name not in linked]
    if missing:
        raise RuntimeError(f"the smoke does not reach relibc's {missing}")
    if MUTATORS & linked:
        raise RuntimeError(f"file stream smoke links mutating calls: {sorted(MUTATORS & linked)}")

    inputs = {Path(__file__).resolve(), (TOOL / "build-eh.py").resolve(), smoke.resolve(), lock_path.resolve(),
              (TOOL / "eh-config-site.h").resolve(), (TOOL / "eh-sections.ld").resolve(),
              (ROOT / "toolchain/relibc/smoke/link.ld").resolve(), (ROOT / "toolchain/relibc/smoke/crt0.S").resolve(),
              (ROOT / "toolchain/relibc/kumo.patch").resolve(), libc.resolve(), eh_archive.resolve(),
              unwind_archive.resolve(), archive.resolve(), (overlay / "__config_site").resolve()}
    for depfile in (*deps, smoke_dep):
        inputs.update(source_dependencies(depfile))
    manifest = {
        "llvm_revision": lock["revision"],
        "llvm_ref": lock["ref"],
        "scope": "libc++ locale layer (classic locale, default rune table) and read-only ifstream over "
                 "relibc FILE; relibc strtod_l/strtof_l/strtold_l/strftime_l reached; no VFS or kernel calls",
        "config": "_LIBCPP_HAS_FILESYSTEM 1, _LIBCPP_HAS_LOCALIZATION 1, _LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE",
        "clang": run(["clang++", "--version"], capture=True).strip(),
        "inputs_sha256": {str(path): sha(path) for path in sorted(inputs) if path.is_file()},
        "archive_sha256": sha(archive),
        "elf_sha256": sha(elf),
        "elf_bytes": elf.stat().st_size,
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"SH9-c4 libc++ file streams compile/link green: {elf}", flush=True)


if __name__ == "__main__":
    main()
