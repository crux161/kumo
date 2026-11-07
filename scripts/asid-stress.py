#!/usr/bin/env python3
#j823
"""AX1: build, boot and check the address-space stress proof; restore the default image.

One boot stages a private autoexec that runs:
  * two live processes (`bg run asidpair conc a` + `run asidpair conc b`) that map DIFFERENT data at
    the SAME user VA and byte-check every read while the 20 Hz scheduler rotates them;
  * one process that unmaps and remaps that VA with new contents;
  * three fresh processes that each map the VA after their predecessor exited.

On the current kernel (ASID 0 for every process, full `tlbi vmalle1` per root switch) every
stale counter must be zero. The harness rejects a missing/duplicated PASS marker, a non-zero
stale count, same-data peers, non-overlapping live windows, a TOWER, or an unexpected fault.
The proof image is a private copy; the default image is rebuilt and checked on every exit.

No NIC, credentials or board access. Artifacts remain under build/verification.
"""

import argparse
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


AUTOEXEC = (
    b"echo AS1 conc begins\n"
    b"bg run asidpair conc a 11\n"
    b"run asidpair conc b 12\n"
    b"wait\n"
    b"echo AS1 conc ends\n"
    b"echo AS1 seq begins\n"
    b"run asidpair remap 21\n"
    b"run asidpair reuse 31\n"
    b"run asidpair reuse 32\n"
    b"run asidpair reuse 33\n"
    b"echo AS1 seq ends\n"
)
CONC_ROLES = ("a", "b")
REMAP_NONCES = (21, 1021, 2021)
REUSE_NONCES = (31, 32, 33)
FIXED_VA = 0x1800_0000
IMAGE_ARGS = ["cargo", "xtask", "image", "--arch", "aarch64", "--hardware", "qemu-virt-aarch64"]
CONTROLS = ("KUMO_CONSOLE_LOGLEVEL", "KUMO_NET_RESTART_PROOF", "KUMO_NVME_ORACLE",
            "KUMO_NVME_PREPARE", "KUMO_NVME_WRITE_PROOF", "KUMO_AX88_ORACLE")
# The live windows must overlap for at least a second. At the 20 Hz tick that is at least
# twenty opportunities for the scheduler to switch address spaces, i.e. real swap pressure,
# not a claim about how many switches actually occurred.
MIN_OVERLAP_NS = 1_000_000_000
# A descheduling gap of more than 20 ms on this workload is a tick, not loop cost.
MIN_GAPS = 2
MIN_ROUNDS = 10


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


def stage(initrd, script):
    original = initrd.read_bytes()
    files = unpack(original)
    require(len(script) <= 256, "autoexec exceeds Sora's 256-byte read")
    require("etc/autoexec" in files, "autoexec missing")
    files["etc/autoexec"] = script
    out = bytearray(original[:16 + len(files) * 80])
    for i, data in enumerate(files.values()):
        struct.pack_into("<QQ", out, 16 + i * 80 + 64, len(out), len(data))
        out.extend(data)
    require([k for k in files if files[k] != unpack(original)[k]] == ["etc/autoexec"], "staging changed executable content")
    initrd.write_bytes(out)
    return sha(out)


def section(text, begin, end):
    require(text.count(begin) == 1 and text.count(end) == 1, f"missing/duplicated {begin!r} or {end!r}")
    start = text.index(begin)
    stop = text.index(end)
    require(start < stop, f"{begin!r} after {end!r}")
    return text[start:stop]


def one(pattern, text, what):
    found = re.findall(pattern, text)
    require(len(found) == 1, f"expected one {what}, found {len(found)}")
    return found[0]


def validate(text):
    """Check one serial transcript against the AX1 proof contract."""
    for marker in ("AS1 conc begins", "AS1 conc ends", "AS1 seq begins", "AS1 seq ends",
                   "autoexec: done", "INDRA login:"):
        require(text.count(marker) == 1, f"missing/duplicated {marker!r}")
    conc = section(text, "AS1 conc begins", "AS1 conc ends")
    seq = section(text, "AS1 seq begins", "AS1 seq ends")
    require(text.index("AS1 conc ends") < text.index("AS1 seq begins"), "phase order")
    require(text.index("AS1 seq ends") < text.index("autoexec: done") < text.index("INDRA login:"),
            "proof/login order")
    require(not re.search(r"asidpair:\s*FAIL\b", text), "asidpair reported a failure")
    require(text.count("TOWER") == 0, "unexpected TOWER")
    require("PANIC" not in text, "kernel panic")

    # ---- concurrent phase -------------------------------------------------------------
    koids = re.findall(r"bin/asidpair: proc koid=0x([0-9a-f]+)", conc)
    require(len(koids) == 2 and len(set(koids)) == 2, f"expected two distinct concurrent processes, saw {koids}")
    begins = {}
    for role, nonce, digest in re.findall(
            r"asidpair: begin conc role=([ab]) nonce=(\d+) va=0x18000000 sum=0x([0-9a-f]{8})", conc):
        require(role not in begins, f"duplicate conc begin role {role}")
        begins[role] = (int(nonce), digest)
    require(set(begins) == set(CONC_ROLES), f"missing conc role: {sorted(begins)}")
    require(begins["a"][1] != begins["b"][1], "peers expect identical data")
    ends = {}
    for role, rounds, stale, gaps, begin_ns, end_ns in re.findall(
            r"asidpair: end conc role=([ab]) rounds=(\d+) stale=(\d+) gaps=(\d+) begin-ns=(\d+) end-ns=(\d+)", conc):
        require(role not in ends, f"duplicate conc end role {role}")
        ends[role] = (int(rounds), int(stale), int(gaps), int(begin_ns), int(end_ns))
    require(set(ends) == set(CONC_ROLES), f"missing conc end role: {sorted(ends)}")
    for role in CONC_ROLES:
        rounds, stale, gaps, begin_ns, end_ns = ends[role]
        require(stale == 0, f"role {role}: {stale} stale bytes")
        require(rounds >= MIN_ROUNDS, f"role {role}: only {rounds} checked rounds")
        require(gaps >= MIN_GAPS, f"role {role}: only {gaps} descheduling gaps (no real swapping)")
        require(end_ns > begin_ns, f"role {role}: inverted window")
        require(one(rf"asidpair: PASS conc role={role}", text, f"conc PASS role {role}"), "conc PASS")
    overlap = min(ends[r][4] for r in CONC_ROLES) - max(ends[r][3] for r in CONC_ROLES)
    require(overlap >= MIN_OVERLAP_NS, f"live windows overlap only {overlap} ns")
    require(overlap <= min(ends[r][4] - ends[r][3] for r in CONC_ROLES), "overlap exceeds a window")

    # ---- unmap / remap phase ----------------------------------------------------------
    remote_koids = re.findall(r"bin/asidpair: proc koid=0x([0-9a-f]+)", seq)
    require(len(remote_koids) == 1 + len(REUSE_NONCES), f"expected {1 + len(REUSE_NONCES)} sequential spawns")
    require(len(set(remote_koids)) == len(remote_koids), "sequential spawns reused a process koid")
    require(one(r"asidpair: PASS remap generations=3 stale=0", text, "remap PASS"), "remap PASS")
    gens = re.findall(r"asidpair: remap gen=(\d+) nonce=(\d+) stale=(\d+) sum=0x([0-9a-f]{8})", seq)
    require([(int(g), int(n), int(s)) for g, n, s, _ in gens]
            == [(i + 1, nonce, 0) for i, nonce in enumerate(REMAP_NONCES)], "remap generation record")
    require(len({digest for _, _, _, digest in gens}) == len(REMAP_NONCES), "remap generations reused data")

    # ---- exit / reuse phase -----------------------------------------------------------
    for index, nonce in enumerate(REUSE_NONCES):
        require(text.count(f"asidpair: PASS reuse nonce={nonce}") == 1, f"reuse PASS nonce {nonce}")
        require(seq.count(f"asidpair: reuse begin nonce={nonce}") == 1, f"reuse begin nonce {nonce}")
        stale = one(rf"asidpair: reuse end nonce={nonce} stale=(\d+)", seq, f"reuse end nonce {nonce}")
        require(int(stale) == 0, f"reuse nonce {nonce}: {stale} stale bytes")
        require(seq.index(f"asidpair: reuse begin nonce={nonce}")
                < seq.index(f"asidpair: reuse end nonce={nonce}")
                < seq.index(f"asidpair: PASS reuse nonce={nonce}"), f"reuse order nonce {nonce}")
        if index:
            require(seq.index(f"asidpair: PASS reuse nonce={REUSE_NONCES[index - 1]}")
                    < seq.index(f"asidpair: reuse begin nonce={nonce}"), "reuse processes overlapped")

    return {
        "conc_koids": koids,
        "sequential_koids": remote_koids,
        "conc_rounds": {role: ends[role][0] for role in CONC_ROLES},
        "conc_gaps": {role: ends[role][2] for role in CONC_ROLES},
        "conc_checksums": {role: begins[role][1] for role in CONC_ROLES},
        "overlap_ns": overlap,
        "remap_nonces": [int(n) for _, n, _, _ in gens],
        "reuse_nonces": list(REUSE_NONCES),
    }


def boot(root, source, work, qemu, firmware, timeout):
    work.mkdir()
    shutil.copytree(source, work / "esp")
    initrd_hash = stage(work / "esp/EFI/KUMO/initrd.img", AUTOEXEC)
    (work / "autoexec.txt").write_bytes(AUTOEXEC)
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
                # Fail on an explicit proof failure or fault immediately: a defective kernel
                # must not cost the whole timeout just because login never arrives. A stale
                # read count outranks the generic failure line so the report names the damage.
                stale = re.search(r"asidpair: (?:FAIL conc-[ab] detail=stale|end conc role=[ab] rounds=\d+)"
                                  r" stale=(\d+)", text)
                if stale and int(stale.group(1)) > 0:
                    raise RuntimeError(f"{stale.group(1)} stale bytes read; inspect {serial}")
                if "asidpair: FAIL" in text:
                    raise RuntimeError(f"asidpair reported a failure; inspect {serial}")
                # A TOWER does not stop the wait: it can mean the boot is wedged, but the
                # stress processes may still run and expose the stale reads we are after.
                # Either way the transcript is rejected by `validate` below or reported here.
                if "INDRA login:" in text and "AS1 seq ends" in text and "autoexec: done" in text:
                    break
                time.sleep(0.1)
            else:
                if "TOWER" in text:
                    raise RuntimeError(f"unexpected TOWER and no login; inspect {serial}")
                if "PANIC" in text:
                    raise RuntimeError(f"kernel panic and no login; inspect {serial}")
                raise RuntimeError(f"timeout; inspect {serial}")
            result = validate(text)
            result.update(seconds=round(time.monotonic() - started, 3), initrd_sha256=initrd_hash,
                          autoexec_bytes=len(AUTOEXEC))
            (work / "report.json").write_text(json.dumps(result, indent=2) + "\n")
            print(f"  ax1-stress: PASS ({result['seconds']}s, overlap {result['overlap_ns'] / 1e9:.2f}s, "
                  f"rounds {result['conc_rounds']}, gaps {result['conc_gaps']})", flush=True)
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
    parser.add_argument("--output", type=Path, help="new directory for evidence (default: build/verification/asid-stress/run-*)")
    parser.add_argument("--firmware", type=Path, help="directory containing QEMU edk2-aarch64-code.fd and edk2-arm-vars.fd")
    parser.add_argument("--timeout", type=float, default=240, help="seconds for the one boot")
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
        parent = root / "build/verification/asid-stress"
        parent.mkdir(parents=True, exist_ok=True)
        work = Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    print(f"AX1 stress artifacts: {work}", flush=True)
    default_env = {k: v for k, v in os.environ.items() if k not in CONTROLS}
    proof_env = dict(default_env, KUMO_CONSOLE_LOGLEVEL="6")
    source = root / "build/images/qemu-virt-aarch64"
    manifest = {"script_sha256": sha(Path(__file__).read_bytes()),
                "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root).decode().strip(),
                "qemu": subprocess.check_output([qemu, "--version"]).decode().splitlines()[0],
                "firmware": {p: sha((firmware / p).read_bytes()) for p in ("edk2-aarch64-code.fd", "edk2-arm-vars.fd")},
                "autoexec": AUTOEXEC.decode(), "cases": {}}
    started = time.monotonic()
    try:
        with (work / "build-proof.log").open("wb") as log:
            subprocess.run(IMAGE_ARGS, cwd=root, env=proof_env, stdout=log, stderr=log, check=True)
        files = unpack((source / "EFI/KUMO/initrd.img").read_bytes())
        manifest["elf_sha256"] = {}
        for name in ("sora", "asidpair"):
            matches = [data for path, data in files.items() if path.endswith("/" + name)]
            require(len(matches) == 1, f"{name} ELF missing from initrd")
            require(matches[0] == (root / "target/aarch64-unknown-none/release" / name).read_bytes(), f"{name} ELF mismatch")
            manifest["elf_sha256"][name] = sha(matches[0])
        manifest["source_image_sha256"] = {str(p.relative_to(source)): sha(p.read_bytes()) for p in source.rglob("*") if p.is_file()}
        manifest["source_initrd_sha256"] = sha((source / "EFI/KUMO/initrd.img").read_bytes())
        manifest["cases"]["stress"] = boot(root, source, work / "stress", qemu, firmware, args.timeout)
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
            require(b"AS1 " not in files["etc/autoexec"] and b"echo kumo autoexec online" in files["etc/autoexec"],
                    "default autoexec not restored")
            manifest["default_restored"] = True
            manifest["default_initrd_sha256"] = sha((source / "EFI/KUMO/initrd.img").read_bytes())
        finally:
            manifest["passed"] = manifest.get("proofs_passed", False) and manifest.get("default_restored", False)
            manifest["total_seconds"] = round(time.monotonic() - started, 3)
            (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"AX1 stress green: concurrent, remap and exit/reuse proofs passed; default image restored "
          f"({manifest['total_seconds']}s)", flush=True)


class TranscriptTests(unittest.TestCase):
    def fixture(self, **overrides):
        begin_a, begin_b = overrides.get("begin_a", 0), overrides.get("begin_b", 600_000_000)
        end_a, end_b = overrides.get("end_a", 6_000_000_000), overrides.get("end_b", 3_500_000_000)
        stale_a, stale_b = overrides.get("stale_a", 0), overrides.get("stale_b", 0)
        sum_a, sum_b = overrides.get("sum_a", "11111111"), overrides.get("sum_b", "22222222")
        koid_a, koid_b = overrides.get("koid_a", "51"), overrides.get("koid_b", "52")
        lines = [
            "AS1 conc begins",
            f"bin/asidpair: proc koid=0x{koid_a}",
            f"bin/asidpair: proc koid=0x{koid_b}",
            f"asidpair: begin conc role=a nonce=11 va=0x18000000 sum=0x{sum_a}",
            f"asidpair: begin conc role=b nonce=12 va=0x18000000 sum=0x{sum_b}",
            f"asidpair: end conc role=a rounds=900 stale={stale_a} gaps=10 begin-ns={begin_a} end-ns={end_a}",
            f"asidpair: end conc role=b rounds=500 stale={stale_b} gaps=8 begin-ns={begin_b} end-ns={end_b}",
            "asidpair: PASS conc role=a",
            "asidpair: PASS conc role=b",
            "AS1 conc ends",
            "AS1 seq begins",
            "bin/asidpair: proc koid=0x9",
            "bin/asidpair: proc koid=0xa",
            "bin/asidpair: proc koid=0xb",
            "bin/asidpair: proc koid=0xc",
            "asidpair: remap gen=1 nonce=21 stale=0 sum=0x00000001",
            "asidpair: remap gen=2 nonce=1021 stale=0 sum=0x00000002",
            "asidpair: remap gen=3 nonce=2021 stale=0 sum=0x00000003",
            "asidpair: PASS remap generations=3 stale=0",
            "asidpair: reuse begin nonce=31",
            "asidpair: reuse end nonce=31 stale=0 sum=0x00000004",
            "asidpair: PASS reuse nonce=31",
            "asidpair: reuse begin nonce=32",
            "asidpair: reuse end nonce=32 stale=0 sum=0x00000005",
            "asidpair: PASS reuse nonce=32",
            "asidpair: reuse begin nonce=33",
            "asidpair: reuse end nonce=33 stale=0 sum=0x00000006",
            "asidpair: PASS reuse nonce=33",
            "AS1 seq ends",
            "autoexec: done",
            "INDRA login:",
        ]
        return "\n".join(lines)

    def test_all_autoexecs_fit(self):
        self.assertLessEqual(len(AUTOEXEC), 256)

    def test_good_fixture_passes(self):
        result = validate(self.fixture())
        self.assertEqual(result["conc_rounds"], {"a": 900, "b": 500})
        self.assertGreater(result["overlap_ns"], 0)

    def test_missing_or_duplicate_pass_is_rejected(self):
        text = self.fixture()
        for broken in (text.replace("asidpair: PASS conc role=a", ""),
                       text + "\nasidpair: PASS conc role=a",
                       text.replace("asidpair: PASS reuse nonce=32", ""),
                       text.replace("asidpair: PASS remap generations=3 stale=0", "")):
            with self.assertRaises(RuntimeError):
                validate(broken)

    def test_stale_count_is_rejected_even_without_fail_line(self):
        for broken in (self.fixture(stale_a=1), self.fixture(stale_b=4096)):
            with self.assertRaises(RuntimeError):
                validate(broken)

    def test_live_windows_must_overlap(self):
        with self.assertRaises(RuntimeError):
            validate(self.fixture(begin_b=6_500_000_000, end_a=6_000_000_000, end_b=7_000_000_000))

    def test_peers_must_expect_different_data(self):
        with self.assertRaises(RuntimeError):
            validate(self.fixture(sum_b="11111111"))

    def test_concurrent_processes_must_be_distinct(self):
        with self.assertRaises(RuntimeError):
            validate(self.fixture(koid_b="51"))

    def test_fault_and_duplicate_markers_are_rejected(self):
        text = self.fixture()
        for broken in (text + "\nTOWER (reversed): MICROSERVICE CONTAINED",
                       text + "\nAS1 conc begins",
                       text.replace("autoexec: done", ""),
                       text.replace("INDRA login:", "")):
            with self.assertRaises(RuntimeError):
                validate(broken)

    def test_reuse_must_be_sequential_and_ordered(self):
        text = self.fixture()
        swapped = text.replace("asidpair: PASS reuse nonce=31", "PLACEHOLDER")
        swapped = swapped.replace("asidpair: PASS reuse nonce=32", "asidpair: PASS reuse nonce=31")
        swapped = swapped.replace("PLACEHOLDER", "asidpair: PASS reuse nonce=32")
        with self.assertRaises(RuntimeError):
            validate(swapped)
        with self.assertRaises(RuntimeError):
            validate(text.replace("bin/asidpair: proc koid=0xb", "bin/asidpair: proc koid=0x9"))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.SubprocessError, OSError) as error:
        print(f"AX1 stress FAILED: {error}", file=sys.stderr)
        sys.exit(1)
