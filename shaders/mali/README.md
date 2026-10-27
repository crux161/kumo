# Vendored Mali compute kernels
<!-- j592 -->
<!-- j596 -->
<!-- j637 -->

KUMO's own GPU kernels for the Mali-G610 (Valhall CSF, Mesa arch 10), compiled offline by Mesa
and vendored by CRUX's decision (PLAN/013 decision 2, option A, 2026-10-24): the kernel source,
the compiled binary, a manifest of what the compiler reported, and the recipe that rebuilds them.
No Mesa source is in the tree.

| File | What it is |
| --- | --- |
| `kumo_paint.cl` | KUMO's kernel source (OpenCL C, MIT OR Apache-2.0): the G5a test pattern, one invocation per pixel. |
| `kumo_paint.v10.bin` | The Valhall binary for arch 10, as `panfrost_compile` emitted it. |
| `kumo_paint.v10.manifest` | Generated: pins, hashes, the compiler's report, Mesa's reference descriptors. |
| `kumo_paint.v10.dis` | Generated: Mesa's Valhall disassembly of the binary, for review. |
| `kumo_triangle.cl` | J0b's compute rasterizer: fixed triangle vertices with red/green/blue barycentric interpolation and a dark background. |
| `kumo_triangle.v10.bin` | The separately compiled 640-byte Valhall triangle kernel; paint remains byte-identical. |
| `kumo_triangle.v10.manifest` | Pinned compiler metadata, source/binary hashes and Mesa reference descriptors for the triangle. |
| `kumo_triangle.v10.dis` | Generated Valhall triangle disassembly. |
| `tools/manifest.c` | KUMO's manifest generator (runs in the build container against Mesa's headers). |
| `LICENCE.mesa` | Mesa's MIT notice; it must stay beside the binary. |

## Rebuilding

```sh
./scripts/build-mali-shaders.sh            # rebuild into build/mali-shaders/, compare
./scripts/build-mali-shaders.sh --update   # rebuild, then replace the vendored files
```

The recipe needs Docker (OrbStack on CRUX's Mac). It pins the container image by digest,
Debian's packages by a snapshot.debian.org timestamp, and Mesa's source by its tarball's
SHA-256, then checks the toolchain versions it got (all recorded in the manifest). It builds only
Mesa's `mesa_clc` and `panfrost_compile`, compiles both kernels with Mesa's own CL flags,
and runs `tools/manifest.c`. The recipe normalizes trailing newlines in the new triangle disassembly;
instruction text is preserved. A rebuild must reproduce both binaries and their manifests/disassemblies bit for bit;
the script exits non-zero if any vendored file differs.

## How KUMO uses it

`lib/kumo-mali` embeds the binary with `include_bytes!` (`KUMO_PAINT_V10`) and carries the
manifest's fields and descriptor packers. Both `drv-gpu` and the separate `gpu-client` use this
shared library; the driver re-exports its original API. The driver's host tests parse this
manifest and check every field, and check the shader program, local storage and workgroup-size
descriptors against Mesa's `pan_pack`
output byte for byte (the `spd_v10`, `tsd_v10` and `wg_size_v10` lines, packed for the binary at
`reference_binary_va`). RESEARCH/009 §12.9 describes the push-data layout and the dispatch.

The `_at` upload/dispatch helpers also pack a caller's GPU addresses. Host tests verify that
relocation changes only the intended descriptor pointers and output argument, and that the
full FAU count survives instruction encoding. G6c's separate client queries the queue's
capabilities, uploads code, descriptors and commands through its shared VMO, dispatches the
kernel, and checks every pixel against the shared CPU reference before closing its session.

## Native triangle (J637)

`KUMO_TRIANGLE_V10` uses the same pinned 16x16 workgroup and output/stride argument ABI
as the painter, with its own executable bytes. Integer half-spaces sample pixel centers
for vertices (128,16), (32,112), (224,112), interpolating red at the top, green at the
bottom left and blue at the bottom right. The 256x128 image covers 9312 triangle pixels;
the remainder is `0x00101820`. The independent CPU edge-equation reference checks all
32768 GPU-produced pixels (FNV-1a `0xc3ccc90f`) without writing or copying them.

The client uploads both kernels before freezing GPU bindings: paint at GPU VA
`0x20002000`, triangle at `0x20002800`, with separate descriptor blocks at `0x20003000`
and `0x20003100`. No executable code is overwritten in a live GPU session. After point3
checks the old bars/ramp, point4 rasterizes the triangle into the original image VMO.
The existing J0a lease presents its pages at display IOVA `0x63000000`, then restores
the console and confirms display unmap before permitting reuse. This is compute
rasterization of one known triangle, not a vertex/fragment pipeline or Mesa runtime API.
Repeated native presentation remains the next J0 gate. — KESTREL 2026-10-26
