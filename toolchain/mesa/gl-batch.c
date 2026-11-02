/* SPDX-License-Identifier: MIT */
/*j765*/
/* GL1-d: Gallium's batches and fences over the native device. The CPU fills a linear 64x64
 * RGBA8 render target with a pattern, pipe->clear replaces it, and the flush returns a fence
 * that is polled once and then waited natively. A read map follows, whose bo_wait checks the
 * texture's write point, then the group's state. On metal the GPU's clear color must replace
 * every pixel. QEMU's modeled service accepts and completes the submission without running
 * it, so the pattern must survive there, and its producer holds the first completion so the
 * poll times out. Teardown frees every BO through Gallium's deferred unmap. Shared by the
 * QEMU device smoke (mode 20, which passes `control`) and the metal client (proof 8, which
 * passes NULL). — TIMBERDOODLE */
#include "kumo_kmod.h"
#include "pipe/p_context.h"
#include "pipe/p_defines.h"
#include "pipe/p_screen.h"
#include "pipe/p_state.h"
#include "util/os_time.h"
#include "util/u_atomic.h"
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

/* Declared by kumo-build.patch's pan_public.h; the host's header tree is unpatched. */
struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);

#define CHECK(test, number) do { if (!(test)) return (number); } while (0)
#define SIZE 64
#define PATTERN 0xdeadbeefu
/* (0.2, 0.4, 0.6, 1.0) packed as RGBA8 and read as one little-endian word. */
#define CLEARED 0xff996633u
/* The modeled producer's controls, as in device-smoke.rs. */
#define HOLD 12u
#define RELEASE 13u

static void report(void (*say)(const char *), const char *format, ...)
{
    char line[240];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof line, format, args);
    va_end(args);
    say(line);
}

/* Write `value` to every pixel, or count the pixels that differ from it. */
static unsigned pixels(struct pipe_context *pipe, struct pipe_resource *texture, unsigned usage,
                       uint32_t value, unsigned *differ)
{
    const struct pipe_box box = {.width = SIZE, .height = SIZE, .depth = 1};
    struct pipe_transfer *transfer = NULL;
    uint8_t *map = pipe->texture_map(pipe, texture, 0, usage, &box, &transfer);
    if (!map || !transfer || transfer->stride < SIZE * 4) return 1;
    unsigned count = 0;
    for (unsigned y = 0; y < SIZE; ++y) {
        uint32_t *row = (uint32_t *)(map + (size_t)y * transfer->stride);
        for (unsigned x = 0; x < SIZE; ++x) {
            if (usage & PIPE_MAP_WRITE) row[x] = value;
            else count += row[x] != value;
        }
    }
    pipe->texture_unmap(pipe, transfer);
    if (differ) *differ = count;
    return 0;
}

unsigned kumo_gl_batch_cases(const struct kumo_gpu_api *api, void (*say)(const char *),
                             unsigned (*control)(unsigned))
{
    struct pan_kmod_dev *kmod = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(kmod, 1);
    struct pipe_screen *screen = panfrost_create_screen_kumo(kmod);
    CHECK(screen, 2);
    CHECK(!screen->caps.native_fence_fd, 3);
    struct pipe_context *pipe = screen->context_create(screen, NULL, 0);
    CHECK(pipe && api->device_status(api->context) == KUMO_GPU_OK, 4);
    const struct pipe_resource template = {
        .target = PIPE_TEXTURE_2D,
        .format = PIPE_FORMAT_R8G8B8A8_UNORM,
        .width0 = SIZE,
        .height0 = SIZE,
        .depth0 = 1,
        .array_size = 1,
        .bind = PIPE_BIND_RENDER_TARGET | PIPE_BIND_LINEAR,
        .usage = PIPE_USAGE_DEFAULT,
    };
    struct pipe_resource *texture = screen->resource_create(screen, &template);
    CHECK(texture, 5);
    CHECK(!pixels(pipe, texture, PIPE_MAP_WRITE, PATTERN, NULL), 6);
    say("GL1-d target: a linear 64x64 RGBA8 render target, filled from the CPU with 0xdeadbeef");

    struct pipe_framebuffer_state framebuffer = {
        .width = SIZE,
        .height = SIZE,
        .nr_cbufs = 1,
    };
    framebuffer.cbufs[0] = (struct pipe_surface){
        .format = PIPE_FORMAT_R8G8B8A8_UNORM,
        .texture = texture,
    };
    pipe->set_framebuffer_state(pipe, &framebuffer);
    const union pipe_color_union color = {.f = {0.2f, 0.4f, 0.6f, 1.0f}};
    pipe->clear(pipe, PIPE_CLEAR_COLOR0, 0xf, 0, NULL, &color, 0.0, 0);
    struct pipe_fence_handle *fence = NULL;
    pipe->flush(pipe, &fence, 0);
    CHECK(fence && api->device_status(api->context) == KUMO_GPU_OK, 7);
    say("GL1-d batch: pipe->clear flushed as one native submission; the fence snapshots the context binary");

    /* The poll's ARM leaves a wake behind when the work is pending; finish gives it back. */
    if (control) CHECK(!control(HOLD), 8);
    const bool polled = screen->fence_finish(screen, NULL, fence, 0);
    if (control) CHECK(!polled && !control(RELEASE), 9);
    CHECK(api->device_status(api->context) == KUMO_GPU_OK, 10);
    report(say, "GL1-d poll: a zero-timeout fence_finish found the batch %s",
           polled ? "already complete" : "still pending, and returned its wake");
    CHECK(screen->fence_finish(screen, NULL, fence, OS_TIMEOUT_INFINITE), 11);
    CHECK(screen->fence_finish(screen, NULL, fence, 0), 12);
    /* A completed poll leaves the fence signaled, so the infinite wait sends nothing. */
    report(say, "GL1-d fence: fence_finish completed natively, %s",
           polled ? "from the poll's ARM" : "by ARM, WAIT and ARM again");

    unsigned differ = SIZE * SIZE;
    const uint32_t expected = control ? PATTERN : CLEARED;
    CHECK(!pixels(pipe, texture, PIPE_MAP_READ, expected, &differ), 13);
    report(say, "GL1-d readback: %u of %u pixels differ from 0x%08x (%s)", differ, SIZE * SIZE,
           expected, control ? "the modeled service runs no GPU" : "the GPU's clear color");
    CHECK(!differ, 14);
    CHECK(pipe->get_device_reset_status(pipe) == PIPE_NO_RESET, 15);
    say("GL1-d state: native GROUP_STATE reports the group usable, so no reset");

    screen->fence_reference(screen, &fence, NULL);
    framebuffer.nr_cbufs = 0;
    framebuffer.cbufs[0] = (struct pipe_surface){0};
    pipe->set_framebuffer_state(pipe, &framebuffer);
    /* pipe_resource_reference's last step; u_inlines.h is not on the host's include path. */
    if (p_atomic_dec_zero(&texture->reference.count))
        screen->resource_destroy(screen, texture);
    pipe->destroy(pipe);
    CHECK(api->device_status(api->context) == KUMO_GPU_OK, 16);
    screen->destroy(screen);
    say("GL1-d destroy: fence, texture, context and screen are gone; every BO left through a deferred unmap");
    return 0;
}
