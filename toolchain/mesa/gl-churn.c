/* SPDX-License-Identifier: MIT */
/* GL4 (PLAN/015 §8.32/§8.2): G8's metal gate through public GLES. Each of 100 frames
 * creates a 256x160 RGBA8 texture (160 KiB, GL4-a's size), uploads a pattern unique to the
 * frame, draws with it (a viewport-covering triangle sampling it by texelFetch), reads the
 * 256x160 framebuffer back and checks every pixel, then deletes the texture: 16,000 KiB
 * of texture storage in all. Each frame's bottom 256x128 is copied into view 1 and
 * presented; G8's bar is 100 Retired, 0 Copied, and the census flat across sessions.
 *
 * Retired needs drv-fb's present gate, which arms only after the boot session's J0c
 * diagnostic. Per CRUX (2026-11-03) J0c is GL-fed: three GL frames copied into one VMO and
 * presented through the unchanged native lease protocol. Regranted sessions carry no
 * native producer and skip it, as G8's churn lane does. QEMU runs the same body with no
 * presenter; its modeled producer executes no GPU, so every frame must keep the observed
 * initial image while the texture lifecycle runs through Gallium and the session. With no GPU
 * executing, a modeled readback returns whatever a recycled staging BO holds (measured: the
 * frame's own upload, flipped), so QEMU checks only the readback guards; Mali checks every
 * pixel. — TIMBERDOODLE (RE4) */
#include "kumo_gl.h"
#include "kumo_kmod.h"
#include "pipe/p_screen.h"
#include <GLES3/gl3.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);

/* Callbacks into the metal runtime; NULL on QEMU, where no view exists. */
struct kumo_gl_churn_presenter {
    bool native;
    unsigned (*j0c)(const uint32_t *pixels, uint32_t width, uint32_t height, uint32_t sequence);
    unsigned (*ready)(void);
    unsigned (*frame)(const uint32_t *pixels, uint32_t width, uint32_t height);
};
unsigned kumo_gl_churn_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled,
                             const struct kumo_gl_churn_presenter *presenter);

#define WIDTH 256u
#define HEIGHT 160u
#define PIXELS (WIDTH * HEIGHT)
#define FRAMES 100u
#define J0C_FRAMES 3u
/* J0c's patterns are tagged 0xe1..0xe3, apart from the churn frames' 1..100. */
#define J0C_TAG 0xe0u
#define GUARDS 16u
#define GUARD UINT32_C(0xa5a5a5a5)
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

/* Texel (x, y) of the frame tagged `tag`, as packed RGBA8: R = x, G = y, B = tag, A = 0xff.
 * The draw copies texel (x, y) to window pixel (x, y), so it is also the readback word. */
static uint32_t pattern(unsigned tag, unsigned x, unsigned y)
{
    return UINT32_C(0xff000000) | ((uint32_t)(tag & 0xff) << 16) | ((uint32_t)y << 8) | x;
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
        report(say, "GL4 shader log: %s", log);
        glDeleteShader(shader);
        return 0;
    }
    return shader;
}

/* One frame: create, upload, draw with and delete a 160 KiB texture, then read every pixel
 * back between guards. Returns 0, or a nonzero failure code. `differ` gets the mismatches
 * against the pattern on Mali; a modeled run compares guards only. */
static unsigned frame(unsigned tag, uint32_t *texels, uint32_t *output, bool modeled,
                      unsigned *differ)
{
    for (unsigned y = 0; y < HEIGHT; ++y)
        for (unsigned x = 0; x < WIDTH; ++x)
            texels[y * WIDTH + x] = pattern(tag, x, y);
    GLuint texture = 0;
    glGenTextures(1, &texture);
    if (!texture)
        return 1;
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, WIDTH, HEIGHT, 0, GL_RGBA, GL_UNSIGNED_BYTE, texels);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    for (unsigned i = 0; i < GUARDS + PIXELS + GUARDS; ++i)
        output[i] = GUARD;
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, output + GUARDS);
    glBindTexture(GL_TEXTURE_2D, 0);
    glDeleteTextures(1, &texture);
    if (glGetError() != GL_NO_ERROR)
        return 2;
    unsigned wrong = 0, damaged = 0;
    for (unsigned y = 0; y < HEIGHT; ++y)
        for (unsigned x = 0; x < WIDTH; ++x) {
            unsigned i = y * WIDTH + x;
            wrong += !modeled && output[GUARDS + i] != pattern(tag, x, y);
        }
    for (unsigned i = 0; i < GUARDS; ++i) {
        damaged += output[i] != GUARD;
        damaged += output[GUARDS + PIXELS + i] != GUARD;
    }
    *differ += wrong;
    return damaged ? 3 : 0;
}

unsigned kumo_gl_churn_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled,
                             const struct kumo_gl_churn_presenter *presenter)
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
    unsigned failed = 0, code = 0;
    GLuint vertex = 0, fragment = 0, program = 0;
    uint32_t *initial = NULL, *output = NULL, *texels = NULL;
    struct kumo_gl_context *context = kumo_gl_context_create(display);
    struct kumo_gl_drawable *drawable = NULL;
    REQUIRE(context, 4);
    drawable = kumo_gl_drawable_create(display, WIDTH, HEIGHT, NULL, NULL);
    REQUIRE(drawable && kumo_gl_make_current(context, drawable), 5);
    REQUIRE(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE && glGetError() == GL_NO_ERROR, 6);
    say("GL4 context: current GLES context and an owned 256x160 RGBA8 framebuffer over the native GPU session");

    /* One triangle covering the viewport, placed by gl_VertexID; no vertex buffer. */
    vertex = compile(say, GL_VERTEX_SHADER,
                     "#version 300 es\n"
                     "void main() {\n"
                     "    vec2 corner = vec2(float((gl_VertexID & 1) * 4) - 1.0, float((gl_VertexID & 2) * 2) - 1.0);\n"
                     "    gl_Position = vec4(corner, 0.0, 1.0);\n"
                     "}\n");
    REQUIRE(vertex && glGetError() == GL_NO_ERROR, 7);
    fragment = compile(say, GL_FRAGMENT_SHADER,
                       "#version 300 es\n"
                       "precision highp float;\n"
                       "uniform highp sampler2D storage;\n"
                       "out vec4 fragment;\n"
                       "void main() { fragment = texelFetch(storage, ivec2(gl_FragCoord.xy), 0); }\n");
    REQUIRE(fragment && glGetError() == GL_NO_ERROR, 8);
    program = glCreateProgram();
    REQUIRE(program, 9);
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = GL_FALSE;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    REQUIRE(linked && glGetError() == GL_NO_ERROR, 10);
    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "storage"), 0);
    glActiveTexture(GL_TEXTURE0);
    glViewport(0, 0, WIDTH, HEIGHT);
    glDisable(GL_DITHER);
    glDisable(GL_SCISSOR_TEST);
    glDisable(GL_BLEND);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);
    REQUIRE(glGetError() == GL_NO_ERROR, 11);
    say("GL4 compiler: GLSL ES 300 sources compiled and linked at runtime; texelFetch copies texel to pixel");

    initial = malloc(PIXELS * sizeof *initial);
    output = malloc((GUARDS + PIXELS + GUARDS) * sizeof *output);
    texels = malloc(PIXELS * sizeof *texels);
    REQUIRE(initial && output && texels, 12);
    for (unsigned i = 0; i < PIXELS; ++i)
        initial[i] = UINT32_C(0x3c3c3c3c);
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, initial);
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 13);
    unsigned initially_different = 0;
    for (unsigned y = 0; y < HEIGHT; ++y)
        for (unsigned x = 0; x < WIDTH; ++x)
            initially_different += initial[y * WIDTH + x] != pattern(1, x, y);
    REQUIRE(initially_different, 14);
    say("GL4 baseline: public glReadPixels observed an image different from frame 1's");

    if (presenter && presenter->native) {
        unsigned differ = 0;
        for (unsigned s = 1; s <= J0C_FRAMES; ++s) {
            code = frame(J0C_TAG + s, texels, output, modeled, &differ);
            REQUIRE(!code && !differ, 100 + code);
            code = presenter->j0c(output + GUARDS, WIDTH, HEIGHT, s);
            REQUIRE(!code, 200 + code);
        }
        say("GL4 J0c: three exact GL frames presented through the native lease, each retired before the next");
    }
    if (presenter) {
        code = presenter->ready();
        REQUIRE(!code, 300 + code);
    }

    unsigned differ = 0;
    for (unsigned f = 1; f <= FRAMES; ++f) {
        code = frame(f, texels, output, modeled, &differ);
        if (f <= 3 || f % 10 == 0)
        if (code || differ) {
            report(say, "GL4 frame %u failed: code %u, %u pixels differ", f, code, differ);
            REQUIRE(false, 400 + code);
        }
        if (presenter) {
            code = presenter->frame(output + GUARDS, WIDTH, HEIGHT);
            REQUIRE(!code, 500 + code);
        }
    }
    REQUIRE(api->device_status(api->context) == KUMO_GPU_OK, 15);
    report(say, "GL4 frames: %u frames each created, uploaded, drew with and deleted one 160 KiB texture "
           "(16000 KiB in all); %s", FRAMES,
           modeled ? "readback guards intact on every frame (modeled service runs no GPU; pixels not compared)"
                   : "0 of 4096000 pixels differ (Mali: every texel copied exactly)");
    _Static_assert(FRAMES * PIXELS == 4096000u, "the metal line names the exact pixel count");
done:
    if (context && kumo_gl_is_current(context)) {
        glUseProgram(0);
        if (program) glDeleteProgram(program);
        if (fragment) glDeleteShader(fragment);
        if (vertex) glDeleteShader(vertex);
        if (!failed && (glGetError() != GL_NO_ERROR || api->device_status(api->context) != KUMO_GPU_OK))
            failed = 16;
    }
    free(texels);
    free(output);
    free(initial);
    if (context)
        kumo_gl_context_destroy(context);
    if (drawable && !kumo_gl_drawable_destroy(drawable))
        failed = failed ? failed : 17;
    if (!kumo_gl_display_destroy(display))
        failed = failed ? failed : 18;
    if (!failed)
        say("GL4 destroy: program, context, drawable and display released");
    return failed;
}

#ifdef KUMO_GL_CHURN_MAIN
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
extern unsigned kumo_metal_churn_take(void);
extern unsigned kumo_metal_churn_native(void);
extern unsigned kumo_metal_j0c(const uint32_t *pixels, uint32_t width, uint32_t height, uint32_t sequence);
extern unsigned kumo_metal_churn_ready(void);
extern unsigned kumo_metal_churn_frame(const uint32_t *pixels, uint32_t width, uint32_t height);
extern void kumo_metal_churn_summary(uint32_t frames);
static struct kumo_gl_churn_presenter presenter = {
    .j0c = kumo_metal_j0c,
    .ready = kumo_metal_churn_ready,
    .frame = kumo_metal_churn_frame,
};
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_gl_churn_cases(api, kumo_metal_log, false, &presenter);
}

int main(void)
{
    kumo_metal_log("GL4 metal churn begin: 100 GL frames of 160 KiB texture churn on view 1");
    unsigned failed = kumo_metal_churn_take();
    if (failed) {
        report(kumo_metal_log, "GL4 metal churn FAILED at lane step %u", failed);
        return 1;
    }
    presenter.native = kumo_metal_churn_native() != 0;
    failed = kumo_metal_proof();
    kumo_metal_churn_summary(FRAMES);
    if (failed) {
        report(kumo_metal_log, "GL4 metal churn FAILED at case %u", failed);
        return 1;
    }
    kumo_metal_log("GL4 metal churn green: 100 exact GL frames of texture churn presented on view 1 and clean teardown; "
                   "the presents line carries the Retired/Copied record");
    return 0;
}
#endif
