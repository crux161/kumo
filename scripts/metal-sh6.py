#!/usr/bin/env python3
#j876
#j878
"""SH6 metal: the signal-subset suite on the Orange Pi 5 Plus, one PXE boot.

    ruststd signals    sigaction/raise/sigprocmask/sigpending/sigaltstack round-trips and
                       std's SIGPIPE-ignore (EPIPE); a child overflows its stack into the
                       guard page under a rustc-style SA_ONSTACK|SA_SIGINFO handler, which
                       reports on the alternate stack and re-raises (WTERMSIG 11, no
                       TOWER); a child dies by SIGPIPE at SIG_DFL (WTERMSIG 13)

The metal half of scripts/signal-smoke.py. Reuses scripts/metal-sh3e.py's board machinery
(import only), like the other metal suites. Prerequisites, per the opi5 serial-loop
recipe: serial adapter at /dev/tty.usbserial-B0001AH9, no other tio or pxehost on the
port, pxehost at build/tools/pxehost-kumo, and the image built with
KUMO_CONSOLE_LOGLEVEL=6 (every proof line is NOTICE).
"""

import argparse
import importlib.util
import json
import shutil
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

BOOT = (
    b"echo SH6 begins\n"
    b"ruststd signals\n"
    b"ruststd sigasync\n"
    b"echo SH6 ends\n"
)
EXPECTED = (
    "std: sh6 state -> OK",
    "std: sh6 epipe -> OK",
    "sh6 handler: SIGSEGV code=1 guard=yes altstack=yes",
    "std: sh6 overflow handler -> OK",
    "std: sh6 overflow signaled -> OK",
    "std: sh6 sigpipe default -> OK",
    # SH6-b: asynchronous delivery (PROPOSAL/004) and the group SIGINT to a spawn tree.
    "std: sh6 async catch -> OK",
    "std: sh6 async default -> OK",
    "std: sh6 async ignore -> OK",
    "std: sh6 async sigkill -> OK",
    "std: sh6 async tree default -> OK",
    "std: sh6 async tree catch -> OK",
)


def load_board():
    spec = importlib.util.spec_from_file_location("sh3e", ROOT / "scripts/metal-sh3e.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/sh6/sh6-metal")
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
        board.wait_for("SH6 ends", work / "boot.log", args.timeout)
        # The login prompt that follows autoexec must come AFTER the end marker: a capture
        # that starts at a stale login (the board's state before the reset) already holds
        # one, which board.wait_for and validate() alone would accept.
        deadline = time.monotonic() + 60
        while True:
            # Prompts end without a newline, so only the raw capture holds them.
            raw = board.raw_text_since(work / "boot.log", 0)
            if "SH6 ends" in raw and "INDRA login:" in raw[raw.rindex("SH6 ends"):]:
                break
            if time.monotonic() > deadline:
                raise RuntimeError("no login prompt after SH6 ends")
            time.sleep(0.2)
        text = board.log_whole(work / "boot.log")
        board.validate(text, EXPECTED, "SH6 begins", "SH6 ends")
        window = text[text.index("SH6 begins"):text.index("SH6 ends")]
        if "TOWER" in window:
            raise RuntimeError("a TOWER inside the SH6 window: the overflow was not handled")
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
    print("SH6 metal suite green", flush=True)


if __name__ == "__main__":
    main()
