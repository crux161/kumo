/* SPDX-License-Identifier: MIT */
/*j769*/
/* GL1-f: a static GLES2 client. Its only GL interface is the public GLES2
 * header and KUMO's frontend; no private Mesa context or dispatch calls.
 * The same application body runs against QEMU's modeled producer. — KESTREL */
#include "kumo_gl.h"
#include "kumo_kmod.h"
#include "pipe/p_screen.h"
#include <GLES2/gl2.h>
#include <stdio.h>
#include <string.h>

struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);
unsigned kumo_gl_client_cases(const struct kumo_gpu_api *api, void (*say)(const char *));

unsigned kumo_gl_client_cases(const struct kumo_gpu_api *api, void (*say)(const char *))
{
    struct pan_kmod_dev *kmod = pan_kmod_kumo_dev_create(api, NULL);
    if (!kmod)
        return 1;
    /* The screen factory owns kmod, including on failure. — KESTREL */
    struct pipe_screen *screen = panfrost_create_screen_kumo(kmod);
    if (!screen)
        return 2;
    struct kumo_gl_display *display = kumo_gl_display_create(screen);
    if (!display) {
        screen->destroy(screen);
        return 3;
    }
    unsigned failed = 4;
    struct kumo_gl_context *context = kumo_gl_context_create(display);
    struct kumo_gl_drawable *drawable = NULL;
    if (!context)
        goto done;
    failed = 5;
    drawable = kumo_gl_drawable_create(display, 64, 48, NULL, NULL);
    if (!drawable || !kumo_gl_make_current(context, drawable))
        goto done;
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    const char *version = (const char *)glGetString(GL_VERSION);
    failed = 6;
    if (!renderer || !strstr(renderer, "Mali-G610") || !strstr(renderer, "(Panfrost)") ||
        !version || strncmp(version, "OpenGL ES ", 10) || glGetError() != GL_NO_ERROR)
        goto done;
    char line[240];
    snprintf(line, sizeof line, "GL1-f renderer: %s", renderer);
    say(line);
    snprintf(line, sizeof line, "GL1-f version: %s", version);
    say(line);
    glFinish();
    failed = 7;
    if (glGetError() != GL_NO_ERROR || api->device_status(api->context) != KUMO_GPU_OK)
        goto done;
    say("GL1-f finish: public glFinish returned with GL_NO_ERROR and the native device live");
    failed = 0;
done:
    if (context)
        kumo_gl_context_destroy(context);
    if (drawable && !kumo_gl_drawable_destroy(drawable))
        failed = failed ? failed : 8;
    if (!kumo_gl_display_destroy(display))
        failed = failed ? failed : 9;
    if (!failed)
        say("GL1-f destroy: context unbound, drawable and display destroyed");
    return failed;
}

#ifdef KUMO_GL_CLIENT_MAIN
/* Reuse the existing Sora GPU-slot bootstrap and native-session owner. The ELF
 * has its own main and no dependency on the metal proof selector. — KESTREL */
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_gl_client_cases(api, kumo_metal_log);
}

int main(void)
{
    kumo_metal_log("GL1-f metal client begin: static GLES2 application over KUMO's native session");
    unsigned failed = kumo_metal_proof();
    if (failed) {
        char line[80];
        snprintf(line, sizeof line, "GL1-f metal client FAILED at case %u", failed);
        kumo_metal_log(line);
        return 1;
    }
    kumo_metal_log("GL1-f metal client green: public GLES2 renderer names Mali-G610, glFinish completed, all frontend resources destroyed");
    return 0;
}
#endif
