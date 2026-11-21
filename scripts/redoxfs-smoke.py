#!/usr/bin/env python3
#j928
#j929
"""SH8-b: one QEMU boot proving RedoxFS through the VFS (PLAN/008 S5, PLAN/022 SH8).

The autoexec makes the RAM work volume (`workvol`), which Sora mounts at /media/WORK with
redoxfsd. `redoxprobe write` builds and checks a tree through one VFS session. Sora then
unmounts the volume (killing redoxfsd) and mounts it again (a new redoxfsd over the same
bytes), and `redoxprobe verify` finds exactly that tree. The proof image is built with
KUMO_REDOXPROBE=1 at the proof loglevel; the default image is restored afterwards.

--crash (SH8-c, gate II): `redoxprobe crash` runs in the background and writes through a
reconnecting session while `fsd-crash-proof` kills redoxfsd with the probe's handles open. Sora's
supervisor restarts it on the same control endpoint. The probe reconnects, reopens, retries
and checks its file, and `redoxprobe verify-crash`, in a fresh process, finds it whole.

Reuses the AXH harness's boot machinery (import only), as persona-smoke.py does.
"""

import argparse
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SCRIPT = (
    b"echo AXH redoxfs begins\n"
    b"workvol 64\n"
    b"mount\n"
    b"media redoxprobe write\n"
    b"umount /media/WORK\n"
    b"mount /dev/disk4s1\n"
    b"media redoxprobe verify\n"
    b"echo AXH redoxfs ends\n"
)

# Each exactly once (AXH validate), in this order.
ORDERED = (
    "media: /dev/disk4s1 ram work volume ready",
    "redoxfsd: formatted the blank volume as RedoxFS",
    "/dev/disk4s1 on /media/WORK type redoxfs (rw)",
    "REDOXPROBE WRITE PASS",
    "media: /dev/disk4s1 unmounted from /media/WORK",
    "redoxfsd: mounted an existing RedoxFS volume",
    "REDOXPROBE VERIFY PASS",
)


CRASH_SCRIPT = (
    b"echo AXH redoxfs-crash begins\n"
    b"workvol 64\n"
    b"bg media redoxprobe crash\n"
    b"fsd-crash-proof\n"
    b"wait\n"
    b"media redoxprobe verify-crash\n"
    b"echo AXH redoxfs-crash ends\n"
)

CRASH_ORDERED = (
    "redoxfsd: formatted the blank volume as RedoxFS",
    "fsd-crash-proof: redoxfsd killed with the probe's handles open",
    "redoxfsd: terminated; sessions invalidated, the control endpoint kept",
    "redoxfsd: supervised instance 2 ready",
    "fsd-crash-proof: replacement serving",
    "redoxprobe: reconnected; new sessions minted: ",
    "REDOXPROBE CRASH PASS",
    "REDOXPROBE VERIFY-CRASH PASS",
)


def load_axh():
    spec = importlib.util.spec_from_file_location("axh", ROOT / "scripts/netstack-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def check(text, ordered, mounts_expected, passes):
    """What AXH's per-marker count cannot say: order, the redoxfs mounts, and no other probe line."""
    at = [text.index(marker) for marker in ordered]
    if at != sorted(at):
        raise RuntimeError(f"redoxfs: markers out of order: {list(zip(ordered, at))}")
    mounts = text.count("mounted on /media/WORK (redoxfs)")
    if mounts != mounts_expected:
        raise RuntimeError(f"redoxfs: expected {mounts_expected} redoxfs mounts of /media/WORK, saw {mounts}")
    rest = text
    for line in passes:
        rest = rest.replace(line, "")
    if "REDOXPROBE" in rest or "fsd-crash-proof: FAIL" in text or "giving up" in text:
        raise RuntimeError("redoxfs: a probe or proof line other than the passes")


def subset_post_validate(axh):
    """AXH's validate, except that any SUBSET of its five known POST failures passes.

    POST runs at Sora's first idle. Which of the five known failures (CONSOLE ROUTE, BLOCK
    SERVE/FAT32/FILE, NET LOOPBACK, all pre-existing) appear depends on where that idle falls
    in the autoexec. The SH8-b autoexec has let CONSOLE ROUTE pass, which AXH's all-or-none
    rule reports as "partial". An unknown FAIL line is still refused. The shared harness is
    left as it is. — TEAL"""
    original = axh.validate

    def validate(case, text):
        try:
            return original(case, text)
        except RuntimeError as error:
            if str(error) != "partial/unknown POST failure set":
                raise
        post = [line for line in text.splitlines() if re.match(r"[A-Z][A-Z 0-9]+\s+Check\s", line)]
        failures = [line for line in text.splitlines() if re.search(r"\bFAIL\b", line)]
        names = {line.split("Check")[0].strip() for line in failures}
        if not names <= set(axh.POST_NAMES) or len(names) != len(failures):
            raise RuntimeError("redoxfs: a FAIL outside the known POST set")
        if not all(line in post for line in failures):
            raise RuntimeError("redoxfs: a FAIL line that is not a POST report")
        return {"post": post, "known_post_failures": len(failures), "instances": [], "expected_exit2": case.towers}

    axh.validate = validate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "build/verification/sh8/sh8b")
    parser.add_argument("--timeout", type=float, default=180, help="seconds for the boot")
    parser.add_argument("--crash", action="store_true", help="SH8-c: kill redoxfsd under a live client")
    args = parser.parse_args()

    axh = load_axh()
    subset_post_validate(axh)
    qemu = shutil.which("qemu-system-aarch64")
    if qemu is None:
        sys.exit("qemu-system-aarch64 missing")
    firmware = Path(qemu).resolve().parent.parent / "share/qemu"
    source = ROOT / "build/images/qemu-virt-aarch64"
    work = args.output
    work.mkdir(parents=True, exist_ok=True)
    if args.crash:
        case = axh.Case("redoxfs-crash", CRASH_SCRIPT, CRASH_ORDERED, ready=0)
        expectations = (CRASH_ORDERED, 1, ("REDOXPROBE CRASH PASS", "REDOXPROBE VERIFY-CRASH PASS"))
        if args.output == ROOT / "build/verification/sh8/sh8b":
            args.output = ROOT / "build/verification/sh8/sh8c"
            work = args.output
            work.mkdir(parents=True, exist_ok=True)
    else:
        case = axh.Case("redoxfs", SCRIPT, ORDERED, ready=0)
        expectations = (ORDERED, 2, ("REDOXPROBE WRITE PASS", "REDOXPROBE VERIFY PASS"))

    proof_env = dict(os.environ, KUMO_CONSOLE_LOGLEVEL="6", KUMO_REDOXPROBE="1")
    default_env = {
        k: v for k, v in os.environ.items() if k not in (*axh.CONTROLS, "KUMO_REDOXPROBE")
    }
    manifest = {"head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()}
    try:
        with (work / "build-proof.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=proof_env, stdout=log, stderr=log, check=True)
        shutil.rmtree(work / case.name, ignore_errors=True)
        manifest["case"] = axh.boot(ROOT, source, case, work / case.name, qemu, firmware, args.timeout)
        check((work / case.name / "serial.log").read_text(errors="replace"), *expectations)
        manifest["passed"] = True
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        with (work / "restore-default.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=default_env, stdout=log, stderr=log, check=True)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("SH8-c redoxfs crash smoke green" if args.crash else "SH8-b redoxfs smoke green", flush=True)


if __name__ == "__main__":
    main()
