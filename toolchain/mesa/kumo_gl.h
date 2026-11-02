/* SPDX-License-Identifier: MIT */
/*j768*/
#ifndef KUMO_GL_H
#define KUMO_GL_H

#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

struct pipe_screen;
struct pipe_resource;
struct kumo_gl_display;
struct kumo_gl_context;
struct kumo_gl_drawable;

/* GL1: one display and one GLES2 context per process, called from one thread.
 * A display owns its screen only after create succeeds. A context cannot be
 * recreated on that display: its native group lasts for the session. — KESTREL */
struct kumo_gl_display *kumo_gl_display_create(struct pipe_screen *screen);
/* Refuses while its context or drawable exists. */
bool kumo_gl_display_destroy(struct kumo_gl_display *display);
struct kumo_gl_context *kumo_gl_context_create(struct kumo_gl_display *display);
void kumo_gl_context_destroy(struct kumo_gl_context *context);

/* The optional callback receives the completed color resource, borrowed until
 * return. It supplies presentation policy; the frontend has no display/device
 * authority. Callbacks must not re-enter this API. NULL means offscreen. — KESTREL */
typedef bool (*kumo_gl_present)(void *user, struct pipe_resource *color);
struct kumo_gl_drawable *kumo_gl_drawable_create(struct kumo_gl_display *display,
                                               unsigned width, unsigned height,
                                               kumo_gl_present present, void *user);
/* Refuses while current. Resources are RGBA8 color and Z24/S8 depth/stencil. */
bool kumo_gl_drawable_destroy(struct kumo_gl_drawable *drawable);
/* Borrowed resources; depth_stencil selects depth/stencil rather than color. */
struct pipe_resource *kumo_gl_drawable_resource(struct kumo_gl_drawable *drawable,
                                              bool depth_stencil);

/* Both NULL unbinds. Otherwise both must belong to the same display. */
bool kumo_gl_make_current(struct kumo_gl_context *context, struct kumo_gl_drawable *drawable);
bool kumo_gl_is_current(struct kumo_gl_context *context);
/* Flush and observe the native fence before calling the presentation callback. */
bool kumo_gl_flush_front(struct kumo_gl_context *context);

#ifdef __cplusplus
}
#endif
#endif
