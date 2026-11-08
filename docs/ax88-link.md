# ax88 volatile initialization and link control (NET5b)
<!-- j833 -->

Date: 2026-11-07. Author: TIMBERDOODLE.

NET5b brings the NET0-profiled AX88179 (bcdDevice 0100, high speed) from the state
firmware leaves to a powered PHY with a known MAC configuration. It then follows
cable up/down. `kumo-ax88::link` is the pure sequence, a state machine over an exact
vendor-request `Port`. drv-xhci adapts that port to the resident owner's
`ControlPipe` on EP0 and paces it with the existing 100 ms proof tick. The PHY and
clock settle waits are therefore timer ticks, not blocking sleeps. HID and storage
keep running beside them. Only `KUMO_AX88_LINK_PROOF=1` images run it. Ordinary
images still leave the adapter unconfigured.

## The sequence

Every value comes from the supplied `resources/net/ax88179_178a.c`: `reset()`,
`link_reset()`, `set_rx_mode()` and `unbind()`. Record bytes are raw little-endian
register bytes. Every write is followed by an exact readback.

| Step | Register | Value | Readback must match | Wait after |
|---|---|---|---|---|
| baseline | 26, 33, 22, 0b, 34, 35, 2e | read only | recorded | — |
| PHY reset asserted | 26 power/reset | 0000 | BZ/IPRL/AT bits (0x1030) | — |
| PHY reset released | 26 | 0020 (IPRL) | BZ/IPRL/AT bits | 500 ms |
| clocks | 33 clock | 03 (ACS\|BCS) | all bits | 200 ms |
| identify | PHY 31, 2, 3, 17, then 0, 1, 4, 9 | read only | page 0, valid IDs | poll ≤ 2 s |
| RX checksum offload off | 34 | 00 | all bits | — |
| TX checksum offload off | 35 | 00 | all bits | — |
| medium cleared | 22 | 0000 | all bits | — |
| RX mode | 0b | 0288 (START\|AB\|IPE) | all bits | — |

On every link up, the sequence reads BMSR, ANAR, ANLPAR, GBCR, GBSR and USB link
status. It then writes `link_reset()`'s bulk-in queue entry for that speed and USB
speed, and a medium value with only the speed and duplex bits:

| Link | USB | Queue (2e) | Medium (22) |
|---|---|---|---|
| 1000 | SuperSpeed | 07 4f 00 12 ff | GIGA\|EN_125MHZ (+FULL) |
| 1000 | high speed | 07 20 03 16 ff | GIGA\|EN_125MHZ (+FULL) |
| 1000 | slower | 07 cc 4c 18 08 | GIGA\|EN_125MHZ (+FULL) |
| 100 | SS or HS | 07 ae 07 18 ff | PS (+FULL) |
| 100 / 10 | other | 07 cc 4c 18 08 | PS or none (+FULL) |

Reserved PHYSR speed bits (11) refuse, where Linux would fall back to its 10 Mb/s
branch. Every entry's aggregate limit, 1024 × (size + 2), is at most 26 KiB, which
fits the 32 KiB resident RX transfer. Parking at the end follows `unbind()`: RX
control 0, clocks 0, PHY power/reset 0. One tick later the PHY must read unavailable
again.

KUMO policy narrows the reference. MAC receive stays off: MEDIUM bit 8 stays clear
until NET6, so RX stays pending. There are no pause enables, and the pause
watermarks are not written. Checksum offload is disabled (software checksums), as
are multicast, promiscuous and accept-physical filtering. CRC-error dropping is left
to the NET3 parser. The sequence writes nothing to the PHY: no page select, EEE
disable, LED setup or autonegotiation restart. The PHY negotiates from its reset
defaults. Auto-detach and the LED and MAC paths read EEPROM/eFuse, so they are
omitted. The node ID is left as NET5a observed it. No WoL monitor mode is set. The
`0x81/0x8c` TX FIFO check and the RX control toggle in `link_reset()` belong to
TX/RX work (NET6/NET7). The request constructors cannot express EEPROM, eFuse or
reload.

Link changes are polled from PHYSR bit 10 every tick. Each change must also be
reported by the 8-byte interrupt-IN status lane (bit 16) within 2 s. The first link
up after PHY release is measured from the release. The proof waits up to 600 s for
two unplug/replug cycles, then parks. Any transfer error or readback mismatch stops
the sequence at once with EP0 and the lanes retained. Speed and duplex are checked
against an independent IEEE 802.3 Annex 28B resolution of both advertisements.

## Metal result (J833, keyboard + ax88, LAN switch peer)

Private image `KUMO_AX88_LINK_PROOF=1 KUMO_CONSOLE_LOGLEVEL=6`. Served initrd SHA-256
`4550c94fc311553b1bc4dbfded4b1a89853d520e9095a074040d83a9ce735840`. Adapter at
fc400000/root1/route3/slot3, high speed, descriptors equal to NET0.

- Baseline left by firmware: power 8000, clock 03, MEDIUM 0033
  (GIGA|FULL|pause both ways, receive off), RX control 0218 (IPE|AM|AB, not started),
  queue `03 00 00 00 00`. The initialization replaced all of it.
- All 16 writes read back exactly. Bit 15 of power/reset did not survive the written
  0000, so the parked chip reads 0000 rather than the 8000 firmware left.
- Clock write 4.0 ms; PHY settle 583 ms, clock settle 245 ms; PHY ID `001cc915` at
  page 0 on the first attempt, 839 ms after release. Initialization took 860 ms.
- Advertisement ANAR 05e1 (10/100 both duplexes plus symmetric pause), GBCR 0200
  (1000 full only). Partner ANLPAR c1e1 (no pause), GBSR 3800 (partner 1000 full).
  The resolution is 1000/full and agrees with PHYSR `ad02` on all three link-ups.
  Queue `07 20 03 16 ff` and MEDIUM `000b` read back each time. USB link reads 42:
  high speed plus an unnamed bit 6.
- First link 2987 ms after PHY release. Then CRUX did two physical unplug/replug
  cycles: down, up after 20.7 s, down after 15.4 s, up after 18.1 s. The status
  endpoint confirmed all five transitions, 0–197 ms after the poll. It reported
  3759 times in 487 s, about once per 128 ms bInterval.
- Census: 16 writes, 3680 reads, 0 router faults, empty EP0 ring, lanes 9/9,
  custody 8/8, both lanes still pending, 0 RX completions with receive off.
  4807 HID reports. CRUX typed `echo net5b-keyboard` at the physical keyboard
  afterwards and it echoed.
- Boot-window costs, as in NET4c-3: slowest vendor transfer 147 ms, busy max 297 ms,
  tick lateness max 1348 ms, HID gap max 1157 ms. These are wall-clock numbers
  without CPU accounting.

The first proof boot failed closed on a PHYSR read during the HDMI re-lock. Its
completion arrived after the 200 ms EP0 deadline had already been judged. J833
corrects that judgement; see [Exact EP0 data transfers](xhci-ep0.md).

During both boots of the corrected image, the display driver's D1d holder-death
recovery check failed, and HDMI went dark after login. The machine stayed live on
serial. The earlier boot, whose NET5b stopped after 2 s, passed that check. This is
recorded as an open coexistence observation, not explained here; see the J833
journal for the ordinary-image boot.

## Reproduce and validate

```sh
KUMO_AX88_LINK_PROOF=1 KUMO_CONSOLE_LOGLEVEL=6 \
  cargo xtask image --arch aarch64 --hardware orange-pi-5-plus
python3 scripts/ax88-link-proof.py --self-test
python3 scripts/ax88-link-proof.py capture.log [--peer 1000/full]
```

The checker independently recomputes the request values, queue and medium tables,
autonegotiation resolution, transition alternation, cycle count and totals. It
requires literal record order, the named-bit readbacks, settle times of at least
500/200 ms, interrupt-endpoint agreement, parking and a flat census. It rejects
missing, duplicated or unknown records, NET4c-3 cycling, RX completions, TOWER and
ABEND. `--peer` adds the speed/duplex the link partner itself reports.

Not claimed: RX or TX frames (NET6/NET7), pause or EEE behaviour, SuperSpeed or
another host, the switch's own port reading (pending), DMA confinement, or a fix for
the display observation above. — TIMBERDOODLE 2026-11-07
