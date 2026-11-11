#!/usr/bin/env python3
#j873
"""SH5-a: one QEMU boot asserting std::thread end to end.

Reuses the AXH harness's own boot machinery (import only). The case runs `ruststd threads`:
N threads meet at a barrier (live census), keep per-thread TLS and distinct thread ids,
pass a token round a mutex/condvar ring in exact order, join, and run 64 create/join
cycles with a flat thread and mapping census; then a child spawned from SMOKE.ELF proves
exit() from a non-main thread ends its whole process. The image is rebuilt with the proof
loglevel, then the default image is restored.
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
    parser.add_argument("--output", type=Path, default=ROOT / "build/verification/sh5/sh5a")
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
        "threads",
        b"echo AXH threads begins\nruststd threads\necho AXH threads ends\n",
        (
            "std: sh5 live -> OK",
            "std: sh5 tls -> OK",
            "std: sh5 tids -> OK",
            "std: sh5 handoff -> OK",
            "std: sh5 join -> OK",
            "std: sh5 cycles flat -> OK",
            "std: sh5 group exit -> OK",
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
        shutil.rmtree(work / "threads", ignore_errors=True)
        manifest["case"] = axh.boot(ROOT, source, case, work / "threads", qemu, firmware, args.timeout)
        manifest["passed"] = True
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        with (work / "restore-default.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=default_env, stdout=log, stderr=log, check=True)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("SH5-a thread smoke green", flush=True)


if __name__ == "__main__":
    main()
