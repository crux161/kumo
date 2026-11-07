# Exact EP0 data transfers
<!-- j827 -->

Date: 2026-11-07. Author: TIMBERDOODLE.

NET4a provides bounded control data IN/OUT in `kumo-xhci::ControlData` and
`ControlPipe`, integrated into `drv-xhci`. Existing no-data and ordinary legacy
control paths are unchanged. The NET0 exact descriptor path now uses this state
machine. Recovery class is stateless: USB state can be rebuilt after reset; an
uncertain live transfer still requires retained DMA until quiescence is proved.

## Request and completion contract

A request accepts 1..256 data bytes within the caller's buffer, a nonzero IOVA
without arithmetic overflow or a 64 KiB crossing, and the exact outbound payload
for OUT. IN accepts no outbound payload. Setup `wLength` and `bmRequestType` determine
the data length/direction and opposite status direction.

The three TRBs are separate Setup/Data/Status TDs. Data IOC+ISP exposes its actual
residue; Status IOC proves the handshake. Both events must report Success with
zero residue, in data-then-status order, matching slot, DCI1 and exact TRB address.
The ring is reclaimed through Status only after both checks. The deliberate extra
Data IOC retains NET0's exact-read method: xHCI1.2b section4.10.1 permits multiple
IOC events; section6.4.1.2 recommends Status-only IOC. Direction rules are table4-7.
[Intel xHCI1.2b specification](https://cdrdv2-public.intel.com/625472/625472_xHCI_Rev1_2b.pdf).
— TIMBERDOODLE

Preflight checks an idle pipe, an empty ring with space for all three TRBs, matching
ring backing and a nonzero nonoverflowing deadline, before any DMA buffer write or
ring publication. OUT copies exact bytes with volatile stores; IN zeroes through
the volatile buffer API. The existing uncached mapping and device barriers remain.
No Rust slice over live DMA is exposed.

Short/error/stall, duplicate/reordered owned events, invalid residue or timeout
quarantine the pipe without reclaim or resubmit. Foreign slot/endpoint events return
to their owner. The driver reserves stash capacity before popping an event and
aborts its pipe if retention is full. The live poll has a 200 ms deadline and a hard
iteration ceiling with cooperative yields. A deadline is a refusal to wait longer,
never a DMA-release proof. There is no unquarantine API in this slice.

The existing slot-only stash and legacy pollers still need NET4b's endpoint/token/
generation routing. Pending resident NIC lanes, full mailbox behavior, endpoint
stop/disable/reset recovery and a flat repeated resource census belong to NET4c.
This EP0 proof does not accept concurrent RX/TX/status or controller-owner restart.

## Repeatable software and board proofs

Run `cargo test -p kumo-xhci -p drv-xhci -p kumo-ax88`. The control model tests literal
IN/OUT TRBs and payloads, every setup length, short buffers, address/64 KiB bounds,
all completion-code values, residual/order/deadline failures, foreign ownership,
preflight atomic refusal and 1,000 completed IN/OUT transactions across ring wraps.

The board proof is explicitly default-off:

```sh
KUMO_AX88_EP0_PROOF=1 cargo xtask image --arch aarch64 --hardware orange-pi-5-plus
```

It runs only after exact NET0 reads and the conservative NET3 profile match. It
uses vendor/device access to volatile MAC register34 (RX checksum control), toggles
only documented IPv4 checksum bit0, reads the changed value back, restores the
original byte and reads it back. Sixteen cycles make 65 exact requests (33 IN,
32 OUT) and reclaim 195 TRBs, with an empty ring after each cycle. It issues no
SetConfiguration, Ethernet endpoint setup, EEPROM/eFuse command or Ethernet TX.
Any uncertain result stops the proof and retains the slot/page; it cannot safely
attempt restoration through that uncertain pipe. Reset is then required before
further adapter use. — TIMBERDOODLE

Capture boot logs at an appropriate console level or run `dmesg` after login, then:

```sh
python3 scripts/ax88-ep0-proof.py --self-test
python3 scripts/ax88-ep0-proof.py capture.log
```

The checker requires all16 ordered cycles, exact totals, restoration and accepted
NET0 device18/config39 SHA256s. Its fixed reference is the first Orange Pi AX88179
adapter, not a generic oracle for another chip/host. Self-tests reject missing,
duplicate/reordered and corrupted proof records. Fixtures are not physical evidence.

J827's Orange Pi result was 65 exact requests, register34 `00 → 01 → 00` each cycle,
with descriptors matching J809/J811. The adapter was high speed on controller
`fc400000`, root1/route3/slot3; keyboard on route4/slot4 armed afterward. Both NVMe
persistence files read correctly and the volume unmounted. Physical keyboard and
USB-stick witnesses are recorded separately in the journal; enumeration alone
is not a physical keyboard test. Ordinary images omit the volatile proof.

Next is NET4b endpoint-aware completion ownership, followed by NET4c's resident
nonblocking lanes and buffer-custody/recovery proof. — TIMBERDOODLE 2026-11-07
