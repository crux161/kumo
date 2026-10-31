#!/usr/bin/env python3
#j736
"""Build G7l's C++ runtime and optionally prove it in a disposable full UEFI image."""
import argparse
from pathlib import Path
import shutil
import struct
import subprocess
import time

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "build/mesa-drivers/result/cxx"
GREEN = b"G7l C++ runtime green: static init, virtual dispatch, vector, sort, guarded statics, mutex, hash, TLS, exit"


def stage_image(source, destination, elf):
    assert not destination.exists(), f"refusing to replace existing image copy: {destination}"
    shutil.copytree(source, destination)
    path = destination / "EFI/KUMO/initrd.img"
    original = path.read_bytes()
    assert original[:8] == b"KUMORD01" and struct.unpack_from("<I", original, 8)[0] == 1
    count = struct.unpack_from("<I", original, 12)[0]
    out = bytearray(original[:16 + 80 * count])
    found = set()
    for index in range(count):
        entry = 16 + index * 80
        name = original[entry:entry + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", original, entry + 64)
        data = original[offset:offset + size]
        if name == b"etc/autoexec":
            data = b"dmesg -n 6\nrelibc g7l\necho G7l-after-launch\n"
            found.add(name)
        elif name == b"bin/relibc-smoke":
            data = elf.read_bytes()
            found.add(name)
        struct.pack_into("<QQ", out, entry + 64, len(out), len(data))
        out.extend(data)
    assert found == {b"etc/autoexec", b"bin/relibc-smoke"}
    path.write_bytes(out)


def boot(image, timeout):
    work = OUT / f"qemu-{time.time_ns()}"
    work.mkdir(parents=True)
    stage_image(image, work / "esp", OUT / "cxx-smoke")
    dtb = work / "esp/EFI/KUMO/dtb/qcom/sc8280xp-lenovo-thinkpad-x13s.dtb"
    dtb.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["qemu-system-aarch64", "-machine", f"virt,gic-version=3,dumpdtb={dtb}",
                    "-cpu", "cortex-a72", "-m", "512", "-display", "none", "-device", "ramfb", "-net", "none"],
                   capture_output=True, check=True, timeout=15)
    firmware = Path("/opt/homebrew/share/qemu")
    shutil.copyfile(firmware / "edk2-arm-vars.fd", work / "vars.fd")
    serial = work / "serial.log"
    print(f"G7l UEFI proof artifacts: {work}", flush=True)
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
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                assert process.poll() is None, f"QEMU exited; inspect {work}"
                transcript = serial.read_bytes() if serial.exists() else b""
                if b"G7l C++ runtime FAILED" in transcript:
                    raise RuntimeError(f"C++ proof failed; inspect {serial}")
                if all(marker in transcript for marker in (GREEN, b"G7l-after-launch", b"INDRA login:")):
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError(f"C++ proof timed out; inspect {serial}")
            before, proof = transcript.split(b"G7l C++ runtime begin", 1)
            assert proof.count(GREEN) == 1 and b"TOWER" not in proof and b"FAILED" not in proof
            # Standing QEMU board-UART exception, restricted to pre-proof faults.
            # It is the same allowance as thread-tls-smoke.py. — SAKER
            faults = before.split(b"TOWER (reversed): MICROSERVICE CONTAINED")[1:]
            assert len(faults) in (0, 2) and before.count(b"TOWER") == len(faults)
            for fault in faults:
                assert b"cause=FAULT" in fault and b"FAR=0x0000000011000000" in fault
                assert b"synchronous external abort on read" in fault
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
    action = parser.add_mutually_exclusive_group()
    action.add_argument("--run", action="store_true")
    action.add_argument("--stage", type=Path, help="create a separate ESP for metal acceptance; never install it")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--timeout", type=float, default=90)
    args = parser.parse_args()
    subprocess.run(["docker", "exec", "kumo-mesa-drivers-g7k", "python3",
                    "/repo/toolchain/cxx/build.py"], check=True)
    if args.run:
        boot(args.image, args.timeout)
    elif args.stage:
        stage_image(args.image, args.stage, OUT / "cxx-smoke")
        print(f"G7l metal proof staged: {args.stage.resolve()}", flush=True)


if __name__ == "__main__":
    main()
