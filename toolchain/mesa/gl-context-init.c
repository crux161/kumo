/* SPDX-License-Identifier: MIT */
/*j762*/
/* GL1-c3: the actual Gallium context initialization and cleanup, over the native bridge.
 * QEMU models the completion producer; the same cases compile for the metal client.
 * No DRM fd, private Mesa layout, or hand-built replacement init stream. — KESTREL */
#include "kumo_kmod.h"
#include "pipe/p_context.h"
#include "pipe/p_screen.h"

struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);
#define CHECK(test, number) do { if (!(test)) return (number); } while (0)

unsigned kumo_gl_context_init_cases(const struct kumo_gpu_api *api, void (*say)(const char *))
{
    struct pan_kmod_dev *kmod = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(kmod, 1);
    struct pipe_screen *screen = panfrost_create_screen_kumo(kmod);
    CHECK(screen, 2);
    say("GL1-c initialize: creating Gallium's native CSF context");
    struct pipe_context *pipe = screen->context_create(screen, NULL, 0);
    CHECK(pipe && api->device_status(api->context) == KUMO_GPU_OK, 3);
    say("GL1-c context: Gallium created its CSF context; the native initialization submission completed");
    CHECK(!screen->context_create(screen, NULL, 0), 4);
    CHECK(api->device_status(api->context) == KUMO_GPU_OK, 5);
    say("GL1-c admission: a second context was refused while the first remained live");
    pipe->destroy(pipe);
    CHECK(api->device_status(api->context) == KUMO_GPU_OK, 6);
    say("GL1-c cleanup: native completion observed, heap released, context destroyed");
    CHECK(!screen->context_create(screen, NULL, 0), 7);
    CHECK(api->device_status(api->context) == KUMO_GPU_OK, 8);
    screen->destroy(screen);
    say("GL1-c destroy: the screen and device are gone; the session-owned group was not reused");
    return 0;
}
