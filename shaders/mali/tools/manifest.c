//j592
/*
 * SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * Runs inside the build container (scripts/build-mali-shaders.sh), compiled with Mesa's own
 * flags against Mesa 26.2.5's headers. It reads the kumo_paint blob panfrost_compile emitted,
 * writes the raw Valhall binary, prints the manifest drv-gpu's descriptors are built from, and
 * packs Mesa's reference SHADER_PROGRAM, LOCAL_STORAGE and COMPUTE_SIZE_WORKGROUP descriptors
 * the way panvk does for a precompiled kernel, so drv-gpu's packers can be pinned to them byte
 * for byte. It also writes the Valhall disassembly. — TIMBERDOODLE 2026-10-24
 *
 * Usage: manifest <binary out> <disassembly out>   (manifest text on stdout)
 */
#define PAN_ARCH 10
#include <stddef.h>
#include <stdio.h>
#include "genxml/gen_macros.h"
#include "panfrost/compiler/pan_compiler.h"
#include "panfrost/compiler/bifrost/bifrost_compile.h"
#include "panfrost/compiler/bifrost/valhall/disassemble.h"
#include "panfrost/lib/pan_shader.h"

#include "kumo_paint_v10.c"

/* drv-gpu maps the binary at this GPU VA in G5c-2; the reference SPD carries it. */
#define REFERENCE_BINARY_VA 0x10040000ull

static void print_hex(const char *key, const void *bytes, size_t len)
{
   printf("%s = ", key);
   for (size_t i = 0; i < len; i++)
      printf("%02x", ((const uint8_t *)bytes)[i]);
   printf("\n");
}

int main(int argc, char **argv)
{
   if (argc != 3) {
      fprintf(stderr, "usage: %s <binary out> <disassembly out>\n", argv[0]);
      return 2;
   }
   const struct bifrost_precompiled_kernel_info *k =
      (const void *)kumo_shaders_default_v10[KUMO_PAINT];
   const struct pan_shader_info *info = &k->info;
   const uint8_t *binary = (const uint8_t *)k + sizeof(*k);

   FILE *out = fopen(argv[1], "wb");
   if (!out || fwrite(binary, 1, k->binary_size, out) != k->binary_size)
      return 1;
   fclose(out);
   FILE *dis = fopen(argv[2], "w");
   if (!dis)
      return 1;
   disassemble_valhall(dis, binary, k->binary_size, false);
   fclose(dis);

   printf("kernel = kumo_paint\n");
   printf("arch = 10\n");
   printf("binary_bytes = %u\n", k->binary_size);
   printf("local_size = %u %u %u\n", k->local_size_x, k->local_size_y, k->local_size_z);
   printf("stage_compute = %d\n", info->stage == MESA_SHADER_COMPUTE);
   printf("work_reg_count = %u\n", info->work_reg_count);
   printf("preload = 0x%016llx\n", (unsigned long long)info->preload);
   printf("tls_size = %u\n", info->tls_size);
   printf("wls_size = %u\n", info->wls_size);
   printf("fau_reserved_words = %u\n", info->fau.reserved);
   printf("allow_merging_workgroups = %d\n", info->cs.allow_merging_workgroups);
   printf("ftz_fp16 = %d\n", info->ftz_fp16);
   printf("ftz_fp32 = %d\n", info->ftz_fp32);
   printf("ubo_count = %u\n", info->ubo_count);
   printf("texture_count = %u\n", info->texture_count);
   printf("sampler_count = %u\n", info->sampler_count);
   printf("sysvals_bytes = %zu\n", (size_t)BIFROST_PRECOMPILED_KERNEL_SYSVALS_SIZE);
   printf("args_offset = %zu\n", (size_t)BIFROST_PRECOMPILED_KERNEL_ARGS_OFFSET);
   printf("arg_out = %zu %zu\n", offsetof(struct kumo_paint_args, out),
          sizeof(((struct kumo_paint_args *)0)->out));
   printf("arg_stride = %zu %zu\n", offsetof(struct kumo_paint_args, stride),
          sizeof(((struct kumo_paint_args *)0)->stride));
   printf("args_bytes = %zu\n", sizeof(struct kumo_paint_args));
   printf("reference_binary_va = 0x%llx\n", REFERENCE_BINARY_VA);

   /* panvk_shader_upload's compute SHADER_PROGRAM, with shader_ftz_mode's choice. */
   struct mali_shader_program_packed spd;
   pan_pack(&spd, SHADER_PROGRAM, cfg) {
      cfg.stage = pan_shader_stage(info);
      cfg.register_allocation = pan_register_allocation(info->work_reg_count);
      cfg.binary = REFERENCE_BINARY_VA;
      cfg.preload.r48_r63 = info->preload >> 48;
      cfg.flush_to_zero_mode = info->ftz_fp32 ? (info->ftz_fp16 ? MALI_FLUSH_TO_ZERO_MODE_ALWAYS
                                                                : MALI_FLUSH_TO_ZERO_MODE_DX11)
                                              : MALI_FLUSH_TO_ZERO_MODE_PRESERVE_SUBNORMALS;
   }
   print_hex("spd_v10", &spd, sizeof(spd));

   /* pan_emit_tls for a kernel with no TLS and no WLS. */
   struct mali_local_storage_packed tsd;
   pan_pack(&tsd, LOCAL_STORAGE, cfg) {
      cfg.wls_instances = MALI_LOCAL_STORAGE_NO_WORKGROUP_MEM;
   }
   print_hex("tsd_v10", &tsd, sizeof(tsd));

   /* panvk_vX_cmd_precomp.c's COMPUTE_SIZE_WORKGROUP. */
   struct mali_compute_size_workgroup_packed wg;
   pan_pack(&wg, COMPUTE_SIZE_WORKGROUP, cfg) {
      cfg.workgroup_size_x = k->local_size_x;
      cfg.workgroup_size_y = k->local_size_y;
      cfg.workgroup_size_z = k->local_size_z;
      cfg.allow_merging_workgroups = info->cs.allow_merging_workgroups;
   }
   print_hex("wg_size_v10", &wg, sizeof(wg));
   return 0;
}
