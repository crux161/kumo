/* SPDX-License-Identifier: MIT */
/*j744*/
/*j745*/
#include "kumo_kmod.h"
#include <sys/mman.h>

#define CHECK(test, number) do { if (!(test)) return (number); } while (0)

static unsigned
check_mapping(const struct kumo_gpu_api *api, struct kumo_mesa_bo *bo)
{
    void *pointer = (void *)1;
    CHECK(kumo_mesa_bo_mmap(api, bo, PROT_READ | PROT_WRITE | PROT_EXEC,
                             MAP_SHARED, NULL, &pointer) == KUMO_GPU_INVALID_REQUEST && !pointer, 10);
    CHECK(kumo_mesa_bo_mmap(api, bo, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE, NULL, &pointer) == KUMO_GPU_INVALID_REQUEST && !pointer, 11);
    CHECK(kumo_mesa_bo_mmap(api, bo, PROT_READ | PROT_WRITE,
                             MAP_SHARED, (void *)0x1000, &pointer) == KUMO_GPU_INVALID_REQUEST && !pointer, 12);
    uint32_t status = kumo_mesa_bo_mmap(api, bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL, &pointer);
    if (bo->base.flags & PAN_KMOD_BO_FLAG_NO_MMAP) {
        CHECK(status == KUMO_GPU_INVALID_REQUEST && !pointer, 13);
        return 0;
    }
    CHECK(status == KUMO_GPU_OK && pointer, 14);
    void *same = NULL;
    CHECK(kumo_mesa_bo_mmap(api, bo, PROT_READ | PROT_WRITE,
                             MAP_SHARED, NULL, &same) == KUMO_GPU_OK && same == pointer, 15);
    for (uint64_t offset = 0; offset < bo->base.size; offset += 4096) {
        volatile uint32_t *first = (volatile uint32_t *)((char *)pointer + offset);
        volatile uint32_t *last = (volatile uint32_t *)((char *)pointer + offset + 4092);
        CHECK(*first == 0 && *last == 0, 16);
        *first = (uint32_t)offset ^ bo->base.handle;
        *last = ~*first;
    }
    CHECK(kumo_mesa_bo_munmap(api, bo) == KUMO_GPU_OK, 17);
    CHECK(kumo_mesa_bo_munmap(api, bo) == KUMO_GPU_OK, 18);
    CHECK(kumo_mesa_bo_mmap(api, bo, PROT_READ | PROT_WRITE,
                             MAP_SHARED, NULL, &pointer) == KUMO_GPU_OK && pointer, 19);
    for (uint64_t offset = 0; offset < bo->base.size; offset += 4096) {
        volatile uint32_t *first = (volatile uint32_t *)((char *)pointer + offset);
        volatile uint32_t *last = (volatile uint32_t *)((char *)pointer + offset + 4092);
        CHECK(*first == ((uint32_t)offset ^ bo->base.handle) && *last == ~*first, 20);
    }
    return 0;
}

unsigned
kumo_mesa_bo_proof(const struct kumo_gpu_api *api)
{
    const uint32_t forbidden[] = {
        PAN_KMOD_BO_FLAG_ALLOC_ON_FAULT, PAN_KMOD_BO_FLAG_EXPORTED,
        PAN_KMOD_BO_FLAG_IMPORTED, PAN_KMOD_BO_FLAG_WB_MMAP,
        PAN_KMOD_BO_FLAG_IO_COHERENT, 1u << 31,
    };
    struct kumo_mesa_bo bo = {.base.handle = 0xfeed};
    struct kumo_gpu_api old = *api;
    old.version = 11;
    CHECK(kumo_mesa_bo_alloc(&old, 1, 0, &bo) == KUMO_GPU_BAD_ARGUMENT && bo.base.handle == 0xfeed, 1);
    old = *api;
    old.buffer_destroy = NULL;
    CHECK(kumo_mesa_bo_alloc(&old, 1, 0, &bo) == KUMO_GPU_BAD_ARGUMENT && bo.base.handle == 0xfeed, 2);
    CHECK(kumo_mesa_bo_alloc(api, 1, 0, NULL) == KUMO_GPU_BAD_ARGUMENT, 3);
    for (unsigned i = 0; i < sizeof(forbidden) / sizeof(forbidden[0]); ++i)
        CHECK(kumo_mesa_bo_alloc(api, 1, forbidden[i], &bo) == KUMO_GPU_INVALID_REQUEST && bo.base.handle == 0xfeed, 4);
    CHECK(kumo_mesa_bo_alloc(api, 0, 0, &bo) == KUMO_GPU_INVALID_REQUEST && bo.base.handle == 0xfeed, 5);
    CHECK(kumo_mesa_bo_alloc(api, UINT64_MAX, 0, &bo) == KUMO_GPU_INVALID_REQUEST && bo.base.handle == 0xfeed, 6);
    CHECK(kumo_mesa_bo_alloc(api, UINT64_C(64) * 1024 * 1024 + 1, 0, &bo) == KUMO_GPU_INVALID_REQUEST, 7);
    for (unsigned combo = 0; combo < 8; ++combo) {
        uint32_t flags = (combo & 1 ? PAN_KMOD_BO_FLAG_EXECUTABLE : 0) |
                         (combo & 2 ? PAN_KMOD_BO_FLAG_NO_MMAP : 0) |
                         (combo & 4 ? PAN_KMOD_BO_FLAG_GPU_UNCACHED : 0);
        CHECK(kumo_mesa_bo_alloc(api, 4097, flags, &bo) == KUMO_GPU_OK, 8);
        CHECK(bo.base.handle == combo + 1 && bo.base.flags == flags && bo.base.size == 8192 &&
              bo.base.refcnt == 1 && !bo.base.dev && !bo.base.exclusive_vm, 9);
        CHECK(kumo_mesa_bo_bind_flags(&bo) == ((combo & 1 ? 1u : 2u) | (combo & 4 ? 4u : 0)), 21);
        old = *api;
        old.context = (void *)1;
        CHECK(kumo_mesa_bo_free(&old, &bo) == KUMO_GPU_BAD_ARGUMENT, 22);
        unsigned error = check_mapping(api, &bo);
        if (error) return error;
        struct kumo_mesa_bo stale = bo;
        CHECK(kumo_mesa_bo_free(api, &bo) == KUMO_GPU_OK && !bo.base.handle && !bo.owner, 23);
        CHECK(kumo_mesa_bo_free(api, &bo) == KUMO_GPU_BAD_ARGUMENT, 24);
        CHECK(kumo_mesa_bo_free(api, &stale) == KUMO_GPU_UNKNOWN_BUFFER, 25);
    }
    /* A real C allocation beyond the old 2 MiB limit, with non-page-aligned input.
     * Every mapped page and both ends are checked before and after remapping. — KESTREL */
    CHECK(kumo_mesa_bo_alloc(api, 4 * 1024 * 1024 + 1,
                              PAN_KMOD_BO_FLAG_EXECUTABLE | PAN_KMOD_BO_FLAG_GPU_UNCACHED, &bo) == KUMO_GPU_OK, 26);
    CHECK(bo.base.size == 4 * 1024 * 1024 + 4096 && bo.base.handle == 9, 27);
    unsigned error = check_mapping(api, &bo);
    if (error) return error;
    CHECK(kumo_mesa_bo_free(api, &bo) == KUMO_GPU_OK, 28);
    return 0;
}
