# ax88 read-only identity proof
<!-- j832 -->

Date: 2026-11-07. Author: TIMBERDOODLE.

NET5a reads a finite register set only after exact NET0 descriptors and the NET3
profile match. `kumo-ax88::identity` supplies the native request list and conservative
PHY decoding; drv-xhci owns EP0 on the existing controller and custodied page. No
new DMA, capability, controller process, Ethernet endpoint or frame channel is added.

| Field | bRequest | wValue | wIndex | Length |
|---|---|---|---|---|
| node ID |1|0x10|6|6|
| USB link |1|0x02|1|1|
| silicon status |1|0x03|1|1|
| PHY power/reset |1|0x26|2|2|
| clock |1|0x33|1|1|
| current PHY page |2|3|31|2|
| PHY ID1 |2|3|2|2|
| PHY ID2 |2|3|3|2|
| PHY basic status |2|3|1|2|
| PHY physical status |2|3|17|2|

Every request is vendor/device IN (`bmRequestType=0xc0`). The request list has no
OUT, page selection, power/clock write, EEPROM/eFuse access or reload. The firmware
reference transcribes the same literal requests independently of the native codec;
it uses only a unique complete cached AX88179/bcdDevice0100 USB2 interface.
Firmware cached descriptors are reconstructed first; the NET0 no-transfer label
refers to that descriptor pass. The subsequent NET5a pass explicitly issues reads.

The native path uses `ControlPipe` with the existing200ms deadline and iteration
ceiling, validates data and status completions and exact byte counts, copies only
the returned span, and stops after any uncertain request with EP0/DMA quarantined.
No retry or partial data is promoted to success. Identity mode suppresses both
NET4a's reversible write proof and NET4c-3's endpoint setup, even if their flags
are also present. An uncertain request cannot be recovered by clearing software
state; a later lifecycle proof is still needed. — TIMBERDOODLE

The independent reader uses firmware's USB_IO control method,200ms timeout and
IN direction. The ABI's DataLength is an input size; there is no output actual
length. Thus firmware status/value evidence is not an exact-byte-count proof.
This follows [UEFI2.11 section17.2.6](https://uefi.org/specs/UEFI/2.11/17_Protocols_USB_Support.html#efi-usb-io-protocol-usbcontroltransfer).
KUMO's completed Data/Status events supply that proof. Host ABI tests check request
size/offsets, protocol slot offsets, argument order/widths, bounds and missing method.
Only named requests are callable; bulk/interrupt/reset methods remain opaque.

Source facts come from the supplied `resources/net/ax88179_178a.c`: NODE_ID0x10,
USB status SS/HS bits2/1, GENERAL_STATUS bit2 UA1/UA2, PHY address3, PHYSR0x11
link bit10, speed bits15:14 and duplex bit13. No vendor implementation is imported.
The current PHY page is read, never selected implicitly. A nonzero page remains
unknown; zero/all-ones IDs or invalid status are unavailable. Reserved speed bits
remain unknown. Reported register bits are not a peer-negotiation proof.

## Reproduce and validate

```sh
KUMO_AX88_ORACLE=1 KUMO_AX88_IDENTITY_PROOF=1 KUMO_CONSOLE_LOGLEVEL=6 \
  cargo xtask image --arch aarch64 --hardware orange-pi-5-plus
python3 scripts/ax88-identity-proof.py --self-test
python3 scripts/ax88-identity-proof.py capture.log
```

The checker requires complete unchanged NET0 descriptor bytes in both paths,
ordered ten-field reads, literal IN setup packets, successful firmware statuses,
exact native lengths and counter deltas, a valid unicast MAC and matching stable
MAC/silicon identity. Available page-zero PHY IDs must agree. Dynamic link/power
samples are reported separately, rather than required to remain constant across
firmware handoff and USB reset. It rejects missing/duplicate/reordered, wrong
request/status/length/MAC/counters, mutating proof records and TOWER/ABEND evidence.
Fixtures are not hardware evidence.

On both separate keyboard+ax88 and USB-stick+ax88 cold boots, both paths reported node MAC `1a:2b:3c:4d:29:04`,
USB high speed (`02`), silicon `06` (UA2), power/reset `8000`, clock `03`, and
page/PHY IDs/basic/physical status all zero. KUMO completed ten exact reads,
20 routed/consumed events, ten retirements, zero faults and an empty EP0 ring.
Both proofs preserved actual NVMe file contents; the physical keyboard echo and
USB J549.TXT read/unmount passed separately. The PHY is reported unavailable because
its IDs are zero; no cable-down/speed
claim is made. The locally administered node MAC is the currently observed RAM
address, not a factory/EEPROM identity or a promised persistent address.

NET5b follows with a separately documented volatile initialization sequence and
readbacks. PHY readiness, link up/down, RX/TX, packets, confinement and portability
to another host remain later gates. Ordinary images keep the existing inventory
and leave the adapter unconfigured. — TIMBERDOODLE 2026-11-07
