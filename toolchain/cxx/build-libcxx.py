#!/usr/bin/env python3
#J945_r11_001
"""Build the complete narrow-character libc++ for native KUMO and install it as a sysroot layer."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "toolchain/cxx"
# Every LIBCXX_SOURCES unit for threads + localization + filesystem, as upstream's
# libcxx/src/CMakeLists.txt lists them, minus what the ABI archive (build-eh.py) already holds
# (exception, stdexcept, verbose_abort, and libc++abi's operator new/delete in place of new.cpp)
# and random.cpp: relibc's KUMO getrandom is a clock-seeded xorshift, not an entropy source, so
# std::random_device stays off (_LIBCPP_HAS_RANDOM_DEVICE 0) until one exists. — TEAL
UNITS = (
    "algorithm", "any", "bind", "call_once", "charconv", "chrono", "error_category", "expected",
    "filesystem/filesystem_clock", "filesystem/filesystem_error", "filesystem/path", "functional", "hash",
    "memory", "memory_resource", "new_handler", "new_helpers", "optional", "print", "random_shuffle",
    "ryu/d2fixed", "ryu/d2s", "ryu/f2s", "string", "system_error", "typeinfo", "valarray", "variant",
    "vector", "atomic", "barrier", "condition_variable_destructor", "condition_variable", "future",
    "mutex_destructor", "mutex", "shared_mutex", "thread", "fstream", "ios", "ios.instantiations",
    "iostream", "locale", "ostream", "regex", "strstream", "filesystem/directory_entry",
    "filesystem/directory_iterator", "filesystem/operations", "filesystem/int128_builtins",
)
RELIBC_CALLS = ("strtod_l", "strtof_l", "strtold_l", "strftime_l")
MAIN_STACK = 1024 * 1024


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(command, *, capture=False):
    command = list(map(str, command))
    if capture:
        return subprocess.check_output(command, text=True)
    subprocess.run(command, check=True)
    return ""


def stack_link_script(out):
    """The relibc smoke script plus a PT_GNU_STACK header, as toolchain/mesa/build-compiler.py
    does for Mesa (G7n, J741): with an explicit PHDRS block lld emits no stack header of its own,
    so `-z stack-size` has nothing to fill."""
    script = (ROOT / "toolchain/relibc/smoke/link.ld").read_text()
    marker = "    tls    PT_TLS  FLAGS(4);"
    if script.count(marker) != 1:
        raise RuntimeError("relibc link.ld changed shape: no single PT_TLS line")
    path = out / "link.ld"
    path.write_text(script.replace(marker, marker + "\n    stack  PT_GNU_STACK FLAGS(6);"))
    return path


def set_stack_size(elf, size):
    """Write `size` into the image's one PT_GNU_STACK p_memsz, the field Sora's loader reads for
    thread zero's stack (J741) and the one `-z stack-size` would set."""
    import struct
    data = bytearray(elf.read_bytes())
    if data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1:
        raise RuntimeError("ELF64 little-endian expected")
    phoff, = struct.unpack_from("<Q", data, 0x20)
    phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
    hits = [phoff + i * phentsize for i in range(phnum)
            if struct.unpack_from("<I", data, phoff + i * phentsize)[0] == 0x6474E551]
    if len(hits) != 1:
        raise RuntimeError("expected exactly one PT_GNU_STACK")
    struct.pack_into("<Q", data, hits[0] + 40, size)
    elf.write_bytes(bytes(data))


def dependencies(depfile, base, allowed):
    """Every input a depfile names; anything outside `allowed` (the stage, the install layer, the
    sysroot, this repository) is host contamination, e.g. a toolchain's own include/c++/v1."""
    text = depfile.read_text().replace("\\\n", " ")
    result = set()
    for raw in text.split(":", 1)[1].split():
        path = Path(raw)
        path = (path if path.is_absolute() else base / path).resolve()
        if path.is_file():
            if not any(path.is_relative_to(root) for root in allowed):
                raise RuntimeError(f"host header contamination: {path}")
            result.add(path)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-libcxx")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    args = parser.parse_args()
    sysroot, out, eh_out = args.sysroot.resolve(), args.out.resolve(), args.eh_out.resolve()
    libc = sysroot / "lib/libc.a"
    if not libc.is_file():
        raise RuntimeError(f"missing native relibc sysroot: {libc}")
    exported = run(["llvm-nm", "--defined-only", libc], capture=True)
    for name in RELIBC_CALLS:
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

    # One configuration for the whole library: SH9-c4/c5's localized, narrow-character site.
    site = (TOOL / "eh-config-site.h").read_text()
    for marker in ("#define _LIBCPP_HAS_FILESYSTEM 0", "#define _LIBCPP_HAS_LOCALIZATION 0",
                   "#define _LIBCPP_HAS_RANDOM_DEVICE 0", "#define _LIBCPP_HAS_WIDE_CHARACTERS 0"):
        if site.count(marker) != 1:
            raise RuntimeError(f"expected one '{marker}' in eh-config-site.h")
    site = site.replace("#define _LIBCPP_HAS_FILESYSTEM 0", "#define _LIBCPP_HAS_FILESYSTEM 1")
    site = site.replace("#define _LIBCPP_HAS_LOCALIZATION 0",
                        "#define _LIBCPP_HAS_LOCALIZATION 1\n#define _LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE")
    install = out / "install"
    if install.exists():
        shutil.rmtree(install)
    headers = install / "include/c++/v1"
    shutil.copytree(stage / "libcxx/include", headers)
    (headers / "__config_site").write_text(site)
    shutil.copyfile(vendor / "libcxx/vendor/llvm/default_assertion_handler.in", headers / "__assertion_handler")

    out.mkdir(parents=True, exist_ok=True)
    common = ["--target=aarch64-unknown-none", f"--sysroot={sysroot}", "-D__kumo__", "-nostdinc++",
              "-isystem", headers, "-isystem", sysroot / "include", "-std=c++23", "-O2", "-ffreestanding",
              "-fno-pic", "-fno-pie", "-fno-stack-protector", "-fexceptions", "-frtti", "-funwind-tables",
              "-ffunction-sections", "-fdata-sections"]
    # As upstream builds libc++ against libc++abi, with LLVM-libc's shared float/charconv core
    # (FindLibcCommonUtils.cmake: the libc root on the include path, its own namespace).
    library = common + ["-I", stage / "libcxx/src", "-I", stage / "libcxx/src/include", "-I", stage / "libc",
                        "-DLIBC_NAMESPACE=__llvm_libc_common_utils", "-DLIBCXX_BUILDING_LIBCXXABI",
                        "-D_LIBCPP_BUILDING_LIBRARY"]

    def compile_unit(unit):
        stem = unit.replace("/", "_")
        obj, dep = out / f"{stem}.o", out / f"{stem}.d"
        run(["clang++", *library, "-MD", "-MF", dep, "-c", stage / f"libcxx/src/{unit}.cpp", "-o", obj])
        return obj, dep

    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        built = list(pool.map(compile_unit, UNITS))
    objects = [obj for obj, _ in built]
    deps = [dep for _, dep in built]
    libdir = install / "lib"
    libdir.mkdir(parents=True)
    archive = libdir / "libc++.a"
    run(["llvm-ar", "rcs", archive, *objects])
    eh_archive, unwind_archive = eh_out / "libkumo-cxx-eh.a", eh_out / "unwind/libunwind.a"
    if not eh_archive.is_file() or not unwind_archive.is_file():
        raise RuntimeError("SH9-b1 exception and unwind archives were not built")
    shutil.copyfile(eh_archive, libdir / "libc++abi.a")
    shutil.copyfile(unwind_archive, libdir / "libunwind.a")

    # The proof compiles against the installed layer alone: headers from include/c++/v1, archives
    # from lib/. Nothing from the staged source tree is on its include path.
    smoke = TOOL / "libcxx-smoke.cpp"
    smoke_obj, smoke_dep = out / "smoke.o", out / "smoke.d"
    run(["clang++", *common, "-MD", "-MF", smoke_dep, "-c", smoke, "-o", smoke_obj])
    crt = out / "crt0.o"
    run(["clang", "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S", "-o", crt])
    elf = out / "cxx-libcxx-smoke"
    link_map = out / "link.map"
    # Thread zero's stack is whatever PT_GNU_STACK asks for (Sora, j741), else 16 KiB. libc++'s
    # regex matcher recurses and an exception's two unwind phases need stack of their own; the
    # first boot overflowed 16 KiB inside a bad_alloc throw. Programs linking this layer ask for
    # a real stack, as Linux gives one. — TEAL
    script = stack_link_script(out)
    run(["ld.lld", "-static", "--gc-sections", "--eh-frame-hdr", "-z", "noexecstack",
         "-T", TOOL / "eh-sections.ld", "-T", script,
         f"-Map={link_map}", "-o", elf, crt, smoke_obj, "--start-group", archive, libdir / "libc++abi.a",
         libdir / "libunwind.a", libc, "--end-group"])
    set_stack_size(elf, MAIN_STACK)
    undefined = run(["llvm-nm", "--undefined-only", elf], capture=True)
    (out / "undefined.txt").write_text(undefined)
    if undefined.strip():
        raise RuntimeError(f"unresolved symbols in the complete libc++ smoke:\n{undefined}")

    inputs = {Path(__file__).resolve(), (TOOL / "build-eh.py").resolve(), smoke.resolve(), lock_path.resolve(),
              (TOOL / "eh-config-site.h").resolve(), (TOOL / "eh-sections.ld").resolve(),
              (ROOT / "toolchain/relibc/smoke/link.ld").resolve(), (ROOT / "toolchain/relibc/smoke/crt0.S").resolve(),
              libc.resolve(), (headers / "__config_site").resolve()}
    allowed = tuple(path.resolve() for path in (out, sysroot, ROOT))
    for depfile in deps:
        inputs.update(dependencies(depfile, stage / "libcxx/src", allowed))
    inputs.update(dependencies(smoke_dep, ROOT, allowed))
    manifest = {
        "llvm_revision": lock["revision"],
        "llvm_ref": lock["ref"],
        "scope": "complete narrow-character libc++ (every LIBCXX_SOURCES unit for threads, localization and "
                 "filesystem except random_device and the units the ABI archive carries), installed as "
                 "include/c++/v1 + lib/{libc++,libc++abi,libunwind}.a; headers are the measured closure",
        "units": list(UNITS),
        "config": "_LIBCPP_HAS_FILESYSTEM 1, _LIBCPP_HAS_LOCALIZATION 1, _LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE, "
                  "_LIBCPP_HAS_WIDE_CHARACTERS 0, _LIBCPP_HAS_RANDOM_DEVICE 0",
        "clang": run(["clang++", "--version"], capture=True).strip(),
        "inputs_sha256": {str(path): sha(path) for path in sorted(inputs) if path.is_file()},
        "installed_sha256": {str(path.relative_to(install)): sha(path) for path in sorted(libdir.iterdir())},
        "installed_headers": sum(1 for path in headers.rglob("*") if path.is_file()),
        "main_stack_bytes": MAIN_STACK,
        "elf_sha256": sha(elf),
        "elf_bytes": elf.stat().st_size,
    }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"SH9-c6 complete libc++ compile/link green: {elf} ({len(UNITS)} units)", flush=True)


if __name__ == "__main__":
    main()
