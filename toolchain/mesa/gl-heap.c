/* SPDX-License-Identifier: MIT */
/* GL3-c (PLAN/015 §8.32, D3): G7q-d under Gallium's real tiler. Before the screen exists,
 * proof-only driconf overrides shrink Gallium's tiler heap to one 256 KiB chunk with a
 * maximum of two; Mesa's defaults and KUMO's patch are unchanged. One instanced draw of a
 * fine cell grid then outgrows the heap: the first TILER_OOM is served by drv-gpu growing
 * the heap to two chunks (G7q-b), and every later one is refused at max_chunks, so
 * Gallium's tiler-OOM exception handler renders incrementally. The image must still be
 * exact.
 *
 * Every layer covers every pixel exactly once: 1x2-pixel cells split on a diagonal that no
 * pixel center lies on. RGB replaces (the last layer's color wins, so ordering across
 * incremental passes shows), and alpha adds a fixed step per layer (so a lost or repeated
 * pass shows as a wrong sum). QEMU's modeled producer runs no GPU and retains the initial
 * image; its mode proves the overrides reach the native heap request. */
#include "kumo_gl.h"
#include "kumo_kmod.h"
#include "pipe/p_screen.h"
#include <GLES3/gl3.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* POSIX, declared here so the proof does not depend on a feature-test macro. */
int setenv(const char *name, const char *value, int overwrite);
int unsetenv(const char *name);

struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);
unsigned kumo_gl_heap_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled);

#define WIDTH 256u
#define HEIGHT 128u
#define PIXELS (WIDTH * HEIGHT)
#define GUARDS 16u
#define GUARD UINT32_C(0xa5a5a5a5)
/* The grid: 256 columns x 64 rows of 1x2-pixel cells, (COLS + 1) x (ROWS + 1) vertices. */
#define COLS 256u
#define ROWS 64u
#define CELL_INDICES 6u
#define INDICES (COLS * ROWS * CELL_INDICES)
#define TRIANGLES (COLS * ROWS * 2u)
/* Layers are sized on metal (§8.32): 16 x 32768 tiny triangles is meant to outgrow a 512 KiB
 * heap several times over. Override with -DKUMO_GL_HEAP_LAYERS. */
#ifndef KUMO_GL_HEAP_LAYERS
#define KUMO_GL_HEAP_LAYERS 16
#endif
#define LAYERS ((unsigned)KUMO_GL_HEAP_LAYERS)
_Static_assert(KUMO_GL_HEAP_LAYERS >= 1 && KUMO_GL_HEAP_LAYERS <= 255, "one to 255 layers");
/* Each layer's blue and alpha step; the last layer's blue and the alpha sum are both
 * STEP * LAYERS, which stays within 8 bits. */
#define STEP (255u / LAYERS)
/* Proof-only driconf overrides, read at screen creation (§8.32: driconf takes environment
 * overrides). The chunk size is Panthor's 256 KiB, in bytes. */
#define CHUNK_BYTES "262144"
#define INITIAL_CHUNKS "1"
#define MAX_CHUNKS "2"
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

/* The expected readback word at window pixel (x, y), rows bottom-up: R = x, G = 2y, B = the
 * last layer's STEP * LAYERS, and alpha the sum of every layer's STEP. */
static uint32_t expected(unsigned x, unsigned y)
{
    uint32_t sum = STEP * LAYERS;
    return (sum << 24) | (sum << 16) | ((2u * y) << 8) | x;
}

static const char *layout(uint64_t modifier)
{
    if (modifier == 0)
        return "linear";
    if (modifier >> 56 != 0x08)
        return "non-ARM";
    switch ((modifier >> 52) & 0xf) {
    case 0x0: return "AFBC";
    case 0x1: return (modifier & 0xfffffffffffffull) == 1 ? "u-interleaved" : "ARM misc";
    case 0x2: return "AFRC";
    default: return "ARM other";
    }
}

static bool modifier_of(struct pipe_screen *screen, struct pipe_resource *resource, uint64_t *value)
{
    return screen->resource_get_param &&
           screen->resource_get_param(screen, NULL, resource, 0, 0, 0,
                                      PIPE_RESOURCE_PARAM_MODIFIER, 0, value);
}

static GLuint compile(void (*say)(const char *), GLenum stage, const GLchar *source)
{
    GLuint shader = glCreateShader(stage);
    if (!shader)
        return 0;
    glShaderSource(shader, 1, &source, NULL);
    glCompileShader(shader);
    GLint compiled = GL_FALSE;
    glGetShaderiv(shader, GL_COMPILE_STATUS, &compiled);
    if (!compiled) {
        char log[200] = {0};
        glGetShaderInfoLog(shader, sizeof log, NULL, log);
        report(say, "GL3-c shader log: %s", log);
        glDeleteShader(shader);
        return 0;
    }
    return shader;
}

unsigned kumo_gl_heap_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled)
{
    /* Before screen creation, which is where Gallium reads its tiler heap shape. */
    if (setenv("pan_csf_chunk_size", CHUNK_BYTES, 1) || setenv("pan_csf_initial_chunks", INITIAL_CHUNKS, 1) ||
        setenv("pan_csf_max_chunks", MAX_CHUNKS, 1))
        return 1;
    say("GL3-c heap config: pan_csf_chunk_size=" CHUNK_BYTES " pan_csf_initial_chunks=" INITIAL_CHUNKS
        " pan_csf_max_chunks=" MAX_CHUNKS " set before screen creation (proof-only overrides)");
    struct pan_kmod_dev *kmod = pan_kmod_kumo_dev_create(api, NULL);
    struct pipe_screen *screen = kmod ? panfrost_create_screen_kumo(kmod) : NULL;
    /* The heap shape is read; nothing later in this process may inherit the overrides. */
    unsetenv("pan_csf_chunk_size");
    unsetenv("pan_csf_initial_chunks");
    unsetenv("pan_csf_max_chunks");
    if (!kmod)
        return 2;
    if (!screen)
        return 3;
    struct kumo_gl_display *display = kumo_gl_display_create(screen);
    if (!display) {
        screen->destroy(screen);
        return 4;
    }
    unsigned failed = 0;
    GLuint vertex = 0, fragment = 0, program = 0, buffer = 0;
    uint32_t *initial = NULL, *output = NULL;
    uint16_t *indices = NULL;
    struct kumo_gl_context *context = kumo_gl_context_create(display);
    struct kumo_gl_drawable *drawable = NULL;
    REQUIRE(context, 5);
    drawable = kumo_gl_drawable_create(display, WIDTH, HEIGHT, NULL, NULL);
    REQUIRE(drawable && kumo_gl_make_current(context, drawable), 6);
    say("GL3-c context: current GLES context over the native GPU session");

    struct pipe_resource *color = kumo_gl_drawable_resource(drawable, false);
    uint64_t before = 0;
    REQUIRE(color && modifier_of(screen, color, &before), 7);
    REQUIRE(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE &&
            glGetError() == GL_NO_ERROR, 8);
    report(say, "GL3-c target: complete owned 256x128 RGBA8 frontend framebuffer, color modifier 0x%016llx (%s)",
           (unsigned long long)before, layout(before));

    /* Vertex i of the (COLS + 1)-wide grid sits at pixel (i % 257, 2 * (i / 257)); every
     * coordinate is exact in float. The layer is the instance. */
    vertex = compile(say, GL_VERTEX_SHADER,
                     "#version 300 es\n"
                     "flat out int layer;\n"
                     "void main() {\n"
                     "    int column = gl_VertexID % 257;\n"
                     "    int row = gl_VertexID / 257;\n"
                     "    layer = gl_InstanceID;\n"
                     "    gl_Position = vec4(float(column) / 128.0 - 1.0, float(row) / 32.0 - 1.0, 0.0, 1.0);\n"
                     "}\n");
    REQUIRE(vertex && glGetError() == GL_NO_ERROR, 9);
    /* gl_FragCoord is the pixel center, so its integer part is the window pixel. */
    char source[640];
    snprintf(source, sizeof source,
             "#version 300 es\n"
             "precision highp float;\n"
             "precision highp int;\n"
             "flat in int layer;\n"
             "out vec4 fragment;\n"
             "void main() {\n"
             "    int x = int(gl_FragCoord.x);\n"
             "    int y = int(gl_FragCoord.y);\n"
             "    fragment = vec4(float(x) / 255.0, float(2 * y) / 255.0,\n"
             "                    float(%u * (layer + 1)) / 255.0, %u.0 / 255.0);\n"
             "}\n", STEP, STEP);
    fragment = compile(say, GL_FRAGMENT_SHADER, source);
    REQUIRE(fragment && glGetError() == GL_NO_ERROR, 10);
    program = glCreateProgram();
    REQUIRE(program, 11);
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = GL_FALSE;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[200] = {0};
        glGetProgramInfoLog(program, sizeof log, NULL, log);
        report(say, "GL3-c program log: %s", log);
    }
    REQUIRE(linked && glGetError() == GL_NO_ERROR, 12);
    say("GL3-c compiler: GLSL ES 300 vertex and fragment sources compiled and linked at runtime");

    /* Cell (c, r) is two triangles split on the diagonal from (c, 2r) to (c + 1, 2r + 2):
     * the line y = 2r + 2(x - c) meets the column's centers at y = 2r + 1, between the two
     * pixel centers, so each center lies strictly inside one triangle. */
    indices = malloc(INDICES * sizeof *indices);
    REQUIRE(indices, 13);
    for (unsigned r = 0, i = 0; r < ROWS; ++r)
        for (unsigned c = 0; c < COLS; ++c) {
            uint16_t base = (uint16_t)(r * (COLS + 1) + c);
            const uint16_t cell[CELL_INDICES] = {base, (uint16_t)(base + 1), (uint16_t)(base + COLS + 2),
                                                 base, (uint16_t)(base + COLS + 2), (uint16_t)(base + COLS + 1)};
            memcpy(indices + i, cell, sizeof cell);
            i += CELL_INDICES;
        }
    glGenBuffers(1, &buffer);
    REQUIRE(buffer, 14);
    glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, buffer);
    glBufferData(GL_ELEMENT_ARRAY_BUFFER, INDICES * sizeof *indices, indices, GL_STATIC_DRAW);
    REQUIRE(glGetError() == GL_NO_ERROR, 15);

    initial = malloc(PIXELS * sizeof *initial);
    output = malloc((GUARDS + PIXELS + GUARDS) * sizeof *output);
    REQUIRE(initial && output, 16);
    for (unsigned i = 0; i < PIXELS; ++i)
        initial[i] = UINT32_C(0x3c3c3c3c);
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, initial);
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 17);
    unsigned initially_different = 0;
    for (unsigned y = 0; y < HEIGHT; ++y)
        for (unsigned x = 0; x < WIDTH; ++x)
            initially_different += initial[y * WIDTH + x] != expected(x, y);
    REQUIRE(initially_different, 18);
    say("GL3-c baseline: public glReadPixels observed an image different from the expected one");

    glViewport(0, 0, WIDTH, HEIGHT);
    glDisable(GL_DITHER);
    glDisable(GL_SCISSOR_TEST);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);
    glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
    /* RGB replaces; alpha accumulates from a zero clear. */
    glEnable(GL_BLEND);
    glBlendEquation(GL_FUNC_ADD);
    glBlendFuncSeparate(GL_ONE, GL_ZERO, GL_ONE, GL_ONE);
    glClearColor(0.2f, 0.4f, 0.6f, 0.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(program);
    glDrawElementsInstanced(GL_TRIANGLES, INDICES, GL_UNSIGNED_SHORT, (const void *)0, LAYERS);
    glFinish();
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 19);
    report(say, "GL3-c draw: glClear and one glDrawElementsInstanced of %u layers x %u triangles (1x2-pixel cells), "
           "native glFinish complete", LAYERS, TRIANGLES);

    for (unsigned i = 0; i < GUARDS + PIXELS + GUARDS; ++i)
        output[i] = GUARD;
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, output + GUARDS);
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 20);
    const uint32_t *pixels = output + GUARDS;
    unsigned differ = 0, damaged = 0;
    for (unsigned y = 0; y < HEIGHT; ++y)
        for (unsigned x = 0; x < WIDTH; ++x) {
            unsigned i = y * WIDTH + x;
            differ += pixels[i] != (modeled ? initial[i] : expected(x, y));
        }
    for (unsigned i = 0; i < GUARDS; ++i) {
        damaged += output[i] != GUARD;
        damaged += output[GUARDS + PIXELS + i] != GUARD;
    }
    if (modeled)
        report(say, "GL3-c readback: %u of %u pixels differ; %u of 32 readback guards changed "
               "(modeled service, initial framebuffer retained)", differ, PIXELS, damaged);
    else
        report(say, "GL3-c readback: %u of %u pixels differ; %u of 32 readback guards changed "
               "(Mali: last layer's RGB, alpha summed over all %u layers)", differ, PIXELS, damaged, LAYERS);
    report(say, "GL3-c samples: (0,0)=0x%08x (255,0)=0x%08x (0,127)=0x%08x (255,127)=0x%08x",
           pixels[0], pixels[WIDTH - 1], pixels[(HEIGHT - 1) * WIDTH], pixels[PIXELS - 1]);
    REQUIRE(!differ && !damaged, 21);
done:
    if (context && kumo_gl_is_current(context)) {
        glUseProgram(0);
        glDisable(GL_BLEND);
        glBindBuffer(GL_ELEMENT_ARRAY_BUFFER, 0);
        if (buffer) glDeleteBuffers(1, &buffer);
        if (program) glDeleteProgram(program);
        if (fragment) glDeleteShader(fragment);
        if (vertex) glDeleteShader(vertex);
        if (!failed && (glGetError() != GL_NO_ERROR || api->device_status(api->context) != KUMO_GPU_OK))
            failed = 22;
    }
    free(indices);
    free(output);
    free(initial);
    if (context)
        kumo_gl_context_destroy(context);
    if (drawable && !kumo_gl_drawable_destroy(drawable))
        failed = failed ? failed : 23;
    if (!kumo_gl_display_destroy(display))
        failed = failed ? failed : 24;
    if (!failed)
        say("GL3-c destroy: shaders, program, index buffer, context, drawable and display released");
    return failed;
}

#ifdef KUMO_GL_HEAP_MAIN
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_gl_heap_cases(api, kumo_metal_log, false);
}

int main(void)
{
    kumo_metal_log("GL3-c metal heap begin: one 256 KiB tiler chunk, max two, under an instanced draw that outgrows it");
    unsigned failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "GL3-c metal heap FAILED at case %u", failed);
        return 1;
    }
    /* The client sees only its image; mesa-device-metal.py --check proves the growth and the
     * refusals from drv-gpu's own lines. */
    kumo_metal_log("GL3-c metal heap green: the instanced draw's image is exact on a one-chunk, max-two tiler heap, "
                   "guards intact and clean teardown");
    return 0;
}
#endif
