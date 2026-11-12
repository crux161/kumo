#!/usr/bin/env python3
#j885
"""SH7-b: one QEMU boot proving the in-process persona shim (PLAN.md §10 R1).

Reuses the AXH harness's own boot machinery (import only). Sora's boot runs the M10 aarch64
Linux ELF inside `persona-linux-shim`: the shim loads the program into its own address space,
the kernel redirects the program's `svc`s to the shim (no Linux number in the kernel), and the
program's `write` arrives as an INFO record. The case requires the program's line and Sora's
`M10-c Check ok` exactly once each, and no TOWER. The image is rebuilt with the proof loglevel,
then the default image is restored.
"""

import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load_axh():
    spec = importlib.util.spec_from_file_location("axh", ROOT / "scripts/netstack-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "build/verification/sh7/sh7b")
    parser.add_argument("--timeout", type=float, default=90)
    args = parser.parse_args()

    axh = load_axh()
    qemu = shutil.which("qemu-system-aarch64")
    if qemu is None:
        sys.exit("qemu-system-aarch64 missing")
    firmware = Path(qemu).resolve().parent.parent / "share/qemu"

    source = ROOT / "build/images/qemu-virt-aarch64"
    work = args.output
    work.mkdir(parents=True, exist_ok=True)

    case = axh.Case(
        "persona",
        b"echo AXH persona begins\necho AXH persona ends\n",
        (
            "M10 elf linux hi",
            "M10-c Check ok",
        ),
        ready=0,
    )

    proof_env = dict(os.environ, KUMO_CONSOLE_LOGLEVEL="6")
    default_env = {k: v for k, v in os.environ.items() if k not in axh.CONTROLS}
    manifest = {"head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()}
    try:
        with (work / "build-proof.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=proof_env, stdout=log, stderr=log, check=True)
        # boot() insists on a fresh case directory; a previous run's evidence must not
        # block or pollute this one.
        shutil.rmtree(work / "persona", ignore_errors=True)
        manifest["case"] = axh.boot(ROOT, source, case, work / "persona", qemu, firmware, args.timeout)
        manifest["passed"] = True
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        with (work / "restore-default.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=default_env, stdout=log, stderr=log, check=True)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("SH7-b persona smoke green", flush=True)


if __name__ == "__main__":
    main()
