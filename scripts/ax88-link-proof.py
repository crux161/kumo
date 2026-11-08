#!/usr/bin/env python3
#j833
"""Check NET5b's volatile ax88 initialization and measured cable up/down capture.

Every queue/medium value, the autonegotiation resolution and the cycle count are
recomputed here from the logged PHY registers, independently of kumo_ax88::link.
"""
import argparse
import json
from pathlib import Path
import re
import sys
import unittest

DEVICE = "12011002ffff0040950b9017000101020301"
CONFIG = "09022700010100e0020904000003ffff00040705810308000b0705820200020007050302000200"
BASELINE = [("26", 2), ("33", 1), ("22", 2), ("0b", 2), ("34", 1), ("35", 1), ("2e", 5)]
PHYPWR_NAMED = 0x1030
INIT = [("phy-reset", "26", "0000", PHYPWR_NAMED), ("phy-release", "26", "2000", PHYPWR_NAMED),
        ("clock", "33", "03", 0xff)]
POLICY = [("rx-checksum", "34", "00", 0xff), ("tx-checksum", "35", "00", 0xff),
          ("medium", "22", "0000", 0xffff), ("rx-control", "0b", "8802", 0xffff)]
PARK = [("park-rx-control", "0b", "0000", 0xffff), ("park-clock", "33", "00", 0xff),
        ("park-phy", "26", "0000", PHYPWR_NAMED)]
QUEUE = {"ss-giga": "074f0012ff", "hs-giga": "07200316ff", "fast": "07ae0718ff", "slow": "07cc4c1808"}
CYCLES = 2
FORBIDDEN = ["NET5b FAIL", "MISMATCH", "NET5b status missed", "TOWER", "ABEND", "PANIC",
             "NET4b completion fault", "NET4c3 nic FAIL", "NET4c3 resident", "NET4c3 cycle=",
             "NET4c3 window=", "NET4c3 census", "NET4c3 nic rx bytes=", "NET5a KUMO start",
             "NET4a EP0 start"]
H4 = "(0x[0-9a-f]{4})"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def le(hexed):
    return int.from_bytes(bytes.fromhex(hexed), "little")


def masked(value, readback, mask):
    return len(value) == len(readback) and (le(value) ^ le(readback)) & mask == 0


def expected(physr, usb):
    """link_reset()'s table for a reported link, minus receive/pause (KUMO policy)."""
    if not physr & 0x0400:
        return None
    full, speed_bits = bool(physr & 0x2000), physr & 0xc000
    if speed_bits == 0x8000:
        speed, medium = 1000, 0x0009
        queue = QUEUE["ss-giga"] if usb & 4 else QUEUE["hs-giga"] if usb & 2 else QUEUE["slow"]
    elif speed_bits == 0x4000:
        speed, medium, queue = 100, 0x0200, QUEUE["fast"] if usb & 6 else QUEUE["slow"]
    elif speed_bits == 0:
        speed, medium, queue = 10, 0x0000, QUEUE["slow"]
    else:
        return None
    medium |= 0x0002 if full else 0
    return speed, full, queue, medium.to_bytes(2, "little").hex()


def resolve(bmsr, anar, anlpar, gbcr, gbsr):
    """IEEE 802.3 Annex 28B.3 highest common denominator."""
    if not bmsr & 0x0020:
        return None
    giga, common = gbcr & (gbsr >> 2), anar & anlpar
    for bits, bit, result in [(giga, 0x200, (1000, True)), (giga, 0x100, (1000, False)),
                              (common, 0x100, (100, True)), (common, 0x200, (100, False)),
                              (common, 0x080, (100, False)), (common, 0x040, (10, True)),
                              (common, 0x020, (10, False))]:
        if bits & bit:
            return result
    return None


def label(mode):
    return f"{mode[0]}/{'full' if mode[1] else 'half'}"


class Cursor:
    def __init__(self, lines):
        self.lines, self.i = lines, 0

    def peek(self):
        return self.lines[self.i] if self.i < len(self.lines) else ""

    def take(self, pattern, what):
        found = re.fullmatch(pattern, self.peek())
        require(found, f"expected {what}, found {self.peek()!r}")
        self.i += 1
        return found


def live_lines(text):
    """Strip ANSI/CR and an optional tio stamp; drop replayed kernel-log duplicates."""
    text = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "", text).replace("\r", "")
    out = []
    for line in text.splitlines():
        line = re.sub(r"^\[\d\d:\d\d:\d\d\.\d+\] ", "", line)
        if not re.match(r"\[\s*\d+\.\d+\] ", line):
            out.append(line)
    return out


def parse(text, peer=None):
    lines = live_lines(text)
    body = "\n".join(lines)
    for word in FORBIDDEN:
        require(word not in body, f"forbidden record: {word}")
    require("INDRA login:" in body, "no login")
    native = re.findall(r"NET0 ax88 controller=(0x[0-9a-f]+) root=\d+ route=0x[0-9a-f]+ slot=(\d+) speed=3 vid=0b95 pid=1790 exact=true", body)
    require(len(native) == 1, "need one exact native inventory")
    controller, slot = native[0]
    for name, accepted in [("DEVICE", DEVICE), ("CONFIG", CONFIG)]:
        data = bytearray()
        for offset, chunk in re.findall(rf"NET0 ax88 ctrl={controller} slot={slot} {name} offset=(\d+) ([0-9a-f]+)", body):
            require(int(offset) == len(data), f"native {name} gap/duplicate")
            data.extend(bytes.fromhex(chunk))
        require(data.hex() == accepted, f"native {name} differs from NET0")
    ready = [i for i, l in enumerate(lines) if l.startswith(f"NET4c3 nic ready controller={controller} slot={slot} ")]
    lanes_on = [i for i, l in enumerate(lines) if l == "drv-xhci: nic irq-driven; rx+status lanes resident"]
    proof = [(i, l) for i, l in enumerate(lines) if l.startswith("NET5b ")]
    require(len(ready) == 1 and len(lanes_on) == 1 and proof, "no resident lanes or NET5b records")
    require(ready[0] < lanes_on[0] < proof[0][0], "NET5b before its lanes")
    require(all("NET5b " not in l for l in lines if not l.startswith("NET5b ")), "malformed NET5b line")
    c = Cursor([l for _, l in proof])

    def write(step, reg, value, mask):
        found = c.take(rf"NET5b write step={step} reg=0x{reg} value={value} readback=([0-9a-f]{{{len(value)}}}) us=(\d+) PASS", step)
        require(masked(value, found[1], mask), f"{step}: readback {found[1]} misses named bits")
        return found[1]

    res = c.take(rf"NET5b resident controller={controller} slot={slot} tick_ms=100 hid_idle_cc=(Some\(\d+\)|None) lanes=(\d+) custody=(\d+)", "resident start")
    c.take(r"NET5b start policy=sw-checksum,unicast\+broadcast,receive-off phy_writes=0 cycles=2 window_s=600", "start")
    baseline = {reg: c.take(rf"NET5b baseline reg=0x{reg} bytes=([0-9a-f]{{{2 * width}}})", f"baseline {reg}")[1]
                for reg, width in BASELINE}
    readbacks = {step: write(step, reg, value, mask) for step, reg, value, mask in INIT[:2]}
    phy_ms = int(c.take(r"NET5b settled step=phy ms=(\d+)", "PHY settle")[1])
    readbacks["clock"] = write(*INIT[2])
    clock_ms = int(c.take(r"NET5b settled step=clock ms=(\d+)", "clock settle")[1])
    require(phy_ms >= 500 and clock_ms >= 200, "settle shorter than the reference")
    phy = c.take(rf"NET5b phy id=([0-9a-f]{{8}}) page=0 bmcr={H4} bmsr={H4} anar={H4} gbcr={H4} attempts=(\d+) ready_ms=(\d+)", "PHY identity")
    require(phy[1] not in ("00000000", "ffffffff"), "PHY identity unavailable")
    anar, gbcr = int(phy[4], 16), int(phy[5], 16)
    for step, reg, value, mask in POLICY:
        readbacks[step] = write(step, reg, value, mask)
    init_ms = int(c.take(r"NET5b init PASS reads=22 writes=7 ms=(\d+)", "init")[1])

    state, ups, downs, cycles, down_after_up, last_ms = None, 0, 0, 0, False, -1
    transitions = []
    while not c.peek().startswith("NET5b monitor "):
        t = c.take(r"NET5b link (up|down|initial-down) n=(\d+) ms=(\d+) since_ms=(\d+) physr=(0x[0-9a-f]{4})", "link transition")
        kind, n, at, since, physr = t[1], int(t[2]), int(t[3]), int(t[4]), int(t[5], 16)
        up = kind == "up"
        require(at >= last_ms and at >= init_ms, "time went backwards")
        require(bool(physr & 0x0400) == up, "transition disagrees with PHYSR link bit")
        if kind == "initial-down":
            require(state is None and n == 0, "initial-down after the first sample")
        else:
            require(state != up, "repeated link state")
        record = dict(kind=kind, ms=at, since_ms=since, physr=hex(physr))
        if up:
            ups += 1
            if down_after_up:
                cycles, down_after_up = cycles + 1, False
            require(n == ups, "up counter")
            g = c.take(rf"NET5b negotiated speed=(\d+) full=([01]) hcd=(\S+) bmsr={H4} anar={H4} anlpar={H4} gbcr={H4} gbsr={H4} usb=(0x[0-9a-f]{{2}}) agrees=1", "negotiation")
            bmsr, l_anar, anlpar, l_gbcr, gbsr = (int(g[k], 16) for k in range(4, 9))
            require((l_anar, l_gbcr) == (anar, gbcr), "advertisement changed without a PHY write")
            usb = int(g[9], 16)
            require(usb & 6 == 2, "chip USB speed disagrees with observed high speed")
            want = expected(physr, usb)
            require(want and (int(g[1]), g[2] == "1") == want[:2], "negotiated line disagrees with PHYSR")
            hcd = resolve(bmsr, anar, anlpar, gbcr, gbsr)
            require(hcd == want[:2] and g[3] == label(hcd), "autonegotiation resolution disagrees with PHYSR")
            require(peer is None or hcd == peer, f"link {label(hcd)} disagrees with peer {label(peer) if peer else ''}")
            write("queue", "2e", want[2], 0xff_ffff_ffff)
            write("medium-link", "22", want[3], 0xffff)
            record.update(mode=label(hcd), anlpar=hex(anlpar), gbsr=hex(gbsr), queue=want[2], medium=want[3])
        elif kind == "down":
            downs += 1
            down_after_up = ups > 0
            require(n == downs, "down counter")
        state, last_ms = up, at
        agrees, prompts = 0, 0
        while not re.match(r"NET5b (link|monitor) ", c.peek()):
            if c.peek().startswith("NET5b status agrees"):
                a = c.take(r"NET5b status agrees link=([01]) lag_ms=(-?\d+) reports=(\d+)", "status agreement")
                require(int(a[1]) == up, "status endpoint reports the other state")
                record["status_lag_ms"], agrees = int(a[2]), agrees + 1
            else:
                c.take(rf"NET5b cable {'unplug' if up else 'replug'} the ax88 Ethernet cable \(cycle {cycles + 1}/{CYCLES}\)", "cable prompt")
                prompts += 1
        require(agrees == (kind != "initial-down"), "status agreement missing/duplicated")
        require(prompts == (cycles < CYCLES), "cable prompt missing/duplicated")
        transitions.append(record)
    require(state is True and cycles == CYCLES, "cable cycles incomplete")
    m = c.take(r"NET5b monitor complete cycles=2/2 ups=(\d+) downs=(\d+) agreed=(\d+) missed=0 disagreed=0 ms=(\d+)", "monitor end")
    require([int(m[k]) for k in (1, 2, 3)] == [ups, downs, ups + downs], "monitor totals")
    for step, reg, value, mask in PARK:
        readbacks[step] = write(step, reg, value, mask)
    parked = c.take(r"NET5b parked power=([0-9a-f]{4}) clock=00 rx_control=0000 phy=Unavailable", "parked")
    require(le(parked[1]) & PHYPWR_NAMED == 0, "PHY not parked")
    r = c.take(r"NET5b result PASS reads=(\d+) writes=(\d+) ups=(\d+) downs=(\d+) cycles=2/2 agreed=(\d+) missed=0 disagreed=0 max_transfer_us=(\d+) init=1 parked=1", "result")
    reads, writes = int(r[1]), int(r[2])
    require([int(r[k]) for k in (3, 4, 5)] == [ups, downs, ups + downs], "result totals")
    require(writes == len(INIT) + len(POLICY) + 2 * ups + len(PARK), "write count")
    require(reads >= 22 + 8 * ups + len(PARK) + 7 + len(transitions), "read count")
    s = c.take(rf"NET5b service controller={controller} ticks=(\d+) ms=(\d+) hid_reports=(\d+) hid_max_gap_ms=(\d+) busy_max_us=(\d+) tick_late_max_ms=(\d+) status_reports=(\d+) rx_completions=0", "service")
    require(int(s[7]) >= ups + downs, "fewer status reports than agreements")
    require(res[1] != "Some(1)" or int(s[3]) > 0, "no HID report with a 100 ms idle keyboard")
    k = c.take(rf"NET5b census controller={controller} slot={slot} registered=(\d+) routed=(\d+) consumed=(\d+) retired=(\d+) faults=0 ep0_queued=0 lanes=(\d+)/(\d+) custody=(\d+)/(\d+) rx_pending=1 status_pending=1 PASS", "census")
    require(k[5] == k[6] == res[2] and k[7] == k[8] == res[3], "lane/custody census not flat")
    require(c.i == len(c.lines), f"unknown trailing record {c.peek()!r}")
    return dict(passed=True, controller=controller, slot=int(slot), baseline=baseline, readbacks=readbacks,
                phy_id=phy[1], advertised=dict(anar=hex(anar), gbcr=hex(gbcr)),
                settle_ms=dict(phy=phy_ms, clock=clock_ms, phy_ready=int(phy[7])), init_ms=init_ms,
                transitions=transitions, cycles=cycles, peer=label(peer) if peer else None,
                reads=reads, writes=writes, max_transfer_us=int(r[6]),
                service=dict(ticks=int(s[1]), ms=int(s[2]), hid_reports=int(s[3]), hid_max_gap_ms=int(s[4]),
                             busy_max_us=int(s[5]), tick_late_max_ms=int(s[6]), status_reports=int(s[7])),
                router=dict(registered=int(k[1]), routed=int(k[2]), consumed=int(k[3]), retired=int(k[4])))


def fixture():
    lines = ["NET0 ax88 controller=0xfc400000 root=1 route=0x3 slot=3 speed=3 vid=0b95 pid=1790 exact=true"]
    for name, data in [("DEVICE", DEVICE), ("CONFIG", CONFIG)]:
        lines += [f"NET0 ax88 ctrl=0xfc400000 slot=3 {name} offset={at} {data[at * 2:(at + 16) * 2]}" for at in range(0, len(data) // 2, 16)]
    lines += ["NET4c3 nic ready controller=0xfc400000 slot=3 config=1 rx=dci5@0x1000/32768 status=dci3@0x9200 rings=0x9000 backing=0x1000 custody=8",
              "drv-xhci: nic irq-driven; rx+status lanes resident",
              "NET5b resident controller=0xfc400000 slot=3 tick_ms=100 hid_idle_cc=Some(1) lanes=9 custody=8",
              "NET5b start policy=sw-checksum,unicast+broadcast,receive-off phy_writes=0 cycles=2 window_s=600"]
    lines += [f"NET5b baseline reg=0x{reg} bytes={v}" for (reg, _), v in zip(BASELINE, ["0080", "03", "0000", "0000", "00", "00", "0000000000"])]
    w = lambda step, reg, value, back: f"NET5b write step={step} reg=0x{reg} value={value} readback={back} us=812 PASS"
    lines += [w("phy-reset", "26", "0000", "0080"), w("phy-release", "26", "2000", "2080"), "NET5b settled step=phy ms=503",
              w("clock", "33", "03", "03"), "NET5b settled step=clock ms=201",
              "NET5b phy id=003b1881 page=0 bmcr=0x1140 bmsr=0x7949 anar=0x01e1 gbcr=0x0300 attempts=1 ready_ms=712"]
    lines += [w(step, reg, value, value) for step, reg, value, _ in POLICY]
    lines += ["NET5b init PASS reads=22 writes=7 ms=720"]
    up = lambda n, ms, lag: [f"NET5b link up n={n} ms={ms} since_ms=2900 physr=0xbc00",
                             "NET5b negotiated speed=1000 full=1 hcd=1000/full bmsr=0x796d anar=0x01e1 anlpar=0xc5e1 gbcr=0x0300 gbsr=0x3c00 usb=0x02 agrees=1",
                             w("queue", "2e", "07200316ff", "07200316ff"), w("medium-link", "22", "0b00", "0b00"),
                             f"NET5b status agrees link=1 lag_ms={lag} reports={2 * n - 1}"]
    down = lambda n, ms: [f"NET5b link down n={n} ms={ms} since_ms=4000 physr=0x0000"]
    lines += up(1, 3620, -40) + ["NET5b cable unplug the ax88 Ethernet cable (cycle 1/2)"]
    lines += down(1, 7700) + ["NET5b cable replug the ax88 Ethernet cable (cycle 1/2)", "NET5b status agrees link=0 lag_ms=60 reports=2"]
    lines += up(2, 12000, 30) + ["NET5b cable unplug the ax88 Ethernet cable (cycle 2/2)"]
    lines += down(2, 16000) + ["NET5b status agrees link=0 lag_ms=-20 reports=4", "NET5b cable replug the ax88 Ethernet cable (cycle 2/2)"]
    lines += up(3, 21000, 0)
    lines += ["NET5b monitor complete cycles=2/2 ups=3 downs=2 agreed=5 missed=0 disagreed=0 ms=21110"]
    lines += [w(step, reg, value, back) for (step, reg, value, _), back in zip(PARK, ["0000", "00", "0080"])]
    lines += ["NET5b parked power=0080 clock=00 rx_control=0000 phy=Unavailable",
              "NET5b result PASS reads=260 writes=16 ups=3 downs=2 cycles=2/2 agreed=5 missed=0 disagreed=0 max_transfer_us=6100 init=1 parked=1",
              "NET4c3 nic status link=0 words=0x00000000/0x00000000",
              "NET5b service controller=0xfc400000 ticks=215 ms=21400 hid_reports=210 hid_max_gap_ms=140 busy_max_us=9000 tick_late_max_ms=60 status_reports=170 rx_completions=0",
              "NET5b census controller=0xfc400000 slot=3 registered=600 routed=900 consumed=900 retired=600 faults=0 ep0_queued=0 lanes=9/9 custody=8/8 rx_pending=1 status_pending=1 PASS",
              "INDRA login:"]
    return "\n".join(lines) + "\n"


class Tests(unittest.TestCase):
    def test_fixture_passes_with_stamps_ansi_and_replays(self):
        text = "\x1b[0m" + "".join(f"[12:00:{i % 60:02d}.{i:03d}] {l}\r\n" for i, l in enumerate(fixture().splitlines()))
        text += "[12:01:00.000] [   92.096389] NET5b link up n=9 ms=1 since_ms=1 physr=0xbc00\n"
        result = parse(text, (1000, True))
        self.assertEqual(result["cycles"], 2)
        self.assertEqual([t["kind"] for t in result["transitions"]], ["up", "down", "up", "down", "up"])

    def test_every_missing_or_duplicated_record_fails(self):
        lines = fixture().splitlines(keepends=True)
        for i, line in enumerate(lines):
            if line.startswith("NET4c3 nic status"):
                continue
            with self.subTest(drop=i), self.assertRaises(ValueError):
                parse("".join(lines[:i] + lines[i + 1:]))
            if "INDRA login:" not in line:
                with self.subTest(duplicate=i), self.assertRaises(ValueError):
                    parse("".join(lines[:i] + [line] + lines[i:]))

    def test_mutations_never_pass(self):
        for old, new in [
            ("readback=2080", "readback=0080"), ("readback=8802", "readback=8803"), ("step=phy ms=503", "step=phy ms=499"),
            ("step=clock ms=201", "step=clock ms=199"), ("phy id=003b1881", "phy id=00000000"),
            ("value=07200316ff readback=07200316ff", "value=074f0012ff readback=074f0012ff"),
            ("value=0b00 readback=0b00", "value=0b01 readback=0b01"), ("anlpar=0xc5e1 gbcr=0x0300 gbsr=0x3c00", "anlpar=0xc5e1 gbcr=0x0300 gbsr=0x0000"),
            ("usb=0x02", "usb=0x04"), ("reads=22 writes=7", "reads=21 writes=7"), ("writes=16", "writes=17"),
            ("rx_completions=0", "rx_completions=1"), ("faults=0", "faults=1"), ("lanes=9/9", "lanes=9/10"),
            ("custody=8/8", "custody=8/9"), ("phy=Unavailable", "phy=Page0"), ("power=0080", "power=2080"),
            ("status agrees link=1 lag_ms=-40", "status agrees link=0 lag_ms=-40"), ("ups=3 downs=2 agreed=5", "ups=3 downs=2 agreed=4"),
            ("anar=0x01e1 anlpar", "anar=0x0101 anlpar"), ("hcd=1000/full", "hcd=100/full"), ("speed=3 vid=", "speed=4 vid="),
            ("hid_reports=210", "hid_reports=0"), ("status_reports=170", "status_reports=4")]:
            with self.subTest(old=old), self.assertRaises(ValueError):
                parse(fixture().replace(old, new, 1))
        for extra in ["NET5b status missed link=1 waited_ms=2001 reported=0", "TOWER", "NET4c3 cycle=1", "NET4c3 nic rx bytes=64",
                      "NET5b FAIL readback step=medium", "NET5b mystery"]:
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                parse(fixture() + extra + "\n")

    def test_peer_and_independent_tables(self):
        with self.assertRaises(ValueError):
            parse(fixture(), (100, True))
        self.assertEqual(expected(0xa400, 4)[2:], (QUEUE["ss-giga"], "0b00"))
        self.assertEqual(expected(0x6400, 2)[2:], (QUEUE["fast"], "0202"))
        self.assertEqual(expected(0x0400, 2)[2:], (QUEUE["slow"], "0000"))
        self.assertIsNone(expected(0xe400, 2))
        self.assertEqual(resolve(0x20, 0x01e1, 0x45e1, 0x0300, 0), (100, True))
        self.assertIsNone(resolve(0, 0x01e1, 0xc5e1, 0x0300, 0x3c00))


def mode(text):
    found = re.fullmatch(r"(10|100|1000)/(full|half)", text)
    if not found:
        raise argparse.ArgumentTypeError("peer must be SPEED/DUPLEX, e.g. 1000/full")
    return int(found[1]), found[2] == "full"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path, nargs="?")
    parser.add_argument("--peer", type=mode, help="speed/duplex the link partner itself reported")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return 0 if unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful() else 1
    if args.capture is None:
        parser.error("capture or --self-test required")
    try:
        print(json.dumps(parse(args.capture.read_text(errors="replace"), args.peer), indent=2))
    except ValueError as error:
        print(f"NET5b link proof FAIL: {error}", file=sys.stderr)
        return 1
    print("NET5b link proof green: literal volatile init, matched autonegotiation, "
          f"{CYCLES} cable cycles confirmed by the interrupt endpoint, chip parked")
    return 0


if __name__ == "__main__":
    sys.exit(main())
