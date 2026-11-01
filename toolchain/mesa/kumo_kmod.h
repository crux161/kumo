/* SPDX-License-Identifier: MIT */
/*j744*/
#pragma once
#include "kumo_gpu.h"
#include "kmod/pan_kmod.h"

/* Unpublished BO storage. A later device backend supplies base.dev/exclusive_vm.
 * Use these native operations only; generic Mesa device helpers require G7o-d.
 * The owner token is borrowed for the same callback scope as the API. — KESTREL */
struct kumo_mesa_bo {
    struct pan_kmod_bo base;
    void *owner;
};

uint32_t kumo_mesa_bo_alloc(const struct kumo_gpu_api *api, uint64_t size, uint32_t flags,
                             struct kumo_mesa_bo *out);
/* On failure retain the BO and retry only a recoverable error; terminal loss ends the
 * session. Rust releases only local claims at scope abandonment. — KESTREL */
uint32_t kumo_mesa_bo_free(const struct kumo_gpu_api *api, struct kumo_mesa_bo *bo);
uint32_t kumo_mesa_bo_mmap(const struct kumo_gpu_api *api, const struct kumo_mesa_bo *bo,
                            int prot, int flags, void *host_addr, void **out);
uint32_t kumo_mesa_bo_munmap(const struct kumo_gpu_api *api, const struct kumo_mesa_bo *bo);
/* GPU W^X policy to be used by G7o-d's binding operation. */
uint32_t kumo_mesa_bo_bind_flags(const struct kumo_mesa_bo *bo);
