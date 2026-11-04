/* SPDX-License-Identifier: MIT */
/*j773*/
/* GL3-a: the first GLES triangle through Mali's tiler, offscreen. Runtime-compiled
 * vertex and fragment shaders draw one triangle into the frontend's owned 256x128
 * RGBA8 framebuffer, and public glReadPixels checks every pixel. No pixel center
 * lies on the hypotenuse, so each pixel has one exact expected value. QEMU retains
 * the observed initial image because its producer executes no GPU. — TIMBERDOODLE */
#include "kumo_gl.h"
#include "kumo_kmod.h"
#include "pipe/p_screen.h"
#include <GLES3/gl3.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);
unsigned kumo_gl_triangle_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled,
                                uint32_t *keep);

#define WIDTH 256u
#define HEIGHT 128u
#define PIXELS (WIDTH * HEIGHT)
#define GUARDS 16u
#define CLEARED UINT32_C(0xff996633)
#define SHADED UINT32_C(0xff6633cc)
#define GUARD UINT32_C(0xa5a5a5a5)
/* Window coordinates, origin bottom-left as glReadPixels returns rows. The edge from
 * (256,0) to (0,128) is X + 2Y = 256; a center (x+0.5, y+0.5) never lies on it. */
#define INSIDE(x, y) ((x) + 2u * (y) <= 254u)
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

/* DRM modifiers name the layout Mesa chose, so readback's path is stated, not guessed:
 * AFBC reads through a staging blit, u-interleaved through a CPU detile. */
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
        report(say, "GL3-a shader log: %s", log);
        glDeleteShader(shader);
        return 0;
    }
    return shader;
}

/* `keep`, when not NULL, receives the verified readback (PIXELS words, rows bottom-up)
 * for GL3-b's copy into a view; GL3-a and QEMU mode 25 pass NULL and are unchanged. */
unsigned kumo_gl_triangle_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled,
                                uint32_t *keep)
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
    GLuint vertex = 0, fragment = 0, program = 0, buffer = 0;
    uint32_t *initial = NULL, *output = NULL;
    struct kumo_gl_context *context = kumo_gl_context_create(display);
    struct kumo_gl_drawable *drawable = NULL;
    REQUIRE(context, 4);
    drawable = kumo_gl_drawable_create(display, WIDTH, HEIGHT, NULL, NULL);
    REQUIRE(drawable && kumo_gl_make_current(context, drawable), 5);
    say("GL3-a context: current GLES context over the native GPU session");

    struct pipe_resource *color = kumo_gl_drawable_resource(drawable, false);
    uint64_t before = 0;
    REQUIRE(color && modifier_of(screen, color, &before), 6);
    REQUIRE(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE &&
            glGetError() == GL_NO_ERROR, 7);
    report(say, "GL3-a target: complete owned 256x128 RGBA8 frontend framebuffer, color modifier 0x%016llx (%s)",
           (unsigned long long)before, layout(before));

    vertex = compile(say, GL_VERTEX_SHADER,
                     "#version 300 es\n"
                     "layout(location = 0) in vec2 position;\n"
                     "layout(location = 1) in vec4 color;\n"
                     "out vec4 shade;\n"
                     "void main() { shade = color; gl_Position = vec4(position, 0.0, 1.0); }\n");
    REQUIRE(vertex && glGetError() == GL_NO_ERROR, 8);
    fragment = compile(say, GL_FRAGMENT_SHADER,
                       "#version 300 es\n"
                       "precision highp float;\n"
                       "in vec4 shade;\n"
                       "out vec4 fragment;\n"
                       "void main() { fragment = shade; }\n");
    REQUIRE(fragment && glGetError() == GL_NO_ERROR, 9);
    program = glCreateProgram();
    REQUIRE(program, 10);
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = GL_FALSE;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) {
        char log[200] = {0};
        glGetProgramInfoLog(program, sizeof log, NULL, log);
        report(say, "GL3-a program log: %s", log);
    }
    REQUIRE(linked && glGetError() == GL_NO_ERROR, 11);
    say("GL3-a compiler: GLSL ES 300 vertex and fragment sources compiled and linked at runtime");

    /* One varying color, equal at all three vertices, so interpolation is exact:
     * (0.8, 0.2, 0.4, 1.0) packs to RGBA8 0xff6633cc. */
    static const GLfloat vertices[] = {
        -1.0f, -1.0f, 0.8f, 0.2f, 0.4f, 1.0f,
         1.0f, -1.0f, 0.8f, 0.2f, 0.4f, 1.0f,
        -1.0f,  1.0f, 0.8f, 0.2f, 0.4f, 1.0f,
    };
    glGenBuffers(1, &buffer);
    REQUIRE(buffer, 12);
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    glBufferData(GL_ARRAY_BUFFER, sizeof vertices, vertices, GL_STATIC_DRAW);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 6 * sizeof(GLfloat), (const void *)0);
    glVertexAttribPointer(1, 4, GL_FLOAT, GL_FALSE, 6 * sizeof(GLfloat), (const void *)(2 * sizeof(GLfloat)));
    glEnableVertexAttribArray(0);
    glEnableVertexAttribArray(1);
    REQUIRE(glGetError() == GL_NO_ERROR, 13);

    initial = malloc(PIXELS * sizeof *initial);
    output = malloc((GUARDS + PIXELS + GUARDS) * sizeof *output);
    REQUIRE(initial && output, 14);
    for (unsigned i = 0; i < PIXELS; ++i)
        initial[i] = UINT32_C(0x3c3c3c3c);
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, initial);
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 15);
    unsigned initially_different = 0;
    for (unsigned y = 0; y < HEIGHT; ++y)
        for (unsigned x = 0; x < WIDTH; ++x)
            initially_different += initial[y * WIDTH + x] != (INSIDE(x, y) ? SHADED : CLEARED);
    REQUIRE(initially_different, 16);
    say("GL3-a baseline: public glReadPixels observed an image different from the expected triangle");

    glViewport(0, 0, WIDTH, HEIGHT);
    glDisable(GL_DITHER);
    glDisable(GL_SCISSOR_TEST);
    glDisable(GL_BLEND);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_CULL_FACE);
    glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
    glClearColor(0.2f, 0.4f, 0.6f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glUseProgram(program);
    glDrawArrays(GL_TRIANGLES, 0, 3);
    glFinish();
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 17);
    say("GL3-a draw: glClear and one glDrawArrays triangle, native glFinish complete");

    /* ReadPixels must write every output pixel and neither CPU-side guard region.
     * This is readback bounds evidence, not a GPU DMA-isolation claim. */
    for (unsigned i = 0; i < GUARDS + PIXELS + GUARDS; ++i)
        output[i] = GUARD;
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, output + GUARDS);
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 18);
    const uint32_t *pixels = output + GUARDS;
    unsigned differ = 0, inside = 0, damaged = 0;
    for (unsigned y = 0; y < HEIGHT; ++y)
        for (unsigned x = 0; x < WIDTH; ++x) {
            unsigned i = y * WIDTH + x;
            inside += INSIDE(x, y);
            differ += pixels[i] != (modeled ? initial[i] : INSIDE(x, y) ? SHADED : CLEARED);
        }
    for (unsigned i = 0; i < GUARDS; ++i) {
        damaged += output[i] != GUARD;
        damaged += output[GUARDS + PIXELS + i] != GUARD;
    }
    uint64_t after = 0;
    REQUIRE(modifier_of(screen, color, &after), 19);
    report(say, "GL3-a readback: %u of 32768 pixels differ; %u of 32 readback guards changed (%s)",
           differ, damaged, modeled ? "modeled service, initial framebuffer retained"
                                    : "Mali triangle 0xff6633cc over clear 0xff996633");
    report(say, "GL3-a coverage: %u pixels inside x+2y<=254; samples (0,0)=0x%08x (254,0)=0x%08x "
           "(255,0)=0x%08x (0,127)=0x%08x (255,127)=0x%08x", inside, pixels[0], pixels[254],
           pixels[255], pixels[127 * WIDTH], pixels[PIXELS - 1]);
    report(say, "GL3-a layout: color modifier after readback 0x%016llx (%s)",
           (unsigned long long)after, layout(after));
    REQUIRE(inside == PIXELS / 2 && !differ && !damaged, 20);
    if (keep)
        memcpy(keep, pixels, PIXELS * sizeof *pixels);
done:
    if (context && kumo_gl_is_current(context)) {
        glUseProgram(0);
        glDisableVertexAttribArray(0);
        glDisableVertexAttribArray(1);
        glBindBuffer(GL_ARRAY_BUFFER, 0);
        if (buffer) glDeleteBuffers(1, &buffer);
        if (program) glDeleteProgram(program);
        if (fragment) glDeleteShader(fragment);
        if (vertex) glDeleteShader(vertex);
        if (!failed && (glGetError() != GL_NO_ERROR || api->device_status(api->context) != KUMO_GPU_OK))
            failed = 21;
    }
    free(output);
    free(initial);
    if (context)
        kumo_gl_context_destroy(context);
    if (drawable && !kumo_gl_drawable_destroy(drawable))
        failed = failed ? failed : 22;
    if (!kumo_gl_display_destroy(display))
        failed = failed ? failed : 23;
    if (!failed)
        say("GL3-a destroy: shaders, program, vertex buffer, context, drawable and display released");
    return failed;
}

#ifdef KUMO_GL_TRIANGLE_MAIN
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_gl_triangle_cases(api, kumo_metal_log, false, NULL);
}

int main(void)
{
    kumo_metal_log("GL3-a metal triangle begin: runtime GLSL ES vertex/fragment shaders and one tiled draw");
    unsigned failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "GL3-a metal triangle FAILED at case %u", failed);
        return 1;
    }
    kumo_metal_log("GL3-a metal triangle green: Mali rasterized the triangle exactly, all 32768 RGBA8 pixels and 32 guards checked, native completion and clean teardown");
    return 0;
}
#endif

#ifdef KUMO_GL_PRESENT_MAIN
/* GL3-b (PLAN/015 §8.32): the GL3-a triangle, read back through public glReadPixels,
 * converted to XRGB8888 on the CPU, written into view 1 (GPU_VIEW) and presented on HDMI.
 * The view arrives over Sora's lane because the stage adds the `etc/gpu-lane` marker
 * `view` (D1), so the lane is decoded before Mesa starts. The GL objects are released
 * before the hold; the view's lease returns when this process exits. This image runs no
 * J0c diagnostic, so drv-fb's present gate stays unarmed and the presents are honestly
 * Copied: CRUX accepted that gate on 2026-11-03, leaving Retired to GL4. */
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
extern unsigned kumo_metal_view_take(void);
extern unsigned kumo_metal_view_present(const uint32_t *pixels, uint32_t width, uint32_t height);
static uint32_t *present_image;
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_gl_triangle_cases(api, kumo_metal_log, false, present_image);
}

int main(void)
{
    kumo_metal_log("GL3-b metal present begin: the GL3-a triangle, read back, copied into view 1 and presented on HDMI");
    unsigned failed = kumo_metal_view_take();
    if (failed) {
        report(kumo_metal_log, "GL3-b metal present FAILED at lane step %u", failed);
        return 1;
    }
    present_image = malloc(PIXELS * sizeof *present_image);
    if (!present_image) {
        kumo_metal_log("GL3-b metal present FAILED: no readback copy buffer");
        return 1;
    }
    failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "GL3-b metal present FAILED at case %u", failed);
        free(present_image);
        return 1;
    }
    failed = kumo_metal_view_present(present_image, WIDTH, HEIGHT);
    free(present_image);
    if (failed) {
        report(kumo_metal_log, "GL3-b metal present FAILED at present step %u", failed);
        return 1;
    }
    kumo_metal_log("GL3-b metal present green: the exact Mali triangle was copied into view 1 and every present was accepted; Copied is the gate on an image without J0c");
    return 0;
}
#endif
