#!/usr/bin/env python3
"""The NET9 proof: exact UDP datagrams in both directions on the peer capture.

Verifies, from one classic pcap (UDP port <port>):
  * every kumo-originated datagram (kumo-net9-<i>-), in order, byte-exact,
  * every peer-originated datagram (peer-net9-<i>-), in order, byte-exact,
  * kumo's echo of every peer datagram back, byte-exact (the peer does not echo),
  * the zero-length and 1472-byte maximum datagrams present in both initiations.
The UDP length field is the authority, so Ethernet padding never counts.

Usage: scripts/net9-proof.py PEER.pcap [--port 7701] [--count 24] [--self-test]
"""

import struct
import sys

PORT = 7701
COUNT = 24


def payload(side, i):
    prefix = f'{side}-net9-{i}-'.encode()
    if i == 0:
        return b''
    if i == 1:
        return prefix + b'\x00' * (1472 - len(prefix))
    return prefix + b'x' * (i % 16)


def read_pcap(path):
    data = open(path, 'rb').read()
    if len(data) < 24:
        sys.exit('pcap too short')
    magic = struct.unpack_from('<I', data, 0)[0]
    if magic in (0xA1B2C3D4, 0xA1B23C4D):
        endian = '<'
    elif magic in (0xD4C3B2A1, 0x4D3CB2A1):
        endian = '>'
    else:
        sys.exit(f'not a classic pcap (magic {magic:#010x})')
    at = 24
    while at + 16 <= len(data):
        _, _, incl, _ = struct.unpack_from(endian + 'IIII', data, at)
        at += 16
        yield data[at:at + incl]
        at += incl


def udp_frame(frame):
    """(src_ip, dst_ip, sport, dport, payload) for an untagged IPv4/UDP frame."""
    if len(frame) < 34 or frame[12:14] != b'\x08\x00':
        return None
    if frame[23] != 17:
        return None
    ihl = (frame[14] & 0x0F) * 4
    if len(frame) < 14 + ihl + 8:
        return None
    src, dst = frame[26:30], frame[30:34]
    sport, dport, ulen = struct.unpack_from('>HHH', frame, 14 + ihl)
    start = 14 + ihl + 8
    end = min(len(frame), 14 + ihl + ulen)
    return src, dst, sport, dport, frame[start:end]


def check(frames, port, count):
    """Return (ok, report lines) for a frame list; the self-test feeds synthetic frames.

    The implemented exchange has three flows: kumo's numbered initiations, the peer's
    numbered initiations, and kumo echoing each peer datagram back byte-exact. The peer
    role (scripts/net9-peer.py) verifies kumo's datagrams at its own socket and does not
    echo them, so a wire peer_echo flow is a protocol violation, not a missing flow.

    The zero-length datagram is byte-identical to its echo, so only counts tell them
    apart: kumo->peer must carry two empties (kumo's original and kumo's echo of the
    peer's) and peer->kumo exactly one (the peer's original).
    """
    problems = []
    flows = {'kumo_orig': [], 'peer_orig': [], 'kumo_echo': [], 'peer_echo': []}
    empties = {'kumo_to_peer': 0, 'peer_to_kumo': 0}
    exp_k = [payload('kumo', i) for i in range(count)]
    exp_p = [payload('peer', i) for i in range(count)]
    for frame in frames:
        u = udp_frame(frame)
        if not u:
            continue
        _, _, sport, dport, data = u
        if dport == port:
            if len(data) == 0:
                empties['kumo_to_peer'] += 1
            elif data in exp_k:
                flows['kumo_orig'].append(exp_k.index(data))
            elif data in exp_p:
                flows['kumo_echo'].append(exp_p.index(data))
            else:
                problems.append(f'unexpected kumo->peer datagram {len(data)} B')
        elif sport == port:
            if len(data) == 0:
                empties['peer_to_kumo'] += 1
            elif data in exp_p:
                flows['peer_orig'].append(exp_p.index(data))
            elif data in exp_k:
                flows['peer_echo'].append(exp_k.index(data))
            else:
                problems.append(f'unexpected peer->kumo datagram {len(data)} B')
    for direction, want in (('kumo_to_peer', 2), ('peer_to_kumo', 1)):
        if empties[direction] != want:
            problems.append(
                f'{direction}: {empties[direction]} zero-length datagrams, want {want}')
    for name in ('kumo_orig', 'peer_orig', 'kumo_echo'):
        got = flows[name]
        for i in range(1, count):
            n = got.count(i)
            if n == 0:
                problems.append(f'{name}: index {i} missing')
            elif n > 1:
                problems.append(f'{name}: index {i} duplicated x{n}')
        if got != sorted(got):
            problems.append(f'{name}: out of order {got}')
    for name in ('kumo_orig', 'peer_orig'):
        table = exp_k if name == 'kumo_orig' else exp_p
        sizes = {len(table[idx]) for idx in flows[name]}
        if 1472 not in sizes:
            problems.append(f'{name}: 1472-byte datagram missing')
    if flows['peer_echo']:
        problems.append(
            f'peer_echo: {len(flows["peer_echo"])} datagram(s); the peer does not echo')
    report = [f'{name}: {len(flow)}' for name, flow in flows.items()]
    report += [f'{name}: {n}' for name, n in empties.items()]
    return (not problems), problems + report


def self_test():
    def ethernet(dst, src, payload):
        return dst + src + b'\x08\x00' + payload

    def ipv4(src, dst, ulen):
        header = struct.pack('>BBHHHBBH4s4s', 0x45, 0, 20 + ulen, 0, 0, 64, 17, 0,
                             src, dst)
        return header

    def udp(sport, dport, data):
        return struct.pack('>HHHH', sport, dport, 8 + len(data), 0) + data

    kumo_ip, peer_ip = bytes([192, 168, 0, 50]), bytes([192, 168, 0, 106])
    kmac, pmac = b'\x02\x00\x00\x00\x00\x50', b'\x02\x00\x00\x00\x00\x66'
    frames = []
    for i in range(COUNT):
        k = payload('kumo', i)
        p = payload('peer', i)
        frames.append(ethernet(pmac, kmac, ipv4(kumo_ip, peer_ip, 8 + len(k)) +
                               udp(49152, PORT, k)))
        frames.append(ethernet(kmac, pmac, ipv4(peer_ip, kumo_ip, 8 + len(p)) +
                               udp(PORT, 53000, p)))
        frames.append(ethernet(pmac, kmac, ipv4(kumo_ip, peer_ip, 8 + len(p)) +
                               udp(49152, PORT, p)))
    ok, report = check(frames, PORT, COUNT)
    print('self-test positive:', 'PASS' if ok else 'FAIL', report)
    good = ok

    # Negative: drop the frame carrying kumo's echo of peer index 23.
    target = payload('peer', COUNT - 1)
    broken = []
    dropped = False
    for f in frames:
        u = udp_frame(f)
        if not dropped and u and u[3] == PORT and u[4] == target:
            dropped = True
            continue
        broken.append(f)
    assert dropped, 'negative self-test could not drop a frame'
    ok, report = check(broken, PORT, COUNT)
    print('self-test negative:', 'PASS' if not ok else 'FAIL', report)
    good = good and not ok

    # Negative: a fabricated peer echo; the peer script never sends kumo payloads.
    k1 = payload('kumo', 1)
    stray = frames + [ethernet(kmac, pmac, ipv4(peer_ip, kumo_ip, 8 + len(k1)) +
                               udp(PORT, 53000, k1))]
    ok, report = check(stray, PORT, COUNT)
    print('self-test peer-echo:', 'PASS' if not ok else 'FAIL', report)
    return good and not ok


def main(argv):
    opts = {'--port': str(PORT), '--count': str(COUNT)}
    path = None
    it = iter(argv[1:])
    for arg in it:
        if arg == '--self-test':
            continue
        if arg in opts:
            try:
                opts[arg] = next(it)
            except StopIteration:
                sys.exit(f'{arg} needs a value')
        elif arg.startswith('--'):
            sys.exit(f'unknown option {arg}')
        elif path is None:
            path = arg
        else:
            sys.exit(f'unexpected argument {arg}')
    port, count = int(opts['--port']), int(opts['--count'])
    if '--self-test' in argv[1:]:
        sys.exit(0 if self_test() else 1)
    if path is None:
        sys.exit('usage: scripts/net9-proof.py PEER.pcap [--port n] [--count n] [--self-test]')
    ok, report = check(list(read_pcap(path)), port, count)
    for line in report:
        print(line)
    if ok:
        print(f'NET9 proof green: {count}/{count} exact datagrams each way, '
              f"kumo's echo of every peer datagram, zero and 1472 present")
    else:
        print('NET9 proof RED')
    sys.exit(0 if ok else 1)


if __name__ == '__main__':
    main(sys.argv)
