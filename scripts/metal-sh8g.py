#!/usr/bin/env python3
#j933
#j934
"""SH8-g (j933): the RedoxFS root on the Orange Pi 5 Plus's NVMe (DESIGN/010 §7 item 4).

--prepare: one PXE boot of the image built with KUMO_NVME_PREPARE_ROOT=1 (and
    KUMO_CONSOLE_LOGLEVEL=6). Nijigumo checks the SSD's identity and J804 layout, prints the GPT
    as it is (SH8G BAK lines, checked against SH8G BAKSHA and saved as gpt-before.bin), and waits
    for the host's receipt. Only with --go does this script send it (D8: TEAL sends it after
    CRUX's go for that boot). Without --go the step times out and writes nothing. Then KUMO boots
    and drv-nvme must report the RedoxFS partition and offer it as volume 2.

--root: three PXE boots of the image built with KUMO_REDOXPROBE=1 KUMO_CONSOLE_LOGLEVEL=6, each
    with its own `root = uuid:` line and autoexec in a private TFTP tree.
    1. root-1: `/` is the NVMe root. SH8-f's workload runs (std, Linux writeprobe, redoxprobe,
       a redoxfsd kill under a reconnecting writer, the cwd surviving), then the J804 FAT volume
       mounts beside it. Then `reboot`: Sora stops the root's redoxfsd, drv-nvme flushes and
       shuts the controller down (NV8), and PSCI resets. The harness swaps in root-2's tree
       while the board resets.
    2. root-2 (after that clean shutdown): every tree reads back.
    3. root-3 (reset from the login prompt): a UUID no disk has. Sora stays on the initrd and
       never starts redoxfsd on the NVMe root.

Reuses scripts/metal-sh3e.py's board machinery (import only), like the other metal suites.
— TEAL
"""

import argparse
import hashlib
import importlib.util
import json
import re
import shutil
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UUID = hashlib.sha256(b"SH8-g opi5 NVMe root").hexdigest()[:32]
WRONG = hashlib.sha256(b"SH8-g no such root").hexdigest()[:32]

PREPARE_BOOT = b"echo SH8G P begins\nmount\necho SH8G P ends\n"

R1 = (
    b"echo SH8G R1 begins\n"
    b"mount\n"
    b"ls /\n"
    b"ruststd mediasuite\n"
    b"linux /bin/writeprobe\n"
    b"echo SH8G timing write begins\n"
    b"root redoxprobe write\n"
    b"echo SH8G timing write ends\n"
    b"cd /SH3E\n"
    b"bg root redoxprobe crash\n"
    b"fsd-crash-proof root\n"
    b"wait\n"
    b"ls\n"
    b"mount /dev/disk3s1\n"
    b"cat /media/KUMONVME/HELLO.TXT\n"
    b"echo SH8G R1 ends\n"
    b"reboot\n"
)
R1_BOOT_EXPECTED = (f"root: redoxfs uuid:{UUID} mounted at /",)
R1_EXPECTED = (
    "/dev/disk3s2 on / type redoxfs (rw)",
    "writeprobe: OK (0 failures)",
    "REDOXPROBE WRITE PASS",
    "fsd-crash-proof: redoxfsd killed with the probe's handles open",
    "fsd-crash-proof: replacement serving",
    "REDOXPROBE CRASH PASS",
    "KUMO NV7 NVMe FAT32 proof.",
)
R2 = (
    b"echo SH8G R2 begins\n"
    b"mount\n"
    b"echo SH8G timing verify begins\n"
    b"ruststd mediacheck\n"
    b"root redoxprobe verify\n"
    b"root redoxprobe verify-crash\n"
    b"echo SH8G timing verify ends\n"
    b"fsd-header-proof /dev/disk3s2\n"
    b"echo SH8G R2 ends\n"
)
R2_BOOT_EXPECTED = R1_BOOT_EXPECTED
R2_EXPECTED = (
    "/dev/disk3s2 on / type redoxfs (rw)",
    "REDOXPROBE VERIFY PASS",
    "REDOXPROBE VERIFY-CRASH PASS",
)
R3 = b"echo SH8G R3 begins\nmount\nls /\nfsd-header-proof /dev/disk3s2\necho SH8G R3 ends\n"
R3_BOOT_EXPECTED = (
    f"root: uuid:{WRONG} not found, staying on initrd",
    f"media: /dev/disk3s2 redoxfs uuid:{UUID} is not the named root; `mount /dev/disk3s2` to use",
)


def load_board():
    spec = importlib.util.spec_from_file_location("sh3e", ROOT / "scripts/metal-sh3e.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def window(text, name):
    begin, end = f"SH8G {name} begins", f"SH8G {name} ends"
    if text.count(begin) < 1 or end not in text:
        raise RuntimeError(f"no complete {name} window")
    # Live lines only: the console replays buffered kernel lines with a second timestamp.
    return text[text.rindex(begin):text.rindex(end)]


def live(text):
    return "\n".join(line for line in text.splitlines() if not re.match(r"^\[[0-9:.]+\] \[ *\d+\.\d+\]", line))


def stamp(text, marker):
    found = re.findall(r"^\[(\d\d):(\d\d):(\d\d\.\d+)\] " + re.escape(marker), text, re.M)
    if not found:
        return None
    h, m, s = found[-1]
    return int(h) * 3600 + int(m) * 60 + float(s)


def check(text, name, expected, forbidden=(), boot_expected=()):
    w = live(window(text, name))
    if boot_expected:
        begin = text.rindex(f"SH8G {name} begins")
        # Mount selection precedes autoexec. Bound it to this boot, so a previous
        # successful root mount cannot satisfy the next boot's proof. — KESTREL
        start = text.rfind("root: boot command line", 0, begin)
        if start < 0:
            raise RuntimeError(f"{name}: no boot command-line marker")
        prefix = live(text[start:begin])
        for marker in boot_expected:
            if marker not in prefix:
                raise RuntimeError(f"{name}: missing boot marker {marker!r}")
        if "TOWER" in prefix:
            raise RuntimeError(f"{name}: a TOWER before autoexec")
    if "TOWER" in w:
        raise RuntimeError(f"{name}: a TOWER inside the window")
    for marker in expected:
        if marker not in w:
            raise RuntimeError(f"{name}: missing {marker!r}")
    for marker in forbidden:
        if marker in w:
            raise RuntimeError(f"{name}: forbidden {marker!r}")
    for line in w.splitlines():
        if ("REDOXPROBE" in line and "FAIL" in line) or "-> FAIL" in line or "fsd-crash-proof: FAIL" in line:
            raise RuntimeError(f"{name}: {line.strip()}")
    return w


def stage(board, work, name, script, uuid):
    tftp = work / f"tftp-{name}"
    board.stage_tree(tftp)
    board.stage_boot(tftp, script)
    if uuid is not None:
        conf = tftp / "EFI/KUMO/nijigumo.conf"
        conf.write_text(conf.read_text() + f"root = uuid:{uuid}\n")
    return tftp


def gpt_receipt(text):
    """The SH8G BAK dump as sectors, checked against SH8G BAKSHA."""
    sectors = {}
    for lba, offset, hexes in re.findall(r"SH8G BAK (\d+) (\d+) ([0-9a-f]{64})", text):
        sectors.setdefault(int(lba), bytearray(512))[int(offset):int(offset) + 32] = bytes.fromhex(hexes)
    want = re.findall(r"SH8G BAKSHA ([0-9a-f]{64})", text)
    if len(sectors) != 67 or not want:
        raise RuntimeError(f"incomplete GPT receipt: {len(sectors)} sectors")
    sha = hashlib.sha256()
    for lba in sorted(sectors):
        sha.update(lba.to_bytes(8, "little"))
        sha.update(sectors[lba])
    if sha.hexdigest() != want[-1]:
        raise RuntimeError("GPT receipt does not match SH8G BAKSHA")
    return sectors, want[-1]


def wake(board, log):
    """The shared waker, except that a board already in its firmware boot (the BdsWait countdown
    or the splash) is left to PXE-boot by itself."""
    try:
        board.wake_board(log)
    except RuntimeError as error:
        raw = board.capture_text(log)
        if "console state is unknown" in str(error) and ("BdsWait" in raw or "NimbusSplash" in raw):
            return
        raise


def run_prepare(board, work, go, reimage=False, wait_for_boot=False, verdict_timeout=400,
                complete_timeout=900):
    tftp = stage(board, work, "prepare", PREPARE_BOOT, None)
    # j934: SH8-h's large tree travels as root-nvme.ext.zst; the timeout covers its fetch.
    if not any((tftp / f"EFI/KUMO/{name}").is_file() for name in ("root-nvme.ext", "root-nvme.ext.zst")):
        raise RuntimeError("the staged image has no root extent file; build with KUMO_NVME_PREPARE_ROOT=1")
    log = work / "prepare.log"
    manifest = {}
    pxehost = board.start_pxehost(tftp, work / "pxehost-prepare.log")
    tio_pid, master = board.start_tio(log)
    try:
        if wait_for_boot:
            print("Private PXE server and serial capture ready; waiting for target reset", flush=True)
        else:
            wake(board, log)
        deadline = time.monotonic() + verdict_timeout
        while True:
            raw = board.capture_text(log)
            if "SH8G PREP waiting for host receipt" in raw or "SH8G PREP already prepared" in raw:
                break
            if "SH8G PREP refused" in raw or "SH8G PREP failed" in raw:
                raise RuntimeError("prepare refused: " + re.findall(r"SH8G PREP (?:refused|failed)[^\r\n]*", raw)[-1])
            if time.monotonic() > deadline:
                raise RuntimeError(f"no SH8G PREP verdict; see {log}")
            time.sleep(0.5)
        if "SH8G PREP waiting for host receipt" in raw:
            sectors, sha = gpt_receipt(raw)
            with (work / "gpt-before.bin").open("wb") as out:
                for lba in sorted(sectors):
                    out.write(lba.to_bytes(8, "little"))
                    out.write(sectors[lba])
            manifest["gpt_before_sha256"] = sha
            manifest["extent"] = re.findall(r"SH8G EXT [^\r\n]*", raw)[-1]
            print(f"  prepare: GPT receipt saved ({sha}); {manifest['extent']}", flush=True)
            if not go:
                manifest["written"] = False
                print("  prepare: no --go; the step times out with zero writes", flush=True)
                board.wait_for("SH8G PREP receipt timeout", log, 200)
                return manifest
            time.sleep(1.0)
            if reimage and "SH8G PREP re-image build" not in board.capture_text(log):
                raise RuntimeError("--reimage, but the firmware did not report a re-image build")
            board.send_raw(b"K933-REIMAGE\r" if reimage else b"K933-ROOT\r")
            board.wait_for("SH8G PREP complete", log, complete_timeout)
            manifest["written"] = True
        else:
            manifest["written"] = False
        board.wait_for("SH8G P ends", log, 400)
        text = board.log_whole(log)
        manifest["phases"] = re.findall(r"SH8G PREP phase \d/4 flushed and verified", text)
        manifest["complete"] = re.findall(r"SH8G PREP complete[^\r\n]*", text)
        w = live(window(text, "P"))
        boot = live(text)
        for marker in ("verdict=redoxfs", "offered as volume 2", "read-write; offered as volume 2"):
            if marker not in boot:
                raise RuntimeError(f"prepare: drv-nvme did not report {marker!r}")
        manifest["volume"] = re.findall(r"SH8G VOLUME [^\r\n]*", boot)[-1]
        manifest["mount"] = [l for l in w.splitlines() if "/dev/disk3" in l]
        print(f"  prepare: {manifest['volume']}", flush=True)
        return manifest
    finally:
        board.stop_all(pxehost, tio_pid, master)


def run_root(board, work):
    trees = {
        "R1": stage(board, work, "R1", R1, UUID),
        "R2": stage(board, work, "R2", R2, UUID),
        "R3": stage(board, work, "R3", R3, WRONG),
    }
    served = work / "tftp-served"
    if served.exists():
        shutil.rmtree(served)
    trees["R1"].rename(served)
    log = work / "root.log"
    manifest = {"uuid": UUID, "wrong": WRONG}
    pxehost = board.start_pxehost(served, work / "pxehost-root.log")
    tio_pid, master = board.start_tio(log)
    try:
        wake(board, log)
        board.wait_for("SH8G R1 ends", log, 900)
        board.wait_for("rebooting", log, 120)
        # Swap in root-2's tree while the board resets (pxehost opens files per request).
        served.rename(work / "tftp-R1-done")
        trees["R2"].rename(served)
        board.wait_for("SH8G R2 ends", log, 900)
        deadline = time.monotonic() + 120
        while "INDRA login:" not in board.raw_text_since(log, 0).rpartition("SH8G R2 ends")[2]:
            if time.monotonic() > deadline:
                raise RuntimeError("no login prompt after root-2")
            time.sleep(0.5)
        text = board.log_whole(log)
        w1 = check(text, "R1", R1_EXPECTED, boot_expected=R1_BOOT_EXPECTED)
        if not re.search(r"^\[[0-9:.]+\] FINAL\.TXT\s*$", w1.partition("REDOXPROBE CRASH PASS")[2], re.M):
            raise RuntimeError("R1: the working directory did not survive the restart")
        tail = live(text[text.rindex("SH8G R1 ends"):text.rindex("SH8G R2 begins")])
        for marker in ("root: redoxfsd stopped for the NVMe shutdown", "NV8 SHUTDOWN", "rebooting"):
            if marker not in tail:
                raise RuntimeError(f"R1 reboot: missing {marker!r}")
        w2 = check(text, "R2", R2_EXPECTED, forbidden=("redoxfsd: formatted",), boot_expected=R2_BOOT_EXPECTED)
        header_before = re.findall(r"SH8G HEADER /dev/disk3s2 sha256 ([0-9a-f]{64})", w2)
        if len(header_before) != 1:
            raise RuntimeError("R2: missing/duplicate root header receipt")
        manifest["write_s"] = (stamp(text, "SH8G timing write ends") or 0) - (stamp(text, "SH8G timing write begins") or 0)
        manifest["verify_s"] = (stamp(text, "SH8G timing verify ends") or 0) - (stamp(text, "SH8G timing verify begins") or 0)
        print(f"  root-1 + reboot + root-2: PASS (redoxprobe write {manifest['write_s']:.1f}s, verify {manifest['verify_s']:.1f}s)", flush=True)
        # root-3: reset from the login prompt into the wrong-UUID tree.
        served.rename(work / "tftp-R2-done")
        trees["R3"].rename(served)
        wake(board, log)
        board.wait_for("SH8G R3 ends", log, 900)
        text = board.log_whole(log)
        w3 = check(text, "R3", (), forbidden=(" on / type", "mounted at /"), boot_expected=R3_BOOT_EXPECTED)
        boot3 = live(text[text.rindex("SH8G R2 ends"):])
        if "redoxfsd starting" in boot3:
            raise RuntimeError("R3: redoxfsd started on a disk that is not the named root")
        header_after = re.findall(r"SH8G HEADER /dev/disk3s2 sha256 ([0-9a-f]{64})", w3)
        if header_after != header_before:
            raise RuntimeError("R3: root header ring changed or receipt missing")
        manifest["header_ring_sha256"] = header_before[0]
        print("  root-3: PASS (wrong UUID: initrd root, root header ring unchanged)", flush=True)
        manifest["passed"] = True
        return manifest
    finally:
        board.stop_all(pxehost, tio_pid, master)


def run_bench(board, work, mode):
    # Each mode gets a fresh boot/cache and reads the same persisted tree. — KESTREL
    body = (b"echo SH8G B begins\nmount\n"
            b"echo SH8G timing verify begins\nroot redoxprobe verify\n"
            b"echo SH8G timing verify ends\necho SH8G B ends\n")
    tree = stage(board, work, "bench", body, UUID)
    log = work / "bench.log"
    server = board.start_pxehost(tree, work / "pxehost-bench.log")
    tio_pid, master = board.start_tio(log)
    try:
        wake(board, log)
        board.wait_for("SH8G B ends", log, 1800)
        text = board.log_whole(log)
        check(text, "B", ("REDOXPROBE VERIFY PASS",),
              boot_expected=(f"root: redoxfs uuid:{UUID} mounted at /", f"SH8G NVMe mode {mode}"))
        result = {"mode": mode}
        for phase in ("verify",):
            begin = stamp(text, f"SH8G timing {phase} begins")
            end = stamp(text, f"SH8G timing {phase} ends")
            if begin is None or end is None or end < begin:
                raise RuntimeError(f"benchmark {phase}: missing or unordered timestamps")
            result[phase + "_s"] = end - begin
        print(f"  {mode}: PASS (cold verify {result['verify_s']:.3f}s)", flush=True)
        return result
    finally:
        board.stop_all(server, tio_pid, master)


def self_test():
    import unittest

    class BootWindowTests(unittest.TestCase):
        def test_root_mount_before_autoexec_is_accepted(self):
            text = "root: boot command line\nmounted root\nSH8G R1 begins\nworkload OK\nSH8G R1 ends\n"
            check(text, "R1", ("workload OK",), boot_expected=("mounted root",))

        def test_previous_boot_cannot_supply_mount(self):
            text = ("root: boot command line\nmounted root\nSH8G R1 begins\nSH8G R1 ends\n"
                    "root: boot command line\nSH8G R2 begins\nworkload OK\nSH8G R2 ends\n")
            with self.assertRaisesRegex(RuntimeError, "missing boot marker"):
                check(text, "R2", ("workload OK",), boot_expected=("mounted root",))

        def test_real_loader_fault_remains_a_failure(self):
            text = "root: boot command line\nmounted root\nSH8G R1 begins\nTOWER\nworkload OK\nSH8G R1 ends\n"
            with self.assertRaisesRegex(RuntimeError, "a TOWER"):
                check(text, "R1", ("workload OK",), boot_expected=("mounted root",))

    result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(BootWindowTests))
    if not result.wasSuccessful():
        raise SystemExit(1)


def main():
    if sys.argv[1:] == ["--self-test"]:
        self_test()
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/j933/metal")
    parser.add_argument("--prepare", action="store_true")
    parser.add_argument("--go", action="store_true", help="send the receipt token (CRUX said go)")
    parser.add_argument("--reimage", action="store_true", help="the image was built with KUMO_NVME_PREPARE_ROOT=reimage")
    parser.add_argument("--root", action="store_true")
    parser.add_argument("--bench", choices=("bulk", "sector"), help="measure the same probe workload; mode must match the built driver")
    parser.add_argument("--wait-for-boot", action="store_true", help="prepare: wait for a manual reset without sending console commands")
    args = parser.parse_args()
    board = load_board()
    if shutil.which("tio") is None or not board.PXEHOST.is_file() or not board.SOURCE_IMAGE.is_dir():
        sys.exit("needs tio, build/tools/pxehost-kumo and build/images/orange-pi-5-plus")
    work = args.work
    work.mkdir(parents=True, exist_ok=True)
    name = "bench-" + args.bench if args.bench else "prepare" if args.prepare else "root"
    manifest = {"mode": name}
    try:
        manifest.update(run_bench(board, work, args.bench) if args.bench else
                        run_prepare(board, work, args.go, args.reimage, args.wait_for_boot) if args.prepare else run_root(board, work))
        manifest["passed"] = True
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        (work / f"manifest-{name}.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"SH8-g metal {name} green", flush=True)


if __name__ == "__main__":
    main()
