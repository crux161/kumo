#!/usr/bin/env python3
#J937_r03_001
"""SH9-a metal acceptance: one Orange Pi 5 Plus PXE boot of the native unwind proof."""

import argparse
import hashlib
import importlib.util
import json
import runpy
import shutil
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROOF = ROOT / "build/verification/r01/final-proof/unwind-smoke"
COMMANDS = b"dmesg -n 6\nrelibc sh9-a\necho SH9-a-after-launch\n"
BEGIN = "SH9-a native unwind begin"
END = "SH9-a-after-launch"
RECORDS = (
    "SH9-a: payload and LIFO cleanup x64 -> OK",
    "SH9-a: resume_unwind cleanup -> OK",
    "SH9-a: concurrent catches and panicking worker join -> OK",
    "SH9-a: real backtrace frames -> OK",
    "SH9-a native unwind green",
)


def load_board():
    spec = importlib.util.spec_from_file_location("sh3e", ROOT / "scripts/metal-sh3e.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/r03/sh9-a")
    parser.add_argument("--image", type=Path, default=None, help="Orange Pi image directory")
    parser.add_argument("--pxehost", type=Path, default=None, help="KUMO-capable pxehost binary")
    parser.add_argument("--proof", type=Path, default=PROOF, help="built SH9-a unwind ELF")
    parser.add_argument("--timeout", type=float, default=600.0, help="seconds to reach the proof marker")
    args = parser.parse_args()

    board = load_board()
    if args.image is not None:
        board.SOURCE_IMAGE = args.image.resolve()
    if args.pxehost is not None:
        board.PXEHOST = args.pxehost.resolve()
    image = board.SOURCE_IMAGE
    proof = args.proof.resolve()
    if shutil.which("tio") is None or not board.PXEHOST.is_file() or not image.is_dir():
        sys.exit("needs tio, pxehost and Orange Pi image")
    if not proof.is_file():
        sys.exit(f"missing {proof}; build SH9-a with scripts/native-unwind-smoke.py")

    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    tftp = work / "tftp"
    if tftp.exists():
        sys.exit(f"refusing to replace existing staged tree: {tftp}")
    stage = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    stage["stage_image"](image, tftp, proof, COMMANDS)

    log = work / "serial.log"
    manifest = {
        "proof": "SH9-a native Rust unwind",
        "board": "Orange Pi 5 Plus",
        "proof_elf_sha256": sha(proof),
        "staged_initrd_sha256": sha(tftp / "EFI/KUMO/initrd.img"),
        "pxehost": str(board.PXEHOST),
        "serial": board.SERIAL,
    }
    pxehost = None
    tio_pid = None
    master = None
    try:
        pxehost = board.start_pxehost(tftp, work / "pxehost.log")
        tio_pid, master = board.start_tio(log)
        board.wake_board(log)
        board.wait_for(END, log, args.timeout)

        deadline = time.monotonic() + 90.0
        captured = ""
        while time.monotonic() < deadline:
            captured = board.capture_text(log)
            end_at = captured.rfind(END)
            if end_at >= 0 and "INDRA login:" in captured[end_at:]:
                break
            time.sleep(0.2)
        else:
            raise RuntimeError("no INDRA login prompt after the SH9-a command returned")

        transcript = board.log_whole(log)
        board.validate(transcript, RECORDS, BEGIN, END, login_text=captured)
        start_at = transcript.index(BEGIN)
        end_at = transcript.index(END, start_at)
        window = transcript[start_at:end_at]
        if "TOWER" in window or "FAILED" in window:
            raise RuntimeError("fault or failure record inside the SH9-a proof window")
        positions = [window.index(record) for record in RECORDS]
        if positions != sorted(positions):
            raise RuntimeError("SH9-a proof records were out of order")
        manifest.update(passed=True, proof_window="clean", login_returned=True)
        print("SH9-a Orange Pi 5 Plus metal green", flush=True)
    except BaseException as error:
        manifest.update(passed=False, error=str(error))
        raise
    finally:
        if pxehost is not None and tio_pid is not None and master is not None:
            board.stop_all(pxehost, tio_pid, master)
        if log.is_file():
            manifest["serial_log_sha256"] = sha(log)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
