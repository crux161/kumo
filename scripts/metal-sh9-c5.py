#!/usr/bin/env python3
#J944_r10_001
"""Prove the pinned libc++ filesystem write smoke on Orange Pi 5 Plus over PXE."""
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
LABEL = "SH9-c5 libc++ filesystem writes"
BEGIN = LABEL + " begin"
END = "sh9-c5-after-launch"
RECORDS = (
    "SH9-c5: directories created, existing and missing parents -> OK",
    "SH9-c5: ofstream write, append, trunc and resize_file -> OK",
    "SH9-c5: copy_file, refusal, overwrite_existing and equivalent -> OK",
    "SH9-c5: rename and the listing after it -> OK",
    "SH9-c5: remove, non-empty refusal and remove_all -> OK",
    LABEL + " green",
)
COMMANDS = b"dmesg -n 6\nrelibc sh9-c5\necho sh9-c5-after-launch\n"


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
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/r10/sh9-c5-metal")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/orange-pi-5-plus")
    parser.add_argument("--pxehost", type=Path, default=ROOT / "build/tools/pxehost-kumo")
    parser.add_argument("--proof", type=Path, default=ROOT / "build/verification/r10/sh9-c5/cxx-fs-write-smoke")
    parser.add_argument("--timeout", type=float, default=900)
    args = parser.parse_args()
    board = load_board()
    image, pxehost_bin, proof = args.image.resolve(), args.pxehost.resolve(), args.proof.resolve()
    proof_json = proof.with_name("proof.json")
    if shutil.which("tio") is None or not pxehost_bin.is_file() or not image.is_dir():
        sys.exit("needs tio, pxehost and Orange Pi image")
    if not proof.is_file() or not proof_json.is_file():
        sys.exit(f"missing QEMU-proven SH9-c5 artifact or proof.json beside {proof}")
    proof_record = json.loads(proof_json.read_text())
    if proof_record.get("boots", {}).get("qemu") != "PASS" or proof_record.get("elf_sha256") != sha(proof):
        sys.exit("SH9-c5 ELF lacks matching QEMU proof")

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
        "proof": "SH9-c5 libc++ filesystem writes (RAM FAT; nothing persistent)",
        "board": "Orange Pi 5 Plus",
        "proof_elf_sha256": sha(proof),
        "qemu_proof_json_sha256": sha(proof_json),
        "staged_initrd_sha256": sha(tftp / "EFI/KUMO/initrd.img"),
        "pxehost": str(pxehost_bin),
        "serial": board.SERIAL,
    }
    pxehost = None
    tio_pid = None
    master = None
    try:
        pxehost = board.start_pxehost(tftp, work / "pxehost.log")
        tio_pid, master = board.start_tio(log)
        try:
            board.wake_board(log)
        except RuntimeError:
            # The firmware's timed UEFI menu can auto-start PXE while the shared
            # front-page steering helper is still reading redraws. If the serial
            # transcript proves that PXE is already active, keep hosting its tree.
            capture = board.capture_text(log)
            if board.classify(capture) == "firmware-net":
                manifest["wake_fallback"] = "UEFI had already started PXE"
            elif "NBP file downloaded successfully" in capture or "NIJIGUMO Boot Loader" in capture:
                # The firmware's own boot order can PXE-load Nijigumo from this pxehost before
                # the steering helper reads a known state; then the boot is already ours, so keep
                # hosting it rather than tear the server down under a loading kernel. — TEAL
                manifest["wake_fallback"] = "Nijigumo was already loading from this pxehost"
            elif "PCIe:" in capture or "BdsWait" in capture:
                # A board just power-cycled is still in firmware bring-up: its boot order
                # reaches PXE by itself, so host and wait out the proof timeout. — TEAL
                manifest["wake_fallback"] = "firmware still booting; its boot order reaches PXE"
            else:
                raise
        board.wait_for(END, log, args.timeout)
        deadline = time.monotonic() + 90
        captured = ""
        while time.monotonic() < deadline:
            captured = board.capture_text(log)
            end_at = captured.rfind(END)
            if end_at >= 0 and "INDRA login:" in captured[end_at:]:
                break
            time.sleep(0.2)
        else:
            raise RuntimeError("no INDRA login after the SH9-c5 suite returned")
        transcript = board.log_whole(log)
        board.validate(transcript, RECORDS, BEGIN, END, login_text=captured)
        start_at = transcript.index(BEGIN)
        end_at = transcript.index(END, start_at)
        window = transcript[start_at:end_at]
        if "TOWER" in window or "FAILED" in window:
            raise RuntimeError("fault or failure record inside the SH9-c5 proof window")
        positions = [window.index(BEGIN)] + [window.index(record) for record in RECORDS]
        if positions != sorted(positions):
            raise RuntimeError("SH9-c5 records are out of order")
        manifest.update(passed=True, proof_window="clean", login_returned=True)
        print("SH9-c5 Orange Pi 5 Plus metal green", flush=True)
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
