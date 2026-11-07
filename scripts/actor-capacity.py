#!/usr/bin/env python3
#j825
"""AX4: build a private proof image, run `ax4bench` on two QEMU boots, or parse one metal capture.

`ax4bench` is opt-in like AX0 `actorbench` and AX2 `ax2bench`: login's autoexec appends the
command and the benchmark starts only after the ordinary login services settle. The settle
first requires a quiet syscall counter, then a census-stable quiescence window: the state
counts and live koid list must hold unchanged for 150 s, retried for up to 360 s, so the
default-login census is taken after boot bring-up rather than during it. Metal shows the boot
gpu-client exiting ~120 s after the settle starts, so a shorter window anchored the census on
an actor that left mid-boundary. One benchmark
per boot. Each boot runs five spawn/teardown cycles of 256 heap-free `ax4idle` children, a
scheduler series (16/32/64/128/256 runnables, 32768 decisions each), a first-wake latency
series (single-target then broadcast), and one over-capacity fill/refusal/recovery phase. Every
line is AX4-prefixed; `AXB1`/`AXR1` and the AX0/AX2 commands and parsers are untouched.

The flat-census contract: after each teardown the state counts and the capacity-long process
koid array must equal the pre-spawn census exactly, and `frames_issued` must not move. AX0's
contamination rule is preserved: only cycle 1 may absorb a non-zero frame delta, and the line
then says `contaminated=1` and names the allowance. Kernel-heap drift is gated: each cycle's heap-delta
and the final census must stay within a 64 KiB allowance, because objects that outlive
their actors exhaust the fixed 8 MiB kernel heap. Each clean snapshot is taken only after a
heap-stable clean settle: the resident set must equal the pre-spawn baseline and `heap_used`
must hold within 4 KiB for 500 ms (100 ms samples, 10 s retry). A
`AX4 clean-settle cycle=<n> stable-ms=<n> waited-ms=<n> heap=<n> change=<0|1>` line must
immediately precede each `AX4 cycle=` line, in consecutive per-cycle order; the boundary's
final census uses the same settle and must follow an `AX4 clean-settle final ...` line. The
refusal phase fills the policy capacity, attempts one more spawn, requires the raw
status `BadState` (-11), requires the census to stay unchanged, and then proves a later spawn
succeeds.

`--serial` parses a metal capture and additionally enforces the metal-capacity rule: the
reported policy capacity must fit the single 256 batch, the login census, and two spare slots
(`capacity - live >= 256 + 2`). A boot whose login profile is too large is rejected rather than
reported as a capacity result. The proof image is built with KUMO_CONSOLE_LOGLEVEL=6 and the
default image is rebuilt, checked and hashed on every exit. No NIC, credentials or board
access. Artifacts stay under build/verification/ax4.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import unittest


AUTOEXEC_APPEND = b"ax4bench\n"
AUTOEXEC_LIMIT = 256
BATCH = 256                 # children per cycle (PLAN/021 AX4)
CYCLES = 5                  # spawn/teardown cycles per boot
CAPACITY_MAX = 288          # RESIDENT_MAX: the policy bound AXB2 must report
SPARE = 2                   # free slots the post-AX4 J819 gate requires
HEAP_ALLOWANCE = 64 * 1024  # kernel-heap bytes a cycle or the final census may retain
QUIESCE_STABLE_MS = 150000  # settle: census unchanged for this long before the default census
QUIESCE_MAX_MS = 360000      # settle: retry budget when the census keeps moving
CLEAN_SETTLE_STABLE_MS = 500    # per-cycle clean settle: heap_used unchanged for this long
CLEAN_SETTLE_MAX_MS = 10000     # per-cycle clean settle: retry budget
CLEAN_SETTLE_EPSILON = 4 * 1024  # heap_used noise the clean settle tolerates
SCHED_SIZES = (16, 32, 64, 128, 256)
SCHED_DECISIONS = 32768
WAKE_SIZES = (16, 32, 64, 128, 256)
BEGINS = "AX4 begins: resident actor capacity (PLAN/021 AX4)"
IMAGE_ARGS = ["cargo", "xtask", "image", "--arch", "aarch64", "--hardware", "qemu-virt-aarch64"]
CONTROLS = ("KUMO_CONSOLE_LOGLEVEL", "KUMO_NET_RESTART_PROOF", "KUMO_NVME_ORACLE",
            "KUMO_NVME_PREPARE", "KUMO_NVME_WRITE_PROOF", "KUMO_AX88_ORACLE")

CENSUS_RE = re.compile(
    r"AX4 census (\w[\w-]*) capacity=(\d+) live=(\d+) stored=(\d+) blocked=(\d+) "
    r"ready=(\d+) running=(\d+) free=(\d+) frames=(\d+) heap=(\d+)")
CYCLE_RE = re.compile(
    r"AX4 cycle=(\d+) children=(\d+) peak-live=(\d+) peak-stored=(\d+) clean-live=(\d+) "
    r"clean-stored=(\d+) frames-delta=(-?\d+) heap-delta=(-?\d+) state-equal=([01]) "
    r"koids-equal=([01]) contaminated=([01]) (PASS|FAIL)")
SCHED_RE = re.compile(r"AX4 scheduler runnable=(\d+) decisions=(\d+) total-ns=(\d+)")
WAKE_RE = re.compile(
    r"AX4 wake n=(\d+) mode=(single|broadcast) (?:targets=(\d+) )?samples=(\d+) "
    r"min-ns=(\d+) median-ns=(\d+) max-ns=(\d+) (PASS|FAIL)(?: clean=([01]))?")
FILL_RE = re.compile(
    r"AX4 boundary fill capacity=(\d+) live=(\d+) stored=(\d+) spawns=(\d+) (PASS|FAIL)")
REFUSAL_RE = re.compile(
    r"AX4 refusal status=(-?\d+) capacity=(\d+) live=(\d+) stored=(\d+) state-equal=([01]) "
    r"koids-equal=([01]) (PASS|FAIL)")
RECOVER_RE = re.compile(r"AX4 boundary recover spawn=(\w+) live=(\d+) (PASS|FAIL)")
POLICY_RE = re.compile(r"AX4 policy limit=(\d+) status=(-?\d+) (PASS|FAIL)")
SETTLE_SAMPLE_RE = re.compile(r"AX4 settle sample=(\d+) syscalls=(\d+) delta=(\d+)")
SETTLE_OK_RE = re.compile(r"AX4 settle ok sample=(\d+) delta=(\d+)")
QUIESCE_RE = re.compile(
    r"AX4 quiesce sample=(\d+) stable-ms=(\d+) waited-ms=(\d+) live=(\d+) stored=(\d+) "
    r"blocked=(\d+) ready=(\d+) running=(\d+) change=([01])")
CLEAN_SETTLE_RE = re.compile(
    r"AX4 clean-settle cycle=(\d+) stable-ms=(\d+) waited-ms=(\d+) heap=(\d+) change=([01])")
FINAL_CLEAN_SETTLE_RE = re.compile(
    r"AX4 clean-settle final stable-ms=(\d+) waited-ms=(\d+) heap=(\d+) change=([01])")
CONTAMINATION_RE = re.compile(
    r"AX4 contamination cycle=(\d+) frames-delta=(-?\d+) settle=green source=first-cycle-allowance")

U64_MAX = 2**64 - 1
MAX_LATENCY_NS = 60_000_000_000   # a sentinel (u64::MAX) or a minute-long wake is a failure


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def unpack(data):
    require(data[:8] == b"KUMORD01" and struct.unpack_from("<I", data, 8)[0] == 1, "bad initrd")
    count = struct.unpack_from("<I", data, 12)[0]
    require(0 < count <= 64 and 16 + count * 80 <= len(data), "bad initrd table")
    files = {}
    for i in range(count):
        at = 16 + i * 80
        name = data[at:at + 64].split(b"\0", 1)[0].decode()
        offset, size = struct.unpack_from("<QQ", data, at + 64)
        require(name not in files and 16 + count * 80 <= offset <= offset + size <= len(data),
                "bad initrd entry")
        files[name] = data[offset:offset + size]
    return files


def stage(initrd, append=AUTOEXEC_APPEND):
    """Append to etc/autoexec in place; every other entry must stay byte-identical."""
    original = initrd.read_bytes()
    files = unpack(original)
    require("etc/autoexec" in files, "autoexec missing")
    files["etc/autoexec"] = files["etc/autoexec"] + append
    require(len(files["etc/autoexec"]) <= AUTOEXEC_LIMIT, "autoexec exceeds Sora's 256-byte read")
    out = bytearray(original[:16 + len(files) * 80])
    for i, data in enumerate(files.values()):
        struct.pack_into("<QQ", out, 16 + i * 80 + 64, len(out), len(data))
        out.extend(data)
    require(unpack(bytes(out)) == files, "staging changed executable content")
    initrd.write_bytes(out)
    return sha(out), files["etc/autoexec"]


def one(text, needle, what):
    require(text.count(needle) == 1, f"missing/duplicated {what}")
    return text.index(needle)


TIMESTAMP_RE = re.compile(r"^\[\d{1,2}:\d{2}:\d{2}\.\d{3}\]\s*")


def matches(regex, text, marker):
    rows = []
    for line in text.splitlines():
        if marker in line:
            # Metal captures are timestamped by the console tool; QEMU proofs are not.
            match = regex.fullmatch(TIMESTAMP_RE.sub("", line.strip()))
            require(match is not None, f"malformed {marker.strip()}: {line!r}")
            rows.append(match.groups())
    return rows


def parse_gate(text):
    """Validate the fresh-boot J819 census with a live netstack and hold client."""
    require("TOWER" not in text and "PANIC" not in text and "AX4 gate FAIL" not in text,
            "gate boot reported a fault/failure")
    done = one(text, "autoexec: done", "autoexec completion")
    login = one(text, "INDRA login:", "login prompt")
    ready = one(text, "AX4 boot-services ready", "boot-service readiness")
    begins = one(text, "AX4 gate begins: post-AX4 capacity gate (J819)", "gate begins")
    require(done < login < ready < begins, "gate preceded login/boot-service readiness")
    require(text.count("AX4 gate PASS") == 1, "missing/duplicated gate PASS")
    require(text.count("netprobe: hold PASS") == 1, "hold client did not pass")
    require(len(SETTLE_OK_RE.findall(text)) == 1, "gate did not settle")
    quiet = QUIESCE_RE.findall(text)
    require(quiet and int(quiet[-1][1]) >= QUIESCE_STABLE_MS
            and int(quiet[-1][2]) <= QUIESCE_MAX_MS, "gate quiescence incomplete")
    rows = CENSUS_RE.findall(text)
    require([r[0] for r in rows] == ["default-login", "netstack-client", "gate-clean"],
            "gate census sequence incomplete")
    result = {}
    for label, *values in rows:
        cap, live, stored, blocked, runnable, running, free, frames, heap = map(int, values)
        require(cap == CAPACITY_MAX and live <= stored <= cap and live + free == cap
                and blocked + runnable + running == live and heap > 0 and frames > 0,
                f"invalid gate census {label}")
        result[label] = {"capacity": cap, "live": live, "stored": stored, "free": free}
    base = result["default-login"]["live"]
    peak = result["netstack-client"]
    require(peak["live"] == base + 2 and peak["free"] >= SPARE,
            "netstack/client census lacks two spare slots")
    require(result["gate-clean"]["live"] == base + 1,
            "hold cleanup must retain exactly the supervised netstack")
    marker = f"AX4 capacity-gate free={peak['free']} requires=2 PASS"
    require(text.count(marker) == 1, "capacity gate marker disagrees with census")
    return {"passed": True, "census": result}


def parse(text, metal=False):
    """Check one serial transcript against the AX4 proof contract."""
    require("AX4 FAIL" not in text, "AX4 reported a failure")
    require("TOWER" not in text, "unexpected TOWER")
    require("PANIC" not in text, "kernel panic")
    login = one(text, "INDRA login:", "login prompt")
    done = one(text, "autoexec: done", "autoexec completion")
    begins = one(text, BEGINS, "AX4 begins")
    ready = one(text, "AX4 boot-services ready", "boot-service readiness")
    require(done < login < ready < begins, "measurement preceded login/boot-service readiness")
    require(text.count("AX4 PASS") == 1, "missing/duplicated final AX4 PASS")

    # ---- settle gate ------------------------------------------------------------------
    samples = [tuple(int(v) for v in row) for row in matches(SETTLE_SAMPLE_RE, text, "AX4 settle sample=")]
    oks = [tuple(int(v) for v in row) for row in matches(SETTLE_OK_RE, text, "AX4 settle ok ")]
    require(len(samples) >= 1 and len(oks) == 1, "missing/duplicated settle result")
    require([s[0] for s in samples] == list(range(1, len(samples) + 1)), "settle sample order")
    require(samples[-1][0] == oks[0][0] and samples[-1][2] == oks[0][1],
            "settle ok does not match the last sample")
    require(oks[0][1] <= 100, "settle delta above the 100-syscall gate")
    quiesce = [tuple(int(v) for v in row)
               for row in matches(QUIESCE_RE, text, "AX4 quiesce sample=")]
    require(len(quiesce) >= 1, "missing settle quiescence samples")
    require([q[0] for q in quiesce] == list(range(1, len(quiesce) + 1)),
            "quiesce sample order")
    require(text.index("AX4 settle ok ") > text.rindex("AX4 quiesce sample="),
            "settle result precedes quiescence")
    require(quiesce[-1][1] >= QUIESCE_STABLE_MS and quiesce[-1][8] == 0,
            f"settle did not hold a {QUIESCE_STABLE_MS} ms census-stable window")
    require(quiesce[-1][2] <= QUIESCE_MAX_MS,
            f"settle waited {quiesce[-1][2]} ms, above the {QUIESCE_MAX_MS} ms budget")

    # ---- policy ABI -------------------------------------------------------------------
    policies = [tuple(int(v) for v in row[:2]) + (row[2],)
                for row in matches(POLICY_RE, text, "AX4 policy ")]
    require(policies == [(0, -1, "PASS"), (CAPACITY_MAX + 1, -1, "PASS"),
                         (CAPACITY_MAX, 0, "PASS")],
            "JobSetPolicy status matrix is not (0:-1, 289:-1, 288:0)")

    # ---- censuses ---------------------------------------------------------------------
    censuses = {}
    for row in matches(CENSUS_RE, text, "AX4 census "):
        label = row[0]
        values = [int(v) for v in row[1:]]
        require(label not in censuses, f"duplicated {label} census")
        censuses[label] = dict(zip(
            ("capacity", "live", "stored", "blocked", "ready", "running", "free", "frames", "heap"),
            values))
    require("default-login" in censuses and "final" in censuses,
            "default-login/final census missing")
    default = censuses["default-login"]
    final = censuses["final"]
    require(1 <= default["capacity"] <= CAPACITY_MAX, "reported capacity is outside 1..=288")
    require(default["live"] <= default["stored"] <= default["capacity"], "wrong resident accounting")
    require(default["free"] == default["capacity"] - default["live"], "wrong free-slot accounting")
    require(default["blocked"] + default["ready"] + default["running"] == default["live"],
            "resident states do not partition live")
    require(default["capacity"] >= default["live"] + BATCH,
            "capacity cannot fit the single 256 batch")
    if metal:
        require(default["free"] >= BATCH + SPARE,
                "capacity cannot fit the single 256 batch plus two spare slots "
                f"(free {default['free']} < {BATCH + SPARE})")

    # ---- five flat-census cycles ------------------------------------------------------
    clean_settles = [tuple(int(v) for v in row)
                     for row in matches(CLEAN_SETTLE_RE, text, "AX4 clean-settle cycle=")]
    require(len(clean_settles) == CYCLES, "missing/duplicated clean-settle samples")
    require([row[0] for row in clean_settles] == list(range(1, CYCLES + 1)),
            "clean-settle order")
    for cycle, stable_ms, waited_ms, heap, change in clean_settles:
        require(stable_ms >= CLEAN_SETTLE_STABLE_MS,
                f"cycle {cycle} clean settle held only {stable_ms} ms")
        require(change == 0, f"cycle {cycle} clean settle did not end stable")
        require(waited_ms >= stable_ms and waited_ms <= CLEAN_SETTLE_MAX_MS,
                f"cycle {cycle} clean settle waited {waited_ms} ms")
        require(heap > 0, f"cycle {cycle} clean settle has no heap sample")
    # Each clean-settle line must immediately precede its cycle line, and the pairs must
    # appear in consecutive per-cycle order.
    sequence = []
    for line in text.splitlines():
        plain = TIMESTAMP_RE.sub("", line.strip())
        match = CLEAN_SETTLE_RE.fullmatch(plain)
        if match is not None:
            sequence.append(("settle", int(match.group(1))))
            continue
        match = CYCLE_RE.fullmatch(plain)
        if match is not None:
            sequence.append(("cycle", int(match.group(1))))
    expected_sequence = []
    for cycle in range(1, CYCLES + 1):
        expected_sequence += [("settle", cycle), ("cycle", cycle)]
    require(sequence == expected_sequence,
            "each AX4 clean-settle line must immediately precede its cycle line, in order")
    cycles = [tuple(int(v) if v.lstrip("-").isdigit() else v for v in row)
              for row in matches(CYCLE_RE, text, "AX4 cycle=")]
    require(len(cycles) == CYCLES, "missing/duplicated spawn cycles")
    contaminated = []
    for position, row in enumerate(cycles, start=1):
        (index, children, peak_live, peak_stored, clean_live, clean_stored, frames_delta,
         heap_delta, state_equal, koids_equal, contam, verdict) = row
        require(index == position, "cycle order")
        require(children == BATCH, "cycle did not spawn one 256 batch")
        require(verdict == "PASS", f"cycle {index} failed")
        require(peak_live == default["live"] + BATCH, f"cycle {index} peak census is not +256")
        require(peak_live <= peak_stored <= default["capacity"], f"cycle {index} peak accounting")
        require(clean_live == default["live"] and clean_stored == default["stored"],
                f"cycle {index} clean census differs from the pre-spawn census")
        require(state_equal == 1, f"cycle {index} state counts changed")
        require(koids_equal == 1, f"cycle {index} process koid list changed")
        require(heap_delta is not None, f"cycle {index} did not record heap drift")
        # The kernel heap is a fixed 8 MiB arena; teardown must return the objects a cycle
        # created. Measured: cycle 1 ~33 KiB of one-time Vec capacity, later cycles ~2 KiB;
        # the pre-fix leak was 213-535 KiB every cycle, far above this bound.
        require(heap_delta <= HEAP_ALLOWANCE,
                f"cycle {index} retained {heap_delta} kernel-heap bytes "
                f"(allowance {HEAP_ALLOWANCE})")
        if frames_delta == 0:
            require(contam == 0, f"cycle {index} claims contamination without a frame delta")
        else:
            require(index == 1 and contam == 1 and frames_delta > 0,
                    f"cycle {index} issued frames outside the first-cycle allowance")
            contaminated.append((index, frames_delta))
    require(len(contaminated) <= 1, "more than one contaminated cycle")
    for index, delta in contaminated:
        found = [tuple(int(v) for v in row)
                 for row in matches(CONTAMINATION_RE, text, "AX4 contamination ")]
        require(found == [(index, delta)], "contamination was not logged and identified")

    # ---- scheduler series -------------------------------------------------------------
    scheduler = [tuple(int(v) for v in row) for row in matches(SCHED_RE, text, "AX4 scheduler ")]
    require([row[0] for row in scheduler] == list(SCHED_SIZES), "scheduler runnable order")
    for n, decisions, total_ns in scheduler:
        require(decisions == SCHED_DECISIONS, f"scheduler {n} did not run 32768 decisions")
        require(0 < total_ns < U64_MAX, f"scheduler {n} has no valid total")

    # ---- first-wake series ------------------------------------------------------------
    wakes = [row for row in matches(WAKE_RE, text, "AX4 wake ")]
    expected = [(n, mode) for n in WAKE_SIZES for mode in ("single", "broadcast")]
    require([(int(row[0]), row[1]) for row in wakes] == expected, "wake series order")
    for n, mode, targets, samples, min_ns, median_ns, max_ns, verdict, clean in wakes:
        n, samples, min_ns, median_ns, max_ns = (int(v) for v in (n, samples, min_ns, median_ns, max_ns))
        require(verdict == "PASS", f"wake {n} {mode} failed")
        require(0 < max_ns < MAX_LATENCY_NS
                and min_ns <= median_ns <= max_ns, f"wake {n} {mode} latencies are invalid")
        if mode == "single":
            require(samples == 1 and targets is None and clean is None,
                    f"wake {n} single-target sample shape")
        else:
            require(targets is not None and int(targets) == n - 1 and samples == n - 1,
                    f"wake {n} broadcast did not cover the still-parked children")
            require(clean == "1", f"wake {n} broadcast did not restore the census")

    # ---- boundary fill, refusal, recovery ---------------------------------------------
    fills = matches(FILL_RE, text, "AX4 boundary fill ")
    require(len(fills) == 1, "missing/duplicated boundary fill")
    capacity, live, stored, spawns, verdict = fills[0]
    capacity, live, stored, spawns = (int(v) for v in (capacity, live, stored, spawns))
    require(verdict == "PASS" and capacity == default["capacity"] and live == capacity
            and stored >= live, "boundary fill did not reach the policy capacity")
    require(spawns == capacity - default["live"], "boundary fill spawned the wrong count")

    refusals = matches(REFUSAL_RE, text, "AX4 refusal ")
    require(len(refusals) == 1, "missing/duplicated refusal")
    status, rcap, rlive, rstored, state_equal, koids_equal, verdict = refusals[0]
    require(int(status) == -11, f"over-limit spawn returned status {status}, not BadState (-11)")
    require(int(rcap) == capacity and int(rlive) == live,
            "refused spawn changed the resident census")
    require(state_equal == "1" and koids_equal == "1" and verdict == "PASS",
            "refused spawn was counted as an admitted resident")

    recovers = matches(RECOVER_RE, text, "AX4 boundary recover ")
    require(len(recovers) == 1, "missing/duplicated recovery spawn")
    spawn, rlive, verdict = recovers[0]
    require(spawn == "Ok" and verdict == "PASS", "a later spawn did not succeed")
    require(int(rlive) == default["live"] + 1, "recovery spawn census is not +1")

    # ---- final census -----------------------------------------------------------------
    final_clean = [tuple(int(v) for v in row)
                   for row in matches(FINAL_CLEAN_SETTLE_RE, text, "AX4 clean-settle final ")]
    require(len(final_clean) == 1, "missing/duplicated final clean-settle")
    stable_ms, waited_ms, heap, change = final_clean[0]
    require(stable_ms >= CLEAN_SETTLE_STABLE_MS,
            f"final clean settle held only {stable_ms} ms")
    require(change == 0, "final clean settle did not end stable")
    require(waited_ms >= stable_ms and waited_ms <= CLEAN_SETTLE_MAX_MS,
            f"final clean settle waited {waited_ms} ms")
    require(heap > 0, "final clean settle has no heap sample")
    require(final["capacity"] == default["capacity"], "capacity changed during the run")
    require(final["live"] == default["live"] and final["stored"] == default["stored"],
            "final census differs from the pre-spawn census")
    require(final["heap"] - default["heap"] <= HEAP_ALLOWANCE,
            f"final kernel heap grew by {final['heap'] - default['heap']} bytes "
            f"(allowance {HEAP_ALLOWANCE}); objects outlived their actors")
    require(final["blocked"] == default["blocked"] and final["ready"] == default["ready"]
            and final["running"] == default["running"], "final state counts changed")
    require(final["free"] == final["capacity"] - final["live"], "final free accounting")
    require(final["frames"] >= default["frames"], "frames_issued went backwards")

    # ---- order ------------------------------------------------------------------------
    final_pass = text.index("AX4 PASS")
    order = [one(text, BEGINS, "AX4 begins"),
             one(text, "AX4 settle ok ", "settle result"),
             text.index("AX4 census default-login "),
             one(text, "AX4 cycle=1 ", "cycle 1"),
             one(text, "AX4 cycle=5 ", "cycle 5"),
             one(text, "AX4 scheduler runnable=16 ", "scheduler 16"),
             one(text, "AX4 scheduler runnable=256 ", "scheduler 256"),
             one(text, "AX4 wake n=16 mode=single ", "wake 16 single"),
             one(text, "AX4 wake n=256 mode=broadcast ", "wake 256 broadcast"),
             one(text, "AX4 boundary fill ", "boundary fill"),
             one(text, "AX4 refusal ", "refusal"),
             one(text, "AX4 boundary recover ", "recovery"),
             one(text, "AX4 clean-settle final ", "final clean-settle"),
             text.index("AX4 census final "),
             final_pass]
    require(order == sorted(order), "phase order")

    return {
        "census": default,
        "final_census": final,
        "cycles": [
            {"cycle": row[0], "children": row[1], "peak_live": row[2], "peak_stored": row[3],
             "clean_live": row[4], "clean_stored": row[5], "frames_delta": row[6],
             "heap_delta": row[7], "state_equal": row[8], "koids_equal": row[9],
             "contaminated": row[10], "verdict": row[11]}
            for row in cycles],
        "contaminated_cycles": [{"cycle": i, "frames_delta": d} for i, d in contaminated],
        "scheduler": [{"runnable": n, "decisions": d, "total_ns": t} for n, d, t in scheduler],
        "wakes": [
            {"n": int(row[0]), "mode": row[1],
             "targets": int(row[2]) if row[2] is not None else None,
             "samples": int(row[3]), "min_ns": int(row[4]), "median_ns": int(row[5]),
             "max_ns": int(row[6]), "verdict": row[7],
             "clean": row[8] == "1" if row[8] is not None else None}
            for row in wakes],
        "boundary": {
            "capacity": capacity, "fill_live": live, "fill_stored": stored, "spawns": spawns,
            "refusal_status": int(status), "refusal_state_equal": state_equal == "1",
            "refusal_koids_equal": koids_equal == "1",
            "recovery_live": int(rlive),
        },
        "policy": [{"limit": limit, "status": status, "verdict": verdict}
                   for limit, status, verdict in policies],
        "capacity": {
            "batch": BATCH,
            "spare": SPARE,
            "metal_requires_free": BATCH + SPARE,
            "metal_rule_pass": default["free"] >= BATCH + SPARE,
        },
        "summary": {
            "cycles": len(cycles),
            "children_spawned": sum(row[1] for row in cycles),
            "contaminated_cycles": len(contaminated),
            "scheduler_runs": len(scheduler),
            "wake_pairs": len(wakes) // 2,
            "refusal_status": int(status),
            "max_wake_ns": max(int(row[6]) for row in wakes),
        },
    }


def boot(root, source, work, qemu, firmware, timeout, stall):
    work.mkdir()
    shutil.copytree(source, work / "esp")
    initrd_hash, autoexec = stage(work / "esp/EFI/KUMO/initrd.img")
    (work / "autoexec.txt").write_bytes(autoexec)
    shutil.copyfile(firmware / "edk2-arm-vars.fd", work / "vars.fd")
    serial = work / "serial.log"
    command = [qemu, "-machine", "virt,gic-version=3", "-cpu", "cortex-a72", "-m", "2048",
               "-display", "none", "-monitor", "none", "-serial", f"file:{serial}",
               "-drive", f"if=pflash,format=raw,file={firmware / 'edk2-aarch64-code.fd'},readonly=on",
               "-drive", f"if=pflash,format=raw,file={work / 'vars.fd'}",
               "-drive", f"file=fat:rw:{work / 'esp'},format=raw,if=virtio", "-net", "none"]
    (work / "command.json").write_text(json.dumps(command, indent=2) + "\n")
    started = time.monotonic()
    last_growth = started
    last_len = -1
    with (work / "qemu.log").open("wb") as log:
        process = subprocess.Popen(command, cwd=root, stdout=log, stderr=log)
        try:
            # Every wait is bounded twice: a per-boot deadline and a no-output stall bound.
            # A guest spinning at 99% CPU with a silent serial fails here in `stall`
            # seconds instead of consuming the whole boot budget.
            while True:
                require(process.poll() is None, f"QEMU exited; inspect {work}")
                now = time.monotonic()
                require(now - started < timeout, f"timeout after {timeout:g}s; inspect {serial}")
                text = serial.read_text(errors="replace") if serial.exists() else ""
                if len(text) != last_len:
                    last_len = len(text)
                    last_growth = now
                require(now - last_growth <= stall,
                        f"stalled: no serial output for {stall:g}s ({len(text)} bytes); "
                        f"inspect {serial}")
                if "TOWER" in text:
                    raise RuntimeError(f"unexpected TOWER; inspect {serial}")
                if "PANIC" in text:
                    raise RuntimeError(f"kernel panic; inspect {serial}")
                if "AX4 PASS" in text or "AX4 FAIL" in text:
                    break
                time.sleep(0.1)
            result = parse(text)
            result.update(seconds=round(time.monotonic() - started, 3),
                          initrd_sha256=initrd_hash, autoexec_bytes=len(autoexec))
            (work / "report.json").write_text(json.dumps(result, indent=2) + "\n")
            s = result["summary"]
            print(f"  ax4-bench: PASS ({result['seconds']}s, {s['cycles']} cycles, "
                  f"{s['children_spawned']} children, refusal {s['refusal_status']}, "
                  f"max wake {s['max_wake_ns']} ns)", flush=True)
            return result
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, help="new directory for evidence (default: build/verification/ax4/run-*)")
    parser.add_argument("--firmware", type=Path, help="directory containing QEMU edk2-aarch64-code.fd and edk2-arm-vars.fd")
    parser.add_argument("--timeout", type=float, default=420, help="seconds per QEMU boot")
    parser.add_argument("--stall", type=float, default=90,
                        help="fail a boot after this many seconds with no new serial output")
    parser.add_argument("--serial", type=Path, help="parse one existing metal capture (enforces the metal capacity rule)")
    parser.add_argument("--gate-serial", type=Path, help="validate one metal ax4gate capture")
    parser.add_argument("--self-test", action="store_true", help="test the transcript rejection rules without QEMU")
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=[sys.argv[0]], exit=False).result.wasSuccessful() or sys.exit(1)
        return
    if args.gate_serial:
        print(json.dumps(parse_gate(args.gate_serial.read_text(errors="replace")), indent=2))
        return
    if args.serial:
        print(json.dumps(parse(args.serial.read_text(errors="replace"), metal=True), indent=2))
        return
    require(args.timeout > 0, "timeout must be positive")
    require(args.stall > 0, "stall bound must be positive")
    root = Path(__file__).resolve().parent.parent
    qemu = shutil.which("qemu-system-aarch64")
    require(qemu is not None, "qemu-system-aarch64 missing")
    firmware = (args.firmware or Path(qemu).resolve().parent.parent / "share/qemu").resolve()
    require(all((firmware / p).is_file() for p in ("edk2-aarch64-code.fd", "edk2-arm-vars.fd")),
            "OVMF files missing; use --firmware")
    if args.output:
        work = args.output.resolve()
        work.mkdir(parents=True)
    else:
        parent = root / "build/verification/ax4"
        parent.mkdir(parents=True, exist_ok=True)
        work = Path(tempfile.mkdtemp(prefix="run-", dir=parent))
    print(f"AX4 proof artifacts: {work}", flush=True)
    default_env = {k: v for k, v in os.environ.items() if k not in CONTROLS}
    proof_env = dict(default_env, KUMO_CONSOLE_LOGLEVEL="6")
    source = root / "build/images/qemu-virt-aarch64"
    manifest = {"script_sha256": sha(Path(__file__).read_bytes()),
                "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root).decode().strip(),
                "qemu": subprocess.check_output([qemu, "--version"]).decode().splitlines()[0],
                "firmware": {p: sha((firmware / p).read_bytes()) for p in ("edk2-aarch64-code.fd", "edk2-arm-vars.fd")},
                "autoexec_append": AUTOEXEC_APPEND.decode(), "cases": {},
                "capacity_rule": {"batch": BATCH, "spare": SPARE,
                                  "metal_requires_free": BATCH + SPARE},
                "scheduler_decisions": SCHED_DECISIONS,
                "heap_allowance": HEAP_ALLOWANCE,
                "settle_quiesce_ms": QUIESCE_STABLE_MS,
                "settle_retry_ms": QUIESCE_MAX_MS,
                "clean_settle_stable_ms": CLEAN_SETTLE_STABLE_MS,
                "clean_settle_max_ms": CLEAN_SETTLE_MAX_MS,
                "clean_settle_epsilon": CLEAN_SETTLE_EPSILON,
                "boot_timeout": args.timeout, "boot_stall": args.stall}
    started = time.monotonic()
    try:
        with (work / "build-proof.log").open("wb") as log:
            subprocess.run(IMAGE_ARGS, cwd=root, env=proof_env, stdout=log, stderr=log, check=True)
        files = unpack((source / "EFI/KUMO/initrd.img").read_bytes())
        require("bin/ax4idle" in files, "ax4idle missing from the proof initrd")
        manifest["elf_sha256"] = {}
        for name in ("sora", "ax4idle"):
            matches_ = [data for path, data in files.items() if path.endswith("/" + name)]
            require(len(matches_) == 1, f"{name} ELF missing from initrd")
            require(matches_[0] == (root / "target/aarch64-unknown-none/release" / name).read_bytes(),
                    f"{name} ELF mismatch")
            manifest["elf_sha256"][name] = sha(matches_[0])
        require("etc/autoexec" in files and b"echo kumo autoexec online" in files["etc/autoexec"],
                "default autoexec missing from the proof image")
        manifest["source_initrd_sha256"] = sha((source / "EFI/KUMO/initrd.img").read_bytes())
        manifest["source_image_sha256"] = {str(p.relative_to(source)): sha(p.read_bytes())
                                           for p in source.rglob("*") if p.is_file()}
        for n in (1, 2):
            manifest["cases"][f"boot{n}"] = boot(
                root, source, work / f"boot{n}", qemu, firmware, args.timeout, args.stall)
        manifest["proofs_passed"] = True
    except BaseException as error:
        manifest["proofs_passed"] = False
        manifest["error"] = str(error)
        raise
    finally:
        try:
            with (work / "restore-default.log").open("wb") as log:
                subprocess.run(IMAGE_ARGS, cwd=root, env=default_env, stdout=log, stderr=log, check=True)
            files = unpack((source / "EFI/KUMO/initrd.img").read_bytes())
            require(b"ax4bench" not in files["etc/autoexec"]
                    and b"echo kumo autoexec online" in files["etc/autoexec"],
                    "default autoexec not restored")
            manifest["default_restored"] = True
            manifest["default_initrd_sha256"] = sha((source / "EFI/KUMO/initrd.img").read_bytes())
        finally:
            manifest["passed"] = manifest.get("proofs_passed", False) and manifest.get("default_restored", False)
            manifest["total_seconds"] = round(time.monotonic() - started, 3)
            (work / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"AX4 QEMU proof green: two boots, five 256-child cycles, scheduler and wake series, "
          f"capacity refusal and recovery; default image restored ({manifest['total_seconds']}s)",
          flush=True)


class TranscriptTests(unittest.TestCase):
    def fixture(self, capacity=288, live=10, **overrides):
        stored = live
        lines = ["autoexec: done", "INDRA login:", "AX4 boot-services ready", BEGINS,
                 "AX4 settle sample=1 syscalls=1000 delta=3"]
        samples = QUIESCE_STABLE_MS // 250 + 1
        for q in range(1, samples + 1):
            lines.append(f"AX4 quiesce sample={q} stable-ms={min(q * 250, QUIESCE_STABLE_MS)} "
                         f"waited-ms={q * 250} live={live} stored={stored} "
                         f"blocked={live} ready=0 running=0 change=0")
        lines += ["AX4 settle ok sample=1 delta=3",
                 "AX4 policy limit=0 status=-1 PASS",
                 "AX4 policy limit=289 status=-1 PASS",
                 "AX4 policy limit=288 status=0 PASS",
                 f"AX4 census default-login capacity={capacity} live={live} stored={stored} "
                 f"blocked={live} ready=0 running=0 free={capacity - live} frames=5000 heap=100000"]
        for index in range(1, CYCLES + 1):
            lines.append(
                f"AX4 clean-settle cycle={index} stable-ms=500 waited-ms=500 heap=100000 change=0")
            lines.append(
                f"AX4 cycle={index} children=256 peak-live={live + 256} "
                f"peak-stored={stored + 256} clean-live={live} clean-stored={stored} "
                f"frames-delta=0 heap-delta=0 state-equal=1 koids-equal=1 contaminated=0 PASS")
        for n in SCHED_SIZES:
            lines.append(f"AX4 scheduler runnable={n} decisions=32768 total-ns={1000 + n}")
        for n in WAKE_SIZES:
            lines.append(f"AX4 wake n={n} mode=single samples=1 min-ns=100 median-ns=100 max-ns=100 PASS")
            lines.append(f"AX4 wake n={n} mode=broadcast targets={n - 1} samples={n - 1} "
                         f"min-ns=100 median-ns=200 max-ns=300 PASS clean=1")
        lines += [
            f"AX4 boundary fill capacity={capacity} live={capacity} stored={capacity} "
            f"spawns={capacity - live} PASS",
            f"AX4 refusal status=-11 capacity={capacity} live={capacity} stored={capacity} "
            f"state-equal=1 koids-equal=1 PASS",
            f"AX4 boundary recover spawn=Ok live={live + 1} PASS",
            "AX4 clean-settle final stable-ms=500 waited-ms=500 heap=100000 change=0",
            f"AX4 census final capacity={capacity} live={live} stored={stored} blocked={live} "
            f"ready=0 running=0 free={capacity - live} frames=6000 heap=100000",
            "AX4 PASS"]
        text = "\n".join(lines)
        for old, new in overrides.items():
            require(old in text, f"fixture override not found: {old!r}")
            text = text.replace(old, new)
        return text

    def test_complete_measurement(self):
        result = parse(self.fixture())
        self.assertEqual(result["summary"]["cycles"], CYCLES)
        self.assertEqual(result["summary"]["children_spawned"], CYCLES * BATCH)
        self.assertEqual(result["summary"]["refusal_status"], -11)
        self.assertEqual([row["runnable"] for row in result["scheduler"]], list(SCHED_SIZES))
        self.assertEqual(len(result["wakes"]), 2 * len(WAKE_SIZES))
        self.assertTrue(result["capacity"]["metal_rule_pass"])

    def test_metal_capacity_rule(self):
        parse(self.fixture(), metal=True)
        # 31 live leaves 257 free: 256 batch + 1, below the two-spare rule.
        with self.assertRaises(RuntimeError):
            parse(self.fixture(live=31), metal=True)
        parse(self.fixture(live=31), metal=False)

    def test_failure_and_missing_markers(self):
        text = self.fixture()
        for broken in (text + "\nTOWER", text + "\nPANIC",
                       text + "\nAX4 FAIL",
                       text.replace("AX4 PASS", "AX4 FAIL"),
                       text.replace("AX4 PASS", ""), text + "\nAX4 PASS",
                       text.replace("autoexec: done", ""),
                       text.replace("INDRA login:", ""),
                       text.replace("INDRA login:", "login later") + "\nINDRA login:"):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_policy_contract(self):
        text = self.fixture()
        for broken in (text.replace("limit=0 status=-1", "limit=0 status=0"),
                       text.replace("limit=289 status=-1", "limit=289 status=0"),
                       text.replace("limit=288 status=0", "limit=288 status=-1"),
                       text.replace("AX4 policy limit=289 status=-1 PASS\n", ""),
                       text.replace("AX4 policy limit=288 status=0 PASS", "AX4 policy limit=288 status=0 FAIL")):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_settle_contract(self):
        text = self.fixture()
        for broken in (text.replace("AX4 settle ok sample=1 delta=3", ""),
                       text.replace("delta=3", "delta=101", 1),
                       text.replace("AX4 settle ok sample=1 delta=3", "AX4 settle ok sample=2 delta=3"),
                       text.replace("AX4 settle sample=1", "AX4 settle sample=2")):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_timestamped_metal_capture(self):
        text = self.fixture()
        stamped = "\n".join(f"[19:54:38.695] {line}" for line in text.splitlines())
        parse(stamped, metal=True)

    def test_boot_services_readiness_required(self):
        text = self.fixture()
        with self.assertRaises(RuntimeError):
            parse(text.replace("AX4 boot-services ready\n", ""))
        with self.assertRaises(RuntimeError):
            parse(text.replace("AX4 boot-services ready\n", "") + "AX4 boot-services ready\n")

    def test_quiesce_contract(self):
        text = self.fixture()
        with self.assertRaises(RuntimeError):
            parse(re.sub(r"AX4 quiesce sample=.*\n", "", text))
        with self.assertRaises(RuntimeError):
            parse(text.replace(f"stable-ms={QUIESCE_STABLE_MS}",
                               f"stable-ms={QUIESCE_STABLE_MS - 1}"))
        last = text.rindex("AX4 quiesce sample=")
        with self.assertRaises(RuntimeError):
            parse(text[:last] + text[last:].replace("change=0", "change=1", 1))
        with self.assertRaises(RuntimeError):
            parse(text[:last] + text[last:].replace(
                "waited-ms=150250", f"waited-ms={QUIESCE_MAX_MS + 1}", 1))
        parse(re.sub(r"AX4 quiesce sample=20 .*\n",
                     "AX4 quiesce sample=20 stable-ms=7250 waited-ms=7250 live=10 "
                     "stored=10 blocked=10 ready=0 running=0 change=0\n", text))

    def test_clean_settle_contract(self):
        text = self.fixture()
        first = "AX4 clean-settle cycle=1 stable-ms=500 waited-ms=500 heap=100000 change=0"
        third = first.replace("cycle=1", "cycle=3")
        final = "AX4 clean-settle final stable-ms=500 waited-ms=500 heap=100000 change=0"
        for broken in (text.replace(first + "\n", ""),
                       text.replace(first, first.replace("stable-ms=500", "stable-ms=499")),
                       text.replace(first, first.replace("change=0", "change=1")),
                       text.replace(first, first.replace("waited-ms=500", "waited-ms=10001")),
                       text.replace(first, first.replace("heap=100000", "heap=0")),
                       text.replace(third + "\n", ""),
                       text.replace(final + "\n", ""),
                       text.replace(final, final.replace("stable-ms=500", "stable-ms=499")),
                       text.replace(final, final.replace("change=0", "change=1"))):
            with self.assertRaises(RuntimeError):
                parse(broken)
        # Moving a settle line out of its immediate-before slot breaks the interleaving.
        with self.assertRaises(RuntimeError):
            parse(text.replace(first + "\n", "").replace(third, third + "\n" + first))

    def test_census_contract(self):
        text = self.fixture()
        for broken in (text.replace("free=278", "free=277", 1),
                       text.replace("AX4 census default-login capacity=288 live=10 stored=10 "
                                    "blocked=10 ready=0",
                                    "AX4 census default-login capacity=288 live=10 stored=10 "
                                    "blocked=9 ready=0"),
                       text.replace("capacity=288 live=10 stored=10", "capacity=289 live=10 stored=10", 1),
                       text.replace("capacity=288 live=10 stored=10", "capacity=288 live=40 stored=40", 1),
                       text.replace("AX4 census final ", "AX4 census interim "),
                       text.replace("frames=6000 heap=100000", "frames=6000 heap=400000"),
                       text.replace("AX4 census default-login ", "AX4 census boot ")):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_cycle_contract(self):
        text = self.fixture()
        first = ("AX4 cycle=1 children=256 peak-live=266 peak-stored=266 clean-live=10 "
                 "clean-stored=10 frames-delta=0 heap-delta=0 state-equal=1 koids-equal=1 "
                 "contaminated=0 PASS")
        second = first.replace("cycle=1", "cycle=2")
        contaminated = first.replace("frames-delta=0", "frames-delta=7").replace(
            "contaminated=0", "contaminated=1")
        contamination_line = ("AX4 contamination cycle=1 frames-delta=7 settle=green "
                              "source=first-cycle-allowance")
        for broken in (text.replace(first + "\n", ""),
                       text.replace(second, second.replace("cycle=2", "cycle=1")),
                       text.replace(first, first.replace("children=256", "children=255")),
                       text.replace(first, first.replace("peak-live=266", "peak-live=265")),
                       text.replace(first, first.replace("clean-live=10", "clean-live=11")),
                       text.replace(first, first.replace("state-equal=1", "state-equal=0")),
                       text.replace(first, first.replace("koids-equal=1", "koids-equal=0")),
                       text.replace(second, second.replace("frames-delta=0", "frames-delta=1")),
                       text.replace(second, second.replace("heap-delta=0", "heap-delta=262144")),
                       text.replace(second, second.replace("heap-delta=0", "heap-delta=100000")),
                       text.replace(second, second.replace("contaminated=0", "contaminated=1")),
                       text.replace(first, first.replace("PASS", "FAIL")),
                       text.replace(first, contaminated),
                       text.replace("AX4 cycle=5", "AX4 cycle=6")):
            with self.assertRaises(RuntimeError):
                parse(broken)
        accepted = text.replace(first, contaminated).replace(
            "AX4 PASS", contamination_line + "\nAX4 PASS")
        result = parse(accepted)
        self.assertEqual(result["summary"]["contaminated_cycles"], 1)

    def test_scheduler_contract(self):
        text = self.fixture()
        for broken in (text.replace("AX4 scheduler runnable=64 decisions=32768 total-ns=1064\n", ""),
                       text.replace("runnable=64", "runnable=32", 1),
                       text.replace("runnable=16 decisions=32768", "runnable=16 decisions=16384"),
                       text.replace("runnable=16 decisions=32768 total-ns=1016",
                                    "runnable=16 decisions=32768 total-ns=0"),
                       text.replace("runnable=256 decisions=32768 total-ns=1256",
                                    f"runnable=256 decisions=32768 total-ns={U64_MAX}")):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_wake_contract(self):
        text = self.fixture()
        for broken in (text.replace("AX4 wake n=16 mode=broadcast targets=15 samples=15 "
                                    "min-ns=100 median-ns=200 max-ns=300 PASS clean=1\n", ""),
                       text.replace("targets=15 samples=15", "targets=16 samples=16"),
                       text.replace("targets=15 samples=15", "targets=15 samples=14"),
                       text.replace("mode=broadcast targets=15", "mode=single targets=15"),
                       text.replace("median-ns=200", "median-ns=50", 1),
                       text.replace("max-ns=300 PASS clean=1", "max-ns=300 PASS clean=0", 1),
                       text.replace("max-ns=100 PASS", "max-ns=0 PASS", 1),
                       text.replace("max-ns=100 PASS", f"max-ns={U64_MAX} PASS", 1)):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_boundary_contract(self):
        text = self.fixture()
        for broken in (text.replace("AX4 boundary fill capacity=288 live=288", "AX4 boundary fill capacity=288 live=287"),
                       text.replace("spawns=278", "spawns=277"),
                       text.replace("status=-11", "status=-5"),
                       text.replace("state-equal=1 koids-equal=1 PASS", "state-equal=0 koids-equal=1 PASS"),
                       text.replace("state-equal=1 koids-equal=1 PASS", "state-equal=1 koids-equal=0 PASS"),
                       text.replace("AX4 refusal ", "AX4 rejected "),
                       text.replace("spawn=Ok live=11", "spawn=Err live=11"),
                       text.replace("spawn=Ok live=11", "spawn=Ok live=12"),
                       text.replace("AX4 boundary recover ", "AX4 recovery ")):
            with self.assertRaises(RuntimeError):
                parse(broken)

    def test_order_is_enforced(self):
        text = self.fixture()
        fill = ("AX4 boundary fill capacity=288 live=288 stored=288 spawns=278 PASS")
        with self.assertRaises(RuntimeError):
            parse(text.replace("autoexec: done\nINDRA login:", "INDRA login:\nautoexec: done"))
        with self.assertRaises(RuntimeError):
            parse(text.replace("AX4 census final ", "AX4 census early ").replace(
                "AX4 PASS", "AX4 PASS\nAX4 census final capacity=288 live=10 stored=10 "
                            "blocked=10 ready=0 running=0 free=278 frames=6000 heap=100000"))
        with self.assertRaises(RuntimeError):
            parse(text.replace(fill + "\n", "").replace(
                "AX4 PASS", fill + "\nAX4 PASS"))

    def test_autoexec_staging(self):
        require(len(AUTOEXEC_APPEND) <= AUTOEXEC_LIMIT, "append alone exceeds the limit")
        autoexec, other = b"echo kumo autoexec online\n", b"binary"
        table = (b"etc/autoexec".ljust(64, b"\0") + struct.pack("<QQ", 16 + 2 * 80, len(autoexec))
                 + b"bin/other".ljust(64, b"\0")
                 + struct.pack("<QQ", 16 + 2 * 80 + len(autoexec), len(other)))
        data = b"KUMORD01" + struct.pack("<II", 1, 2) + table + autoexec + other
        path = Path(tempfile.mkdtemp(prefix="ax4-stage-", dir=os.environ.get("TMPDIR", "/tmp"))) / "initrd.img"
        path.write_bytes(data)
        try:
            digest, staged = stage(path)
            self.assertEqual(staged, autoexec + AUTOEXEC_APPEND)
            self.assertEqual(unpack(path.read_bytes())["bin/other"], other)
            self.assertEqual(digest, sha(path.read_bytes()))
            big = b"x" * (AUTOEXEC_LIMIT + 1)
            big_table = (b"etc/autoexec".ljust(64, b"\0") + struct.pack("<QQ", 16 + 2 * 80, len(big))
                         + b"bin/other".ljust(64, b"\0")
                         + struct.pack("<QQ", 16 + 2 * 80 + len(big), len(other)))
            path.write_bytes(b"KUMORD01" + struct.pack("<II", 1, 2) + big_table + big + other)
            with self.assertRaises(RuntimeError):
                stage(path)
        finally:
            shutil.rmtree(path.parent, ignore_errors=True)


class GateTests(unittest.TestCase):
    def fixture(self, live=9):
        cap = CAPACITY_MAX
        rows = ["autoexec: done", "INDRA login:", "AX4 boot-services ready",
                "AX4 gate begins: post-AX4 capacity gate (J819)",
                "AX4 quiesce sample=600 stable-ms=150000 waited-ms=150000 live=9 stored=9 blocked=9 ready=0 running=0 change=0",
                "AX4 settle ok sample=1 delta=3"]
        for label, count in [("default-login", live), ("netstack-client", live + 2), ("gate-clean", live + 1)]:
            rows.append(f"AX4 census {label} capacity={cap} live={count} stored={count} blocked={count} ready=0 running=0 free={cap-count} frames=10000 heap=600000")
        rows += [f"AX4 capacity-gate free={cap-live-2} requires=2 PASS", "netprobe: hold PASS", "AX4 gate PASS"]
        return "\n".join(rows)

    def test_normal_and_missing_client(self):
        self.assertTrue(parse_gate(self.fixture())["passed"])
        with self.assertRaises(RuntimeError):
            parse_gate(self.fixture().replace("netprobe: hold PASS", ""))

    def test_insufficient_spares(self):
        with self.assertRaises(RuntimeError):
            parse_gate(self.fixture(live=285))

    def test_cleanup_and_readiness(self):
        text = self.fixture()
        with self.assertRaises(RuntimeError):
            parse_gate(text.replace("gate-clean capacity=288 live=10 stored=10 blocked=10", "gate-clean capacity=288 live=9 stored=10 blocked=9"))
        with self.assertRaises(RuntimeError):
            parse_gate(text.replace("AX4 boot-services ready", ""))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, subprocess.SubprocessError, OSError) as error:
        print(f"AX4 proof FAILED: {error}", file=sys.stderr)
        sys.exit(1)
