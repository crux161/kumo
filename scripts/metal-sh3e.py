#!/usr/bin/env python3
#j870
#j872
#j874
"""SH3-e: the cargo-shaped metal suite on the Orange Pi 5 Plus, two PXE boots.

Boot 1 (std::fs and C on the mounted volume, cross-process locks):
    media std-smoke mediasuite        mkdir tree, write, atomic rename, stat, readdir
    media relibc-smoke media          the C leg: mkdir/write/rename/stat/readdir/lockf
    bg media std-smoke lockhold ...   a bounded exclusive hold in one process
    media std-smoke locktry ...       WouldBlock in a second process of the same server
    wait
    media std-smoke locktry ...       acquired after the holder released, then reboot

Boot 2 (persistence read-back): mediacheck asserts the tree survived the power cycle,
then removes it.

Prerequisites, per the opi5 serial-loop recipe: board powered with the KUMO stick in the
rear USB-A port, serial adapter at /dev/tty.usbserial-B0001AH9, no other tio or pxehost
process, pxehost binary at build/tools/pxehost-kumo. The image must be built with
KUMO_CONSOLE_LOGLEVEL=6: every proof line (autoexec, `std: sh3`, Sora's own log) is
NOTICE, and the kernel's default console level suppresses NOTICE entirely.
"""

import argparse
import importlib.util
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SERIAL = "/dev/tty.usbserial-B0001AH9"
TIO_SOCKET = "/tmp/kumo-sh3e.sock"
PXEHOST = ROOT / "build/tools/pxehost-kumo"
SOURCE_IMAGE = ROOT / "build/images/orange-pi-5-plus"

BOOT1 = (
    b"echo SH3E begins\n"
    b"media std-smoke mediasuite\n"
    b"media relibc-smoke media\n"
    b"bg media std-smoke lockhold /SH3E/LOCK 3000\n"
    b"media std-smoke locktry /SH3E/LOCK\n"
    b"wait\n"
    b"media std-smoke locktry /SH3E/LOCK\n"
    b"echo SH3E boot1 ends\n"
)
BOOT2 = b"echo SH3E-2 begins\nmedia std-smoke mediacheck\necho SH3E-2 ends\n"

BOOT1_EXPECTED = (
    "std: sh3 e1 mkdir -> OK",
    "std: sh3 e1 mkdir tree -> OK",
    "std: sh3 e1 write -> OK",
    "std: sh3 e1 rename -> OK",
    "std: sh3 e1 stat -> OK",
    "std: sh3 e1 read -> OK",
    "std: sh3 e1 readdir -> OK",
    "c-media: mkdir -> OK",
    "c-media: rename -> OK",
    "c-media: stat -> OK",
    "c-media: readdir -> OK",
    "c-media: lockf -> OK",
    "c-media: all -> OK",
    "std: sh3 lock held",
    "std: sh3 lock wouldblock",
    "std: sh3 lock released",
    "std: sh3 lock acquired",
    "SH3E boot1 ends",
)
BOOT2_EXPECTED = (
    "std: sh3 e3 persisted -> OK",
    "std: sh3 e3 cleanup -> OK",
    "std: sh3 e3 cleaned -> OK",
    "SH3E-2 ends",
)


def load_harness():
    spec = importlib.util.spec_from_file_location("axh", ROOT / "scripts/netstack-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def stage_tree(tftp):
    """A private TFTP root from the built image. The firmware's NBP is the top-level
    `bootaa64.efi` (PXEHOST_ARM64_BOOTFILE); `xtask image` emits the loader only as
    EFI/BOOT/BOOTAA64.EFI, so stage that copy too. J870 passed only because the main
    checkout's image directory carried a hand-staged stray bootaa64.efi (found j872)."""
    if tftp.exists():
        shutil.rmtree(tftp)
    shutil.copytree(SOURCE_IMAGE, tftp)
    loader = tftp / "EFI/BOOT/BOOTAA64.EFI"
    if not loader.is_file():
        raise RuntimeError(f"missing {loader}; rebuild the orange-pi-5-plus image")
    shutil.copyfile(loader, tftp / "bootaa64.efi")


def stage_boot(tftp, script):
    # Reuse the AXH harness's own staging (unpack -> replace etc/autoexec -> repack), so
    # the metal and QEMU flows cannot drift: it takes a Case carrying the script.
    axh = load_harness()
    initrd = tftp / "EFI/KUMO/initrd.img"
    axh.stage(initrd, axh.Case("sh3e-metal", script, (), ready=0))


def start_pxehost(tftp, log_path):
    env = dict(os.environ)
    env["PXEHOST_TFTP_ROOT"] = str(tftp)
    env["PXEHOST_ARM64_BOOTFILE"] = "bootaa64.efi"
    log = log_path.open("wb")
    return subprocess.Popen([str(PXEHOST)], env=env, stdout=log, stderr=log)


def start_tio(log_path):
    if os.path.exists(TIO_SOCKET):
        os.unlink(TIO_SOCKET)
    # tio needs a tty for its own UI; a forked pty gives it one while the log captures
    # everything. --log-strip removes tio's own framing from the log lines.
    pid, master = pty.fork()
    if pid == 0:
        os.execvp(
            "tio",
            [
                "tio", "-b", "1500000", SERIAL, "-t",
                "-L", "--log-file", str(log_path), "--log-strip",
                "-S", f"unix:{TIO_SOCKET}",
            ],
        )
        os._exit(127)
    # tio draws its UI on the pty and writes the serial stream there too. Nothing else
    # reads the master, and an unread pty eventually blocks tio on write (the J601
    # lesson), after which socket input is silently never forwarded. Drain it, and keep
    # the raw bytes beside the log for menu-level forensics.
    raw_path = log_path.with_suffix(log_path.suffix + ".raw")
    def drain():
        with raw_path.open("wb") as sink:
            while True:
                try:
                    chunk = os.read(master, 4096)
                except OSError:
                    return
                if not chunk:
                    return
                sink.write(chunk)
                sink.flush()
    threading.Thread(target=drain, daemon=True).start()
    wait_socket()
    return pid, master


def send_line(line):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(10)
        sock.connect(TIO_SOCKET)
        time.sleep(0.3)
        sock.sendall(line.encode() + b"\n")


def log_tail(log_path, lines=40):
    if not log_path.exists():
        return ""
    return "\n".join(log_path.read_text(errors="replace").splitlines()[-lines:])


def wait_socket(timeout=8.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(TIO_SOCKET):
            return
        time.sleep(0.1)
    raise RuntimeError(f"tio never created {TIO_SOCKET}; inspect the boot log")


def send_raw(payload):
    deadline = time.monotonic() + 5.0
    while True:
        try:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.settimeout(10)
            sock.connect(TIO_SOCKET)
            break
        except (FileNotFoundError, ConnectionRefusedError) as error:
            sock.close()
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.2)
    with sock:
        time.sleep(0.3)
        sock.sendall(payload)


def log_whole(log_path):
    if not log_path.exists():
        return ""
    return log_path.read_text(errors="replace")


def capture_text(log_path):
    """Everything the console has shown: tio's line log plus the raw pty capture.
    --log-file flushes per newline, so prompt lines (no trailing newline) exist only in
    the raw stream — exactly the state that matters when the board sits at a shell."""
    text = log_whole(log_path)
    raw = log_path.with_suffix(log_path.suffix + ".raw")
    if raw.exists():
        noise = ("tio 3", "Press ctrl-t q", "Listening on socket", "Connected to ",
                 "Disconnected", "Reconnecting")
        for line in raw.read_text(errors="replace").splitlines():
            if any(marker in line for marker in noise):
                continue
            text += re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", line) + "\n"
    return text


def classify(text):
    """The console state judged over the WHOLE capture, not a tail: UEFI screens redraw
    as pages of blank, space-padded rows that bury the earlier menu text. Login vs shell
    is decided by the LAST prompt position (the serial-loop lesson), because a capture
    that spans a login and later shell must answer for where the board is now."""
    shell_at = max(
        text.rfind("└──//"),
        text.rfind("// λ"),
        text.rfind("┌──["),
        text.rfind("No directory, logging in with HOME=/"),
    )
    login_at = text.rfind("INDRA login:")
    if shell_at != -1 and shell_at > login_at:
        return "shell"
    if login_at != -1:
        return "login"
    if "SH3E begins" in text:
        return "running"
    if "PXE-E16" in text or "Start HTTP Boot" in text or ">>Start PXE" in text:
        return "firmware-net"
    if "Boot Manager" in text or "Device Manager" in text:
        return "firmware-menu"
    # The front page prints only the highlighted option's help text, never its own name:
    # with the highlight on Select Language the capture reads "...change the language for
    # the current system".
    if "change the" in text or "Select Language" in text:
        return "firmware-menu"
    if "UEFI" in text:
        return "firmware-menu"
    # j874: a poke at the front page can redraw ONLY the help panel of the highlighted
    # entry, so the capture holds one bare label (seen: "Reset" at row 8, column 71) and
    # nothing else this function knows. A front-page label alone on its line is the menu.
    if re.search(
        r"^(\[[\d:.]+\]\s*)?(" + "|".join(FRONT_PAGE) + r")\s*$", text, re.MULTILINE
    ):
        return "firmware-menu"
    return "unknown"


FRONT_PAGE = (
    "Boot Maintenance Manager", "Select Language", "Device Manager", "Boot Manager",
    "Continue", "Reset",
)
RESET_SEEN = ("DDR ", "U-Boot SPL", "BL31:")


def raw_text_since(log_path, offset):
    raw = log_path.with_suffix(log_path.suffix + ".raw")
    data = raw.read_bytes()[offset:] if raw.exists() else b""
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", " ", data.decode(errors="replace"))
    text = re.sub(r"\[\d\d:\d\d:\d\d\.\d+\]", " ", text)
    return re.sub(r"\s+", " ", text)


def raw_size(log_path):
    raw = log_path.with_suffix(log_path.suffix + ".raw")
    return raw.stat().st_size if raw.exists() else 0


def front_page_reset(log_path):
    """Steer the UEFI front page onto Reset by reading the highlight, never by count.

    Each arrow redraws the old entry, then `<Enter>=Select Entry`, then the newly
    highlighted entry's label and help text. The highlight does NOT reliably start on
    Select Language (j872 found it on Device Manager), and the list wraps, so a blind
    five-downs-then-Enter can open the language popup instead. A pending popup is closed
    with Esc first. The board has also been seen to reset by itself once Reset is
    highlighted (J741/J747); Enter is sent only if it has not.
    """
    if "Esc=Exit Entry" in raw_text_since(log_path, max(0, raw_size(log_path) - 600)):
        send_raw(b"\x1b")
        time.sleep(1.0)
    for _ in range(len(FRONT_PAGE) + 2):
        mark = raw_size(log_path)
        send_raw(b"\x1b[B")
        time.sleep(1.2)
        fresh = raw_text_since(log_path, mark)
        if any(marker in fresh for marker in RESET_SEEN):
            return
        _, _, after = fresh.rpartition("Select Entry")
        after = re.sub(r"^\s*(<[^>]*>\s*)?>?\s*", "", after)
        highlighted = next((name for name in FRONT_PAGE if after.startswith(name)), None)
        if highlighted == "Reset":
            mark = raw_size(log_path)
            time.sleep(2.0)
            if not any(marker in raw_text_since(log_path, mark) for marker in RESET_SEEN):
                send_raw(b"\r")
            return
    raise RuntimeError(f"could not steer the UEFI front page onto Reset; see {log_path}.raw")


def wake_board(log_path):
    """Reset the board into a fresh PXE boot by the safest method for its current state.

    Safe methods per the serial-loop recipe: the Ctrl-\\ then r escape hatch ONLY at the
    login prompt (it powers the board off from a logged-in shell), `reboot` at a logged-in
    shell, Enter at the firmware's network screen, and the documented front-page reset
    (five downs onto Reset, then Enter) when a UEFI menu is redrawing. Silence means the
    board is off or the port is held: report rather than waiting out the timeout.
    """
    # A board that powered on before the capture idles with an EMPTY log: poke the line
    # first, as the serial-loop recipe prescribes, and give the console time to answer.
    deadline = time.monotonic() + 30.0
    while time.monotonic() < deadline:
        if log_path.exists() and log_path.stat().st_size > 0:
            break
        # Prompts carry no trailing newline, so tio's line log never shows them; the raw
        # capture is the only witness. Any recognized console text means the board
        # answered the poke, while a genuinely silent board gets the full window.
        if any(marker in capture_text(log_path) for marker in (
            "INDRA login:", "└──//", "[MUREX>", "change the", "UEFI", "SH3E begins",
        )):
            break
        send_raw(b"\n")
        time.sleep(2.0)
    text = log_whole(log_path)
    if not text and not capture_text(log_path).replace("tio", "").strip():
        raise RuntimeError(
            "serial line produces no output at all after poking; check the board's power "
            f"(Feit switch) and the USB-serial adapter, then rerun ({log_path})"
        )
    # Let an in-progress judgement settle: the firmware redraws for a moment after a key.
    time.sleep(1.0)
    state = classify(capture_text(log_path))
    if state == "login":
        send_raw(b"\x1cr")
    elif state == "shell":
        send_line("reboot")
    elif state == "running":
        return
    elif state == "firmware-menu":
        front_page_reset(log_path)
    elif state == "firmware-net":
        # Enter starts the highlighted network boot; if that entry has no link the
        # firmware falls through to the cabled port on its own.
        send_raw(b"\r")
    else:
        raise RuntimeError(
            f"board responded but the console state is unknown; inspect {log_path}"
        )
    return


def wait_for(text_wanted, log_path, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        text = log_path.read_text(errors="replace") if log_path.exists() else ""
        if text_wanted in text:
            return text
        time.sleep(0.2)
    raise RuntimeError(f"timeout waiting for {text_wanted!r}; see {log_path}")


def validate(text, expected, begin, end):
    if text.count(begin) != 1 or text.count(end) != 1:
        raise RuntimeError(f"missing/duplicated {begin!r} or {end!r}")
    for marker in expected:
        if text.count(marker) != 1:
            raise RuntimeError(f"missing/duplicated {marker!r}")
    if text.count("INDRA login:") < 1:
        raise RuntimeError("no login prompt")


def stop_all(pxehost, tio_pid, master):
    pxehost.terminate()
    try:
        pxehost.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pxehost.kill()
    os.kill(tio_pid, signal.SIGTERM)
    try:
        os.waitpid(tio_pid, 0)
    except ChildProcessError:
        pass
    os.close(master)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, default=ROOT / "build/verification/sh3/sh3e")
    parser.add_argument("--timeout", type=float, default=180.0)
    args = parser.parse_args()

    for tool in ("tio",):
        if shutil.which(tool) is None:
            sys.exit(f"{tool} not on PATH")
    if not PXEHOST.is_file():
        sys.exit(f"missing {PXEHOST}; build with scripts/build-pxehost-kumo.sh")
    if not SOURCE_IMAGE.is_dir():
        sys.exit(f"missing {SOURCE_IMAGE}; run KUMO_CONSOLE_LOGLEVEL=6 cargo xtask image --arch aarch64 --hardware opi5plus")

    work = args.work
    work.mkdir(parents=True, exist_ok=True)
    tftp = work / "tftp"
    stage_tree(tftp)

    manifest = {"boots": {}, "pxehost": str(PXEHOST)}
    # One pxehost+tio lifetime spans both boots: the escape hatch needs the serial socket
    # alive, and only the initrd is restaged between them (the board is between fetches).
    stage_boot(tftp, BOOT1)
    pxehost = start_pxehost(tftp, work / "pxehost.log")
    tio_pid, master = start_tio(work / "boot1.log")
    try:
        wake_board(work / "boot1.log")
        text = wait_for("SH3E boot1 ends", work / "boot1.log", args.timeout)
        # The login prompt follows autoexec once the display stack settles; validate()
        # requires it, and the escape-hatch reboot is only safe from it.
        wait_for("INDRA login:", work / "boot1.log", 60)
        text = log_whole(work / "boot1.log")
        validate(text, BOOT1_EXPECTED, "SH3E begins", "SH3E boot1 ends")
        manifest["boots"]["boot1"] = "PASS"
        print("boot1: PASS", flush=True)
        # The board sits at the login prompt; from there the kernel escape hatch is the
        # safe reset (Ctrl-\ then r -> sysrq reboot -> PXE). Restage boot 2's initrd
        # first so the fresh fetch sees it.
        stage_boot(tftp, BOOT2)
        # tio keeps writing to boot1.log (it flushes per newline and we must not move an
        # open log); boot 2's markers are simply matched in the same stream.
        send_line(chr(0x1C) + "r")
        text = wait_for("SH3E-2 ends", work / "boot1.log", args.timeout + 60)
        validate(text, BOOT2_EXPECTED, "SH3E-2 begins", "SH3E-2 ends")
        manifest["boots"]["boot2"] = "PASS"
        print("boot2: PASS", flush=True)
        manifest["passed"] = True
    except BaseException as error:
        manifest["passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        stop_all(pxehost, tio_pid, master)
        (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("SH3E metal suite green", flush=True)


if __name__ == "__main__":
    main()
