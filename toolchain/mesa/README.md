# Native Mesa boundary — G7b/G7c/G7d/G7e/G7f/G7g/G7h/G7i/G7j
<!-- j740 -->
<!-- j741 -->
<!-- j742 -->
<!-- j743 -->
<!-- j744 -->

## G7o-c: native BO allocation, destruction and CPU mapping

The borrowed callback ABI is version **11** (56-byte API); its existing 104-byte
property result and 240-byte report retain their layouts. Four optional callbacks
create, destroy, map and unmap BOs. `with_api` stays query-only;
`with_buffer_api` supplies all four over a Rust `bo::Buffers` store. Kernel VMO
handles remain in Rust. C sees a session-local ID, flags, rounded size and a scoped
CPU pointer. Query and BO callbacks use the same client and request sequence.

`kumo_kmod.h` embeds the pinned Mesa `pan_kmod_bo` in caller-owned native storage.
Its allocation/free/mmap/munmap helpers use the callback table directly. The
owner token rejects a foreign context before I/O. This BO is unpublished:
`base.dev` and `exclusive_vm` remain null, and generic Mesa device helpers require
G7o-d. A service VM must already exist before BO allocation. — KESTREL, 2026-10-31

- Only EXECUTABLE, NO_MMAP and GPU_UNCACHED are accepted, in any combination.
  Zero, overflowing and over-limit sizes refuse before I/O; other sizes round up
  to the 4 KiB page granule.
- CPU mapping is lazy and covers the complete BO with RW Normal-NC data pages.
  Repeated mmap returns the existing pointer; munmap is idempotent and remapping
  preserves the VMO contents. NO_MMAP refuses before mapping. The native helper
  accepts the drivers' `PROT_READ | PROT_WRITE`, `MAP_SHARED`, null-address call;
  other protections, private/fixed requests and CPU execution refuse explicitly.
- GPU bind policy is READ_ONLY for EXECUTABLE, NO_EXECUTE otherwise, plus UNCACHED
  for GPU_UNCACHED. Actual VM binding and both drivers' W^X acceptance remain d/e.
- Free first confirms BufferDestroy, which orders GPU work and unbinds in the
  service, then unmaps the CPU alias and closes the local VMO grant. Recoverable
  failures keep the BO retryable. If unmap fails after the destroy acknowledgement,
  retry finishes local cleanup without issuing a second destroy.
- Scope abandonment marks the client lost, releases local mappings/grants, and
  sends no destroy replay. The caller must close the endpoint and reconnect;
  drv-gpu retains its backing until contained reset proves quiescence. A failed
  CPU unmap retains the kernel mapping's pin. — KESTREL, 2026-10-31

```sh
python3 scripts/mesa-bo-smoke.py --run
```

The runner verifies the pinned header/source and existing property-link closure,
then links the real C BO helpers into a native Rust test executable. The service
is modeled through the real wire codec and Session; allocation and mapping use
real kernel paged VMOs and `native.rs`'s kernel adapter. It checks all eight flag
combinations, every page's first/last words across unmap/remap, a 4 MiB + 1-byte
request, stale/foreign rejection, exact request/handle/map counts and zero live
bytes at close. It publishes no device and claims no GPU hardware execution.
— KESTREL, 2026-10-31

## G7o-b: service memory for real driver allocations

The native channel now allows 64 MiB per BO, 128 MiB live per session, 256 bindings,
and a 256 MiB GPU VA window (`0x20000000..0x30000000`). These are bounded bring-up
limits, not a promise that every arbitrary GL/Vulkan workload fits. Mesa 26.2.5's
v10 drivers use 64 KiB command rings and geometry storage, 16/64 KiB descriptor and
shader slabs, and 2 MiB tiler chunks; images and buffers can exceed the former
2 MiB cap. The 128 MiB legacy tiler BO in both sources is gated to PAN_ARCH <= 9,
and is not part of the G610/v10 requirement. — KESTREL, 2026-10-31

BufferCreate now uses `VmoCreatePaged` (syscall 55): independent zeroed/cleaned DMA
pages, one shareable VMO, and one contiguous Normal-NC CPU mapping. The physical
page list is returned to the allocating driver only; the GPU channel still returns
an ID, size and VMO grant. VmBind resolves offsets in that list and writes individual
GPU leaves. The driver reserves enough table pages to visit the entire VA window;
binding metadata and temporary unbind batches live on the heap. Reset-before-release,
checked unbind/flush, live-byte accounting and monotonic IDs keep their existing contract.
A failed allocation returns its entire prefix to the DMA free list. Duplicates, CPU
mappings, queued transfers and retained kernel references prevent reclamation.

```sh
python3 scripts/gpu-memory-smoke.py --run
```

This builds a small native Rust probe and copies the QEMU image into a disposable
ESP. It forces physical holes with live guard VMOs, allocates 64 MiB, checks both
ends of every page through two shared NC aliases, closes one owner while retaining
the other, and requires the same pages back zeroed after final release. Host tests
cover the GPU table walk, offsets, table capacity, the budget and binding quotas,
and failure/close accounting. GPU hardware acceptance remains G7o-f; Mesa's BO and
VM backend operations remain G7o-c/d. — KESTREL, 2026-10-31

## G7o-a: the property set is complete

GetMemoryProperties (opcode 17) attests `L2_FEATURES` (+0x004), `MEM_FEATURES` (+0x010),
`COHERENCY_FEATURES` (+0x300) and the `GPU_COHERENCY_PROTOCOL` value drv-gpu programs before L2
power-up (31, "none"). All four words are required and cross verbatim. drv-gpu already read
and logged the three registers at bring-up (J653); it now serves them.

`kumo_kmod.c` builds one complete `pan_kmod_dev_props`, which G7o-d's device will publish
unchanged:
- `l2_features` and `mem_features` are attested; upstream's `pan_query_l2_slices` and
  `pan_query_bus_width` derive 4 slices and a 128-bit bus from the board's words;
- `is_io_coherent` is upstream's `protocol != none`, so false on the Orange Pi;
- `pgsize_bitmap` is the protocol's 4 KiB page granule, carried from `kumo_gpu::PAGE_BYTES`
  and the only leaf size drv-gpu writes;
- KUMO's stated policy, which the later BO and VM slices must honor: MEDIUM priority only
  (GroupCreate carries none); Panthor's base BO flags EXECUTABLE, NO_MMAP and GPU_UNCACHED,
  with no WB_MMAP because CPU mappings are non-cacheable; no VM op flags (no sparse); no
  timestamps.

The missing mask therefore reaches zero for a device whose optional groups are present.
Only bits 1, 3 and 4 can remain, for an absent or unusable tiler, texture or thread group.
`kumo_mesa_probe` still returns INCOMPLETE and publishes no device: Device, VM and group operations
are not published yet. The live gpu-client logs the memory group on every boot, decoded as upstream
does. — TIMBERDOODLE, 2026-10-31

## G7n: the compiler runs on KUMO

After G7m, keep the offline driver container running:

```sh
python3 scripts/mesa-compiler-smoke.py --run
```

`build-compiler.py` builds one harness twice from the same Mesa build: for KUMO
on relibc, and natively for Linux as the reference. It runs three compiles, all
for v10 (GPU_ID `0xa8670005`):

- **SPIR-V → NIR → Valhall.** Mesa's own `panfrost_compile`
  (`src/panfrost/clc/pan_compile.c`, compiled unchanged) on the shader recipe's
  `kumo_paint` SPIR-V. On Linux its two outputs must equal the recipe's own
  `panfrost_compile` byte for byte.
- **GLSL ES 3.10 → NIR → Valhall**, twice. Mesa's standalone GLSL front end and
  linker run first. The state tracker's compute lowering follows, then Gallium
  panfrost's compute compile in `panfrost_shader_compile`'s order.
  `g7n/paint.comp` is the kumo_paint pattern in GLSL. `g7n/math.comp` adds
  shared memory, barriers, a reduction, and a constant expression folded with
  the build's libm (openlibm on KUMO, glibc on Linux).

KUMO must print every Linux line exactly: size, BLAKE3 of the output, work
registers, TLS, WLS and sysvals.

`pan_compile.c` reads and writes files. KUMO's root volume is read-only and
relibc's `mmap` is anonymous-only. So its file calls are renamed in the object
(`llvm-objcopy --redefine-sym`) to an in-memory layer in `compiler-smoke.c`.
glibc's headers spell them `open64`/`fopen64`/`lseek64`/`mmap64`, so both
spellings are renamed.

The GLSL compiler outran the 16 KiB main stack every KUMO program had. The
harness asks for 2 MiB through `PT_GNU_STACK`, and Sora now honours its
`p_memsz` (16 KiB floor, 8 MiB ceiling, refused above). With an explicit
`PHDRS` block lld emits the header but leaves the size zero, so the build
writes the field after linking and checks it with llvm-readelf.

CPU only: no GPU device is opened and no compiled binary runs. QEMU and Orange
Pi 5 Plus metal both printed the Linux reference byte for byte (J741). Outputs,
the Linux transcript and the manifest live in
`build/mesa-drivers/result/compiler/`. — TIMBERDOODLE, 2026-10-31

## G7m: OS closure and Mesa utilities

After G7k preparation and G7l, keep the offline driver container running:

```sh
scripts/relibc-headers.sh
python3 scripts/mesa-util-smoke.py --run
```

The closure recipe synchronizes the actual SDK, rebuilds affected Mesa archives
and the measured C++ runtime, then links both complete probe graphs without
ignoring undefined symbols. It retains only the previously inventoried optional
weak dispatch/TLS references. Both links have zero required gaps.

`drm-refusal.c` exposes the measured libdrm signatures only under `__kumo__`.
Every device, ioctl, BO/FD, syncobj and sync-file operation refuses with ENOTSUP;
outputs remain untouched. No integer FD becomes a capability. Null cleanup is
harmless; attempting to free a live foreign DRM object aborts. These entry points
keep retained DRM code explicit until native seams replace their callers.

`static-build-id.c` reads the linker's actual GNU note from mapped program
headers in a static ET_EXEC image. Its utility target preserves that note in a
read-only PT_LOAD and PT_NOTE and uses LLD's SHA1 build ID. The runtime ID must
match llvm-readelf byte for byte; foreign addresses have no ID. The relibc
overlay also corrects Elf64_Word/Sword to their 32-bit ELF widths, with compile-time
size and offset checks at the consumer.

The QEMU proof executes Mesa's ralloc ownership/destructors, collision-heavy
hash insertion/growth/removal, stable sparse storage across distant indices,
dynarray growth, simple mutex/once/once-data, and C++ TLS sort callback. It also
checks every refusal, the actual Mesa DRM factory, and relibc's existing dlopen
refusal/error reset. It creates no thread and makes no concurrency claim.
Build manifests, both probe links/maps/extraction traces, note bytes and UEFI
proof logs live in `build/mesa-drivers/result/closure/`.

G7m is closed by the passing OS/util proof (J740). Runtime GLSL/SPIR-V compiler
acceptance (G7n), native device/memory/sync/heaps (G7o–G7q), and Gallium rendering
remain later gates. Closure decisions — SAKER, 2026-10-31.

## G7k: both complete driver graphs cross-build

`scripts/mesa-drivers.py` builds Gallium panfrost, the Mesa state tracker/GLSL
compiler/GLES2 dispatch, and panvk together for `aarch64-unknown-none`, with
`host_machine.system = 'kumo'`. This is a separate compile/link inventory; the
native property probe below keeps its original scope. No target probe ELF is
executed, and no kernel, runtime, image or served payload changes in G7k.

```sh
scripts/relibc-headers.sh
# Network is allowed only while preparing the pinned tool cache and original tarball:
python3 scripts/mesa-drivers.py --prepare
# Subsequent clean builds use the cache, with Docker networking disabled:
python3 scripts/mesa-drivers.py
```

Preparation extends the existing shader builder with clang/lld/libc++ headers
22.1.8-3 from the same signed Debian snapshot (20261024T000000Z), builds the
native `vtn_bindgen2`, then snapshots that tool cache. The source tarball's full
SHA256 is checked before every extraction. The actual build runs in a separate
`--network=none` container with a read-only repository mount and Meson
`--wrap-mode=nodownload`. Python/Mako, native compiler tools and every package
version are recorded. `--keep` leaves this offline container running for the
following slices; remove `kumo-mesa-drivers-g7k` before starting another clean run.
No Linux library or Linux host-header directory is passed to the target compiler.
The Debian clang driver uses `aarch64-none-elf` only for configure-test links so
that it selects LLD directly; every target translation unit uses
`aarch64-unknown-none`. C++ uses `-fno-exceptions -fno-rtti` and libc++ headers
configured for relibc's pthread declarations, without localization or parallel
algorithms. The Linux-built libc++ binary is never linked.

The checked [driver inventory](DRIVER-INVENTORY.md) records the measured result:
53 required GL gaps, 44 required Vulkan gaps, and the requesting archive/object.
`build/mesa-drivers/result/` contains all static archives, diagnostic ELFs,
linker errors/maps/extraction traces, per-object sizes, the exhaustive
`inventory.json` and `optional-undefined.tsv`, and input/tool/archive hashes.
There is one optional GL TLS wrapper and 6,292 optional Vulkan dispatch symbols;
each is verified against a weak reference in an extracted object. They are
accounted separately from required failures. Every symbol must be classified or
the recipe fails. The plan's glapi root has the pinned source's actual name,
`_mesa_glapi_get_dispatch`.

`kumo-build.patch` is applied to the pristine source, independently of the older
`native-header.patch`. It admits generic POSIX utilities on KUMO, uses relibc's
existing C11 threads instead of Mesa's duplicate fallback, exposes build-ID
declarations without providing bodies, retains the DRM modifier metadata that
panvk requires, supplies libdrm declarations only, and creates static GLES/panvk
targets. All upstream GPU architecture variants are compiled and linked where
their driver tables reference them, including v10. The legacy JM Linux kernel
backend `panfrost_kmod.c` alone is excluded for KUMO because its performance
session needs eventfd/timerfd; the Panthor backend, both real drivers, their
compilers, and their CSF operations remain in the inventory. Optional standalone
compiler scaffolds, test libraries, video implementations and alternative lite
Vulkan runtimes are outside the driver link graphs.

The recipe repairs only its private header copies: classification declarations
are protected from macro expansion and libc++ overload conflicts, missing
`HUGE_VAL`/`NAN` definitions use compiler constants, openlibm fenv headers are
installed from the pinned relibc submodule, and a compile-only header declares
`open_memstream` while importing the existing `alloca`/strings declarations.
No missing function is stubbed successfully. `open_memstream`, math exports,
the measured C++ runtime, DRM/sync seams and static build IDs remain explicit
follow-up work. Link success against an existing libc export is not proof that
the operation succeeds at runtime. G7l/G7m must establish that separately.

G7k build and inventory decisions — SAKER, 2026-10-31.

The original scaffold documentation follows.

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
104-byte result (private C ABI version11; older versions are refused). The C-owned report the
bridge fills is 240 bytes; `has_afbc` carries upstream's AFBC constant rather than a hardware
attestation, and G7o-a's block follows it. `with_api` lends an **existing** typed client for
synchronous serial queries. Each callback performs GetInfo, GetTopology, GetMmuFeatures,
GetTilerFeatures, GetTextureFeatures, GetCoreFeatures, GetThreadProperties then
GetMemoryProperties, preserving the same client and consuming eight request IDs on success.
The result is published only after all eight succeed; a failed later query leaves no partial
identity. NotSupported from an optional query is the one status that does not abort it;
core features and memory properties are required, so any failure of either aborts the whole
report, and any thread-query failure other than NotSupported aborts it too. The thread group is optional:
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
The scaffold C object refers to exactly nine upstream functions: `pan_query_core_count`,
`pan_query_core_id_range`, `pan_query_tiler_features`, `pan_query_compressed_formats`,
`pan_query_afrc`, `pan_query_afbc`, since G7g `pan_get_model`, and since G7o-a
`pan_query_l2_slices` and `pan_query_bus_width` (both in the already-linked `pan_props.c`).
All nine must appear defined in the final
native ELF, which has no unresolved symbols. G7h deliberately adds none: the thread
derivations are upstream's expressions applied to the attested words, and the optional
`pan_query_thread_tls_alloc` helper is not called, because the report already records its
value (`max_tls_instance_per_core`, the documented fallback to `max_threads_per_core`) as a
field rather than a returned device property. G7j's `pan_query_afbc` also lives in that same
`pan_props.c` and consumes upstream's constant `.afbc_features = 0`, so it likewise adds no
translation unit. G7d's
`pan_kmod_dev_query_user_va_range` is `static inline` in the pinned header, so it adds no
entry to that inventory; G7e's tiler helper is a real function in the already-compiled
`pan_props.c`, which is why the asserted set grew from two to three and no new translation
unit was added. G7j's `pan_query_afbc` also lives in that same `pan_props.c` and consumes
upstream's constant `.afbc_features = 0`, so it likewise adds no translation unit. G7g's
`pan_get_model` lives in `model/pan_model.c`, so that upstream
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
the raw four-word `TEXTURE_FEATURES[0..3]` array from GPU_CONTROL +0x0B0..+0x0BC under the same
optional rule; the driver reads all four words in the same powered block and the wire carries
them verbatim. Only element 0 is an absence gate: upstream returns it as the compressed-format
mask and tests bit 25, behind its own arch check, for AFRC, so a zero there means the backend
has no attestation, the query still succeeds with all four words zero, and the group stays in
the missing mask. Elements 1..3 cross verbatim, zero included — they are attested values, not
derived, and no consumer here decodes them. A successful query therefore attests the whole
group and missing-property bit 3 clears. Additive GetCoreFeatures
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
bit 4 stays set, marking the group unusable instead. The report grew to 168 bytes: the
enlarged 80-byte `kumo_gpu_info` shifts the whole report, so the five new fields sit at
offsets 144–160; the four bytes that were tail padding after G7h now carry G7j's `has_afbc`.

AFBC is the one discovery group with no hardware register at all. The pinned
`panthor_kmod.c` sets `.afbc_features = 0` as a constant with the comment "This register does
not exist because AFBC is no longer optional", so the bridge does the same and consumes it
through upstream's own `pan_query_afbc()`, which returns true for `pan_arch(gpu_id) >= 5` when
the property is 0. The boolean reaches the report as `has_afbc`, and missing-property bit 5
clears only when that helper returned true — never merely because the query succeeded. This is
service-side knowledge the way upstream has it, not an attested hardware word, so no wire
opcode or `Info` field changes for it.
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
| 2 | ~~mem_features, l2_features~~ supplied by G7o-a over GetMemoryProperties (mmu_features supplied by G7d); required, so the bit clears on every published report |
| 3 | ~~texture_features[4]~~ supplied by G7i — all four words read in one powered block; bit clears only when the query succeeded |
| 4 | ~~max_threads_per_core, max_tasks_per_core, max_threads_per_wg, num_registers_per_core, max_tls_instance_per_core~~ supplied by G7h; cleared only when all four upstream-asserted derivations are nonzero |
| 5 | ~~afbc_features~~ supplied by G7j as upstream's constant, consumed through `pan_query_afbc`; there is no hardware register |
| 6 | ~~gpu_can_query_timestamp, timestamp_device_coherent, timestamp_frequency, timestamp_cycles_to_ns_factor~~ G7o-a policy: no timestamps (the protocol has no timestamp query), all zero |
| 7 | ~~allowed_group_priorities_mask~~ G7o-a policy: MEDIUM only, because GroupCreate carries no priority |
| 8 | ~~supported_bo_flags, supported_vm_op_flags~~ G7o-a policy: EXECUTABLE, NO_MMAP, GPU_UNCACHED; no VM op flags |
| 9 | ~~is_io_coherent, pgsize_bitmap~~ G7o-a: upstream's `protocol != none` from the programmed protocol (false), and the protocol's 4 KiB granule |

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
