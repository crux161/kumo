//j592
/*
 * SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * kumo_paint: PLAN/013 G5c's compute kernel. It paints the same 256x128 XRGB8888 test pattern
 * as drv-gpu's command-stream painter (`paint_pixel`, J590): gfx-demo's red, green, blue and
 * white 64-pixel bars on the top half, and below them a ramp with blue following x and green
 * following twice the row. One invocation per pixel, 16x16 workgroups.
 *
 * Built offline by Mesa's compilers (mesa_clc, then panfrost_compile for arch 10, Valhall CSF);
 * `scripts/build-mali-shaders.sh` is the recipe and `kumo_paint.v10.manifest` records what came
 * out. The kernel uses no private arrays, so it needs no thread-local storage.
 * — TIMBERDOODLE 2026-10-24
 */
#include "compiler/libcl/libcl.h"

static uint32_t kumo_pixel(uint32_t x, uint32_t y)
{
   if (y < 64) {
      return x < 64 ? 0x00FF0000u : x < 128 ? 0x0000FF00u : x < 192 ? 0x000000FFu : 0x00FFFFFFu;
   }
   return (((y - 64u) * 2u) << 8) | x;
}

KERNEL2D(16, 16)
kumo_paint(global uint32_t *out, uint32_t stride)
{
   uint32_t x = cl_global_id.x;
   uint32_t y = cl_global_id.y;
   out[y * stride + x] = kumo_pixel(x, y);
}
