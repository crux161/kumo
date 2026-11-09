#!/usr/bin/env python3
"""The NET10 live peer: real TCP against kumo, one endpoint per campaign phase.

Port contract (base = --port; kumo runs `netprobe tcpwire <kumo-ip-of-mac> <base>`):
  base+0  kumo connects. Both sides stream STREAM bytes of the shared generator at once;
          each verifies the other's stream byte-exact and prints its FNV hash, then both
          half-close (orderly) and read EOF.
  base+1  the peer connects to kumo's listener and streams the generator; kumo echoes it
          back; the peer verifies the echo byte-exact, half-close and EOF.
  base+2  left unbound: kumo's CONNECT must be refused (RST).
  base+3  the peer accepts one connection and resets it (SO_LINGER 0): kumo's RECV must
          surface ConnReset.
  base+4  the peer accepts with a small receive buffer and never reads: kumo's WAIT must
          time out and its stream must reach backpressure (WouldBlock).
  base+5  churn: ten sequential connections, each carrying a 256-byte cycle-tagged
          payload the peer echoes and closes.

Usage: scripts/net10-peer.py [--port 7710] [--kumo 192.168.0.50] [--wait 900]
Exit: 0 PASS, 1 FAIL.
"""
import argparse
import socket
import struct
import sys
import threading
import time

STREAM_TOTAL = 12000


def stream_byte(i):
    return ((i & 0xFF) * 13 + ((i >> 8) & 0xFF)) & 0xFF


def stream(n=STREAM_TOTAL):
    return bytes(stream_byte(i) for i in range(n))


def fnv(data, h=0x811C9DC5):
    for b in data:
        h = ((h ^ b) * 0x01000193) & 0xFFFFFFFF
    return h


def read_all(sock):
    out = bytearray()
    while True:
        chunk = sock.recv(65536)
        if not chunk:
            return bytes(out)
        out += chunk


def accept_one(listener, timeout):
    listener.settimeout(2.0)
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            return listener.accept()[0]
        except socket.timeout:
            continue
        except OSError:
            return None
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--port', type=int, default=7710)
    ap.add_argument('--kumo', default='192.168.0.50')
    ap.add_argument('--wait', type=float, default=900.0)
    args = ap.parse_args()
    base = args.port
    problems = []

    def note(name, fine, detail=''):
        print(f'net10-peer: {name} {"PASS" if fine else "FAIL"} {detail}'.rstrip(),
              flush=True)
        if not fine:
            problems.append(name)

    def listening(port, rcvbuf=None, backlog=4):
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        if rcvbuf is not None:
            s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, rcvbuf)
        s.bind(('0.0.0.0', port))
        s.listen(backlog)
        return s

    expected = stream()

    def phase_client_exchange():
        l = listening(base + 0)
        conn = accept_one(l, args.wait)
        l.close()
        if conn is None:
            note('client-exchange', False, 'no kumo connection')
            return
        conn.settimeout(30.0)
        try:
            conn.sendall(expected)
            conn.shutdown(socket.SHUT_WR)
            got = read_all(conn)
            conn.close()
        except OSError as e:
            note('client-exchange', False, f'socket {e}')
            return
        fine = got == expected
        note('client-exchange', fine,
             f'kumo stream {len(got)} B fnv=0x{fnv(got):08x}')

    def phase_server_exchange():
        deadline = time.time() + args.wait
        conn = None
        while time.time() < deadline:
            try:
                conn = socket.create_connection((args.kumo, base + 1), timeout=2.0)
                break
            except OSError:
                time.sleep(0.5)
        if conn is None:
            note('server-exchange', False, 'kumo never listened')
            return
        conn.settimeout(30.0)
        try:
            conn.sendall(expected)
            conn.shutdown(socket.SHUT_WR)
            got = read_all(conn)
            conn.close()
        except OSError as e:
            note('server-exchange', False, f'socket {e}')
            return
        fine = got == expected
        note('server-exchange', fine,
             f'echo {len(got)} B fnv=0x{fnv(got):08x}')

    def phase_reset():
        l = listening(base + 3)
        conn = accept_one(l, args.wait)
        l.close()
        if conn is None:
            note('reset', False, 'no kumo connection')
            return
        # Let kumo's CONNECT settle first: the RST must surface on RECV, not race the
        # handshake into a refusal.
        time.sleep(0.4)
        conn.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
        conn.close()
        note('reset', True, 'accepted then reset')

    def phase_timeout():
        l = listening(base + 4, rcvbuf=16384)
        conn = accept_one(l, args.wait)
        l.close()
        if conn is None:
            note('timeout', False, 'no kumo connection')
            return
        time.sleep(2.5)
        conn.close()
        note('timeout', True, 'held unread for 2.5 s then closed')

    def phase_churn():
        l = listening(base + 5)
        good = 0
        for cycle in range(10):
            # The first connection waits out kumo's whole boot path; later cycles are
            # back-to-back.
            conn = accept_one(l, args.wait if cycle == 0 else 120.0)
            if conn is None:
                break
            conn.settimeout(10.0)
            try:
                want = bytes(((cycle * 31) + i) & 0xFF for i in range(256))
                got = bytearray()
                while len(got) < len(want):
                    chunk = conn.recv(len(want) - len(got))
                    if not chunk:
                        break
                    got += chunk
                if bytes(got) != want:
                    conn.close()
                    break
                conn.sendall(want)
                conn.shutdown(socket.SHUT_WR)
                read_all(conn)
                conn.close()
            except OSError:
                conn.close()
                break
            good += 1
        l.close()
        note('churn', good == 10, f'{good}/10 connections')

    def refusal_port_free():
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            s.bind(('0.0.0.0', base + 2))
        except OSError:
            note('refusal', False, f'base+2 unexpectedly bound')
            return
        finally:
            s.close()
        note('refusal', True, 'base+2 unbound for the RST')

    threads = [
        threading.Thread(target=phase_client_exchange, daemon=True),
        threading.Thread(target=phase_server_exchange, daemon=True),
        threading.Thread(target=phase_reset, daemon=True),
        threading.Thread(target=phase_timeout, daemon=True),
        threading.Thread(target=phase_churn, daemon=True),
    ]
    refusal_port_free()
    for t in threads:
        t.start()
    for t in threads:
        t.join(args.wait + 120.0)
    ok = not problems
    for p in problems:
        print('net10-peer: PROBLEM', p, flush=True)
    print('net10-peer: PASS' if ok else 'net10-peer: FAIL', flush=True)
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
