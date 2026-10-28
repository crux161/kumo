/* SPDX-License-Identifier: MIT */
/*j643*/
/*j644*/
#ifndef KUMO_MESA_GPU_H
#define KUMO_MESA_GPU_H

#include <stddef.h>
#include <stdint.h>

/* Private versioned C boundary, mirrored by kumo_gpu::c. The opaque context is a
 * borrowed Rust Client, never an FD or kernel handle. It may be called serially,
 * synchronously within with_api only; do not retain, reenter, race or unwind it.
 * No handle or object ownership crosses the query-only G7 boundary. — KESTREL */
#define KUMO_GPU_API_VERSION 2u
#define KUMO_GPU_OK 0u
#define KUMO_GPU_BAD_ARGUMENT 1u
#define KUMO_GPU_INVALID_REQUEST 2u
#define KUMO_GPU_TRANSPORT 3u
#define KUMO_GPU_PROTOCOL 4u
#define KUMO_GPU_DEVICE_LOST 5u
#define KUMO_GPU_SERVER_ERROR 16u /* plus kumo_gpu::Status */

struct kumo_gpu_info {
    uint32_t gpu_id;
    uint32_t cs_features;
    uint64_t va_start; /* service limits, not full hardware MMU properties */
    uint64_t va_end;
    uint64_t max_buffer_bytes;
    uint64_t shader_present; /* attested shader core-ID mask */
};
struct kumo_gpu_api {
    uint32_t version;
    uint32_t bytes;
    void *context;
    uint32_t (*query)(void *context, struct kumo_gpu_info *out);
};

#define KUMO_MESA_INCOMPLETE 1u
#define KUMO_MESA_BAD_ABI 2u
#define KUMO_MESA_QUERY_FAILED 3u

/* Complete pan_kmod_dev_props groups still unavailable over GetInfo. See README.
 * This mask records missing information; zero-filled properties are never a device.
 * Unsupported BO/VM/group/sync operations are listed separately. — KESTREL */
#define KUMO_MESA_MISSING_VARIANT (UINT64_C(1) << 0)
#define KUMO_MESA_MISSING_TOPOLOGY (UINT64_C(1) << 1)
#define KUMO_MESA_MISSING_MEMORY (UINT64_C(1) << 2)
#define KUMO_MESA_MISSING_TEXTURE (UINT64_C(1) << 3)
#define KUMO_MESA_MISSING_THREADS (UINT64_C(1) << 4)
#define KUMO_MESA_MISSING_COMPRESSION (UINT64_C(1) << 5)
#define KUMO_MESA_MISSING_TIMESTAMP (UINT64_C(1) << 6)
#define KUMO_MESA_MISSING_PRIORITIES (UINT64_C(1) << 7)
#define KUMO_MESA_MISSING_BO_VM_FLAGS (UINT64_C(1) << 8)
#define KUMO_MESA_MISSING_COHERENCY_PAGES (UINT64_C(1) << 9)
#define KUMO_MESA_MISSING_ALL ((UINT64_C(1) << 10) - 1)

struct kumo_mesa_report {
    struct kumo_gpu_info info;
    uint64_t missing_properties;
    uint32_t query_status;
    uint32_t reserved;
    uint32_t core_count;
    uint32_t core_id_range;
};

uint32_t kumo_mesa_probe(const struct kumo_gpu_api *api,
                         struct kumo_mesa_report *out);

_Static_assert(sizeof(struct kumo_gpu_info) == 40, "query layout");
_Static_assert(offsetof(struct kumo_gpu_info, va_start) == 8, "query alignment");
_Static_assert(sizeof(struct kumo_mesa_report) == 64, "report layout");
#if UINTPTR_MAX == UINT64_MAX
_Static_assert(sizeof(struct kumo_gpu_api) == 24, "64-bit callback layout");
_Static_assert(offsetof(struct kumo_gpu_api, query) == 16, "callback alignment");
#endif
#endif
