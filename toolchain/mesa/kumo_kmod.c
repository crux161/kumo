/* SPDX-License-Identifier: MIT */
/*j652*/
/*j653*/
/*j742*/
/*j744*/
/*j745*/
#include "kumo_kmod.h"
#include "kmod/pan_kmod.h"
#include "model/pan_model.h"
#include <sys/mman.h>

/* This table is deliberately unregistered and incomplete. A DRM fd is never
 * reinterpreted as a capability. No pan_kmod_dev is published until native
 * properties, object lifetimes and all selected operations are real. — KESTREL */
static struct pan_kmod_dev *
refuse_device(int fd, uint32_t flags, const struct pan_kmod_driver *driver,
              const struct pan_kmod_allocator *allocator)
{
    (void)fd;
    (void)flags;
    (void)driver;
    (void)allocator;
    return NULL;
}

static const struct pan_kmod_ops native_scaffold = {
    .dev_create = refuse_device,
};

/* Compile against real pinned Mesa structures, including their native platform
 * header closure. Do not mirror a hand-written pan_kmod_ops definition. — KESTREL */
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->gpu_id) == 8, "Mesa GPU ID");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->shader_present) == 8, "Mesa topology");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->mmu_features) == 4, "Mesa MMU properties");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->texture_features) == 16, "Mesa texture properties");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->gpu_variant) == 4, "Mesa GPU variant");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->max_threads_per_core) == 4, "Mesa thread properties");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->max_threads_per_wg) == 4, "Mesa thread properties");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->max_tasks_per_core) == 1, "Mesa thread properties");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->num_registers_per_core) == 4, "Mesa thread properties");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->max_tls_instance_per_core) == 4, "Mesa thread properties");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->afbc_features) == 4, "Mesa AFBC properties");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->l2_features) == 4, "Mesa memory properties");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->mem_features) == 4, "Mesa memory properties");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->pgsize_bitmap) == 8, "Mesa page sizes");
_Static_assert(sizeof(((struct pan_kmod_dev_props *)0)->timestamp_frequency) == 8, "Mesa timestamps");

/* KUMO's stated policy for the pan_kmod_dev_props fields no register holds (G7o-a). Each is a
 * fact of the native protocol, and the backend's BO/VM operations must honor exactly these:
 * - GroupCreate carries no priority, so every group runs at the one class the drivers call
 *   MEDIUM, Panthor's default and the one both Gallium and panvk request by default;
 * - BO flags are Panthor's base set. EXECUTABLE binds without NO_EXECUTE (and so read-only,
 *   by the bind rule), NO_MMAP skips the CPU mapping and GPU_UNCACHED is BindFlags::UNCACHED.
 *   No WB_MMAP: CPU mappings of GPU memory are non-cacheable, and the device is not IO
 *   coherent;
 * - no VM operation flags: there is no sparse mapping;
 * - no timestamps: the protocol has no timestamp query and no job requirement for one.
 * — TIMBERDOODLE */
#define KUMO_ALLOWED_GROUP_PRIORITIES PAN_KMOD_GROUP_ALLOW_PRIORITY_MEDIUM
#define KUMO_SUPPORTED_BO_FLAGS \
    (PAN_KMOD_BO_FLAG_EXECUTABLE | PAN_KMOD_BO_FLAG_NO_MMAP | PAN_KMOD_BO_FLAG_GPU_UNCACHED)
#define KUMO_SUPPORTED_VM_OP_FLAGS 0u

uint32_t
kumo_mesa_query_properties(const struct kumo_gpu_api *api, struct kumo_mesa_report *out,
                 struct pan_kmod_dev_props *complete)
{
    if (!out)
        return KUMO_MESA_BAD_ABI;
    if (!api || api->version != KUMO_GPU_API_VERSION ||
        api->bytes != sizeof(*api) || !api->context || !api->query)
        return KUMO_MESA_BAD_ABI;
    /* Validate the ABI before writing the enlarged report: a version1 caller may
     * only own its older, smaller output allocation. — KESTREL */
    *out = (struct kumo_mesa_report){0};

    out->query_status = api->query(api->context, &out->info);
    if (out->query_status != KUMO_GPU_OK) {
        out->info = (struct kumo_gpu_info){0};
        return KUMO_MESA_QUERY_FAILED;
    }
    /* Upstream's own expressions, verbatim from panthor_dev_query_thread_props: the two
     * workgroup/thread words copy straight across, tasks per core is THREAD_FEATURES >> 24
     * and registers per core is THREAD_FEATURES & 0x3fffff. The TLS instance count is not a
     * register: upstream assumes it equals max_threads_per_core on v10+. Everything is
     * recorded verbatim, zero included — upstream answers a zero field with an assert, but
     * KUMO is an attestation channel and a zero derivation is data, not a fault. Aborting
     * here would hide every other attested property, so the group is instead marked missing
     * (bit 4 stays set) and left unusable downstream. — TIMBERDOODLE */
    out->max_threads_per_core = out->info.thread_max_threads;
    out->max_threads_per_wg = out->info.thread_max_workgroup_size;
    out->max_tasks_per_core = out->info.thread_features >> 24;
    out->num_registers_per_core = out->info.thread_features & 0x3fffff;
    out->max_tls_instance_per_core = out->max_threads_per_core;

    /* Once a partial input for single helpers, this is now the complete property set (G7o-a):
     * every field is an attested word, upstream's derivation of one, or KUMO's stated policy
     * above. It is still never published as a device, because native device, VM and group
     * operations are not published yet; the missing mask below records what is still unusable.
     * — KESTREL, SAKER, TIMBERDOODLE */
    const struct pan_kmod_dev_props props = {
        .shader_present = out->info.shader_present,
        .mmu_features = out->info.mmu_features,
        .tiler_features = out->info.tiler_features,
        /* pan_query_afrc reads gpu_id as well as texture_features[0], so the ID must be in
         * this set for upstream's own arch test to run. All four attested words go
         * in; upstream's helpers read only element 0, and a zero it sees there leaves the
         * derived fields zero because the query reported the group absent. — SAKER, TIMBERDOODLE */
        .gpu_id = out->info.gpu_id,
        .texture_features[0] = out->info.texture_features_0,
        .texture_features[1] = out->info.texture_features_1,
        .texture_features[2] = out->info.texture_features_2,
        .texture_features[3] = out->info.texture_features_3,
        /* Upstream's own derivation, verbatim from panthor_kmod.c: the variant is bits [7:0]
         * of the attested word. The opi5's measured word is exactly zero, and variant byte 0
         * is the pinned table's G610 entry, so zero here is the expected outcome, not a
         * failed read. — TIMBERDOODLE */
        .gpu_variant = out->info.core_features & 0xff,
        /* Upstream's constant, verbatim from panthor_kmod.c: "This register does not exist
         * because AFBC is no longer optional." AFBC is claimed the way upstream claims it,
         * from service-side knowledge rather than from an attested register word. The same
         * constant reaches the real pan_query_afbc below. — TIMBERDOODLE */
        .afbc_features = 0,
        .max_threads_per_core = out->max_threads_per_core,
        .max_threads_per_wg = out->max_threads_per_wg,
        .max_tasks_per_core = (uint8_t)out->max_tasks_per_core,
        .num_registers_per_core = out->num_registers_per_core,
        .max_tls_instance_per_core = out->max_tls_instance_per_core,
        /* Attested raw (G7o-a). Upstream copies both words from Panthor's GPU_INFO and
         * decodes them only through its own pan_query_* helpers. — TIMBERDOODLE */
        .l2_features = out->info.l2_features,
        .mem_features = out->info.mem_features,
        /* Upstream's expression, verbatim from panthor_dev_query_props: the device is IO
         * coherent exactly when the selected protocol is not "none". drv-gpu programs none,
         * because RK3588 is not dma-coherent and COHERENCY_FEATURES reads zero. */
        .is_io_coherent = out->info.coherency_protocol != KUMO_GPU_COHERENCY_NONE,
        /* A single page size is a one-bit bitmap: its value is the size itself. */
        .pgsize_bitmap = out->info.page_bytes,
        .allowed_group_priorities_mask = KUMO_ALLOWED_GROUP_PRIORITIES,
        .supported_bo_flags = KUMO_SUPPORTED_BO_FLAGS,
        .supported_vm_op_flags = KUMO_SUPPORTED_VM_OP_FLAGS,
        /* No timestamps. Upstream computes timestamp_cycles_to_ns_factor only from a nonzero
         * frequency, so it stays zero with it. */
        .gpu_can_query_timestamp = false,
        .timestamp_device_coherent = false,
        .timestamp_frequency = 0,
    };
    out->core_count = pan_query_core_count(&props);
    out->core_id_range = pan_query_core_id_range(&props);
    /* pan_get_model matches (gpu_prod_id, gpu_variant) against Mesa's static table; the
     * pinned tree's G610 entry carries variant 0. Resolution is the point of attesting this
     * word: it is what unlocks the model-name and TIB-size paths, but it is recorded, not
     * required — a table miss is a real, reportable outcome. — TIMBERDOODLE */
    out->gpu_variant = props.gpu_variant;
    out->model_resolved = pan_get_model(props.gpu_id, props.gpu_variant) != NULL;
    /* Derived only from an attested word, and only when the bin-size field cannot make
     * upstream's `1 << (raw & 0x1f)` shift overflow a signed int. Either way this degrades to
     * "shape unknown" rather than failing the query: tiler properties are optional and the
     * compute path this scaffold serves never depended on them. — SAKER */
    const int tiler_derived = out->info.tiler_features &&
                              (out->info.tiler_features & 0x1f) < 31;
    if (tiler_derived) {
        const struct pan_tiler_features tiler = pan_query_tiler_features(&props);
        out->tiler_bin_size = tiler.bin_size;
        out->tiler_max_levels = tiler.max_levels;
    }
    /* Both are pure reads of the attested word with no shift hazard: compressed_formats
     * returns it verbatim and afrc tests bit 25 behind upstream's own arch check. Absence
     * leaves both zero rather than reporting "no formats". These derivations read element 0
     * only; elements 1..3 are attested and carried in the partial input but not decoded.
     * — SAKER, TIMBERDOODLE */
    if (out->info.texture_features_0) {
        out->compressed_formats = pan_query_compressed_formats(&props);
        out->has_afrc = pan_query_afrc(&props) ? 1u : 0u;
    }
    /* The same four fields upstream asserts nonzero. This predicate decides only whether the
     * group is safely usable; it never gates the report. — TIMBERDOODLE */
    const int threads_derived = out->max_threads_per_core && out->max_threads_per_wg &&
                                out->max_tasks_per_core && out->num_registers_per_core;
    /* AFBC is the one discovery group with no register at all, so the pinned upstream
     * constant is consumed through upstream's own helper instead of being copied by hand:
     * pan_query_afbc returns true for pan_arch(gpu_id) >= 5 when afbc_features is 0.
     * Recorded for the report and used for the missing mask below. — TIMBERDOODLE */
    out->has_afbc = pan_query_afbc(&props) ? 1u : 0u;
    /* Upstream's own helpers from the already-linked pan_props.c, applied to the attested
     * words. The board's 0x301 and 0x0712_0306 give 4 slices and a 128-bit bus, as RESEARCH/009
     * predicted from Rockchip's datasheet. — TIMBERDOODLE */
    out->l2_slices = pan_query_l2_slices(&props);
    out->bus_width = pan_query_bus_width(&props);
    out->pgsize_bitmap = props.pgsize_bitmap;
    out->is_io_coherent = props.is_io_coherent ? 1u : 0u;
    out->allowed_group_priorities_mask = props.allowed_group_priorities_mask;
    out->supported_bo_flags = props.supported_bo_flags;
    out->supported_vm_op_flags = props.supported_vm_op_flags;
    out->gpu_can_query_timestamp = props.gpu_can_query_timestamp ? 1u : 0u;
    out->timestamp_device_coherent = props.timestamp_device_coherent ? 1u : 0u;
    out->timestamp_frequency = props.timestamp_frequency;

    /* Upstream derives the range by shifting 1ull by this field, so an attested width of
     * 64 or more would be undefined behaviour inside Mesa's own helper. Refuse before
     * calling it rather than let a bogus attestation reach the shift. — SAKER */
    if ((out->info.mmu_features & 0xff) >= 64) {
        *out = (struct kumo_mesa_report){0};
        return KUMO_MESA_VA_RANGE;
    }

    /* Upstream derives the user VA range itself. The scaffold table leaves
     * dev_query_user_va_range NULL, so the header's fallback reads the attested
     * MMU_FEATURES word. This partial device is an input to that one inline helper;
     * it is never returned, stored or published as a device. — SAKER */
    const struct pan_kmod_dev attesting = {
        .props = props,
        .ops = &native_scaffold,
    };
    const struct pan_kmod_va_range range = pan_kmod_dev_query_user_va_range(&attesting);
    /* An attested range that cannot hold this service's own window is a contradiction,
     * not a narrower device: refuse rather than clamp, and publish nothing. — SAKER */
    if (range.start > out->info.va_start ||
        out->info.va_end - range.start > range.size) {
        *out = (struct kumo_mesa_report){0};
        return KUMO_MESA_VA_RANGE;
    }
    out->va_range_start = range.start;
    out->va_range_size = range.size;
    /* The mask tracks what is actually attested instead of staying a constant: bit 1 is the
     * tiler group, so it clears only when the shape was genuinely derived. Bit 3 is the
     * texture group, so it clears only when the query succeeded — all four words were read in
     * the same powered block and crossed verbatim — and stays when element 0 reported the
     * group absent. Bit 4 clears only when the query succeeded and all four upstream-asserted
     * thread fields are nonzero; a zero derivation keeps it set rather than aborting, so the
     * rest of the report survives. Bit 5 clears only when upstream's own pan_query_afbc
     * returned true for the constant afbc_features 0; it is never cleared merely because the
     * query succeeded. Bit 0 (the variant) clears unconditionally once the query succeeded,
     * because core features are required: the word is attested or the session died before
     * publication. — SAKER, TIMBERDOODLE */
    out->missing_properties = tiler_derived
                                  ? KUMO_MESA_MISSING_ALL & ~KUMO_MESA_MISSING_TOPOLOGY
                                  : KUMO_MESA_MISSING_ALL;
    if (threads_derived)
        out->missing_properties &= ~KUMO_MESA_MISSING_THREADS;
    out->missing_properties &= ~KUMO_MESA_MISSING_VARIANT;
    /* G7o-a: the memory words are required, so bit 2 clears with every published report, like
     * bit 0. Bits 6-9 clear because their fields now hold either an attested value (the
     * coherency protocol) or KUMO's stated policy; "no timestamps" is complete information,
     * not missing information. — TIMBERDOODLE */
    out->missing_properties &= ~(KUMO_MESA_MISSING_MEMORY | KUMO_MESA_MISSING_TIMESTAMP |
                                 KUMO_MESA_MISSING_PRIORITIES | KUMO_MESA_MISSING_BO_VM_FLAGS |
                                 KUMO_MESA_MISSING_COHERENCY_PAGES);
    if (out->info.texture_features_0)
        out->missing_properties &= ~KUMO_MESA_MISSING_TEXTURE;
    if (out->has_afbc)
        out->missing_properties &= ~KUMO_MESA_MISSING_COMPRESSION;
    /* No generic FD entry point or Mesa runtime initialization is called. The
     * refusal callback is type-checked by Mesa and exercised by this probe. — KESTREL */
    if (native_scaffold.dev_create(-1, 0, NULL, NULL) != NULL)
        __builtin_trap();
    if (complete) *complete = props;
    return KUMO_MESA_INCOMPLETE;
}

uint32_t
kumo_mesa_probe(const struct kumo_gpu_api *api, struct kumo_mesa_report *out)
{
    return kumo_mesa_query_properties(api, out, NULL);
}

/* These BO-only helpers leave device initialization to native-device.c, which also
 * uses them for the published backend. The embedded BO is the actual pinned Mesa
 * type, never an emulated definition. — KESTREL */
_Static_assert(PAN_KMOD_BO_FLAG_EXECUTABLE == KUMO_GPU_BO_EXECUTABLE, "executable flag");
_Static_assert(PAN_KMOD_BO_FLAG_NO_MMAP == KUMO_GPU_BO_NO_MMAP, "no-mmap flag");
_Static_assert(PAN_KMOD_BO_FLAG_GPU_UNCACHED == KUMO_GPU_BO_GPU_UNCACHED, "uncached flag");

static bool
buffer_api_valid(const struct kumo_gpu_api *api)
{
    return api && api->version == KUMO_GPU_API_VERSION && api->bytes == sizeof(*api) &&
           api->context && api->buffer_create && api->buffer_destroy &&
           api->buffer_map && api->buffer_unmap;
}

static bool
buffer_owned(const struct kumo_gpu_api *api, const struct kumo_mesa_bo *bo)
{
    return buffer_api_valid(api) && bo && bo->owner == api->context && bo->base.handle;
}

uint32_t
kumo_mesa_bo_alloc(const struct kumo_gpu_api *api, uint64_t size, uint32_t flags,
                    struct kumo_mesa_bo *out)
{
    if (!buffer_api_valid(api) || !out)
        return KUMO_GPU_BAD_ARGUMENT;
    if (flags & ~KUMO_GPU_BO_FLAGS)
        return KUMO_GPU_INVALID_REQUEST;
    struct kumo_gpu_buffer_info info = {0};
    uint32_t status = api->buffer_create(api->context, size, flags, &info);
    if (status)
        return status;
    *out = (struct kumo_mesa_bo){
        .base = {.refcnt = 1, .size = info.bytes, .handle = info.id, .flags = info.flags},
        .owner = api->context,
    };
    return KUMO_GPU_OK;
}

uint32_t
kumo_mesa_bo_free(const struct kumo_gpu_api *api, struct kumo_mesa_bo *bo)
{
    if (!buffer_owned(api, bo))
        return KUMO_GPU_BAD_ARGUMENT;
    uint32_t status = api->buffer_destroy(api->context, bo->base.handle);
    if (!status)
        *bo = (struct kumo_mesa_bo){0};
    return status;
}

uint32_t
kumo_mesa_bo_mmap(const struct kumo_gpu_api *api, const struct kumo_mesa_bo *bo,
                   int prot, int flags, void *host_addr, void **out)
{
    if (!buffer_owned(api, bo) || !out)
        return KUMO_GPU_BAD_ARGUMENT;
    *out = NULL;
    /* The native mapper implements exactly the drivers' full-BO RW shared mapping.
     * Unsupported protections or fixed/private mappings must not be silently widened.
     * GPU executable permission does not make this CPU mapping executable. — KESTREL */
    if (prot != (PROT_READ | PROT_WRITE) || flags != MAP_SHARED || host_addr || (bo->base.flags & PAN_KMOD_BO_FLAG_NO_MMAP))
        return KUMO_GPU_INVALID_REQUEST;
    return api->buffer_map(api->context, bo->base.handle, out);
}

uint32_t
kumo_mesa_bo_munmap(const struct kumo_gpu_api *api, const struct kumo_mesa_bo *bo)
{
    if (!buffer_owned(api, bo))
        return KUMO_GPU_BAD_ARGUMENT;
    return api->buffer_unmap(api->context, bo->base.handle);
}

uint32_t
kumo_mesa_bo_bind_flags(const struct kumo_mesa_bo *bo)
{
    return ((bo->base.flags & PAN_KMOD_BO_FLAG_EXECUTABLE) ? 1u : 2u) |
           ((bo->base.flags & PAN_KMOD_BO_FLAG_GPU_UNCACHED) ? 4u : 0u);
}
