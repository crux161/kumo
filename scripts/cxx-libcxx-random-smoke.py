#!/usr/bin/env python3
#J947_r13_001
"""SH9-c8: std::random_device over the kernel CSPRNG, proven on two UEFI QEMU boots.

The seeded boot attaches `virtio-rng-device`, so edk2 offers EFI_RNG_PROTOCOL and Nijigumo
seeds the kernel; the proof must draw real entropy. The unseeded boot has no RNG device, so
the kernel stays unseeded; the same ELF must see random_device, getentropy and blocking
getrandom refuse while GRND_INSECURE still answers. Each boot must also print Nijigumo's
matching `rng seed` line. — TEAL
"""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "SH9-c8 entropy"
MODES = {
    "seeded": {
        "qemu_args": ("-device", "virtio-rng-device"),
        "loader": b"rng seed     : 32 bytes from EFI_RNG_PROTOCOL",
        "records": (
            "SH9-c8: random_device draws from the kernel CSPRNG -> OK",
            "SH9-c8: getentropy, its limit, and a multi-read getrandom -> OK",
            "SH9-c8 entropy green (seeded)",
        ),
    },
    "unseeded": {
        "qemu_args": (),
        "loader": b"rng seed     : none (EFI_RNG_PROTOCOL locate",
        "records": (
            "SH9-c8: an unseeded kernel refuses random_device, getentropy and getrandom -> OK",
            "SH9-c8: GRND_INSECURE still answers unseeded -> OK",
            "SH9-c8 entropy green (unseeded)",
        ),
    },
}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def commands(mode):
    return f"dmesg -n 6\nrelibc sh9-c8 {mode}\necho sh9-c8-after-launch\n".encode()


def check(transcript, mode):
    spec = MODES[mode]
    if spec["loader"] not in transcript:
        raise RuntimeError(f"{mode}: Nijigumo did not report the expected rng seed line")
    proof = transcript.split((LABEL + " begin").encode(), 1)[1]
    records = [record.encode() for record in spec["records"]]
    positions = [proof.find(record) for record in records]
    if any(proof.count(record) != 1 for record in records) or positions != sorted(positions):
        raise RuntimeError(f"{mode}: entropy records are missing, duplicated, or out of order")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true", required=True, help="boot both modes under UEFI QEMU")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/qemu-virt-aarch64")
    parser.add_argument("--sysroot", type=Path, default=ROOT / "target/kumo-sysroot")
    parser.add_argument("--out", type=Path, default=ROOT / "target/cxx-libcxx-random")
    parser.add_argument("--eh-out", type=Path, default=ROOT / "target/cxx-eh")
    parser.add_argument("--evidence", type=Path, default=ROOT / "build/verification/r13/sh9-c8")
    parser.add_argument("--timeout", type=float, default=180)
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    build = [sys.executable, ROOT / "toolchain/cxx/build-libcxx.py", "--sysroot", args.sysroot, "--out", args.out,
             "--eh-out", args.eh_out,
             "--smoke", ROOT / "toolchain/cxx/libcxx-random-smoke.cpp", "--elf-name", "cxx-libcxx-random-smoke"]
    subprocess.run(list(map(str, build)), check=True)
    out = args.out.resolve()
    elf = evidence / "cxx-libcxx-random-smoke"
    for name in ("cxx-libcxx-random-smoke", "manifest.json", "undefined.txt", "install/lib/libc++.a", "link.map"):
        shutil.copyfile(out / name, evidence / Path(name).name)
    result = {"elf_sha256": sha(elf), "boots": {}}
    helpers = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    try:
        for mode, spec in MODES.items():
            transcript = helpers["boot"](args.image.resolve(), args.timeout, elf=elf, label=LABEL,
                                         green=spec["records"][-1].encode(), tag="sh9-c8",
                                         commands=commands(mode), qemu_args=spec["qemu_args"])
            check(transcript, mode)
            log = evidence / f"qemu-serial-{mode}.log"
            log.write_bytes(transcript)
            result["boots"][mode] = {"status": "PASS", "serial_sha256": sha(log)}
            print(f"SH9-c8 entropy QEMU {mode} green", flush=True)
        result["boots"]["qemu"] = "PASS"
    except BaseException as error:
        result["passed"] = False
        result["error"] = str(error)
        raise
    else:
        result["passed"] = True
    finally:
        (evidence / "proof.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
