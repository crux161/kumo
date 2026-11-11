#!/usr/bin/env python3
#j872
"""SH4-b: the process-spawn suite on the Orange Pi 5 Plus, one PXE boot.

    ruststd spawnparent    a child fed through its stdin pipe (exact stdout/stderr/
                           status through wait_with_output), a killed child reaped with
                           the kernel's killed status, and the no-grant proof

Reuses scripts/metal-sh3e.py's board machinery (import only): a private TFTP root under
the work directory served by pxehost, tio on a forked pty with the raw capture kept
beside the log, and the state-judged wake (reboot from a shell, the escape hatch only at
the login prompt, the documented UEFI front-page reset). Prerequisites, per the opi5
serial-loop recipe: serial adapter at /dev/tty.usbserial-B0001AH9, no other tio or
pxehost on the port, pxehost at build/tools/pxehost-kumo, and the image built with
KUMO_CONSOLE_LOGLEVEL=6 (every proof line is NOTICE).
"""

import argparse
import importlib.util
import json
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

BOOT = (
    b"echo SH4 begins\n"
    b"ruststd spawnparent\n"
    b"echo SH4 ends\n"
)
EXPECTED = (
    "std: sh3 spawn output -> OK",
    "std: sh3 spawn stderr -> OK",
    "std: sh3 spawn status -> OK",
    "std: sh3 spawn kill -> OK",
    "std: sh3 spawn nogrant -> OK",
)


def load_board():
    spec = importlib.util.spec_from_file_location("sh3e", ROOT / "scripts/metal-sh3e.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/sh4/sh4b")
    parser.add_argument("--timeout", type=float, default=180.0)
    args = parser.parse_args()

    board = load_board()
    if shutil.which("tio") is None:
        sys.exit("tio not on PATH")
    if not board.PXEHOST.is_file():
        sys.exit(f"missing {board.PXEHOST}; build with scripts/build-pxehost-kumo.sh")
    if not board.SOURCE_IMAGE.is_dir():
        sys.exit(
            f"missing {board.SOURCE_IMAGE}; run KUMO_CONSOLE_LOGLEVEL=6 cargo xtask image "
            "--arch aarch64 --hardware orange-pi-5-plus"
        )

    work = args.work
    work.mkdir(parents=True, exist_ok=True)
    tftp = work / "tftp"
    board.stage_tree(tftp)

    manifest = {"pxehost": str(board.PXEHOST)}
    board.stage_boot(tftp, BOOT)
    pxehost = board.start_pxehost(tftp, work / "pxehost.log")
    tio_pid, master = board.start_tio(work / "boot.log")
    try:
        board.wake_board(work / "boot.log")
        board.wait_for("SH4 ends", work / "boot.log", args.timeout)
        # validate() requires the login prompt that follows autoexec.
        board.wait_for("INDRA login:", work / "boot.log", 60)
        text = board.log_whole(work / "boot.log")
        board.validate(text, EXPECTED, "SH4 begins", "SH4 ends")
        manifest["boot"] = "PASS"
        manifest["passed"] = True
        print("boot: PASS", flush=True)
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        board.stop_all(pxehost, tio_pid, master)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("SH4-b metal suite green", flush=True)


if __name__ == "__main__":
    main()
