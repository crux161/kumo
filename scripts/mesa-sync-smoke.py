#!/usr/bin/env python3
#j749
"""Build G7p-a's native sync proof; optionally boot a disposable full UEFI QEMU image."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "build/proofs/g7p-a"
GREEN = b"G7p-a native sync green: binary, timeline, accepted signals, snapshots, wait-pending, kernel wake/timeout, terminal loss; zero SyncRead"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true")
    args = parser.parse_args()
    OUT.mkdir(parents=True, exist_ok=True)
    source = ROOT / "toolchain/mesa/sync-smoke.rs"
    subprocess.run(["rustfmt", "--edition", "2021", "--check", source], cwd=ROOT, check=True)
    subprocess.run(["cargo", "build", "--offline", "--locked", "--release", "--target", "aarch64-unknown-none",
                    "-p", "kumo-rt", "-p", "kumo-gpu", "-p", "kumo-abi"], cwd=ROOT, check=True)
    deps = ROOT / "target/aarch64-unknown-none/release"
    elf = OUT / "sync-smoke"
    command = ["rustc", "--edition=2021", "--target=aarch64-unknown-none", "-C", "panic=abort",
               "-C", "opt-level=2", "-C", f"link-arg=-T{ROOT / 'toolchain/relibc/smoke/link.ld'}"]
    for name in ["kumo-rt", "kumo-gpu", "kumo-abi"]:
        crate = name.replace("-", "_")
        command.extend(["--extern", f"{crate}={deps / ('lib' + crate + '.rlib')}"])
    command.extend(["-L", f"dependency={deps / 'deps'}", source, "-o", elf])
    subprocess.run(command, cwd=ROOT, check=True)
    inputs = [Path(__file__), source, ROOT / "lib/kumo-gpu/src/sync.rs",
              ROOT / "lib/kumo-gpu/src/client.rs", ROOT / "lib/kumo-gpu/src/session.rs",
              ROOT / "lib/kumo-gpu/src/wire.rs", elf]
    (OUT / "manifest.json").write_text(json.dumps({"input_sha256": {
        str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in inputs}}, indent=2) + "\n")
    print(f"G7p-a native sync compile/link green: {elf}", flush=True)
    if args.run:
        spec = importlib.util.spec_from_file_location("uefi_smoke", ROOT / "scripts/cxx-smoke.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        module.boot(ROOT / "build/images/qemu-virt-aarch64", 120, elf=elf,
                    label="G7p-a native sync", green=GREEN, tag="G7p-a")


if __name__ == "__main__":
    main()
