#!/usr/bin/env python3
#j640
"""Prove AArch64 thread-pointer preservation in the full scheduled UEFI image.

Only a disposable ESP copy's autoexec changes. Two residents share one address space,
install distinct TLS words, block on futexes, then observe timer-only peer progress.
The ordinary relibc C smoke also exercises its real ELF TLS and errno storage.
"""

import argparse
from pathlib import Path
import re
import shutil
import struct
import subprocess
import tempfile
import time


def set_autoexec(path):
    original = path.read_bytes()
    assert original[:8] == b"KUMORD01" and struct.unpack_from("<I", original, 8)[0] == 1
    count = struct.unpack_from("<I", original, 12)[0]
    out = bytearray(original[:16 + 80 * count])
    found = False
    for index in range(count):
        entry = 16 + index * 80
        name = original[entry:entry + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", original, entry + 64)
        data = original[offset:offset + size]
        if name == b"etc/autoexec":
            data = b"dmesg -n 6\nthreads\nrelibc i-a2\necho thread-tls-smoke-after-launch\n"
            found = True
        struct.pack_into("<QQ", out, entry + 64, len(out), len(data))
        out.extend(data)
    assert found, "image has no autoexec"
    path.write_bytes(out)


def main():
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, default=root / "build/images/qemu-virt-aarch64")
    parser.add_argument("--timeout", type=float, default=90)
    parser.add_argument("--expect-contamination", action="store_true", help="negative control with the unfixed kernel")
    args = parser.parse_args()
    work = Path(tempfile.mkdtemp(prefix="kumo-thread-tls-"))
    print(f"Thread TLS smoke artifacts: {work}", flush=True)
    shutil.copytree(args.image, work / "esp")
    set_autoexec(work / "esp/EFI/KUMO/initrd.img")
    # Use QEMU's live discovery data, as graphics-smoke --with-dtb does. The loader
    # currently searches this fixed path; it exists only in the disposable copy. — KESTREL
    dtb = work / "esp/EFI/KUMO/dtb/qcom/sc8280xp-lenovo-thinkpad-x13s.dtb"
    dtb.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([
        "qemu-system-aarch64", "-machine", f"virt,gic-version=3,dumpdtb={dtb}",
        "-cpu", "cortex-a72", "-m", "512", "-display", "none", "-device", "ramfb", "-net", "none",
    ], check=True, capture_output=True, timeout=15)
    firmware = Path("/opt/homebrew/share/qemu")
    shutil.copyfile(firmware / "edk2-arm-vars.fd", work / "vars.fd")
    serial = work / "serial.log"
    with (work / "qemu.log").open("wb") as log:
        process = subprocess.Popen([
            "qemu-system-aarch64", "-machine", "virt,gic-version=3", "-cpu", "cortex-a72",
            "-m", "512", "-display", "none", "-device", "ramfb", "-monitor", "none",
            "-serial", f"file:{serial}",
            "-drive", f"if=pflash,format=raw,file={firmware / 'edk2-aarch64-code.fd'},readonly=on",
            "-drive", f"if=pflash,format=raw,file={work / 'vars.fd'}",
            "-drive", f"file=fat:rw:{work / 'esp'},format=raw,if=virtio", "-net", "none",
        ], stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                assert process.poll() is None, f"QEMU exited; inspect {work}"
                transcript = serial.read_bytes() if serial.exists() else b""
                if args.expect_contamination:
                    if b" -> FAIL: thread pointer contamination" in transcript:
                        break
                elif all(marker in transcript for marker in [
                    b"deferred work ran in deadline order, none early",
                    b"thread-tls-smoke-after-launch", b"INDRA login:",
                ]):
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError(f"thread TLS smoke timed out; inspect {serial}")
            tls = re.findall(rb"threads: TLS faults fresh=(\d+) blocking=(\d+) preemption=(\d+) -> (OK|FAIL)", transcript)
            assert len(tls) == 1, f"missing or duplicated TLS report; inspect {serial}"
            fresh, blocked, preempted, outcome = tls[0]
            print(f"TLS faults fresh={fresh.decode()} blocking={blocked.decode()} preemption={preempted.decode()} {outcome.decode()}")
            if args.expect_contamination:
                assert outcome == b"FAIL" and (int(blocked) or int(preempted)), f"negative control did not detect context contamination; inspect {serial}"
                print("Thread TLS negative control green: unfixed kernel contamination detected")
                return
            assert (fresh, blocked, preempted, outcome) == (b"0", b"0", b"0", b"OK"), f"TLS contamination; inspect {serial}"
            for marker in [
                b"two threads, one address space, futex handoff",
                b"equal-priority compute peers made even progress",
                b"FIFO queue and synchronous completion",
                b"startup+printf+argv+tls+calloc+file+write+tty+winsize+stdin -> OK",
                b"sora: VmoCreate SIMD preserved",
            ]:
                assert marker in transcript, f"missing {marker!r}; inspect {serial}"
            before, proof = transcript.split(b"threads: two threads, one address space", 1)
            # The standing boot tries the board-specific serial service twice on QEMU;
            # its absent 0x11000000 UART gives a contained external abort before this proof.
            # Match only those baseline reports; any fault during the proof still fails. — KESTREL
            early_faults = before.split(b"TOWER (reversed): MICROSERVICE CONTAINED")[1:]
            assert len(early_faults) in (0, 2), f"unexpected early fault count; inspect {serial}"
            for fault in early_faults:
                assert b"cause=FAULT" in fault and b"FAR=0x0000000011000000" in fault, f"unexpected early fault; inspect {serial}"
                assert b"synchronous external abort on read" in fault, f"unexpected early fault; inspect {serial}"
            assert before.count(b"TOWER") == len(early_faults), f"unexpected early kernel fault; inspect {serial}"
            assert b" -> FAIL" not in proof and b"TOWER" not in proof, f"unexpected proof failure; inspect {serial}"
            print(f"Pre-proof baseline UART external aborts: {len(early_faults)}; no fault during TLS proof")
            print("Thread TLS UEFI smoke green: fresh zero, futex and timer preservation, real relibc TLS, queues, SIMD, login")
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


if __name__ == "__main__":
    main()
