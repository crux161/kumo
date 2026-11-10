#!/usr/bin/env python3
"""The NET15 external std::net peer: real UDP and TCP against the std campaign on kumo.

Contract (shared with toolchain/rust/smoke `netwire`):
  UDP: kumo sends 24 numbered datagrams `kumo-std-<i>-` (index 0 zero-length, index 1 the
       1472-byte maximum, otherwise prefix + i % 16 'x' fill). The peer verifies them
       byte-exact and in order — and does NOT echo them (kumo's echo loop expects only
       `peer-std-<i>` datagrams) — then sends its own 24 `peer-std-<i>-` datagrams;
       kumo must echo each back byte-exact.
  TCP: the peer accepts one connection and streams 12000 bytes of the shared generator
       while verifying kumo's simultaneous stream byte-exact; both half-close and read
       EOF. Generator: stream_byte(i) = ((i * 31 + 7) & 0xFF).

Usage: scripts/net15-peer.py [--udp-port 7801] [--tcp-port 7802] [--wait 900]
                             [--skip-udp]
Exit: 0 PASS, 1 FAIL.
"""
import argparse
import socket
import sys
import threading
import time

TOTAL = 12000
COUNT = 24


def stream_byte(i):
    return (i * 31 + 7) & 0xFF


def payload(side, i):
    if i == 0:
        return b''
    prefix = f'{side}-std-{i}-'.encode()
    if i == 1:
        return prefix + b'\x00' * (1472 - len(prefix))
    return prefix + b'x' * (i % 16)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--udp-port', type=int, default=7801)
    ap.add_argument('--tcp-port', type=int, default=7802)
    ap.add_argument('--wait', type=float, default=900.0)
    ap.add_argument('--skip-udp', action='store_true',
                    help='serve only the TCP duplex (a second instance for the '
                         'hostname campaign)')
    args = ap.parse_args()

    phases = []
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    udp.bind(('0.0.0.0', args.udp_port))
    udp.settimeout(2.0)
    tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    tcp.bind(('0.0.0.0', args.tcp_port))
    tcp.listen(2)
    tcp.settimeout(2.0)
    print(f'net15-peer: udp {args.udp_port} tcp {args.tcp_port}; waiting up to {args.wait:.0f}s',
          flush=True)

    deadline = time.time() + args.wait
    kumo_src = None

    # The TCP accept runs from the start: the hostname campaign connects without
    # any UDP phase, so it must not wait behind the datagram capture.
    tcp_result = {}

    def tcp_phase():
        conn = None
        while time.time() < deadline:
            try:
                conn, _ = tcp.accept()
                break
            except socket.timeout:
                continue
        if conn is None:
            tcp_result['fine'] = False
            return
        conn.settimeout(10.0)
        ours = bytes(stream_byte(i) for i in range(TOTAL))
        got = bytearray()
        sender = threading.Thread(target=lambda: (conn.sendall(ours),
                                                  conn.shutdown(socket.SHUT_WR)), daemon=True)
        sender.start()
        while len(got) < TOTAL:
            try:
                chunk = conn.recv(4096)
            except socket.timeout:
                break
            if not chunk:
                break
            got.extend(chunk)
        sender.join(timeout=5)
        exact = bytes(got) == ours
        eof_ok = False
        try:
            while True:
                chunk = conn.recv(4096)
                if not chunk:
                    eof_ok = True
                    break
        except socket.timeout:
            pass
        conn.close()
        print(f'net15-peer: tcp kumo stream {len(got)} B '
              f'{"exact" if exact else "MISMATCH"}; half-close EOF {eof_ok}', flush=True)
        tcp_result['fine'] = exact and eof_ok

    tcp_thread = threading.Thread(target=tcp_phase, daemon=True)
    tcp_thread.start()

    if args.skip_udp:
        tcp_thread.join()
        fine = tcp_result.get('fine', False)
        print(f'net15-peer: tcp duplex {"PASS" if fine else "FAIL"}', flush=True)
        print('net15-peer: PASS' if fine else 'net15-peer: FAIL', flush=True)
        return 0 if fine else 1

    # UDP: capture and verify kumo's 24 numbered datagrams, echoing each back.
    seen = []
    while len(seen) < COUNT and time.time() < deadline:
        try:
            data, src = udp.recvfrom(2048)
        except socket.timeout:
            continue
        except OSError:
            break
        kumo_src = src
        i = len(seen)
        want = payload('kumo', i)
        fine = data == want
        seen.append(fine)
        print(f'net15-peer: udp kumo-std-{i} len={len(data)} {"exact" if fine else "MISMATCH"}',
              flush=True)
    ok_read = len(seen) == COUNT and all(seen)
    print(f'net15-peer: udp kumo->peer {"PASS" if ok_read else "FAIL"}', flush=True)
    phases.append(('udp kumo->peer', ok_read))

    # UDP: send the peer's 24 numbered datagrams; kumo must echo each back exactly.
    echoed = 0
    if kumo_src is not None:
        udp.settimeout(5.0)
        for i in range(COUNT):
            udp.sendto(payload('peer', i), kumo_src)
            got = None
            while time.time() < deadline:
                try:
                    candidate, src = udp.recvfrom(2048)
                except socket.timeout:
                    break
                if src == kumo_src:
                    got = candidate
                    break
            want = payload('peer', i)
            if got == want:
                echoed += 1
            else:
                print(f'net15-peer: udp peer-std-{i} echo {"exact" if got == want else "MISSING"}',
                      flush=True)
    ok_echo = echoed == COUNT
    print(f'net15-peer: udp peer->kumo echoed {echoed}/{COUNT} '
          f'{"PASS" if ok_echo else "FAIL"}', flush=True)
    phases.append(('udp echo', ok_echo))

    # TCP: join the thread started at the top.
    tcp_thread.join()
    phases.append(('tcp duplex', tcp_result.get('fine', False)))

    ok = all(fine for _, fine in phases)
    for name, fine in phases:
        print(f'net15-peer: {name} {"PASS" if fine else "FAIL"}', flush=True)
    print('net15-peer: PASS' if ok else 'net15-peer: FAIL', flush=True)
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
