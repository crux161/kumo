# USB arcade stick: raw input
<!-- j896 -->

The first arcade-stick slice collects native USB HID reports from **1a34:0203**,
the device selected by the Python reference in `kumo-joy/driver.py`. The transport
runs in the existing `drv-xhci` owner; `kumo-hid::arcade` holds the pure matcher and
capture logic. This slice provides raw diagnostics. Application events and a
button/axis mapping are a follow-up.

Connect the stick before booting the Orange Pi 5 Plus. It can share an already
supported hub with boot input devices. The driver reads complete device and
configuration descriptors, selects one alternate-zero nonboot HID interrupt-IN
interface, and retrieves the HID report descriptor. Ambiguous interfaces/endpoints,
oversized packets/descriptors, and incomplete transfers are refused. The arcade
interface stays in report protocol; it receives no boot `SET_PROTOCOL` request.

The tested stick advertises endpoint `0x81`, 64-byte maximum packets, interval 10,
and a 157-byte report descriptor. Actual input packets are **27 bytes**. The
descriptor defines 12 buttons, a hat and four axes in its first seven bytes,
followed by 20 vendor-defined bytes. No Report ID is declared. Its vendor byte 19
alternates even at rest, so full-packet change detection exhausts a log quickly.
Only an exact match to the captured descriptor enables comparison of the first
seven bytes. Every selected sample still prints the entire raw packet; an unknown
descriptor uses full-packet comparison. No physical button labels are inferred.

Output includes `JOY device`, descriptor chunks, and `JOY raw` records with slot,
interface, sequence, actual length and hexadecimal bytes. Identical control states
are suppressed, and output stops after 128 changed reports per boot. The endpoint
continues receiving through its existing IRQ/completion ownership after that limit.
Raw reports never enter the shell keyboard or mouse decoder.

To reproduce the physical capture:

```sh
KUMO_CONSOLE_LOGLEVEL=6 cargo xtask image --arch aarch64 --hardware orange-pi-5-plus
python3 scripts/metal-joy.py --work build/verification/joy/capture --seconds 180
```

When the harness prints `JOY READY`, move the stick through its directions, press
and release buttons, and leave it neutral. Creating `<work>/finish` in the chosen
work directory ends the input window early. The harness requires three distinct
control states plus a return, saves the report descriptor and raw samples, and
checks thermal/SMART service replies during the test boot. It uses a
private PXE tree and one serial connection. Keep `dmesg` out of level-6 autoexec.

Pure host coverage is available with `cargo test -p kumo-hid -p drv-xhci`. The
driver's 32 KiB ELF stack request is confined to its bare-metal binary so these
tests also link on macOS.

HID protocol reference: [USB HID 1.11](https://www.usb.org/sites/default/files/hid1_11.pdf),
descriptor items and interface requests. The packet geometry and vendor-field
behavior above come from this stick's captured descriptor and real reports.
