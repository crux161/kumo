#!/usr/bin/env python3
#j795
"""Exercise console authentication in a disposable full Rust UEFI/QEMU image.

Only the copy gets a public Argon2 fixture shadow, a quiet autoexec and QEMU's DTB.
The real shadow source and the supplied image are never changed. No real credentials
are needed. --delay-reply builds a private test service that parks before answering;
with --expect-stale it reproduces the pre-j795 one-attempt-late authorization bug.
"""

import argparse
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import tempfile
import time

FIXTURE_PASSWORD = b"kumo-login-fixture"
FIXTURE_SHADOW = b'crux:$argon2id$v=19$m=19456,t=2,p=1$FG/LTr8ryGW0hUX+hv/TOw$Ue+tiPge/dc8iKbA0yUrsvhR/XtQ4cUNXnxk3BCVqyU:20762:0:99999:7:::\n'
LOGIN = b"INDRA login:"
SUCCESS = b"login: crux on the console"
DENIED = b"Login incorrect"
PROMPT = "λ".encode()


def stage(source, destination, identityd=None):
    shutil.copytree(source, destination)
    path = destination / "EFI/KUMO/initrd.img"
    original = path.read_bytes()
    assert original[:8] == b"KUMORD01" and struct.unpack_from("<I", original, 8)[0] == 1
    count = struct.unpack_from("<I", original, 12)[0]
    out = bytearray(original[:16 + 80 * count])
    changed = []
    for index in range(count):
        at = 16 + 80 * index
        name = original[at:at + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", original, at + 64)
        data = original[offset:offset + size]
        if name == b"bin/identityd" and identityd is not None:
            data = identityd.read_bytes()
            changed.append(name)
        elif name == b"etc/shadow":
            data = b"root:!::0:99999:7:::\n" + FIXTURE_SHADOW + b"nobody:*::0:99999:7:::\n"
            changed.append(name)
        elif name == b"etc/autoexec":
            data = b"dmesg -n 6\necho login-smoke-ready\n"
            changed.append(name)
        struct.pack_into("<QQ", out, at + 64, len(out), len(data))
        out.extend(data)
    expected = [b"etc/autoexec", b"etc/shadow"] + ([b"bin/identityd"] if identityd else [])
    assert sorted(changed) == sorted(expected)
    path.write_bytes(out)


def delayed_service(root, work):
    """Keep the production service intact; inject one park into a private source copy."""
    crate = work / "delayed-identityd"
    (crate / "src").mkdir(parents=True)
    source = (root / "userland/servers/identityd/src/main.rs").read_text()
    needle = "            Some(n as usize)"
    assert source.count(needle) == 1, "identityd recv changed; review the delay fixture"
    source = source.replace(needle, "            let _ = kumo_rt::sleep_ns(250_000_000);\n" + needle)
    (crate / "src/main.rs").write_text(source)
    manifest = '[package]\nname="identityd-login-delay-proof"\nversion="0.0.0"\nedition="2021"\n[workspace]\n[dependencies]\n'
    for name, path in [("identityd", "userland/servers/identityd"), ("kumo-identity", "lib/kumo-identity"),
                       ("kumo-abi", "lib/kumo-abi"), ("kumo-rt", "userland/kumo-rt")]:
        manifest += f'{name}={{path="{root / path}"}}\n'
    manifest += '[profile.release]\npanic="abort"\n'
    (crate / "Cargo.toml").write_text(manifest)
    with (work / "delay-build.log").open("wb") as log:
        subprocess.run(["cargo", "build", "--manifest-path", str(crate / "Cargo.toml"),
                        "--target", "aarch64-unknown-none", "--release", "--offline"],
                       cwd=root, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=120)
    return crate / "target/aarch64-unknown-none/release/identityd-login-delay-proof"


class Console:
    def __init__(self, connection, process, log, delay_reply=False):
        self.connection = connection
        self.process = process
        self.log = log
        self.data = bytearray()
        self.delay_reply = delay_reply
        connection.settimeout(0.1)

    def wait(self, markers, start=0, timeout=45):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            tail = self.data[start:]
            for marker in markers:
                if marker in tail:
                    return marker
            assert self.process.poll() is None, "QEMU exited; inspect artifacts"
            try:
                chunk = self.connection.recv(65536)
            except socket.timeout:
                continue
            assert chunk, "serial disconnected"
            self.data.extend(chunk)
            self.log.write(chunk)
            self.log.flush()
        raise RuntimeError("console result timed out; inspect artifacts")

    def line(self, text):
        for byte in text + b"\r":
            self.connection.sendall(bytes([byte]))
            time.sleep(0.015)

    def attempt(self, name, password, accepted):
        start = len(self.data)
        self.line(name)
        self.wait([b"Password:"], start)
        self.line(password)
        if self.delay_reply:
            # Wake the supervisor while the service is parked. DEL on the empty
            # next input line is harmless; its readiness is unrelated to identityd.
            time.sleep(0.04)
            self.connection.sendall(b"\x7f")
        outcome = self.wait([SUCCESS, b"login: root on the console", DENIED], start)
        assert (outcome != DENIED) == accepted, f"unexpected authentication result for {name!r}"
        self.wait([PROMPT if accepted else LOGIN], start)
        if accepted:
            assert b"login: " + name + b" on the console" in self.data[start:], "wrong session subject"
        assert FIXTURE_PASSWORD not in self.data[start:], "fixture password was echoed"

    def logout(self):
        start = len(self.data)
        self.line(b"logout")
        self.wait([LOGIN], start)


def main():
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", type=Path, default=root / "build/images/qemu-virt-aarch64")
    parser.add_argument("--firmware", type=Path, default=Path("/opt/homebrew/share/qemu"))
    parser.add_argument("--expect-stale", action="store_true")
    parser.add_argument("--delay-reply", action="store_true", help="park the private identityd for 250ms before each reply")
    args = parser.parse_args()
    if args.expect_stale and not args.delay_reply:
        parser.error("--expect-stale requires --delay-reply")
    work = Path(tempfile.mkdtemp(prefix="kumo-login-"))
    print(f"Login smoke artifacts: {work}", flush=True)
    esp = work / "esp"
    stage(args.image, esp, delayed_service(root, work) if args.delay_reply else None)
    dtb = esp / "EFI/KUMO/dtb/qcom/sc8280xp-lenovo-thinkpad-x13s.dtb"
    dtb.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([
        "qemu-system-aarch64", "-machine", f"virt,gic-version=3,dumpdtb={dtb}",
        "-cpu", "cortex-a72", "-m", "512", "-display", "none", "-device", "ramfb", "-net", "none",
    ], check=True, capture_output=True, timeout=15)
    shutil.copyfile(args.firmware / "edk2-arm-vars.fd", work / "vars.fd")
    serial = work / "serial.sock"
    with (work / "qemu.log").open("wb") as qemu_log, (work / "serial.log").open("wb") as serial_log:
        process = subprocess.Popen([
            "qemu-system-aarch64", "-machine", "virt,gic-version=3", "-cpu", "cortex-a72",
            "-m", "512", "-display", "none", "-device", "ramfb", "-monitor", "none",
            "-serial", f"unix:{serial},server=on,wait=off",
            "-drive", f"if=pflash,format=raw,file={args.firmware / 'edk2-aarch64-code.fd'},readonly=on",
            "-drive", f"if=pflash,format=raw,file={work / 'vars.fd'}",
            "-drive", f"file=fat:rw:{esp},format=raw,if=virtio", "-net", "none",
        ], stdout=qemu_log, stderr=qemu_log)
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                for _ in range(200):
                    try:
                        connection.connect(str(serial))
                        break
                    except (FileNotFoundError, ConnectionRefusedError):
                        time.sleep(0.05)
                else:
                    raise RuntimeError("QEMU serial socket unavailable")
                console = Console(connection, process, serial_log, args.delay_reply)
                console.wait([LOGIN], timeout=90)
                proof_start = len(console.data)
                if args.expect_stale:
                    console.attempt(b"crux", FIXTURE_PASSWORD, False)
                    time.sleep(1)  # Let the abandoned successful reply become readable.
                    console.attempt(b"crux", b"a", True)
                    print("Login negative control green: correct rejected, following wrong password accepted (stale authorization reproduced)", flush=True)
                    return
                console.attempt(b"crux", FIXTURE_PASSWORD, True)
                start = len(console.data)
                console.line(b"id crux")
                console.wait([b"uid=1000(crux)"], start)
                console.wait([PROMPT], start)
                console.logout()
                for wrong in [b"a", b"wrong-again"]:
                    console.attempt(b"crux", wrong, False)
                console.attempt(b"crux", FIXTURE_PASSWORD, True)
                console.logout()
                for name in [b"root", b"no-such-user"]:
                    console.attempt(name, FIXTURE_PASSWORD, False)
                console.attempt(b"crux", FIXTURE_PASSWORD, True)
                console.logout()
                console.attempt(b"crux", b"", False)
                console.attempt(b"crux", FIXTURE_PASSWORD, True)
                console.logout()
                assert not any(marker in console.data[proof_start:] for marker in [b"TOWER", b"ABEND", b"FAILED"])
                print("Login smoke green: first correct accepted; wrong/empty/locked/unknown denied after prior success; next correct accepted; lookup/logout synchronized; password echo absent", flush=True)
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


if __name__ == "__main__":
    main()
