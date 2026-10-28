/* SPDX-License-Identifier: MIT */
/*j643*/
/*j644*/
/*j645*/
/*j646*/
/*j647*/
/*j650*/
#ifndef KUMO_MESA_GPU_H
#define KUMO_MESA_GPU_H

#include <stddef.h>
#include <stdint.h>

/* Private versioned C boundary, mirrored by kumo_gpu::c. The opaque context is a
 * borrowed Rust Client, never an FD or kernel handle. It may be called serially,
 * synchronously within with_api only; do not retain, reenter, race or unwind it.
 * No handle or object ownership crosses the query-only G7 boundary. — KESTREL */
#define KUMO_GPU_API_VERSION 6u
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
    uint64_t va_start; /* this service's window, not the hardware MMU range */
    uint64_t va_end;
    uint64_t max_buffer_bytes;
    uint64_t shader_present; /* attested shader core-ID mask */
    uint32_t mmu_features;   /* attested MMU_FEATURES; [7:0] is the VA width — SAKER */
    uint32_t tiler_features; /* attested TILER_FEATURES, or 0 when absent — SAKER */
    uint32_t texture_features_0; /* attested TEXTURE_FEATURES[0] only, or 0 when absent — SAKER */
    uint32_t core_features; /* attested CORE_FEATURES, verbatim; [7:0] is the variant. The
                             * query is required, but a zero word is a valid value: the opi5
                             * measures exactly 0x00000000. — TIMBERDOODLE */
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
/* The attested hardware VA range cannot hold this service's own window. — SAKER */
#define KUMO_MESA_VA_RANGE 4u

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
    /* Derived by upstream pan_kmod_dev_query_user_va_range from the attested
     * mmu_features, never computed by KUMO. — SAKER */
    uint64_t va_range_start;
    uint64_t va_range_size;
    /* Derived by upstream pan_query_tiler_features from the attested word, and left zero
     * when that word is absent. Appended last so every earlier field keeps its version3
     * offset. — SAKER */
    uint32_t tiler_bin_size;
    uint32_t tiler_max_levels;
    /* Derived by upstream pan_query_compressed_formats and pan_query_afrc from the attested
     * TEXTURE_FEATURES[0] and GPU_ID, and left zero when that word is absent. — SAKER */
    uint32_t compressed_formats;
    uint32_t has_afrc;
    /* The variant is derived with upstream's own expression (`core_features & 0xff`,
     * panthor_kmod.c) and model_resolved records whether pan_get_model found a table entry
     * for (gpu_id, variant). Appended last so every earlier field keeps its version5 offset.
     * — TIMBERDOODLE */
    uint32_t gpu_variant;
    uint32_t model_resolved;
};

uint32_t kumo_mesa_probe(const struct kumo_gpu_api *api,
                         struct kumo_mesa_report *out);

_Static_assert(sizeof(struct kumo_gpu_info) == 56, "query layout");
_Static_assert(offsetof(struct kumo_gpu_info, va_start) == 8, "query alignment");
_Static_assert(offsetof(struct kumo_gpu_info, mmu_features) == 40, "query tail alignment");
_Static_assert(offsetof(struct kumo_gpu_info, tiler_features) == 44, "query tail alignment");
_Static_assert(offsetof(struct kumo_gpu_info, texture_features_0) == 48, "query tail alignment");
_Static_assert(offsetof(struct kumo_gpu_info, core_features) == 52, "query tail alignment");
_Static_assert(sizeof(struct kumo_mesa_report) == 120, "report layout");
#if UINTPTR_MAX == UINT64_MAX
_Static_assert(sizeof(struct kumo_gpu_api) == 24, "64-bit callback layout");
_Static_assert(offsetof(struct kumo_gpu_api, query) == 16, "callback alignment");
#endif
#endif
