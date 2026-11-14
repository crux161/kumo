#!/usr/bin/env python3
#j886
"""USB enumeration on the Orange Pi 5 Plus, one PXE boot (drv-xhci bus-resets every connected
root port, including ones the firmware left enabled; 2026-11-12).

Boots build/images/orange-pi-5-plus (default console level) with an autoexec that raises the console
level to info and lists /media, then
checks the xHCI story in the log:
  - every connected root port got a bus reset from drv-xhci ("bus reset port=");
  - no Address Device failed with a transaction error ("addr dev cc=0x4");
  - something behind the hub enumerated (a "media:" line or a /media entry).
The keyboard itself needs a human: after the run, type at the board's console.

Prerequisites (opi5 serial-loop recipe): serial adapter at /dev/tty.usbserial-B0001AH9, no
other tio or pxehost running, build/tools/pxehost-kumo present, image built with
`cargo xtask image --arch aarch64 --hardware orange-pi-5-plus`.
"""
import argparse
import importlib.util
import json
import re
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# Never a plain `dmesg` here: during autoexec its output re-enters the log it is reading, so it
# never ends and starves every driver (2026-11-12).
# `dmesg -n info` raises the console level at run time, after the kernel's early boot (a
# KUMO_CONSOLE_LOGLEVEL=6 image panics before Sora on j884), so drv-xhci's lines reach serial.
BOOT = b"dmesg -n info\necho USB begins\nls /media\necho USB ends\n"


def load_board():
    spec = importlib.util.spec_from_file_location("sh3e", ROOT / "scripts/metal-sh3e.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/usb-reset/metal")
    parser.add_argument("--timeout", type=float, default=300.0)
    parser.add_argument("--power-cycle", action="store_true",
                        help="do not try to wake the board; a human power-cycles it after the start")
    args = parser.parse_args()
    board = load_board()
    work = args.work
    work.mkdir(parents=True, exist_ok=True)
    tftp = work / "tftp"
    board.stage_tree(tftp)
    board.stage_boot(tftp, BOOT)
    pxehost = board.start_pxehost(tftp, work / "pxehost.log")
    tio_pid, master = board.start_tio(work / "boot.log")
    report = {}
    try:
        if args.power_cycle:
            print("waiting for a power cycle (PXE and serial are up)", flush=True)
        else:
            board.wake_board(work / "boot.log")
        board.wait_for("USB ends", work / "boot.log", args.timeout)
        time.sleep(2)
        text = board.log_whole(work / "boot.log")
        start = text.rfind("NIJIGUMO Boot Loader")
        text = text[start:] if start >= 0 else text
        resets = re.findall(r"drv-xhci: bus reset port=(0x[0-9a-f]+) was enabled=(0x[0-9a-f]+)", text)
        cc4 = len(re.findall(r"addr dev cc=0x4", text))
        media = re.findall(r"^.*media: .*$", text, re.M)
        report = {"bus_resets": resets, "addr_dev_cc4": cc4, "media_lines": media}
        for line in text.splitlines():
            if "drv-xhci" in line and any(k in line for k in ("bus reset", "dev port", "addr dev", "slot enabled", "hub", "hid", "storage", "no enabled")):
                print(line.strip())
        for line in media:
            print(line.strip())
        print(json.dumps({k: (v if k != "media_lines" else len(v)) for k, v in report.items()}))
        report["passed"] = bool(resets) and cc4 == 0
    finally:
        board.stop_all(pxehost, tio_pid, master)
        (work / "manifest.json").write_text(json.dumps(report, indent=2) + "\n")
    print("USB metal:", "PASS" if report.get("passed") else "FAIL", flush=True)
    sys.exit(0 if report.get("passed") else 1)


if __name__ == "__main__":
    main()
