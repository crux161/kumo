#!/usr/bin/env python3
#j856
#j866
#j867
#j868
#j869
"""AXH: build, boot and check native NET2 loopback/recovery; restore the default image.

No NIC, credentials or board access. Artifacts remain under build/verification. POST is
reported separately (DEFERRED/015); every unexpected process fault or unmet proof fails.
"""

import argparse
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import unittest


@dataclass(frozen=True)
class Case:
    name: str
    script: bytes
    expected: tuple
    ready: int = 1
    towers: int = 0


CASES = (
    Case("authority", b"echo AXH authority begins\nrun netprobe udp\nnet netprobe udp\nnet netprobe errors\nnet netprobe tcperrors\necho AXH authority ends\n", (
        "no network grant (launch it as `net netprobe ...`)", "netprobe: udp PASS", "netprobe: errors PASS", "netprobe: tcperrors PASS",
        "recvfrom empty -> WouldBlock ok", "wait 50ms -> TimedOut ok",
        "close unknown socket -> BadSocket ok", "listen on udp -> Unsupported ok",
        "sendto 10.0.0.1 -> NetUnreachable ok", "sendto 1473 bytes -> MessageTooLarge ok",
        "socket 17 -> QuotaExceeded ok", "malformed header -> session closed by netstack ok",
        "wait connected -> ConnRefused ok", "accept with none pending -> WouldBlock ok",
        "recv with nothing sent -> WouldBlock ok", "wait readable 50ms -> TimedOut ok",
        "send into an unread stream -> WouldBlock ok", "recv after peer close (EOF) -> Ok ok",
    ), towers=1),
    Case("peers", b"echo AXH peers begins\nbg net netprobe tcpserve 8000\nnet netprobe tcpclient 8000\nwait\nbg net netprobe recv 8001 4\nnet netprobe send 8001 4\nwait\necho AXH peers ends\n", (
        "netprobe: tcpserve PASS", "netprobe: tcpclient PASS", "netprobe: recv PASS", "netprobe: send PASS",
        "echo 12000 B fnv=768a0d45 exact, then EOF",
        "tcp echoed 12000 B fnv=768a0d45, EOF after the client's half-close",
        "send 0: 13 B fnv=81f3eabf echoed exact by 127.0.0.1:8001",
        "send 1: 0 B fnv=811c9dc5 echoed exact by 127.0.0.1:8001",
        "send 2: 1472 B fnv=7a220985 echoed exact by 127.0.0.1:8001",
        "send 3: 512 B fnv=9c0721c5 echoed exact by 127.0.0.1:8001",
        "recv 2: 1472 B fnv=7a220985 from 127.0.0.1:",
    )),
    Case("restart", b"echo AXH restart begins\nbg net netprobe restartwait\nnetstack-crash-proof\nwait\nnet netprobe restartfresh 2\necho AXH restart ends\n", (
        "restartwait armed (live TCP, pending WAIT)", "old session -> PeerClosed; no replay ok",
        "netprobe: restartwait PASS", "netprobe: restartfresh PASS", "session generation 131073,",
        "old socket id on fresh session -> BadSocket ok", "old TCP bind reused -> Ok ok",
        "old generation -> StaleSession ok", "netstack-proof: kill observed",
    ), ready=2),
    Case("burst", b"echo AXH burst begins\nnet netprobe udp\nnetstack-crash-proof burst\nnet netprobe udp\necho AXH burst ends\n", (
        "netprobe: udp PASS", "giving up (restart intensity or resources exhausted)",
        "unavailable after give-up; session not granted", "startup grant unavailable",
        "netstack-proof: kill observed",
    ), ready=4),
    Case("idle", b"echo AXH idle begins\nnet netprobe udp\nnetstack-crash-proof idle\necho AXH idle ends\n", (
        "netprobe: udp PASS", "netstack-proof: idle kill armed", "netstack-proof: idle kill sent",
    ), ready=2),
    # NET15-c1 on the std path: one self-contained builtin spawns the arming std client,
    # kills the supervised netstack, waits for the replacement, drains the old client and
    # runs the fresh one. Nothing is asked of the autoexec after the kill. — TIMBERDOODLE
    Case("stdrestart", b"echo AXH stdrestart begins\nnetstack-crash-proof std 7805\necho AXH stdrestart ends\n", (
        "netstack-proof: std client armed",
        "std: net recover-wait armed (live TCP, pending read)",
        "netstack: terminated; old sessions invalidated",
        "std: net recover-wait old session -> ECONNRESET ok",
        "netstack-proof: std kill observed",
        "netstack: restart delayed",
        "netstack: supervised instance 2 ready",
        "netstack-proof: std instance ready",
        "netstack-proof: std old client exited",
        "std: net recover-fresh bind on the old port -> OK",
        "std: net recover-fresh exact exchange -> OK",
        "std: net recovery -> OK",
        "netstack-proof: std fresh client done",
    ), ready=2),
    # SH3-d2: the Rust std metadata surface. std::fs create_dir/metadata/write/rename/
    # remove_file/remove_dir reach relibc's mkdirat/fstatat/renameat2/unlinkat, which
    # speak the SH3 vfs ops to this launch's fatfs server. Net-free by design: no net
    # word runs, so Sora never starts the netstack supervisor and ready=0 asserts that
    # this boot has no netstack instance at all. — TIMBERDOODLE
    Case("fsmeta", b"echo AXH fsmeta begins\nruststd fsmeta\necho AXH fsmeta ends\n", (
        "std: sh3 mkdir -> OK",
        "std: sh3 root stat -> OK",
        "std: sh3 stat dir -> OK",
        "std: sh3 file write -> OK",
        "std: sh3 file stat -> OK",
        "std: sh3 rename -> OK",
        "std: sh3 read back -> OK",
        "std: sh3 lock ex -> OK",
        "std: sh3 lock conflict ex -> OK",
        "std: sh3 lock conflict sh -> OK",
        "std: sh3 unlock -> OK",
        "std: sh3 lock sh -> OK",
        "std: sh3 lock ex vs sh -> OK",
        "std: sh3 shared unlock -> OK",
        "std: sh3 blocking lock free -> OK",
        "std: sh3 fcntl setlk -> OK",
        "std: sh3 fcntl unlock -> OK",
        "std: sh3 fcntl getlk -> OK",
        "std: sh3 mkdir sub -> OK",
        "std: sh3 read dir -> OK",
        "std: sh3 read dir root -> OK",
        "std: sh3 read dir empty -> OK",
        "std: sh3 rmdir sub -> OK",
        "std: sh3 d2d write -> OK",
        "std: sh3 seek -> OK",
        "std: sh3 seek read -> OK",
        "std: sh3 seek end -> OK",
        "std: sh3 at read -> OK",
        "std: sh3 at write -> OK",
        "std: sh3 at write readback -> OK",
        "std: sh3 append -> OK",
        "std: sh3 append readback -> OK",
        "std: sh3 set len -> OK",
        "std: sh3 set len stat -> OK",
        "std: sh3 fchmod fd -> OK",
        "std: sh3 clone -> OK",
        "std: sh3 mkdir all -> OK",
        "std: sh3 mkdir all stat -> OK",
        "std: sh3 copy -> OK",
        "std: sh3 copy readback -> OK",
        "std: sh3 remove all -> OK",
        "std: sh3 remove all gone -> OK",
        "std: sh3 d2d cleanup c -> OK",
        "std: sh3 d2d cleanup e -> OK",
        "std: sh3 old name gone -> OK",
        "std: sh3 unlink -> OK",
        "std: sh3 rmdir -> OK",
        "std: sh3 stat removed dir -> OK",
    ), ready=0),
)
POST_NAMES = ("CONSOLE ROUTE", "BLOCK SERVE", "BLOCK FAT32", "BLOCK FILE", "NET LOOPBACK")
POST_ALL = ("USERLAND EL0", "SORA HANDSHAKE", *POST_NAMES, "HANDLE XFER", "PORT BIND")
IMAGE_ARGS = ["cargo", "xtask", "image", "--arch", "aarch64", "--hardware", "qemu-virt-aarch64"]
CONTROLS = ("KUMO_CONSOLE_LOGLEVEL", "KUMO_NET_RESTART_PROOF", "KUMO_NVME_ORACLE",
            "KUMO_NVME_PREPARE", "KUMO_NVME_WRITE_PROOF", "KUMO_AX88_ORACLE")


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
        require(name not in files and 16 + count * 80 <= offset <= offset + size <= len(data), "bad initrd entry")
        files[name] = data[offset:offset + size]
    return files


def stage(initrd, case):
    original = initrd.read_bytes()
    files = unpack(original)
    require(len(case.script) <= 256, f"{case.name}: autoexec exceeds Sora's 256-byte read")
    require("etc/autoexec" in files, "autoexec missing")
    files["etc/autoexec"] = case.script
    out = bytearray(original[:16 + len(files) * 80])
    for i, data in enumerate(files.values()):
        struct.pack_into("<QQ", out, 16 + i * 80 + 64, len(out), len(data))
        out.extend(data)
    require([k for k in files if files[k] != unpack(original)[k]] == ["etc/autoexec"], "staging changed executable content")
    initrd.write_bytes(out)
    return sha(out)


def validate(case, text):
    begin, end = f"AXH {case.name} begins", f"AXH {case.name} ends"
    for marker in (begin, end, "autoexec: done", "INDRA login:", *case.expected):
        require(text.count(marker) == 1, f"{case.name}: missing/duplicated {marker!r}")
    require(text.index(begin) < text.index(end) < text.index("INDRA login:"), "proof/login order")
    require(not re.search(r"netprobe: (?:FAIL\b|[^\r\n]* FAIL\b)|netstack-proof: FAIL", text), "probe failure")
    towers = text.split("TOWER (reversed): MICROSERVICE CONTAINED")[1:]
    require(text.count("TOWER") == case.towers == len(towers), "unexpected TOWER")
    for tower in towers:
        require("cause=ABEND  exit=0x0000000000000002" in tower.split("fbgeom", 1)[0], "unexpected fault/exit")
    if towers:
        launch = re.search(r"bin/netprobe: proc koid=0x([0-9a-f]+)", text[text.index(begin):])
        require(launch is not None and f"victim koid=0x{int(launch[1], 16):016x}" in towers[0], "fault was not the no-grant probe")
        no_grant = text.index("no network grant")
        require(text.index(begin) < text.index("TOWER") < text.index("netprobe: udp PASS") < text.index(end), "unrelated exit-2 fault")
        require(no_grant < text.index("netprobe: udp PASS"), "grant refusal missing")
    ready = re.findall(r"netstack: supervised instance (\d+) ready", text)
    require(ready == [str(i) for i in range(1, case.ready + 1)], "wrong instance sequence/respawn loop")
    if case.name == "peers":
        require(text.count("wait: background job finished") == 2, "background peers not drained")
        require(re.search(r"\([1-9][0-9]* partial\)", text), "partial TCP send not exercised")
        require(text.count("live sessions=2") >= 2, "peers did not overlap")
        require(text.count("1472 B fnv=7a220985") == 2, "maximum UDP datagram not exercised")
    if case.name == "burst":
        require(text.count("netstack: terminated; old sessions invalidated") == 4, "burst termination count")
    if case.name == "idle":
        require(text.index("INDRA login:") < text.index("netstack-proof: idle kill sent"), "kill was not idle at login")
    post = [line for line in text.splitlines() if re.match(r"[A-Z][A-Z 0-9]+\s+Check\s", line)]
    for name in POST_ALL:
        reports = [line for line in post if " ".join(line.split("Check")[0].split()) == name]
        require(len(reports) == 1 and reports[0].endswith(("OK", "FAIL")), f"missing/duplicated POST {name}")
    failures = [line for line in text.splitlines() if re.search(r"\bFAIL\b", line)]
    # Do not ignore arbitrary FAIL output: exactly the known five POST lines, or none.
    # The idle case never blocks autoexec and must keep POST green. — TIMBERDOODLE
    expected_failures = [line for line in post if line.endswith("FAIL") and line.startswith(POST_NAMES)]
    require(failures == expected_failures, "unexpected FAIL outside known POST checks")
    require(not failures or (len(failures) == 5 and {line.split("Check")[0].strip() for line in failures} == set(POST_NAMES)), "partial/unknown POST failure set")
    require(case.name != "idle" or not failures, "idle POST failure")
    return {"post": post, "known_post_failures": len(failures), "instances": ready, "expected_exit2": case.towers}


def boot(root, source, case, work, qemu, firmware, timeout):
    work.mkdir()
    shutil.copytree(source, work / "esp")
    initrd_hash = stage(work / "esp/EFI/KUMO/initrd.img", case)
    (work / "autoexec.txt").write_bytes(case.script)
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
                if "INDRA login:" in text and (case.name != "idle" or "supervised instance 2 ready" in text):
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError(f"{case.name}: timeout; inspect {serial}")
            result = validate(case, text)
            result.update(seconds=round(time.monotonic() - started, 3), initrd_sha256=initrd_hash, autoexec_bytes=len(case.script))
            (work / "report.json").write_text(json.dumps(result, indent=2) + "\n")
            print(f"  {case.name}: PASS ({result['seconds']}s, POST failures {result['known_post_failures']}, expected exit-2 TOWER {case.towers})", flush=True)
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
    parser.add_argument("--output", type=Path, help="new directory for evidence (default: build/verification/netstack-smoke/run-*)")
    parser.add_argument("--firmware", type=Path, help="directory containing QEMU edk2-aarch64-code.fd and edk2-arm-vars.fd")
    parser.add_argument("--timeout", type=float, default=90, help="seconds per boot")
    parser.add_argument("--self-test", action="store_true", help="test the transcript rejection rules without QEMU")
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=[sys.argv[0]], exit=False).result.wasSuccessful() or sys.exit(1)
        return
    require(args.timeout > 0, "timeout must be positive")
    root = Path(__file__).resolve().parent.parent
    qemu = shutil.which("qemu-system-aarch64")
    require(qemu is not None, "qemu-system-aarch64 missing")
    firmware = (args.firmware or Path(qemu).resolve().parent.parent / "share/qemu").resolve()
    require(all((firmware / p).is_file() for p in ("edk2-aarch64-code.fd", "edk2-arm-vars.fd")), "OVMF files missing; use --firmware")
    if args.output:
        work = args.output.resolve()
        work.mkdir(parents=True)
    else:
        parent = root / "build/verification/netstack-smoke"
        parent.mkdir(parents=True, exist_ok=True)
        work = Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    print(f"NET2/AXH artifacts: {work}", flush=True)
    default_env = {k: v for k, v in os.environ.items() if k not in CONTROLS}
    proof_env = dict(default_env, KUMO_CONSOLE_LOGLEVEL="6", KUMO_NET_RESTART_PROOF="1")
    source = root / "build/images/qemu-virt-aarch64"
    manifest = {"script_sha256": sha(Path(__file__).read_bytes()), "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root).decode().strip(),
                "qemu": subprocess.check_output([qemu, "--version"]).decode().splitlines()[0], "cases": {}, "firmware": {p: sha((firmware / p).read_bytes()) for p in ("edk2-aarch64-code.fd", "edk2-arm-vars.fd")}}
    started = time.monotonic()
    try:
        with (work / "build-proof.log").open("wb") as log:
            subprocess.run(IMAGE_ARGS, cwd=root, env=proof_env, stdout=log, stderr=log, check=True)
        files = unpack((source / "EFI/KUMO/initrd.img").read_bytes())
        manifest["elf_sha256"] = {}
        for name in ("sora", "netstack", "netprobe"):
            matches = [data for path, data in files.items() if path.endswith("/" + name)]
            require(len(matches) == 1 and matches[0] == (root / "target/aarch64-unknown-none/release" / name).read_bytes(), f"{name} ELF mismatch")
            manifest["elf_sha256"][name] = sha(matches[0])
        manifest["source_image_sha256"] = {str(p.relative_to(source)): sha(p.read_bytes()) for p in source.rglob("*") if p.is_file()}
        for case in CASES:
            manifest["cases"][case.name] = boot(root, source, case, work / case.name, qemu, firmware, args.timeout)
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
            sora = next(data for path, data in files.items() if path.endswith("/sora"))
            require(b"netstack-crash-proof" not in sora and b"netstack-proof:" not in sora, "fault injection survived default restoration")
            manifest["default_restored"] = True
            manifest["default_initrd_sha256"] = sha((source / "EFI/KUMO/initrd.img").read_bytes())
        finally:
            manifest["passed"] = manifest.get("proofs_passed", False) and manifest.get("default_restored", False)
            manifest["total_seconds"] = round(time.monotonic() - started, 3)
            (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"NET2/AXH green: {len(CASES)} proofs passed; default image restored ({manifest['total_seconds']}s)", flush=True)


class TranscriptTests(unittest.TestCase):
    def fixture(self, case):
        lines = [f"AXH {case.name} begins", *case.expected]
        lines.extend(f"netstack: supervised instance {n} ready" for n in range(1, case.ready + 1))
        lines.extend([f"{name} Check baseline OK" for name in POST_ALL])
        lines.extend([f"AXH {case.name} ends", "autoexec: done", "INDRA login:"])
        return "\n".join(lines)

    def test_all_autoexecs_fit(self):
        self.assertTrue(all(len(case.script) <= 256 for case in CASES))

    def test_missing_duplicate_and_failed_proof_are_rejected(self):
        case = CASES[2]
        text = self.fixture(case)
        validate(case, text)
        for broken in (text.replace("netprobe: restartwait PASS", ""), text + "\nnetprobe: restartwait PASS", text + "\nnetprobe: FAIL wrong bytes", text + "\nnetprobe: tcpclient FAIL"):
            with self.assertRaises(RuntimeError):
                validate(case, broken)

    def test_extra_fault_and_respawn_are_rejected(self):
        case = CASES[2]
        text = self.fixture(case)
        for broken in (text + "\nTOWER (reversed): MICROSERVICE CONTAINED\ncause=FAULT", text + "\nnetstack: supervised instance 3 ready"):
            with self.assertRaises(RuntimeError):
                validate(case, broken)

    def test_only_complete_known_post_failure_set_is_allowed(self):
        case = CASES[2]
        text = self.fixture(case)
        failures = [f"{name} Check baseline FAIL" for name in POST_NAMES]
        known = text
        for name in POST_NAMES:
            known = known.replace(f"{name} Check baseline OK", f"{name} Check baseline FAIL")
        self.assertEqual(validate(case, known)["known_post_failures"], 5)
        for extra in (failures[0], "ALIEN Check unexpected FAIL", "random FAIL"):
            with self.assertRaises(RuntimeError):
                validate(case, text + "\n" + extra)

    def test_exit2_is_allowed_only_for_authority(self):
        case = CASES[0]
        text = self.fixture(case).replace("netprobe: udp PASS", "TOWER (reversed): MICROSERVICE CONTAINED\nvictim koid=0x0000000000000050\ncause=ABEND  exit=0x0000000000000002\nfbgeom\nnetprobe: udp PASS", 1)
        text = text.replace("AXH authority begins", "AXH authority begins\nbin/netprobe: proc koid=0x50", 1)
        validate(case, text)
        for broken in (text.replace("exit=0x0000000000000002", "exit=0x0000000000000001"), text.replace("victim koid=0x0000000000000050", "victim koid=0x0000000000000051")):
            with self.assertRaises(RuntimeError):
                validate(case, broken)

    def test_missing_post_is_rejected(self):
        case = CASES[2]
        with self.assertRaises(RuntimeError):
            validate(case, self.fixture(case).replace("PORT BIND Check baseline OK", ""))

    def test_idle_kill_must_follow_login_and_keep_post_green(self):
        case = CASES[4]
        text = self.fixture(case)
        with self.assertRaises(RuntimeError):
            validate(case, text)
        text = text.replace("netstack-proof: idle kill sent", "") + "\nnetstack-proof: idle kill sent"
        validate(case, text)
        with self.assertRaises(RuntimeError):
            validate(case, text.replace("PORT BIND Check baseline OK", "PORT BIND Check baseline FAIL"))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.SubprocessError, OSError) as error:
        print(f"NET2/AXH FAILED: {error}", file=sys.stderr)
        sys.exit(1)
