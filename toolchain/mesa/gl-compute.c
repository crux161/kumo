/* SPDX-License-Identifier: MIT */
/*j770*/
/* GL2-a: runtime GLSL ES compilation, dispatch and SSBO readback through public
 * GL entry points. QEMU checks the original poison, never fabricated GPU output.
 * The standalone client checks shader results on Mali. — KESTREL */
#include "kumo_gl.h"
#include "kumo_kmod.h"
#include "pipe/p_screen.h"
#include <GLES3/gl31.h>
#include <stdarg.h>
#include <stdio.h>

struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);
unsigned kumo_gl_compute_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled);

#define WORDS 64u
#define GUARDS 16u
#define POISON UINT32_C(0xdeadbeef)
#define EXPECTED(i) (UINT32_C(0x6b750000) ^ ((i) * UINT32_C(0x9e3779b9)))
#define REQUIRE(test, code) do { if (!(test)) { failed = (code); goto done; } } while (0)

static void report(void (*say)(const char *), const char *format, ...)
{
    char line[240];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof line, format, args);
    va_end(args);
    say(line);
}

unsigned kumo_gl_compute_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled)
{
    struct pan_kmod_dev *kmod = pan_kmod_kumo_dev_create(api, NULL);
    if (!kmod)
        return 1;
    struct pipe_screen *screen = panfrost_create_screen_kumo(kmod);
    if (!screen)
        return 2;
    struct kumo_gl_display *display = kumo_gl_display_create(screen);
    if (!display) {
        screen->destroy(screen);
        return 3;
    }
    unsigned failed = 0;
    GLuint shader = 0, program = 0, buffer = 0;
    struct kumo_gl_context *context = kumo_gl_context_create(display);
    struct kumo_gl_drawable *drawable = NULL;
    REQUIRE(context, 4);
    drawable = kumo_gl_drawable_create(display, 64, 48, NULL, NULL);
    REQUIRE(drawable && kumo_gl_make_current(context, drawable), 5);
    GLint major = 0, minor = 0;
    glGetIntegerv(GL_MAJOR_VERSION, &major);
    glGetIntegerv(GL_MINOR_VERSION, &minor);
    REQUIRE((major > 3 || (major == 3 && minor >= 1)) && glGetError() == GL_NO_ERROR, 6);
    say("GL2-a context: current GLES 3.1-capable context over the native GPU session");

    const GLchar *source =
        "#version 310 es\n"
        "layout(local_size_x = 16) in;\n"
        "layout(std430, binding = 0) buffer Output { uint values[]; };\n"
        "void main() { uint i = gl_GlobalInvocationID.x;\n"
        "  values[i] = 0x6b750000u ^ (i * 0x9e3779b9u); }\n";
    shader = glCreateShader(GL_COMPUTE_SHADER);
    REQUIRE(shader, 7);
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint compiled = GL_FALSE;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &compiled);
    if (!compiled) {
        char log[200] = {0};
        glGetShaderInfoLog(shader, sizeof log, NULL, log);
        report(say, "GL2-a shader log: %s", log);
    }
    REQUIRE(compiled && glGetError() == GL_NO_ERROR, 8);
    program = glCreateProgram();
    REQUIRE(program, 9);
    glAttachShader(program, shader);
    glLinkProgram(program);
    GLint linked = GL_FALSE;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[200] = {0};
        glGetProgramInfoLog(program, sizeof log, NULL, log);
        report(say, "GL2-a program log: %s", log);
    }
    REQUIRE(linked && glGetError() == GL_NO_ERROR, 10);
    glUseProgram(program);
    say("GL2-a compiler: GLSL ES 310 compute source compiled and linked at runtime");

    uint32_t initial[WORDS + GUARDS];
    for (unsigned i = 0; i < WORDS + GUARDS; ++i)
        initial[i] = POISON;
    glGenBuffers(1, &buffer);
    REQUIRE(buffer, 11);
    glBindBuffer(GL_SHADER_STORAGE_BUFFER, buffer);
    glBufferData(GL_SHADER_STORAGE_BUFFER, sizeof initial, initial, GL_STATIC_DRAW);
    glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, buffer);
    REQUIRE(glGetError() == GL_NO_ERROR, 12);
    say("GL2-a buffer: 64 output words and 16 guard words initialized to 0xdeadbeef");
    glDispatchCompute(4, 1, 1);
    glMemoryBarrier(GL_SHADER_STORAGE_BARRIER_BIT | GL_BUFFER_UPDATE_BARRIER_BIT);
    glFinish();
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 13);
    say("GL2-a dispatch: four groups of sixteen invocations, memory barrier and native glFinish complete");

    const uint32_t *mapped = glMapBufferRange(GL_SHADER_STORAGE_BUFFER, 0, sizeof initial, GL_MAP_READ_BIT);
    REQUIRE(mapped && glGetError() == GL_NO_ERROR, 14);
    unsigned differ = 0, damaged = 0;
    for (unsigned i = 0; i < WORDS; ++i)
        differ += mapped[i] != (modeled ? POISON : EXPECTED(i));
    for (unsigned i = WORDS; i < WORDS + GUARDS; ++i)
        damaged += mapped[i] != POISON;
    report(say, "GL2-a readback: %u of 64 output words differ; %u of 16 guards changed (%s)",
           differ, damaged, modeled ? "modeled service, poison retained" : "Mali shader results");
    GLboolean unmapped = glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
    REQUIRE(unmapped && glGetError() == GL_NO_ERROR && !differ && !damaged, 15);
done:
    if (context && kumo_gl_is_current(context)) {
        glUseProgram(0);
        glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, 0);
        glBindBuffer(GL_SHADER_STORAGE_BUFFER, 0);
        if (buffer) glDeleteBuffers(1, &buffer);
        if (program) glDeleteProgram(program);
        if (shader) glDeleteShader(shader);
        if (!failed && (glGetError() != GL_NO_ERROR || api->device_status(api->context) != KUMO_GPU_OK))
            failed = 16;
    }
    if (context)
        kumo_gl_context_destroy(context);
    if (drawable && !kumo_gl_drawable_destroy(drawable))
        failed = failed ? failed : 17;
    if (!kumo_gl_display_destroy(display))
        failed = failed ? failed : 18;
    if (!failed)
        say("GL2-a destroy: shader, program, SSBO, context, drawable and display released");
    return failed;
}

#ifdef KUMO_GL_COMPUTE_MAIN
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_gl_compute_cases(api, kumo_metal_log, false);
}

int main(void)
{
    kumo_metal_log("GL2-a metal compute begin: runtime GLSL ES compiler and native GLES SSBO dispatch");
    unsigned failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "GL2-a metal compute FAILED at case %u", failed);
        return 1;
    }
    kumo_metal_log("GL2-a metal compute green: runtime-compiled GLES compute wrote all 64 SSBO words, guards intact, native completion and clean teardown");
    return 0;
}
#endif
