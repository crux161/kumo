#!/usr/bin/env python3
#j834
"""NET6 passive RX: the peer sender and the capture checker.

  send   broadcast the numbered test datagrams from this host (no root needed)
  check  verify a KUMO serial capture against the sender manifest and, optionally,
         an independent packet capture (pcap/pcapng) of the peer's own frames

Payload sizes and bytes follow from the sequence number alone, exactly as in
lib/kumo-ax88/src/probe.rs; --self-test pins the same golden values as its tests.
"""
import argparse
import errno
import functools
import importlib.util
import json
from pathlib import Path
import re
import socket
import struct
import sys
import time
import unittest

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("ax88_link_proof", HERE / "ax88-link-proof.py")
link = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(link)
require, Cursor, live_lines, le, masked = link.require, link.Cursor, link.live_lines, link.le, link.masked

PORT = 50606
MAGIC = b"KUMONET6"
EXACT, BURST, END = 64, 4096, 0xFFFFFFFF
IP_BOUND_IF = 25  # macOS <netinet/in.h>
FORBIDDEN = ["NET5b FAIL", "MISMATCH", "NET5b status missed", "NET5b cable", "TOWER", "ABEND", "PANIC",
             "NET4b completion fault", "NET4c3 nic FAIL", "NET4c3 resident", "NET4c3 cycle=", "NET4c3 nic rx bytes=",
             "NET5a KUMO start", "NET4a EP0 start", " parse=", "verdict=IpChecksum", "verdict=UdpChecksum",
             "verdict=Length", "verdict=Payload"]


def payload_len(seq):
    if seq == END:
        return 18
    if seq < 16:
        return 18 + seq
    if seq < 32:
        return 454 + seq - 16
    if seq < 48:
        return 966 + seq - 32
    if seq < 64:
        return 1457 + seq - 48
    return 18 + (seq * 389) % 1455


def pattern(seq, i):
    return (((seq & 0xff) + (i * 13 & 0xff)) & 0xff) ^ ((i >> 8) & 0xff)


@functools.lru_cache(maxsize=None)
def payload(seq):
    n = payload_len(seq)
    head = MAGIC + struct.pack("<IHBB", seq, n, 1, 0)
    return head + bytes(pattern(seq, i) for i in range(n - 16))


def fnv64(data):
    h = 0xcbf29ce484222325
    for b in data:
        h = ((h ^ b) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return h


def crc32(data):
    c = 0xFFFFFFFF
    for b in data:
        c ^= b
        for _ in range(8):
            c = (c >> 1) ^ (0xEDB88320 & -(c & 1))
    return ~c & 0xFFFFFFFF


@functools.lru_cache(maxsize=None)
def payload_fnv(seq):
    return fnv64(payload(seq))


def burst_term(seq, payload_fnv):
    return fnv64(struct.pack("<IQ", seq, payload_fnv))


def ones(data, total=0):
    if len(data) % 2:
        data += b"\0"
    for i in range(0, len(data), 2):
        total += data[i] << 8 | data[i + 1]
    while total > 0xFFFF:
        total = (total & 0xFFFF) + (total >> 16)
    return total


def checksums_ok(frame):
    ip, total = frame[14:], struct.unpack(">H", frame[16:18])[0]
    udp = ip[20:total]
    pseudo = ones(ip[12:20], 17 + len(udp))
    return ones(ip[:20]) == 0xFFFF and (udp[6:8] == b"\0\0" or ones(udp, pseudo) == 0xFFFF)


# ---------------------------------------------------------------- sender

def send(args):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    s.setsockopt(socket.IPPROTO_IP, IP_BOUND_IF, socket.if_nametoindex(args.interface))
    s.bind((args.source, 0))
    target = (args.broadcast, PORT)
    retries = 0

    def put(seq):
        nonlocal retries
        data = payload(seq)
        for _ in range(100000):
            try:
                s.sendto(data, target)
                return
            except OSError as error:
                if error.errno != errno.ENOBUFS:
                    raise
                retries += 1
                time.sleep(0.0002)
        raise RuntimeError(f"seq {seq} never left the interface queue")

    manifest = dict(interface=args.interface, source_ip=args.source, source_port=s.getsockname()[1],
                    broadcast=args.broadcast, port=PORT, started=time.time())
    time.sleep(args.idle)
    t = time.monotonic()
    for seq in range(EXACT):
        put(seq)
        time.sleep(args.gap)
    manifest["exact_sent"], manifest["exact_seconds"] = EXACT, round(time.monotonic() - t, 3)
    time.sleep(1)
    t = time.monotonic()
    for seq in range(EXACT, EXACT + BURST):
        put(seq)
    manifest["burst_sent"], manifest["burst_seconds"] = BURST, round(time.monotonic() - t, 4)
    manifest["burst_bytes"] = sum(payload_len(q) + 42 for q in range(EXACT, EXACT + BURST))
    time.sleep(1)
    for _ in range(args.ends):
        put(END)
        time.sleep(0.2)
    manifest.update(end_sent=args.ends, enobufs_retries=retries, finished=time.time())
    args.manifest.write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest))
    return 0


# ---------------------------------------------------------------- capture parsing

def pcap_frames(path):
    """Ethernet frames from a classic pcap or pcapng file, in capture order."""
    data = path.read_bytes()
    frames = []
    if data[:4] in (b"\xd4\xc3\xb2\xa1", b"\xa1\xb2\xc3\xd4", b"\x4d\x3c\xb2\xa1", b"\xa1\xb2\x3c\x4d"):
        order = "<" if data[:4] in (b"\xd4\xc3\xb2\xa1", b"\x4d\x3c\xb2\xa1") else ">"
        require(struct.unpack(order + "I", data[20:24])[0] == 1, "pcap is not Ethernet")
        at = 24
        while at + 16 <= len(data):
            incl, orig = struct.unpack(order + "II", data[at + 8:at + 16])
            require(incl == orig, "truncated capture record (use -s 0)")
            frames.append(data[at + 16:at + 16 + incl])
            at += 16 + incl
    elif data[:4] == b"\x0a\x0d\x0d\x0a":
        order = "<" if data[8:12] == b"\x4d\x3c\x2b\x1a" else ">"
        at = 0
        while at + 12 <= len(data):
            kind, length = struct.unpack(order + "II", data[at:at + 8])
            if kind == 1:
                require(struct.unpack(order + "H", data[at + 8:at + 10])[0] == 1, "pcapng is not Ethernet")
            if kind == 6:
                incl, orig = struct.unpack(order + "II", data[at + 20:at + 28])
                require(incl == orig, "truncated capture record (use -s 0)")
                frames.append(data[at + 28:at + 28 + incl])
            require(length >= 12, "corrupt pcapng block")
            at += length
    else:
        raise ValueError("capture is neither pcap nor pcapng")
    return frames


def test_seq(frame):
    if len(frame) < 58 or frame[12:14] != b"\x08\x00" or frame[23] != 17:
        return None
    if struct.unpack(">H", frame[36:38])[0] != PORT or frame[42:50] != MAGIC:
        return None
    return struct.unpack("<I", frame[50:54])[0]


# ---------------------------------------------------------------- checker

def receive_init(c, controller, slot):
    """NET5b's init and receive-goal monitor, recomputed independently."""
    def write(step, reg, value, mask):
        found = c.take(rf"NET5b write step={step} reg=0x{reg} value={value} readback=([0-9a-f]{{{len(value)}}}) us=(\d+) PASS", step)
        require(masked(value, found[1], mask), f"{step}: readback {found[1]} misses named bits")

    res = c.take(rf"NET5b resident controller={controller} slot={slot} tick_ms=100 hid_idle_cc=(Some\(\d+\)|None) lanes=(\d+) custody=(\d+)", "resident start")
    c.take(r"NET5b start policy=sw-checksum,unicast\+broadcast,receive-on phy_writes=0 goal=receive window_s=600", "receive start")
    baseline = {reg: c.take(rf"NET5b baseline reg=0x{reg} bytes=([0-9a-f]{{{2 * width}}})", f"baseline {reg}")[1]
                for reg, width in link.BASELINE}
    for step in link.INIT[:2]:
        write(*step)
    require(int(c.take(r"NET5b settled step=phy ms=(\d+)", "PHY settle")[1]) >= 500, "PHY settle short")
    write(*link.INIT[2])
    require(int(c.take(r"NET5b settled step=clock ms=(\d+)", "clock settle")[1]) >= 200, "clock settle short")
    phy = c.take(rf"NET5b phy id=([0-9a-f]{{8}}) page=0 bmcr={link.H4} bmsr={link.H4} anar={link.H4} gbcr={link.H4} attempts=(\d+) ready_ms=(\d+)", "PHY identity")
    require(phy[1] not in ("00000000", "ffffffff"), "PHY identity unavailable")
    anar, gbcr = int(phy[4], 16), int(phy[5], 16)
    for step in link.POLICY:
        write(*step)
    c.take(r"NET5b init PASS reads=22 writes=7 ms=(\d+)", "init")
    state, ups, downs, modes = None, 0, 0, []
    while not c.peek().startswith("NET5b monitor "):
        t = c.take(r"NET5b link (up|down|initial-down) n=(\d+) ms=(\d+) since_ms=(\d+) physr=(0x[0-9a-f]{4})", "link transition")
        kind, physr, up = t[1], int(t[5], 16), t[1] == "up"
        require(bool(physr & 0x400) == up and state != up, "transition disagrees with PHYSR or repeats")
        require(kind != "initial-down" or state is None, "late initial-down")
        if up:
            ups += 1
            g = c.take(rf"NET5b negotiated speed=(\d+) full=([01]) hcd=(\S+) bmsr={link.H4} anar={link.H4} anlpar={link.H4} gbcr={link.H4} gbsr={link.H4} usb=(0x[0-9a-f]{{2}}) agrees=1", "negotiation")
            bmsr, l_anar, anlpar, l_gbcr, gbsr = (int(g[k], 16) for k in range(4, 9))
            require((l_anar, l_gbcr) == (anar, gbcr), "advertisement changed without a PHY write")
            want = link.expected(physr, int(g[9], 16))
            hcd = link.resolve(bmsr, anar, anlpar, gbcr, gbsr)
            require(want and hcd == want[:2] and g[3] == link.label(hcd), "resolution disagrees with PHYSR")
            write("rx-reset", "0b", "0000", 0xffff)
            write("rx-control", "0b", "8802", 0xffff)
            write("queue", "2e", want[2], 0xff_ffff_ffff)
            write("medium-receive", "22", (le(want[3]) | 0x0100).to_bytes(2, "little").hex(), 0xffff)
            modes.append(link.label(hcd))
        elif kind == "down":
            downs += 1
        state = up
        if kind != "initial-down":
            a = c.take(r"NET5b status agrees link=([01]) lag_ms=(-?\d+) reports=(\d+)", "status agreement")
            require(int(a[1]) == up, "status endpoint reports the other state")
    require(state is True and ups > 0, "receive window ended without link")
    m = c.take(r"NET5b monitor complete goal=receive concluded=1 ups=(\d+) downs=(\d+) agreed=(\d+) missed=0 disagreed=0 ms=(\d+)", "monitor end")
    require([int(m[k]) for k in (1, 2, 3)] == [ups, downs, ups + downs], "monitor totals")
    write("park-medium", "22", "0000", 0xffff)
    for step in link.PARK:
        write(*step)
    c.take(r"NET5b parked power=([0-9a-f]{4}) clock=00 rx_control=0000 phy=Unavailable", "parked")
    c.take(rf"NET5b result PASS goal=receive reads=(\d+) writes=(\d+) ups={ups} downs={downs} agreed={ups + downs} missed=0 disagreed=0 max_transfer_us=(\d+) init=1 receive=1 concluded=1 parked=1", "result")
    s = c.take(rf"NET5b service controller={controller} ticks=(\d+) ms=(\d+) hid_reports=(\d+) hid_max_gap_ms=(\d+) busy_max_us=(\d+) tick_late_max_ms=(\d+) status_reports=(\d+) rx_completions=(\d+)", "service")
    require(res[1] != "Some(1)" or int(s[3]) > 0, "no HID report with a 100 ms idle keyboard")
    require(c.i == len(c.lines), f"unknown trailing NET5b record {c.peek()!r}")
    return dict(baseline=baseline, phy_id=phy[1], modes=modes, ups=ups, downs=downs, lanes=res[2], custody=res[3],
                service=dict(zip(["ticks", "ms", "hid_reports", "hid_max_gap_ms", "busy_max_us", "tick_late_max_ms",
                                  "status_reports", "rx_completions"], map(int, s.groups()))))


def check(text, manifest, captured=None):
    lines = live_lines(text)
    body = "\n".join(lines)

    def one(pattern, what):
        found = [m for m in (re.fullmatch(pattern, l) for l in lines) if m]
        require(len(found) == 1, f"need one {what}, found {len(found)}")
        return found[0]

    for word in FORBIDDEN:
        require(word not in body, f"forbidden record: {word}")
    require("INDRA login:" in body, "no login")
    native = re.findall(r"NET0 ax88 controller=(0x[0-9a-f]+) root=\d+ route=0x[0-9a-f]+ slot=(\d+) speed=3 vid=0b95 pid=1790 exact=true", body)
    require(len(native) == 1, "need one exact native inventory")
    controller, slot = native[0]
    for name, accepted in [("DEVICE", link.DEVICE), ("CONFIG", link.CONFIG)]:
        data = bytearray()
        for offset, chunk in re.findall(rf"NET0 ax88 ctrl={controller} slot={slot} {name} offset=(\d+) ([0-9a-f]+)", body):
            require(int(offset) == len(data), f"native {name} gap/duplicate")
            data.extend(bytes.fromhex(chunk))
        require(data.hex() == accepted, f"native {name} differs from NET0")
    require(body.count("drv-xhci: nic irq-driven; rx+status lanes resident") == 1, "no resident lanes")
    net5b = [l for l in lines if l.startswith("NET5b ")]
    net6 = [l for l in lines if l.startswith("NET6 ")]
    require(all(l.startswith(("NET5b ", "NET6 ")) for l in lines if "NET5b " in l or "NET6 " in l), "malformed record")
    init = receive_init(Cursor(net5b), controller, slot)
    # Order: receive on, every frame, receive off, result, then NET6's summary.
    order = {k: next((i for i, l in enumerate(lines) if l.startswith(k)), None) for k in
             ["NET5b write step=medium-receive", "NET5b write step=park-medium", "NET5b result", "NET6 rx aggregates", "NET6 census"]}
    require(None not in order.values(), "missing receive window boundary")
    frames_at = [i for i, l in enumerate(lines) if l.startswith(("NET6 frame", "NET6 aggregate"))]
    require(frames_at and order["NET5b write step=medium-receive"] < frames_at[0] and frames_at[-1] < order["NET5b write step=park-medium"],
            "frames outside the receive window")
    require(order["NET5b result"] < order["NET6 rx aggregates"] < order["NET6 census"], "summary order")

    exact, ends, macs, fcs_flags, aggregates = {}, 0, set(), set(), []
    for l in net6:
        f = re.fullmatch(r"NET6 frame seq=(\d+) len=(\d+) fnv=([0-9a-f]{16}) trailing=(\d+) fcs=([01]) verdict=Valid hdr=([0-9a-f]{84})", l)
        if f:
            seq, length, fnv, trailing, fcs, hdr = int(f[1]), int(f[2]), f[3], int(f[4]), f[5] == "1", bytes.fromhex(f[6])
            require(seq < EXACT or seq == END, "burst frame logged individually")
            require(hdr[:6] == b"\xff" * 6 and hdr[12:14] == b"\x08\x00" and hdr[14] == 0x45 and hdr[23] == 17, "not a broadcast IPv4/UDP header")
            require(socket.inet_ntoa(hdr[26:30]) == manifest["source_ip"] and socket.inet_ntoa(hdr[30:34]) == manifest["broadcast"], "IP addresses differ from the sender")
            require(struct.unpack(">HH", hdr[34:38]) == (manifest["source_port"], PORT), "UDP ports differ from the sender")
            body_frame = hdr + payload(seq)
            require(struct.unpack(">H", hdr[16:18])[0] == 28 + payload_len(seq) and struct.unpack(">H", hdr[38:40])[0] == 8 + payload_len(seq), "IP/UDP length differs from the schedule")
            require(checksums_ok(body_frame), "reconstructed checksums invalid")
            wire = body_frame + (struct.pack("<I", crc32(body_frame)) if fcs else b"")
            require(trailing == (4 if fcs else 0) and length == len(wire), "length/trailer differs from the reconstruction")
            require(int(fnv, 16) == fnv64(wire), f"seq {seq}: received bytes differ from the sender's frame")
            macs.add(hdr[6:12])
            fcs_flags.add(fcs)
            if seq == END:
                ends += 1
            else:
                require(seq not in exact, f"seq {seq} logged twice")
                exact[seq] = dict(len=length, wire=wire)
            continue
        a = re.fullmatch(r"NET6 aggregate n=(\d+) bytes=(\d+) frames=(\d+) dropped=(\d+) dummies=(\d+)", l)
        if a:
            aggregates.append(tuple(map(int, a.groups())))
            continue
        require(re.match(r"NET6 (rx aggregates=|rx test |rx bitmap |census )", l), f"unknown NET6 record {l!r}")
    require(sorted(exact) == list(range(EXACT)), f"exact phase incomplete: {sorted(set(range(EXACT)) - set(exact))}")
    require(len(macs) == 1 and len(fcs_flags) == 1, "inconsistent peer MAC or FCS pass-through")
    r = one(r"NET6 rx aggregates=(\d+) parse_errors=0 frames=(\d+) dropped=(\d+) dummies=(\d+) max_frames=(\d+) max_bytes=(\d+) arp=(\d+) ipv4=(\d+) ipv6=(\d+) other=(\d+) rx_completions=(\d+)", "aggregate summary")
    totals = dict(zip(["aggregates", "frames", "dropped", "dummies", "max_frames", "max_bytes", "arp", "ipv4", "ipv6", "other", "rx_completions"], map(int, r.groups())))
    require(totals["aggregates"] == totals["rx_completions"] == init["service"]["rx_completions"], "aggregates differ from RX completions")
    require(totals["max_frames"] >= 2, "no multi-frame aggregate observed")
    require(totals["max_bytes"] <= 32768, "aggregate beyond the RX transfer")
    t = one(r"NET6 rx test exact=(\d+)/64 burst=(\d+)/4096 digest=([0-9a-f]{16}) end=(\d+) duplicates=0 invalid=0 unexpected=0 fcs=(\d+) no_fcs=(\d+)", "test summary")
    require(int(t[1]) == EXACT and int(t[4]) >= 1 and int(t[4]) >= ends, "exact/END totals")
    bits = 0
    for n in range(8):
        w = one(rf"NET6 rx bitmap line={n} words=([0-9a-f]{{128}})", f"bitmap line {n}")
        for k in range(8):
            word = int(w[1][k * 16:(k + 1) * 16], 16)
            for b in range(64):
                if word >> b & 1:
                    bits |= 1 << (n * 512 + k * 64 + b)
    received = [EXACT + i for i in range(BURST) if bits >> i & 1]
    require(len(received) == int(t[2]) > 0, "bitmap count differs from the burst total")
    digest = sum(burst_term(q, payload_fnv(q)) for q in received) & 0xFFFFFFFFFFFFFFFF
    require(digest == int(t[3], 16), "burst digest differs: some received burst payload is not the sender's")
    require(int(t[5]) + int(t[6]) == EXACT + len(received) + int(t[4]), "FCS counts differ from test frames")
    require((int(t[5]) > 0) == (True in fcs_flags), "FCS pass-through disagrees with exact frames")
    k = one(rf"NET6 census controller={controller} slot={slot} registered=(\d+) routed=(\d+) consumed=(\d+) retired=(\d+) faults=0 ep0_queued=0 lanes=(\d+)/(\d+) custody=(\d+)/(\d+) rx_pending=1 status_pending=1 PASS", "passing census")
    require(k[5] == k[6] == init["lanes"] and k[7] == k[8] == init["custody"], "census not flat or failed")
    require(manifest["exact_sent"] == EXACT and manifest["burst_sent"] == BURST and manifest["end_sent"] >= 1, "sender manifest incomplete")
    capture = None
    if captured is not None:
        sent = {}
        for frame in captured:
            seq = test_seq(frame)
            if seq is not None and seq != END:
                require(seq not in sent, f"capture repeats seq {seq}")
                sent[seq] = frame
        require(sorted(sent) == list(range(EXACT + BURST)), "capture lacks some sent sequence")
        offload = 0
        for seq, frame in sent.items():
            require(frame[42:] == payload(seq), f"captured payload {seq} is not the schedule")
            if seq < EXACT:
                ours = exact[seq]["wire"][:len(frame)]
                if frame != ours:
                    # A sender-side capture can precede checksum offload: allow only the
                    # IPv4/UDP checksum fields to differ, and only if invalid in the capture.
                    fixed = bytearray(frame)
                    fixed[24:26], fixed[40:42] = ours[24:26], ours[40:42]
                    require(bytes(fixed) == ours and not checksums_ok(frame), f"captured frame {seq} differs from what KUMO received")
                    offload += 1
        capture = dict(frames=len(sent), exact_matched=EXACT, checksum_offload_artifacts=offload)
    lost = BURST - len(received)
    return dict(passed=True, controller=controller, slot=int(slot), init=init, peer_mac=macs.pop().hex(":"),
                fcs_passed_through=fcs_flags.pop(), exact=EXACT, burst_received=len(received), burst_lost=lost,
                burst_loss_percent=round(100 * lost / BURST, 2), end_frames=int(t[4]), totals=totals,
                exact_aggregates=len(aggregates), multi_frame_exact_aggregates=sum(1 for a in aggregates if a[2] > 1),
                manifest={k: manifest[k] for k in ["source_ip", "source_port", "broadcast", "burst_seconds", "enobufs_retries"]},
                capture=capture)


# ---------------------------------------------------------------- self-test

def fixture(fcs=True, lost=(70, 71, 4000)):
    src_mac, ip_src, ip_dst, sport = bytes.fromhex("02aabbccdd06"), "192.168.0.106", "192.168.0.255", 61000
    manifest = dict(interface="en0", source_ip=ip_src, source_port=sport, broadcast=ip_dst, port=PORT,
                    exact_sent=EXACT, burst_sent=BURST, end_sent=5, burst_seconds=0.05, enobufs_retries=0)

    def frame(seq, ident):
        data = payload(seq)
        ip = bytearray(struct.pack(">BBHHHBBH4s4s", 0x45, 0, 28 + len(data), ident, 0, 64, 17, 0,
                                   socket.inet_aton(ip_src), socket.inet_aton(ip_dst)))
        ip[10:12] = struct.pack(">H", ~ones(bytes(ip)) & 0xFFFF)
        udp = bytearray(struct.pack(">HHHH", sport, PORT, 8 + len(data), 0)) + data
        c = ~ones(bytes(udp), ones(bytes(ip[12:20]), 17 + len(udp))) & 0xFFFF
        udp[6:8] = struct.pack(">H", c or 0xFFFF)
        return b"\xff" * 6 + src_mac + b"\x08\x00" + bytes(ip) + bytes(udp)

    wire = {seq: frame(seq, seq & 0xFFFF) for seq in list(range(EXACT + BURST)) + [END]}
    lines = ["NET0 ax88 controller=0xfc400000 root=1 route=0x3 slot=3 speed=3 vid=0b95 pid=1790 exact=true"]
    for name, data in [("DEVICE", link.DEVICE), ("CONFIG", link.CONFIG)]:
        lines += [f"NET0 ax88 ctrl=0xfc400000 slot=3 {name} offset={at} {data[at * 2:(at + 16) * 2]}" for at in range(0, len(data) // 2, 16)]
    w = lambda step, reg, value, back: f"NET5b write step={step} reg=0x{reg} value={value} readback={back} us=500 PASS"
    lines += ["drv-xhci: nic irq-driven; rx+status lanes resident",
              "NET5b resident controller=0xfc400000 slot=3 tick_ms=100 hid_idle_cc=Some(1) lanes=9 custody=8",
              "NET5b start policy=sw-checksum,unicast+broadcast,receive-on phy_writes=0 goal=receive window_s=600"]
    lines += [f"NET5b baseline reg=0x{reg} bytes={'00' * width}" for reg, width in link.BASELINE]
    lines += [w("phy-reset", "26", "0000", "0000"), w("phy-release", "26", "2000", "2000"), "NET5b settled step=phy ms=540",
              w("clock", "33", "03", "03"), "NET5b settled step=clock ms=230",
              "NET5b phy id=001cc915 page=0 bmcr=0x1140 bmsr=0x7949 anar=0x05e1 gbcr=0x0200 attempts=1 ready_ms=830"]
    lines += [w(s, r, v, v) for s, r, v, _ in link.POLICY] + ["NET5b init PASS reads=22 writes=7 ms=850",
              "NET5b link up n=1 ms=3900 since_ms=2990 physr=0xad02",
              "NET5b negotiated speed=1000 full=1 hcd=1000/full bmsr=0x796d anar=0x05e1 anlpar=0xc1e1 gbcr=0x0200 gbsr=0x3800 usb=0x42 agrees=1",
              w("rx-reset", "0b", "0000", "0000"), w("rx-control", "0b", "8802", "8802"), w("queue", "2e", "07200316ff", "07200316ff"),
              w("medium-receive", "22", "0b01", "0b01"), "NET5b status agrees link=1 lag_ms=0 reports=4"]
    n = 0
    received = []
    for seq in list(range(EXACT)) + [s for s in range(EXACT, EXACT + BURST) if s not in lost] + [END]:
        f = wire[seq] + (struct.pack("<I", crc32(wire[seq])) if fcs else b"")
        if seq < EXACT or seq == END:
            n += 1
            lines.append(f"NET6 frame seq={seq} len={len(f)} fnv={fnv64(f):016x} trailing={4 if fcs else 0} fcs={int(fcs)} verdict=Valid hdr={f[:42].hex()}")
            lines.append(f"NET6 aggregate n={n} bytes={len(f) + 40} frames=1 dropped=0 dummies=1")
        else:
            received.append(seq)
    lines += ["NET5b monitor complete goal=receive concluded=1 ups=1 downs=0 agreed=1 missed=0 disagreed=0 ms=40000",
              w("park-medium", "22", "0000", "0000")]
    lines += [w(s, r, v, b) for (s, r, v, _), b in zip(link.PARK, ["0000", "00", "0000"])]
    lines += ["NET5b parked power=0000 clock=00 rx_control=0000 phy=Unavailable",
              "NET5b result PASS goal=receive reads=600 writes=15 ups=1 downs=0 agreed=1 missed=0 disagreed=0 max_transfer_us=900 init=1 receive=1 concluded=1 parked=1",
              "NET5b service controller=0xfc400000 ticks=400 ms=40000 hid_reports=380 hid_max_gap_ms=150 busy_max_us=9000 tick_late_max_ms=60 status_reports=300 rx_completions=120"]
    digest = sum(burst_term(q, payload_fnv(q)) for q in received) & 0xFFFFFFFFFFFFFFFF
    total = EXACT + len(received) + 1
    lines += ["NET6 rx aggregates=120 parse_errors=0 frames=4200 dropped=0 dummies=4200 max_frames=37 max_bytes=24500 arp=12 ipv4=3 ipv6=0 other=1 rx_completions=120",
              f"NET6 rx test exact=64/64 burst={len(received)}/4096 digest={digest:016x} end=1 duplicates=0 invalid=0 unexpected=0 fcs={total if fcs else 0} no_fcs={0 if fcs else total}"]
    bits = sum(1 << (q - EXACT) for q in received)
    for line in range(8):
        words = "".join(f"{(bits >> ((line * 8 + k) * 64)) & (2**64 - 1):016x}" for k in range(8))
        lines.append(f"NET6 rx bitmap line={line} words={words}")
    lines += ["NET6 census controller=0xfc400000 slot=3 registered=900 routed=1300 consumed=1300 retired=900 faults=0 ep0_queued=0 lanes=9/9 custody=8/8 rx_pending=1 status_pending=1 PASS",
              "INDRA login:"]
    return "\n".join(lines) + "\n", manifest, [wire[s] for s in range(EXACT + BURST)]


class Tests(unittest.TestCase):
    def test_golden_values_match_the_rust_probe(self):
        for seq, n, pfnv, term in [(0, 18, 0xf99558d90f5b6892, 0x0b60e5b260f48482),
                                   (63, 1472, 0xbcfa71f7edd4756f, 0x93454c1909f025e7),
                                   (64, 179, 0x9f9da6a69572256f, 0x8288d5196dae5c88),
                                   (EXACT + BURST - 1, 1364, 0xba30f9e8f25f68b7, 0x09c0c5eccfd373a5),
                                   (END, 18, 0x96b8b8e38f9bf3c6, 0x9be64cc035a0d789)]:
            self.assertEqual((payload_len(seq), fnv64(payload(seq)), burst_term(seq, fnv64(payload(seq)))), (n, pfnv, term))
        total = sum(burst_term(q, payload_fnv(q)) for q in range(EXACT, EXACT + BURST)) & 0xFFFFFFFFFFFFFFFF
        self.assertEqual(total, 0x1be1d7bbe7105c2b)
        self.assertEqual(crc32(b"123456789"), 0xCBF43926)

    def test_fixture_passes_with_and_without_fcs_and_capture(self):
        for fcs in (True, False):
            text, manifest, wire = fixture(fcs)
            text = text.replace("NET5b settled step=clock", "INDRA login: NET5b settled step=clock")
            result = check(text, manifest, wire)
            self.assertEqual((result["burst_lost"], result["fcs_passed_through"]), (3, fcs))
            self.assertEqual(result["capture"]["exact_matched"], 64)
            self.assertIsNone(check(text, manifest)["capture"])

    def test_a_sender_side_checksum_offload_artifact_is_the_only_tolerated_difference(self):
        text, manifest, wire = fixture()
        offloaded = [bytes(f[:40]) + b"\x12\x34" + bytes(f[42:]) if i == 5 else f for i, f in enumerate(wire)]
        self.assertEqual(check(text, manifest, offloaded)["capture"]["checksum_offload_artifacts"], 1)
        changed = [bytes(f[:20]) + b"\x99" + bytes(f[21:]) if i == 5 else f for i, f in enumerate(wire)]
        with self.assertRaises(ValueError):
            check(text, manifest, changed)
        with self.assertRaises(ValueError):
            check(text, manifest, wire[:-1])

    def test_every_dropped_or_duplicated_receive_record_fails(self):
        text, manifest, _ = fixture()
        lines = text.splitlines(keepends=True)
        for i, line in enumerate(lines):
            if line.startswith("NET6 aggregate") or line.startswith("NET6 frame seq=4294967295"):
                continue
            with self.subTest(drop=i), self.assertRaises(ValueError):
                check("".join(lines[:i] + lines[i + 1:]), manifest)
            if "INDRA login:" not in line and not line.startswith(("NET0 ax88 ctrl", "NET6 aggregate")):
                with self.subTest(duplicate=i), self.assertRaises(ValueError):
                    check("".join(lines[:i] + [line] + lines[i:]), manifest)

    def test_mutations_never_pass(self):
        text, manifest, _ = fixture()
        for old, new in [("value=0b01 readback=0b01", "value=0b00 readback=0b00"), ("fcs=1 verdict=Valid", "fcs=0 verdict=Valid"),
                         ("max_frames=37", "max_frames=1"), ("burst=4093/4096", "burst=4094/4096"), ("faults=0", "faults=1"),
                         ("exact=64/64", "exact=63/64"), ("custody=8/8", "custody=8/9"), ("rx_completions=120\n", "rx_completions=121\n"),
                         ("concluded=1 ups=1", "concluded=0 ups=1"), ("hid_reports=380", "hid_reports=0"), ("max_bytes=24500", "max_bytes=40000"),
                         ("value=0000 readback=0000 us=500 PASS\nNET5b write step=rx-control", "value=0001 readback=0001 us=500 PASS\nNET5b write step=rx-control")]:
            with self.subTest(old=old), self.assertRaises(ValueError):
                check(text.replace(old, new, 1), manifest)
        first = re.search(r"NET6 frame seq=10 len=\d+ fnv=([0-9a-f]{16})", text)
        with self.assertRaises(ValueError):
            check(text.replace(first[1], f"{int(first[1], 16) ^ 1:016x}"), manifest)
        digest = re.search(r"digest=([0-9a-f]{16})", text)[1]
        with self.assertRaises(ValueError):
            check(text.replace(digest, f"{int(digest, 16) ^ 1:016x}"), manifest)
        with self.assertRaises(ValueError):
            check(text, dict(manifest, source_port=1))
        for extra in ["NET6 frame seq=70 len=60 fnv=0 trailing=0 fcs=0 verdict=Payload hdr=00", "TOWER", "NET5b cable unplug", "NET6 mystery"]:
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                check(text + extra + "\n", manifest)

    def test_pcap_and_pcapng_round_trip(self):
        import tempfile
        frames = [payload(0), b"\x01" * 61]
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "a.pcap"
            p.write_bytes(struct.pack("<IHHiIII", 0xa1b2c3d4, 2, 4, 0, 0, 65535, 1) +
                          b"".join(struct.pack("<IIII", 0, 0, len(f), len(f)) + f for f in frames))
            self.assertEqual(pcap_frames(p), frames)
            pad = lambda f: f + b"\0" * (-len(f) % 4)
            blocks = struct.pack("<IIIHHqI", 0x0a0d0d0a, 28, 0x1a2b3c4d, 1, 0, -1, 28)
            blocks += struct.pack("<IIHHII", 1, 20, 1, 0, 65535, 20)
            for f in frames:
                body = struct.pack("<IIIII", 0, 0, 0, len(f), len(f)) + pad(f)
                blocks += struct.pack("<II", 6, 12 + len(body)) + body + struct.pack("<I", 12 + len(body))
            q = Path(d) / "a.pcapng"
            q.write_bytes(blocks)
            self.assertEqual(pcap_frames(q), frames)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true")
    sub = parser.add_subparsers(dest="command")
    s = sub.add_parser("send", help="broadcast the test datagrams")
    s.add_argument("--interface", default="en0")
    s.add_argument("--source", required=True, help="this host's address on that interface")
    s.add_argument("--broadcast", required=True, help="directed broadcast address of that subnet")
    s.add_argument("--manifest", type=Path, required=True)
    s.add_argument("--idle", type=float, default=10.0, help="seconds of idle cable before sending")
    s.add_argument("--gap", type=float, default=0.03, help="seconds between exact-phase frames")
    s.add_argument("--ends", type=int, default=5)
    c = sub.add_parser("check", help="verify a KUMO serial capture")
    c.add_argument("capture", type=Path)
    c.add_argument("--manifest", type=Path, required=True)
    c.add_argument("--pcap", type=Path, help="the peer's own packet capture of these datagrams")
    args = parser.parse_args()
    if args.self_test:
        return 0 if unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful() else 1
    if args.command == "send":
        return send(args)
    if args.command != "check":
        parser.error("send, check or --self-test")
    try:
        result = check(args.capture.read_text(errors="replace"), json.loads(args.manifest.read_text()),
                       pcap_frames(args.pcap) if args.pcap else None)
    except ValueError as error:
        print(f"NET6 passive RX proof FAIL: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2))
    print(f"NET6 passive RX proof green: 64/64 exact frames match{' the peer capture' if result['capture'] else ''}, "
          f"{result['burst_received']}/4096 burst frames verified ({result['burst_lost']} lost), census flat")
    return 0


if __name__ == "__main__":
    sys.exit(main())
