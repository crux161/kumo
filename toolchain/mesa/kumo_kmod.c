/* SPDX-License-Identifier: MIT */
/*j643*/
/*j644*/
/*j645*/
/*j646*/
/*j647*/
/*j650*/
#include "kumo_gpu.h"
#include "kmod/pan_kmod.h"
#include "model/pan_model.h"

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
        /* pan_query_afrc reads gpu_id as well as texture_features[0], so the ID must be in
         * this partial input for upstream's own arch test to run. — SAKER */
        .gpu_id = out->info.gpu_id,
        .texture_features[0] = out->info.texture_features_0,
        /* Upstream's own derivation, verbatim from panthor_kmod.c: the variant is bits [7:0]
         * of the attested word. The opi5's measured word is exactly zero, and variant byte 0
         * is the pinned table's G610 entry, so zero here is the expected outcome, not a
         * failed read. — TIMBERDOODLE */
        .gpu_variant = out->info.core_features & 0xff,
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
     * leaves both zero rather than reporting "no formats". — SAKER */
    if (out->info.texture_features_0) {
        out->compressed_formats = pan_query_compressed_formats(&props);
        out->has_afrc = pan_query_afrc(&props) ? 1u : 0u;
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
     * tiler group, so it clears only when the shape was genuinely derived. Bit 3 stays set even
     * now, because only element 0 of the four-word texture_features array is attested and
     * claiming the group would overstate it. Bit 0 (the variant) clears unconditionally once
     * the query succeeded, because core features are required: the word is attested or the
     * session died before publication. — SAKER, TIMBERDOODLE */
    out->missing_properties = tiler_derived
                                  ? KUMO_MESA_MISSING_ALL & ~KUMO_MESA_MISSING_TOPOLOGY
                                  : KUMO_MESA_MISSING_ALL;
    out->missing_properties &= ~KUMO_MESA_MISSING_VARIANT;
    /* No generic FD entry point or Mesa runtime initialization is called. The
     * refusal callback is type-checked by Mesa and exercised by this probe. — KESTREL */
    if (native_scaffold.dev_create(-1, 0, NULL, NULL) != NULL)
        __builtin_trap();
    return KUMO_MESA_INCOMPLETE;
}
