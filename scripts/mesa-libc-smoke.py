#!/usr/bin/env python3
#j738
"""Build and optionally boot the real libc closure required by Mesa."""
import argparse
import importlib.util
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "build/mesa-libc"
GREEN = b"G7m libc closure green: memory stream growth, seek, holes, ownership, refusal, math"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true")
    args = parser.parse_args()
    OUT.mkdir(exist_ok=True)
    elf = OUT / "mesa-libc-smoke"
    subprocess.run([str(ROOT / "scripts/kumo-cc.sh"), str(elf),
                    str(ROOT / "toolchain/mesa/libc-smoke.c")], check=True)
    if args.run:
        spec = importlib.util.spec_from_file_location("uefi_c_smoke", ROOT / "scripts/cxx-smoke.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        module.boot(ROOT / "build/images/qemu-virt-aarch64", 90, elf=elf,
                    label="G7m libc closure", green=GREEN, tag="G7m-libc")


if __name__ == "__main__":
    main()
