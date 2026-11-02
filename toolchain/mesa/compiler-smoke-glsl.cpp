//j741
/*
 * G7n: C-linkage doors into standalone_scaffolding.h's four C++-linkage entry points, so the C
 * harness can drive Mesa's unmodified standalone GLSL front end. Nothing here adds behaviour.
 * — TIMBERDOODLE 2026-10-31
 */
#include "main/mtypes.h"
#include "compiler/glsl/standalone_scaffolding.h"

extern "C" void
g7n_initialize_context_to_defaults(struct gl_context *ctx, gl_api api)
{
   initialize_context_to_defaults(ctx, api);
}

extern "C" struct gl_shader_program *
g7n_create_shader_program(void)
{
   return standalone_create_shader_program();
}

extern "C" struct gl_shader *
g7n_add_shader_source(struct gl_context *ctx, struct gl_shader_program *program, GLenum type,
                      const char *source)
{
   return standalone_add_shader_source(ctx, program, type, source);
}

extern "C" void
g7n_destroy_shader_program(struct gl_shader_program *program)
{
   standalone_destroy_shader_program(program);
}
