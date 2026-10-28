# Native Mesa boundary — G7b/G7c/G7d/G7e/G7f/G7g/G7h
<!-- j645 -->
<!-- j646 -->
<!-- j647 -->
<!-- j650 -->
<!-- j651 -->

This is a compile/link scaffold, not a usable Mesa device or a rendering driver.
The probe compiles the actual pinned `pan_kmod.h` and `pan_props.c` for
**aarch64-unknown-none** against
KUMO's real relibc headers, links C → Rust → C callbacks into a static KUMO ELF,
and can run it as a separate process in a disposable full UEFI QEMU image.
Its transport uses the real GPU `Session`/wire implementation with **modeled hardware**.
It never connects to a GPU, allocates device memory, submits work or changes a served image.

The deliberately unregistered `pan_kmod_ops` scaffold has a type-checked `dev_create`
that always refuses. No half-initialized `pan_kmod_dev`, zero-filled feature report,
FD-to-capability cast, fake mutex or successful unsupported operation is returned.
The native identity/topology query bridge and Mesa's two shader-topology helpers are
implemented. The full G7 gate remains open.

## Reproduce

The source is the same Mesa 26.2.5 original tarball used by the offline shader recipe
(Debian package 26.2.5-1). SHA256:
`794b3d1729075dae7ebd47c7283b7ca0cdb5dec668f7f11cc0a5d82f6138a0a5`.
The pristine `src/panfrost/lib/kmod/pan_kmod.h` SHA256 is
`55fa7f89e5728a209fb615a26ee4099733bba29cee0695c754878f0c5dc6c4c4`.
No Mesa source checkout or Linux driver implementation is vendored. The tarball's
original copyright/license files remain in the private extraction. `NOTICE` accompanies
the small header patch; KUMO owns the new C/Rust bridge and probe.

If the shader compiler cache is absent, build it once using the existing pinned recipe:
`KUMO_MALI_SHADERS_KEEP=1 scripts/build-mali-shaders.sh`. With the existing cache:

```sh
mkdir -p build/mesa-native
# Start OrbStack first if needed. Preserve the existing task container/cache.
docker start kumo-mali-shaders
docker cp kumo-mali-shaders:/work/mesa_26.2.5.orig.tar.xz build/mesa-native/mesa.tar.xz
docker stop kumo-mali-shaders
scripts/relibc-headers.sh
python3 scripts/mesa-native-probe.py
# Requires the normal full QEMU image, without modifying it:
cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64
python3 scripts/mesa-native-probe.py --run
```

Alternatively supply that exact original archive with `--source-tar /path/to/archive`.
The script makes no network requests and rejects the wrong tarball before extraction.
The local stable Rust toolchain, clang, ld.lld and llvm-nm are required. Their versions,
the libc archive and all included header hashes are recorded in
`build/mesa-native/probe/manifest.json`; compiler bytes are not newly pinned by this slice.
`--run` uses the same local Homebrew QEMU/EDK2 setup as `scripts/thread-tls-smoke.py`.
The probe replaces only `bin/relibc-smoke` and autoexec in its **disposable ESP copy**.

Acceptance checks real cross-language ABI layout, wrong version/size/null rejection
before I/O, recoverable Busy followed by two queries with increasing IDs, continued
Rust use of the same client, rejected-handle closure, and latched loss without replay.
The output explicitly says `modeled service`. A positive incomplete result is acceptance
of this scaffold's refusal, not successful Mesa device creation. G7c additionally executes
the unmodified Mesa helpers on contiguous, sparse, high-bit and full64-bit masks, and
checks invalid zero topology, cleared failure output and unchanged loss behavior. G7d
additionally executes the unmodified `pan_kmod_dev_query_user_va_range` on attested
MMU_FEATURES words, and checks that a width which cannot hold the service window, or
which would make Mesa's own `1ull <<` shift undefined, is refused with nothing published.
The separate Orange Pi demo queries the real server mask; modeled QEMU coverage is recorded
separately from hardware evidence.

## Native seam and runtime closure

`lib/kumo-gpu/src/c.rs` and `kumo_gpu.h` share a versioned callback descriptor and a
72-byte result (private C ABI version7; older versions are refused). `with_api` lends an **existing** typed client for synchronous serial
queries. Each callback performs GetInfo, GetTopology, GetMmuFeatures, GetTilerFeatures,
GetTextureFeatures, GetCoreFeatures then GetThreadProperties, preserving the same client and
consuming seven request IDs on success. The
result is published only after all seven succeed; a failed later query leaves no partial
identity. NotSupported from an optional query is the one status that does not abort it;
core features are required, so any failure of that sixth query aborts the whole report, and
any thread-query failure other than NotSupported aborts it too. The thread group is optional:
NotSupported leaves its three words zero and missing-property bit 4 set. The opaque context may not be retained, raced, reentered or unwound. It carries
no FD, OS handle, Rust layout or new authority. The Rust owner keeps endpoint and grant
cleanup responsibility. G7a's request ID, correlation and loss rules remain authoritative.
Numeric query errors distinguish invalid arguments, invalid request, transport, protocol,
loss and server status; the result is cleared on operational failure. No retry occurs.

`native-header.patch` excludes the generic DRM factory/ioctl, FD import/export and
FD-backed mmap helpers under `__kumo__`, plus their unneeded DRM/mman includes. Missing
native implementations remain compile/link errors if called. The actual Mesa structures
and vtable remain unchanged. Generic kmod sources and the Panthor DRM backend are not linked.
No Linux preprocessor identity or DRM emulation is used.

Compilation uses real relibc pthread declarations because Mesa's embedded lock structures
require them (`HAVE_PTHREAD`). This does **not** initialize a Mesa lock or prove pthread
execution. `HAVE_ENDIAN_H` and `HAVE_STRUCT_TIMESPEC` describe actual installed headers.
The scaffold C object refers to exactly six upstream functions: `pan_query_core_count`,
`pan_query_core_id_range`, `pan_query_tiler_features`, `pan_query_compressed_formats`,
`pan_query_afrc` and, since G7g, `pan_get_model`. All six must appear defined in the final
native ELF, which has no unresolved symbols. G7h deliberately adds none: the thread
derivations are upstream's expressions applied to the attested words, and the optional
`pan_query_thread_tls_alloc` helper is not called, because the report already records its
value (`max_tls_instance_per_core`, the documented fallback to `max_threads_per_core`) as a
field rather than a returned device property. G7d's
`pan_kmod_dev_query_user_va_range` is `static inline` in the pinned header, so it adds no
entry to that inventory; G7e's tiler helper is a real function in the already-compiled
`pan_props.c`, which is why the asserted set grew from two to three and no new translation
unit was added. G7g's `pan_get_model` lives in `model/pan_model.c`, so that upstream
translation unit is now compiled and linked too. `pan_props.c` and `pan_model.c` are
compiled unchanged from the verified
archive; `common_pack.h` is generated by that archive's own Python/XML source. Only
upstream's unused-parameter warning is suppressed for that upstream translation unit;
KUMO's C scaffold retains Wall/Wextra/Werror. No pthread, DRM or allocator operation
is added by these helpers. Recheck these inventories before adding runtime calls. The executable
also links the real relibc startup/stdio/TLS path, Rust core and compiler builtins.

A probe-private `math.h` copy parenthesizes four existing function declarators
(`isfinite`, `isinf`, `isnan`, `isnormal`) to avoid expansion by their own function-like
macros. Types, macros and function bodies are unchanged; the installed sysroot and
upstream checkout are immutable. The script checks each exact old declaration before
applying this narrow workaround. A general header-generator repair is separate work.

## Properties and operations still missing

GetInfo supplies GPU_ID and CS_FEATURES. Additive GetTopology (opcode8) supplies the
nonzero64-bit shader-present mask attested by driver bring-up and used in its CSG masks.
Additive GetMmuFeatures (opcode9) supplies the nonzero raw MMU_FEATURES word that bring-up
already read, whose [7:0] field is the hardware virtual-address width; the word crosses
undecoded so the derivation stays upstream's. Additive GetTilerFeatures (opcode10) supplies
the nonzero raw TILER_FEATURES word from GPU_CONTROL +0x00C, whose [4:0] field is a log2 bin
size and [11:8] the level count. Unlike the other properties this one is **optional**: an
absent word is reported as NotSupported, the query still succeeds with `tiler_features` 0,
and the tiler group stays in the missing mask. Additive GetTextureFeatures (opcode11) supplies
the nonzero raw `TEXTURE_FEATURES[0]` from GPU_CONTROL +0x0B0 under the same optional rule.
Upstream returns it verbatim as the compressed-format mask and tests bit 25, behind its own
arch check, for AFRC. Only element 0 of the four-word array is attested, so missing-property
bit 3 deliberately stays set rather than claiming the whole group. Additive GetCoreFeatures
(opcode12) supplies the raw CORE_FEATURES word from GPU_CONTROL +0x008 under the **required**
rule of topology and MMU features: the query must succeed — bits [7:0] are the GPU variant —
but the word crosses verbatim, zero included. The Orange Pi 5 Plus measures exactly
0x00000000, and its variant byte 0 is the pinned table's G610 entry, so the measured word is
the resolving one. The bridge derives the variant with upstream's own
expression (`core_features & 0xff`, verbatim from `panthor_kmod.c`) and records whether
`pan_get_model` finds a table entry for `(gpu_id, variant)` in `model_resolved`; the pinned
tree's G610 entry carries variant 0, and a table miss is reported, not failed. Missing-mask
bit 0 clears on every successful query because the word is required.
Additive GetThreadProperties (opcode13) supplies the raw `THREAD_MAX_THREADS` (GPU_CONTROL
+0x0A0), `THREAD_MAX_WORKGROUP_SIZE` (+0x0A4) and `THREAD_FEATURES` (+0x0AC) words under the
same optional rule as tiler and texture features: all three zero means the group is absent,
the backend reports NotSupported, the query still succeeds with three zero words and
missing-property bit 4 stays set. The bridge derives the five `pan_kmod_dev_props` fields
with upstream's exact expressions from `panthor_dev_query_thread_props`: the first two words
become `max_threads_per_core` and `max_threads_per_wg`, `max_tasks_per_core` is
`THREAD_FEATURES >> 24`, `num_registers_per_core` is `THREAD_FEATURES & 0x3fffff`, and
`max_tls_instance_per_core` copies `max_threads_per_core` because there is no TLS register on
v10+. Upstream answers a zero derivation with an assert; KUMO deliberately does not. Aborting
would discard every other attested property, so the derived values are reported verbatim and
bit 4 stays set, marking the group unusable instead. The report grew to 160 bytes: the
enlarged 72-byte `kumo_gpu_info` shifts the whole report by 16 bytes, so the five new fields
sit at offsets 136–152 with four bytes of zero tail padding, not at version6's 120–136.
No additional MMIO read or client-selectable register is introduced. The bridge also reports the
**service's** admitted VA window and maximum buffer size. Those remain service policy rather
than hardware reports, and are now distinct from the attested MMU_FEATURES word the consumer
derives the hardware range from. Coherency, memory-type and shader feature reports are still
absent. The scaffold never initializes
`pan_kmod_dev_props` from guesses. The missing-property mask groups its remaining fields:

| Bit | Fields in the pinned Mesa structure |
|---|---|
| 0 | gpu_variant |
| 1 | ~~tiler_features~~ supplied by G7e; shader_present by G7c — cleared only when a shape is derived |
| 2 | mem_features, l2_features (mmu_features supplied by G7d) |
| 3 | texture_features[4] |
| 4 | ~~max_threads_per_core, max_tasks_per_core, max_threads_per_wg, num_registers_per_core, max_tls_instance_per_core~~ supplied by G7h; cleared only when all four upstream-asserted derivations are nonzero |
| 5 | afbc_features |
| 6 | gpu_can_query_timestamp, timestamp_device_coherent, timestamp_frequency, timestamp_cycles_to_ns_factor |
| 7 | allowed_group_priorities_mask |
| 8 | supported_bo_flags, supported_vm_op_flags |
| 9 | is_io_coherent, pgsize_bitmap |

This inventories the structure, not a promise that the first renderer needs every field
or that optional timestamp/performance features must be enabled. G7c selects Panfrost device discovery as the first consumer: its `pan_device.c`
uses `pan_query_core_count` and `pan_query_core_id_range`; `pan_screen.c` also uses the
count in the renderer name. Both helpers read only `shader_present`. A private partial
property value is passed only to those two functions and is never published as a device.
G7d extends that same private partial input with `mmu_features` and hands it to the pinned
header's `pan_kmod_dev_query_user_va_range`. The scaffold's ops table deliberately leaves
`dev_query_user_va_range` NULL, so upstream's own fallback derives `{start 0, size 1 << va_bits}`
from the attested word and KUMO computes no range itself. That partial device is an input to
this one inline helper and is never returned, stored or published. The derived range must
contain the service's own window or the bridge refuses, because an attestation that cannot
hold the window is a contradiction rather than a narrower device.
G7e extends the same private partial input with `tiler_features` and calls the real
`pan_query_tiler_features` from the already-compiled `pan_props.c`, reporting the derived bin
size and level count. Two guards keep that honest. A zero word means the shape is unknown
rather than the device unusable, so the backend reports NotSupported, the query still succeeds
and the tiler group stays in the missing mask; the compute path never depended on tiler
properties. And a bin-size field of 31 is not decoded at all, because upstream computes
`1 << (raw & 0x1f)` with a signed `int` and that shift would overflow — the shape degrades to
unknown instead. This matters more here than for the earlier properties: RESEARCH/009's oracle
table has never recorded a measured TILER_FEATURES, listing `0x809` only as a recollection, so
the board's actual value is what this slice measures.
The next slice obtains the remaining required discovery fields from attested hardware
or native service policy before admitting full device initialization. Optional features may be explicitly unsupported; do not fabricate
hardware values. CS_FEATURES also does not replace Panthor's full CSIF property structure.

Required later operations include native BO allocation/CPU mapping/ownership; VM creation,
VA policy, bind and safe unbind/destroy; native queue/group creation, Submit and observed
completion; and explicit synchronization/cache visibility. Optional FD/dma-buf import,
export, performance counters, timestamps, sparse mappings, eviction and asynchronous VM
operations remain unsupported unless selected and separately implemented. The existing
service admits one cooperative session with fixed bindings and retains DMA backing to
reboot. Freeing a C wrapper cannot change that lifetime.

Before Mesa initialization is admitted, compile and implement the required allocator,
mutex/once, sparse-array/dynarray and runtime-symbol closure. Before G7 closes, show actual
Mesa-rendered output through the native service and prove quiescence plus a fresh usable
session after device loss. No pthread creation/join, Shanghai worker/queue or Shinjuku module
work is included here. — KESTREL 2026-10-27
