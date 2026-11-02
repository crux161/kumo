//j741
/*
 * G7n: Mesa's two shader compilers, run as ordinary code on KUMO.
 *
 * SPIR-V half. Upstream's own `panfrost_compile` (pan_compile.c, unmodified and linked in as
 * `pan_compile_main`) compiles kumo_paint's SPIR-V exactly as scripts/build-mali-shaders.sh runs
 * it on Linux: library `kumo_shaders`, arch 10, relative file names. The build redirects that
 * one translation unit's file calls (open/lseek/close/mmap/munmap/fopen) to the g7n_* layer
 * below, so its input is the embedded SPIR-V and its two outputs are open_memstream buffers, the
 * same on both platforms: the QEMU root volume refuses writes, and KUMO's relibc maps only
 * anonymous memory. The Linux reference also writes the outputs to a scratch directory, where
 * the build compares them with the shader recipe's.
 *
 * GLSL ES half. Mesa's standalone GLSL front end and NIR linker (unmodified) compile each GLSL
 * ES 3.10 compute shader from memory against a scaffolding context carrying panfrost's NIR
 * options for every stage, as panfrost's screen installs them. Then the state tracker's compute
 * steps for an SSBO-only program, then Gallium panfrost's compute sequence from
 * panfrost_shader_compile, ending in pan_shader_compile for the board's attested GPU_ID. No
 * device, screen or context is created; the uniform/sampler/image paths are GL2's.
 *
 * Every output is printed with its size and BLAKE3, so the same program built for Linux in the
 * offline container and for KUMO must print identical lines. — TIMBERDOODLE 2026-10-31
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#include "nir.h"
#include "main/mtypes.h"
#include "main/shaderobj.h"
#include "compiler/glsl/builtin_functions.h"
#include "compiler/glsl/gl_nir.h"
#include "compiler/glsl/gl_nir_linker.h"
#include "compiler/glsl/glsl_parser_extras.h"
#include "compiler/glsl/linker_util.h"
#include "panfrost/compiler/bifrost/bifrost_compile.h"
#include "panfrost/compiler/pan_compiler.h"
#include "pan_context.h"
#include "util/mesa-blake3.h"
#include "util/ralloc.h"
#include "util/u_dynarray.h"

#include "g7n-inputs.h"

/* standalone_scaffolding.h is C++-only and gives these four entry points C++ linkage;
 * compiler-smoke-glsl.cpp forwards to them unchanged. */
void g7n_initialize_context_to_defaults(struct gl_context *ctx, gl_api api);
struct gl_shader_program *g7n_create_shader_program(void);
struct gl_shader *g7n_add_shader_source(struct gl_context *ctx, struct gl_shader_program *program,
                                        GLenum type, const char *source);
void g7n_destroy_shader_program(struct gl_shader_program *program);

int pan_compile_main(int argc, const char **argv);

#define G7N_ARCH 10
/* GPU_ID and variant as the Orange Pi's Mali-G610 reports them (G7a/G7g attestations). */
#define G7N_GPU_ID 0xa8670005ull
#define G7N_GPU_VARIANT 0u

static int failures;

static void
fail(const char *what)
{
   printf("G7n compiler smoke FAILED: %s (errno=%d)\n", what, errno);
   failures++;
}

static void
report(const char *kind, const char *name, const void *data, size_t size, const char *extra)
{
   blake3_hash hash;
   char hex[BLAKE3_HEX_LEN];
   _mesa_blake3_compute(data, size, hash);
   _mesa_blake3_format(hex, hash);
   printf("G7n %s %s: %zu bytes blake3 %s%s\n", kind, name, size, hex, extra ? extra : "");
}

/* pan_compile.c's file layer. One input name opens the embedded SPIR-V read-only; mapping it
 * returns the embedded bytes, which MAP_PRIVATE + PROT_READ may share. The two output names
 * open memory streams; fclose leaves their final buffers here. Anything else refuses. */
#define G7N_SPIRV_FD 0x6b75
#define G7N_OUTPUTS 2

static const char *const g7n_output_names[G7N_OUTPUTS] = {"kumo_paint_v10.h", "kumo_paint_v10.c"};
static char *g7n_output_data[G7N_OUTPUTS];
static size_t g7n_output_size[G7N_OUTPUTS];

int
g7n_open(const char *path, int flags, ...)
{
   if (strcmp(path, "kumo_paint_v10.spv") == 0 && (flags & O_ACCMODE) == O_RDONLY)
      return G7N_SPIRV_FD;
   errno = ENOENT;
   return -1;
}

off_t
g7n_lseek(int fd, off_t offset, int whence)
{
   if (fd != G7N_SPIRV_FD) {
      errno = EBADF;
      return -1;
   }
   off_t base = whence == SEEK_END ? (off_t)sizeof(g7n_kumo_paint_spv) : 0;
   if ((whence != SEEK_SET && whence != SEEK_END) || base + offset < 0) {
      errno = EINVAL;
      return -1;
   }
   return base + offset;
}

int
g7n_close(int fd)
{
   if (fd != G7N_SPIRV_FD) {
      errno = EBADF;
      return -1;
   }
   return 0;
}

void *
g7n_file_mmap(void *addr, size_t len, int prot, int flags, int fd, off_t off)
{
   if (addr || prot != PROT_READ || flags != MAP_PRIVATE || off != 0 || fd != G7N_SPIRV_FD ||
       len != sizeof(g7n_kumo_paint_spv)) {
      errno = EINVAL;
      return MAP_FAILED;
   }
   return (void *)g7n_kumo_paint_spv;
}

int
g7n_file_munmap(void *addr, size_t len)
{
   if (addr != (const void *)g7n_kumo_paint_spv || len != sizeof(g7n_kumo_paint_spv)) {
      errno = EINVAL;
      return -1;
   }
   return 0;
}

FILE *
g7n_fopen(const char *path, const char *mode)
{
   for (unsigned i = 0; i < G7N_OUTPUTS; i++) {
      if (strcmp(path, g7n_output_names[i]) == 0 && strcmp(mode, "w") == 0 &&
          !g7n_output_data[i])
         return open_memstream(&g7n_output_data[i], &g7n_output_size[i]);
   }
   errno = EACCES;
   return NULL;
}

static bool
write_file(const char *path, const void *data, size_t size)
{
   FILE *f = fopen(path, "wb");
   if (!f)
      return false;
   bool ok = fwrite(data, 1, size, f) == size;
   return fclose(f) == 0 && ok;
}

static void
spirv_half(bool mirror)
{
   const char *argv[] = {"panfrost_compile", "kumo_shaders", "10", "kumo_paint_v10.spv",
                         "kumo_paint_v10.h", "kumo_paint_v10.c", NULL};
   if (pan_compile_main(6, argv) != 0) {
      fail("panfrost_compile");
      return;
   }
   for (unsigned i = 0; i < G7N_OUTPUTS; i++) {
      if (!g7n_output_data[i] || g7n_output_size[i] == 0) {
         fail(g7n_output_names[i]);
         continue;
      }
      report("spirv", g7n_output_names[i], g7n_output_data[i], g7n_output_size[i], NULL);
      if (mirror && !write_file(g7n_output_names[i], g7n_output_data[i], g7n_output_size[i]))
         fail("mirror output");
      free(g7n_output_data[i]);
   }
}

/* The scaffolding context standalone.cpp builds, with panfrost's options as panfrost's screen
 * installs them (pan_screen.c: every stage through compute, merge_wg false). */
static void
init_context(struct gl_context *ctx)
{
   g7n_initialize_context_to_defaults(ctx, API_OPENGLES2);
   _mesa_glsl_builtin_functions_init_or_ref();
   ctx->Version = 31;
   for (unsigned i = 0; i <= MESA_SHADER_COMPUTE; i++)
      ctx->screen->nir_options[i] = pan_get_nir_shader_compiler_options(G7N_ARCH, i, false);
   ctx->Const.GLSLVersion = 310;
   ctx->Extensions.ARB_ES3_compatibility = true;
   ctx->Extensions.ARB_ES3_1_compatibility = true;
   ctx->Const.MaxComputeWorkGroupCount[0] = 65535;
   ctx->Const.MaxComputeWorkGroupCount[1] = 65535;
   ctx->Const.MaxComputeWorkGroupCount[2] = 65535;
   ctx->Const.MaxComputeWorkGroupSize[0] = 1024;
   ctx->Const.MaxComputeWorkGroupSize[1] = 1024;
   ctx->Const.MaxComputeWorkGroupSize[2] = 64;
   ctx->Const.MaxComputeWorkGroupInvocations = 1024;
   ctx->Const.MaxComputeSharedMemorySize = 32768;
   ctx->Const.Program[MESA_SHADER_COMPUTE].MaxUniformComponents = 1024;
   ctx->Const.Program[MESA_SHADER_COMPUTE].MaxCombinedUniformComponents = 1024;
   /* standalone.cpp's limits stop at GLSL ES 3.00 (it has no 310 case, so its own tool would
    * refuse these shaders); its 300 values plus its generic tail follow. Limits gate front-end
    * validation only, never code generation. */
   ctx->Const.MaxClipPlanes = 8;
   ctx->Const.MaxCombinedTextureImageUnits = 32;
   ctx->Const.MaxDrawBuffers = 4;
   ctx->Const.MinProgramTexelOffset = -8;
   ctx->Const.MaxProgramTexelOffset = 7;
   ctx->Const.MaxUniformBufferBindings = 84;
   ctx->Const.MaxVertexStreams = 4;
   ctx->Const.MaxTransformFeedbackBuffers = 4;
   ctx->Const.GenerateTemporaryNames = true;
   ctx->Const.MaxPatchVertices = 32;
   ctx->Const.MaxUserAssignableUniformLocations = 4 * MESA_SHADER_MESH_STAGES * MAX_UNIFORMS;
   /* GLSL ES 3.10's storage-buffer minimums (OpenGL ES 3.1 spec, table 20.47). */
   ctx->Const.Program[MESA_SHADER_COMPUTE].MaxShaderStorageBlocks = 4;
   ctx->Const.MaxShaderStorageBufferBindings = 4;
   ctx->Const.MaxCombinedShaderStorageBlocks = 4;
   ctx->Const.MaxShaderStorageBlockSize = 1 << 27;
}

static unsigned
uniform_vec4_slots(const struct glsl_type *type, bool bindless)
{
   return glsl_count_vec4_slots(type, false, bindless);
}

static void
glsl_half(struct gl_context *ctx, const char *name, const char *source)
{
   struct gl_shader_program *prog = g7n_create_shader_program();
   struct gl_shader *shader = g7n_add_shader_source(ctx, prog, GL_COMPUTE_SHADER, source);
   _mesa_glsl_compile_shader(ctx, shader, NULL, false, false, true);
   if (!shader->CompileStatus) {
      printf("G7n %s info log:\n%s\n", name, shader->InfoLog);
      fail("GLSL compile");
      return;
   }
   _mesa_clear_shader_program_data(ctx, prog);
   prog->data->LinkStatus = LINKING_SUCCESS;
   link_shaders_init(ctx, prog);
   gl_nir_link_glsl(ctx, prog);
   struct gl_linked_shader *linked = prog->_LinkedShaders[MESA_SHADER_COMPUTE];
   if (!prog->data->LinkStatus || !linked || !linked->Program->nir) {
      printf("G7n %s link log:\n%s\n", name, prog->data->InfoLog);
      fail("GLSL link");
      return;
   }
   nir_shader *nir = linked->Program->nir;

   /* st_link_glsl_to_nir and st_glsl_to_nir_post_opts, compute and SSBO-only. */
   nir_shader_gather_info(nir, nir_shader_get_entrypoint(nir));
   NIR_PASS(_, nir, nir_remove_dead_variables, nir_var_shader_in | nir_var_shader_out, NULL);
   NIR_PASS(_, nir, gl_nir_lower_buffers, prog);
   struct nir_lower_compute_system_values_options cs_options = {0};
   NIR_PASS(_, nir, nir_lower_system_values);
   NIR_PASS(_, nir, nir_lower_compute_system_values, &cs_options);
   NIR_PASS(_, nir, nir_lower_io, nir_var_uniform, uniform_vec4_slots, (nir_lower_io_options)0);
   if (nir->options->lower_uniforms_to_ubo)
      NIR_PASS(_, nir, nir_lower_uniforms_to_ubo, ctx->Const.PackedDriverUniformStorage,
               !ctx->Const.NativeIntegers);

   /* panfrost_shader_compile's compute path. */
   nir_shader *s = nir_shader_clone(NULL, nir);
   pan_preprocess_nir(s, G7N_GPU_ID);
   struct pan_compile_inputs inputs = {
      .gpu_id = G7N_GPU_ID,
      .gpu_variant = G7N_GPU_VARIANT,
   };
   inputs.trust_varying_flat_highp_types = false;
   inputs.varying_layout = NULL;
   NIR_PASS(_, s, panfrost_nir_lower_res_indices, inputs.gpu_id);
   struct pan_shader_info info = {0};
   pan_postprocess_nir(s, &inputs, &info);
   info.cs.allow_merging_workgroups = valhall_can_merge_workgroups(s);
   struct panfrost_sysvals sysvals = {0};
   NIR_PASS(_, s, panfrost_nir_lower_sysvals, G7N_ARCH, &sysvals);
   if (!s->info.first_ubo_is_default_ubo) {
      fail("default UBO not first");
      ralloc_free(s);
      return;
   }
   inputs.fau.pushable_ubos = BITFIELD_BIT(0);
   if (sysvals.sysval_count != 0)
      inputs.fau.pushable_ubos |= BITFIELD_BIT(PAN_UBO_SYSVALS);
   inputs.fau.promote_immediates = true;
   inputs.robust_descriptors = true;

   struct util_dynarray binary = UTIL_DYNARRAY_INIT;
   pan_shader_compile(s, &inputs, &binary, &info);
   if (binary.size == 0) {
      fail("pan_shader_compile produced no code");
   } else {
      char extra[96];
      snprintf(extra, sizeof(extra), "; work regs %u, tls %u, wls %u, sysvals %u",
               info.work_reg_count, info.tls_size, info.wls_size, sysvals.sysval_count);
      report("glsl", name, binary.data, binary.size, extra);
   }
   util_dynarray_fini(&binary);
   ralloc_free(s);
   g7n_destroy_shader_program(prog);
}

int
main(int argc, char **argv)
{
   /* The Linux reference names a scratch directory and mirrors panfrost_compile's two outputs
    * there; on KUMO the arguments come from the shell and nothing touches a filesystem. */
   bool mirror = argc > 1 && argv[1][0] == '/';
   if (mirror && chdir(argv[1]) != 0) {
      fail("chdir");
      return 1;
   }
   puts("G7n compiler smoke begin");
   glsl_type_singleton_init_or_ref();
   spirv_half(mirror);

   struct gl_context *ctx = calloc(1, sizeof(*ctx));
   if (!ctx) {
      fail("context allocation");
      return 1;
   }
   init_context(ctx);
   glsl_half(ctx, "paint.comp", g7n_paint_comp);
   glsl_half(ctx, "math.comp", g7n_math_comp);
   _mesa_glsl_builtin_functions_decref();
   free(ctx->screen);
   free(ctx);
   glsl_type_singleton_decref();

   if (failures) {
      printf("G7n compiler smoke FAILED: %d failures\n", failures);
      return 1;
   }
   puts("G7n compiler smoke green: panfrost_compile kumo_paint SPIR-V, GLSL ES paint.comp, GLSL ES math.comp");
   return 0;
}
