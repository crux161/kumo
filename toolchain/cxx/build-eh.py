#!/usr/bin/env python3
#J936_r02_001
"""Offline native C++ exception ABI/RTTI build over relibc and SH9-a libunwind."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "toolchain/cxx"
ABI_SOURCES = (
    "cxa_aux_runtime.cpp", "cxa_default_handlers.cpp", "cxa_demangle.cpp",
    "cxa_exception_storage.cpp", "cxa_guard.cpp", "cxa_handlers.cpp",
    "cxa_vector.cpp", "cxa_virtual.cpp", "stdlib_exception.cpp", "stdlib_stdexcept.cpp",
    "stdlib_typeinfo.cpp", "abort_message.cpp", "fallback_malloc.cpp", "private_typeinfo.cpp",
    "stdlib_new_delete.cpp", "cxa_exception.cpp", "cxa_personality.cpp",
)
CXX_SOURCES = ("exception.cpp", "stdexcept.cpp", "verbose_abort.cpp")


def run(command):
    subprocess.run(list(map(str, command)), check=True)


def capture(command):
    return subprocess.check_output(list(map(str, command)), text=True)


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-eh")
    args = parser.parse_args()
    sysroot, out = args.sysroot.resolve(), args.out.resolve()
    lock = json.loads((TOOL / "SOURCE.lock").read_text())
    for name, pin in lock["files"].items():
        data = (TOOL / "vendor" / name).read_bytes()
        blob = b"blob " + str(len(data)).encode() + b"\0" + data
        if sha(TOOL / "vendor" / name) != pin["sha256"] or hashlib.sha1(blob).hexdigest() != pin["git_blob"]:
            raise RuntimeError(f"source pin mismatch: {name}")
    if not (sysroot / "lib/libc.a").is_file():
        raise RuntimeError("run scripts/relibc-headers.sh first")
    out.mkdir(parents=True, exist_ok=True)
    stage = out / "source"
    if stage.exists():
        shutil.rmtree(stage)
    for name in lock["files"]:
        destination = stage / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(TOOL / "vendor" / name, destination)
    shutil.copyfile(TOOL / "eh-config-site.h", stage / "libcxx/include/__config_site")
    shutil.copyfile(stage / "libcxx/vendor/llvm/default_assertion_handler.in", stage / "libcxx/include/__assertion_handler")
    # The same admission as G7l: Linux syscall constants in relibc headers are
    # not native syscall authority. Keep the real pthread guard fallback. — KESTREL
    guard = stage / "libcxxabi/src/cxa_guard_impl.h"
    text = guard.read_text()
    anchor = "#  if __has_include(<sys/syscall.h>)"
    if text.count(anchor) != 1:
        raise RuntimeError("cxa_guard syscall-header anchor changed")
    guard.write_text(text.replace(anchor, anchor + " && !defined(__kumo__)"))
    run(["python3", ROOT / "toolchain/unwind/build.py", "--sysroot", sysroot, "--out", out / "unwind"])
    flags = ["--target=aarch64-unknown-none", f"--sysroot={sysroot}", "-D__kumo__",
             "-nostdinc++", "-isystem", stage / "libcxx/include", "-isystem", sysroot / "include",
             "-I", stage / "libcxxabi/include", "-I", stage / "libcxx/src",
             "-I", ROOT / "toolchain/unwind/vendor/include", "-std=c++23", "-O2",
             "-ffreestanding", "-fno-pic", "-fno-pie", "-fno-stack-protector",
             "-fexceptions", "-frtti", "-funwind-tables", "-ffunction-sections", "-fdata-sections"]
    sources = ([stage / "libcxxabi/src" / name for name in ABI_SOURCES]
               + [stage / "libcxx/src" / name for name in CXX_SOURCES]
               + [TOOL / "eh-runtime.cpp"])
    objects, deps = [], []
    for source in sources:
        obj = out / (source.stem + ".o")
        dep = out / (source.stem + ".d")
        run(["clang++", *flags, "-D_LIBCPP_BUILDING_LIBRARY", "-DLIBCXX_BUILDING_LIBCXXABI",
             "-D_LIBCXXABI_BUILDING_LIBRARY", "-MD", "-MF", dep, "-c", source, "-o", obj])
        objects.append(obj)
        deps.append(dep)
    archive = out / "libkumo-cxx-eh.a"
    archive.unlink(missing_ok=True)
    run(["llvm-ar", "rcs", archive, *objects])
    run(["clang++", *flags, "-MD", "-MF", out / "smoke.d", "-c", TOOL / "eh-smoke.cpp", "-o", out / "smoke.o"])
    deps.append(out / "smoke.d")
    run(["clang", "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S", "-o", out / "crt0.o"])
    elf = out / "cxx-eh-smoke"
    run(["ld.lld", "-static", "--gc-sections", "--eh-frame-hdr", "-z", "noexecstack",
         "-T", TOOL / "eh-sections.ld", "-T", ROOT / "toolchain/relibc/smoke/link.ld",
         f"-Map={out}/link.map", "-o", elf, out / "crt0.o", out / "smoke.o",
         # Retain the fini hook even without a runtime reference. — KESTREL
         out / "eh-runtime.o", "--start-group", archive, out / "unwind/libunwind.a",
         sysroot / "lib/libc.a", "--end-group"])
    undefined = capture(["llvm-nm", "--undefined-only", elf])
    (out / "undefined.txt").write_text(undefined)
    if undefined.strip():
        raise RuntimeError(f"unresolved final symbols: {undefined}")
    inputs = {Path(__file__).resolve(), TOOL / "SOURCE.lock", TOOL / "eh-config-site.h",
              TOOL / "eh-sections.ld", ROOT / "toolchain/relibc/smoke/link.ld",
              ROOT / "toolchain/relibc/smoke/crt0.S", sysroot / "lib/libc.a", out / "unwind/libunwind.a"}
    for dep in deps:
        inputs.update(Path(p) for p in dep.read_text().replace("\\\n", " ").split(":", 1)[1].split())
    for path in inputs:
        if str(path).startswith(("/usr/include/", "/Library/Developer/")) or "include/c++/v1" in str(path):
            raise RuntimeError(f"host header contamination: {path}")
    (out / "manifest.json").write_text(json.dumps({
        "llvm": lock["ref"], "clang": capture(["clang++", "--version"]).strip(),
        "inputs_sha256": {str(p): sha(p) for p in sorted(inputs)},
        "archive_sha256": sha(archive), "elf_sha256": sha(elf), "elf_bytes": elf.stat().st_size,
    }, indent=2) + "\n")
    print(f"SH9-b1 native C++ EH compile/link green: {elf}", flush=True)


if __name__ == "__main__":
    main()
