#!/usr/bin/env python3
#j822
"""AX0: two QEMU boots of an existing image, or parse a captured metal boot.

Append actorbench to the default autoexec in a private copy. The opt-in benchmark starts
only after login services settle. No credentials or board reset is part of this command.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import statistics
import struct
import subprocess
import tempfile
import time
import unittest


def require(ok, why):
    if not ok:
        raise RuntimeError(why)


def fields(line):
    return {key: int(value) for key, value in re.findall(r"([a-z][a-z0-9-]*)=(-?\d+)", line)}


def _parse(text):
    require(text.count("AX0 PASS") == 1 and "AX0 FAIL" not in text and "TOWER" not in text, "failed/incomplete actor proof")
    require(text.index("INDRA login:") < text.index("AX0 begins:"), "measurement preceded login")
    require(all(line.rstrip().endswith("PASS") for line in text.splitlines() if "AX0 trial=" in line), "failed individual trial")
    trials = [fields(line) for line in text.splitlines() if "AX0 trial=" in line]
    memory = [fields(line) for line in text.splitlines() if "AX0 memory trial=" in line]
    switches = [fields(line) for line in text.splitlines() if "AX0 switch trial=" in line]
    require(len(trials) == len(memory) == len(switches) == 5, "missing/duplicated trials")
    for group in (trials, memory, switches):
        require([row["trial"] for row in group] == list(range(1, 6)), "wrong trial order")
    for t, m, s in zip(trials, memory, switches):
        require(t["rounds"] == 1024 and t["bytes"] == 64 and min(t["rtt-total-ns"], t["spawn-a-ns"], t["spawn-b-ns"]) > 0, "invalid workload")
        require(m["frames-a"] > 0 and m["frames-b"] > 0, "no issued-frame measurement")
        require(s["simd-errors"] == 0 and s["root-count"] > 0 and s["root-total-ns"] > 0 and s["syscalls"] > 0 and s["switches"] > 0, "invalid switch measurement")
    census = {}
    for label in ("default-login", "netstack-client"):
        rows = [fields(l) for l in text.splitlines() if f"AX0 census {label} " in l]
        require(len(rows) == 1, "missing/duplicated census")
        c = census[label] = rows[0]
        require(c["capacity"] == 12 and c["live"] <= c["stored"] <= 12 and c["free"] == 12 - c["live"], "wrong resident accounting")
        require(c["blocked"] + c["ready"] + c["running"] == c["live"], "wrong resident states")
    require(census["netstack-client"]["live"] == census["default-login"]["live"] + 2, "netstack/client not both resident")
    require(all(m["clean"] == census["default-login"]["live"] for m in memory), "probe residents leaked")
    gate=[line for line in text.splitlines() if "AX0 capacity-gate " in line]
    free=census["netstack-client"]["free"]
    require(len(gate)==1 and fields(gate[0]).get("free")==free and gate[0].rstrip().endswith(" PASS" if free>=2 else " AX4-REQUIRED"), "missing/incorrect capacity gate")
    require("netprobe: hold PASS" in text, "census client did not finish")
    sched = [fields(l) for l in text.splitlines() if "AX0 scheduler " in l]
    require([s["runnable"] for s in sched] == [1, 2, 4, 8, 12] and all(s["decisions"] == 32768 and 0 < s["total-ns"] < (1 << 64) - 1 for s in sched), "missing/invalid scheduler series")
    clocks = {label: [fields(l) for l in text.splitlines() if f"AX0 {label} " in l] for label in ("kernel-clock-pair", "clock-pair")}
    require(all(len(rows) == 1 and rows[0]["samples"] == 1024 for rows in clocks.values()), "missing clock calibration")
    result = {"census": census, "trials": trials, "memory": memory, "switches": switches, "scheduler": sched, "clocks": clocks}
    result["summary"] = {
        "rtt_ns": [t["rtt-total-ns"] / t["rounds"] for t in trials],
        "spawn_ns": [t[k] for t in trials for k in ("spawn-a-ns", "spawn-b-ns")],
        "frames_per_idle_actor": [m[k] for m in memory for k in ("frames-a", "frames-b")],
        "root_call_ns": [s["root-total-ns"] / s["root-count"] for s in switches],
        "scheduler_decision_ns": {s["runnable"]: s["total-ns"] / s["decisions"] for s in sched},
        "two_spare_slots": census["netstack-client"]["free"] >= 2,
    }
    return result


def parse(text):
    try:
        return _parse(text)
    except (KeyError, ValueError) as error:
        raise RuntimeError("malformed measurement fields") from error


def stage(source, esp):
    shutil.copytree(source, esp)
    p = esp / "EFI/KUMO/initrd.img"
    data = p.read_bytes()
    require(data[:8] == b"KUMORD01", "bad initrd")
    count = struct.unpack_from("<I", data, 12)[0]
    out = bytearray(data[:16 + 80 * count])
    found = False
    for i in range(count):
        at = 16 + 80 * i
        name = data[at:at + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", data, at + 64)
        content = data[offset:offset + size]
        if name == b"etc/autoexec":
            content += b"actorbench\n"
            require(len(content) <= 256, "autoexec exceeds Sora read")
            found = True
        struct.pack_into("<QQ", out, at + 64, len(out), len(content))
        out.extend(content)
    require(found, "no autoexec")
    p.write_bytes(out)
    return hashlib.sha256(out).hexdigest()


def boot(source, work, firmware, timeout):
    work.mkdir()
    image_hash = stage(source, work / "esp")
    shutil.copyfile(firmware / "edk2-arm-vars.fd", work / "vars.fd")
    serial = work / "serial.log"
    command = ["qemu-system-aarch64", "-machine", "virt,gic-version=3", "-cpu", "cortex-a72", "-m", "2048", "-display", "none", "-monitor", "none", "-serial", f"file:{serial}", "-drive", f"if=pflash,format=raw,file={firmware / 'edk2-aarch64-code.fd'},readonly=on", "-drive", f"if=pflash,format=raw,file={work / 'vars.fd'}", "-drive", f"file=fat:rw:{work / 'esp'},format=raw,if=virtio", "-net", "none"]
    (work / "command.json").write_text(json.dumps(command, indent=2) + "\n")
    started = time.monotonic()
    with (work / "qemu.log").open("wb") as log:
        q = subprocess.Popen(command, stdout=log, stderr=log)
        try:
            while time.monotonic() - started < timeout:
                require(q.poll() is None, "QEMU exited")
                text = serial.read_text(errors="replace") if serial.exists() else ""
                if "AX0 PASS" in text or "AX0 FAIL" in text or "TOWER" in text:
                    break
                time.sleep(.1)
            result = parse(text)
            result.update(private_initrd_sha256=image_hash, elapsed_seconds=round(time.monotonic() - started, 3))
            return result
        finally:
            q.terminate()
            try:
                q.wait(timeout=5)
            except subprocess.TimeoutExpired:
                q.kill()
                q.wait(timeout=5)


def main():
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, default=root / "build/images/qemu-virt-aarch64")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--serial", type=Path, help="parse one existing metal capture instead of booting QEMU")
    parser.add_argument("--firmware", type=Path, default=Path("/opt/homebrew/share/qemu"))
    parser.add_argument("--timeout", type=float, default=90)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=["actor-baseline"], exit=False).result.wasSuccessful() or exit(1)
        return
    if args.serial:
        print(json.dumps(parse(args.serial.read_text(errors="replace")), indent=2))
        return
    require(args.timeout > 0, "invalid timeout")
    work = args.output.resolve() if args.output else Path(tempfile.mkdtemp(prefix="actor-baseline-", dir=root / "build/verification"))
    if args.output:
        work.mkdir(parents=True)
    records = []
    for n in (1, 2):
        result = boot(args.image.resolve(), work / f"boot{n}", args.firmware.resolve(), args.timeout)
        records.append(result)
        (work / f"boot{n}/report.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"AX0 boot{n}: PASS; median RTT {statistics.median(result['summary']['rtt_ns']):.1f} ns; free slots {result['census']['netstack-client']['free']}", flush=True)
    summary = {"source_initrd_sha256": hashlib.sha256((args.image / "EFI/KUMO/initrd.img").read_bytes()).hexdigest(), "source_kernel_sha256": hashlib.sha256((args.image / "EFI/KUMO/kernel/kumo-kernel.elf").read_bytes()).hexdigest(), "qemu": subprocess.check_output(["qemu-system-aarch64", "--version"]).decode().splitlines()[0], "boots": records}
    (work / "report.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(f"AX0 QEMU baseline recorded: {work}", flush=True)


class ParserTests(unittest.TestCase):
    def fixture(self):
        lines=["INDRA login:","AX0 begins:","AX0 census default-login capacity=12 live=4 stored=4 blocked=4 ready=0 running=0 free=8", "AX0 census netstack-client capacity=12 live=6 stored=6 blocked=6 ready=0 running=0 free=6", "AX0 capacity-gate free=6 requires=2 PASS", "netprobe: hold PASS", "AX0 kernel-clock-pair samples=1024 total-ns=10", "AX0 clock-pair samples=1024 total-ns=10"]
        for i in range(1,6):
            lines += [f"AX0 trial={i} rounds=1024 bytes=64 rtt-total-ns=2048 spawn-a-ns=1 spawn-b-ns=2 PASS",f"AX0 memory trial={i} frames-a=10 frames-b=10 clean=4",f"AX0 switch trial={i} simd-errors=0 root-count=2 root-total-ns=4 syscalls=6 switches=2"]
        lines += [f"AX0 scheduler runnable={n} decisions=32768 total-ns=99" for n in [1,2,4,8,12]]
        return "\n".join(lines+["AX0 PASS"])
    def test_complete_measurement(self):
        self.assertEqual(parse(self.fixture())["summary"]["frames_per_idle_actor"],[10]*10)
    def test_failure_and_missing_metrics(self):
        text=self.fixture()
        for broken in (text+"\nTOWER",text.replace("simd-errors=0","simd-errors=1",1),text.replace("AX0 trial=5","missing=5",1),text.replace("frames-a=10","frames-a=0",1),text.replace("syscalls=6","nope=6",1)):
            with self.assertRaises(RuntimeError):parse(broken)
    def test_resident_accounting_and_cleanup(self):
        text=self.fixture()
        for broken in (text.replace("free=6","free=7"),text.replace("live=6","live=5"),text.replace("clean=4","clean=5",1),text.replace("INDRA login:","late login:")+"\nINDRA login:"):
            with self.assertRaises(RuntimeError):parse(broken)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        raise SystemExit(f"AX0 baseline failed: {error}")
