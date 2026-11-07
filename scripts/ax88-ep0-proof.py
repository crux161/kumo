#!/usr/bin/env python3
#j827
"""Validate NET4a's Orange Pi EP0 trace; fixtures are not physical evidence."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sys
import unittest

DEVICE_SHA = "8f3a364dd0edc2cfc2e00e0e2a0ee8f2c81d177876c822e1cac280ca2085e99d"
CONFIG_SHA = "32a0f0cb1f97ae5ada11ed06c9064ea159bb75ec57479b6ff461d46f4d6b7de1"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def parse(text):
    text = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "", text).replace("\r", "")
    require(not re.search(r"NET4a EP0 (FAIL|REFUSED)", text), "failed/refused EP0 proof")
    starts = re.findall(r"NET4a EP0 start slot=(\d+) original=(0x[0-9a-f]+) test=(0x[0-9a-f]+) cycles=(\d+)", text)
    require(len(starts) == 1, "need exactly one EP0 start")
    slot, original, changed, limit = [int(v, 0) for v in starts[0]]
    require(slot > 0 and limit == 16 and 0 <= original <= 255 and changed == original ^ 1, "wrong start/mutation")
    headers = re.findall(r"NET0 ax88 controller=(0x[0-9a-f]+) root=(\d+) route=(0x[0-9a-f]+) slot=(\d+) speed=(\d+) vid=0b95 pid=1790 exact=true", text)
    require(len(headers) == 1 and int(headers[0][3]) == slot, "missing/ambiguous exact NET0 inventory")
    hashes = {}
    for name, length, expected in [("DEVICE", 18, DEVICE_SHA), ("CONFIG", 39, CONFIG_SHA)]:
        chunks = re.findall(rf"NET0 ax88 ctrl=0x[0-9a-f]+ slot={slot} {name} offset=(\d+) ([0-9a-f]+)", text)
        raw = bytearray()
        for offset, encoded in chunks:
            require(int(offset) == len(raw), f"{name}: gap/duplicate offset")
            raw.extend(bytes.fromhex(encoded))
        require(len(raw) == length, f"{name}: wrong/truncated descriptor length")
        hashes[name.lower()] = hashlib.sha256(raw).hexdigest()
        require(hashes[name.lower()] == expected, f"{name}: changed NET0 descriptor")
    records = re.findall(r"NET4a EP0 cycle=(\d+) changed=(0x[0-9a-f]+) restored=(0x[0-9a-f]+) queued=(\d+) PASS", text)
    require(len(records) == 16, "need all16 unique cycles")
    for expected, record in enumerate(records, 1):
        cycle, test, restored, queued = [int(v, 0) for v in record]
        require((cycle, test, restored, queued) == (expected, changed, original, 0), "cycle/order/restore/reclaim mismatch")
    summaries = re.findall(r"NET4a EP0 PASS cycles=(\d+) requests=(\d+) in=(\d+) out=(\d+) reclaimed=(\d+) queued=(\d+) restored=(\d+)", text)
    require(len(summaries) == 1, "missing/duplicate summary")
    require(tuple(map(int, summaries[0])) == (16, 65, 33, 32, 195, 0, 1), "wrong completion/reclaim totals")
    positions = [m.start() for m in re.finditer("NET4a EP0 cycle=", text)]
    require(text.index("NET4a EP0 start") < positions[0] <= positions[-1] < text.index("NET4a EP0 PASS"), "wrong proof ordering")
    return {"passed": True, "controller": headers[0][0], "root": int(headers[0][1]),
            "route": headers[0][2], "slot": slot, "speed": int(headers[0][4]),
            "cycles": 16, "requests": 65, "in": 33, "out": 32, "reclaimed": 195,
            "original": original, "restored": True, "descriptor_sha256": hashes}


class Tests(unittest.TestCase):
    def trace(self):
        lines = ["NET0 ax88 controller=0xfc400000 root=1 route=0x3 slot=3 speed=3 vid=0b95 pid=1790 exact=true"]
        for name, raw in [("DEVICE", bytes.fromhex("12011002ffff0040950b9017000101020301")),
                          ("CONFIG", bytes.fromhex("09022700010100e0020904000003ffff00040705810308000b0705820200020007050302000200"))]:
            for at in range(0, len(raw), 16):
                lines.append(f"NET0 ax88 ctrl=0xfc400000 slot=3 {name} offset={at} {raw[at:at+16].hex()}")
        lines.append("NET4a EP0 start slot=3 original=0x00 test=0x01 cycles=16")
        lines += [f"NET4a EP0 cycle={i} changed=0x01 restored=0x00 queued=0 PASS" for i in range(1, 17)]
        lines.append("NET4a EP0 PASS cycles=16 requests=65 in=33 out=32 reclaimed=195 queued=0 restored=1")
        return "\n".join(lines) + "\n"

    def test_literal_trace_and_console_escapes(self):
        text = self.trace()
        self.assertTrue(parse(text)["passed"])
        self.assertTrue(parse("\x1b[0m" + text.replace("\n", "\r\n"))["passed"])

    def test_missing_duplicate_reordered_cycles_and_summary(self):
        lines = self.trace().splitlines(keepends=True)
        for index in range(len(lines)):
            with self.subTest(index=index):
                with self.assertRaises(ValueError): parse("".join(lines[:index] + lines[index+1:]))
                with self.assertRaises(ValueError): parse("".join(lines[:index] + [lines[index]] + lines[index:]))
        a = self.trace().replace("cycle=1 ", "cycle=99 ")
        with self.assertRaises(ValueError): parse(a)
        lines[-1], lines[-2] = lines[-2], lines[-1]
        with self.assertRaises(ValueError): parse("".join(lines))

    def test_wrong_fields_and_failed_restore_never_pass(self):
        for old, new in [("test=0x01", "test=0x02"), ("changed=0x01", "changed=0x00"),
                         ("restored=0x00", "restored=0x01"), ("queued=0", "queued=1"),
                         ("requests=65", "requests=64"), ("in=33", "in=32"),
                         ("out=32", "out=31"), ("reclaimed=195", "reclaimed=192"),
                         ("restored=1\n", "restored=0\n"), ("exact=true", "exact=false"),
                         ("DEVICE offset=16", "DEVICE offset=15"), ("12011002", "12011003")]:
            with self.subTest(field=old):
                with self.assertRaises(ValueError): parse(self.trace().replace(old, new, 1))
        for failure in ["NET4a EP0 FAIL error=Timeout", "NET4a EP0 REFUSED profile=Revision"]:
            with self.assertRaises(ValueError): parse(self.trace() + failure)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path, nargs="?", help="serial capture or board dmesg output")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return 0 if unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful() else 1
    if not args.capture:
        parser.error("capture or --self-test is required")
    try:
        result = parse(args.capture.read_text(errors="replace"))
    except ValueError as error:
        print(f"NET4a EP0 proof FAIL: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2))
    print("NET4a EP0 proof green: 16 reversible cycles, 65 exact requests, ring empty; original register restored")
    return 0


if __name__ == "__main__":
    sys.exit(main())
