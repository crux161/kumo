//j637
/*
 * SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * J0b: one compute-rasterized RGB triangle in the 256x128 XRGB8888 image.
 * Vertices (128,16), (32,112), (224,112) are red, green and blue. Integer
 * barycentric half-spaces sample pixel centers; all weights sum to 192.
 * Outside is dark blue-grey. No CPU rasterization or texture upload. — KESTREL
 */
#include "compiler/libcl/libcl.h"

KERNEL2D(16, 16)
kumo_triangle(global uint32_t *out, uint32_t stride)
{
   int32_t x = cl_global_id.x;
   int32_t y = cl_global_id.y;
   int32_t red = 223 - 2 * y;
   int32_t green = y - x + 112;
   int32_t blue = y + x - 143;
   uint32_t pixel = 0x00101820u;
   if (red >= 0 && green >= 0 && blue >= 0) {
      uint32_t r = ((uint32_t)red * 255u + 96u) / 192u;
      uint32_t g = ((uint32_t)green * 255u + 96u) / 192u;
      uint32_t b = ((uint32_t)blue * 255u + 96u) / 192u;
      pixel = (r << 16) | (g << 8) | b;
   }
   out[(uint32_t)y * stride + (uint32_t)x] = pixel;
}
