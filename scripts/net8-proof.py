#!/usr/bin/env python3
"""The NET8 proof: static IPv4 ARP/ICMP, checked on the peer capture.

From one classic pcap, requires:

  * one complete ARP exchange (a request and its reply) in at least one direction,
  * every kumo-originated echo request (data `kumo-net8-<i>-...`) answered byte-exact
    by the peer, for all `--count` indices in order,
  * `--peer-count` peer-originated echo requests each answered by kumo itself
    (the interface's auto-icmp-echo-reply),
  * every matched ICMP message carries a valid checksum.

Usage: scripts/net8-proof.py PEER.pcap [--wire 192.168.0.50] [--peer 192.168.0.106]
                               [--count 24] [--peer-count 4] [--self-test]
"""

import struct
import sys

WIRE = '192.168.0.50'
PEER = '192.168.0.106'
COUNT = 24
PEER_COUNT = 4


def ip(text):
    return bytes(int(part) for part in text.split('.'))


def read_pcap(path):
    """Yield Ethernet frames from a classic pcap, either endianness, us or ns stamps."""
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


def checksum(data):
    if len(data) % 2:
        data += b'\0'
    total = 0
    for i in range(0, len(data), 2):
        total += (data[i] << 8) | data[i + 1]
    while total >> 16:
        total = (total & 0xFFFF) + (total >> 16)
    return (~total) & 0xFFFF


def arp_frame(frame):
    """(src_ip, target_ip, opcode) for an untagged ARP Ethernet frame."""
    if len(frame) < 42 or frame[12:14] != b'\x08\x06':
        return None
    opcode = struct.unpack_from('>H', frame, 20)[0]
    return frame[28:32], frame[38:42], opcode


def icmp_frame(frame):
    """(src_ip, dst_ip, kind, ident, seq_no, data, checksum_ok) for an IPv4/ICMP frame."""
    if len(frame) < 34 or frame[12:14] != b'\x08\x00':
        return None
    if frame[23] != 1:
        return None
    ihl = (frame[14] & 0x0F) * 4
    if len(frame) < 14 + ihl + 8:
        return None
    src, dst = frame[26:30], frame[30:34]
    icmp = frame[14 + ihl:]
    kind = icmp[0]
    ident, seq_no = struct.unpack_from('>HH', icmp, 4)
    data = icmp[8:]
    return src, dst, kind, ident, seq_no, data, checksum(icmp) == 0


def expected(index):
    return b'kumo-net8-' + str(index).encode() + b'-' + b'x' * (index % 16)


def check(frames, wire, peer, count, peer_count):
    """Return (ok, report lines) for a frame list; the self-test feeds synthetic frames."""
    problems = []
    arp = [a for a in (arp_frame(f) for f in frames) if a]
    had_reply = {(src, target) for src, target, op in arp if op == 2}
    exchanges = [('peer', peer, wire), ('wire', wire, peer)]
    complete = [
        name
        for name, asker, answerer in exchanges
        if any(src == asker and target == answerer and op == 1 for src, target, op in arp)
        and (answerer, asker) in had_reply
    ]
    if not complete:
        problems.append('no complete ARP exchange in either direction')
    else:
        problems_note = f'ARP exchange complete ({", ".join(complete)})'

    icmp = [i for i in (icmp_frame(f) for f in frames) if i]
    bad_checksums = [i for i in icmp if not i[6]]
    if bad_checksums:
        problems.append(f'{len(bad_checksums)} ICMP message(s) with a bad checksum')

    kumo_answers = {}
    peer_answers = {}
    for src, dst, kind, ident, seq_no, data, _ in icmp:
        key = (ident, seq_no, data)
        if kind == 8 and src == wire and dst == peer:
            kumo_answers.setdefault(data, []).append(key)
        elif kind == 0 and src == peer and dst == wire:
            pass  # replies are matched from the request side below
    reply_keys = {
        (ident, seq_no, data)
        for src, dst, kind, ident, seq_no, data, _ in icmp
        if kind == 0 and src == peer and dst == wire
    }
    for index in range(count):
        key = None
        for data, keys in kumo_answers.items():
            if data == expected(index):
                key = keys[0]
                break
        if key is None:
            problems.append(f'no kumo echo request for index {index}')
            break
        if key not in reply_keys:
            problems.append(f'no peer echo reply for index {index}')
            break

    peer_requests = [
        (ident, seq_no, data)
        for src, dst, kind, ident, seq_no, data, _ in icmp
        if kind == 8 and src == peer and dst == wire
    ]
    peer_replies = {
        (ident, seq_no, data)
        for src, dst, kind, ident, seq_no, data, _ in icmp
        if kind == 0 and src == wire and dst == peer
    }
    answered = [key for key in peer_requests if key in peer_replies]
    if len(answered) < peer_count:
        problems.append(
            f'{len(answered)} peer ping(s) answered by kumo, want {peer_count}'
        )

    report = [
        f'kumo-originated requests seen: {sum(len(v) for v in kumo_answers.values())}',
        f'peer-originated pings answered by kumo: {len(answered)}',
    ]
    if not problems:
        report.insert(0, problems_note)
    return (not problems), problems + report


def self_test():
    def ethernet(dst, src, ethertype, payload):
        return dst + src + struct.pack('>H', ethertype) + payload

    def ipv4(src, dst, payload):
        header = struct.pack(
            '>BBHHHBBH4s4s',
            0x45,
            0,
            20 + len(payload),
            0,
            0,
            64,
            1,
            0,
            src,
            dst,
        )
        header = header[:10] + struct.pack('>H', checksum(header)) + header[12:]
        return header + payload

    def icmp(kind, ident, seq_no, data):
        body = struct.pack('>BBHHH', kind, 0, 0, ident, seq_no) + data
        body = body[:2] + struct.pack('>H', checksum(body)) + body[4:]
        return body

    wire, peer = ip(WIRE), ip(PEER)
    wmac, pmac = b'\x02\x00\x00\x00\x00\x50', b'\x02\x00\x00\x00\x00\x66'
    frames = []
    # ARP both ways.
    def arp(op, sha, spa, tha, tpa):
        return struct.pack('>HHBBH6s4s6s4s', 1, 0x0800, 6, 4, op, sha, spa, tha, tpa)

    frames.append(ethernet(b'\xff' * 6, pmac, 0x0806, arp(1, pmac, peer, b'\0' * 6, wire)))
    frames.append(ethernet(pmac, wmac, 0x0806, arp(2, wmac, wire, pmac, peer)))
    frames.append(ethernet(b'\xff' * 6, wmac, 0x0806, arp(1, wmac, wire, b'\0' * 6, peer)))
    frames.append(ethernet(wmac, pmac, 0x0806, arp(2, pmac, peer, wmac, wire)))
    # Both ping directions, all replies.
    for index in range(COUNT):
        data = expected(index)
        frames.append(ethernet(pmac, wmac, 0x0800, ipv4(wire, peer, icmp(8, 0x4B55, index, data))))
    for index in range(COUNT):
        data = expected(index)
        frames.append(ethernet(wmac, pmac, 0x0800, ipv4(peer, wire, icmp(0, 0x4B55, index, data))))
    for index in range(PEER_COUNT):
        data = b'macos ping payload %d' % index
        frames.append(ethernet(wmac, pmac, 0x0800, ipv4(peer, wire, icmp(8, 0x0100 + index, index, data))))
        frames.append(ethernet(pmac, wmac, 0x0800, ipv4(wire, peer, icmp(0, 0x0100 + index, index, data))))

    ok, report = check(frames, wire, peer, COUNT, PEER_COUNT)
    print('self-test positive:', 'PASS' if ok else 'FAIL', report)
    good = ok

    # Negative: drop kumo's last reply.
    broken = frames[:]
    drop_at = None
    for i, frame in enumerate(broken):
        parsed = icmp_frame(frame) if frame[12:14] == b'\x08\x00' else None
        if parsed and parsed[2] == 0 and parsed[0] == peer and parsed[5] == expected(COUNT - 1):
            drop_at = i
    assert drop_at is not None
    broken.pop(drop_at)
    ok, report = check(broken, wire, peer, COUNT, PEER_COUNT)
    print('self-test negative:', 'PASS' if not ok else 'FAIL', report)
    return good and not ok


def main(argv):
    opts = {'--wire': WIRE, '--peer': PEER, '--count': str(COUNT), '--peer-count': str(PEER_COUNT)}
    self_t = '--self-test' in argv[1:]
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
    wire, peer = ip(opts['--wire']), ip(opts['--peer'])
    count, peer_count = int(opts['--count']), int(opts['--peer-count'])
    if self_t:
        sys.exit(0 if self_test() else 1)
    if path is None:
        sys.exit('usage: scripts/net8-proof.py PEER.pcap [--wire W] [--peer P] [--count n] [--peer-count n] [--self-test]')
    ok, report = check(list(read_pcap(path)), wire, peer, count, peer_count)
    for line in report:
        print(line)
    if ok:
        print(f'NET8 proof green: ARP exchange, {count}/{count} kumo pings answered, '
              f'{peer_count}/{peer_count} peer pings answered')
    else:
        print('NET8 proof RED')
    sys.exit(0 if ok else 1)


if __name__ == '__main__':
    main(sys.argv)
