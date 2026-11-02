/* SPDX-License-Identifier: MIT */
/*j761*/
/*j762*/
/* GL1-c2: the actual Gallium context constructor owns native binary syncs, then stops at
 * an injected native group refusal. The test-only API observer gives real native IDs a
 * high word, translates them back at the callback boundary, and checks each released ID
 * is gone from Rust's metadata. This exercises Gallium's full-width storage and cleanup
 * without moving its private structs or changing the native ABI. — KESTREL */
#include "kumo_kmod.h"
#include "drm-uapi/panthor_drm.h"
#include "pipe/p_context.h"
#include "pipe/p_screen.h"
#include <stdio.h>
#include <string.h>

struct pipe_screen *panfrost_create_screen_kumo(struct pan_kmod_dev *kmod);
int __real_drmIoctl(int fd, unsigned long request, void *arg);
int __wrap_drmIoctl(int fd, unsigned long request, void *arg);

#define CHECK(test, number) do { if (!(test)) return (number); } while (0)
#define HIGH_ID (UINT64_C(1) << 32)
static uint32_t (*real_native)(void *, const struct kumo_gpu_native_request *, struct kumo_gpu_native_reply *);
static struct {
    uint32_t creates, drops, fail_create, group_calls, drm_calls;
    bool faulty;
    struct { uint64_t original, exposed; } ids[2];
} observed;

/* Only the proof ELF wraps this symbol; no ioctl belongs in the native context path.
 * Still delegate to the real refusal, never translate one into a native call. — KESTREL */
int __wrap_drmIoctl(int fd, unsigned long request, void *arg)
{
    observed.faulty = true;
    ++observed.drm_calls;
    return __real_drmIoctl(fd, request, arg);
}

static uint32_t observe_native(void *context, const struct kumo_gpu_native_request *request,
                                struct kumo_gpu_native_reply *out)
{
    struct kumo_gpu_native_request translated = *request;
    uint32_t index = 0;
    if (request->operation == KUMO_NATIVE_CREATE_BINARY) {
        ++observed.creates;
        if (observed.creates == observed.fail_create) {
            *out = (struct kumo_gpu_native_reply){0};
            return KUMO_GPU_NO_MEMORY;
        }
        index = observed.creates - 1;
        if (index >= 2 || request->value != (index == 0)) {
            observed.faulty = true;
            return KUMO_GPU_PROTOCOL;
        }
    } else if (request->operation == KUMO_NATIVE_GROUP_CREATE) {
        ++observed.group_calls;
        *out = (struct kumo_gpu_native_reply){0};
        return KUMO_GPU_NO_MEMORY;
    } else if (request->operation == KUMO_NATIVE_DROP) {
        for (; index < 2; ++index)
            if (observed.ids[index].exposed == request->id) break;
        if (index == 2 || !(request->id & HIGH_ID)) {
            observed.faulty = true;
            return KUMO_GPU_UNKNOWN_OBJECT;
        }
        translated.id = observed.ids[index].original;
    } else {
        observed.faulty = true;
        return KUMO_GPU_INVALID_REQUEST;
    }
    uint32_t status = real_native(context, &translated, out);
    if (status != KUMO_GPU_OK) { observed.faulty = true; return status; }
    if (request->operation == KUMO_NATIVE_CREATE_BINARY) {
        if (!out->id || out->id >= HIGH_ID) {
            observed.faulty = true;
            return KUMO_GPU_PROTOCOL;
        }
        observed.ids[index].original = out->id;
        out->id |= HIGH_ID;
        observed.ids[index].exposed = out->id;
    } else {
        ++observed.drops;
        observed.ids[index].exposed = 0;
    }
    const struct kumo_gpu_native_request pending = {
        .operation = KUMO_NATIVE_PENDING, .id = observed.ids[index].original,
    };
    struct kumo_gpu_native_reply state;
    uint32_t checked = real_native(context, &pending, &state);
    if (request->operation == KUMO_NATIVE_DROP) {
        if (checked != KUMO_GPU_UNKNOWN_OBJECT) observed.faulty = true;
    } else if (checked != KUMO_GPU_OK ||
               state.state != (index == 0 ? KUMO_NATIVE_AVAILABLE : KUMO_NATIVE_UNAVAILABLE)) {
        observed.faulty = true;
    }
    return status;
}

unsigned kumo_gl_context_sync_cases(const struct kumo_gpu_api *api)
{
    struct kumo_gpu_api tracked = *api;
    real_native = api->native;
    tracked.native = observe_native;
    memset(&observed, 0, sizeof observed);
    struct pan_kmod_dev *kmod = pan_kmod_kumo_dev_create(&tracked, NULL);
    CHECK(kmod, 1);
    uint64_t id = UINT64_MAX;
    CHECK(pan_kmod_kumo_sync_create(NULL, false, &id) && !id, 2);
    CHECK(pan_kmod_kumo_sync_create(kmod, false, NULL), 3);
    pan_kmod_kumo_sync_destroy(kmod, 0);
    CHECK(!observed.creates && !observed.drops, 4);
    struct pipe_screen *screen = panfrost_create_screen_kumo(kmod);
    CHECK(screen, 5);
    for (unsigned attempt = 0; attempt < 4; ++attempt) {
        memset(&observed, 0, sizeof observed);
        observed.fail_create = attempt < 2 ? attempt + 1 : 0;
        struct pipe_context *pipe = screen->context_create(screen, NULL, 0);
        CHECK(!pipe && !observed.faulty, 10 + attempt);
        CHECK(observed.creates == (attempt == 0 ? 1 : 2), 20 + attempt);
        CHECK(observed.drops == (attempt == 0 ? 0 : attempt == 1 ? 1 : 2), 30 + attempt);
        CHECK(!observed.ids[0].exposed && !observed.ids[1].exposed, 40 + attempt);
        CHECK(observed.group_calls == (attempt >= 2) && !observed.drm_calls, 50 + attempt);
    }
    screen->destroy(screen);
    puts("GL1-c2 native context sync green: full-width binary IDs, signaled and empty payloads, first/second-create refusal, repeated group refusal, every sync released");
    return 0;
}
