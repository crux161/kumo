#!/usr/bin/env python3
#j743
"""Build G7o-b's real DMA-memory probe; optionally boot a disposable QEMU image."""
import argparse
import importlib.util
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "build/proofs/g7o-b"
GREEN = b"G7o-b GPU memory green: 64 MiB, fragmented pages, shared NC aliases, retained guards, reclamation and zeroed reuse"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true")
    args = parser.parse_args()
    OUT.mkdir(parents=True, exist_ok=True)
    subprocess.run(["cargo", "build", "--offline", "--locked", "--release", "--target", "aarch64-unknown-none",
                    "-p", "kumo-rt", "-p", "kumo-gpu", "-p", "kumo-abi"], cwd=ROOT, check=True)
    deps = ROOT / "target/aarch64-unknown-none/release"
    elf = OUT / "memory-smoke"
    subprocess.run(["rustc", "--edition=2021", "--target=aarch64-unknown-none", "-C", "panic=abort",
                    "-C", "opt-level=2", "-C", f"link-arg=-T{ROOT / 'toolchain/relibc/smoke/link.ld'}",
                    "--extern", f"kumo_rt={deps / 'libkumo_rt.rlib'}", "--extern", f"kumo_gpu={deps / 'libkumo_gpu.rlib'}",
                    "--extern", f"kumo_abi={deps / 'libkumo_abi.rlib'}",
                    "-L", f"dependency={deps / 'deps'}", ROOT / "toolchain/mesa/memory-smoke.rs", "-o", elf],
                   cwd=ROOT, check=True)
    if args.run:
        spec = importlib.util.spec_from_file_location("uefi_smoke", ROOT / "scripts/cxx-smoke.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        module.boot(ROOT / "build/images/qemu-virt-aarch64", 120, elf=elf,
                    label="G7o-b GPU memory", green=GREEN, tag="G7o-b")
    else:
        print(f"G7o-b GPU memory probe built: {elf}")


if __name__ == "__main__":
    main()
