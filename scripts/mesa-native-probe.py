#!/usr/bin/env python3
#j643
#j644
#j645
"""Compile/link the native Mesa property boundary; optionally run a modeled-service UEFI proof.

No download, image installation or hardware reset. See toolchain/mesa/README.md.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import struct
import subprocess
import tarfile
import time

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / "toolchain/mesa"
TAR_SHA = "794b3d1729075dae7ebd47c7283b7ca0cdb5dec668f7f11cc0a5d82f6138a0a5"
HEADER_SHA = "55fa7f89e5728a209fb615a26ee4099733bba29cee0695c754878f0c5dc6c4c4"
GREEN = b"G7d native Mesa properties green: upstream topology and VA-range derivation, refusal, loss; modeled service"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(command, **kwargs):
    print("+ " + " ".join(map(str, command)), flush=True)
    return subprocess.run(list(map(str, command)), cwd=ROOT, check=True, **kwargs)


def copy_initrd(path, probe):
    original = path.read_bytes()
    assert original[:8] == b"KUMORD01" and struct.unpack_from("<I", original, 8)[0] == 1
    count = struct.unpack_from("<I", original, 12)[0]
    out = bytearray(original[:16 + 80 * count])
    replaced = set()
    for index in range(count):
        entry = 16 + index * 80
        name = original[entry:entry + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", original, entry + 64)
        data = original[offset:offset + size]
        if name == b"etc/autoexec":
            data = b"dmesg -n 6\nrelibc g7d\necho G7d-after-launch\n"
            replaced.add(name)
        elif name == b"bin/relibc-smoke":
            data = probe.read_bytes()
            replaced.add(name)
        struct.pack_into("<QQ", out, entry + 64, len(out), len(data))
        out.extend(data)
    assert replaced == {b"etc/autoexec", b"bin/relibc-smoke"}, "missing disposable probe slots"
    path.write_bytes(out)


def boot(out, probe):
    work = out / "qemu"
    if work.exists():
        shutil.rmtree(work)
    work.mkdir()
    shutil.copytree(ROOT / "build/images/qemu-virt-aarch64", work / "esp")
    copy_initrd(work / "esp/EFI/KUMO/initrd.img", probe)
    dtb = work / "esp/EFI/KUMO/dtb/qcom/sc8280xp-lenovo-thinkpad-x13s.dtb"
    dtb.parent.mkdir(parents=True, exist_ok=True)
    run(["qemu-system-aarch64", "-machine", f"virt,gic-version=3,dumpdtb={dtb}",
         "-cpu", "cortex-a72", "-m", "512", "-display", "none", "-device", "ramfb", "-net", "none"],
        capture_output=True, timeout=15)
    firmware = Path("/opt/homebrew/share/qemu")
    shutil.copyfile(firmware / "edk2-arm-vars.fd", work / "vars.fd")
    serial = work / "serial.log"
    with (work / "qemu.log").open("wb") as log:
        process = subprocess.Popen([
            "qemu-system-aarch64", "-machine", "virt,gic-version=3", "-cpu", "cortex-a72",
            "-m", "512", "-display", "none", "-device", "ramfb", "-monitor", "none",
            "-serial", f"file:{serial}",
            "-drive", f"if=pflash,format=raw,file={firmware / 'edk2-aarch64-code.fd'},readonly=on",
            "-drive", f"if=pflash,format=raw,file={work / 'vars.fd'}",
            "-drive", f"file=fat:rw:{work / 'esp'},format=raw,if=virtio", "-net", "none",
        ], stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline:
                assert process.poll() is None, f"QEMU exited; inspect {work}"
                transcript = serial.read_bytes() if serial.exists() else b""
                if all(marker in transcript for marker in [GREEN, b"G7d-after-launch", b"INDRA login:"]):
                    break
                if b"G7d native Mesa properties FAILED" in transcript:
                    raise RuntimeError(f"native proof failed; inspect {serial}")
                time.sleep(0.1)
            else:
                raise RuntimeError(f"native proof timed out; inspect {serial}")
            before, proof = transcript.split(b"G7d native Mesa properties begin", 1)
            assert proof.count(GREEN) == 1 and b"TOWER" not in proof and b"FAILED" not in proof
            # Same narrow standing QEMU UART exception allowance as thread-tls-smoke;
            # it applies only before this separate process starts. — KESTREL
            faults = before.split(b"TOWER (reversed): MICROSERVICE CONTAINED")[1:]
            assert len(faults) in (0, 2) and before.count(b"TOWER") == len(faults)
            for fault in faults:
                assert b"cause=FAULT" in fault and b"FAR=0x0000000011000000" in fault
                assert b"synchronous external abort on read" in fault
            print(f"Pre-proof baseline UART external aborts: {len(faults)}")
            print(GREEN.decode(), flush=True)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-tar", type=Path, default=ROOT / "build/mesa-native/mesa.tar.xz")
    parser.add_argument("--run", action="store_true", help="run in a disposable full UEFI QEMU image")
    args = parser.parse_args()
    assert sha(args.source_tar) == TAR_SHA, "Mesa source tarball does not match pin"
    sysroot = ROOT / "target/kumo-sysroot"
    assert (sysroot / "lib/libc.a").is_file(), "run scripts/relibc-headers.sh first"
    out = ROOT / "build/mesa-native/probe"
    out.mkdir(parents=True, exist_ok=True)
    source = out / "source"
    if source.exists():
        shutil.rmtree(source)
    source.mkdir()
    with tarfile.open(args.source_tar) as tar:
        tar.extractall(source, filter="data")
    mesa = source / "mesa-26.2.5"
    header = mesa / "src/panfrost/lib/kmod/pan_kmod.h"
    assert sha(header) == HEADER_SHA, "Mesa header does not match pin"
    run(["patch", "--batch", "-p1", "-d", mesa, "-i", TOOL / "native-header.patch"])

    # Keep the original sysroot immutable. Parenthesized function declarators avoid
    # expansion of relibc's existing classification macros; no ABI/body changes. — KESTREL
    compat = out / "compat"
    compat.mkdir(exist_ok=True)
    math = (sysroot / "include/math.h").read_text()
    for name in ("isfinite", "isinf", "isnan", "isnormal"):
        declaration = f"int {name}(double x);"
        assert math.count(declaration) == 1, f"recheck math-header workaround: {name}"
        math = math.replace(declaration, f"int ({name})(double x);")
    (compat / "math.h").write_text(math)

    cc = shutil.which("clang")
    ld = shutil.which("ld.lld")
    nm = shutil.which("llvm-nm")
    assert cc and ld and nm, "clang, ld.lld and llvm-nm are required"
    flags = ["--target=aarch64-unknown-none", "-D__kumo__", "-std=c11", "-O2", "-ffreestanding",
             "-fno-pic", "-fno-pie", "-fno-stack-protector", "-ffunction-sections", "-fdata-sections",
             "-Wall", "-Wextra", "-Werror", f"--sysroot={sysroot}", "-isystem", str(sysroot / "include"),
             "-I", str(TOOL), "-I", str(compat), "-DHAVE_PTHREAD", "-DHAVE_ENDIAN_H", "-DHAVE_STRUCT_TIMESPEC"]
    for include in ["include", "src", "src/panfrost", "src/panfrost/lib", "src/panfrost/model"]:
        flags.extend(["-isystem", str(mesa / include)])
    generated = out / "generated/genxml"
    generated.mkdir(parents=True, exist_ok=True)
    with (generated / "common_pack.h").open("w") as generated_header:
        run(["python3", mesa / "src/panfrost/genxml/gen_pack.py", mesa / "src/panfrost/genxml/common.xml"], stdout=generated_header)
    flags.extend(["-isystem", str(generated.parent)])
    run([cc, *flags, "-MD", "-MF", out / "headers.d", "-c", TOOL / "kumo_kmod.c", "-o", out / "kumo_kmod.o"])
    run([cc, *flags, "-Wno-unused-parameter", "-MD", "-MF", out / "props-headers.d", "-c", mesa / "src/panfrost/lib/pan_props.c", "-o", out / "pan_props.o"])
    run(["cargo", "build", "--offline", "--locked", "-p", "kumo-gpu", "--release", "--target", "aarch64-unknown-none"])
    deps = ROOT / "target/aarch64-unknown-none/release"
    run(["rustc", "--edition=2021", "--crate-type=staticlib", "--target=aarch64-unknown-none",
         "-C", "panic=abort", "-C", "opt-level=2", "--extern", f"kumo_gpu={deps / 'libkumo_gpu.rlib'}",
         "-L", f"dependency={deps / 'deps'}", TOOL / "probe.rs", "-o", out / "libprobe.a"])
    for name, path in [("main", TOOL / "probe.c"), ("crt0", ROOT / "toolchain/relibc/smoke/crt0.S")]:
        run([cc, *flags, "-c", path, "-o", out / f"{name}.o"])
    elf = out / "mesa-native-probe"
    run([ld, "-static", "--gc-sections", "-z", "noexecstack", "-T", ROOT / "toolchain/relibc/smoke/link.ld",
         f"-Map={out / 'link.map'}", "-o", elf, out / "crt0.o", out / "main.o", out / "kumo_kmod.o",
         out / "pan_props.o", out / "libprobe.a", sysroot / "lib/libc.a"])
    symbols = run([nm, "--undefined-only", out / "kumo_kmod.o"], capture_output=True, text=True).stdout
    (out / "backend-undefined.txt").write_text(symbols)
    assert not re.search(r"\b(?:drm\w*|ioctl|mmap|pthread\w*|mtx_\w*|ralloc\w*|util_\w*)\b", symbols), "unexpected backend runtime expansion"
    expected = {"pan_query_core_count", "pan_query_core_id_range"}
    assert {line.split()[-1] for line in symbols.splitlines() if line.strip()} == expected, "unexpected C backend dependency"
    linked = run([nm, "--defined-only", elf], capture_output=True, text=True).stdout
    (out / "elf-defined.txt").write_text(linked)
    assert expected <= {line.split()[-1] for line in linked.splitlines() if line.strip()}, "Mesa helpers absent from native ELF"
    undefined = run([nm, "--undefined-only", elf], capture_output=True, text=True).stdout
    (out / "elf-undefined.txt").write_text(undefined)
    assert not undefined.strip(), "native ELF has unresolved symbols"
    run(["rustfmt", "--edition", "2021", "--check", TOOL / "probe.rs"])
    dependencies = []
    for depfile in ["headers.d", "props-headers.d"]:
        dependencies.extend((out / depfile).read_text().replace("\\\n", " ").split(":", 1)[1].split())
    inputs = set(map(Path, dependencies)) | set(TOOL.iterdir()) | {
        ROOT / "lib/kumo-gpu/src/lib.rs", ROOT / "lib/kumo-gpu/src/c.rs",
        ROOT / "lib/kumo-gpu/src/client.rs", ROOT / "lib/kumo-gpu/src/wire.rs",
        ROOT / "lib/kumo-gpu/src/session.rs", sysroot / "lib/libc.a", sysroot / "include/math.h", elf,
        mesa / "src/panfrost/genxml/gen_pack.py", mesa / "src/panfrost/genxml/common.xml",
    }
    versions = {tool: run([tool, "--version"], capture_output=True, text=True).stdout for tool in [cc, ld, "rustc"]}
    (out / "manifest.json").write_text(json.dumps({"mesa_tar_sha256": TAR_SHA, "tools": versions,
        "input_sha256": {str(p): sha(p) for p in sorted(inputs) if p.is_file()}}, indent=2) + "\n")
    print(f"G7d native compile/link green: {elf}; dependency hashes and runtime symbols in {out}", flush=True)
    if args.run:
        boot(out, elf)


if __name__ == "__main__":
    main()
