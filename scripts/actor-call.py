#!/usr/bin/env python3
#j824
"""AX2: build a private proof image, run `ax2bench` on two QEMU boots, or parse one metal capture.

`ax2bench` is opt-in like AX0 `actorbench`: it is appended to login's autoexec and starts only
after the ordinary login services settle. One benchmark per boot (never both commands in one
autoexec: two five-second timers would arm, and four probe children would break the metal
two-child rule). Each boot records ten 1024-round-trip trials (five at 8 bytes, five at 32),
then the die/never/hold/repeat correctness matrix and the resident census.

The PLAN/021 metal criterion is enforced by `--serial`: median RTT over the ten trials strictly
below 53.000 us (AX0's lower metal boot median). Syscalls and switches are gated per trial with
Sora's own arithmetic: 3 and 2 per round trip respectively, plus a fixed 64-event allowance for
Sora's observer work (bounded reads and snapshots) inside the trial bracket. `docs/actor-call.md`
records the convention.

The proof image is built with KUMO_CONSOLE_LOGLEVEL=6; the default image is rebuilt, checked
and hashed on every exit. No NIC, credentials or board access. Artifacts stay under
build/verification/ax2.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import struct
import subprocess
import sys
import tempfile
import time
import unittest


AUTOEXEC_APPEND = b"ax2bench\n"
AUTOEXEC_LIMIT = 256
TRIAL_ROUNDS = 1024
TRIAL_KINDS = (8, 8, 8, 8, 8, 32, 32, 32, 32, 32)
CALL_SYSCALLS = 3          # ChannelCall, server ChannelRead, ChannelReply
CALL_SWITCHES = 2          # caller -> server, server -> caller
OBSERVER_SLACK = 64        # fixed allowance for Sora's bounded reads/snapshots in the bracket
MAX_MEDIAN_RTT_NS = 53_000 # AX0's lower metal boot median; AX2 must beat it on metal
CAPACITY = 12              # the login profile's resident capacity (AX0's constant)
REQUIRED_FREE = 2          # at most two concurrent probe children (J819/J822)
HOLD_ROUNDS = 3
BEGINS = "AX2 begins: ChannelCall register fast path (PLAN/021 AX2)"
IMAGE_ARGS = ["cargo", "xtask", "image", "--arch", "aarch64", "--hardware", "qemu-virt-aarch64"]
CONTROLS = ("KUMO_CONSOLE_LOGLEVEL", "KUMO_NET_RESTART_PROOF", "KUMO_NVME_ORACLE",
            "KUMO_NVME_PREPARE", "KUMO_NVME_WRITE_PROOF", "KUMO_AX88_ORACLE")

TRIAL_RE = re.compile(
    r"AX2 trial=(\d+) kind=(\d+) rounds=(\d+) rtt-total-ns=(\d+) syscalls=(\d+) "
    r"switches=(\d+) frames=(\d+) simd-errors=(\d+) (PASS|FAIL)")
MATRIX_RE = re.compile(r"AX2 matrix=(die|never|hold|repeat) result=(\w+) (PASS|FAIL)")
CENSUS_RE = {
    "default-login": re.compile(
        r"AX2 census default-login capacity=(\d+) live=(\d+) stored=(\d+) blocked=(\d+) "
        r"ready=(\d+) running=(\d+) free=(\d+) frames=(\d+) heap=(\d+)"),
    "hold": re.compile(r"AX2 census hold switches=(\d+) rounds=(\d+)"),
    "repeat": re.compile(r"AX2 census repeat frames=(\d+) live=(\d+) stored=(\d+) free=(\d+)"),
}
GATE_RE = re.compile(r"AX2 capacity-gate free=(\d+) requires=(\d+) (PASS|AX4-REQUIRED)")
MATRIX_RESULTS = {"die": "PeerClosed", "never": "TimedOut", "hold": "Ok", "repeat": "Ok"}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def unpack(data):
    require(data[:8] == b"KUMORD01" and struct.unpack_from("<I", data, 8)[0] == 1, "bad initrd")
    count = struct.unpack_from("<I", data, 12)[0]
    require(0 < count <= 64 and 16 + count * 80 <= len(data), "bad initrd table")
    files = {}
    for i in range(count):
        at = 16 + i * 80
        name = data[at:at + 64].split(b"\0", 1)[0].decode()
        offset, size = struct.unpack_from("<QQ", data, at + 64)
        require(name not in files and 16 + count * 80 <= offset <= offset + size <= len(data),
                "bad initrd entry")
        files[name] = data[offset:offset + size]
    return files


def stage(initrd, append=AUTOEXEC_APPEND):
    """Append to etc/autoexec in place; every other entry must stay byte-identical."""
    original = initrd.read_bytes()
    files = unpack(original)
    require("etc/autoexec" in files, "autoexec missing")
    files["etc/autoexec"] = files["etc/autoexec"] + append
    require(len(files["etc/autoexec"]) <= AUTOEXEC_LIMIT, "autoexec exceeds Sora's 256-byte read")
    out = bytearray(original[:16 + len(files) * 80])
    for i, data in enumerate(files.values()):
        struct.pack_into("<QQ", out, 16 + i * 80 + 64, len(out), len(data))
        out.extend(data)
    require(unpack(bytes(out)) == files, "staging changed executable content")
    initrd.write_bytes(out)
    return sha(out), files["etc/autoexec"]


def one(text, needle, what):
    require(text.count(needle) == 1, f"missing/duplicated {what}")
    return text.index(needle)


def parse(text, metal=False):
    """Check one serial transcript against the AX2 proof contract."""
    require("AX2 FAIL" not in text, "AX2 reported a failure")
    require("TOWER" not in text, "unexpected TOWER")
    require("PANIC" not in text, "kernel panic")
    login = one(text, "INDRA login:", "login prompt")
    done = one(text, "autoexec: done", "autoexec completion")
    begins = one(text, BEGINS, "AX2 begins")
    require(done < login < begins, "measurement preceded login")
    require(text.count("AX2 PASS") == 1, "missing/duplicated final AX2 PASS")

    # ---- trials -----------------------------------------------------------------------
    trials, first_trial = [], None
    for line in text.splitlines():
        if "AX2 trial=" not in line:
            continue
        match = TRIAL_RE.fullmatch(line.strip())
        require(match is not None, f"malformed trial: {line!r}")
        index, kind, rounds, total, syscalls, switches, frames, simd, verdict = match.groups()
        row = {"trial": int(index), "kind": int(kind), "rounds": int(rounds),
               "rtt-total-ns": int(total), "syscalls": int(syscalls),
               "switches": int(switches), "frames": int(frames), "simd-errors": int(simd),
               "verdict": verdict}
        trials.append(row)
        first_trial = first_trial or line.strip()
    require(len(trials) == len(TRIAL_KINDS), "missing/duplicated trials")
    require([(t["trial"], t["kind"]) for t in trials]
            == [(i % 5 + 1, k) for i, k in enumerate(TRIAL_KINDS)], "wrong trial order")
    for t in trials:
        require(t["verdict"] == "PASS", "failed individual trial")
        require(t["rounds"] == TRIAL_ROUNDS and t["rtt-total-ns"] > 0, "invalid workload")
        require(t["frames"] == 0, "issued frames during a call trial")
        require(t["simd-errors"] == 0, "SIMD canary corruption")
        require(0 < t["syscalls"] <= CALL_SYSCALLS * t["rounds"] + OBSERVER_SLACK,
                "syscall budget exceeded")
        require(0 < t["switches"] <= CALL_SWITCHES * t["rounds"] + OBSERVER_SLACK,
                "switch budget exceeded")

    # ---- matrices and censuses --------------------------------------------------------
    matrices = []
    for line in text.splitlines():
        if "AX2 matrix=" not in line:
            continue
        match = MATRIX_RE.fullmatch(line.strip())
        require(match is not None, f"malformed matrix: {line!r}")
        matrices.append(match.groups())
    require([m[0] for m in matrices] == ["die", "never", "hold", "repeat"], "wrong matrix order")
    for name, result, verdict in matrices:
        require(verdict == "PASS" and result == MATRIX_RESULTS[name],
                f"matrix {name} failed: result={result} {verdict}")

    census = {}
    for label, pattern in CENSUS_RE.items():
        matches = [pattern.fullmatch(line.strip()) for line in text.splitlines()
                   if line.strip().startswith(f"AX2 census {label} ")]
        require(len(matches) == 1 and matches[0] is not None, f"missing/duplicated {label} census")
        census[label] = [int(v) for v in matches[0].groups()]
    default = dict(zip(("capacity", "live", "stored", "blocked", "ready", "running", "free",
                        "frames", "heap"), census["default-login"]))
    require(default["capacity"] == CAPACITY, "unexpected resident capacity")
    require(0 < default["stored"] <= CAPACITY and default["live"] <= default["stored"],
            "wrong resident accounting")
    require(default["free"] == CAPACITY - default["live"], "wrong free-slot accounting")
    require(default["blocked"] + default["ready"] + default["running"] == default["live"],
            "wrong resident states")
    require(default["free"] >= REQUIRED_FREE, "fewer than two free resident slots")
    hold_switches, hold_rounds = census["hold"]
    require(hold_rounds == HOLD_ROUNDS and hold_switches > 0, "hold phase did not deschedule")
    frames, repeat_live, repeat_stored, repeat_free = census["repeat"]
    require(frames == 0, "issued frames after repeated calls")
    require(repeat_live == default["live"] and 0 < repeat_stored <= CAPACITY
            and repeat_free == CAPACITY - repeat_live, "probe residents leaked")

    gate = [GATE_RE.fullmatch(line.strip()) for line in text.splitlines()
            if line.strip().startswith("AX2 capacity-gate ")]
    require(len(gate) == 1 and gate[0] is not None, "missing/duplicated capacity gate")
    free, requires, status = gate[0].groups()
    require(int(free) == default["free"] and int(requires) == REQUIRED_FREE
            and status == "PASS", "incorrect capacity gate")

    # ---- order ------------------------------------------------------------------------
    require(one(text, "AX2 capacity-gate ", "capacity gate") < text.index(first_trial),
            "capacity gate after a trial")
    require(text.index("AX2 trial=5 kind=32 ") < one(text, "AX2 matrix=die ", "die matrix"),
            "matrix before the trials")
    final = text.index("AX2 PASS")
    require(one(text, "AX2 matrix=die ", "die matrix") < one(text, "AX2 matrix=never ", "never matrix")
            < one(text, "AX2 census hold ", "hold census") < one(text, "AX2 matrix=hold ", "hold matrix")
            < one(text, "AX2 census repeat ", "repeat census") < one(text, "AX2 matrix=repeat ", "repeat matrix")
            < final,
            "phase order")

    rtt = [t["rtt-total-ns"] / t["rounds"] for t in trials]
    median = statistics.median(rtt)
    syscalls_per_rtt = sum(t["syscalls"] for t in trials) / sum(t["rounds"] for t in trials)
    switches_per_rtt = sum(t["switches"] for t in trials) / sum(t["rounds"] for t in trials)
    if metal:
        require(median < MAX_MEDIAN_RTT_NS,
                f"metal median RTT {median:.1f} ns is not below {MAX_MEDIAN_RTT_NS} ns")
    return {
        "trials": trials,
        "matrices": {name: result for name, result, _ in matrices},
        "census": default | {"hold_switches": hold_switches, "hold_rounds": hold_rounds,
                             "repeat_frames": frames, "repeat_live": repeat_live,
                             "repeat_stored": repeat_stored, "repeat_free": repeat_free},
        "capacity_gate": {"free": int(free), "requires": int(requires), "status": status},
        "summary": {
            "rtt_ns": rtt,
            "min_rtt_ns": min(rtt),
            "median_rtt_ns": median,
            "max_rtt_ns": max(rtt),
            "syscalls_per_rtt": syscalls_per_rtt,
            "switches_per_rtt": switches_per_rtt,
            "syscall_gate": CALL_SYSCALLS * TRIAL_ROUNDS + OBSERVER_SLACK,
            "switch_gate": CALL_SWITCHES * TRIAL_ROUNDS + OBSERVER_SLACK,
            "frames_total": sum(t["frames"] for t in trials),
            "simd_errors_total": sum(t["simd-errors"] for t in trials),
            "ax0_reference_ns": MAX_MEDIAN_RTT_NS,
            "metal_rtt_pass": median < MAX_MEDIAN_RTT_NS,
            "factor_vs_ax0": MAX_MEDIAN_RTT_NS / median,
        },
    }


def boot(root, source, work, qemu, firmware, timeout):
    work.mkdir()
    shutil.copytree(source, work / "esp")
    initrd_hash, autoexec = stage(work / "esp/EFI/KUMO/initrd.img")
    (work / "autoexec.txt").write_bytes(autoexec)
    shutil.copyfile(firmware / "edk2-arm-vars.fd", work / "vars.fd")
    serial = work / "serial.log"
    command = [qemu, "-machine", "virt,gic-version=3", "-cpu", "cortex-a72", "-m", "2048",
               "-display", "none", "-monitor", "none", "-serial", f"file:{serial}",
               "-drive", f"if=pflash,format=raw,file={firmware / 'edk2-aarch64-code.fd'},readonly=on",
               "-drive", f"if=pflash,format=raw,file={work / 'vars.fd'}",
               "-drive", f"file=fat:rw:{work / 'esp'},format=raw,if=virtio", "-net", "none"]
    (work / "command.json").write_text(json.dumps(command, indent=2) + "\n")
    started = time.monotonic()
    with (work / "qemu.log").open("wb") as log:
        process = subprocess.Popen(command, cwd=root, stdout=log, stderr=log)
        try:
            while time.monotonic() - started < timeout:
                require(process.poll() is None, f"QEMU exited; inspect {work}")
                text = serial.read_text(errors="replace") if serial.exists() else ""
                if "TOWER" in text:
                    raise RuntimeError(f"unexpected TOWER; inspect {serial}")
                if "PANIC" in text:
                    raise RuntimeError(f"kernel panic; inspect {serial}")
                if "AX2 PASS" in text or "AX2 FAIL" in text:
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError(f"timeout; inspect {serial}")
            result = parse(text)
            result.update(seconds=round(time.monotonic() - started, 3),
                          initrd_sha256=initrd_hash, autoexec_bytes=len(autoexec))
            (work / "report.json").write_text(json.dumps(result, indent=2) + "\n")
            s = result["summary"]
            print(f"  ax2-bench: PASS ({result['seconds']}s, median RTT {s['median_rtt_ns']:.1f} ns, "
                  f"syscalls/RTT {s['syscalls_per_rtt']:.3f}, switches/RTT {s['switches_per_rtt']:.3f}, "
                  f"free {result['census']['free']})", flush=True)
            return result
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, help="new directory for evidence (default: build/verification/ax2/run-*)")
    parser.add_argument("--firmware", type=Path, help="directory containing QEMU edk2-aarch64-code.fd and edk2-arm-vars.fd")
    parser.add_argument("--timeout", type=float, default=240, help="seconds per QEMU boot")
    parser.add_argument("--serial", type=Path, help="parse one existing metal capture (enforces the metal criterion)")
    parser.add_argument("--self-test", action="store_true", help="test the transcript rejection rules without QEMU")
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=[sys.argv[0]], exit=False).result.wasSuccessful() or sys.exit(1)
        return
    if args.serial:
        print(json.dumps(parse(args.serial.read_text(errors="replace"), metal=True), indent=2))
        return
    require(args.timeout > 0, "timeout must be positive")
    root = Path(__file__).resolve().parent.parent
    qemu = shutil.which("qemu-system-aarch64")
    require(qemu is not None, "qemu-system-aarch64 missing")
    firmware = (args.firmware or Path(qemu).resolve().parent.parent / "share/qemu").resolve()
    require(all((firmware / p).is_file() for p in ("edk2-aarch64-code.fd", "edk2-arm-vars.fd")),
            "OVMF files missing; use --firmware")
    if args.output:
        work = args.output.resolve()
        work.mkdir(parents=True)
    else:
        parent = root / "build/verification/ax2"
        parent.mkdir(parents=True, exist_ok=True)
        work = Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    print(f"AX2 proof artifacts: {work}", flush=True)
    default_env = {k: v for k, v in os.environ.items() if k not in CONTROLS}
    proof_env = dict(default_env, KUMO_CONSOLE_LOGLEVEL="6")
    source = root / "build/images/qemu-virt-aarch64"
    manifest = {"script_sha256": sha(Path(__file__).read_bytes()),
                "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root).decode().strip(),
                "qemu": subprocess.check_output([qemu, "--version"]).decode().splitlines()[0],
                "firmware": {p: sha((firmware / p).read_bytes()) for p in ("edk2-aarch64-code.fd", "edk2-arm-vars.fd")},
                "autoexec_append": AUTOEXEC_APPEND.decode(), "cases": {},
                "metal_criterion_ns": MAX_MEDIAN_RTT_NS,
                "observer_slack": {"syscalls": OBSERVER_SLACK, "switches": OBSERVER_SLACK}}
    started = time.monotonic()
    try:
        with (work / "build-proof.log").open("wb") as log:
            subprocess.run(IMAGE_ARGS, cwd=root, env=proof_env, stdout=log, stderr=log, check=True)
        files = unpack((source / "EFI/KUMO/initrd.img").read_bytes())
        manifest["elf_sha256"] = {}
        for name in ("sora", "actorcall"):
            matches = [data for path, data in files.items() if path.endswith("/" + name)]
            require(len(matches) == 1, f"{name} ELF missing from initrd")
            require(matches[0] == (root / "target/aarch64-unknown-none/release" / name).read_bytes(),
                    f"{name} ELF mismatch")
            manifest["elf_sha256"][name] = sha(matches[0])
        require("etc/autoexec" in files and b"echo kumo autoexec online" in files["etc/autoexec"],
                "default autoexec missing from the proof image")
        manifest["source_initrd_sha256"] = sha((source / "EFI/KUMO/initrd.img").read_bytes())
        manifest["source_image_sha256"] = {str(p.relative_to(source)): sha(p.read_bytes())
                                          for p in source.rglob("*") if p.is_file()}
        for n in (1, 2):
            manifest["cases"][f"boot{n}"] = boot(root, source, work / f"boot{n}", qemu, firmware, args.timeout)
        manifest["proofs_passed"] = True
    except BaseException as error:
        manifest["proofs_passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        try:
            with (work / "restore-default.log").open("wb") as log:
                subprocess.run(IMAGE_ARGS, cwd=root, env=default_env, stdout=log, stderr=log, check=True)
            files = unpack((source / "EFI/KUMO/initrd.img").read_bytes())
            require(b"ax2bench" not in files["etc/autoexec"]
                    and b"echo kumo autoexec online" in files["etc/autoexec"],
                    "default autoexec not restored")
            manifest["default_restored"] = True
            manifest["default_initrd_sha256"] = sha((source / "EFI/KUMO/initrd.img").read_bytes())
        finally:
            manifest["passed"] = manifest.get("proofs_passed", False) and manifest.get("default_restored", False)
            manifest["total_seconds"] = round(time.monotonic() - started, 3)
            (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"AX2 QEMU proof green: two boots, ten trials, four matrices, census; default image restored "
          f"({manifest['total_seconds']}s)", flush=True)


class TranscriptTests(unittest.TestCase):
    def fixture(self, rtt_ns=2048, **overrides):
        total = rtt_ns * TRIAL_ROUNDS
        lines = ["autoexec: done", "INDRA login:", BEGINS,
                 "AX2 census default-login capacity=12 live=4 stored=4 blocked=4 ready=0 "
                 "running=0 free=8 frames=0 heap=0",
                 "AX2 capacity-gate free=8 requires=2 PASS"]
        for i, kind in enumerate(TRIAL_KINDS):
            lines.append(f"AX2 trial={i % 5 + 1} kind={kind} rounds=1024 rtt-total-ns={total} "
                         f"syscalls=3102 switches=2058 frames=0 simd-errors=0 PASS")
        lines += ["AX2 matrix=die result=PeerClosed PASS",
                  "AX2 matrix=never result=TimedOut PASS",
                  "AX2 census hold switches=12 rounds=3",
                  "AX2 matrix=hold result=Ok PASS",
                  "AX2 census repeat frames=0 live=4 stored=4 free=8",
                  "AX2 matrix=repeat result=Ok PASS",
                  "AX2 PASS"]
        return "\n".join(lines)

    def test_complete_measurement(self):
        result = parse(self.fixture())
        self.assertEqual(result["summary"]["median_rtt_ns"], 2048)
        self.assertEqual(result["matrices"], MATRIX_RESULTS)
        self.assertTrue(result["summary"]["metal_rtt_pass"])
        self.assertEqual([t["kind"] for t in result["trials"]], list(TRIAL_KINDS))

    def test_metal_median_criterion(self):
        parse(self.fixture(rtt_ns=52_000), metal=True)
        with self.assertRaises(RuntimeError):
            parse(self.fixture(rtt_ns=53_000), metal=True)
        parse(self.fixture(rtt_ns=53_000), metal=False)

    def test_failure_and_missing_markers(self):
        text = self.fixture()
        for broken in (text + "\nTOWER", text.replace("AX2 PASS", "AX2 FAIL"),
                       text.replace("AX2 PASS", ""), text + "\nAX2 PASS",
                       text.replace("autoexec: done", ""),
                       text.replace("INDRA login:", ""),
                       text.replace("INDRA login:", "login later") + "\nINDRA login:",
                       text.replace("PANIC", "") + "\nPANIC"):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_trial_contract(self):
        text = self.fixture()
        first = "AX2 trial=1 kind=8 rounds=1024 rtt-total-ns=2097152 syscalls=3102 switches=2058 frames=0 simd-errors=0 PASS"
        for broken in (text.replace("kind=8", "kind=16", 1),
                       text.replace("rounds=1024", "rounds=512", 1),
                       text.replace(first, first.replace("frames=0", "frames=1")),
                       text.replace("simd-errors=0", "simd-errors=1", 1),
                       text.replace("syscalls=3102", "syscalls=3137", 1),
                       text.replace("switches=2058", "switches=2113", 1),
                       text.replace(first, first.replace("PASS", "FAIL")),
                       text.replace(first + "\n", ""),
                       text.replace("AX2 trial=2 kind=8", "AX2 trial=1 kind=8"),
                       text.replace(first, first + " extra=1")):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_matrix_contract(self):
        text = self.fixture()
        for broken in (text.replace("AX2 matrix=die result=PeerClosed PASS", ""),
                       text.replace("result=TimedOut", "result=Ok"),
                       text.replace("result=Ok PASS", "result=Ok FAIL", 1),
                       text.replace("AX2 matrix=die result=PeerClosed PASS\n", ""),
                       (text.replace("AX2 matrix=die result=PeerClosed PASS", "TMP")
                            .replace("AX2 matrix=never result=TimedOut PASS",
                                     "AX2 matrix=die result=PeerClosed PASS")
                            .replace("TMP", "AX2 matrix=never result=TimedOut PASS"))):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_census_and_gate_contract(self):
        text = self.fixture()
        for broken in (text.replace("free=8 frames=0", "free=7 frames=0"),
                       text.replace("blocked=4", "blocked=3"),
                       text.replace("AX2 capacity-gate free=8", "AX2 capacity-gate free=7"),
                       text.replace("requires=2 PASS", "requires=2 AX4-REQUIRED"),
                       text.replace("switches=12 rounds=3", "switches=0 rounds=3"),
                       text.replace("switches=12 rounds=3", "switches=12 rounds=2"),
                       text.replace("frames=0 live=4", "frames=1 live=4"),
                       text.replace("live=4 stored=4 free=8", "live=5 stored=5 free=7"),
                       text.replace("AX2 census repeat frames=0 live=4 stored=4 free=8", ""),
                       text.replace("AX2 capacity-gate free=8 requires=2 PASS", "")):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_order_is_enforced(self):
        text = self.fixture()
        hold = "AX2 census hold switches=12 rounds=3"
        for broken in (text.replace("autoexec: done\nINDRA login:", "INDRA login:\nautoexec: done"),
                       text.replace("AX2 matrix=die result=PeerClosed PASS\n", ""),
                       text.replace(hold + "\nAX2 matrix=hold result=Ok PASS",
                                    "AX2 matrix=hold result=Ok PASS\n" + hold)):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_autoexec_staging(self):
        require(len(AUTOEXEC_APPEND) <= AUTOEXEC_LIMIT, "append alone exceeds the limit")
        # A synthetic initrd: header, two entries, autoexec and a second file.
        autoexec, other = b"echo kumo autoexec online\n", b"binary"
        table = (b"etc/autoexec".ljust(64, b"\0") + struct.pack("<QQ", 16 + 2 * 80, len(autoexec))
                 + b"bin/other".ljust(64, b"\0")
                 + struct.pack("<QQ", 16 + 2 * 80 + len(autoexec), len(other)))
        data = b"KUMORD01" + struct.pack("<II", 1, 2) + table + autoexec + other
        path = Path(tempfile.mkdtemp(prefix="ax2-stage-", dir=os.environ.get("TMPDIR", "/tmp"))) / "initrd.img"
        path.write_bytes(data)
        try:
            digest, staged = stage(path)
            self.assertEqual(staged, autoexec + AUTOEXEC_APPEND)
            self.assertEqual(unpack(path.read_bytes())["bin/other"], other)
            self.assertEqual(digest, sha(path.read_bytes()))
            big = b"x" * (AUTOEXEC_LIMIT + 1)
            big_table = (b"etc/autoexec".ljust(64, b"\0") + struct.pack("<QQ", 16 + 2 * 80, len(big))
                         + b"bin/other".ljust(64, b"\0")
                         + struct.pack("<QQ", 16 + 2 * 80 + len(big), len(other)))
            path.write_bytes(b"KUMORD01" + struct.pack("<II", 1, 2) + big_table + big + other)
            with self.assertRaises(RuntimeError):
                stage(path)
        finally:
            shutil.rmtree(path.parent, ignore_errors=True)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.SubprocessError, OSError) as error:
        print(f"AX2 proof FAILED: {error}", file=sys.stderr)
        sys.exit(1)
