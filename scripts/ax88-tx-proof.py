#!/usr/bin/env python3
#j835
#j836
#j839
"""NET7a: check a KUMO full-duplex TX proof against the peer's own packet capture.

The RX half is NET6's check (same sender, same manifest). Every frame KUMO handed to
the bus must appear in the capture exactly once and byte-exact to the schedule in
lib/kumo-ax88/src/probe.rs; a USB completion alone is not delivery. Frames shorter
than 60 bytes must arrive padded to 60; the padding bytes are reported.

Capture on the peer with:  tcpdump -i en0 -s 0 -U -w peer.pcap 'udp port 50606 or ether proto 0x88b5'
"""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import struct
import sys
import unittest

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("ax88_rx_proof", HERE / "ax88-rx-proof.py")
rx = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rx)
require, fnv64, pattern = rx.require, rx.fnv64, rx.pattern

ETHERTYPE = 0x88B5
MAGIC = b"KUMOTX07"
EXACT, BURST = 32, 2048
SIZES = [1514, 42, 59, 60, 61, 64, 100, 200, 400, 503, 504, 505, 511, 512, 513, 600, 800, 1000,
         1015, 1016, 1017, 1023, 1024, 1025, 1200, 1400, 1500, 1510, 1511, 1512, 1513, 30]


def frame_len(seq):
    return SIZES[seq] if seq < EXACT else 60 + (seq * 389) % 1455


def frame(seq, node):
    n = frame_len(seq)
    head = b"\xff" * 6 + node + struct.pack(">H", ETHERTYPE) + MAGIC + struct.pack("<IHBB", seq, n, 1, 0)
    return head + bytes(pattern(seq, i) for i in range(n - 30))


def tx_check(text, captured, node_hint=None):
    lines = rx.live_lines(text)
    body = "\n".join(lines)
    for word in ["NET7 tx FAIL"]:
        require(word not in body, f"forbidden record: {word}")
    tx = [l for l in lines if l.startswith("NET7 tx")]
    require(len(tx) == len([l for l in lines if "NET7 tx" in l]), "malformed NET7 record")
    node_line = [re.fullmatch(r"NET7 tx node=([0-9a-f]{12}) exact_in_ms=(\d+) burst_in_ms=(\d+) mps=(\d+)", l) for l in tx]
    node_line = [m for m in node_line if m]
    require(len(node_line) == 1, "need one node record")
    node, mps = bytes.fromhex(node_line[0][1]), int(node_line[0][4])
    require(not node[0] & 1 and node != b"\0" * 6, "node address is not unicast")
    require(node_hint is None or node == node_hint, "node address differs from the expected adapter")
    exact = {}
    last_progress = -1
    for l in tx:
        m = re.fullmatch(r"NET7 tx seq=(\d+) len=(\d+) transfer=(\d+) pad=([01]) us=(\d+)", l)
        if m:
            seq, n, transfer, pad = int(m[1]), int(m[2]), int(m[3]), m[4] == "1"
            require(seq < EXACT and seq not in exact, f"tx seq {seq} out of the exact phase or repeated")
            require(n == frame_len(seq) and transfer == n + 8 and pad == (transfer % mps == 0), f"tx seq {seq} prefix/length")
            exact[seq] = int(m[5])
        else:
            progress = re.fullmatch(r"NET7 tx progress seq=(\d+) completed=(\d+) ms=(\d+)", l)
            slow = re.fullmatch(r"NET7 tx slow seq=(\d+) ms=(\d+)", l)
            fifo = re.fullmatch(r"NET7 tx fifo seq=(\d+) pending_ms=(\d+) value=(0x[0-9a-f]{8})", l)
            rearm = re.fullmatch(r"NET7 tx rearm fifo=(0x[0-9a-f]{8})", l)
            if progress:
                require(int(progress[1]) < EXACT + BURST and int(progress[2]) == int(progress[1]) + 1, f"tx progress {l!r}")
                require(int(progress[1]) > last_progress, f"tx progress out of order {l!r}")
                last_progress = int(progress[1])
            elif slow:
                require(int(slow[1]) < EXACT + BURST and int(slow[2]) >= 1000, f"tx slow {l!r}")
            elif fifo:
                require(int(fifo[1]) < EXACT + BURST and int(fifo[2]) >= 1000, f"tx fifo {l!r}")
            elif rearm:
                pass
            else:
                require(l.startswith(("NET7 tx node=", "NET7 tx summary ")), f"unknown NET7 record {l!r}")
    require(sorted(exact) == list(range(EXACT)), "exact TX phase incomplete")
    total = EXACT + BURST
    pads = sum((frame_len(q) + 8) % mps == 0 for q in range(total))
    s = [re.fullmatch(r"NET7 tx summary node=([0-9a-f]{12}) exact=32/32 burst=2048/2048 submitted=(\d+) completed=(\d+) bytes=(\d+) pad_frames=(\d+) errors=0 max_us=(\d+) tx_pending=0", l) for l in tx]
    s = [m for m in s if m]
    require(len(s) == 1 and bytes.fromhex(s[0][1]) == node, "need one clean TX summary")
    require(int(s[0][2]) == int(s[0][3]) == total, "submitted/completed totals")
    require(int(s[0][4]) == sum(frame_len(q) for q in range(total)), "TX byte total")
    require(int(s[0][5]) == pads >= 2, "max-packet boundary frames")
    require(captured is not None, "NET7a needs the peer capture: a USB completion is not delivery")
    seen, padding = {}, {}
    for f in captured:
        if len(f) < 30 or f[12:14] != struct.pack(">H", ETHERTYPE) or f[14:22] != MAGIC:
            continue
        seq = struct.unpack("<I", f[22:26])[0]
        require(seq < total and seq not in seen, f"capture has unexpected or repeated tx seq {seq}")
        want = frame(seq, node)
        if len(want) < 60:
            require(len(f) == 60 and f[:len(want)] == want, f"short tx seq {seq} not padded to 60 exactly")
            padding[seq] = f[len(want):].hex()
        else:
            require(f == want, f"captured tx seq {seq} differs from the schedule")
        seen[seq] = len(f)
    require(sorted(seen) == list(range(total)), f"peer lacks {total - len(seen)} handed-over frames")
    return dict(node=node.hex(":"), mps=mps, frames=total, delivered=len(seen), boundary_frames=pads,
                short_frame_padding=padding, exact_completion_us=dict(max=max(exact.values()), min=min(exact.values())),
                max_completion_us=int(s[0][6]))


def check(text, manifest, captured):
    duplex = rx.check(text, manifest, captured, census="NET7")
    return dict(tx=tx_check(text, captured), rx=duplex)


def fixture():
    text, manifest, wire = rx.fixture()
    node = bytes.fromhex("1a2b3c4d2904")
    text = text.replace("NET6 census", "NET7 census")
    # The bulk OUT lane occupies one more completion slot for the driver's life.
    text = re.sub(r"(NET7 census .*?)lanes=9/9", r"\1lanes=10/10", text, count=1)
    lines = text.splitlines()
    at = next(i for i, l in enumerate(lines) if l.startswith("NET5b status agrees"))
    tx = ["NET7 tx node=1a2b3c4d2904 exact_in_ms=2000 burst_in_ms=11000 mps=512"]
    tx += [f"NET7 tx seq={q} len={frame_len(q)} transfer={frame_len(q) + 8} pad={int((frame_len(q) + 8) % 512 == 0)} us=300" for q in range(EXACT)]
    lines[at + 1:at + 1] = tx
    total = EXACT + BURST
    pads = sum((frame_len(q) + 8) % 512 == 0 for q in range(total))
    summary = (f"NET7 tx summary node=1a2b3c4d2904 exact=32/32 burst=2048/2048 submitted={total} completed={total} "
               f"bytes={sum(frame_len(q) for q in range(total))} pad_frames={pads} errors=0 max_us=900 tx_pending=0")
    census = next(i for i, l in enumerate(lines) if l.startswith("NET7 census"))
    lines.insert(census, summary)
    sent = [frame(q, node) for q in range(total)]
    sent = [f + b"\0" * (60 - len(f)) if len(f) < 60 else f for f in sent]
    return "\n".join(lines) + "\n", manifest, wire + sent


class Tests(unittest.TestCase):
    def test_golden_frames_match_the_rust_schedule(self):
        node = bytes.fromhex("1a2b3c4d2904")
        self.assertEqual(fnv64(frame(1, node)), 0x94e3b3b642dd8ed6)
        last = frame(EXACT + BURST - 1, node)
        self.assertEqual((len(last), fnv64(last)), (1266, 0xecf36f7b2369b700))
        self.assertEqual([q for q in range(EXACT) if (frame_len(q) + 8) % 512 == 0][:2], [10, 19])

    def test_fixture_passes(self):
        text, manifest, captured = fixture()
        result = check(text, manifest, captured)
        self.assertEqual(result["tx"]["delivered"], 2080)
        self.assertEqual(sorted(result["tx"]["short_frame_padding"]), [1, 2, 31])

    def test_delivery_and_bytes_are_proved_by_the_capture_only(self):
        text, manifest, captured = fixture()
        tx_at = [i for i, f in enumerate(captured) if f[12:14] == b"\x88\xb5"]
        for mutate in [lambda c: c[:tx_at[5]] + c[tx_at[5] + 1:],
                       lambda c: c + [c[tx_at[7]]],
                       lambda c: [f[:40] + bytes([f[40] ^ 1]) + f[41:] if i == tx_at[600] else f for i, f in enumerate(c)],
                       lambda c: [f[:29] + b"\x01" + f[30:] if i == tx_at[0] else f for i, f in enumerate(c)]]:
            with self.assertRaises(ValueError):
                check(text, manifest, mutate(list(captured)))
        with self.assertRaises(ValueError):
            check(text, manifest, None)
        # Ethernet padding content is unspecified: reported, never required.
        padded = [f[:59] + b"\x01" if i == tx_at[31] else f for i, f in enumerate(captured)]
        self.assertEqual(check(text, manifest, padded)["tx"]["short_frame_padding"][31][-2:], "01")

    def test_record_mutations_fail(self):
        text, manifest, captured = fixture()
        for old, new in [("errors=0", "errors=1"), ("tx_pending=0", "tx_pending=1"), ("submitted=2080", "submitted=2079"),
                         ("seq=10 len=504 transfer=512 pad=1", "seq=10 len=504 transfer=512 pad=0"),
                         ("seq=3 len=60 transfer=68", "seq=3 len=60 transfer=69"), ("node=1a2b3c4d2904 exact_in", "node=1b2b3c4d2904 exact_in")]:
            with self.subTest(old=old), self.assertRaises(ValueError):
                check(text.replace(old, new, 1), manifest, captured)
        lines = text.splitlines(keepends=True)
        for i, l in enumerate(lines):
            if l.startswith("NET7 tx"):
                with self.subTest(drop=i), self.assertRaises(ValueError):
                    check("".join(lines[:i] + lines[i + 1:]), manifest, captured)
        with self.assertRaises(ValueError):
            check(text + "NET7 tx FAIL seq=5 stage=complete actual=1 transfer=2\n", manifest, captured)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("capture", type=Path, nargs="?")
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--pcap", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return 0 if unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful() else 1
    if not (args.capture and args.manifest and args.pcap):
        parser.error("capture, --manifest and --pcap are required")
    try:
        result = check(args.capture.read_text(errors="replace"), json.loads(args.manifest.read_text()), rx.pcap_frames(args.pcap))
    except ValueError as error:
        print(f"NET7a TX proof FAIL: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2))
    t = result["tx"]
    print(f"NET7a TX proof green: {t['delivered']}/{t['frames']} handed-over frames captured byte-exact by the peer "
          f"({t['boundary_frames']} on the max-packet boundary), full-duplex RX {result['rx']['burst_received']}/4096 verified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
