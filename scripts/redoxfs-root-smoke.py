#!/usr/bin/env python3
#j932
"""SH8-f (j932): RedoxFS is the root under QEMU (DESIGN/010 §7 items 1 and 3, PLAN/008 M16).

One host disk file, `cargo xtask redoxfs-image --root`: the FHS skeleton plus the pinned Linux
probes. Each boot's ESP manifest names the root by its RedoxFS UUID (`root = uuid:<hex>`), which
Nijigumo carries to Sora in BootInfo's command line.

1. root-1: Sora mounts the disk at `/`. `ls /` lists the skeleton. The Rust std smoke writes a
   tree at `/SH3E` through relibc's ordinary openat. The Linux writeprobe runs at `/tmp` with no
   `--rw` and no `/media`. A reconnecting probe writes through a root session while
   `fsd-crash-proof root` kills the root's redoxfsd. Sora restarts it, the probe finishes, and the
   shell's working directory, entered before the kill, still lists. No RAM FAT scratch server
   starts.
2. root-2 (cold): the std tree and both probe trees read back from the same disk.
3. root-3: the manifest names a UUID no disk has. Sora boots on the initrd root, as before
   SH8-f, leaves the other RedoxFS disk unmounted, and the disk file's bytes do not change. The
   same std suite as root-1 then writes the RAM FAT image, which shows the FAT marker root-1 and
   root-2 must not print.
4. root-4: a copy with every header hash broken (signature and UUID intact). Sora matches
   it, redoxfsd refuses it, the supervisor gives up, and `/` falls back to the initrd with
   no TOWER.

Reuses redoxfs-smoke.py's disk boot (import only).
"""

import argparse
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SEED = "SH8-f root"
WRONG_SEED = "SH8-f no such root"


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def uuid_of(seed):
    return hashlib.sha256(seed.encode()).hexdigest()[:32]


def case(name, body, expected, uuid):
    script = f"echo AXH {name} begins\n".encode() + body + f"echo AXH {name} ends\n".encode()
    return name, script, expected, uuid


UUID = uuid_of(SEED)
WRONG = uuid_of(WRONG_SEED)

CASES = (
    case(
        "root-1",
        b"mount\n"
        b"ls /\n"
        b"ruststd mediasuite\n"
        b"linux /bin/writeprobe\n"
        b"root redoxprobe write\n"
        b"cd /SH3E\n"
        b"bg root redoxprobe crash\n"
        b"fsd-crash-proof root\n"
        b"wait\n"
        b"ls\n",
        (
            f"root: redoxfs uuid:{UUID} mounted at /",
            "/dev/disk5s1 on / type redoxfs (rw)",
            "writeprobe: OK (0 failures)",
            "REDOXPROBE WRITE PASS",
            "fsd-crash-proof: redoxfsd killed with the probe's handles open",
            "fsd-crash-proof: replacement serving",
            "REDOXPROBE CRASH PASS",
        ),
        UUID,
    ),
    case(
        "root-2",
        b"ruststd mediacheck\nroot redoxprobe verify\nroot redoxprobe verify-crash\nmount\n",
        (
            f"root: redoxfs uuid:{UUID} mounted at /",
            "redoxfsd: mounted an existing RedoxFS volume",
            "REDOXPROBE VERIFY PASS",
            "REDOXPROBE VERIFY-CRASH PASS",
            "/dev/disk5s1 on / type redoxfs (rw)",
        ),
        UUID,
    ),
    case(
        "root-3",
        b"mount\nls /\nruststd mediasuite\n",
        (
            f"media: /dev/disk5s1 redoxfs uuid:{UUID} is not the named root; `mount /dev/disk5s1` to use",
            f"root: uuid:{WRONG} not found, staying on initrd",
            "fatfs: scratch image server started (RAM FAT /tmp)",
        ),
        WRONG,
    ),
    case(
        "root-4",
        b"ls /\n",
        ("root: redoxfsd gave up; / is the initrd until reboot",),
        UUID,
    ),
)

# The std smoke's SH3-e lines this proof needs (mediasuite in boot 1, mediacheck in boot 2).
STD_LINES = {
    1: ("e1 mkdir -> OK", "e1 write -> OK", "e1 rename -> OK"),
    2: ("e3 persisted -> OK", "e3 cleanup -> OK"),
    # Without a root the same suite writes the RAM FAT image: the item-3 marker is live.
    3: ("e1 mkdir -> OK", "e1 write -> OK"),
}


def checks(index, text):
    if "redoxfsd: formatted the blank volume as RedoxFS" in text:
        raise RuntimeError(f"root-{index}: Sora formatted a disk it did not create")
    if " FAIL" in "".join(line for line in text.splitlines() if "REDOXPROBE" in line or "-> FAIL" in line):
        raise RuntimeError(f"root-{index}: a probe failed")
    if "writeprobe: FAIL" in text or "fsd-crash-proof: FAIL" in text:
        raise RuntimeError(f"root-{index}: a probe or proof line failed")
    for line in STD_LINES.get(index, ()):
        if line not in text:
            raise RuntimeError(f"root-{index}: std smoke line missing: {line!r}")
    if index in (1, 2):
        if "fatfs: scratch image server started" in text:
            raise RuntimeError(f"root-{index}: a RAM FAT scratch server started under a RedoxFS root")
        if "giving up" in text:
            raise RuntimeError(f"root-{index}: the root mount was given up")
    if index == 1:
        listing = text[text.index("AXH root-1 begins"):]
        for name in ("etc", "home", "tmp", "usr", "var"):
            if not re.search(rf"^{name}/?\s*$", listing, re.M):
                raise RuntimeError(f"root-1: `ls /` does not list {name}")
        after = listing[listing.index("REDOXPROBE CRASH PASS"):]
        if not re.search(r"^FINAL\.TXT\s*$", after, re.M):
            raise RuntimeError("root-1: the working directory did not survive the restart")
        if text.count("redoxfsd: mounted an existing RedoxFS volume") != 2:
            raise RuntimeError("root-1: expected the root mount and the supervised respawn")
    if index == 3:
        if "redoxfsd starting" in text or " on / type" in text:
            raise RuntimeError("root-3: a disk with another UUID reached redoxfsd or became the root")
    if index == 4:
        if text.count("redoxfsd: volume refused") < 2:
            raise RuntimeError("root-4: expected every instance to refuse the volume")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "build/verification/sh8/sh8f")
    parser.add_argument("--timeout", type=float, default=300, help="seconds per boot")
    args = parser.parse_args()

    smoke = load("redoxfs_smoke", ROOT / "scripts/redoxfs-smoke.py")
    axh = smoke.load_axh()
    smoke.subset_post_validate(axh)
    qemu = shutil.which("qemu-system-aarch64")
    if qemu is None:
        sys.exit("qemu-system-aarch64 missing")
    firmware = Path(qemu).resolve().parent.parent / "share/qemu"
    work = args.output
    work.mkdir(parents=True, exist_ok=True)
    proof_env = dict(os.environ, KUMO_CONSOLE_LOGLEVEL="6", KUMO_REDOXPROBE="1")
    default_env = {k: v for k, v in os.environ.items() if k not in (*axh.CONTROLS, "KUMO_REDOXPROBE")}
    manifest = {"head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip()}
    disk = work / "root.img"
    try:
        with (work / "build-proof.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=proof_env, stdout=log, stderr=log, check=True)
        for name, _, _, _ in CASES:
            shutil.rmtree(work / name, ignore_errors=True)
        for stale in ("root.img", "hashless.img"):
            (work / stale).unlink(missing_ok=True)
        made = subprocess.run(
            ["cargo", "xtask", "redoxfs-image", str(disk), "--size", "256", "--uuid", SEED, "--root"],
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout
        if f"uuid {UUID}" not in made:
            raise RuntimeError(f"redoxfs-image printed another UUID: {made!r}")
        results = []
        for index, (name, script, expected, uuid) in enumerate(CASES, start=1):
            target = disk
            if index == 4:
                target = work / "hashless.img"
                shutil.copyfile(disk, target)
                print(f"  root-4: broke {smoke.break_header_hashes(target)} header hashes", flush=True)
            before = hashlib.sha256(target.read_bytes()).hexdigest()
            result, text = smoke.boot_with_disk(
                axh, axh.Case(name, script, expected, ready=0), work / name, qemu, firmware,
                args.timeout, target, manifest_extra=f"root = uuid:{uuid}\n",
            )
            checks(index, text)
            after = hashlib.sha256(target.read_bytes()).hexdigest()
            if index == 3 and before != after:
                raise RuntimeError("root-3: a disk that is not the root was written")
            result.update(disk_sha256_before=before, disk_sha256_after=after, root_uuid=uuid)
            results.append(result)
        manifest["cases"] = results
        manifest["passed"] = True
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        with (work / "restore-default.log").open("wb") as log:
            subprocess.run(axh.IMAGE_ARGS, cwd=ROOT, env=default_env, stdout=log, stderr=log, check=True)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("SH8-f redoxfs root smoke green", flush=True)


if __name__ == "__main__":
    main()
