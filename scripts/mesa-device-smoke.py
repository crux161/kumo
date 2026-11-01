#!/usr/bin/env python3
#j745
"""Prove the native Mesa device/VM backend with the real C runtime and real VMOs."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / "toolchain/mesa"
OUT = ROOT / "build/proofs/g7o-d"
GREEN = b"G7o-d native device green: complete props, Mesa device/BO/VM lifecycle, immediate binds, generations, loss and contained close"


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
    for name in ["native-device", "device-smoke"]:
        run([cc, *flags, "-Wno-unused-parameter", "-c", TOOL / f"{name}.c", "-o", OUT / f"{name}.o"])
    run(["cargo", "build", "--offline", "--locked", "--release", "--target", "aarch64-unknown-none",
         "-p", "kumo-rt", "-p", "kumo-gpu", "-p", "kumo-abi"])
    deps = ROOT / "target/aarch64-unknown-none/release"
    elf = OUT / "device-smoke"
    run(["rustc", "--edition=2021", "--crate-type=staticlib", "--target=aarch64-unknown-none",
         "-C", "panic=abort", "-C", "opt-level=2", "--extern", f"kumo_gpu={deps / 'libkumo_gpu.rlib'}",
         "--extern", f"kumo_abi={deps / 'libkumo_abi.rlib'}", "-L", f"dependency={deps / 'deps'}",
         TOOL / "device-smoke.rs", "-o", OUT / "libdevice.a"])
    run([cc, "--target=aarch64-unknown-none", "-c", ROOT / "toolchain/relibc/smoke/crt0.S", "-o", OUT / "crt0.o"])
    mode = subprocess.check_output(["docker", "inspect", "--format", "{{.HostConfig.NetworkMode}}", "kumo-mesa-drivers-g7k"], text=True).strip()
    assert mode == "none", "native device build container must remain offline"
    # Mesa's exported archives are thin. Materialize their real cached members, rather
    # than using dangling host paths or replacing the generic Mesa lifecycle. — KESTREL
    materialize = """from pathlib import Path
import subprocess
root=Path('/work/kumo-mesa/build/src')
out=Path('/output/device-deps');out.mkdir(exist_ok=True)
for name, relative in [('kmod','panfrost/lib/kmod/libpankmod_lib.a'),('util','util/libmesa_util.a'),('c11','c11/impl/libmesa_util_c11.a')]:
    archive=root/relative
    members=subprocess.check_output(['llvm-ar-22','t',archive],text=True).splitlines()
    target=out/(name+'.a')
    target.unlink(missing_ok=True)
    subprocess.run(['llvm-ar-22','rcs',target,*[archive.parent/m for m in members]],check=True)
"""
    run(["docker", "exec", "kumo-mesa-drivers-g7k", "python3", "-c", materialize])
    for name in ['kmod', 'util', 'c11']:
        run(["docker", "cp", f"kumo-mesa-drivers-g7k:/output/device-deps/{name}.a", OUT / f"{name}.a"])
    command = ["ld.lld", "-static", "--gc-sections", "-z", "noexecstack", "-T", ROOT / "toolchain/relibc/smoke/link.ld",
               "-o", elf, OUT / "crt0.o", OUT / "device-smoke.o", OUT / "native-device.o", native / "kumo_kmod.o",
               native / "pan_props.o", native / "pan_model.o", "--start-group", OUT / "libdevice.a",
               OUT / "kmod.a", OUT / "util.a", OUT / "c11.a", sysroot / "lib/libc.a", "--end-group"]
    run(command)
    undefined = run([nm, "--undefined-only", elf], capture_output=True, text=True).stdout
    assert not undefined.strip(), "Device proof ELF has unresolved symbols"
    run(["rustfmt", "--edition", "2021", "--check", TOOL / "device-smoke.rs", TOOL / "native.rs"])
    paths = [elf, Path(__file__), TOOL / "device-smoke.c", TOOL / "device-smoke.rs", TOOL / "native-device.c", TOOL / "native.rs", TOOL / "kumo_gpu.h", TOOL / "kumo_kmod.h",
             TOOL / "kumo_kmod.c", ROOT / "lib/kumo-gpu/src/bo.rs", ROOT / "lib/kumo-gpu/src/c.rs",
             ROOT / "lib/kumo-gpu/src/client.rs", native / "manifest.json", OUT / "kmod.a", OUT / "util.a", OUT / "c11.a", sysroot / "lib/libc.a"]
    (OUT / "manifest.json").write_text(json.dumps({"input_sha256": {
        str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in paths}}, indent=2) + "\n")
    print(f"G7o-d native device compile/link green: {elf}", flush=True)
    if args.run:
        spec = importlib.util.spec_from_file_location("uefi_smoke", ROOT / "scripts/cxx-smoke.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        module.boot(ROOT / "build/images/qemu-virt-aarch64", 120, elf=elf,
                    label="G7o-d native device", green=GREEN, tag="G7o-d")


if __name__ == "__main__":
    main()
