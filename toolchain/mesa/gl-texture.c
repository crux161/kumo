/* SPDX-License-Identifier: MIT */
/* GL4-a (PLAN/015 §8.32 D4): public texture allocation against the session budget. J772's
 * first texture-backed framebuffer failed with GL_OUT_OF_MEMORY before any resource
 * existed: panfrost's size preflight asked for physical memory, and relibc's
 * _SC_PHYS_PAGES returns -1. Under __kumo__ the preflight now compares against the
 * native session budget instead, claiming no RAM figure. So a texture larger than the
 * budget is still refused, and GL4's 160 KiB texture is created, rendered and read
 * back. QEMU retains the observed initial texture image because its producer executes
 * no GPU. */
#include "kumo_gl.h"
#include "kumo_kmod.h"
#include "pipe/p_screen.h"
#include <GLES3/gl3.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);
unsigned kumo_gl_texture_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled);

/* GL4's per-frame storage: 256 x 160 RGBA8 is exactly 160 KiB. */
#define WIDTH 256u
#define HEIGHT 160u
#define PIXELS (WIDTH * HEIGHT)
/* Twice the 128 MiB budget, and inside panfrost's 16384 limit. */
#define HUGE 8192
#define GUARDS 16u
#define CLEARED UINT32_C(0xff996633)
#define GUARD UINT32_C(0xa5a5a5a5)
#define REQUIRE(test, code) do { if (!(test)) { failed = (code); goto done; } } while (0)
_Static_assert(KUMO_GPU_SESSION_BUDGET_BYTES < (unsigned long long)HUGE * HUGE * 4u,
               "the refused texture must exceed the session budget");

static void report(void (*say)(const char *), const char *format, ...)
{
    char line[240];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof line, format, args);
    va_end(args);
    say(line);
}

unsigned kumo_gl_texture_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled)
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
    GLuint texture = 0, framebuffer = 0;
    uint32_t *initial = NULL, *output = NULL;
    struct kumo_gl_context *context = kumo_gl_context_create(display);
    struct kumo_gl_drawable *drawable = NULL;
    REQUIRE(context, 4);
    drawable = kumo_gl_drawable_create(display, 64, 48, NULL, NULL);
    REQUIRE(drawable && kumo_gl_make_current(context, drawable), 5);
    say("GL4-a context: current GLES context over the native GPU session");

    /* Single-level textures, so the preflight sizes level 0 only. */
    glGenTextures(1, &texture);
    REQUIRE(texture, 6);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    REQUIRE(glGetError() == GL_NO_ERROR, 7);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, HUGE, HUGE, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    REQUIRE(glGetError() == GL_OUT_OF_MEMORY && api->device_status(api->context) == KUMO_GPU_OK, 8);
    say("GL4-a preflight: an 8192x8192 RGBA8 texture (256 MiB) refused with GL_OUT_OF_MEMORY against the 128 MiB session budget");

    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, WIDTH, HEIGHT, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    REQUIRE(glGetError() == GL_NO_ERROR, 9);
    glGenFramebuffers(1, &framebuffer);
    REQUIRE(framebuffer, 10);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
    REQUIRE(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE &&
            glGetError() == GL_NO_ERROR, 11);
    say("GL4-a texture: a 256x160 RGBA8 texture (160 KiB) created and attached; framebuffer complete");

    initial = malloc(PIXELS * sizeof *initial);
    output = malloc((GUARDS + PIXELS + GUARDS) * sizeof *output);
    REQUIRE(initial && output, 12);
    for (unsigned i = 0; i < PIXELS; ++i)
        initial[i] = UINT32_C(0x3c3c3c3c);
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, initial);
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 13);
    unsigned initially_different = 0;
    for (unsigned i = 0; i < PIXELS; ++i)
        initially_different += initial[i] != CLEARED;
    REQUIRE(initially_different, 14);
    say("GL4-a baseline: public glReadPixels observed texture pixels different from the requested clear color");

    glViewport(0, 0, WIDTH, HEIGHT);
    glDisable(GL_DITHER);
    glDisable(GL_SCISSOR_TEST);
    glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
    glClearColor(0.2f, 0.4f, 0.6f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 15);
    say("GL4-a clear: public glClear into the texture and native glFinish complete, expected RGBA8 0xff996633");

    for (unsigned i = 0; i < GUARDS + PIXELS + GUARDS; ++i)
        output[i] = GUARD;
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, output + GUARDS);
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 16);
    unsigned differ = 0, damaged = 0;
    for (unsigned i = 0; i < PIXELS; ++i)
        differ += output[GUARDS + i] != (modeled ? initial[i] : CLEARED);
    for (unsigned i = 0; i < GUARDS; ++i) {
        damaged += output[i] != GUARD;
        damaged += output[GUARDS + PIXELS + i] != GUARD;
    }
    report(say, "GL4-a readback: %u of %u pixels differ; %u of 32 readback guards changed (%s)", differ, PIXELS,
           damaged, modeled ? "modeled service, initial texture retained" : "Mali clear color 0xff996633");
    REQUIRE(!differ && !damaged, 17);
done:
    if (context && kumo_gl_is_current(context)) {
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        glBindTexture(GL_TEXTURE_2D, 0);
        if (framebuffer) glDeleteFramebuffers(1, &framebuffer);
        if (texture) glDeleteTextures(1, &texture);
        if (!failed && (glGetError() != GL_NO_ERROR || api->device_status(api->context) != KUMO_GPU_OK))
            failed = 18;
    }
    free(output);
    free(initial);
    if (context)
        kumo_gl_context_destroy(context);
    if (drawable && !kumo_gl_drawable_destroy(drawable))
        failed = failed ? failed : 19;
    if (!kumo_gl_display_destroy(display))
        failed = failed ? failed : 20;
    if (!failed)
        say("GL4-a destroy: texture, framebuffer, context, drawable and display released");
    return failed;
}

#ifdef KUMO_GL_TEXTURE_MAIN
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_gl_texture_cases(api, kumo_metal_log, false);
}

int main(void)
{
    kumo_metal_log("GL4-a metal texture begin: public texture allocation against the session budget");
    unsigned failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "GL4-a metal texture FAILED at case %u", failed);
        return 1;
    }
    kumo_metal_log("GL4-a metal texture green: a 256 MiB texture refused by the budget preflight, a 160 KiB texture "
                   "cleared on Mali and read back exactly, guards intact and clean teardown");
    return 0;
}
#endif
