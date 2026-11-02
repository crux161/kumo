/* SPDX-License-Identifier: MIT */
/*j768*/
/* Shared lifecycle proof: real Mesa state tracker; modeled completion in QEMU,
 * actual native initialization on Mali. No replacement GL state. — KESTREL */
#include "kumo_gl.h"
#include "kumo_kmod.h"
#include "pipe/p_screen.h"
#include "pipe/p_state.h"
#include "state_tracker/st_context.h"

struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);
unsigned kumo_gl_frontend_cases(const struct kumo_gpu_api *api, void (*say)(const char *));
#define CHECK(test, number) do { if (!(test)) return (number); } while (0)

struct presentation {
    struct pipe_resource *expected;
    unsigned calls;
    bool accept;
};
static bool present(void *user, struct pipe_resource *color)
{
    struct presentation *state = user;
    if (color != state->expected || color->width0 != 64 || color->height0 != 48)
        return false;
    ++state->calls;
    return state->accept;
}

unsigned kumo_gl_frontend_cases(const struct kumo_gpu_api *api, void (*say)(const char *))
{
    CHECK(!kumo_gl_display_create(NULL), 1);
    struct pan_kmod_dev *kmod = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(kmod, 2);
    struct pipe_screen *screen = panfrost_create_screen_kumo(kmod);
    CHECK(screen, 3);
    struct kumo_gl_display *display = kumo_gl_display_create(screen);
    CHECK(display && !kumo_gl_display_create(screen), 4);
    CHECK(!kumo_gl_drawable_create(display, 0, 48, NULL, NULL) &&
          !kumo_gl_drawable_create(display, 64, 0, NULL, NULL) &&
          !kumo_gl_drawable_create(display, screen->caps.max_texture_2d_size + 1, 48, NULL, NULL), 5);
    struct kumo_gl_drawable *unused = kumo_gl_drawable_create(display, 16, 16, NULL, NULL);
    CHECK(unused && kumo_gl_drawable_destroy(unused), 21);
    struct presentation state = {.accept = true};
    struct kumo_gl_drawable *drawable = kumo_gl_drawable_create(display, 64, 48, present, &state);
    CHECK(drawable && !kumo_gl_drawable_create(display, 64, 48, NULL, NULL), 6);
    state.expected = kumo_gl_drawable_resource(drawable, false);
    struct pipe_resource *depth = kumo_gl_drawable_resource(drawable, true);
    CHECK(state.expected && depth && state.expected != depth &&
          state.expected->format == PIPE_FORMAT_R8G8B8A8_UNORM &&
          depth->format == PIPE_FORMAT_Z24_UNORM_S8_UINT && depth->width0 == 64 && depth->height0 == 48, 7);
    say("GL1-e drawable: owned 64x48 RGBA8 color and Z24/S8 depth/stencil attachments");
    struct kumo_gl_context *context = kumo_gl_context_create(display);
    CHECK(context && !kumo_gl_context_create(display) && !kumo_gl_display_destroy(display), 8);
    CHECK(!st_api_get_current() && !kumo_gl_is_current(context), 9);
    CHECK(!kumo_gl_make_current(context, NULL) && !kumo_gl_make_current(NULL, drawable), 10);
    CHECK(kumo_gl_make_current(context, drawable) && kumo_gl_is_current(context) &&
          st_api_get_current(), 11);
    struct st_context *st = st_api_get_current();
    struct gl_framebuffer *framebuffer = st->ctx->DrawBuffer;
    CHECK(st->ctx->API == API_OPENGLES2 && !st->ctx->GLThread.enabled && !st->is_threaded_context, 22);
    CHECK(framebuffer && framebuffer->Width == 64 && framebuffer->Height == 48 &&
          framebuffer->Attachment[BUFFER_FRONT_LEFT].Renderbuffer &&
          framebuffer->Attachment[BUFFER_FRONT_LEFT].Renderbuffer->texture == state.expected &&
          framebuffer->Attachment[BUFFER_DEPTH].Renderbuffer &&
          framebuffer->Attachment[BUFFER_DEPTH].Renderbuffer->texture == depth, 23);
    say("GL1-e current: Mesa's GLES2 state-tracker context made current; glthread and threaded pipe off");
    CHECK(!kumo_gl_drawable_destroy(drawable), 12);
    CHECK(kumo_gl_flush_front(context) && state.calls == 1, 13);
    state.accept = false;
    CHECK(!kumo_gl_flush_front(context) && state.calls == 2, 14);
    state.accept = true;
    say("GL1-e front: validate supplied the attachments; native flush completed before the callback; callback refusal propagated");
    CHECK(kumo_gl_make_current(NULL, NULL) && !st_api_get_current() && !kumo_gl_is_current(context) &&
          !kumo_gl_flush_front(context), 15);
    CHECK(kumo_gl_make_current(context, drawable), 16);
    kumo_gl_context_destroy(context);
    CHECK(!st_api_get_current() && !kumo_gl_context_create(display), 17);
    CHECK(kumo_gl_drawable_destroy(drawable), 18);
    CHECK(api->device_status(api->context) == KUMO_GPU_OK, 19);
    CHECK(kumo_gl_display_destroy(display), 20);
    say("GL1-e destroy: current context unbound, drawable and display released, session context recreation refused");
    return 0;
}
