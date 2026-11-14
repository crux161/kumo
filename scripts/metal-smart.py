#!/usr/bin/env python3
#j895
"""Read-only NVMe SMART temperature proof, interleaved with real volume reads over serial.
Build a level-6 opi5 image. Uses private PXE staging; no dmesg or disk writes. — KESTREL
"""
import argparse
import hashlib
import importlib.util
import json
import re
import subprocess
import time
from pathlib import Path
ROOT=Path(__file__).resolve().parent.parent
BOOT=(b"echo SMART begins\nsmart\nmount /dev/disk3s1 /media/SMART\n"
      b"ls /media/SMART\nsmart\nls /media/SMART\nsmart\nthermal\necho SMART ends\n")


def load_thermal():
    spec=importlib.util.spec_from_file_location("thermal",ROOT/"scripts/metal-thermal.py")
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module);return module


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work",type=Path,required=True)
    parser.add_argument("--booting",action="store_true")
    parser.add_argument("--timeout",type=float,default=300)
    parser.add_argument("--pxehost",type=Path,default=Path("/Users/crux/git/kumo/build/tools/pxehost-kumo"))
    args=parser.parse_args()
    assert not subprocess.run(["lsof","/dev/tty.usbserial-B0001AH9"],capture_output=True).stdout,"serial port is occupied"
    thermal=load_thermal();board=thermal.load_board();board.PXEHOST=args.pxehost
    work=args.work.resolve();work.mkdir(parents=True,exist_ok=True)
    tftp=work/"tftp";board.stage_tree(tftp);board.stage_boot(tftp,BOOT)
    thermal.restage_zstd(tftp/"EFI/KUMO")
    log=work/"boot.log"
    manifest={"initrd_sha256":hashlib.sha256((tftp/"EFI/KUMO/initrd.img").read_bytes()).hexdigest()}
    pxe=board.start_pxehost(tftp,work/"pxehost.log")
    try:pid,master=board.start_tio(log)
    except BaseException:pxe.terminate();pxe.wait(timeout=5);raise
    try:
        if not args.booting:
            try:board.wake_board(log)
            except RuntimeError:
                if not any(m in board.capture_text(log) for m in ("BdsWait(","NimbusSplash:")):raise
        board.wait_for("SMART ends",log,args.timeout)
        deadline=time.monotonic()+60
        while board.capture_text(log).rfind("INDRA login:")<board.capture_text(log).rfind("SMART ends"):
            if time.monotonic()>deadline:raise RuntimeError("no login after SMART proof")
            time.sleep(.25)
        text=board.log_whole(log)
        assert text.count("SMART begins")==1 and text.count("SMART ends")==1
        window=text[text.index("SMART begins"):text.index("SMART ends")]
        replies=re.findall(r"smart: nvme composite ([^\r\n]+)",window)
        assert len(replies)==3,replies
        assert all("fresh=true" in r and "composite unknown" not in r for r in replies),replies
        commands=[tuple(map(int,m)) for m in re.findall(r"NVME SMART session=\d+ cid=(\d+) status=0x0 sampled_ns=(\d+) composite_k=(\d+) critical_warning=0x[0-9a-f]+ elapsed=\d+us io_entries=(\d+)",window)]
        assert len(commands)==3,commands
        assert all(200<=k<=450 for _,_,k,_ in commands),commands
        assert commands[0][1]<commands[1][1]<commands[2][1],commands
        assert commands[1][3]>commands[0][3] and commands[2][3]>=commands[1][3],commands
        assert "/dev/disk3s1 mounted on /media/SMART" in window
        failures=re.findall(r"(?:^|\] )((?:mount|ls): (?!proc koid=)[^\r\n]*)",window,re.MULTILINE)
        assert not failures,failures
        assert not any(marker in window for marker in ("ownership unproven","invalid/fatal controller","write refused")),"storage error in proof window"
        assert re.search(r"fan 20% [1-9]\d* rpm",window),"fan curve/tach unavailable"
        assert "NV9 WRITE" not in text and "NV8 WRITE" not in text,"unexpected disk write"
        launches={name:int(value,16) for name,value in re.findall(r"(gpu-client|drv-nvme|drv-thermal): proc koid=0x([0-9a-f]+)",text)}
        deaths=[]
        for m in re.finditer(r"TOWER \(reversed\): MICROSERVICE CONTAINED",text):
            chunk=text[m.end():m.end()+700]
            victim=int(re.search(r"victim koid=0x([0-9a-f]+)",chunk)[1],16)
            code=int(re.search(r"cause=ABEND  exit=0x([0-9a-f]+)",chunk)[1],16)
            assert victim==launches.get("gpu-client") and code==1,(victim,code)
            deaths.append("known gpu-client ABEND 1")
        manifest.update(passed=True,replies=replies,commands=commands,service_deaths=deaths)
        print(f"SMART metal green: {replies}; io_entries={[r[3] for r in commands]}",flush=True)
    except BaseException as e:
        manifest.update(passed=False,error=str(e));raise
    finally:
        board.stop_all(pxe,pid,master);(work/"manifest.json").write_text(json.dumps(manifest,indent=2)+"\n")


if __name__=="__main__":main()
