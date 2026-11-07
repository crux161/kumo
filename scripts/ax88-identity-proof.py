#!/usr/bin/env python3
#j832
"""Compare NET5a's independent firmware and exact KUMO vendor-IN evidence."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import sys
import unittest

FIELDS = [
    ("node_id", "c001100006000600"), ("usb_link", "c001020001000100"),
    ("silicon_status", "c001030001000100"), ("phy_power_reset", "c001260002000200"),
    ("clock", "c001330001000100"), ("phy_page", "c00203001f000200"),
    ("phy_id1", "c002030002000200"), ("phy_id2", "c002030003000200"),
    ("phy_basic_status", "c002030001000200"), ("phy_physical_status", "c002030011000200"),
]
DEVICE = "12011002ffff0040950b9017000101020301"
CONFIG = "09022700010100e0020904000003ffff00040705810308000b0705820200020007050302000200"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def only(pattern, text, label):
    records = list(re.finditer(pattern, text))
    require(len(records) == 1, f"need one {label}, found {len(records)}")
    return records[0]


def parse(text):
    text = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]", "", text).replace("\r", "")
    for word in ["NET5a KUMO REFUSED", "NET5a UEFI REFUSED", "NET5a KUMO unavailable",
                 "NET5a UEFI unavailable", "NET5a KUMO FAIL", "NET4b completion fault",
                 "TOWER", "ABEND", "PANIC", "NET4a EP0 start", "NET4c3 nic ready"]:
        require(word not in text, f"forbidden record: {word}")
    require("INDRA login:" in text, "no login")
    only(r"NET0 UEFI ax88 cached reference: matches=1 complete=1 unique=true no transfer/reset call", text, "unique firmware reference")
    device = only(r"NET0 UEFI ax88 instance=(\d+) DEVICE ([0-9a-f]+)", text, "firmware device")
    require(device[2] == DEVICE, "firmware descriptor changed")
    instance = device[1]
    raw = bytearray()
    for offset, data in re.findall(rf"NET0 UEFI ax88 instance={instance} CONFIG offset=(\d+) ([0-9a-f]+)", text):
        require(int(offset) == len(raw), "firmware config gap/duplicate")
        raw.extend(bytes.fromhex(data))
    require(raw.hex() == CONFIG, "firmware config changed/truncated")
    native = only(r"NET0 ax88 controller=(0x[0-9a-f]+) root=(\d+) route=(0x[0-9a-f]+) slot=(\d+) speed=3 vid=0b95 pid=1790 exact=true", text, "exact native inventory")
    for name, expected in [("DEVICE", DEVICE), ("CONFIG", CONFIG)]:
        data = bytearray()
        for offset, chunk in re.findall(rf"NET0 ax88 ctrl={native[1]} slot={native[4]} {name} offset=(\d+) ([0-9a-f]+)", text):
            require(int(offset) == len(data), f"native {name} gap/duplicate")
            data.extend(bytes.fromhex(chunk))
        require(data.hex() == expected, f"native {name} differs from firmware")
    ustart = only(r"NET5a UEFI start instance=(\d+) fields=10 writes=0 length_reported=false", text, "firmware start")
    require(ustart[1] == instance, "another firmware interface")
    uend = only(r"NET5a UEFI PASS reads=10 writes=0 length_reported=false", text, "firmware result")
    kstart = only(r"NET5a KUMO start controller=(0x[0-9a-f]+) slot=(\d+) fields=10 writes=0", text, "native start")
    require((kstart[1], kstart[2]) == (native[1], native[4]), "another native adapter")
    kend = only(r"NET5a KUMO PASS reads=10 registered=10 routed=20 consumed=20 retired=10 faults=0 queued=0 writes=0", text, "native completion totals")
    require(ustart.end() < uend.start() < native.start() < kstart.start() < kend.start(), "phase ordering")
    results = {}
    recognized = [r[0] for r in [ustart, uend, kstart, kend]]
    for origin, begin, end, pattern in [
        ("UEFI", ustart, uend, r"NET5a UEFI field=(\w+) setup=([0-9a-f]+) status=(0x[0-9a-f]+) usb=(0x[0-9a-f]+) bytes=([0-9a-f]+)"),
        ("KUMO", kstart, kend, r"NET5a KUMO field=(\w+) setup=([0-9a-f]+) bytes=([0-9a-f]+) actual=(\d+)"),
    ]:
        records = list(re.finditer(pattern, text))
        require([r[1] for r in records] == [f[0] for f in FIELDS], f"{origin}: missing/duplicate/reordered fields")
        recognized.extend(r[0] for r in records)
        values = {}
        for r, (name, setup) in zip(records, FIELDS):
            require(begin.end() < r.start() < r.end() < end.start(), f"{origin}: record outside phase")
            require(r[2] == setup, f"{origin}: unexpected request, OUT/opcode/page write")
            length = bytes.fromhex(setup)[6]
            if origin == "UEFI":
                require(int(r[3], 16) == int(r[4], 16) == 0, "firmware USB/status error")
                data = bytes.fromhex(r[5])
            else:
                data = bytes.fromhex(r[3])
                require(int(r[4]) == length, "native short completion")
            require(len(data) == length, f"{origin}: malformed sample width")
            values[name] = data
        results[origin] = values
    u, k = results["UEFI"], results["KUMO"]
    for name in ["node_id", "silicon_status"]:
        require(u[name] == k[name], f"independent {name} mismatch")
    mac = k["node_id"]
    require(len(mac) == 6 and mac != b"\0" * 6 and not mac[0] & 1, "invalid/multicast/zero MAC")
    require(all(v["usb_link"][0] & 6 == 2 for v in [u, k]), "chip USB speed disagrees with observed high speed")
    scalar = lambda values, name: int.from_bytes(values[name], "little")
    phy = {}
    for origin, values in results.items():
        page, id1, id2, physical = [scalar(values, n) for n in ["phy_page", "phy_id1", "phy_id2", "phy_physical_status"]]
        identity = id1 << 16 | id2
        if identity in [0, 0xffffffff] or physical == 0xffff or page == 0xffff:
            phy[origin] = {"available": False, "reason": "invalid ID/page/status", "rust": "Unavailable"}
        elif page != 0:
            phy[origin] = {"available": False, "reason": "unknown page", "page": page, "rust": f"UnknownPage({page})"}
        else:
            link = bool(physical & 0x400)
            speed = {0: 10, 0x4000: 100, 0x8000: 1000}.get(physical & 0xc000) if link else None
            full = bool(physical & 0x2000)
            rust = f"Page0 {{ id: {identity}, link: {str(link).lower()}, speed_mbps: " + (f"Some({speed})" if speed else "None") + f", full_duplex: {str(full).lower()} }}"
            phy[origin] = dict(available=True, id=hex(identity), reported_link=link, reported_speed_mbps=speed, rust=rust)
    if all(p["available"] for p in phy.values()):
        require(phy["UEFI"]["id"] == phy["KUMO"]["id"], "PHY identity mismatch")
    summary = only(r"NET5a KUMO identity mac=([0-9a-f]+) usb=(0x[0-9a-f]+) silicon=(0x[0-9a-f]+) power=(0x[0-9a-f]+) clock=(0x[0-9a-f]+) phy=([^\n]+)", text, "decoded identity")
    require(summary[1] == mac.hex(), "summary MAC mismatch")
    require([int(summary[n], 16) for n in range(2, 6)] == [scalar(k, n) for n in ["usb_link", "silicon_status", "phy_power_reset", "clock"]], "summary scalar mismatch")
    require(summary[6] == phy["KUMO"]["rust"] and kstart.end() < summary.start() < kend.start(), "summary PHY/ordering mismatch")
    recognized.append(summary[0])
    proof_lines = [line[line.index("NET5a "):].strip() for line in text.splitlines() if "NET5a " in line]
    require(len(proof_lines) == len(recognized) and set(proof_lines) == set(recognized),
            "unknown/malformed identity proof record")
    return dict(passed=True, controller=native[1], slot=int(native[4]),
        mac=":".join(f"{b:02x}" for b in mac), locally_administered=bool(mac[0] & 2),
        silicon_status=hex(k["silicon_status"][0]), stepping="UA2" if k["silicon_status"][0] & 4 else "UA1",
        usb="high-speed", phy={o: {n: v for n,v in p.items() if n != "rust"} for o,p in phy.items()},
        samples={o: {n: v.hex() for n,v in values.items()} for o,values in results.items()},
        firmware_actual_length_observable=False, exact_native_reads=10, vendor_writes=0,
        descriptor_sha256={"device":hashlib.sha256(bytes.fromhex(DEVICE)).hexdigest(),"config":hashlib.sha256(bytes.fromhex(CONFIG)).hexdigest()})


def fixture():
    lines = [f"NET0 UEFI ax88 instance=2 DEVICE {DEVICE}"]
    lines += [f"NET0 UEFI ax88 instance=2 CONFIG offset={at} {CONFIG[at*2:(at+16)*2]}" for at in range(0,39,16)]
    lines += ["NET0 UEFI ax88 cached reference: matches=1 complete=1 unique=true no transfer/reset call", "NET5a UEFI start instance=2 fields=10 writes=0 length_reported=false"]
    values = ["1a2b3c4d2904","02","06","0080","03","0000","0000","0000","0000","0000"]
    lines += [f"NET5a UEFI field={n} setup={s} status=0x0 usb=0x0 bytes={v}" for (n,s),v in zip(FIELDS,values)]
    lines += ["NET5a UEFI PASS reads=10 writes=0 length_reported=false", "NET0 ax88 controller=0xfc400000 root=1 route=0x3 slot=3 speed=3 vid=0b95 pid=1790 exact=true"]
    for name, data in [("DEVICE",DEVICE),("CONFIG",CONFIG)]:
        lines += [f"NET0 ax88 ctrl=0xfc400000 slot=3 {name} offset={at} {data[at*2:(at+16)*2]}" for at in range(0,len(data)//2,16)]
    lines += ["NET5a KUMO start controller=0xfc400000 slot=3 fields=10 writes=0"]
    lines += [f"NET5a KUMO field={n} setup={s} bytes={v} actual={len(v)//2}" for (n,s),v in zip(FIELDS,values)]
    lines += ["NET5a KUMO identity mac=1a2b3c4d2904 usb=0x02 silicon=0x06 power=0x8000 clock=0x03 phy=Unavailable", "NET5a KUMO PASS reads=10 registered=10 routed=20 consumed=20 retired=10 faults=0 queued=0 writes=0", "INDRA login:"]
    return "\n".join(lines) + "\n"


class Tests(unittest.TestCase):
    def test_literal_independent_trace_and_unavailable_phy(self):
        result=parse("\x1b[0m"+fixture().replace("\n","\r\n"))
        self.assertTrue(result["passed"])
        self.assertFalse(result["phy"]["KUMO"]["available"])
        self.assertTrue(result["locally_administered"])

    def test_missing_duplicate_reordered_records(self):
        lines=fixture().splitlines(keepends=True)
        for i,line in enumerate(lines):
            with self.subTest(i=i), self.assertRaises(ValueError): parse("".join(lines[:i]+lines[i+1:]))
            if "INDRA login:" not in line:
                with self.subTest(duplicate=i), self.assertRaises(ValueError): parse("".join(lines[:i]+[line]+lines[i:]))
        a=lines.index(next(l for l in lines if "KUMO field=phy_id1" in l));lines[a],lines[a+1]=lines[a+1],lines[a]
        with self.assertRaises(ValueError):parse("".join(lines))

    def test_mutation_errors_never_pass(self):
        for old,new in [("c001100006000600","4001100006000600"),("status=0x0 usb=0x0","status=0x0 usb=0x2"),
            ("1a2b3c4d2904","1a2b3c4d2905"),("actual=6","actual=5"),("reads=10 registered=10","reads=10 registered=9"),
            ("faults=0","faults=1"),("queued=0","queued=1"),("phy=Unavailable","phy=UnknownPage(0)"),
            ("speed=3 vid=","speed=4 vid="),("writes=0","writes=1"),("length_reported=false","length_reported=true")]:
            with self.subTest(field=old),self.assertRaises(ValueError):parse(fixture().replace(old,new,1))
        for line in ["NET5a KUMO unavailable field=phy_id1", "TOWER", "NET4a EP0 start", "NET5a KUMO write unknown"]:
            with self.assertRaises(ValueError):parse(fixture()+line)

    def test_zero_multicast_and_available_phy_decoding(self):
        for mac in ["000000000000","1b2b3c4d2904","ffffffffffff"]:
            with self.assertRaises(ValueError):parse(fixture().replace("1a2b3c4d2904",mac))
        text=fixture().replace("field=phy_id1 setup=c002030002000200 status=0x0 usb=0x0 bytes=0000","field=phy_id1 setup=c002030002000200 status=0x0 usb=0x0 bytes=3412").replace("field=phy_id1 setup=c002030002000200 bytes=0000","field=phy_id1 setup=c002030002000200 bytes=3412")
        text=text.replace("phy=Unavailable","phy=Page0 { id: 305397760, link: false, speed_mbps: None, full_duplex: false }")
        self.assertTrue(parse(text)["phy"]["KUMO"]["available"])


def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument("capture",type=Path,nargs="?");parser.add_argument("--self-test",action="store_true");args=parser.parse_args()
    if args.self_test:
        return 0 if unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful() else 1
    if args.capture is None:parser.error("capture or --self-test required")
    try:print(json.dumps(parse(args.capture.read_text(errors="replace")),indent=2))
    except ValueError as error:print(f"NET5a identity proof FAIL: {error}",file=sys.stderr);return 1
    print("NET5a identity proof green: independent MAC/silicon match; ten exact native IN reads, zero writes")
    return 0


if __name__=="__main__":sys.exit(main())
