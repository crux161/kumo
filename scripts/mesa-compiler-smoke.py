#!/usr/bin/env python3
#j741
"""G7n: build Mesa's compilers for KUMO and Linux; optionally run the KUMO build under QEMU.

The offline container builds both and checks the Linux reference against the shader recipe's own
panfrost_compile. With --run, the KUMO ELF must print every reference line exactly.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "build/mesa-drivers/result/compiler"
LABEL = "G7n compiler smoke"
GREEN = (b"G7n compiler smoke green: panfrost_compile kumo_paint SPIR-V, GLSL ES paint.comp, "
         b"GLSL ES math.comp")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true", help="run the KUMO build in a disposable QEMU image")
    parser.add_argument("--timeout", type=int, default=600)
    args = parser.parse_args()
    mode = subprocess.check_output(["docker", "inspect", "--format", "{{.HostConfig.NetworkMode}}",
                                    "kumo-mesa-drivers-g7k"], text=True).strip()
    assert mode == "none", "target build container must be offline"
    subprocess.run(["docker", "exec", "kumo-mesa-drivers-g7k", "python3",
                    "/repo/toolchain/mesa/build-compiler.py"], check=True)
    if not args.run:
        return
    expected = json.loads((OUT / "manifest.json").read_text())["expected_lines"]
    assert len(expected) == 4, expected
    spec = importlib.util.spec_from_file_location("uefi_c_smoke", ROOT / "scripts/cxx-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    transcript = module.boot(ROOT / "build/images/qemu-virt-aarch64", args.timeout,
                             elf=OUT / "compiler-smoke", label=LABEL, green=GREEN, tag="G7n-cc")
    missing = [line for line in expected if line.encode() not in transcript]
    assert not missing, "KUMO output differs from the Linux reference:\n" + "\n".join(missing)
    print("G7n QEMU green: KUMO printed every Linux reference line byte for byte", flush=True)
    for line in expected:
        print(line, flush=True)


if __name__ == "__main__":
    main()
