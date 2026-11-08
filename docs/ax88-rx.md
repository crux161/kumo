# ax88 passive receive against a real peer (NET6)
<!-- j834 -->

Date: 2026-11-07. Author: TIMBERDOODLE.

NET6 turns MAC receive on and proves what arrives. It runs NET5b's sequence with the
receive goal (`kumo_ax88::link::Goal::Receive`). On every link up, that goal first does
`link_reset()`'s RX control stop/start (`0b` 0000, then 0288). It then writes the
speed queue and the speed/duplex medium with `RECEIVE_EN` (bit 8). The window stays
open until the peer's END frame; 2 s later the sequence turns receive off (MEDIUM
0000) and parks. Every write is read back. Pause stays disabled in both directions,
the pause watermarks stay unwritten and EEE stays at the PHY's reset default; the
results below are measured under that policy. Only `KUMO_AX88_RX_PROOF=1` images run
this mode. Ordinary images still leave the adapter unconfigured.

## Test frames

`scripts/ax88-rx-proof.py send` runs on a peer host on the same switch. It needs no
root: it sends ordinary IPv4/UDP broadcasts to port 50606, bound to one interface.
`kumo_ax88::probe` defines the payload: `KUMONET6`, sequence (u32 LE), payload
length (u16 LE), version 1, 0, then `pattern(seq, i)` bytes. Every size and byte
follows from the sequence alone. Python pins the same golden digests as the Rust
tests.

| Phase | Sequences | Sizes (frame bytes) | Pace |
|---|---|---|---|
| idle | — | — | 10 s with receive on and no test traffic |
| exact | 0–63 | 60–75, 496–511, 1008–1023, 1499–1514 | 30 ms apart |
| burst | 64–4159 | 60–1514, `18 + (seq × 389) mod 1455` payload | back to back |
| end | 0xffffffff | 60 | five, 200 ms apart |

KUMO checks every test frame in software: the IPv4 header checksum, the UDP checksum,
the length fields and every payload byte. It also measures whether the frame's last
four bytes are the CRC-32 of the rest, i.e. whether the chip passes the FCS through.
Exact-phase frames are logged one per line, with the length, an FNV-1a 64 of the
whole frame and the 42 header bytes. Burst frames are only tallied: arrivals go into a
4096-bit bitmap and an order-independent digest, `Σ fnv64(seq ‖ fnv64(payload))`.
Other traffic is counted by EtherType. Per-aggregate logging is suppressed, so a
burst cannot flood the console. The RX lane remains NET4c-3's single 32 KiB transfer,
re-armed after each completion. The tally reads a CPU snapshot of the completed DMA
bytes, never the buffer itself while it is armed.

## Checking

`scripts/ax88-rx-proof.py check capture.log --manifest manifest.json [--pcap peer.pcap]`
recomputes everything from the sequence numbers:

- the receive-goal init and its write values, from the logged PHY registers;
- each exact frame, rebuilt from its logged header plus the scheduled payload (and an
  FCS when present). The rebuilt frame's checksums must be valid, its hash must equal
  KUMO's, and its addresses and ports must match the sender manifest;
- the burst digest, from the logged bitmap;
- that every frame falls inside the receive window, that no record is unknown or
  duplicated, and that at least one aggregate carried several frames;
- a flat census.

With `--pcap` (the peer's own `tcpdump -s 0` of port 50606, pcap or pcapng), every
captured exact frame must equal what KUMO received. The only tolerated difference is
a sender-side checksum-offload placeholder, and only where the capture's checksum is
invalid. The capture must also hold every sent sequence. Loss in the burst is
reported, not hidden.

## Metal result (J834, keyboard + ax88, LAN switch, peer = Mac en0)

The private image `KUMO_AX88_RX_PROOF=1 KUMO_CONSOLE_LOGLEVEL=6` (served initrd
SHA-256 `1d4c6ca3431fae089e9744841a043d4924e05ed4cf01bc14339a0439919ac510`) ran on two
separate boots. The peer was the Mac's wired en0, sending from 192.168.0.106 to
192.168.0.255. CRUX ran `sudo tcpdump -s 0` on en0 during the second boot.

- The init was exact on both boots. The link came up 2.3–3.0 s after PHY release at
  1000/full, matching the Annex 28B resolution. RX control 0000 then 0288, queue
  `07 20 03 16 ff` and MEDIUM `010b` (receive on) all read back exactly. Receive was
  turned off and the chip parked after the END frames.
- All 64 exact frames arrived on both boots, every one with valid IPv4 and UDP
  checksums and payload. On boot 2 each matches the peer's capture byte for byte,
  except the IPv4 and UDP checksum fields: the Mac's sender-side capture holds
  offload placeholders (IP 0000, UDP pseudo-header partial), while the wire and KUMO
  carry the finished values. The capture holds all 4160 sent frames.
- The chip removes the FCS: none of the 373 and 355 test frames ended in its
  CRC-32. Aggregates have no dummy entries; a lone 60-byte frame is 72 bytes.
- The burst sent 4096 frames, 3.22 MB in 0.31 s (about 83 Mb/s). KUMO received and
  verified 286 and 304 of them; the burst digest from the bitmap matches the schedule
  exactly. Up to 25 frames shared one aggregate, and the largest aggregate was 20704
  bytes, under the 26 KiB queue limit. The chip marked 94 and 115 entries as dropped
  in its metadata. Loss is 93%, at a rate far below USB 2's bandwidth. The limit is
  the single 32 KiB transfer, which the resident loop re-arms only after servicing
  each completion. That is the measured bound, not a corruption; more outstanding RX
  transfers are a later throughput slice.
- There were no parse errors, invalid, duplicate or unexpected frames, and 0 router
  faults. The census was flat: lanes 9/9, custody 8/8, empty EP0 ring. Background
  LAN traffic in the window was 3–7 ARP, 10–13 other IPv4 and 0–2 other frames.
- The keyboard reported 183–189 times during the 20 s windows (100 ms idle rate).
  Both NVMe files were read and unmounted on both boots. The boot-window costs match
  NET4c-3/NET5b: busy max about 299 ms, HID gap up to 1.7 s. These are wall-clock
  numbers.

Not claimed: unicast-to-node-ID or multicast filtering (NET8 and later); TX; pause,
EEE or flow-control behaviour under load; throughput beyond this single-transfer
bound; SuperSpeed; another host; or the reason the chip dropped each entry, since
CRC errors and FIFO drops are counted together.
— TIMBERDOODLE 2026-11-07
