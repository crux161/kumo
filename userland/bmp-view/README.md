<!-- j648 -->
<!-- j649 -->
# bmp-view

Run `bmp-view [path]` in KUMO. With no path it displays the built-in sample.
For example:

```text
bmp-view /share/images/sample.bmp
cd /share/images
bmp-view sample.bmp
bmp-view /tmp/IMAGE.BMP
bmp-view /media/<volume>/IMAGE.BMP
```

Paths name files in KUMO's initrd, scratch filesystem, or a mounted removable volume.
Relative paths use the shell's current directory. A path on the host computer must
first be included in the initrd or copied onto media mounted by KUMO. Every AArch64
image includes `/share/images/sample.bmp` as a file for the first example.

The launcher copies just the selected file into a read-only VMO before taking a view;
the viewer receives no initrd or filesystem authority. Files must be nonempty and at
most 16 MiB. Missing/inaccessible paths, empty/oversized files and read failures leave
the view available. Invalid BMP data or unsupported compression are reported by the
child after launch, consuming that view under the existing presenter contract.

The separate process decodes on the CPU using tinybmp, scales with nearest-neighbor
sampling, preserves aspect ratio, centers on black in a 256×128 view, and waits for the
copy acknowledgement before exiting. Uncompressed RGB and bitfield BMPs are supported;
Standard 32-bit BGRA bitfield images are displayed opaquely, ignoring alpha.
RLE is not supported. The presenter consumes a view grant on process exit, so repeated
successful launches require another available view or a reboot. Serial-only boots
report no graphics view.

Build with `cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64`
(or the existing X13s/Orange Pi hardware names). Verify with:

```sh
cargo test --offline -p bmp-view
python3 scripts/graphics-smoke.py --bitmap
python3 scripts/graphics-smoke.py --bitmap-path
python3 scripts/graphics-smoke.py --bitmap-path /tmp/INPUT.BMP
python3 scripts/graphics-smoke.py --bitmap-file /host/path/image.bmp
```

The smoke boots a disposable image, checks all 32768 view pixels against an independent
BMP reference, and checks refusal paths, chrome, the second view, console and login.
`--bitmap-file` injects the host file only into that disposable test image.

Vendored sources and sample provenance: [tinybmp](../../vendor/tinybmp/KUMO.md).
