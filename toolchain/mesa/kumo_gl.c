/* SPDX-License-Identifier: MIT */
/*j768*/
/* The state-tracker frontend follows Mesa's hgl structure. All window-system
 * policy stays in the caller's callback; resources stay in Gallium. — KESTREL */
#include "kumo_gl.h"
#include "state_tracker/st_context.h"
#include <stdlib.h>

struct kumo_gl_display {
    struct pipe_frontend_screen frontend;
    struct st_visual visual;
    struct kumo_gl_context *context;
    struct kumo_gl_drawable *drawable;
    bool context_attempted;
};
struct kumo_gl_context {
    struct kumo_gl_display *display;
    struct st_context *st;
    struct kumo_gl_drawable *current;
};
struct kumo_gl_drawable {
    struct pipe_frontend_drawable frontend;
    struct kumo_gl_display *display;
    struct pipe_resource *color, *depth;
    kumo_gl_present present;
    void *user;
};
static struct kumo_gl_display *active_display;
static uint32_t next_drawable_id = 1;

static int manager_param(struct pipe_frontend_screen *screen, enum st_manager_param param)
{
    (void)screen;
    return param == ST_MANAGER_BROKEN_INVALIDATE;
}

static bool flush_wait(struct st_context *st)
{
    struct pipe_fence_handle *fence = NULL;
    st_context_flush(st, 0, &fence, NULL, NULL);
    /* A missing fence cannot attest completion. Native loss is contained by the
     * bridge before any BO backing can be freed or presented. — KESTREL */
    bool complete = fence && st->screen->fence_finish(st->screen, st->pipe, fence,
                                                      OS_TIMEOUT_INFINITE);
    st->screen->fence_reference(st->screen, &fence, NULL);
    return complete;
}

static bool validate(struct st_context *st, struct pipe_frontend_drawable *frontend,
                     const enum st_attachment_type *attachments, unsigned count,
                     struct pipe_resource **out, struct pipe_resource **resolve)
{
    struct kumo_gl_drawable *drawable = (struct kumo_gl_drawable *)frontend;
    if (st->frontend_screen != frontend->fscreen || count > ST_ATTACHMENT_COUNT)
        return false;
    /* Validate the complete request before returning any references. — KESTREL */
    for (unsigned i = 0; i < count; ++i)
        if (attachments[i] != ST_ATTACHMENT_FRONT_LEFT &&
            attachments[i] != ST_ATTACHMENT_DEPTH_STENCIL)
            return false;
    if (resolve)
        *resolve = NULL;
    for (unsigned i = 0; i < count; ++i)
        pipe_resource_reference(&out[i], attachments[i] == ST_ATTACHMENT_FRONT_LEFT ?
                                drawable->color : drawable->depth);
    return true;
}

static bool flush_front(struct st_context *st, struct pipe_frontend_drawable *frontend,
                        enum st_attachment_type attachment)
{
    struct kumo_gl_drawable *drawable = (struct kumo_gl_drawable *)frontend;
    if (attachment != ST_ATTACHMENT_FRONT_LEFT || st->frontend_screen != frontend->fscreen ||
        !flush_wait(st))
        return false;
    return !drawable->present || drawable->present(drawable->user, drawable->color);
}

struct kumo_gl_display *kumo_gl_display_create(struct pipe_screen *screen)
{
    if (!screen || active_display ||
        !screen->is_format_supported(screen, PIPE_FORMAT_R8G8B8A8_UNORM, PIPE_TEXTURE_2D,
                                     0, 0, PIPE_BIND_RENDER_TARGET | PIPE_BIND_SAMPLER_VIEW) ||
        !screen->is_format_supported(screen, PIPE_FORMAT_Z24_UNORM_S8_UINT, PIPE_TEXTURE_2D,
                                     0, 0, PIPE_BIND_DEPTH_STENCIL))
        return NULL;
    struct kumo_gl_display *display = calloc(1, sizeof *display);
    if (!display)
        return NULL;
    display->frontend.screen = screen;
    display->frontend.get_param = manager_param;
    display->visual = (struct st_visual) {
        .buffer_mask = ST_ATTACHMENT_FRONT_LEFT_MASK | ST_ATTACHMENT_DEPTH_STENCIL_MASK,
        .color_format = PIPE_FORMAT_R8G8B8A8_UNORM,
        .depth_stencil_format = PIPE_FORMAT_Z24_UNORM_S8_UINT,
    };
    active_display = display;
    return display;
}

bool kumo_gl_display_destroy(struct kumo_gl_display *display)
{
    if (!display || display != active_display || display->context || display->drawable)
        return false;
    st_screen_destroy(&display->frontend);
    display->frontend.screen->destroy(display->frontend.screen);
    active_display = NULL;
    free(display);
    return true;
}

struct kumo_gl_context *kumo_gl_context_create(struct kumo_gl_display *display)
{
    if (!display || display != active_display || display->context_attempted)
        return NULL;
    struct kumo_gl_context *context = calloc(1, sizeof *context);
    if (!context)
        return NULL;
    const struct st_context_attribs attributes = {
        .profile = API_OPENGLES2, .major = 2, .minor = 0, .visual = display->visual,
    };
    enum st_context_error error;
    /* Even a failed constructor may have consumed the session's sole group.
     * Reconnect with a fresh display/session instead of retrying it. — KESTREL */
    display->context_attempted = true;
    context->st = st_api_create_context(&display->frontend, &attributes, &error, NULL);
    if (!context->st) {
        free(context);
        return NULL;
    }
    /* Neither hgl-style creation nor this frontend enables glthread. Refuse a
     * threaded pipe too: GL1 owns no background-thread capabilities. — KESTREL */
    if (context->st->ctx->GLThread.enabled || context->st->is_threaded_context) {
        st_destroy_context(context->st);
        free(context);
        return NULL;
    }
    context->display = display;
    context->st->frontend_context = context;
    display->context = context;
    return context;
}

void kumo_gl_context_destroy(struct kumo_gl_context *context)
{
    if (!context)
        return;
    flush_wait(context->st);
    if (kumo_gl_is_current(context))
        kumo_gl_make_current(NULL, NULL);
    st_destroy_context(context->st);
    context->display->context = NULL;
    free(context);
}

struct kumo_gl_drawable *kumo_gl_drawable_create(struct kumo_gl_display *display,
                                               unsigned width, unsigned height,
                                               kumo_gl_present present, void *user)
{
    if (!display || display != active_display || display->drawable || !next_drawable_id ||
        !width || !height || width > display->frontend.screen->caps.max_texture_2d_size ||
        height > display->frontend.screen->caps.max_texture_2d_size)
        return NULL;
    struct kumo_gl_drawable *drawable = calloc(1, sizeof *drawable);
    if (!drawable)
        return NULL;
    drawable->display = display;
    drawable->present = present;
    drawable->user = user;
    struct pipe_resource template = {
        .target = PIPE_TEXTURE_2D, .format = display->visual.color_format,
        .width0 = width, .height0 = height, .depth0 = 1, .array_size = 1,
        .bind = PIPE_BIND_RENDER_TARGET | PIPE_BIND_SAMPLER_VIEW,
    };
    struct pipe_screen *screen = display->frontend.screen;
    drawable->color = screen->resource_create(screen, &template);
    template.format = display->visual.depth_stencil_format;
    template.bind = PIPE_BIND_DEPTH_STENCIL;
    drawable->depth = screen->resource_create(screen, &template);
    if (!drawable->color || !drawable->depth) {
        pipe_resource_reference(&drawable->color, NULL);
        pipe_resource_reference(&drawable->depth, NULL);
        free(drawable);
        return NULL;
    }
    drawable->frontend = (struct pipe_frontend_drawable) {
        .stamp = 1, .ID = next_drawable_id++, .fscreen = &display->frontend,
        .visual = &display->visual, .validate = validate, .flush_front = flush_front,
    };
    display->drawable = drawable;
    return drawable;
}

bool kumo_gl_drawable_destroy(struct kumo_gl_drawable *drawable)
{
    if (!drawable || (drawable->display->context &&
                      drawable->display->context->current == drawable))
        return false;
    if (drawable->display->frontend.st_screen)
        st_api_destroy_drawable(&drawable->frontend);
    pipe_resource_reference(&drawable->color, NULL);
    pipe_resource_reference(&drawable->depth, NULL);
    drawable->display->drawable = NULL;
    free(drawable);
    return true;
}

struct pipe_resource *kumo_gl_drawable_resource(struct kumo_gl_drawable *drawable,
                                              bool depth_stencil)
{
    return drawable ? (depth_stencil ? drawable->depth : drawable->color) : NULL;
}

bool kumo_gl_make_current(struct kumo_gl_context *context, struct kumo_gl_drawable *drawable)
{
    if (!context && !drawable) {
        if (!st_api_make_current(NULL, NULL, NULL))
            return false;
        if (active_display && active_display->context)
            active_display->context->current = NULL;
        return true;
    }
    if (!context || !drawable || context->display != drawable->display)
        return false;
    if (!st_api_make_current(context->st, &drawable->frontend, &drawable->frontend))
        return false;
    context->current = drawable;
    return true;
}

bool kumo_gl_is_current(struct kumo_gl_context *context)
{
    return context && st_api_get_current() == context->st;
}

bool kumo_gl_flush_front(struct kumo_gl_context *context)
{
    return kumo_gl_is_current(context) && context->current &&
           flush_front(context->st, &context->current->frontend, ST_ATTACHMENT_FRONT_LEFT);
}
