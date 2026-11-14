#!/usr/bin/env python3
#j894
"""Fan/T1 metal proof over serial. Build a level-6 opi5 image first.

Modes: auto (normal curve and live RPM), cycle (bounded synthetic temperatures/faults
with real PWM and tach), panic (verify full duty before panic logging and ProcessExit).
Only proof images carry etc/thermal-test. No dmesg is used. — KESTREL
"""
import argparse
import hashlib
import importlib.util
import json
import re
import struct
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROOF = re.compile(r"thermal proof: sample=(\d+) duty=(\d+) rpm=(\d+) state=(.*?) pwm_period=(\d+) pwm_duty=(\d+) capture_h=(\d+) capture_l=(\d+)")
FAN = re.compile(r"fan (\d+)% (\d+) rpm (auto|sensor fault|clock fault|stall)")


def load_thermal():
    spec = importlib.util.spec_from_file_location("thermal", ROOT / "scripts/metal-thermal.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def add_marker(initrd, mode, board):
    axh = board.load_harness()
    raw = initrd.read_bytes()
    files = axh.unpack(raw)
    assert "etc/thermal-test" not in files
    files["etc/thermal-test"] = mode.encode() + b"\n"
    assert len(files) <= 48
    out = bytearray(raw[:12] + struct.pack("<I", len(files)) + bytes(80 * len(files)))
    for i, (name, contents) in enumerate(files.items()):
        at = 16 + 80 * i
        out[at:at + 64] = name.encode().ljust(64, b"\0")
        struct.pack_into("<QQ", out, at + 64, len(out), len(contents))
        out.extend(contents)
    assert axh.unpack(out) == files
    initrd.write_bytes(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("auto", "cycle", "panic", "exit"), default="auto")
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--pxehost", type=Path, default=Path("/Users/crux/git/kumo/build/tools/pxehost-kumo"))
    parser.add_argument("--timeout", type=float, default=300)
    parser.add_argument("--booting", action="store_true", help="wait for an already-started cold boot")
    args = parser.parse_args()
    thermal = load_thermal()
    board = thermal.load_board()
    board.PXEHOST = args.pxehost
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    tftp = work / "tftp"
    board.stage_tree(tftp)
    boot = b"echo FAN begins\nthermal\nruststd spawnspin\nthermal\necho FAN ends\n"
    board.stage_boot(tftp, boot)
    if args.mode != "auto":
        add_marker(tftp / "EFI/KUMO/initrd.img", args.mode, board)
    thermal.restage_zstd(tftp / "EFI/KUMO")
    log_path = work / "boot.log"
    manifest = {"mode": args.mode, "initrd_sha256": hashlib.sha256((tftp / "EFI/KUMO/initrd.img").read_bytes()).hexdigest()}
    pxe = board.start_pxehost(tftp, work / "pxehost.log")
    try:
        pid, master = board.start_tio(log_path)
    except BaseException:
        pxe.terminate()
        pxe.wait(timeout=5)
        raise
    try:
        if not args.booting:
            try:
                board.wake_board(log_path)
            except RuntimeError:
                if not any(marker in board.capture_text(log_path) for marker in ("BdsWait(", "NimbusSplash:")):
                    raise
        board.wait_for("FAN ends", log_path, args.timeout)
        deadline = time.monotonic() + 100
        while True:
            raw = board.capture_text(log_path)
            text = board.log_whole(log_path)
            login = raw.rfind("INDRA login:") > raw.rfind("FAN ends")
            enough = args.mode != "cycle" or "thermal proof: sample=60 " in text
            if login and enough:
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("missing login or complete bounded fan proof")
            time.sleep(0.25)
        assert "PWM12 primed 100%, pins 16/18 muxed, tach pull-up verified" in text
        assert "thermal: PWM12 25kHz, startup 100%; PWM13 tach capture" in text
        manifest["towers"] = text.count("TOWER")
        fan = [(int(a), int(b), c) for a,b,c in FAN.findall(text)]
        manifest["fan"] = fan
        requests = [(int(d),int(r)) for d,r,tail in re.findall(r"thermal: package[^\r\n]+ fan (\d+)% (\d+) rpm([^\r\n]*)",text) if not tail.strip()]
        manifest["status_requests"] = requests
        assert fan, "no fan state reported"
        # Attribute every TOWER by its process koid. The existing native gpu-client exit-1
        # (j893 notes/J888) is recorded; any other unexpected service death still fails.
        launches = {name: int(value,16) for name,value in re.findall(r"(gpu-client|drv-thermal): proc koid=0x([0-9a-f]+)", text)}
        deaths=[]
        for match in re.finditer(r"TOWER \(reversed\): MICROSERVICE CONTAINED", text):
            chunk=text[match.end():match.end()+700]
            victim=int(re.search(r"victim koid=0x([0-9a-f]+)",chunk)[1],16)
            code=int(re.search(r"cause=ABEND  exit=0x([0-9a-f]+)",chunk)[1],16)
            if victim == launches.get("gpu-client") and code == 1:
                deaths.append("gpu-client ABEND 1")
            elif victim == launches.get("drv-thermal") and args.mode == "panic" and code == 1:
                deaths.append("expected drv-thermal panic")
            elif victim == launches.get("drv-thermal") and args.mode == "exit" and code == 0:
                deaths.append("expected drv-thermal graceful exit")
            else:
                raise AssertionError(f"unexpected service death: koid={victim:x} exit={code}")
        assert len(deaths)==manifest["towers"]
        manifest["service_deaths"]=deaths
        if args.mode in ("panic", "exit"):
            assert any(d == 20 and r > 0 for d,r,_ in fan), "floor/tach absent before exit"
            assert "thermal: exit fail-safe 100%, PWM duty readback=960" in text
            if args.mode == "panic":
                at=text.index("PANIC: thermal fail-safe proof")
                assert "thermal: exit fail-safe 100%, PWM duty readback=960" in text[:at]
                assert deaths.count("expected drv-thermal panic")==1
            else:
                assert "PANIC: thermal fail-safe proof" not in text
        elif args.mode == "cycle":
            records = [(int(n),int(d),int(r),s,int(p),int(h),int(ch),int(cl)) for n,d,r,s,p,h,ch,cl in PROOF.findall(text)]
            manifest["proof"] = records
            assert records and records[-1][0] >= 60
            for _,d,_,_,p,h,_,_ in records:
                assert p == 960 and h == d * 960 // 1000, (d,p,h)
            speeds=[]
            for lo,hi,duty in [(2,4,1000),(7,9,600),(13,16,200)]:
                matching=[r for n,d,r,s,*_ in records if lo <= n <= hi and d == duty and s == "auto" and r > 0]
                assert matching, (lo,hi,duty,records)
                speeds.append(matching[-1])
            assert speeds[0] > speeds[1] > speeds[2] > 0, speeds
            assert any(17 <= n <= 20 and d == 1000 and s == "sensor fault" for n,d,r,s,*_ in records)
            assert any(d == 1000 and s == "stall" for n,d,r,s,*_ in records)
            assert any(30 <= n <= 33 and d == 1000 and s == "clock fault" for n,d,r,s,*_ in records)
            assert records[-1][3] == "auto" and records[-1][2] > 0
            manifest["rpm_full_mid_floor"] = speeds
        else:
            assert len(requests) >= 2 and requests[-1][0] == 20 and 100 <= requests[-1][1] < 2500, requests
            assert any(d == 20 and r > 0 and s == "auto" for d,r,s in fan), fan
            assert not any(s != "auto" for _,_,s in fan), fan
        manifest["passed"] = True
        print(f"fan {args.mode} metal green: status={requests}, deaths={deaths}", flush=True)
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        board.stop_all(pxe, pid, master)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    main()
