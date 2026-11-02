/* SPDX-License-Identifier: MIT */
/*j744*/
/*j745*/
/*j751*/
/*j757*/
/*j760*/
#pragma once
#include "kumo_gpu.h"
#include "kmod/pan_kmod.h"

/* Native BO storage embeds the pinned Mesa layout. The device factory initializes
 * base.dev/refcounts; the BO-only helpers leave base.dev/exclusive_vm null. The owner
 * token is borrowed for the API scope; exclusive_vm records a logical generation so
 * allocator address reuse cannot reassign an exclusive BO. — KESTREL */
struct kumo_mesa_bo {
    struct pan_kmod_bo base;
    void *owner;
    struct kumo_mesa_bo *next;
    uint32_t exclusive_vm;
};

uint32_t kumo_mesa_bo_alloc(const struct kumo_gpu_api *api, uint64_t size, uint32_t flags,
                             struct kumo_mesa_bo *out);
/* On failure retain the BO and retry only a recoverable error; terminal loss ends the
 * session. Rust releases only local claims at scope abandonment. — KESTREL */
uint32_t kumo_mesa_bo_free(const struct kumo_gpu_api *api, struct kumo_mesa_bo *bo);
uint32_t kumo_mesa_bo_mmap(const struct kumo_gpu_api *api, const struct kumo_mesa_bo *bo,
                            int prot, int flags, void *host_addr, void **out);
uint32_t kumo_mesa_bo_munmap(const struct kumo_gpu_api *api, const struct kumo_mesa_bo *bo);
/* GPU W^X policy, shared with Rust's immediate native bind operation. */
uint32_t kumo_mesa_bo_bind_flags(const struct kumo_mesa_bo *bo);

/* The explicit native factory selects the kumo backend without a DRM name or FD.
 * Its API/context and allocator must stay live until device destruction, all inside
 * with_device_api. A closed device/session is recreated only on a fresh grant. — KESTREL */
struct pan_kmod_dev *pan_kmod_kumo_dev_create(const struct kumo_gpu_api *api,
                                             const struct pan_kmod_allocator *allocator);
/* CS_FEATURES[7:0] + 1, cached from the factory's attested query. A foreign device returns
 * zero. The native Submit wrapper clobbers the last four registers. — KESTREL */
uint32_t pan_kmod_kumo_cs_reg_count(const struct pan_kmod_dev *dev);
void *pan_kmod_kumo_bo_mmap(struct pan_kmod_bo *bo, int prot, int flags, void *host_addr);
int pan_kmod_kumo_bo_munmap(struct pan_kmod_bo *bo);
/* The published device's original session/BO owner also owns all native sync metadata.
 * No DRM dispatch, FD, second Client, or kernel handle is exposed. — MILVUS */
uint32_t pan_kmod_kumo_native(struct pan_kmod_dev *dev,
                              const struct kumo_gpu_native_request *request,
                              struct kumo_gpu_native_reply *out);
/* G7q-c: TILER_HEAP_CREATE and _DESTROY for GL1's pan_csf.c and VK2's queue seams, through the
 * same owner. request->vm is the logical VM's handle. Known loss poisons the device, as with
 * every other native call. — TIMBERDOODLE */
uint32_t pan_kmod_kumo_heap_create(struct pan_kmod_dev *dev,
                                   const struct kumo_gpu_heap_request *request,
                                   struct kumo_gpu_heap_info *out);
uint32_t pan_kmod_kumo_heap_destroy(struct pan_kmod_dev *dev, uint32_t heap, uint64_t *bytes);
extern const struct pan_kmod_ops kumo_kmod_ops;
/* One property builder is shared by the report and native device publication. */
uint32_t kumo_mesa_query_properties(const struct kumo_gpu_api *api, struct kumo_mesa_report *out,
                                    struct pan_kmod_dev_props *complete);
