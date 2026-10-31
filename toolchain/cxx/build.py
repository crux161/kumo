#!/usr/bin/env python3
#j736
"""Compile the measured C++ runtime in the offline G7k container."""
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path("/repo")
TOOL = ROOT / "toolchain/cxx"
MESA = Path("/work/kumo-mesa")
WORK = Path("/work/kumo-cxx")
OUT = Path("/output/cxx")


def run(command):
    p = subprocess.run(list(map(str, command)), capture_output=True, text=True)
    if p.returncode:
        raise RuntimeError(" ".join(map(str, command)) + "\n" + p.stdout + p.stderr)
    return p.stdout


def main():
    lock = json.loads((TOOL / "SOURCE.lock").read_text())
    for name, entry in lock["files"].items():
        data = (TOOL / "vendor" / name).read_bytes()
        assert hashlib.sha256(data).hexdigest() == entry["sha256"], name
        assert hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest() == entry["git_blob"], name
    WORK.mkdir(exist_ok=True)
    OUT.mkdir(parents=True, exist_ok=True)
    shutil.copytree(TOOL / "vendor", WORK / "vendor", dirs_exist_ok=True)
    # relibc's syscall-number header describes compatibility constants. It does
    # not authorize libc++abi to issue Linux syscalls. Keep upstream's real
    # pthread mutex/condition-variable guard implementation on KUMO. — SAKER
    guard = WORK / "vendor/libcxxabi/src/cxa_guard_impl.h"
    text = guard.read_text()
    old = "#  if __has_include(<sys/syscall.h>)"
    assert text.count(old) == 1
    guard.write_text(text.replace(old, old + " && !defined(__kumo__)"))
    flags = ["--target=aarch64-unknown-none", f"--sysroot={MESA}/sysroot", "-D__kumo__",
             "-nostdinc++", "-isystem", str(MESA / "libcxx"), "-isystem", str(MESA / "sysroot/include"),
             "-std=c++17", "-O2", "-ffreestanding", "-fno-pic", "-fno-pie", "-fno-stack-protector",
             "-ffunction-sections", "-fdata-sections", "-fno-exceptions", "-fno-rtti",
             "-I", str(WORK / "vendor/libcxx/src"), "-I", str(WORK / "vendor/libcxxabi/src")]
    sources = [TOOL / "runtime.cpp", *[WORK / "vendor" / name for name in (
        "libcxx/src/algorithm.cpp", "libcxx/src/hash.cpp", "libcxx/src/mutex.cpp", "libcxx/src/mutex_destructor.cpp",
        "libcxxabi/src/cxa_guard.cpp", "libcxxabi/src/cxa_virtual.cpp")]]
    objects = []
    depfiles = []
    for source in sources:
        obj = WORK / (source.stem + ".o")
        dep = WORK / (source.stem + ".d")
        run(["clang++-22", *flags, "-std=c++23", "-MD", "-MF", dep, "-c", source, "-o", obj])
        objects.append(obj)
        depfiles.append(dep)
    runtime = OUT / "libkumo-cxx.a"
    runtime.unlink(missing_ok=True)
    run(["llvm-ar-22", "rcs", runtime, *objects])
    measured = json.loads(Path("/output/inventory.json").read_text())
    defined = {line.split()[0] for line in run(["llvm-nm-22", "-P", "--defined-only", runtime]).splitlines()
               if len(line.split()) >= 4}
    required_cxx = {symbol for result in measured.values() for symbol, entry in result["undefined"].items()
                    if entry["required"] and entry["category"] == "C++ runtime"}
    assert required_cxx <= defined, f"missing measured C++ exports: {required_cxx - defined}"
    for driver in ("gl", "vk"):
        archives = [MESA / name for name in measured[driver]["archive_bytes"] if name != "sysroot/lib/libc.a"]
        p = subprocess.run(list(map(str, ["ld.lld-22", "-static", "--gc-sections", "-e", "main",
            "--error-limit=0", "--no-demangle", "-o", WORK / f"mesa-{driver}.elf", MESA / f"link-{driver}.o",
            "--start-group", runtime, *archives, MESA / "sysroot/lib/libc.a", "--end-group"])), capture_output=True, text=True)
        (OUT / f"mesa-{driver}.errors").write_text(p.stderr)
        errors = re.findall(r"^ld.lld-22: error: (.+)$", p.stderr, re.M)
        assert all(error.startswith("undefined symbol: ") for error in errors), errors
        remaining = {error.removeprefix("undefined symbol: ") for error in errors}
        assert p.returncode == (1 if remaining else 0), p.stderr
        assert not remaining & required_cxx, remaining & required_cxx
        assert not remaining - {s for s, e in measured[driver]["undefined"].items() if e["required"]}, remaining
        print(f"G7l {driver.upper()} C++ closure green: {len(remaining)} non-C++ gaps remain", flush=True)
    run(["clang++-22", *flags, "-MD", "-MF", WORK / "smoke.d", "-c", TOOL / "smoke.cpp", "-o", WORK / "smoke.o"])
    depfiles.append(WORK / "smoke.d")
    run(["clang-22", "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S",
         "-o", WORK / "crt0.o"])
    elf = OUT / "cxx-smoke"
    run(["ld.lld-22", "-static", "--gc-sections", "-z", "noexecstack", "-T",
         ROOT / "toolchain/relibc/smoke/link.ld", f"-Map={OUT}/link.map", "-o", elf,
         WORK / "crt0.o", WORK / "smoke.o", "--start-group", runtime,
         MESA / "sysroot/lib/libc.a", "--end-group"])
    undefined = run(["llvm-nm-22", "--undefined-only", elf])
    (OUT / "undefined.txt").write_text(undefined)
    assert not undefined.strip(), undefined
    (OUT / "runtime-undefined.txt").write_text(run(["llvm-nm-22", "--undefined-only", runtime]))
    inputs = set(TOOL.rglob("*")) | {MESA / "sysroot/lib/libc.a", MESA / "libcxx/__config_site", guard}
    for dep in depfiles:
        for path in dep.read_text().replace("\\\n", " ").split(":", 1)[1].split():
            assert not path.startswith("/usr/include/"), "Linux host header contamination"
            inputs.add(Path(path))
    (OUT / "manifest.json").write_text(json.dumps({
        "llvm": lock["ref"], "clang": run(["clang++-22", "--version"]),
        "inputs_sha256": {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(inputs) if p.is_file()},
        "elf_sha256": hashlib.sha256(elf.read_bytes()).hexdigest(),
    }, indent=2) + "\n")
    print(f"G7l C++ compile/link green: {elf}", flush=True)


if __name__ == "__main__":
    main()
