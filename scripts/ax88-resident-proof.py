#!/usr/bin/env python3
#j831
"""Validate NET4c-3 resident ax88 lane records from one completed boot.

The adapter's custodied 64 KiB backing must hold a 32 KiB RX window that crosses no
64 KiB boundary plus disjoint rings. During 30 idle ticks both lanes stay pending
while every exact EP0 read succeeds and the keyboard keeps reporting. Then 20 cycles
each stop both lanes through the verified stop, deconfigure, reclaim and reconfigure,
then 30 settled idle ticks repeat the first window. The final census must be exact
and flat. Each window's own-busy, tick-lateness and HID-gap maxima are reported, not
gated: they are measurements. — TIMBERDOODLE
"""
import argparse
import json
from pathlib import Path
import re
import sys
import unittest

IDLE_TICKS, CYCLES, SETTLED_TICKS, TICK_MS = 30, 20, 30, 100
RX_LEN, BACKING, BOUNDARY, RINGS_LEN = 0x8000, 0x10000, 0x10000, 0x300
H = r"(0x[0-9a-f]+)"
READY = re.compile(
    rf"NET4c3 nic ready controller={H} slot=(\d+) config=(\d+) rx=dci(\d+)@{H}/(\d+) "
    rf"status=dci(\d+)@{H} rings={H} backing={H} custody=(\d+)$")
START = re.compile(
    rf"NET4c3 resident start controller={H} slot=(\d+) idle_ticks=(\d+) cycles=(\d+) "
    r"settled_ticks=(\d+) tick_ms=(\d+) hid_idle_cc=(Some\(\d+\)|None) lanes=(\d+) "
    r"custody=(\d+)$")
WINDOW = re.compile(
    rf"NET4c3 window=(idle|cycles|settled) controller={H} ms=(\d+) control=(\d+)/(\d+) "
    r"control_max_us=(\d+) hid_reports=(\d+) hid_max_gap_ms=(\d+) busy_max_us=(\d+) "
    r"tick_late_max_ms=(\d+) "
    r"status_reports=(\d+) rx_completions=(\d+) rx_pending=([01]) status_pending=([01])$")
SHAPE = r"(Stopped\(2[67]\)|Raced)"
CYCLE = re.compile(
    rf"NET4c3 cycle=(\d+) controller={H} slot=(\d+) rx={SHAPE} status={SHAPE} "
    r"off=Addressed/Disabled/Disabled on=Configured/Running/Running clear_halt=2 "
    r"queued=1/1 commands=0 us=(\d+) stop_us=(\d+) off_us=(\d+) on_us=(\d+) "
    r"clear_us=(\d+) PASS$")
CENSUS = re.compile(
    rf"NET4c3 census controller={H} configures=(\d+) arms=(\d+) completions=(\d+) "
    r"cancels=(\d+) stops=(\d+) disables=(\d+) reclaims=(\d+) router_stopped=(\d+) "
    r"router_cancelled=(\d+) router_faults=(\d+) lanes=(\d+)/(\d+) custody=(\d+)/(\d+)$")
FINAL = re.compile(
    rf"NET4c3 resident PASS controller={H} slot=(\d+) cycles=(\d+) control=(\d+)/(\d+) "
    r"hid_reports=(\d+) status_reports=(\d+) rx_completions=(\d+) rx_frames=(\d+) "
    r"link=(Some\((?:true|false)\)|None) max_cycle_us=(\d+) rx_pending=1 status_pending=1$")
FORBIDDEN = ["NET4c3 nic FAIL", "NET4c3 resident FAIL", "NET4b completion fault",
             "quiesce failed", "NET4c2 quiesce FAIL", "TOWER", "ABEND", "PANIC"]
REPLAYED = re.compile(r"^\[\s*\d+\.\d+\] ")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def one(lines, pattern, what):
    found = [m for m in (pattern.search(l) for l in lines) if m]
    require(len(found) == 1, f"need exactly one {what} record, found {len(found)}")
    return found[0]


def num(value):
    return int(value, 0)


def crosses(start, length):
    return start // BOUNDARY != (start + length - 1) // BOUNDARY


def parse(text):
    text = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "", text).replace("\r", "")
    for word in FORBIDDEN:
        require(word not in text, f"forbidden record: {word}")
    require("INDRA login:" in text, "no login")
    lines = [l.strip() for l in text.splitlines() if not REPLAYED.match(l)]
    for line in lines:
        if "NET4c3 " in line:
            require(any(p.search(line) for p in (READY, START, WINDOW, CYCLE, CENSUS, FINAL))
                    or "NET4c3 nic status link=" in line or "NET4c3 nic rx bytes=" in line,
                    f"malformed NET4c3 record: {line[:120]}")

    ready = one(lines, READY, "nic ready")
    controller, slot, _config, rx_dci, rx, rx_len, st_dci, status, rings, backing, custody = (
        num(v) for v in ready.groups())
    require(rx_len == RX_LEN and backing % 0x1000 == 0, "rx window size or backing alignment")
    require(backing <= rx and rx + RX_LEN <= backing + BACKING, "rx window outside backing")
    require(not crosses(rx, RX_LEN), "rx window crosses a 64 KiB boundary")
    require(backing <= rings and rings + RINGS_LEN <= backing + BACKING, "rings outside backing")
    require(rings + RINGS_LEN <= rx or rings >= rx + RX_LEN, "rings overlap the rx window")
    require(status == rings + 0x200, "status buffer not in the rings page")
    require((rx_dci, st_dci) == (5, 3), "unexpected endpoint DCIs")
    ack = f"XHCI DMA ACK controller={controller:#x} seq={custody - 1} bytes={BACKING} phys={backing:#x}"
    require(any(l.endswith(ack) for l in lines), "NIC backing lacks its parent custody ACK")

    start = one(lines, START, "resident start")
    (s_controller, s_slot, idle_ticks, cycles, settled_ticks, tick_ms, hid_cc, lanes,
     s_custody) = start.groups()
    require((num(s_controller), num(s_slot)) == (controller, slot), "start names another adapter")
    require((num(idle_ticks), num(cycles), num(settled_ticks), num(tick_ms)) ==
            (IDLE_TICKS, CYCLES, SETTLED_TICKS, TICK_MS), "proof timeline")
    require(num(s_custody) == custody, "an allocation happened between ready and start")
    lanes, keyboard = num(lanes), hid_cc == "Some(1)"

    order = [l for l in lines if WINDOW.search(l) or CYCLE.search(l)]
    names = ["window:" + WINDOW.search(l).group(1) if WINDOW.search(l) else "cycle" for l in order]
    require(names == ["window:idle"] + ["cycle"] * CYCLES + ["window:cycles", "window:settled"],
            "windows and cycles out of order, missing or duplicated")
    windows = {}
    for line in order:
        m = WINDOW.search(line)
        if not m:
            continue
        (name, w_controller, ms, ok, total, control_max, hid, gap, busy, late, w_status, w_rx,
         rx_p, st_p) = m.groups()
        require(num(w_controller) == controller, f"{name} window names another controller")
        expect = 0 if name == "cycles" else (IDLE_TICKS if name == "idle" else SETTLED_TICKS)
        require(num(ok) == num(total) == expect, f"{name} window EP0 reads")
        require(rx_p == st_p == "1", f"{name} window ended with a NIC lane not pending")
        require(not keyboard or num(hid) > 0, f"{name} window: keyboard set to report but silent")
        windows[name] = {"ms": num(ms), "control_max_us": num(control_max),
                         "hid_reports": num(hid), "hid_max_gap_ms": num(gap),
                         "busy_max_us": num(busy), "tick_late_max_ms": num(late),
                         "status_reports": num(w_status), "rx_completions": num(w_rx)}

    cycle_lines = [m for m in (CYCLE.search(l) for l in lines) if m]
    require([num(m.group(1)) for m in cycle_lines] == list(range(1, CYCLES + 1)),
            "cycles missing, duplicated or reordered")
    shapes = {}
    for m in cycle_lines:
        require((num(m.group(2)), num(m.group(3))) == (controller, slot), "cycle names another adapter")
        for shape in m.group(4, 5):
            shapes[shape] = shapes.get(shape, 0) + 1
    max_us = max(num(m.group(6)) for m in cycle_lines)
    for m in cycle_lines:
        require(sum(num(v) for v in m.group(7, 8, 9, 10)) <= num(m.group(6)) + 4,
                "cycle steps exceed the cycle time")
    steps = {name: max(num(m.group(i)) for m in cycle_lines)
             for name, i in (("stop_us", 7), ("off_us", 8), ("on_us", 9), ("clear_us", 10))}

    census = one(lines, CENSUS, "census")
    (c_controller, configures, arms, completions, cancels, stops, disables, reclaims,
     r_stopped, r_cancelled, r_faults, lanes_a, lanes_b, custody_a, custody_b) = (
        num(v) for v in census.groups())
    require(c_controller == controller, "census names another controller")
    require((configures, cancels, stops, disables, reclaims) ==
            (CYCLES + 1, 2 * CYCLES, CYCLES, CYCLES, CYCLES), "lifecycle counts")
    require(arms == completions + cancels + 2, "an armed transfer is unaccounted for")
    require((r_stopped, r_cancelled, r_faults) == (2 * CYCLES, 2 * CYCLES, 0), "router census")
    require(lanes_a == lanes_b == lanes, "router lane count moved")
    require(custody_a == custody_b == custody, "a DMA allocation happened during the proof")

    final = one(lines, FINAL, "resident PASS")
    (f_controller, f_slot, f_cycles, f_ok, f_total, f_hid, f_status, f_rx, f_frames, link,
     f_max_us) = final.groups()
    require((num(f_controller), num(f_slot), num(f_cycles)) == (controller, slot, CYCLES),
            "final record")
    controls = IDLE_TICKS + SETTLED_TICKS
    require((num(f_ok), num(f_total)) == (controls, controls), "final control count")
    require(num(f_hid) >= sum(w["hid_reports"] for w in windows.values()), "final HID count")
    require(num(f_status) == windows["settled"]["status_reports"]
            and num(f_rx) == windows["settled"]["rx_completions"], "final NIC counters")
    require(num(f_max_us) == max_us, "max cycle time disagrees with the cycle records")
    return {
        "passed": True, "controller": hex(controller), "slot": slot,
        "rx_window": [hex(rx), RX_LEN], "rings": hex(rings), "backing": hex(backing),
        "windows": windows, "cycles": CYCLES, "stop_shapes": shapes, "max_cycle_us": max_us,
        "max_step_us": steps,
        "proof": {"hid_reports": num(f_hid), "status_reports": num(f_status),
                  "rx_completions": num(f_rx), "rx_frames": num(f_frames), "link": link},
        "census": {"arms": arms, "completions": completions, "cancels": cancels,
                   "lanes": lanes, "custody": custody},
    }


def trace(shapes=None, hid=29, completions=3):
    shapes = shapes or {}
    c, s = "controller=0xfc400000", "slot=3"

    def window(name, control, status, gap=104):
        return (f"NET4c3 window={name} {c} ms=3100 control={control} control_max_us=1200 "
                f"hid_reports={hid} "
                f"hid_max_gap_ms={gap} busy_max_us=1800 tick_late_max_ms=4 "
                f"status_reports={status} rx_completions=0 rx_pending=1 status_pending=1")

    out = [
        "XHCI DMA ACK controller=0xfc400000 seq=7 bytes=65536 phys=0x4010000",
        f"NET4c3 nic ready {c} {s} config=1 rx=dci5@0x4010000/32768 status=dci3@0x4018200 "
        "rings=0x4018000 backing=0x4010000 custody=8",
        "drv-xhci: nic irq-driven; rx+status lanes resident",
        f"NET4c3 resident start {c} {s} idle_ticks=30 cycles=20 settled_ticks=30 tick_ms=100 "
        "hid_idle_cc=Some(1) lanes=6 custody=8",
        window("idle", "30/30", 0),
    ]
    for k in range(1, CYCLES + 1):
        rx, st = shapes.get(k, ("Stopped(26)", "Stopped(26)"))
        out.append(f"NET4c3 cycle={k} {c} {s} rx={rx} status={st} "
                   "off=Addressed/Disabled/Disabled on=Configured/Running/Running clear_halt=2 "
                   f"queued=1/1 commands=0 us={900 + k} stop_us=400 off_us=100 on_us=150 "
                   "clear_us=200 PASS")
    out.append(window("cycles", "0/0", 1, gap=160))
    out.append(window("settled", "30/30", completions))
    out.append(f"NET4c3 census {c} configures=21 arms={40 + completions + 2} "
               f"completions={completions} cancels=40 stops=20 disables=20 reclaims=20 "
               "router_stopped=40 router_cancelled=40 router_faults=0 lanes=6/6 custody=8/8")
    out.append(f"NET4c3 resident PASS {c} {s} cycles=20 control=60/60 hid_reports={3 * hid} "
               f"status_reports={completions} rx_completions=0 rx_frames=0 link=None "
               "max_cycle_us=920 rx_pending=1 status_pending=1")
    return "\n".join(out) + "\nINDRA login:"


class Tests(unittest.TestCase):
    def test_green_trace_with_escapes_and_replay(self):
        result = parse("\x1b[0m" + trace({4: ("Stopped(27)", "Raced")}).replace("\n", "\r\n"))
        self.assertEqual(result["stop_shapes"], {"Stopped(26)": 38, "Stopped(27)": 1, "Raced": 1})
        replay = "".join(f"[   91.900406] {l}\n" for l in trace().splitlines() if "NET4c3" in l)
        self.assertTrue(parse(trace() + "\n" + replay)["passed"])

    def test_every_missing_duplicate_or_reordered_record_fails(self):
        lines = trace().splitlines(keepends=True)
        for i, line in enumerate(lines):
            if "NET4c3" not in line and "XHCI DMA ACK" not in line:
                continue
            with self.subTest(drop=i), self.assertRaises(ValueError):
                parse("".join(lines[:i] + lines[i + 1:]))
            if "XHCI DMA ACK" not in line:
                with self.subTest(dup=i), self.assertRaises(ValueError):
                    parse("".join(lines[:i + 1] + [line] + lines[i + 1:]))
        cycles = [i for i, l in enumerate(lines) if "NET4c3 cycle=" in l]
        swapped = lines[:]
        swapped[cycles[3]], swapped[cycles[4]] = swapped[cycles[4]], swapped[cycles[3]]
        with self.assertRaises(ValueError):
            parse("".join(swapped))

    def test_wrong_fields_fail(self):
        good = trace()
        for old, new in [
            ("rx=dci5@0x4010000/32768", "rx=dci5@0x400c000/32768"),
            ("window=settled", "window=idle"), ("control=0/0", "control=1/1"),
            ("settled_ticks=30", "settled_ticks=29"), ("control=60/60", "control=59/60"),
            ("status_reports=3 rx_completions=0 rx_frames", "status_reports=2 rx_completions=0 rx_frames"),
            ("rx=dci5@0x4010000/32768", "rx=dci5@0x4010000/16384"),
            ("rings=0x4018000", "rings=0x4014000"), ("status=dci3@0x4018200", "status=dci3@0x4018300"),
            ("seq=7 bytes=65536", "seq=6 bytes=65536"), ("control=30/30 control_max", "control=29/30 control_max"),
            ("hid_reports=29", "hid_reports=0"), ("rx_pending=1 status_pending=1\n", "rx_pending=0 status_pending=1\n"),
            ("rx=Stopped(26) status=Stopped(26) off", "rx=Idle status=Stopped(26) off"),
            ("stop_us=400", "stop_us=900"),
            ("configures=21", "configures=20"), ("cancels=40", "cancels=39"),
            ("router_faults=0", "router_faults=1"), ("router_stopped=40", "router_stopped=41"),
            ("lanes=6/6", "lanes=6/7"), ("custody=8/8", "custody=8/9"), ("arms=45", "arms=46"),
            ("max_cycle_us=920", "max_cycle_us=921"), ("idle_ticks=30", "idle_ticks=29"),
            ("lanes=6 custody=8", "lanes=6 custody=9"),
        ]:
            with self.subTest(old=old, new=new), self.assertRaises(ValueError):
                parse(good.replace(old, new, 1))
        for extra in ["NET4c3 nic FAIL stage=arm", "NET4c3 resident FAIL cycle=3",
                      "NET4b completion fault slot=3", "TOWER", "NET4c3 junk"]:
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                parse(good + "\n" + extra)
        with self.assertRaises(ValueError):
            parse(good.replace("INDRA login:", ""))

    def test_no_keyboard_means_no_hid_requirement(self):
        quiet = trace(hid=0).replace("hid_idle_cc=Some(1)", "hid_idle_cc=None")
        self.assertEqual(parse(quiet)["proof"]["hid_reports"], 0)
        with self.assertRaises(ValueError):
            parse(trace(hid=0))


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
        print(f"NET4c3 resident proof FAIL: {error}", file=sys.stderr)
        return 1
    print("NET4c3 resident proof green: lanes pending beside live HID/EP0, 20 flat stop/disable/reclaim cycles")
    return 0


if __name__ == "__main__":
    sys.exit(main())
