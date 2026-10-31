#!/usr/bin/env python3
#j740
"""Close both Mesa links, optionally execute the actual utilities on KUMO."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "build/mesa-drivers/result/closure"
GREEN = b"G7m Mesa utilities green: ralloc, hash, sparse, dynarray, mutex, once, sort, DRM refusal, dlopen refusal, static build ID"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true")
    args = parser.parse_args()
    mode = subprocess.check_output(["docker", "inspect", "--format", "{{.HostConfig.NetworkMode}}",
                                    "kumo-mesa-drivers-g7k"], text=True).strip()
    assert mode == "none", "target build container must be offline"
    subprocess.run(["docker", "exec", "kumo-mesa-drivers-g7k", "python3", "/repo/toolchain/mesa/build-closure.py"], check=True)
    if args.run:
        spec = importlib.util.spec_from_file_location("uefi_c_smoke", ROOT / "scripts/cxx-smoke.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        transcript = module.boot(ROOT / "build/images/qemu-virt-aarch64", 90, elf=OUT / "util-smoke",
                                 label="G7m Mesa utilities", green=GREEN, tag="G7m-util")
        expected = json.loads((OUT / "manifest.json").read_text())["build_id"]
        assert ("G7m static build ID: " + expected).encode() in transcript, "runtime/build-time identity mismatch"
        print("G7m static build ID agrees with llvm-readelf", flush=True)


if __name__ == "__main__":
    main()
