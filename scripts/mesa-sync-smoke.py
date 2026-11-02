#!/usr/bin/env python3
#j749
#j750
"""Build native sync and submit-dependency proofs; optionally boot a disposable UEFI image."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "build/proofs/g7p-a"
GREEN = b"G7p-a native sync green: binary, timeline, accepted signals, snapshots, wait-pending, kernel wake/timeout, terminal loss; zero SyncRead"
DEPENDENCIES_GREEN = b"G7p-b submit dependencies green: captured waits gate Submit, timeout/refusal/failure preserve outputs, acceptance once; zero SyncRead"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--dependencies", action="store_true", help="select the G7p-b dependency acceptance and output directory")
    args = parser.parse_args()
    out = ROOT / "build/proofs/g7p-b" if args.dependencies else OUT
    label = "G7p-b submit dependencies" if args.dependencies else "G7p-a native sync"
    green = DEPENDENCIES_GREEN if args.dependencies else GREEN
    tag = "G7p-b" if args.dependencies else "G7p-a"
    out.mkdir(parents=True, exist_ok=True)
    source = ROOT / "toolchain/mesa/sync-smoke.rs"
    subprocess.run(["rustfmt", "--edition", "2021", "--check", source], cwd=ROOT, check=True)
    subprocess.run(["cargo", "build", "--offline", "--locked", "--release", "--target", "aarch64-unknown-none",
                    "-p", "kumo-rt", "-p", "kumo-gpu", "-p", "kumo-abi"], cwd=ROOT, check=True)
    deps = ROOT / "target/aarch64-unknown-none/release"
    elf = out / "sync-smoke"
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
    (out / "manifest.json").write_text(json.dumps({"input_sha256": {
        str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in inputs}}, indent=2) + "\n")
    print(f"{label} compile/link green: {elf}", flush=True)
    if args.run:
        spec = importlib.util.spec_from_file_location("uefi_smoke", ROOT / "scripts/cxx-smoke.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        transcript = module.boot(ROOT / "build/images/qemu-virt-aarch64", 120, elf=elf,
                                 label=label, green=green, tag=tag)
        assert transcript.count(GREEN) == 1 and transcript.count(DEPENDENCIES_GREEN) == 1
        assert transcript.index(GREEN) < transcript.index(DEPENDENCIES_GREEN)


if __name__ == "__main__":
    main()
