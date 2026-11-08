# ax88 transmit and full duplex (NET7a)
<!-- j835 -->

Date: 2026-11-07. Author: TIMBERDOODLE.

NET7a adds the bulk OUT lane on the resident owner and proves, with the peer's own
capture, that every frame KUMO hands to the bus reaches the wire byte-exact. A USB
completion only says the chip took the transfer; delivery is whatever the peer
captured. Only `KUMO_AX88_TX_PROOF=1` images run it. That flag implies NET6's
receive mode, so TX runs while the peer's RX test frames are arriving. Ordinary images
still leave the adapter unconfigured. The device-frame channel between drv-xhci and
netstack (KNF1 credits, backpressure, caller isolation) is NET7b.

## Lane and buffer custody

The OUT endpoint (EP 0x03, MPS 512 at high speed) joins the status and RX endpoints in
one Configure Endpoint. Only the TX flag adds it, so earlier proofs keep their exact
two-endpoint census. It reads back Running at its fresh ring, and
CLEAR_FEATURE(ENDPOINT_HALT) resets its data toggle like the others. Its 16-TRB ring
(offset 0x300) and one 1536-byte frame buffer (0x400) live in the same page as the other
rings, inside the custodied 64 KiB NIC backing. A page cannot straddle a 64 KiB
boundary, so neither can they.

One transfer is outstanding at a time. The next frame is encoded into the buffer only
while the lane is idle, which happens only after the previous transfer's exact Success
completion was routed, validated, retired and reclaimed. A completion code other than
Success, or any residue, stops the schedule with the lane failed and everything
retained; nothing is resubmitted. Full-duplex load starves OUT completions: on metal
the frame in flight when the peer's receive burst landed took 12.2 s and 32.5 s on
two boots. A completion delayed more than 1 s is recorded as `NET7 tx slow`; one
that never arrives within 120 s stops the schedule the same way, recorded once as
`stage=deadline` with the endpoint state, so a lost event cannot wedge the proof. Each transfer is NET3's 8-byte prefix (length,
then 0) followed by the frame. When `length + 8` is a multiple of the max packet, the
prefix's second word is `0x80008000`, with no extra bytes and no ZLP. The reference
link_reset()'s `0x81/0x8c` TX FIFO check is not issued. The vendor request codec cannot
express it, and the capture shows whether TX works without it.

## Test frames

`kumo_ax88::probe::tx_frame` builds broadcast frames from the chip's node address
(read once over EP0 when receive turns on) on the IEEE local-experimental EtherType
0x88B5. No LAN host interprets them. The payload is `KUMOTX07`, sequence (u32 LE),
frame length (u16 LE), version 1, 0, then `pattern(seq, i)`. 32 exact-phase lengths go
one per 100 ms tick from 2 s after receive turns on: 30, 42 and 59 (the chip must pad
these), 60–64, 503–513 and 1015–1025 around the 512/1024 transfer boundary, and up to
1514. Exactly 504 and 1016 land on the boundary. 2048 burst frames,
`60 + (seq × 389) mod 1455` bytes, follow back to back from 11 s, overlapping the
start of the peer's RX exact phase. The peer's 4096-frame flood is sent after the
burst (`send --idle 17.5` from the receive window): on metal the OUT frame in flight
when the flood's aftermath reaches the chip starves for tens of seconds
(12.2–122.4 s measured at seq 623), so delivery must be provable before it lands. Python pins the same golden digests as the Rust tests.

## Checking

`scripts/ax88-tx-proof.py capture.log --manifest manifest.json --pcap peer.pcap` runs
NET6's whole RX check, with the census line prefixed `NET7`. On the TX side it
recomputes each exact record's length, transfer size and boundary flag, plus the
totals, byte count and boundary-frame count. It then requires every one of the 2080
frames in the capture exactly once, byte-exact to the schedule. Frames under 60 bytes
must arrive padded to exactly 60; their padding bytes are reported, not required.
Missing, repeated, altered or unexpected frames fail. A missing capture fails too.
Capture on the peer with
`tcpdump -i en0 -s 0 -U -w peer.pcap 'udp port 50606 or ether proto 0x88b5'`.

## Metal result (J835, keyboard + ax88, LAN switch, peer = Mac en0)

Private image `KUMO_AX88_TX_PROOF=1 KUMO_CONSOLE_LOGLEVEL=6` (served initrd SHA-256
`de6bbbe32598259c60c939a42d4cb256073c44f330c2fec880e034e57b052f97`). One clean
boot: all 32 exact frames and all 2048 burst frames completed exactly
(submitted=completed=2080, errors=0, 1636190 payload bytes, 4 max-packet-boundary
frames), the worst completion latency was 243 ms, and the census was flat
(lanes 10/10, custody 8/8, faults 0) — `NET7 census ... PASS`. The keyboard stayed
live and the peer's 155 RX aggregates (269 frames) verified.

The transfer-624 wall is intermittent: four earlier boots stalled the OUT in flight
at seq 623 (~490 KB handed over) for 12.2, 32.5, 122.4 and 122.2 s with
`ep_state=Running`, in one case with no peer traffic in flight; the completion always
arrived later and was counted. A completion absent for 120 s stops the schedule once
with the endpoint state, and one delayed over 1 s is recorded as `NET7 tx slow`.
J840 closed the delivery half: with the reference link_reset TX re-arm (RX_CTL
0000/0288 plus one 0x81/0x8c read) requested on every completion and run by a
per-wake pump in the driver's arm_idle, the peer's capture held all 2080 frames
byte-exact (exact 32/32, burst 2048/2048, 4 boundary frames) and the official
checker is green. The transfer-624 stall is the USB-side buffer filling, separate
from delivery; the re-arm needed a 32 KiB drv-xhci stack (PT_GNU_STACK via
build.rs), because 16 KiB overflowed on proof-path additions three times. Kick
cadence measured: tick-only 2027/2080, per-wake at 8 completions 2060/2080, at 1
2080/2080.
