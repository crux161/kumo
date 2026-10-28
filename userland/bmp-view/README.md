<!-- j648 -->
# bmp-view

At the KUMO shell, run `bmp-view` to display the bundled tinybmp sample in an
available 256×128 graphics view. The separate process decodes on the CPU, scales
with nearest-neighbor sampling, preserves aspect ratio, centers on black, and
waits for the presenter's copy acknowledgement before exiting.

This first program embeds `assets/sample.bmp`; replace that file and rebuild to
show another BMP. It accepts uncompressed RGB and bitfield BMPs supported by
tinybmp; file arguments and RLE compression are not supported. It receives only
its presentation channel and surface VMO. The existing presenter consumes a view
grant on process exit, so repeated launches require another available view or a
reboot. Serial-only boots report no graphics view.

Build with `cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64`
(or the existing X13s/Orange Pi hardware names). Verify via
`cargo test --offline -p bmp-view` and `python3 scripts/graphics-smoke.py --bitmap`.
The latter compares every view pixel against an independent BMP decoder through
a disposable UEFI QEMU boot and checks that the console still reaches login.

Vendored sources and sample provenance: [tinybmp](../../vendor/tinybmp/KUMO.md).
