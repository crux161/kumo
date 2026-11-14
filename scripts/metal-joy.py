#!/usr/bin/env python3
#j896
"""Capture the known USB arcade stick's descriptor and changed raw reports on opi5.

Build a level-6 image first. One private PXE/serial owner; no dmesg or volume writes.
When ready, move the stick, press/release buttons, then leave neutral. Creating
<work>/finish ends the capture early; otherwise --seconds bounds the input window.
"""
import argparse
import hashlib
import importlib.util
import json
import re
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BOOT = (b"echo JOY boot begins\nthermal\nsmart\necho JOY boot ready\n"
        b"ruststd spawnspin\nthermal\nsmart\necho JOY service alive\n")


def analyze(text, work, manifest):
    devices = re.findall(r"JOY device 1a34:0203 slot=(\d+) if=(\d+) ep=0x([0-9a-f]+) mps=(\d+) interval=(\d+) report_bytes=(\d+)", text)
    assert len(devices) == 1, devices
    slot, interface, endpoint, mps, interval, length = devices[0]
    descriptor = bytearray()
    for seen_slot, offset, raw in re.findall(r"JOY descriptor slot=(\d+) offset=(\d+) bytes=([0-9A-F ]+)", text):
        assert seen_slot == slot and int(offset) == len(descriptor)
        descriptor.extend(bytes.fromhex(raw))
    assert len(descriptor) == int(length), (len(descriptor), length)
    (work / "report-descriptor.bin").write_bytes(descriptor)
    reports = []
    for seen_slot, seen_if, sequence, count, raw in re.findall(r"JOY raw slot=(\d+) if=(\d+) seq=(\d+) len=(\d+) bytes=([0-9A-F ]+)", text):
        data = bytes.fromhex(raw)
        assert seen_slot == slot and seen_if == interface
        assert int(sequence) == len(reports) + 1
        assert len(data) == int(count) and 0 < len(data) <= min(64, int(mps))
        reports.append(data.hex(" "))
    manifest.update(device="1a34:0203", slot=int(slot), interface=int(interface),
                    endpoint=int(endpoint, 16), max_packet_size=int(mps), interval=int(interval),
                    descriptor_hex=descriptor.hex(" "), reports=reports)
    prefix = re.search(r"JOY capture control_prefix=Some\((\d+)\)", text)
    prefix = int(prefix[1]) if prefix else None
    states = [bytes.fromhex(report)[:prefix].hex(" ") for report in reports]
    manifest.update(control_prefix_bytes=prefix, control_states=states)
    assert len(set(states)) >= 3, "need at least three distinct physical control states"
    assert len(set(states)) < len(states), "need a return to a previously observed control state (release/neutral)"
    limited = "JOY capture limit reached" in text
    if limited:
        assert len(reports) == 128, "limit marker without the complete bounded capture"
    manifest.update(capture_limit_reached=limited)
    health = re.findall(r"smart: nvme composite [^\r\n]+", text)
    assert len(health) == 2 and all("fresh=true" in report for report in health), health
    assert re.search(r"thermal: package [^\r\n]+ fan 20% [1-9]\d* rpm", text)
    assert "JOY report descriptor failed" not in text and "JOY interface refused" not in text
    launches = {name: int(value, 16) for name, value in re.findall(r"(gpu-client|drv-xhci|drv-thermal|drv-nvme): proc koid=0x([0-9a-f]+)", text)}
    deaths = []
    for match in re.finditer(re.escape("TOWER (reversed): MICROSERVICE CONTAINED"), text):
        chunk = text[match.end():match.end() + 700]
        victim = int(re.search(r"victim koid=0x([0-9a-f]+)", chunk)[1], 16)
        code = int(re.search(r"cause=ABEND  exit=0x([0-9a-f]+)", chunk)[1], 16)
        assert victim == launches.get("gpu-client") and code == 1, (victim, code)
        deaths.append("known gpu-client ABEND 1")
    manifest.update(passed=True, service_deaths=deaths)
    print(f"JOY metal green: {len(descriptor)} descriptor bytes, {len(reports)} changed reports, {len(set(states))} distinct control states including return", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--seconds", type=float, default=180)
    parser.add_argument("--booting", action="store_true")
    parser.add_argument("--analyze", action="store_true", help="validate an existing boot.log without opening serial or rebooting; write analysis.json")
    parser.add_argument("--pxehost", type=Path, default=Path("/Users/crux/git/kumo/build/tools/pxehost-kumo"))
    args = parser.parse_args()
    assert 1 <= args.seconds <= 600
    spec = importlib.util.spec_from_file_location("thermal", ROOT / "scripts/metal-thermal.py")
    thermal = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(thermal)
    board = thermal.load_board()
    board.PXEHOST = args.pxehost
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    if args.analyze:
        manifest = json.loads((work / "manifest.json").read_text())
        manifest.pop("error", None)
        analyze(board.log_whole(work / "boot.log"), work, manifest)
        (work / "analysis.json").write_text(json.dumps(manifest, indent=2) + "\n")
        return
    assert not subprocess.run(["lsof", "/dev/tty.usbserial-B0001AH9"], capture_output=True).stdout, "serial occupied"
    for name in ["ready", "finish"]:
        (work / name).unlink(missing_ok=True)
    tftp = work / "tftp"
    board.stage_tree(tftp)
    board.stage_boot(tftp, BOOT)
    thermal.restage_zstd(tftp / "EFI/KUMO")
    manifest = {"initrd_sha256": hashlib.sha256((tftp / "EFI/KUMO/initrd.img").read_bytes()).hexdigest()}
    log = work / "boot.log"
    pxe = board.start_pxehost(tftp, work / "pxehost.log")
    try:
        pid, master = board.start_tio(log)
    except BaseException:
        pxe.terminate()
        pxe.wait(timeout=5)
        raise
    try:
        if not args.booting:
            try:
                board.wake_board(log)
            except RuntimeError:
                if not any(m in board.capture_text(log) for m in ("BdsWait(", "NimbusSplash:")):
                    raise
        board.wait_for("usb: arcade raw capture ready", log, 300)
        board.wait_for("JOY boot ready", log, 120)
        (work / "ready").write_text("Move stick, press/release buttons, leave neutral; create finish when done.\n")
        print("JOY READY: move stick and press/release buttons; capture is running", flush=True)
        deadline = time.monotonic() + args.seconds
        while time.monotonic() < deadline and not (work / "finish").exists():
            time.sleep(.25)
        board.wait_for("JOY service alive", log, 90)
        text = board.log_whole(log)
        analyze(text, work, manifest)
    except BaseException as error:
        manifest.update(passed=False, error=str(error))
        raise
    finally:
        board.stop_all(pxe, pid, master)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
