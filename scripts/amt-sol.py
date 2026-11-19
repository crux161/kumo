#!/usr/bin/env python3
#j908
"""amt-sol.py - Intel AMT Serial-over-LAN console for the NUC (no TLS, stdlib only).

KUMO mirrors its x86 serial console to AMT's SOL UART (j907). Homebrew's amtterm cannot open a
non-TLS session (its -P path fails before sending a byte), and this NUC's AMT only listens on the
plain redirection port 16994. This client speaks the AMT redirection protocol directly:

  StartRedirectionSession "SOL " -> AuthenticateSession (digest, RFC 2617 qop=auth, so the
  password itself never crosses the wire) -> StartSOLRedirection -> stream SOL data, echoing
  AMT's heartbeats so the session stays up.

Everything received is written to stdout and appended to a log file (default build/nuc-sol.log)
so another reader - Claude - can follow the boot without the password. The password is read
from the terminal (or $AMT_PASSWORD) and is never written anywhere.

Usage: scripts/amt-sol.py [-u admin] [--log build/nuc-sol.log] [--probe] host [port]
  --probe   stop after the pre-login handshake (session start + offered auth methods +
            the digest challenge): needs no password, proves the protocol path.
Ctrl-C ends the session.
"""
import argparse
import getpass
import hashlib
import os
import select
import socket
import struct
import sys
import time

START_SESSION, START_SESSION_REPLY = 0x10, 0x11
END_SESSION = 0x12
AUTH, AUTH_REPLY = 0x13, 0x14
START_SOL, START_SOL_REPLY = 0x20, 0x21
END_SOL, END_SOL_REPLY = 0x22, 0x23
KEEPALIVE_PING, KEEPALIVE_PONG = 0x24, 0x25
SOL_DATA_TO_HOST, SOL_CONTROLS_FROM_HOST, SOL_DATA_FROM_HOST, SOL_HEARTBEAT = 0x28, 0x29, 0x2A, 0x2B
URI = b"/RedirectionService"
METHOD_DIGEST_QOP = 4


class Stream:
    """Byte-exact reads over the redirection socket."""

    def __init__(self, sock):
        self.sock = sock
        self.buf = b""

    def need(self, n, timeout=10.0):
        deadline = time.monotonic() + timeout
        while len(self.buf) < n:
            left = deadline - time.monotonic()
            if left <= 0:
                raise TimeoutError(f"waited for {n} bytes, have {len(self.buf)}: {self.buf.hex(' ')}")
            ready, _, _ = select.select([self.sock], [], [], left)
            if ready:
                chunk = self.sock.recv(65536)
                if not chunk:
                    raise ConnectionError("AMT closed the connection")
                self.buf += chunk

    def take(self, n, timeout=10.0):
        self.need(n, timeout)
        out, self.buf = self.buf[:n], self.buf[n:]
        return out


def element(value: bytes) -> bytes:
    if len(value) > 255:
        raise ValueError("auth element too long")
    return bytes([len(value)]) + value


def auth_message(method: int, body: bytes) -> bytes:
    return struct.pack("<BBHBI", AUTH, 0, 0, method, len(body)) + body


def read_auth_reply(stream):
    header = stream.take(9)
    opcode, status, _, method, length = struct.unpack("<BBHBI", header)
    if opcode != AUTH_REPLY:
        raise ConnectionError(f"expected auth reply 0x14, got {header.hex(' ')}")
    return status, method, stream.take(length)


def elements(body: bytes):
    out, i = [], 0
    while i < len(body):
        n = body[i]
        out.append(body[i + 1:i + 1 + n])
        i += 1 + n
    return out


def md5hex(*parts: bytes) -> bytes:
    return hashlib.md5(b":".join(parts)).hexdigest().encode()


def handshake(stream, user: bytes):
    """Session start, auth-method query, digest challenge. Returns (realm, nonce, qop)."""
    stream.sock.sendall(bytes([START_SESSION, 0, 0, 0]) + b"SOL ")
    reply = stream.take(13)
    if reply[0] != START_SESSION_REPLY or reply[1] != 0:
        raise ConnectionError(f"SOL session refused (is SOL enabled in AMT?): {reply.hex(' ')}")
    stream.sock.sendall(auth_message(0, b""))
    status, _, methods = read_auth_reply(stream)
    print(f"amt-sol: SOL session accepted; auth methods offered {list(methods)}", file=sys.stderr)
    if METHOD_DIGEST_QOP not in methods:
        raise ConnectionError(f"AMT offers no RFC 2617 digest (methods {list(methods)})")
    first = element(user) + b"\0\0" + element(URI) + b"\0\0\0\0"
    stream.sock.sendall(auth_message(METHOD_DIGEST_QOP, first))
    status, _, body = read_auth_reply(stream)
    fields = elements(body)
    if len(fields) < 3 or fields[2] != b"auth":
        raise ConnectionError(f"unexpected digest challenge: status {status} {body.hex(' ')}")
    realm, nonce, qop = fields[0], fields[1], fields[2]
    print(f"amt-sol: digest challenge realm={realm.decode(errors='replace')} qop={qop.decode()}",
          file=sys.stderr)
    return realm, nonce, qop


def authenticate(stream, user: bytes, password: bytes, realm, nonce, qop):
    cnonce = os.urandom(16).hex().encode()
    nc = b"00000002"
    ha1 = md5hex(user, realm, password)
    ha2 = md5hex(b"POST", URI)
    response = md5hex(ha1, nonce, nc, cnonce, qop, ha2)
    body = b"".join(element(x) for x in (user, realm, nonce, URI, cnonce, nc, response, qop))
    stream.sock.sendall(auth_message(METHOD_DIGEST_QOP, body))
    status, _, _ = read_auth_reply(stream)
    if status != 0:
        raise PermissionError("AMT rejected the login (wrong user or password)")


def start_sol(stream):
    msg = bytes([START_SOL, 0, 0, 0, 0, 0, 0, 0]) + struct.pack(
        "<HHHHHHI",
        1000,   # max tx buffer
        100,    # tx buffer timeout, ms
        0,      # tx overflow timeout
        10000,  # host session rx timeout, ms
        0,      # host fifo rx flush timeout
        5000,   # heartbeat interval, ms
        0,
    )
    stream.sock.sendall(msg)
    stream.need(2)
    if stream.buf[0] != START_SOL_REPLY or stream.buf[1] != 0:
        raise ConnectionError(f"StartSOLRedirection refused: {stream.buf[:24].hex(' ')}")
    stream.take(23)


def pump(stream, log):
    """Stream SOL data until Ctrl-C or the session ends."""
    out = sys.stdout.buffer
    while True:
        stream.need(1, timeout=30.0)
        opcode = stream.buf[0]
        if opcode == SOL_DATA_FROM_HOST:
            header = stream.take(10, timeout=30.0)
            length = struct.unpack_from("<H", header, 8)[0]
            data = stream.take(length, timeout=30.0)
            out.write(data)
            out.flush()
            log.write(data)
            log.flush()
        elif opcode in (SOL_HEARTBEAT, KEEPALIVE_PING):
            stream.sock.sendall(stream.take(8))  # echo, as amtterm does
        elif opcode == KEEPALIVE_PONG:
            stream.take(8)
        elif opcode == SOL_CONTROLS_FROM_HOST:
            stream.take(10)
        elif opcode in (END_SOL_REPLY, END_SESSION):
            print("\namt-sol: AMT ended the session", file=sys.stderr)
            return
        else:
            raise ConnectionError(f"unknown redirection message: {stream.buf[:32].hex(' ')}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("host")
    parser.add_argument("port", nargs="?", type=int, default=16994)
    parser.add_argument("-u", "--user", default="admin")
    parser.add_argument("--log", default=os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "build", "nuc-sol.log"))
    parser.add_argument("--probe", action="store_true")
    args = parser.parse_args()

    user = args.user.encode()
    sock = socket.create_connection((args.host, args.port), timeout=10)
    sock.setblocking(True)
    stream = Stream(sock)
    realm, nonce, qop = handshake(stream, user)
    if args.probe:
        print("amt-sol: probe done (no password used)", file=sys.stderr)
        return 0
    password = os.environ.get("AMT_PASSWORD") or getpass.getpass(f"AMT password for {args.user}@{args.host}: ")
    authenticate(stream, user, password.encode(), realm, nonce, qop)
    del password
    start_sol(stream)
    log_path = os.path.normpath(args.log)
    os.makedirs(os.path.dirname(log_path), exist_ok=True)
    print(f"amt-sol: SOL running; logging to {log_path} (Ctrl-C to stop)", file=sys.stderr)
    with open(log_path, "ab") as log:
        log.write(f"\n--- amt-sol {time.strftime('%Y-%m-%d %H:%M:%S')} {args.host} ---\n".encode())
        try:
            pump(stream, log)
        except KeyboardInterrupt:
            print("\namt-sol: stopped", file=sys.stderr)
            try:
                sock.sendall(bytes([END_SOL, 0, 0, 0, 0, 0, 0, 0]))
                sock.sendall(bytes([END_SESSION, 0, 0, 0]))
            except OSError:
                pass
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ConnectionError, PermissionError, TimeoutError, OSError) as err:
        print(f"amt-sol: {err}", file=sys.stderr)
        sys.exit(1)
