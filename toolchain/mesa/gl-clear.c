/* SPDX-License-Identifier: MIT */
/*j772*/
/* GL2-b: public GLES clear/readback over the native session. QEMU retains an
 * observed initial framebuffer because its producer executes no GPU. Mali must
 * replace every pixel with the clear color. — KESTREL */
#include "kumo_gl.h"
#include "kumo_kmod.h"
#include "pipe/p_screen.h"
#include <GLES3/gl3.h>
#include <stdarg.h>
#include <stdio.h>

struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);
unsigned kumo_gl_clear_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled);

#define WIDTH 64u
#define HEIGHT 48u
#define PIXELS (WIDTH * HEIGHT)
#define GUARDS 16u
#define CLEARED UINT32_C(0xff996633)
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

unsigned kumo_gl_clear_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled)
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
    struct kumo_gl_context *context = kumo_gl_context_create(display);
    struct kumo_gl_drawable *drawable = NULL;
    REQUIRE(context, 4);
    drawable = kumo_gl_drawable_create(display, WIDTH, HEIGHT, NULL, NULL);
    REQUIRE(drawable && kumo_gl_make_current(context, drawable), 5);
    say("GL2-b context: current GLES context over the native GPU session");

    REQUIRE(glCheckFramebufferStatus(GL_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE &&
            glGetError() == GL_NO_ERROR, 6);
    say("GL2-b target: complete owned 64x48 RGBA8 frontend framebuffer");

    uint32_t initial[PIXELS];
    for (unsigned i = 0; i < PIXELS; ++i)
        initial[i] = UINT32_C(0x3c3c3c3c);
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, initial);
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 7);
    unsigned initially_different = 0;
    for (unsigned i = 0; i < PIXELS; ++i)
        initially_different += initial[i] != CLEARED;
    REQUIRE(initially_different, 8);
    say("GL2-b baseline: public glReadPixels observed pixels different from the requested clear color");

    glDisable(GL_DITHER);
    glDisable(GL_SCISSOR_TEST);
    glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
    glClearColor(0.2f, 0.4f, 0.6f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 10);
    say("GL2-b clear: public glClear and native glFinish complete, expected RGBA8 0xff996633");

    /* ReadPixels must write every output pixel and neither CPU-side guard region.
     * This is readback bounds evidence, not a GPU DMA-isolation claim. — KESTREL */
    uint32_t output[GUARDS + PIXELS + GUARDS];
    for (unsigned i = 0; i < GUARDS + PIXELS + GUARDS; ++i)
        output[i] = GUARD;
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, WIDTH, HEIGHT, GL_RGBA, GL_UNSIGNED_BYTE, output + GUARDS);
    REQUIRE(glGetError() == GL_NO_ERROR && api->device_status(api->context) == KUMO_GPU_OK, 11);
    unsigned differ = 0, damaged = 0;
    for (unsigned i = 0; i < PIXELS; ++i)
        differ += output[GUARDS + i] != (modeled ? initial[i] : CLEARED);
    for (unsigned i = 0; i < GUARDS; ++i) {
        damaged += output[i] != GUARD;
        damaged += output[GUARDS + PIXELS + i] != GUARD;
    }
    report(say, "GL2-b readback: %u of 3072 pixels differ; %u of 32 readback guards changed (%s)",
           differ, damaged, modeled ? "modeled service, initial framebuffer retained" : "Mali clear color 0xff996633");
    report(say, "GL2-b samples: first=0x%08x last=0x%08x", output[GUARDS], output[GUARDS + PIXELS - 1]);
    REQUIRE(!differ && !damaged, 12);
done:
    if (context && kumo_gl_is_current(context)) {
        if (!failed && (glGetError() != GL_NO_ERROR || api->device_status(api->context) != KUMO_GPU_OK))
            failed = 13;
    }
    if (context)
        kumo_gl_context_destroy(context);
    if (drawable && !kumo_gl_drawable_destroy(drawable))
        failed = failed ? failed : 14;
    if (!kumo_gl_display_destroy(display))
        failed = failed ? failed : 15;
    if (!failed)
        say("GL2-b destroy: context, owned framebuffer drawable and display released");
    return failed;
}

#ifdef KUMO_GL_CLEAR_MAIN
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_gl_clear_cases(api, kumo_metal_log, false);
}

int main(void)
{
    kumo_metal_log("GL2-b metal clear begin: public GLES clear and framebuffer readback");
    unsigned failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "GL2-b metal clear FAILED at case %u", failed);
        return 1;
    }
    kumo_metal_log("GL2-b metal clear green: Mali glClear replaced all 3072 RGBA8 pixels, glReadPixels guards intact, native completion and clean teardown");
    return 0;
}
#endif
