#!/usr/bin/env python3
#j897
"""j897: one QEMU boot proving a std program starts with no stdin and no filesystem grant.

`run sdk-smoke` grants stdout only. Since j868 relibc's fcntl(F_GETFD) reports the closed
fd 0 as EBADF, so std's start-up opens /dev/null to fill it and aborts (brk #1, a TOWER on
every default boot) when that fails. relibc now answers /dev/null itself. The case allows no
TOWER and asserts sdk-smoke reached main, then used /dev/null by path.

Reuses the AXH harness's boot machinery (import only). The image is rebuilt with the proof
loglevel, then the default image is restored.
"""

import argparse
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load_axh():
    spec = importlib.util.spec_from_file_location("axh", ROOT / "scripts/netstack-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "build/verification/j897")
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
        "devnull",
        b"echo AXH devnull begins\nrun sdk-smoke\necho AXH devnull ends\n",
        (
            "sdk-smoke: stdout via relibc stdio -> OK",
            "sdk-smoke: /dev/null write=5 read=0 chardev without an fs grant -> OK",
        ),
        ready=0,
    )

    proof_env = dict(os.environ, KUMO_CONSOLE_LOGLEVEL="6")
    default_env = {k: v for k, v in os.environ.items() if k not in axh.CONTROLS}
    manifest = {"head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()}
    try:
        with (work / "build-proof.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=proof_env, stdout=log, stderr=log, check=True)
        shutil.rmtree(work / "devnull", ignore_errors=True)
        # Since PROPOSAL/025 Z1 Nijigumo boots initrd.img.zst when the manifest names it,
        # so the raw initrd boot() stages would never be seen (j891 fixes netstack-smoke on
        # its own branch). Boot from a copy whose manifest names only the raw initrd.
        raw_source = work / "esp-raw"
        shutil.rmtree(raw_source, ignore_errors=True)
        shutil.copytree(source, raw_source)
        conf = raw_source / "EFI/KUMO/nijigumo.conf"
        conf.write_text(re.sub(r"(?m)^initrd_(zstd|zstd_sha256|size) = .*\n", "", conf.read_text()))
        (raw_source / "EFI/KUMO/initrd.img.zst").unlink(missing_ok=True)
        manifest["case"] = axh.boot(ROOT, raw_source, case, work / "devnull", qemu, firmware, args.timeout)
        manifest["passed"] = True
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        with (work / "restore-default.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=default_env, stdout=log, stderr=log, check=True)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("j897 /dev/null smoke green", flush=True)


if __name__ == "__main__":
    main()
