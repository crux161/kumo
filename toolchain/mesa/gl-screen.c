/* SPDX-License-Identifier: MIT */
/*j759*/
/* GL1-b: Gallium's panfrost screen over KUMO's native device. No DRM fd and no pipe-loader:
 * the client makes the kmod device from its granted session and hands it to
 * panfrost_create_screen_kumo, which owns it from then on. Opening the device creates the
 * AUTO_VA | TRACK_ACTIVITY VM and the first BOs (the sample positions, auto-VA bound and
 * CPU-written), so a screen that comes up has already exercised the new VM path. The cases
 * then query identity and caps, create and destroy a render-target texture, and destroy the
 * screen, which must release every BO and binding. Shared by the QEMU device smoke (mode 17)
 * and the metal client (proof 6). — TIMBERDOODLE */
#include "kumo_kmod.h"
#include "pipe/p_defines.h"
#include "pipe/p_screen.h"
#include "pipe/p_state.h"
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

/* Declared by kumo-build.patch's pan_public.h; the host's header tree is unpatched. */
struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);

#define CHECK(test, number) do { if (!(test)) return (number); } while (0)

static void report(void (*say)(const char *), const char *format, ...)
{
    char line[240];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof line, format, args);
    va_end(args);
    say(line);
}

unsigned kumo_gl_screen_cases(const struct kumo_gpu_api *api, void (*say)(const char *))
{
    struct pan_kmod_dev *kmod = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(kmod, 1);
    struct pipe_screen *screen = panfrost_create_screen_kumo(kmod);
    CHECK(screen, 2);
    const char *name = screen->get_name(screen);
    const char *vendor = screen->get_vendor(screen);
    CHECK(name && strstr(name, "Mali-G610") && strstr(name, "(Panfrost)"), 3);
    report(say, "GL1-b screen: %s from %s, over KUMO's native device with no DRM fd", name, vendor);
    report(say, "GL1-b caps: GLSL %u, max 2D texture %u, compute %s, max texture array layers %u",
           screen->caps.glsl_feature_level, screen->caps.max_texture_2d_size,
           screen->caps.compute ? "yes" : "no", screen->caps.max_texture_array_layers);
    CHECK(screen->caps.max_texture_2d_size >= 4096 && screen->caps.compute, 4);
    CHECK(screen->is_format_supported(screen, PIPE_FORMAT_R8G8B8A8_UNORM, PIPE_TEXTURE_2D, 0, 0,
                                      PIPE_BIND_RENDER_TARGET | PIPE_BIND_SAMPLER_VIEW), 5);
    const struct pipe_resource template = {
        .target = PIPE_TEXTURE_2D,
        .format = PIPE_FORMAT_R8G8B8A8_UNORM,
        .width0 = 64,
        .height0 = 64,
        .depth0 = 1,
        .array_size = 1,
        .bind = PIPE_BIND_RENDER_TARGET | PIPE_BIND_SAMPLER_VIEW,
        .usage = PIPE_USAGE_DEFAULT,
    };
    struct pipe_resource *texture = screen->resource_create(screen, &template);
    CHECK(texture && texture->width0 == 64 && texture->format == PIPE_FORMAT_R8G8B8A8_UNORM, 6);
    report(say, "GL1-b resource: a 64x64 RGBA8 render-target texture created, its BO bound by AUTO_VA");
    /* The one reference resource_create returned; pipe_resource_reference would do the same. */
    screen->resource_destroy(screen, texture);
    screen->destroy(screen);
    say("GL1-b destroy: the texture and the screen are gone, and the device with them");
    return 0;
}
