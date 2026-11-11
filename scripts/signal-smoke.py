#!/usr/bin/env python3
#j875
"""SH6-a: one QEMU boot asserting the signal subset end to end.

Reuses the AXH harness's own boot machinery (import only). The case runs `ruststd signals`:
sigaction/raise/sigprocmask/sigpending/sigaltstack round-trips and std's SIGPIPE-ignore
(EPIPE) in process; then a child from SMOKE.ELF overflows its stack into the guard page
under a rustc-style SA_ONSTACK|SA_SIGINFO handler (the kernel redirects the fault to the
handler on the alternate stack; the handler reports and re-raises with SIG_DFL, so the
parent sees WTERMSIG 11), and a second child dies by SIGPIPE at SIG_DFL (WTERMSIG 13).
The case allows no TOWER: the overflow must be reported through the handler. The image is
rebuilt with the proof loglevel, then the default image is restored.
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
    parser.add_argument("--output", type=Path, default=ROOT / "build/verification/sh6/sh6a")
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
        "signals",
        b"echo AXH signals begins\nruststd signals\necho AXH signals ends\n",
        (
            "std: sh6 state -> OK",
            "std: sh6 epipe -> OK",
            "sh6 handler: SIGSEGV code=1 guard=yes altstack=yes",
            "std: sh6 overflow handler -> OK",
            "std: sh6 overflow signaled -> OK",
            "std: sh6 sigpipe default -> OK",
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
        shutil.rmtree(work / "signals", ignore_errors=True)
        manifest["case"] = axh.boot(ROOT, source, case, work / "signals", qemu, firmware, args.timeout)
        manifest["passed"] = True
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        with (work / "restore-default.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=default_env, stdout=log, stderr=log, check=True)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("SH6-a signal smoke green", flush=True)


if __name__ == "__main__":
    main()
