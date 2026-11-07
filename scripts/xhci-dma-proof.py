#!/usr/bin/env python3
#j829
"""Validate parent-before-driver xHCI custody records from one completed boot."""
import argparse
import json
from pathlib import Path
import re
import sys
import unittest


def require(condition, message):
    if not condition:
        raise ValueError(message)


def parse(text):
    text = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "", text).replace("\r", "")
    require("custody refused" not in text and "custody ACK failed" not in text,
            "custody refusal/delivery failure")
    records = list(re.finditer(
        r"XHCI DMA (HOLD|ACK) controller=(0x[0-9a-f]+) seq=(\d+) bytes=(\d+) (?:held=(\d+)|phys=(0x[0-9a-f]+))",
        text))
    require(records, "no custody records")
    require(len(records) == len(re.findall(r"XHCI DMA (?:HOLD|ACK)\b", text)),
            "malformed custody record")
    ledgers = {}
    for record in records:
        kind, controller, sequence, size, count, physical = record.groups()
        controller, sequence, size = int(controller, 16), int(sequence), int(size)
        require(controller > 0 and controller % 4096 == 0, "invalid controller")
        require(0 < size <= 4 * 1024 * 1024 and size % 4096 == 0, "invalid byte count")
        ledger = ledgers.setdefault(controller, {"count": 0, "bytes": 0, "pending": None, "spans": []})
        if kind == "HOLD":
            require(count is not None and physical is None, "wrong HOLD shape")
            require(ledger["pending"] is None, "unacknowledged allocation")
            require(sequence == ledger["count"] + 1 == int(count) and sequence <= 32,
                    "sequence/count mismatch")
            ledger["bytes"] += size
            require(ledger["bytes"] <= 16 * 1024 * 1024, "total allocation budget exceeded")
            ledger["count"] = sequence
            ledger["pending"] = (sequence, size)
        else:
            require(physical is not None and count is None, "wrong ACK shape")
            require(ledger["pending"] == (sequence, size), "ACK without matching prior HOLD")
            physical = int(physical, 16)
            end = physical + size
            require(physical > 0 and physical % 4096 == 0 and end <= (1 << 64) - 1,
                    "invalid physical span")
            require(all(end <= lo or physical >= hi for lo, hi in ledger["spans"]),
                    "overlapping retained allocations")
            ledger["spans"].append((physical, end))
            ledger["pending"] = None
    require(all(l["pending"] is None for l in ledgers.values()), "missing final ACK")
    logins = [m.start() for m in re.finditer("INDRA login:", text)]
    require(logins and logins[0] > records[-1].end(), "need login after all allocation ACKs")
    return {"passed": True, "controllers": {
        hex(c): {"allocations": l["count"], "bytes": l["bytes"]}
        for c, l in sorted(ledgers.items())}}


class Tests(unittest.TestCase):
    def trace(self):
        return ("XHCI DMA HOLD controller=0xfc400000 seq=1 bytes=4096 held=1\n"
                "XHCI DMA ACK controller=0xfc400000 seq=1 bytes=4096 phys=0x200000\n"
                "XHCI DMA HOLD controller=0xfc000000 seq=1 bytes=4096 held=1\n"
                "XHCI DMA HOLD controller=0xfc400000 seq=2 bytes=32768 held=2\n"
                "XHCI DMA ACK controller=0xfc400000 seq=2 bytes=32768 phys=0x210000\n"
                "XHCI DMA ACK controller=0xfc000000 seq=1 bytes=4096 phys=0x300000\n"
                "INDRA login:")

    def test_interleaved_controllers_and_escapes(self):
        result = parse("\x1b[0m" + self.trace().replace("\n", "\r\n"))
        self.assertEqual(result["controllers"]["0xfc400000"], {"allocations": 2, "bytes": 36864})

    def test_missing_duplicate_and_reordered_handshakes(self):
        lines = self.trace().splitlines(keepends=True)
        for index in range(len(lines)):
            with self.subTest(index=index):
                with self.assertRaises(ValueError):
                    parse("".join(lines[:index] + lines[index + 1:]))
                if index < len(lines) - 1:
                    with self.assertRaises(ValueError):
                        parse("".join(lines[:index] + [lines[index]] + lines[index:]))
        lines[0], lines[1] = lines[1], lines[0]
        with self.assertRaises(ValueError):
            parse("".join(lines))

    def test_refusals_wrong_fields_overlap_and_early_login(self):
        for old, new in [("held=2", "held=1"), ("seq=2", "seq=3"),
                         ("bytes=32768 phys", "bytes=4096 phys"),
                         ("phys=0x210000", "phys=0x200000"),
                         ("phys=0x210000", "phys=0x210001"),
                         ("phys=0x210000", "phys=0xfffffffffffff000"),
                         ("held=2", "phys=0x210000"),
                         ("seq=1 bytes=4096 held=1", "seq=1 bytes=0 held=1")]:
            with self.subTest(field=old), self.assertRaises(ValueError):
                parse(self.trace().replace(old, new, 1))
        for extra in ["xhci: DMA custody refused", "xhci: custody ACK failed", "XHCI DMA HOLD malformed"]:
            with self.assertRaises(ValueError):
                parse(self.trace() + extra)
        with self.assertRaises(ValueError):
            parse("INDRA login:\n" + self.trace())

    def test_budget_and_capacity(self):
        def trace(count, size):
            return "".join(
                f"XHCI DMA HOLD controller=0xfc400000 seq={i} bytes={size} held={i}\n"
                f"XHCI DMA ACK controller=0xfc400000 seq={i} bytes={size} phys={hex(i * 0x400000)}\n"
                for i in range(1, count + 1)) + "INDRA login:"
        self.assertTrue(parse(trace(32, 4096))["passed"])
        self.assertTrue(parse(trace(4, 0x400000))["passed"])
        for count, size in [(33, 4096), (5, 0x400000), (1, 0x401000)]:
            with self.assertRaises(ValueError):
                parse(trace(count, size))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path, nargs="?")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests))
        return 0 if result.wasSuccessful() else 1
    if args.capture is None:
        parser.error("capture or --self-test required")
    try:
        print(json.dumps(parse(args.capture.read_text(errors="replace")), indent=2))
    except ValueError as error:
        print(f"xHCI custody proof FAIL: {error}", file=sys.stderr)
        return 1
    print("xHCI custody proof green: every allocation acknowledged after parent retention")
    return 0


if __name__ == "__main__":
    sys.exit(main())
