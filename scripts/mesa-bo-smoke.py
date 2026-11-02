#!/usr/bin/env python3
#j744
"""Build the native Mesa BO bridge and prove it with real VMOs in disposable UEFI QEMU."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / "toolchain/mesa"
OUT = ROOT / "build/proofs/g7o-c"
GREEN = b"G7o-c native BOs green: all flags, rounding, lazy NC mmap, remap, destroy, stale and foreign refusal; real VMOs"


def run(command, **kwargs):
    return subprocess.run(list(map(str, command)), cwd=ROOT, check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true")
    args = parser.parse_args()
    OUT.mkdir(parents=True, exist_ok=True)
    # This verifies the source/header pins, prepares native headers, builds the same C
    # bridge and checks its measured dependency closure; no download. — KESTREL
    run(["python3", ROOT / "scripts/mesa-native-probe.py"])
    native = ROOT / "build/mesa-native/probe"
    mesa = native / "source/mesa-26.2.5"
    sysroot = ROOT / "target/kumo-sysroot"
    cc = shutil.which("clang")
    nm = shutil.which("llvm-nm")
    assert cc and nm
    flags = ["--target=aarch64-unknown-none", "-D__kumo__", "-std=c11", "-O2", "-ffreestanding",
             "-fno-pic", "-fno-pie", "-fno-stack-protector", "-ffunction-sections", "-fdata-sections",
             "-Wall", "-Wextra", "-Werror", f"--sysroot={sysroot}", "-isystem", sysroot / "include",
             "-I", TOOL, "-I", native / "compat", "-DHAVE_PTHREAD", "-DHAVE_ENDIAN_H", "-DHAVE_STRUCT_TIMESPEC"]
    for include in ["include", "src", "src/panfrost", "src/panfrost/lib", "src/panfrost/model"]:
        flags.extend(["-isystem", mesa / include])
    flags.extend(["-isystem", native / "generated"])
    run([cc, *flags, "-c", TOOL / "bo-smoke.c", "-o", OUT / "bo-smoke.o"])
    run(["cargo", "build", "--offline", "--locked", "--release", "--target", "aarch64-unknown-none",
         "-p", "kumo-rt", "-p", "kumo-gpu", "-p", "kumo-abi"])
    deps = ROOT / "target/aarch64-unknown-none/release"
    elf = OUT / "bo-smoke"
    command = ["rustc", "--edition=2021", "--target=aarch64-unknown-none", "-C", "panic=abort", "-C", "opt-level=2",
               "-C", f"link-arg=-T{ROOT / 'toolchain/relibc/smoke/link.ld'}"]
    for obj in [OUT / "bo-smoke.o", native / "kumo_kmod.o", native / "pan_props.o", native / "pan_model.o"]:
        command.extend(["-C", f"link-arg={obj}"])
    for name in ["kumo_rt", "kumo_gpu", "kumo_abi"]:
        command.extend(["--extern", f"{name}={deps / ('lib' + name + '.rlib')}"])
    command.extend(["-L", f"dependency={deps / 'deps'}", TOOL / "bo-smoke.rs", "-o", elf])
    run(command)
    undefined = run([nm, "--undefined-only", elf], capture_output=True, text=True).stdout
    assert not undefined.strip(), "BO proof ELF has unresolved symbols"
    run(["rustfmt", "--edition", "2021", "--check", TOOL / "bo-smoke.rs", TOOL / "native.rs"])
    paths = [elf, TOOL / "bo-smoke.c", TOOL / "bo-smoke.rs", TOOL / "native.rs", TOOL / "kumo_gpu.h", TOOL / "kumo_kmod.h",
             TOOL / "kumo_kmod.c", ROOT / "lib/kumo-gpu/src/bo.rs", ROOT / "lib/kumo-gpu/src/c.rs",
             ROOT / "lib/kumo-gpu/src/client.rs", native / "manifest.json"]
    (OUT / "manifest.json").write_text(json.dumps({"input_sha256": {
        str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in paths}}, indent=2) + "\n")
    print(f"G7o-c native BO compile/link green: {elf}", flush=True)
    if args.run:
        spec = importlib.util.spec_from_file_location("uefi_smoke", ROOT / "scripts/cxx-smoke.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        module.boot(ROOT / "build/images/qemu-virt-aarch64", 120, elf=elf,
                    label="G7o-c native BOs", green=GREEN, tag="G7o-c")


if __name__ == "__main__":
    main()
