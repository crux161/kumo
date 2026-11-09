#!/usr/bin/env python3
"""The NET9 live peer: a real UDP endpoint on the Mac, witnessing both directions.

Binds UDP 0.0.0.0:<port> and waits for kumo's numbered datagrams (kumo-net9-<i>-),
verifying each byte-exact and in order. It then sends its own numbered datagrams
(peer-net9-<i>-), one at a time, and requires each to come back echoed byte-exact by
kumo, which proves kumo received the peer direction. Payload contract, shared with
netprobe and scripts/net9-proof.py: index 0 is the zero-length datagram, index 1 is the
1472-byte maximum (prefix then zeros), every other index is the prefix plus i % 16 fill
bytes.

Usage: scripts/net9-peer.py [--port 7701] [--count 24] [--wait 600]
Exit: 0 PASS, 1 FAIL.
"""
import argparse
import socket
import sys
import time


def payload(side, i):
    prefix = f'{side}-net9-{i}-'.encode()
    if i == 0:
        return b''
    if i == 1:
        return prefix + b'\x00' * (1472 - len(prefix))
    return prefix + b'x' * (i % 16)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--port', type=int, default=7701)
    ap.add_argument('--count', type=int, default=24)
    ap.add_argument('--wait', type=float, default=600.0,
                    help='seconds to wait for kumo to appear')
    args = ap.parse_args()
    count = max(2, min(48, args.count))
    expected_kumo = [payload('kumo', i) for i in range(count)]
    expected_peer = [payload('peer', i) for i in range(count)]

    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(('0.0.0.0', args.port))
    print(f'net9-peer: bound udp {args.port}; waiting up to {args.wait:.0f}s for kumo',
          flush=True)

    problems = []
    kumo = None
    seen = {}
    order = []
    deadline = time.time() + args.wait
    s.settimeout(2.0)
    while time.time() < deadline:
        try:
            data, addr = s.recvfrom(65535)
        except socket.timeout:
            continue
        kumo = addr
        match = None
        for i, want in enumerate(expected_kumo):
            if data == want:
                match = i
                break
        if match is None:
            problems.append(f'unexpected datagram {len(data)} B from {addr}')
            continue
        order.append(match)
        seen[match] = seen.get(match, 0) + 1
        if len(seen) == count:
            break
    if kumo is None:
        problems.append('kumo never sent a datagram')
    missing = [i for i in range(count) if i not in seen]
    dups = sum(1 for n in seen.values() if n > 1)
    if missing:
        problems.append(f'kumo datagrams missing: {missing}')
    ordered = order == sorted(order)
    if not ordered:
        problems.append(f'kumo datagrams out of order: {order}')
    print(f'net9-peer: kumo->peer {len(seen)}/{count} exact, '
          f'order {"ok" if ordered else "VIOLATED"}, duplicates {dups}', flush=True)

    if kumo is not None:
        sent = echoed = 0
        for i in range(count):
            s.sendto(expected_peer[i], kumo)
            sent += 1
            until = time.time() + 3.0
            s.settimeout(0.5)
            while time.time() < until:
                try:
                    data, addr = s.recvfrom(65535)
                except socket.timeout:
                    continue
                if addr == kumo and data == expected_peer[i]:
                    echoed += 1
                    break
                # A late kumo initiation is tolerated; it was counted above or is a
                # duplicate the capture will show.
            else:
                problems.append(f'no exact echo for peer index {i}')
        print(f'net9-peer: peer->kumo {sent}/{count} sent, {echoed}/{count} echoed exact',
              flush=True)

    ok = not problems
    for p in problems:
        print('net9-peer: PROBLEM', p)
    print('net9-peer: PASS' if ok else 'net9-peer: FAIL')
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
