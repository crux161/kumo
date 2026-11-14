#!/usr/bin/env python3
#j891
"""SH7-d metal: the persona-linux programs on the Orange Pi 5 Plus, one PXE boot.

    linux /bin/m10-hello   static Linux ELF through the in-process shim (SH7-b)
    linux /bin/hello-dyn   dynamic musl program through ld-musl (SH7-c)
    linux /bin/vmprobe     mmap/mprotect/munmap semantics (SH7-d1)
    linux /bin/greet       NEEDED libgreet.so, found by musl in /lib (SH7-d2)
    linux /bin/fileprobe   the Linux file surface (SH7-d2)
    linux /bin/bigread     bulk read/mmap/pread of a 6 MiB file through vfs ReadVmo (SH7-d3a)

The metal half of scripts/persona-smoke.py. Reuses scripts/metal-sh3e.py's board machinery
(import only), like the other metal suites. Prerequisites, per the opi5 serial-loop recipe:
serial adapter at /dev/tty.usbserial-B0001AH9, no other tio or pxehost on the port, pxehost at
build/tools/pxehost-kumo, and the image built with KUMO_CONSOLE_LOGLEVEL=6. tio stamps every
line, so the bigread phase lines time the bulk reads. — TEAL
"""

import argparse
import importlib.util
import json
import re
import shutil
import sys
import time
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

BOOT = (
    b"echo SH7D begins\n"
    b"linux /bin/m10-hello\n"
    b"linux /bin/hello-dyn\n"
    b"linux /bin/vmprobe\n"
    b"linux /bin/greet\n"
    b"linux /bin/fileprobe\n"
    b"linux /bin/bigread\n"
    b"echo SH7D ends\n"
)
EXPECTED = (
    "M10 elf linux hi",
    "linux: /bin/m10-hello exited 0",
    "SH7-c dynamic musl hello: argc=1 argv0=/bin/hello-dyn",
    "linux: /bin/hello-dyn exited 0",
    "vmprobe: OK (0 failures)",
    "linux: /bin/vmprobe exited 0",
    "greet: hello from libgreet.so",
    "linux: /bin/greet exited 0",
    "fileprobe: OK (0 failures)",
    "linux: /bin/fileprobe exited 0",
    "bigread: OK (0 failures)",
    "linux: /bin/bigread exited 0",
)
PHASES = (
    "bigread: size",
    "bigread: read() done",
    "bigread: mmap() done",
    "bigread: OK",
)


def load_board():
    spec = importlib.util.spec_from_file_location("sh3e", ROOT / "scripts/metal-sh3e.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def phase_times(text):
    """Seconds between bigread's phase lines, from tio's `[HH:MM:SS.mmm]` stamps."""
    stamps = []
    for marker in PHASES:
        match = re.search(r"^\[(\d\d:\d\d:\d\d\.\d+)\] " + re.escape(marker), text, re.M)
        if match is None:
            return None
        stamps.append(datetime.strptime(match.group(1), "%H:%M:%S.%f"))
    return {
        "read_s": (stamps[1] - stamps[0]).total_seconds(),
        "mmap_s": (stamps[2] - stamps[1]).total_seconds(),
        "pread_s": (stamps[3] - stamps[2]).total_seconds(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/sh7/sh7d-metal")
    parser.add_argument("--timeout", type=float, default=240.0)
    args = parser.parse_args()

    board = load_board()
    if shutil.which("tio") is None:
        sys.exit("tio not on PATH")
    if not board.PXEHOST.is_file():
        sys.exit(f"missing {board.PXEHOST}; build with scripts/build-pxehost-kumo.sh")
    if not board.SOURCE_IMAGE.is_dir():
        sys.exit(
            f"missing {board.SOURCE_IMAGE}; run KUMO_CONSOLE_LOGLEVEL=6 cargo xtask image "
            "--arch aarch64 --hardware orange-pi-5-plus"
        )

    work = args.work
    work.mkdir(parents=True, exist_ok=True)
    tftp = work / "tftp"
    board.stage_tree(tftp)

    manifest = {"pxehost": str(board.PXEHOST)}
    board.stage_boot(tftp, BOOT)
    pxehost = board.start_pxehost(tftp, work / "pxehost.log")
    tio_pid, master = board.start_tio(work / "boot.log")
    try:
        board.wake_board(work / "boot.log")
        board.wait_for("SH7D ends", work / "boot.log", args.timeout)
        # The login prompt after autoexec must come AFTER the end marker (a stale pre-reset
        # prompt would otherwise pass); prompts lack newlines, so read the raw capture.
        deadline = time.monotonic() + 60
        while True:
            raw = board.raw_text_since(work / "boot.log", 0)
            if "SH7D ends" in raw and "INDRA login:" in raw[raw.rindex("SH7D ends"):]:
                break
            if time.monotonic() > deadline:
                raise RuntimeError("no login prompt after SH7D ends")
            time.sleep(0.2)
        text = board.log_whole(work / "boot.log")
        board.validate(text, EXPECTED, "SH7D begins", "SH7D ends", login_text=raw)
        window = text[text.index("SH7D begins"):text.index("SH7D ends")]
        if "TOWER" in window:
            raise RuntimeError("a TOWER inside the SH7D window")
        unsupported = sorted(set(re.findall(r"syscall (\d+) unsupported", window)))
        manifest["unsupported_syscalls"] = unsupported
        manifest["bigread"] = phase_times(text)
        manifest["boot"] = "PASS"
        manifest["passed"] = True
        print(f"boot: PASS bigread={manifest['bigread']} unsupported={unsupported}", flush=True)
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        board.stop_all(pxehost, tio_pid, master)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("SH7-d metal suite green", flush=True)


if __name__ == "__main__":
    main()
