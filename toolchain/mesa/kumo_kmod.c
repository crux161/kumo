/* SPDX-License-Identifier: MIT */
/*j643*/
/*j644*/
/*j645*/
/*j646*/
#include "kumo_gpu.h"
#include "kmod/pan_kmod.h"

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

uint32_t
kumo_mesa_probe(const struct kumo_gpu_api *api, struct kumo_mesa_report *out)
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
    /* A private partial input for these two pure upstream helpers only. Both read
     * shader_present; it is never published as a complete device property set.
     * Tiler features are still absent, so the topology group stays missing. — KESTREL */
    /* It now also carries mmu_features for the VA-range helper below. The set is still
     * partial and still never published as a device. — SAKER */
    const struct pan_kmod_dev_props props = {
        .shader_present = out->info.shader_present,
        .mmu_features = out->info.mmu_features,
        .tiler_features = out->info.tiler_features,
    };
    out->core_count = pan_query_core_count(&props);
    out->core_id_range = pan_query_core_id_range(&props);
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
     * tiler group, so it clears only when the shape was genuinely derived. — SAKER */
    out->missing_properties = tiler_derived
                                  ? KUMO_MESA_MISSING_ALL & ~KUMO_MESA_MISSING_TOPOLOGY
                                  : KUMO_MESA_MISSING_ALL;
    /* No generic FD entry point or Mesa runtime initialization is called. The
     * refusal callback is type-checked by Mesa and exercised by this probe. — KESTREL */
    if (native_scaffold.dev_create(-1, 0, NULL, NULL) != NULL)
        __builtin_trap();
    return KUMO_MESA_INCOMPLETE;
}
