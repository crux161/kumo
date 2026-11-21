#!/usr/bin/env python3
#j919
#j920
#j922
#j924
#j925
"""SH7-d metal: the persona-linux programs on the Orange Pi 5 Plus, one PXE boot.

    linux /bin/m10-hello   static Linux ELF through the in-process shim (SH7-b)
    linux /bin/hello-dyn   dynamic musl program through ld-musl (SH7-c)
    linux /bin/vmprobe     mmap/mprotect/munmap semantics (SH7-d1)
    linux /bin/greet       NEEDED libgreet.so, found by musl in /lib (SH7-d2)
    linux /bin/fileprobe   the Linux file surface (SH7-d2)
    linux /bin/writeprobe  the write surface over the image's read-write session (SH7-e1)
    linux /bin/bigread     bulk read/mmap/pread of a 6 MiB file through vfs ReadVmo (SH7-d3a)
    linux /bin/pipeprobe   Linux pipe descriptors, byte-stream tails, EOF and errors (SH7-d3c-1)
    linux /bin/threadprobe musl pthreads: clone, futex, a parked reader woken by a sibling (SH7-d3c-2)
    linux /bin/sigprobe    Linux signal delivery: frames, masks, EINTR/SA_RESTART, FP state (SH7-e2)
    --stick: linux /media/KUMO/bin/bigread, the same probe from the USB stick, whose fatfs
             session hands the client's VMO to drv-xhci (SH7-d3b; the stick carries the files
             build/verification/sh7/sh7d3/stick stages)
    --rustc: `linux /media/KUMO/opt/rust/bin/rustc --version` (SH7-d3d): the pinned Linux rustc
             from the stick (scripts/persona-rustc.sh fetches the same tree; the stick carries
             /opt/rust and /lib/{ld-musl-aarch64.so.1,libgcc_s.so.1}), after listing those
             folders so a stick without them reads as such
    --rustc-image: rustc -Vv, a no_std compile and independent hash from the image's FAT, for a
             board without the stick: build the image with KUMO_PERSONA_RUSTC=1
             KUMO_INITRD_ZSTD=0 (and KUMO_CONSOLE_LOGLEVEL=6); the opt-in raw initrd netboots

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

import persona_rustc_proof

ROOT = Path(__file__).resolve().parent.parent

BOOT = (
    b"echo SH7D begins\n"
    b"run asidpair snapshot\n"
    b"linux /bin/m10-hello\n"
    b"linux /bin/hello-dyn\n"
    b"linux /bin/vmprobe\n"
    b"linux /bin/greet\n"
    b"linux /bin/fileprobe\n"
    b"linux /bin/writeprobe\n"
    b"linux /bin/pipeprobe\n"
    b"linux /bin/threadprobe\n"
    b"linux /bin/sigprobe\n"
    b"linux /bin/forkprobe\n"
    b"linux /bin/spawnprobe\n"
    b"linux /bin/bigread\n"
    b"echo SH7D ends\n"
)
EXPECTED = (
    "asidpair: PASS snapshot private bytes, child aliases, explicit handles",
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
    "writeprobe: create, 5003-byte write and read-back OK",
    "writeprobe: pwrite, ftruncate, append, O_TRUNC and O_EXCL OK",
    "writeprobe: rename, cwd, dir fds, unlink and rmdir OK",
    "writeprobe: OK (0 failures)",
    "linux: /bin/writeprobe exited 0",
    "pipeprobe: flags, errors and empty reads OK",
    "pipeprobe: 5003 exact bytes, short-read tails and EOF OK",
    "pipeprobe: EPIPE, 128 reuse cycles and EMFILE rollback OK",
    "pipeprobe: OK (0 failures)",
    "linux: /bin/pipeprobe exited 0",
    "threadprobe: create and join, tid != pid OK",
    "threadprobe: 4 threads x 10000 locked increments = 40000 OK",
    "threadprobe: a parked pipe reader woke on a sibling's write OK",
    "threadprobe: detached thread and timed wait OK",
    "threadprobe: OK (0 failures)",
    "linux: /bin/threadprobe exited 0",
    "sigprobe: raise, siginfo and ucontext OK",
    "sigprobe: masks, sigpending and SIG_IGN OK",
    "sigprobe: altstack and nested frames OK",
    "sigprobe: EINTR and SA_RESTART on a parked pipe read OK",
    "sigprobe: futex-parked handler and interrupted sleep OK",
    "sigprobe: OK (0 failures)",
    "linux: /bin/sigprobe exited 0",
    "forkprobe: OK (0 failures)",
    "linux: /bin/forkprobe exited 0",
    "spawnprobe: OK (0 failures)",
    "linux: /bin/spawnprobe exited 0",
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


def phase_times(text, run):
    """Seconds between the `run`-th bigread's phase lines, from tio's `[HH:MM:SS.mmm]` stamps."""
    stamps = []
    for marker in PHASES:
        found = re.findall(r"^\[(\d\d:\d\d:\d\d\.\d+)\] " + re.escape(marker), text, re.M)
        if len(found) <= run:
            return None
        stamps.append(datetime.strptime(found[run], "%H:%M:%S.%f"))
    return {
        "read_s": (stamps[1] - stamps[0]).total_seconds(),
        "mmap_s": (stamps[2] - stamps[1]).total_seconds(),
        "pread_s": (stamps[3] - stamps[2]).total_seconds(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/sh7/sh7d-metal")
    parser.add_argument("--timeout", type=float, help="seconds after reset (default 240, or 900 with --rustc-image)")
    parser.add_argument("--stick", action="store_true", help="also run bigread from /media/KUMO")
    parser.add_argument("--rustc", action="store_true", help="also run rustc --version from /media/KUMO")
    parser.add_argument(
        "--rustc-image",
        action="store_true",
        help="compile and verify /w/add.o with the image's rustc (KUMO_PERSONA_RUSTC=1)",
    )
    parser.add_argument(
        "--cargo-image",
        action="store_true",
        help="SH7-g: cargo builds the proc-macro workspace, hashes the binary and runs it "
        "(KUMO_PERSONA_RUSTC=1 KUMO_PERSONA_LINK=1 KUMO_PERSONA_CARGO=1)",
    )
    parser.add_argument(
        "--link-image",
        action="store_true",
        help="SH7-f: rustc links /w/hello for KUMO through rust-lld, hashes it and runs it "
        "(an image built with KUMO_PERSONA_RUSTC=1 KUMO_PERSONA_LINK=1)",
    )
    parser.add_argument(
        "--enter",
        action="store_true",
        help="the board waits at the UEFI front page with Reset highlighted: send one Enter "
        "instead of the usual wake-up (which pokes and steers the menu)",
    )
    args = parser.parse_args()
    boot, expected = BOOT, list(EXPECTED)
    if args.stick:
        boot = boot.replace(
            b"echo SH7D ends\n", b"linux /media/KUMO/bin/bigread\necho SH7D ends\n"
        )
        # The verdict appears once per run; it is counted below instead.
        expected.remove("bigread: OK (0 failures)")
        expected.append("linux: /media/KUMO/bin/bigread exited 0")

    if args.rustc:
        boot = boot.replace(
            b"echo SH7D ends\n",
            b"ls /media/KUMO/opt/rust/lib\nls /media/KUMO/lib\n"
            b"linux /media/KUMO/opt/rust/bin/rustc --version\necho SH7D ends\n",
        )
        expected.append("linux: /media/KUMO/opt/rust/bin/rustc exited 0")

    if args.rustc_image:
        boot = boot.replace(b"echo SH7D ends\n", persona_rustc_proof.BOOT + b"echo SH7D ends\n")
    if args.link_image:
        boot = boot.replace(b"echo SH7D ends\n", persona_rustc_proof.LINK_BOOT + b"echo SH7D ends\n")
    if args.cargo_image:
        boot = boot.replace(b"echo SH7D ends\n", persona_rustc_proof.CARGO_BOOT + b"echo SH7D ends\n")

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
    board.stage_boot(tftp, boot)
    pxehost = board.start_pxehost(tftp, work / "pxehost.log")
    tio_pid, master = board.start_tio(work / "boot.log")
    try:
        if args.enter:
            time.sleep(1.0)
            board.send_raw(b"\r")
        else:
            board.wake_board(work / "boot.log")
        long_run = args.rustc_image or args.link_image or args.cargo_image
        slow = args.link_image or args.cargo_image
        timeout = args.timeout if args.timeout is not None else (3600 if slow else 900 if long_run else 240)
        board.wait_for("SH7D ends", work / "boot.log", timeout)
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
        board.validate(text, expected, "SH7D begins", "SH7D ends", login_text=raw)
        window = text[text.index("SH7D begins"):text.index("SH7D ends")]
        if args.rustc and window.partition("SH7-e3 begins")[0].count(persona_rustc_proof.VERSION[0]) != 1:
            raise RuntimeError("wrong or missing rustc version from the stick")
        if args.rustc_image:
            persona_rustc_proof.validate(window)
        if args.link_image:
            persona_rustc_proof.validate_link(window)
        if args.cargo_image:
            persona_rustc_proof.validate_cargo(window)
        if args.stick and window.count("bigread: OK (0 failures)") != 2:
            raise RuntimeError("bigread did not pass both from the image and from the stick")
        if "TOWER" in window:
            raise RuntimeError("a TOWER inside the SH7D window")
        unsupported = sorted(set(re.findall(r"syscall (\d+) unsupported", window)))
        manifest["unsupported_syscalls"] = unsupported
        manifest["bigread"] = phase_times(text, 0)
        if args.stick:
            manifest["bigread_stick"] = phase_times(text, 1)
        manifest["boot"] = "PASS"
        manifest["passed"] = True
        print(
            f"boot: PASS bigread={manifest['bigread']} "
            f"stick={manifest.get('bigread_stick')} unsupported={unsupported}",
            flush=True,
        )
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
