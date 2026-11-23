#!/usr/bin/env python3
#J938_r04_001
"""Prove SH9-b1 C++ exception handling on Orange Pi 5 Plus over PXE."""

import argparse
import hashlib
import importlib.util
import json
import os
import runpy
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROOF = ROOT / "build/verification/r02/final-proof/cxx-eh-smoke"
EXPECTED_ELF_SHA256 = "33e971f89d90ab8369bf1ed2ebf03ab3942e7332e129d35c3403ac687357f96a"
BEGIN = "SH9-b1 native C++ EH begin"
END = "SH9-b1-after-launch"
COMMANDS = (
    b"dmesg -n 6\n"
    b"relibc sh9-b1\n"
    b"echo SH9-b1-positive-returned\n"
    b"relibc sh9-b1-noexcept\n"
    b"echo SH9-b1-noexcept-returned\n"
    b"relibc sh9-b1-uncaught\n"
    b"echo SH9-b1-uncaught-returned\n"
    b"echo SH9-b1-after-launch\n"
)
RECORDS = (
    "SH9-b1: typed catches, RTTI and LIFO cleanup x64 -> OK",
    "SH9-b1: standard exception message and rethrow -> OK",
    "SH9-b1: throwing static initializer retries exactly once -> OK",
    "SH9-b1: allocation failure throws bad_alloc -> OK",
    "SH9-b1: thread-local catches and exception_ptr x64 -> OK",
    "SH9-b1 native C++ EH green",
    "SH9-b1-positive-returned",
    "SH9-b1 noexcept begin",
    "SH9-b1 noexcept green",
    "SH9-b1-noexcept-returned",
    "SH9-b1 uncaught begin",
    "SH9-b1 uncaught green",
    "SH9-b1-uncaught-returned",
)


def load_board():
    spec = importlib.util.spec_from_file_location("sh3e", ROOT / "scripts/metal-sh3e.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def stop_services(pxehost, tio_pid, master):
    errors = []
    if pxehost is not None:
        try:
            pxehost.terminate()
        except ProcessLookupError:
            pass
        try:
            pxehost.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                pxehost.kill()
            except ProcessLookupError:
                pass
            pxehost.wait(timeout=5)
        except Exception as error:
            errors.append(str(error))
    if tio_pid is not None:
        try:
            os.kill(tio_pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(tio_pid, 0)
        except ChildProcessError:
            pass
        except Exception as error:
            errors.append(str(error))
    if master is not None:
        try:
            os.close(master)
        except OSError:
            pass
    if errors:
        raise RuntimeError("; ".join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/r04/sh9-b1")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/orange-pi-5-plus")
    parser.add_argument("--pxehost", type=Path, default=ROOT / "build/tools/pxehost-kumo")
    parser.add_argument("--proof", type=Path, default=PROOF)
    parser.add_argument("--timeout", type=float, default=900.0)
    args = parser.parse_args()

    board = load_board()
    proof = args.proof.resolve()
    proof_manifest = proof.with_name("proof.json")
    image = args.image.resolve()
    pxehost_bin = args.pxehost.resolve()
    if shutil.which("tio") is None or not pxehost_bin.is_file() or not image.is_dir():
        sys.exit("needs tio, pxehost and Orange Pi image")
    if not proof.is_file() or not proof_manifest.is_file():
        sys.exit(f"missing the archived SH9-b1 proof or proof.json beside {proof}")

    proof_sha = sha(proof)
    proof_record = json.loads(proof_manifest.read_text())
    expected_boots = {"positive": "PASS", "noexcept": "PASS", "uncaught": "PASS"}
    if proof_sha != EXPECTED_ELF_SHA256 or proof_record.get("elf_sha256") != proof_sha:
        sys.exit("SH9-b1 ELF does not match the QEMU-verified r02 proof")
    if proof_record.get("boots") != expected_boots:
        sys.exit("archived SH9-b1 proof lacks positive/noexcept/uncaught QEMU passes")

    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    tftp = work / "tftp"
    if tftp.exists():
        sys.exit(f"refusing to replace existing staged tree: {tftp}")
    stage = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    stage["stage_image"](image, tftp, proof, COMMANDS)

    board.SOURCE_IMAGE = image
    board.PXEHOST = pxehost_bin
    log = work / "serial.log"
    manifest = {
        "proof": "SH9-b1 native C++ EH",
        "board": "Orange Pi 5 Plus",
        "qemu_verified_proof_elf": str(proof),
        "proof_elf_sha256": proof_sha,
        "proof_json_sha256": sha(proof_manifest),
        "staged_initrd_sha256": sha(tftp / "EFI/KUMO/initrd.img"),
        "pxehost": str(pxehost_bin),
        "serial": board.SERIAL,
        "qemu_cases": proof_record["boots"],
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
            raise RuntimeError("no INDRA login prompt after the SH9-b1 suite returned")

        transcript = board.log_whole(log)
        board.validate(transcript, RECORDS, BEGIN, END, login_text=captured)
        start_at = transcript.index(BEGIN)
        end_at = transcript.index(END, start_at)
        window = transcript[start_at:end_at]
        if "TOWER" in window or "FAILED" in window:
            raise RuntimeError("fault or failure record inside the SH9-b1 proof window")
        positions = [window.index(BEGIN)] + [window.index(record) for record in RECORDS]
        if positions != sorted(positions):
            raise RuntimeError("SH9-b1 proof records were out of order")
        manifest.update(passed=True, proof_window="clean", login_returned=True)
        print("SH9-b1 Orange Pi 5 Plus metal green: positive, noexcept, uncaught", flush=True)
    except BaseException as error:
        manifest.update(passed=False, error=str(error))
        raise
    finally:
        try:
            stop_services(pxehost, tio_pid, master)
        except Exception as error:
            manifest["cleanup_error"] = str(error)
            manifest["passed"] = False
        if log.is_file():
            manifest["serial_log_sha256"] = sha(log)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
