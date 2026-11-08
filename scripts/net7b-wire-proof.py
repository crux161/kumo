#!/usr/bin/env python3
"""The NET7b wire proof: netstack-originated frames byte-exact on the peer capture.

netprobe `wire` sends datagrams whose payloads are `kumo-net7b-<i>-` plus `i % 16` fill
bytes. This checker reads a classic pcap, takes the last run of traffic from the wire
address, and requires:

  * one ARP request for the peer from the wire address (the neighbour discover),
  * exactly the expected datagrams, in order, byte-exact, to the peer's port,
  * no duplicates, and optionally the peer's ARP reply.

Usage: scripts/net7b-wire-proof.py PEER.pcap [--wire 192.168.0.50] [--peer 192.168.0.106]
                                              [--port 7701] [--count 24]
"""

import struct
import sys

WIRE = '192.168.0.50'
PEER = '192.168.0.106'
PORT = 7701
COUNT = 24


def ip(text):
    return bytes(int(part) for part in text.split('.'))


def read_pcap(path):
    """Yield Ethernet frames from a classic pcap, either endianness, us or ns stamps."""
    data = open(path, 'rb').read()
    magic = struct.unpack_from('<I', data, 0)[0]
    if magic in (0xA1B2C3D4, 0xA1B23C4D):
        endian, header = '<', 24
    elif magic in (0xD4C3B2A1, 0x4D3CB2A1):
        endian, header = '>', 24
    else:
        sys.exit(f'not a classic pcap (magic {magic:#010x})')
    at = header
    while at + 16 <= len(data):
        _, _, incl, _ = struct.unpack_from(endian + 'IIII', data, at)
        at += 16
        yield data[at:at + incl]
        at += incl


def udp_frame(frame):
    """(src_ip, dst_ip, sport, dport, payload) for an untagged IPv4/UDP Ethernet frame."""
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
    # Short frames are zero-padded to Ethernet's 60-byte minimum; the UDP length field is
    # the authority on the true payload.
    return src, dst, sport, dport, frame[start:end]


def arp_frame(frame):
    """(src_ip, target_ip, opcode) for an untagged ARP Ethernet frame."""
    if len(frame) < 42 or frame[12:14] != b'\x08\x06':
        return None
    opcode = struct.unpack_from('>H', frame, 20)[0]
    return frame[28:32], frame[38:42], opcode


def expected(count):
    return [b'kumo-net7b-' + str(i).encode() + b'-' + b'x' * (i % 16) for i in range(count)]


def main(argv):
    opts = {'--wire': WIRE, '--peer': PEER, '--port': str(PORT), '--count': str(COUNT)}
    args = argv[1:]
    path = None
    while args:
        arg = args.pop(0)
        if arg in opts:
            opts[arg] = args.pop(0)
        else:
            path = arg
    if path is None:
        sys.exit(__doc__)
    wire, peer = ip(opts['--wire']), ip(opts['--peer'])
    port, count = int(opts['--port']), int(opts['--count'])

    skip_arp = peer[3] == 255  # broadcast target: smoltcp sends without neighbor discovery
    requests, replies = [], []
    frames = []
    for index, frame in enumerate(read_pcap(path)):
        arp = arp_frame(frame)
        if arp:
            src, target, opcode = arp
            if src == wire and target == peer:
                (requests if opcode == 1 else replies).append(index)
            continue
        parsed = udp_frame(frame)
        if parsed:
            frames.append((index, parsed))
    if not requests and not skip_arp:
        sys.exit(f'no ARP request from {opts["--wire"]} for {opts["--peer"]}')

    # One boot may hold several runs: judge the last one. A broadcast run has no ARP
    # handshake to anchor it, so judge the whole capture.
    start = requests[-1] if requests else -1
    datagrams = [entry for entry in frames if entry[0] > start and entry[1][0] == wire
                 and entry[1][1] == peer and entry[1][3] == port]
    want = expected(count)
    if len(datagrams) != count:
        print(f'FAIL: {len(datagrams)} datagrams to {opts["--peer"]}:{port} after the last ARP, want {count}')
        for entry in datagrams:
            print('  got', entry[1][4])
        return 1
    bad = 0
    port_of = datagrams[0][1][2]
    for i, (index, (src, dst, sport, dport, payload)) in enumerate(datagrams):
        if payload != want[i]:
            bad += 1
            print(f'FAIL frame {i}: payload {payload!r} want {want[i]!r}')
        if sport != port_of:
            bad += 1
            print(f'FAIL frame {i}: source port changed {sport} != {port_of}')
    if bad:
        return 1
    print(f'NET7b wire proof green: {count}/{count} netstack datagrams captured byte-exact '
          f'({opts["--wire"]}:{port_of} -> {opts["--peer"]}:{port}), ARP resolved, '
          f'{len(replies)} ARP repl{"" if len(replies) == 1 else "ies"}')
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
