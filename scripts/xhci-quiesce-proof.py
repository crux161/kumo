#!/usr/bin/env python3
#j830
"""Validate NET4c-2 endpoint-quiescence records from one completed boot.

Every hub status endpoint must first be cancelled by the ordinary path (stop event,
Stopped context at that TRB, Set TR Dequeue past it, lane retired). The default-off
proof then repeats cancel + idle probe for CYCLES cycles; each cycle's records must
follow the ring exactly across wraps, the probe must be a Force Stopped Event at the
restart, and the final census must be flat. — TIMBERDOODLE
"""
import argparse
import json
from pathlib import Path
import re
import sys
import unittest

CYCLES = 40
RING_TRBS = 16  # drv-xhci INT_RING_TRBS: 15 usable TRBs, then the Link TRB.
TRB = 16
STOPPED, STOPPED_LENGTH_INVALID, STOPPED_SHORT_PACKET = 26, 27, 28
H = r"(0x[0-9a-f]+)"
START = re.compile(
    rf"NET4c2 quiesce start controller={H} slot=(\d+) dci=(\d+) cycles=(\d+) "
    rf"restart={H} dcs=([01]) lanes=(\d+) custody=(\d+)$")
CYCLE = re.compile(
    rf"NET4c2 cycle=(\d+) controller={H} slot=(\d+) dci=(\d+) stop=(\d+)@{H} rem=(\d+) "
    rf"ctx={H} set={H}/([01]) raced=([01]) probe=(\d+)@{H} queued=0 commands=0 PASS$")
FINAL = re.compile(
    rf"NET4c2 quiesce PASS controller={H} slot=(\d+) dci=(\d+) cycles=(\d+) wraps=(\d+) "
    r"interrupted=(\d+) partial=(\d+) unstarted=(\d+) raced=(\d+) probes=(\d+) "
    r"registered=(\d+) stopped=(\d+) "
    r"cancelled=(\d+) retired=(\d+) faults=(\d+) lanes=(\d+)/(\d+) custody=(\d+)/(\d+)$")
IDLE = re.compile(
    rf"drv-xhci: hub status idle; controller={H} slot=(\d+) dci=(\d+) stop cc=(\d+) "
    rf"trb={H} rem=(\d+) ctx=Stopped@{H} set={H}/([01]) retries=(\d+) abandoned=1 "
    r"completed=([01]); lane retired$")
FORBIDDEN = ["NET4c2 quiesce FAIL", "quiesce failed", "NET4b completion fault",
             "hub status idle; lane/DMA retained", "TOWER", "ABEND", "PANIC"]
REPLAYED = re.compile(r"^\[\s*\d+\.\d+\] ")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def ints(match):
    return [int(g, 0) for g in match.groups()]


def check_set(td, set_iova, set_dcs, prev_dcs):
    """Either the TD was not the last usable slot (same DCS, next TRB) or it was and
    the restart wrapped to the base with the cycle toggled."""
    if set_dcs == prev_dcs:
        require(set_iova == td + TRB, f"restart {set_iova:#x} does not follow TD {td:#x}")
        return False
    require(set_iova == td - (RING_TRBS - 2) * TRB, f"wrap from {td:#x} to {set_iova:#x}")
    return True


def check_stop(code, ptr, rem, ctx, raced, td, set_iova, wrapped):
    """Return "raced", "interrupted", "partial" or "unstarted" (xHCI 4.6.9 and table
    4-2). Stopped reports the residue; Stopped - Short Packet reports the EDTLA."""
    require(ctx == ptr, f"stopped context {ctx:#x} differs from event {ptr:#x}")
    if raced:
        link = set_iova + (RING_TRBS - 1) * TRB if wrapped else None
        require(code == STOPPED_LENGTH_INVALID and ptr in (set_iova, link),
                "raced completion needs a Force Stopped Event at the restart")
        return "raced"
    require(ptr == td, "an uncompleted cancel must stop at its own TRB")
    if code == STOPPED and rem == 4:
        return "interrupted"
    if code == STOPPED and rem < 4 or code == STOPPED_SHORT_PACKET and rem <= 4:
        return "partial"
    require(code == STOPPED_LENGTH_INVALID and rem == 0,
            "uncompleted cancel needs a Stopped shape or a Force Stopped Event")
    return "unstarted"


def parse(text, ordinary=False):
    """`ordinary` checks an image built without KUMO_XHCI_QUIESCE_PROOF: every hub
    status lane must be cancelled by the ordinary path and no proof may run."""
    text = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "", text).replace("\r", "")
    for word in FORBIDDEN:
        require(word not in text, f"forbidden record: {word}")
    # The proof runs inside one driver child and may finish after Sora prints the
    # login prompt (on metal a record shared the prompt's line), so it is not cut there.
    require("INDRA login:" in text, "no login")
    lines = [l.strip() for l in text.splitlines() if not REPLAYED.match(l)]
    lanes = {}
    for line in lines:
        if "NET4c2 " not in line and "hub status idle" not in line:
            continue
        for kind, pattern in (("idle", IDLE), ("start", START), ("cycle", CYCLE),
                              ("final", FINAL)):
            match = pattern.search(line)
            if match:
                break
        else:
            raise ValueError(f"malformed quiescence record: {line[:120]}")
        values = ints(match)
        key = tuple(values[1:4]) if kind == "cycle" else tuple(values[0:3])
        lane = lanes.setdefault(key, {"idle": None, "start": None, "cycles": [], "final": None})
        if kind == "idle":
            require(lane["idle"] is None and lane["start"] is None, "idle record out of order")
            lane["idle"] = values
        elif kind == "start":
            require(lane["idle"] is not None and lane["start"] is None, "start without idle cancel")
            lane["start"] = values
        elif kind == "cycle":
            require(lane["start"] is not None and lane["final"] is None, "cycle outside proof")
            lane["cycles"].append(values)
        else:
            require(lane["start"] is not None and lane["final"] is None, "final outside proof")
            lane["final"] = values
    require(lanes, "no quiescence record")
    if ordinary:
        require(not any(l["start"] for l in lanes.values()), "proof ran in an ordinary image")
    else:
        require(any(l["start"] for l in lanes.values()), "no quiescence proof ran")
    summary = {}
    for key, lane in sorted(lanes.items()):
        name = f"{key[0]:#x}/slot{key[1]}/dci{key[2]}"
        require(lane["idle"] is not None, f"{name}: no ordinary cancel")
        _, _, _, cc, trb, rem, ctx, set_iova, set_dcs, _retries, completed = lane["idle"]
        # The ordinary cancel is the first TD on a fresh ring, so it cannot wrap.
        td = set_iova - TRB if completed else trb
        check_stop(cc, trb, rem, ctx, completed, td, set_iova, False)
        require(set_iova == td + TRB, f"{name}: ordinary cancel did not repoint past its TD")
        if lane["start"] is None:
            summary[name] = {"ordinary": True, "proof": False}
            continue
        _, _, _, cycles, restart, dcs, lanes_before, custody = lane["start"]
        require(cycles == CYCLES and (restart, dcs) == (set_iova, set_dcs),
                f"{name}: proof does not start at the ordinary restart")
        require(lane["final"] is not None, f"{name}: no final census")
        require([c[0] for c in lane["cycles"]] == list(range(1, CYCLES + 1)),
                f"{name}: cycles missing, duplicated or reordered")
        shapes = {"interrupted": 0, "partial": 0, "unstarted": 0, "raced": 0}
        wraps = 0
        for c in lane["cycles"]:
            _, _, _, _, code, ptr, rem, ctx, s_iova, s_dcs, was_raced, probe, probe_ptr = c
            td = restart
            wrapped = check_set(td, s_iova, s_dcs, dcs)
            shapes[check_stop(code, ptr, rem, ctx, was_raced, td, s_iova, wrapped)] += 1
            link = s_iova + (RING_TRBS - 1) * TRB if wrapped else None
            require(probe == STOPPED_LENGTH_INVALID and probe_ptr in (s_iova, link),
                    f"{name}: probe found something other than the restart")
            wraps += wrapped
            restart, dcs = s_iova, s_dcs
        f = lane["final"]
        expected = [*key, CYCLES, wraps, shapes["interrupted"], shapes["partial"],
                    shapes["unstarted"], shapes["raced"], CYCLES, CYCLES, 2 * CYCLES,
                    2 * CYCLES, 0, 0, lanes_before, lanes_before, custody, custody]
        require(f == expected, f"{name}: final census {f} != {expected}")
        require(wraps >= 2, f"{name}: proof did not wrap the ring twice")
        summary[name] = {"ordinary": True, "proof": True, "cycles": CYCLES, "wraps": wraps,
                         **shapes}
    return {"passed": True, "lanes": summary}


def trace(controller=0xfc400000, slot=1, dci=3, base=0x3f50e00, raced=(), unstarted=(),
          partial=()):
    """A synthetic capture that follows drv-xhci's records exactly."""
    out = []
    c = f"controller={controller:#x} slot={slot} dci={dci}"
    out.append(f"drv-xhci: hub status idle; {c} stop cc=26 trb={base:#x} rem=4 "
               f"ctx=Stopped@{base:#x} set={base + TRB:#x}/1 retries=0 abandoned=1 "
               f"completed=0; lane retired")
    restart, dcs, index = base + TRB, 1, 1
    out.append(f"NET4c2 quiesce start {c} cycles={CYCLES} restart={restart:#x} dcs=1 "
               f"lanes=4 custody=5")
    wraps = 0
    for cycle in range(1, CYCLES + 1):
        td = restart
        if index == RING_TRBS - 2:
            restart, dcs, index, wraps = base, dcs ^ 1, 0, wraps + 1
        else:
            restart, index = td + TRB, index + 1
        if cycle in raced:
            stop = f"stop=27@{restart:#x} rem=0 ctx={restart:#x}"
        elif cycle in unstarted:
            stop = f"stop=27@{td:#x} rem=0 ctx={td:#x}"
        elif cycle in partial:
            stop = f"stop=28@{td:#x} rem=1 ctx={td:#x}"
        else:
            stop = f"stop=26@{td:#x} rem=4 ctx={td:#x}"
        out.append(f"NET4c2 cycle={cycle} {c} {stop} set={restart:#x}/{dcs} "
                   f"raced={int(cycle in raced)} probe=27@{restart:#x} queued=0 commands=0 PASS")
    n, u, p = len(raced), len(unstarted), len(partial)
    out.append(f"NET4c2 quiesce PASS {c} cycles={CYCLES} wraps={wraps} "
               f"interrupted={CYCLES - n - u - p} partial={p} unstarted={u} raced={n} "
               f"probes={CYCLES} "
               f"registered={CYCLES} "
               f"stopped={2 * CYCLES} cancelled={2 * CYCLES} retired=0 faults=0 "
               f"lanes=4/4 custody=5/5")
    return "\n".join(out) + "\n"


class Tests(unittest.TestCase):
    def test_two_hubs_interleaved_with_races_wraps_and_escapes(self):
        a = trace().splitlines(keepends=True)
        b = trace(slot=2, base=0x3f53e00, raced={1, 2, 17, 40},
                  unstarted={3, 29}, partial={14}).splitlines(keepends=True)
        mixed = [line for pair in zip(a, b) for line in pair]
        mixed[20] = "INDRA login: " + mixed[20]
        mixed.insert(5, "NET4c2-autoexec-DONE\n")
        result = parse("\x1b[0m" + "".join(mixed).replace("\n", "\r\n"))
        lane = result["lanes"]["0xfc400000/slot2/dci3"]
        self.assertEqual((lane["wraps"], lane["raced"], lane["unstarted"], lane["partial"],
                          lane["interrupted"]), (2, 4, 2, 1, 33))
        # Replayed, timestamped duplicates after the live copy are ignored.
        replay = "".join(f"[   91.900406] {line}" for line in a)
        self.assertTrue(parse(trace() + replay + "INDRA login:")["passed"])

    def test_every_missing_duplicate_or_reordered_record_fails(self):
        lines = trace().splitlines(keepends=True)
        for index in range(len(lines)):
            with self.subTest(index=index):
                with self.assertRaises(ValueError):
                    parse("".join(lines[:index] + lines[index + 1:]) + "INDRA login:")
                with self.assertRaises(ValueError):
                    parse("".join(lines[:index + 1] + [lines[index]] + lines[index + 1:])
                          + "INDRA login:")
        for i in range(1, len(lines) - 1):
            swapped = lines[:]
            swapped[i], swapped[i + 1] = swapped[i + 1], swapped[i]
            with self.subTest(swap=i), self.assertRaises(ValueError):
                parse("".join(swapped) + "INDRA login:")

    def test_wrong_fields_fail(self):
        good = trace(raced={5}, unstarted={7}, partial={9})
        cycle1 = next(l for l in good.splitlines() if "cycle=1 " in l)
        cycle5 = next(l for l in good.splitlines() if "cycle=5 " in l)
        cycle7 = next(l for l in good.splitlines() if "cycle=7 " in l)
        cycle9 = next(l for l in good.splitlines() if "cycle=9 " in l)
        wrap = next(l for l in good.splitlines() if "cycle=14 " in l)
        final = good.splitlines()[-1]
        for line, old, new in [
            (cycle1, "stop=26", "stop=27"), (cycle1, "rem=4", "rem=3"),
            (cycle1, "ctx=0x3f50e10", "ctx=0x3f50e20"),
            (cycle1, "set=0x3f50e20/1", "set=0x3f50e30/1"),
            (cycle1, "set=0x3f50e20/1", "set=0x3f50e20/0"),
            (cycle1, "probe=27@0x3f50e20", "probe=26@0x3f50e20"),
            (cycle1, "probe=27@0x3f50e20", "probe=27@0x3f50e10"),
            (cycle1, "raced=0", "raced=1"), (cycle5, "stop=27", "stop=26"),
            (cycle7, "rem=0", "rem=4"), (cycle7, "stop=27", "stop=28"),
            (cycle7, "raced=0", "raced=1"), (cycle9, "rem=1", "rem=5"),
            (cycle9, "stop=28", "stop=27"), (cycle1, "rem=4", "rem=5"),
            (wrap, "set=0x3f50e00/0", "set=0x3f50e00/1"),
            (final, "wraps=2", "wraps=3"), (final, "interrupted=37", "interrupted=38"),
            (final, "partial=1", "partial=0"),
            (final, "unstarted=1", "unstarted=0"),
            (final, "raced=1", "raced=0"), (final, "registered=40", "registered=41"),
            (final, "stopped=80", "stopped=79"), (final, "cancelled=80", "cancelled=81"),
            (final, "retired=0", "retired=1"), (final, "faults=0", "faults=1"),
            (final, "lanes=4/4", "lanes=4/5"), (final, "custody=5/5", "custody=5/6"),
        ]:
            with self.subTest(old=old, new=new), self.assertRaises(ValueError):
                parse(good.replace(line, line.replace(old, new, 1)) + "INDRA login:")
        for extra in ["NET4c2 quiesce FAIL controller=0x1", "NET4b completion fault slot=2",
                      "drv-xhci: hub status idle; lane/DMA retained", "TOWER", "NET4c2 junk"]:
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                parse(good + extra + "\nINDRA login:")
        with self.assertRaises(ValueError):
            parse(good)

    def test_ordinary_cancel_without_proof_is_reported_but_a_proof_is_required(self):
        idle = trace().splitlines(keepends=True)[0]
        with self.assertRaises(ValueError):
            parse(idle + "INDRA login:")
        self.assertEqual(parse(idle + "INDRA login:", ordinary=True)["lanes"],
                         {"0xfc400000/slot1/dci3": {"ordinary": True, "proof": False}})
        for bad in [trace(), "INDRA login:", idle.replace("rem=4", "rem=5")]:
            with self.subTest(bad=bad[:30]), self.assertRaises(ValueError):
                parse(bad + "INDRA login:", ordinary=True)
        both = trace() + trace(slot=2, base=0x3f53e00).splitlines(keepends=True)[0]
        result = parse(both + "INDRA login:")
        self.assertEqual(result["lanes"]["0xfc400000/slot2/dci3"],
                         {"ordinary": True, "proof": False})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path, nargs="?")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--ordinary", action="store_true",
                        help="image built without the proof flag: ordinary cancels only")
    args = parser.parse_args()
    if args.self_test:
        result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests))
        return 0 if result.wasSuccessful() else 1
    if args.capture is None:
        parser.error("capture or --self-test required")
    try:
        print(json.dumps(parse(args.capture.read_text(errors="replace"), args.ordinary),
                         indent=2))
    except ValueError as error:
        print(f"NET4c2 quiescence proof FAIL: {error}", file=sys.stderr)
        return 1
    if args.ordinary:
        print("NET4c2 ordinary cancel green: every idle hub status lane stopped, repointed, retired")
    else:
        print("NET4c2 quiescence proof green: every cancel stopped, drained, repointed and probed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
