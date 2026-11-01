/* SPDX-License-Identifier: MIT */
/*j650*/
/*j651*/
/*j652*/
/*j653*/
/*j742*/
#ifndef KUMO_MESA_GPU_H
#define KUMO_MESA_GPU_H

#include <stddef.h>
#include <stdint.h>

/* Private versioned C boundary, mirrored by kumo_gpu::c. The opaque context is a
 * borrowed Rust Client, never an FD or kernel handle. It may be called serially,
 * synchronously within with_api only; do not retain, reenter, race or unwind it.
 * No handle or object ownership crosses the query-only G7 boundary. — KESTREL */
#define KUMO_GPU_API_VERSION 10u
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
    uint32_t texture_features_0; /* attested TEXTURE_FEATURES[0]; zero means the whole
                                 * array is absent — TIMBERDOODLE */
    uint32_t core_features; /* attested CORE_FEATURES, verbatim; [7:0] is the variant. The
                             * query is required, but a zero word is a valid value: the opi5
                             * measures exactly 0x00000000. — TIMBERDOODLE */
    uint32_t thread_max_threads;      /* THREAD_MAX_THREADS (+0x0a0), or 0 when absent;
                                       * upstream's max_threads_per_core. — TIMBERDOODLE */
    uint32_t thread_max_workgroup_size; /* THREAD_MAX_WORKGROUP_SIZE (+0x0a4), or 0 when
                                         * absent; upstream's max_threads_per_wg. */
    uint32_t thread_features; /* THREAD_FEATURES (+0x0ac), or 0 when absent: [21:0] is
                               * registers per core, [31:24] maximum tasks per core. */
    /* Attested TEXTURE_FEATURES[1..3], verbatim, zero included. Appended in version8 after
     * the thread words so every earlier field keeps its version7 offset. — TIMBERDOODLE */
    uint32_t texture_features_1;
    uint32_t texture_features_2;
    uint32_t texture_features_3;
    /* Version10 (G7o-a): the required memory-system words, verbatim, zero included —
     * L2_FEATURES (+0x004), MEM_FEATURES (+0x010), COHERENCY_FEATURES (+0x300) — and the
     * GPU_COHERENCY_PROTOCOL value drv-gpu programmed before L2 power-up. — TIMBERDOODLE */
    uint32_t l2_features;
    uint32_t mem_features;
    uint32_t coherency_features;
    uint32_t coherency_protocol;
    /* The protocol's VM page granule and the only leaf size drv-gpu writes: service policy,
     * like the window above. It becomes pgsize_bitmap. */
    uint64_t page_bytes;
};

/* GPU_COHERENCY_PROTOCOL's "no coherency" encoding, also Panthor's
 * DRM_PANTHOR_GPU_COHERENCY_NONE. Upstream's is_io_coherent is `protocol != NONE`. */
#define KUMO_GPU_COHERENCY_NONE 31u
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
 * Unsupported BO/VM/group/sync operations are listed separately. — KESTREL
 * Since G7o-a bits 2 and 6-9 always clear on a published report: memory words are attested
 * and the rest is stated KUMO policy. Only 1, 3 and 4 can remain, each for an absent or
 * unusable optional group. — TIMBERDOODLE */
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
    /* Derived with upstream's exact expressions from panthor_dev_query_thread_props, and left
     * zero when the thread group is absent. Upstream asserts all four nonzero; KUMO does not,
     * because a zero derivation is a reportable attestation, not a reason to abort: the group
     * stays missing (bit 4) and C must not use it. The enlarged kumo_gpu_info shifts the whole
     * report, so these sit after its new 144-byte body, not after version6's 120.
     * — TIMBERDOODLE */
    uint32_t max_threads_per_core;
    uint32_t max_threads_per_wg;
    uint32_t max_tasks_per_core;
    uint32_t num_registers_per_core;
    uint32_t max_tls_instance_per_core;
    /* Upstream's constant, not a hardware attestation: the pinned panthor_kmod.c sets
     * `.afbc_features = 0` with "This register does not exist because AFBC is no longer
     * optional", and the real pan_query_afbc() consumes it. Appended last after the thread
     * block; it occupies the four bytes that were tail padding, so the report stays 168 bytes.
     * — TIMBERDOODLE */
    uint32_t has_afbc;
    /* G7o-a. The first two are upstream's own derivations from the attested words,
     * pan_query_l2_slices (MEM_FEATURES[11:8] + 1) and pan_query_bus_width
     * (1 << L2_FEATURES[27:24]). The rest copy the completed pan_kmod_dev_props: pgsize_bitmap
     * from page_bytes, is_io_coherent by upstream's expression, and KUMO's stated policy for
     * priorities, BO/VM flags and timestamps. — TIMBERDOODLE */
    uint32_t l2_slices;
    uint32_t bus_width;
    uint64_t pgsize_bitmap;
    uint32_t is_io_coherent;
    uint32_t allowed_group_priorities_mask;
    uint32_t supported_bo_flags;
    uint32_t supported_vm_op_flags;
    uint32_t gpu_can_query_timestamp;
    uint32_t timestamp_device_coherent;
    uint64_t timestamp_frequency;
};

uint32_t kumo_mesa_probe(const struct kumo_gpu_api *api,
                         struct kumo_mesa_report *out);

_Static_assert(sizeof(struct kumo_gpu_info) == 104, "query layout");
_Static_assert(offsetof(struct kumo_gpu_info, va_start) == 8, "query alignment");
_Static_assert(offsetof(struct kumo_gpu_info, mmu_features) == 40, "query tail alignment");
_Static_assert(offsetof(struct kumo_gpu_info, tiler_features) == 44, "query tail alignment");
_Static_assert(offsetof(struct kumo_gpu_info, texture_features_0) == 48, "query tail alignment");
_Static_assert(offsetof(struct kumo_gpu_info, core_features) == 52, "query tail alignment");
_Static_assert(offsetof(struct kumo_gpu_info, thread_max_threads) == 56, "query thread offset");
_Static_assert(offsetof(struct kumo_gpu_info, thread_max_workgroup_size) == 60, "query thread offset");
_Static_assert(offsetof(struct kumo_gpu_info, thread_features) == 64, "query thread offset");
_Static_assert(offsetof(struct kumo_gpu_info, texture_features_1) == 68, "query tail alignment");
_Static_assert(offsetof(struct kumo_gpu_info, texture_features_2) == 72, "query tail alignment");
_Static_assert(offsetof(struct kumo_gpu_info, texture_features_3) == 76, "query tail alignment");
_Static_assert(offsetof(struct kumo_gpu_info, l2_features) == 80, "query memory offset");
_Static_assert(offsetof(struct kumo_gpu_info, mem_features) == 84, "query memory offset");
_Static_assert(offsetof(struct kumo_gpu_info, coherency_features) == 88, "query memory offset");
_Static_assert(offsetof(struct kumo_gpu_info, coherency_protocol) == 92, "query memory offset");
_Static_assert(offsetof(struct kumo_gpu_info, page_bytes) == 96, "query page granule offset");
/* Version10 grew the embedded info from 80 to 104 bytes, so every report field moved by 24:
 * the thread block now starts at 168 and has_afbc sits at 188. The G7o-a block follows at
 * 192 with both u64 fields naturally aligned, and the report ends at 240 with no padding.
 * Field order is unchanged; only the trailing layout moves. — TIMBERDOODLE */
_Static_assert(sizeof(struct kumo_mesa_report) == 240, "report layout");
_Static_assert(offsetof(struct kumo_mesa_report, missing_properties) == 104, "report layout");
_Static_assert(offsetof(struct kumo_mesa_report, max_threads_per_core) == 168, "report thread offset");
_Static_assert(offsetof(struct kumo_mesa_report, max_threads_per_wg) == 172, "report thread offset");
_Static_assert(offsetof(struct kumo_mesa_report, max_tasks_per_core) == 176, "report thread offset");
_Static_assert(offsetof(struct kumo_mesa_report, num_registers_per_core) == 180, "report thread offset");
_Static_assert(offsetof(struct kumo_mesa_report, max_tls_instance_per_core) == 184, "report thread offset");
_Static_assert(offsetof(struct kumo_mesa_report, has_afbc) == 188, "report tail alignment");
_Static_assert(offsetof(struct kumo_mesa_report, l2_slices) == 192, "report memory offset");
_Static_assert(offsetof(struct kumo_mesa_report, pgsize_bitmap) == 200, "report page offset");
_Static_assert(offsetof(struct kumo_mesa_report, is_io_coherent) == 208, "report policy offset");
_Static_assert(offsetof(struct kumo_mesa_report, timestamp_frequency) == 232, "report policy offset");
#if UINTPTR_MAX == UINT64_MAX
_Static_assert(sizeof(struct kumo_gpu_api) == 24, "64-bit callback layout");
_Static_assert(offsetof(struct kumo_gpu_api, query) == 16, "callback alignment");
#endif
#endif
