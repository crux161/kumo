# G7o-e — W^X against both Mesa drivers
<!-- j746 -->
<!-- j747 -->

Measured against the pinned Mesa 26.2.5 tarball (sha256 `794b3d17…a0a5`), v10/CSF paths of
Gallium panfrost and panvk. `python3 scripts/mesa-wx-audit.py` re-reads the tarball in place
and checks every row below; a Mesa bump that moves one fails the audit until this file is
measured again. The QEMU half is mode 8 of `python3 scripts/mesa-device-smoke.py --run`.
— TIMBERDOODLE

## The rule, on both sides

KUMO: a binding of an EXECUTABLE BO is READ_ONLY and executable, every other binding is
NO_EXECUTE and writable (`lib/kumo-gpu/src/bo.rs` `bind_flags`, the C mirror
`kumo_mesa_bo_bind_flags`). `Binding::validate` refuses a binding that is neither, so there
is no writable executable GPU mapping. drv-gpu turns READ_ONLY into AP[2] and NO_EXECUTE into
XN bits 53-54.

Panthor: `panthor_kmod.c:1205` maps an EXECUTABLE BO `MAP_READONLY` and everything else
`MAP_NOEXEC`. **This is the same derivation.** Both drivers already run under it on Linux,
where a GPU store into a shader BO faults.

## Executable BOs (every site naming the flag)

| Driver | Site | What it is | Flags on KUMO |
|---|---|---|---|
| Gallium | `pan_context.c:1195` `ctx->shaders` | per-context shader pool, 4 KiB slabs | EXECUTABLE |
| Gallium | `pan_screen.c:1134` `screen->mempools.bin` | preload, blend and precompiled-kernel binaries, 4 KiB slabs | EXECUTABLE |
| panvk | `panvk_vX_device.c:98` `dev->mempools.exec` | every shader binary, 16 KiB slabs | EXECUTABLE (WB_MMAP dropped) |

The other nine flag sites are definitions (`pan_bo.h:20`, `pan_kmod.h:95`), Gallium's
translation (`pan_bo.c:43-44`) and the Panthor/JM backends. No resource, VkDeviceMemory,
descriptor pool, CS ring or tiler heap is executable. Gallium's pools are not SHAREABLE, so
their BOs are exclusive to the context VM (`pan_bo.c`); panvk's go on the device VM.

panvk asks for `EXECUTABLE | WB_MMAP`; `panvk_device_adjust_bo_flags` (`panvk_device.h:166-171`)
drops WB_MMAP when `supported_bo_flags` lacks it, which is KUMO's G7o-a policy. Unmasked, the
request fails closed in the bridge before any wire I/O (device smoke case 41).

## What is written into them, and by whom

Every allocation from the three pools is a CPU copy of a finished binary:

| Driver | Upload | Source |
|---|---|---|
| Gallium | `pan_shader.c:279` `pan_pool_upload_aligned` | compiled shader variants (`panfrost_shader_get`, three callers) |
| Gallium | `pan_blend_cso.c:124` alloc + memcpy | blend shaders |
| Gallium | `pan_precomp.c:74` alloc + memcpy | precompiled CL kernels |
| Gallium | `pan_fb_preload.c:556` `pan_pool_upload_aligned` | framebuffer preload shaders |
| panvk | `panvk_vX_shader.c:1168` and `:2539` `panvk_pool_upload_aligned` | compiled and deserialized shaders, precomp included |

Descriptors that point at the code (RSD, SPD, blend) go into separate non-executable pools
(`ctx->descs`, `mempools.desc`, `mempools.rw`). **No executable BO is a GPU write target.**
Both pools are bump allocators that never rewind inside a BO, so code bytes are written once,
before the first submit that names them. Gallium's `gpu_access |= PAN_BO_ACCESS_RW` on pool
BOs is dependency tracking, not a mapping permission.

CSF command streams, panvk's tiler-OOM handlers and provoking-vertex functions are CS
instructions, read as data by the command-stream front end. They live in non-executable BOs,
as DESIGN/021 allows ("command streams are data reads and can be NO_EXECUTE").

## CPU mappings

Every executable BO is mapped `PROT_READ | PROT_WRITE, MAP_SHARED, NULL` (Gallium
`pan_bo.c:329`, panvk `panvk_priv_bo.c:43`). That is the one shape KUMO's mapper accepts, as a
Normal-NC data mapping. `PROT_EXEC` appears nowhere in either driver; the bridge refuses it
(case 43). Neither executable pool asks NO_MMAP or GPU_UNCACHED.

Two non-executable panvk shapes fall outside the mapper. They are not W^X findings, but VK2
must settle them:

- `vkMapMemory2` with `VK_MEMORY_MAP_PLACED_BIT_EXT` (`panvk_device_memory.c:364`): `MAP_FIXED`
  at a caller address, for VK_EXT_map_memory_placed;
- `panvk_host_copy.c:233`: `PROT_READ`-only or `PROT_WRITE`-only plane maps, for
  VK_EXT_host_image_copy.

panvk advertises both extensions. VK2 either masks them or grows the mapper; it must not
widen a protection silently. Gallium has neither shape.

## One 4 GiB code region

Valhall's `Blend Shader` descriptor (`genxml/v10.xml:1022`) keeps a 28-bit PC shifted by 4:
the low 32 bits of the blend shader's address. The top 32 bits come from the fragment shader.

- panvk asserts it (`panvk_vX_blend.c:141`).
- **Gallium on v10 truncates without a check** (`pan_cmdstream.c:376`). The matching assert is
  compiled only for `PAN_ARCH <= 7`, and its TODO says the placement is not guaranteed.
- Its blend shaders (`mempools.bin`) and fragment shaders (`ctx->shaders`) are different
  BOs, so the VM placement alone keeps them together.

Panthor's auto-VA places every executable BO in the first 4 GiB-aligned window of the user
range (`panthor_kmod.c:834`, `:970`), and non-executable BOs prefer the space above it.

KUMO's window, `0x2000_0000..0x3000_0000`, is one 4 GiB region. G7o-e makes that an invariant:
- a const assertion in `lib/kumo-gpu/src/lib.rs`;
- `pan_kmod_kumo_dev_create` refuses a reported window that crosses a 4 GiB boundary rather
  than publish it (device smoke case 39);
- the audit re-reads the constants.

The automatic VA allocator of G7p/GL1 inherits the rule. So does any later raise of the
window.

## What this does not cover

- Permissions only. Cache maintenance for recycled code is not measured here. A freed BO's
  VA, or its pages after the J743 free list returns them zeroed, can carry new code. GPU-side
  invalidation at that reuse belongs to drv-gpu's unbind and to the submit path (G7p/GL1).
- Shader execution through a Mesa-made binding. Execution from a READ_ONLY executable
  binding is metal-proven by G6c's client compute dispatch (J596, DESIGN/021), which ran its
  256-byte binary from a read-only, executable, cached binding at `0x2000_2000` on the Orange
  Pi with zero mismatches. G7o-f did not repeat it through the bridge.

## On metal (J747)

G7o-f's live client bound a 4 KiB EXECUTABLE BO through `pan_kmod_vm_bind`, then aimed a CS
store at it. The Orange Pi's MMU refused it:
`AS1 fault at VA=0x0000000020100080: PERMISSION_FAULT Write`.

- drv-gpu retired the job after its 1 s work timeout.
- It contained the close: `reset=ok buffers=5 bindings=5 waits=0 retained=no`.
- The executable bytes were unchanged through the CPU map.

The rule holds in hardware, not only in the bind flags.
