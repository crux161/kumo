#!/usr/bin/env python3
#j893
"""Thermal metal: drv-thermal on the Orange Pi 5 Plus, one PXE boot.

    thermal            the seven TS-ADC sensors as drv-thermal last read them
    thermal            again, to verify another status request succeeds

Reuses scripts/metal-sh3e.py's board machinery (import only), like the other metal suites.
Prerequisites, per the opi5 serial-loop recipe: serial adapter at /dev/tty.usbserial-B0001AH9,
no other tio or pxehost on the port, a pxehost binary (--pxehost, default the board module's
build/tools/pxehost-kumo), and the image built with KUMO_CONSOLE_LOGLEVEL=6 so the driver's
INFO diagnostics reach the console. Never run dmesg in this level-6 autoexec: its output
can feed the console's replay loop.

With --cpu-load, the existing `ruststd spawnspin` runs for 30 seconds between requests.
"""

import argparse
import hashlib
import importlib.util
import json
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

BOOT = (
    b"echo THERMAL begins\n"
    b"thermal\n"
    b"thermal\n"
    b"echo THERMAL ends\n"
)
EXPECTED_ONCE = (
    "[  OK  ] drv-thermal: TS-ADC online, package ",
)
THERMAL_LINE = re.compile(
    r"thermal: package (-?\d+)C a76-01 (-?\d+)C a76-23 (-?\d+)C a55 (-?\d+)C "
    r"center (-?\d+)C gpu (-?\d+)C npu (-?\d+)C max (-?\d+)C"
)


def load_board():
    spec = importlib.util.spec_from_file_location("sh3e", ROOT / "scripts/metal-sh3e.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def restage_zstd(kumo):
    """Since PROPOSAL/025 Z1 Nijigumo boots `initrd.img.zst` when the manifest names it, so a
    staged raw initrd alone is never seen. Recompress it and rewrite the manifest keys the way
    `xtask image` writes them (j891 fixes the same in metal-sh3e on its own branch)."""
    conf = kumo / "nijigumo.conf"
    text = conf.read_text()
    if "initrd_zstd" not in text:
        return
    raw = (kumo / "initrd.img").read_bytes()
    zst = kumo / "initrd.img.zst"
    subprocess.run(
        ["zstd", "-19", "-q", "-f", "-o", str(zst), str(kumo / "initrd.img")], check=True
    )
    digest = hashlib.sha256(zst.read_bytes()).hexdigest()
    text = re.sub(r"(?m)^initrd_zstd_sha256 = .*$", f"initrd_zstd_sha256 = {digest}", text)
    text = re.sub(r"(?m)^initrd_size = .*$", f"initrd_size = {len(raw)}", text)
    conf.write_text(text)


def readings(window):
    """Every `thermal:` line in the window, as eight integers (seven channels, max)."""
    return [tuple(int(v) for v in m.groups()) for m in THERMAL_LINE.finditer(window)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--work", type=Path, default=ROOT / "build/verification/thermal/t0-metal"
    )
    parser.add_argument("--pxehost", type=Path, default=None)
    parser.add_argument("--timeout", type=float, default=240.0)
    parser.add_argument(
        "--cpu-load", action="store_true",
        help="run the existing bounded 30-second CPU spinner between thermal requests",
    )
    parser.add_argument(
        "--booting", action="store_true",
        help="board is already booting after power-on; wait without sending a reset",
    )
    args = parser.parse_args()

    board = load_board()
    if args.pxehost is not None:
        board.PXEHOST = args.pxehost
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
    boot = (
        BOOT.replace(b"thermal\nthermal\n", b"thermal\nruststd spawnspin\nthermal\n")
        if args.cpu_load else BOOT
    )
    board.stage_boot(tftp, boot)
    restage_zstd(tftp / "EFI/KUMO")

    manifest = {"pxehost": str(board.PXEHOST)}
    pxehost = board.start_pxehost(tftp, work / "pxehost.log")
    tio_pid, master = board.start_tio(work / "boot.log")
    try:
        if not args.booting:
            board.wake_board(work / "boot.log")
        board.wait_for("THERMAL ends", work / "boot.log", args.timeout)
        deadline = time.monotonic() + 60
        while True:
            raw = board.raw_text_since(work / "boot.log", 0)
            if "THERMAL ends" in raw and "INDRA login:" in raw[raw.rindex("THERMAL ends"):]:
                break
            if time.monotonic() > deadline:
                raise RuntimeError("no login prompt after THERMAL ends")
            time.sleep(0.2)
        text = board.log_whole(work / "boot.log")
        board.validate(text, EXPECTED_ONCE, "THERMAL begins", "THERMAL ends")
        window = text[text.index("THERMAL begins"):text.index("THERMAL ends")]
        # Two `thermal` word answers (prefix exactly `thermal: package`).
        found = readings(window)
        if len(found) < 2:
            raise RuntimeError(f"expected two thermal readings in the window, found {found}")
        for reading in found:
            channels, peak = reading[:7], reading[7]
            if not all(20 <= c <= 95 for c in channels) or peak != max(channels):
                raise RuntimeError(f"implausible reading {reading}")
            if max(channels) - min(channels) > 15:
                raise RuntimeError(f"channels disagree by more than 15 C: {reading}")
        # A pre-existing gpu-client ABEND can TOWER on this board (JOURNAL/888); only one that
        # names drv-thermal fails this suite. Every TOWER is counted in the manifest.
        towers = text.split("TOWER")[1:]
        manifest["towers"] = len(towers)
        if any("drv-thermal" in tower[:2000] for tower in towers):
            raise RuntimeError("a TOWER names drv-thermal")
        manifest["readings"] = found
        manifest["passed"] = True
        print(f"thermal readings: {found}", flush=True)
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        board.stop_all(pxehost, tio_pid, master)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("thermal metal T0 green", flush=True)


if __name__ == "__main__":
    main()
