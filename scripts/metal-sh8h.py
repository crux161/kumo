#!/usr/bin/env python3
#j934
"""SH8-h (j934): SH7-g's Cargo workspace builds on the opi5's NVMe root and survives redoxfsd
being killed mid-build (PLAN/022 SH8's checkout half, on metal; SH8-e proved it on QEMU).

--reimage [--go]: one PXE boot of the image built with KUMO_NVME_PREPARE_ROOT=reimage
    KUMO_NVME_ROOT_TREE=cargo KUMO_PERSONA_SDK_SOURCE=<SH7-g SDK> KUMO_CONSOLE_LOGLEVEL=6.
    Nijigumo fetches the zstd extent file, checks it, prints the GPT preimage and partition 1's
    raw hashes, and waits for the receipt. Only --go sends it (D4: after CRUX's go). After the
    four phases partition 1's hashes must be unchanged (closing j933's G-e1 gap).

--cargo: three PXE boots of the image built with KUMO_REDOXPROBE=1 KUMO_CONSOLE_LOGLEVEL=6, `/`
    on the NVMe root by UUID, every Linux file (toolchain, Cargo home, TMPDIR, output) on it.
    1. B1: a clean Cargo build; objprobe hashes the binary against SH7-g's Linux oracle and Sora
       runs it; then `reboot` (clean NV8 shutdown), and the harness swaps in B2's tree.
    2. B2: `cargo clean`, then the same build in the background with `fsd-crash-proof root` armed
       on the proc macro's dep-info file, the final binary required absent. Exactly one restart,
       the shim reconnects, the same Cargo run finishes; hash and run; `reboot`.
    3. B3 (cold): the B2 artifact hashes and runs with no build; J804's FAT seed still reads.

Reuses scripts/metal-sh8g.py (and through it metal-sh3e.py's board machinery). — TEAL
"""

import argparse
import importlib.util
import json
import re
import shutil
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
import persona_rustc_proof as oracle  # noqa: E402

CARGO = (b"linux /opt/rust/bin/cargo -Zhost-config -Ztarget-applies-to-host -Zjson-target-spec build "
         b"--offline --release --target /sdk/share/kumo/aarch64-unknown-kumo.json\n")
VERIFY = b"linux /bin/objprobe " + oracle.CARGO_BIN + b"\nrun " + oracle.CARGO_BIN + b"\n"
ARMED = b"/out/target/release/deps/message-d6b32edc9b72d2ca.d"

B1 = (b"echo SH8H B1 begins\nmount\necho SH8H timing build begins\n"
      b"env " + oracle.CARGO_ENV + b" " + CARGO
      + b"echo SH8H timing build ends\n" + VERIFY + b"echo SH8H B1 ends\nreboot\n")
B2 = (b"echo SH8H B2 begins\nmount\n"
      b"env " + oracle.CARGO_ENV + b" linux /opt/rust/bin/cargo clean --offline\n"
      b"echo SH8H timing build begins\n"
      b"env " + oracle.CARGO_ENV + b" bg " + CARGO
      + b"fsd-crash-proof root " + ARMED + b" 1 " + oracle.CARGO_BIN + b"\nwait\n"
      + b"echo SH8H timing build ends\n" + VERIFY + b"echo SH8H B2 ends\nreboot\n")
B3 = (b"echo SH8H B3 begins\nmount\n" + VERIFY
      + b"mount /dev/disk3s1\ncat /media/KUMONVME/HELLO.TXT\necho SH8H B3 ends\n")

BUILT = ("Compiling message v0.1.0", "Compiling sh7-hello v0.1.0", "Finished `release` profile")
RAN = (oracle.CARGO_OBJECT, oracle.CARGO_HELLO, "linux: /bin/objprobe exited 0")
B1_EXPECTED = ("/dev/disk3s2 on / type redoxfs (rw)", *BUILT, "linux: /opt/rust/bin/cargo exited 0", *RAN)
B2_EXPECTED = ("/dev/disk3s2 on / type redoxfs (rw)", *BUILT, *RAN,
               "fsd-crash-proof: final artifact absent before kill",
               "fsd-crash-proof: observed " + ARMED.decode(),
               "fsd-crash-proof: redoxfsd killed with the probe's handles open",
               "redoxfsd: supervised instance 2 ready",
               "wait: background job finished")
B3_EXPECTED = ("/dev/disk3s2 on / type redoxfs (rw)", *RAN, "KUMO NV7 NVMe FAT32 proof.")
RECONNECT = "persona-linux: filesystem session lost;"


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def window(sh8g, text, name):
    begin, end = f"SH8H {name} begins", f"SH8H {name} ends"
    if begin not in text or end not in text:
        raise RuntimeError(f"no complete {name} window")
    return sh8g.live(text[text.rindex(begin):text.rindex(end)])


def check(sh8g, text, name, expected):
    w = window(sh8g, text, name)
    for marker in expected:
        if marker not in w:
            raise RuntimeError(f"{name}: missing {marker!r}")
    if "TOWER" in w:
        raise RuntimeError(f"{name}: a TOWER inside the window")
    for bad in ("fsd-crash-proof: FAIL", "redoxfsd: formatted", "giving up"):
        if bad in w:
            raise RuntimeError(f"{name}: {bad!r}")
    return w


def build_seconds(text, name):
    w = text[text.rindex(f"SH8H {name} begins"):]
    times = re.findall(r"^\[(\d\d):(\d\d):(\d\d\.\d+)\] SH8H timing build (begins|ends)", w, re.M)
    if len(times) < 2:
        return None
    (h0, m0, s0, _), (h1, m1, s1, _) = times[0], times[1]
    return (int(h1) * 3600 + int(m1) * 60 + float(s1)) - (int(h0) * 3600 + int(m0) * 60 + float(s0))


def fat_receipt(text, when):
    """Partition 1's (first8, seed) hashes from the firmware's two receipt lines."""
    found = [re.findall(rf"SH8G FATSHA {when} {part} ([0-9a-f]{{64}})", text) for part in ("first8", "seed")]
    return (found[0][-1], found[1][-1]) if all(found) else None


def run_reimage(sh8g, board, work, go):
    # About 1.13 M sectors through firmware Block I/O, written then read back: allow 2 h.
    manifest = sh8g.run_prepare(board, work, go, reimage=True, verdict_timeout=1800,
                                complete_timeout=7200)
    text = board.log_whole(work / "prepare.log")
    manifest["root_extents"] = re.findall(r"root extents : [^\r\n]*", text)
    before = fat_receipt(text, "before")
    if before is None:
        raise RuntimeError("no partition-1 receipt before the writes")
    manifest["fat_before"] = before
    if go:
        after = fat_receipt(text, "after")
        if after != before or "SH8G FATSHA partition 1 unchanged" not in text:
            raise RuntimeError("partition 1 changed or its receipt after the writes is missing")
        manifest["fat_after"] = after
    print(f"  re-image: {manifest.get('complete')} {manifest['root_extents']} fat={manifest['fat_before'][0][:16]}...", flush=True)
    return manifest


def run_cargo(sh8g, board, work, timeout, start="B1"):
    """`start`: B2 reruns the restart build and the cold boot over the existing root, whose
    B1 artifact the restart build replaces (it begins with `cargo clean`)."""
    boots = [name for name in ("B1", "B2", "B3") if name >= start]
    scripts = {"B1": B1, "B2": B2, "B3": B3}
    trees = {name: sh8g.stage(board, work, name, scripts[name], sh8g.UUID) for name in boots}
    served = work / "tftp-served"
    if served.exists():
        shutil.rmtree(served)
    trees[boots[0]].rename(served)
    log = work / "cargo.log"
    manifest = {"uuid": sh8g.UUID}
    pxehost = board.start_pxehost(served, work / "pxehost-cargo.log")
    tio_pid, master = board.start_tio(log)
    try:
        sh8g.wake(board, log)
        for name, following in zip(boots, boots[1:]):
            board.wait_for(f"SH8H {name} ends", log, timeout)
            board.wait_for("rebooting", log, 300)
            served.rename(work / f"tftp-{name}-done")
            trees[following].rename(served)
        board.wait_for("SH8H B3 ends", log, timeout)
        text = board.log_whole(log)
        if "B1" in boots:
            check(sh8g, text, "B1", B1_EXPECTED)
        w2 = check(sh8g, text, "B2", B2_EXPECTED)
        check(sh8g, text, "B3", B3_EXPECTED)
        for name in boots[:-1]:
            tail = sh8g.live(text[text.rindex(f"SH8H {name} ends"):])
            for marker in ("root: redoxfsd stopped for the NVMe shutdown", "NV8 SHUTDOWN", "rebooting"):
                if marker not in tail:
                    raise RuntimeError(f"{name} reboot: missing {marker!r}")
        restarts = w2.count("redoxfsd: terminated; sessions invalidated")
        if restarts != 1:
            raise RuntimeError(f"B2: expected exactly one redoxfsd restart, saw {restarts}")
        kill, finished = w2.index("fsd-crash-proof: redoxfsd killed"), w2.index("Finished `release` profile")
        reconnects = [m.start() for m in re.finditer(re.escape(RECONNECT), w2)]
        if not reconnects or not all(kill < at < finished for at in reconnects):
            raise RuntimeError("B2: the running Cargo tree did not reconnect inside the build")
        ordered = ["fsd-crash-proof: observed", "redoxfsd killed", "redoxfsd: supervised instance 2 ready",
                   "Finished `release` profile", oracle.CARGO_OBJECT, oracle.CARGO_HELLO]
        at = [w2.index(m) for m in ordered]
        if at != sorted(at):
            raise RuntimeError("B2: build/recovery/verification out of order")
        manifest.update(boots=boots,
                        build_s=build_seconds(text, "B1") if "B1" in boots else None,
                        restart_build_s=build_seconds(text, "B2"),
                        shim_reconnects=len(reconnects),
                        bulk=re.findall(r"NV7 CLIENT session=\d+ slot=\d+ volume=2 closed[^\n]*", sh8g.live(text))[-3:])
        clean = f"{manifest['build_s']:.0f}s" if manifest["build_s"] else "(earlier run)"
        print(f"  B1 clean build {clean}, B2 restart build {manifest['restart_build_s']:.0f}s "
              f"({len(reconnects)} reconnects), B3 cold: PASS", flush=True)
        return manifest
    finally:
        board.stop_all(pxehost, tio_pid, master)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/j934/metal")
    parser.add_argument("--reimage", action="store_true")
    parser.add_argument("--go", action="store_true", help="send the re-image receipt (CRUX said go)")
    parser.add_argument("--cargo", action="store_true")
    parser.add_argument("--timeout", type=float, default=1800, help="seconds per boot (D5: 30 min)")
    parser.add_argument("--from", dest="start", choices=("B1", "B2"), default="B1")
    args = parser.parse_args()
    sh8g = load("sh8g", ROOT / "scripts/metal-sh8g.py")
    board = sh8g.load_board()
    if shutil.which("tio") is None or not board.PXEHOST.is_file() or not board.SOURCE_IMAGE.is_dir():
        sys.exit("needs tio, build/tools/pxehost-kumo and build/images/orange-pi-5-plus")
    work = args.work
    work.mkdir(parents=True, exist_ok=True)
    name = "reimage" if args.reimage else "cargo"
    manifest = {"mode": name}
    try:
        manifest.update(run_reimage(sh8g, board, work, args.go) if args.reimage
                        else run_cargo(sh8g, board, work, args.timeout, args.start))
        manifest["passed"] = True
    except BaseException as error:
        manifest.update(passed=False, error=str(error))
        raise
    finally:
        (work / f"manifest-{name}.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"SH8-h metal {name} green", flush=True)


if __name__ == "__main__":
    main()
