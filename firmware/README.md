# Vendored firmware

`mali_csffw.bin` is the Arm Mali-G610 CSF firmware: the image the GPU's microcontroller runs
once G3 boots it. It is KUMO's first proprietary binary, vendored by CRUX's decision (PLAN/013
decision 1, 2026-10-24). It is redistributed under the Arm End User Licence Agreement in
`LICENCE.mali_csffw`; that licence must stay beside the blob, because clause 1.2 requires a copy
of it to accompany any redistribution.

- Source: linux-firmware, `arm/mali/arch10.8/mali_csffw.bin`
  (<https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git>).
- Licence: `LICENSES/LICENCE.mali_csffw` from the same tree, copied verbatim here.
- `mali_csffw.bin`: 282624 bytes,
  sha256 `a27847ea11f8efb3136340c3ba8aab413ae25145eeb4a7f64ff5edd829a2405b`.
- `LICENCE.mali_csffw`: 173 lines,
  sha256 `ebedc86d1767186a66dcb59ce8dcb97c5fd9ac10de2adaacad226714e4712f6d`.
- Header facts (read by the G3a parser): magic `0xC3F13A6E`, minor 3, version hash `0x01050000`,
  entry table `0x318` bytes; the embedded build metadata says
  `git_sha: 95a25d71030715381f33105394285e1dcc860a65`.

`xtask image` packs the blob into the initrd at `lib/firmware/mali_csffw.bin`. `drv-gpu` parses
it with the G3a parser (`Firmware::parse`, which is host-tested against this exact image); G3b
loads its sections into address space 0 and starts the MCU.

The arch10.8 image under `resources/mali-g610/panthor-linux6.1/firmware/` is a different build
(sha256 `43c3c36b…`, version hash `0x01010000`); it is not vendored.
