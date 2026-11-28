#!/usr/bin/env python3
#J955_r21_001
#J956_r22_001 (committed here: r21 ran it but left it out of its commit)
"""Prove PROPOSAL/027 P2b (the child filesystem grant tree, `ruststd grants`) on Orange Pi 5 Plus over PXE.

The proof is the image's own Rust std smoke; `bin/relibc-smoke` is staged back unchanged. — TEAL
"""
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
LABEL = "P2b child grants"
BEGIN = LABEL + " begin"
END = "p2b-after-launch"
RECORDS = tuple(f"std: p2b {line}" for line in (
    "setup /P2B -> OK",
    "read-write tree: child and grandchild wrote their files, marker inherited -> OK",
    "read-only grant: reads, write refused EACCES, its child gets nothing -> OK",
    "no marker: the child cannot open anything -> OK",
    "3 granted children and the parent do I/O concurrently, no crossed replies -> OK",
    "files removed -> OK",
    LABEL + " green",
))
COMMANDS = b"dmesg -n 6\nruststd grants\necho p2b-after-launch\n"


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
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/r21/p2b-metal")
    parser.add_argument("--image", type=Path, default=ROOT / "build/images/orange-pi-5-plus")
    parser.add_argument("--pxehost", type=Path, default=ROOT / "build/tools/pxehost-kumo")
    parser.add_argument("--qemu-proof", type=Path, default=ROOT / "build/verification/r21/p2b/proof.json")
    parser.add_argument("--timeout", type=float, default=900)
    args = parser.parse_args()
    board = load_board()
    image, pxehost_bin, proof_json = args.image.resolve(), args.pxehost.resolve(), args.qemu_proof.resolve()
    if shutil.which("tio") is None or not pxehost_bin.is_file() or not image.is_dir():
        sys.exit("needs tio, pxehost and Orange Pi image")
    if not proof_json.is_file() or json.loads(proof_json.read_text()).get("boots", {}).get("qemu") != "PASS":
        sys.exit(f"missing a passing QEMU P2b proof at {proof_json}")
    grant = runpy.run_path(str(ROOT / "scripts/child-grant-smoke.py"))
    std_smoke = grant["initrd_entry"](image, b"bin/std-smoke")

    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    tftp = work / "tftp"
    if tftp.exists():
        sys.exit(f"refusing to replace existing staged tree: {tftp}")
    unchanged = work / "relibc-smoke"
    unchanged.write_bytes(grant["initrd_entry"](image, b"bin/relibc-smoke"))
    stage = runpy.run_path(str(ROOT / "scripts/cxx-smoke.py"))
    stage["stage_image"](image, tftp, unchanged, COMMANDS)
    board.SOURCE_IMAGE = image
    board.PXEHOST = pxehost_bin
    log = work / "serial.log"
    manifest = {
        "proof": "PROPOSAL/027 P2b: hereditary read-write tree, read-only grant that cannot widen, no-marker child, 3 concurrent granted children (RAM FAT; nothing persistent)",
        "board": "Orange Pi 5 Plus",
        "std_smoke_sha256": hashlib.sha256(std_smoke).hexdigest(),
        "std_smoke_matches_qemu": hashlib.sha256(std_smoke).hexdigest() == json.loads(proof_json.read_text()).get("std_smoke_sha256"),
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
            raise RuntimeError("no INDRA login after the P2b suite returned")
        transcript = board.log_whole(log)
        board.validate(transcript, RECORDS, BEGIN, END, login_text=captured)
        start_at = transcript.index(BEGIN)
        end_at = transcript.index(END, start_at)
        window = transcript[start_at:end_at]
        if "TOWER" in window or "FAILED" in window:
            raise RuntimeError("fault or failure record inside the P2b proof window")
        positions = [window.index(BEGIN)] + [window.index(record) for record in RECORDS]
        if positions != sorted(positions):
            raise RuntimeError("P2b records are out of order")
        if "std: p2b child " in window:
            raise RuntimeError("a grant-tree child reported a failure")
        busy = window.split(RECORDS[3], 1)[1].split(RECORDS[4], 1)[0]
        events = [line.split("fatfs: session ", 1)[1][:7] for line in busy.splitlines() if "fatfs: session " in line]
        manifest["busy_session_events"] = events
        if events[:3] != ["granted"] * 3:
            raise RuntimeError(f"the busy children did not overlap: {events}")
        manifest.update(passed=True, proof_window="clean", login_returned=True)
        print("P2b Orange Pi 5 Plus metal green", flush=True)
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
