# ax88 protocol codec
<!-- j826 -->
<!-- j833 -->

Date: 2026-11-07. Author: TIMBERDOODLE.

`lib/kumo-ax88` is the allocation-free, `no_std`, safe Rust codec for the AX88179 /
AX88178A family. It describes bytes; `drv-xhci` continues to own USB transport,
completion routing and buffer lifetime. The existing `drv_xhci::ax88` inventory API
reexports the library's unchanged NET0 implementation. This slice submits no new
USB request, configures no Ethernet endpoint and sends no Ethernet frame.

## Sources and supported profile

The supplied `resources/net/ax88179_178a.c` is the wire reference: its
`ax88179_rx_fixup`, `ax88179_tx_fixup`, command helpers and named register constants.
Its SHA-256 is
`b4272082722ab1c31ee6d67d106200b41e1558809f0ed33f8dec41a55828802f`.
The GPL driver is reference material; no C implementation is incorporated here.
`resources/net/AX88179.pdf` (V1.31, section 3.1) confirms the bulk IN/OUT and
interrupt IN topology; SHA-256
`e86165513bbf2a346cc935fb97b6e727604984ac4ebd79901d5dbe046a132e55`.
The informal `ax88179_note.md` describes a different RX arrangement and a four-byte
TX header. Those descriptions must not drive implementation. — TIMBERDOODLE

`profile::Profile` layers a conservative binding policy over NET0's descriptor
inventory. It selects endpoint roles by direction and transfer type, independent of
descriptor order. It accepts only ASIX `0b95:1790` / `0b95:178a`, revision `0100`,
one vendor `ff/ff/00` alternate-zero interface, bulk IN and OUT, and interrupt IN.
Recognized USB revision encodings are `0200`, `0210`, `0300`. Full/high-speed bulk
MPS is 64/512 with EP0 encoding 64; SuperSpeed is 1024 with EP0 encoding 9 and
adjacent validated companions. Streams, bulk polling intervals, extra endpoint
attribute bits and unsupported layouts refuse. Status packets must be eight bytes
with a valid interval and, at SuperSpeed, an eight-byte nonburst companion.

Only the AX88179 profile accepts SuperSpeed. AX88178A full/high speed and AX88179
SuperSpeed are synthetic fixture coverage, not new physical acceptance. The NET0
adapter was AX88179 revision0100 at high speed, EP81 status / EP82 RX / EP03 TX.
Another revision or host requires its own reviewed initialization and hardware proof.
The profile is not yet wired into the live driver's configuration path.

## Vendor requests

`control::Request` constructs USB setup bytes in little-endian order and retains
at most six outbound bytes. Vendor/device IN uses `bmRequestType=0xc0`; OUT uses
`0x40`. Exact successful data and status completion must precede
`validate_transferred(actual)`; that function checks only the byte count, not USB
completion or timeout state.

| Access | bRequest | wValue | wIndex | wLength |
|---|---|---|---|---|
| MAC | 1 | named register address | register width | register width |
| PHY | 2 | PHY address 3 | explicit register 0..31 | 2 |

Named MAC registers are USB link `02` (1 byte), silicon status `03` (1), RX control
`0b` (2), node ID `10` (6), medium `22` (2), PHY power/reset `26` (2), RX bulk queue
`2e` (5), clock `33` (1), RX checksum `34` (1), TX checksum `35` (1).
Link, silicon status and node ID are read-only in this API. Other MAC writes accept
exact raw register bytes; PHY writes encode a `u16` little-endian. EEPROM, eFuse,
reload and arbitrary opcode/register constructors are absent. PHY page selection is
an explicit register31 write, never a side effect of reading another register.
`decode_mac` requires exactly six nonzero unicast address bytes; locally administered
addresses are allowed.

These encoders do not approve an initialization sequence. NET4a proves USB control
transport; NET5a proves identity reads; NET5b records every volatile write, delay,
readback and software-offload setting before RX/TX is enabled.

## RX aggregate

The enabled RX mode must provide the two-byte IP-alignment pseudo header. NET5b
(J833) sets and reads back RX control 0288 with IPE; see [the link proof](ax88-link.md).
The aggregate is:

```text
[wire packet, padding to 8 bytes] ...
[LE32 metadata entry] ... [optional 4 bytes] [LE32 trailer]
```

The trailer's low16 bits count metadata entries, including dummies; high16 bits are
the metadata table's byte offset. Each metadata word's bits16..28 give wire length.
Zero length is a dummy and consumes no packet bytes. For nonzero lengths, the data
cursor advances by `(wire_length + 7) & !7`. CRC error bit29 or hardware-drop bit31
drops the frame while consuming its validated padded span. Runt/oversize packets
are similarly counted and dropped. Accepted frame bytes exclude the first two wire
bytes; no additional FCS trimming is performed.

`rx::Aggregate::parse` validates the entire immutable transfer before returning an
iterator. The metadata/table/trailer must fit; the optional table-to-trailer gap is
exactly zero or four bytes, with unspecified contents. Packet data must consume
exactly the region before metadata, without overlap or unexplained gaps. Even an
error-marked packet's padded span must fit. A malformed later entry therefore cannot
expose an earlier frame. Old layouts without dummies or final padding are accepted.

An aggregate is at most 32 KiB with at most 1024 metadata entries, bounding both
validation passes. These are software work budgets; exceeding them rejects the
aggregate. Frames are 14..1514 bytes without FCS, for the initial untagged MTU1500
contract. Hardware IP/transport checksum flags are telemetry; software verification
remains required. Empty aggregates refuse; valid all-dummy/all-drop aggregates can
return zero frames. Counts distinguish accepted, dropped and dummy entries.

The codec allocates nothing and holds no queue or DMA authority. A caller must prove
transfer completion, retain the buffer while borrowed slices exist, and finish
borrowing/copying before resubmitting it. Parse success alone is never permission to
reuse an in-flight buffer. — TIMBERDOODLE

## TX prefix

Two little-endian words precede the unchanged Ethernet bytes: frame length, then
zero MSS/offload control. When `(frame_length + 8) % endpoint_max_packet == 0`, the
second word is `0x80008000`. The transfer still contains exactly `8 + frame_length`
bytes. The codec neither appends padding bytes nor requests a guessed USB ZLP.
Supported max-packet sizes are 64, 512 and 1024; unsupported frame sizes/MPS refuse.
`tx::encode` checks output capacity before writing and preserves output on error.

## Proof and next gate

Run `cargo test -p kumo-ax88 -p drv-xhci`. Fixtures are explicitly synthetic, including
an independently written literal RX aggregate and literal control/TX prefixes.
Tests cover every prefix truncation of the RX fixture, all 8192 encoded packet lengths,
all 65536 values of each trailer field, byte mutations, maximum work/transfer bounds,
late invalid entries, dummies/CRC/drop, and every frame length at each supported MPS.
Descriptor fixtures cover both families, endpoint reordering and rejected profiles.
No physical aggregate or Ethernet traffic claim follows from these tests.

Next is NET4a's bounded EP0 data IN/OUT and exact completion proof. NET4b completion
ownership and NET4c nonblocking resident lanes still precede chip initialization and
traffic. Those lanes remain in-process actors in the single `drv-xhci` controller
owner. AX4 has supplied resident capacity; AX6 mailbox bounds and AX9 nonblocking
Sora service remain separate open gates. — TIMBERDOODLE 2026-11-07
