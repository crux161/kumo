# G7k — measured Mesa driver link inventory

Mesa 26.2.5, aarch64-unknown-none, KUMO relibc. These incomplete probe ELFs must never be executed.

All upstream per-architecture variants are built; v10 is present in both links. The legacy JM kernel backend `panfrost_kmod.c` is excluded on KUMO; Panthor remains fully inventoried.

| Driver | Required gaps | Optional weak symbols | Extracted objects | Probe ELF bytes |
|---|---:|---:|---:|---:|
| GL | 53 | 1 | 939 | 18529504 |
| VK | 44 | 6292 | 635 | 16049688 |

Optional generated panvk dispatch entries and the GL TLS wrapper are verified against weak references in extracted objects. Every name and owner is in `optional-undefined.tsv`; every required and optional entry is in `inventory.json`. Local undefined symbol-table placeholders are included in this accounting.

| Required symbol | Driver | Classification | Requesting archive(object), first owner |
|---|---|---|---|
| `_ZNSt3__112__next_primeEm` | GL | C++ runtime | `build/src/mesa/glapi/es2api/libGLESv2.a(texcompress_astc_luts.cpp.o)` |
| `_ZNSt3__122__libcpp_verbose_abortEPKcz` | GL | C++ runtime | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/lower_precision.cpp.o)` |
| `_ZNSt3__15mutex4lockEv` | GL | C++ runtime | `build/src/mesa/glapi/es2api/libGLESv2.a(texcompress_astc_luts.cpp.o)` |
| `_ZNSt3__15mutex6unlockEv` | GL | C++ runtime | `build/src/mesa/glapi/es2api/libGLESv2.a(texcompress_astc_luts.cpp.o)` |
| `_ZNSt3__15mutexD1Ev` | GL | C++ runtime | `build/src/mesa/glapi/es2api/libGLESv2.a(texcompress_astc_luts.cpp.o)` |
| `_ZdaPv` | GL | C++ runtime | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/opt_function_inlining.cpp.o)` |
| `_ZdlPvm` | GL | C++ runtime | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/glsl_to_nir.cpp.o)` |
| `_Znam` | GL | C++ runtime | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/opt_function_inlining.cpp.o)` |
| `_Znwm` | GL | C++ runtime | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/lower_precision.cpp.o)` |
| `__cxa_guard_acquire` | GL | C++ runtime | `build/src/mesa/glapi/es2api/libGLESv2.a(texcompress_astc_luts.cpp.o)` |
| `__cxa_guard_release` | GL | C++ runtime | `build/src/mesa/glapi/es2api/libGLESv2.a(texcompress_astc_luts.cpp.o)` |
| `acosf` | GL | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `asinf` | GL | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `atan2f` | GL | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `atanf` | GL | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `build_id_find_nhdr_for_addr` | VK | other: static build ID | `build/src/panfrost/vulkan/libvulkan_panfrost.a(panvk_instance.c.o)` |
| `build_id_length` | VK | other: static build ID | `build/src/panfrost/vulkan/libvulkan_panfrost.a(panvk_instance.c.o)` |
| `copy_build_id_to_sha1` | VK | other: static build ID | `build/src/panfrost/vulkan/libvulkan_panfrost.a(panvk_instance.c.o)` |
| `cos` | GL/VK | relibc gap | `build/src/compiler/nir/libnir.a(libnir.a.p/meson-generated_.._nir_constant_expressions.c.o)` |
| `cosf` | GL/VK | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `drmCloseBufferHandle` | GL/VK | DRM / libdrm | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/pan_kmod.c.o)` |
| `drmFreeDevice` | VK | DRM / libdrm | `build/src/panfrost/vulkan/libvulkan_panfrost.a(panvk_wsi.c.o)` |
| `drmFreeDevices` | VK | DRM / libdrm | `build/src/panfrost/vulkan/libvulkan_panfrost.a(vk_instance.c.o)` |
| `drmFreeVersion` | GL/VK | DRM / libdrm | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/pan_kmod.c.o)` |
| `drmGetCap` | VK | DRM / libdrm | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmGetDevice2` | VK | DRM / libdrm | `build/src/panfrost/vulkan/libvulkan_panfrost.a(panvk_wsi.c.o)` |
| `drmGetDevices2` | VK | DRM / libdrm | `build/src/panfrost/vulkan/libvulkan_panfrost.a(vk_instance.c.o)` |
| `drmGetVersion` | GL/VK | DRM / libdrm | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/pan_kmod.c.o)` |
| `drmIoctl` | GL/VK | Panthor ioctl user | `build/src/gallium/auxiliary/libgallium.a(libgallium.a.p/renderonly_renderonly.c.o)` |
| `drmPrimeFDToHandle` | GL | DRM / libdrm | `build/src/gallium/auxiliary/libgallium.a(libgallium.a.p/renderonly_renderonly.c.o)` |
| `drmPrimeHandleToFD` | GL/VK | DRM / libdrm | `build/src/gallium/auxiliary/libgallium.a(libgallium.a.p/renderonly_renderonly.c.o)` |
| `drmSyncobjCreate` | GL/VK | syncobj / sync_file | `build/src/gallium/drivers/panfrost/libpanfrost.a(libpanfrost.a.p/pan_context.c.o)` |
| `drmSyncobjDestroy` | GL/VK | syncobj / sync_file | `build/src/gallium/drivers/panfrost/libpanfrost.a(libpanfrost.a.p/pan_context.c.o)` |
| `drmSyncobjExportSyncFile` | GL/VK | syncobj / sync_file | `build/src/gallium/drivers/panfrost/libpanfrost.a(libpanfrost.a.p/pan_context.c.o)` |
| `drmSyncobjFDToHandle` | GL/VK | syncobj / sync_file | `build/src/gallium/drivers/panfrost/libpanfrost.a(libpanfrost.a.p/pan_fence.c.o)` |
| `drmSyncobjHandleToFD` | VK | syncobj / sync_file | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmSyncobjImportSyncFile` | GL/VK | syncobj / sync_file | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `drmSyncobjQuery2` | VK | syncobj / sync_file | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmSyncobjReset` | GL/VK | syncobj / sync_file | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/panthor_kmod.c.o)` |
| `drmSyncobjSignal` | VK | syncobj / sync_file | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmSyncobjTimelineSignal` | VK | syncobj / sync_file | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmSyncobjTimelineWait` | GL/VK | syncobj / sync_file | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `drmSyncobjTransfer` | GL/VK | syncobj / sync_file | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `drmSyncobjWait` | GL/VK | syncobj / sync_file | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `exp2f` | GL/VK | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `expf` | GL | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `frexp` | GL/VK | relibc gap | `build/src/compiler/nir/libnir.a(libnir.a.p/meson-generated_.._nir_constant_expressions.c.o)` |
| `frexpf` | GL/VK | relibc gap | `build/src/compiler/nir/libnir.a(libnir.a.p/nir_lower_flrp.c.o)` |
| `ldexp` | GL/VK | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `ldexpf` | GL/VK | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `llrint` | GL | relibc gap | `build/src/gallium/auxiliary/libgallium.a(libgallium.a.p/util_u_surface.c.o)` |
| `llround` | GL | relibc gap | `build/src/mesa/libmesa.a(libmesa.a.p/main_get.c.o)` |
| `llroundf` | GL/VK | relibc gap | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_cmdstream.c.o)` |
| `log` | GL/VK | relibc gap | `build/src/compiler/spirv/libvtn.a(libvtn.a.p/vtn_opencl.c.o)` |
| `log10` | GL/VK | relibc gap | `build/src/compiler/nir/libnir.a(libnir.a.p/nir_print.c.o)` |
| `log2f` | GL/VK | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `logf` | GL/VK | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `lrint` | GL | relibc gap | `build/src/gallium/auxiliary/libgallium.a(libgallium.a.p/util_u_surface.c.o)` |
| `lrintf` | GL/VK | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `lroundf` | GL | relibc gap | `build/src/mesa/libmesa.a(libmesa.a.p/main_drawpix.c.o)` |
| `open_memstream` | GL/VK | relibc gap | `build/src/mesa/glapi/es2api/libGLESv2.a(memstream.c.o)` |
| `pow` | GL/VK | relibc gap | `build/src/compiler/nir/libnir.a(libnir.a.p/meson-generated_.._nir_constant_expressions.c.o)` |
| `powf` | GL/VK | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |
| `sin` | GL/VK | relibc gap | `build/src/compiler/nir/libnir.a(libnir.a.p/meson-generated_.._nir_constant_expressions.c.o)` |
| `sinf` | GL/VK | relibc gap | `build/src/compiler/glsl/libglsl.a(libglsl.a.p/ir_constant_expression.cpp.o)` |

## Archive sizes

Object text/data/BSS sizes for each driver are in `link-{gl,vk}.object-sizes`; the extracted-member trace and map preserve the exact link graph. Full archive sizes include unextracted members.

| Archive | GL bytes | VK bytes |
|---|---:|---:|
| `build/src/c11/impl/libmesa_util_c11.a` | 246 | 246 |
| `build/src/compiler/glsl/glcpp/libglcpp.a` | 1374 | — |
| `build/src/compiler/glsl/libglsl.a` | 48490 | — |
| `build/src/compiler/libcompiler.a` | 13436 | 13436 |
| `build/src/compiler/nir/libnir.a` | 42090 | 42090 |
| `build/src/compiler/spirv/libvtn.a` | 4716 | 4716 |
| `build/src/gallium/auxiliary/libgallium.a` | 37586 | — |
| `build/src/gallium/drivers/panfrost/libpanfrost-v10.a` | 2318 | — |
| `build/src/gallium/drivers/panfrost/libpanfrost-v12.a` | 2318 | — |
| `build/src/gallium/drivers/panfrost/libpanfrost-v13.a` | 2318 | — |
| `build/src/gallium/drivers/panfrost/libpanfrost-v14.a` | 2318 | — |
| `build/src/gallium/drivers/panfrost/libpanfrost-v4.a` | 2194 | — |
| `build/src/gallium/drivers/panfrost/libpanfrost-v5.a` | 2194 | — |
| `build/src/gallium/drivers/panfrost/libpanfrost-v6.a` | 2194 | — |
| `build/src/gallium/drivers/panfrost/libpanfrost-v7.a` | 2194 | — |
| `build/src/gallium/drivers/panfrost/libpanfrost-v9.a` | 2194 | — |
| `build/src/gallium/drivers/panfrost/libpanfrost.a` | 5130 | — |
| `build/src/mesa/glapi/es2api/libGLESv2.a` | 6198492 | — |
| `build/src/mesa/libmesa.a` | 197428 | — |
| `build/src/panfrost/compiler/bifrost/libpanfrost_bifrost.a` | 8020 | 8020 |
| `build/src/panfrost/compiler/bifrost/libpanfrost_bifrost_disasm.a` | 664 | 664 |
| `build/src/panfrost/compiler/bifrost/valhall/libpanfrost_valhall_disasm.a` | 306 | 306 |
| `build/src/panfrost/compiler/libpanfrost_compiler.a` | 3446 | 3446 |
| `build/src/panfrost/compiler/midgard/libpanfrost_midgard.a` | 4790 | 4790 |
| `build/src/panfrost/compiler/midgard/libpanfrost_midgard_disasm.a` | 638 | 638 |
| `build/src/panfrost/genxml/libpandecode-arch-v10.a` | 940 | 940 |
| `build/src/panfrost/genxml/libpandecode-arch-v12.a` | 904 | 904 |
| `build/src/panfrost/genxml/libpandecode-arch-v13.a` | 904 | 904 |
| `build/src/panfrost/genxml/libpandecode-arch-v14.a` | 882 | 882 |
| `build/src/panfrost/genxml/libpandecode-arch-v4.a` | 550 | 550 |
| `build/src/panfrost/genxml/libpandecode-arch-v5.a` | 622 | 622 |
| `build/src/panfrost/genxml/libpandecode-arch-v6.a` | 714 | 714 |
| `build/src/panfrost/genxml/libpandecode-arch-v7.a` | 714 | 714 |
| `build/src/panfrost/genxml/libpandecode-arch-v9.a` | 888 | 888 |
| `build/src/panfrost/genxml/libpanfrost_decode.a` | 746 | 746 |
| `build/src/panfrost/lib/kmod/libpankmod_lib.a` | 788 | 788 |
| `build/src/panfrost/lib/libpan-arch-v10.a` | 2416 | 2416 |
| `build/src/panfrost/lib/libpan-arch-v12.a` | 2416 | 2416 |
| `build/src/panfrost/lib/libpan-arch-v13.a` | 2416 | 2416 |
| `build/src/panfrost/lib/libpan-arch-v14.a` | 2416 | 2416 |
| `build/src/panfrost/lib/libpan-arch-v4.a` | 1456 | 1456 |
| `build/src/panfrost/lib/libpan-arch-v5.a` | 1950 | 1950 |
| `build/src/panfrost/lib/libpan-arch-v6.a` | 2090 | 2090 |
| `build/src/panfrost/lib/libpan-arch-v7.a` | 2162 | 2162 |
| `build/src/panfrost/lib/libpan-arch-v9.a` | 2132 | 2132 |
| `build/src/panfrost/lib/libpan-format-v10.a` | 314 | 314 |
| `build/src/panfrost/lib/libpan-format-v12.a` | 314 | 314 |
| `build/src/panfrost/lib/libpan-format-v13.a` | 314 | 314 |
| `build/src/panfrost/lib/libpan-format-v14.a` | 314 | 314 |
| `build/src/panfrost/lib/libpan-format-v5.a` | 314 | 314 |
| `build/src/panfrost/lib/libpan-format-v6.a` | 282 | 282 |
| `build/src/panfrost/lib/libpan-format-v7.a` | 312 | 312 |
| `build/src/panfrost/lib/libpan-format-v9.a` | 282 | 282 |
| `build/src/panfrost/lib/libpanfrost_lib.a` | 2408 | 2408 |
| `build/src/panfrost/model/libpanfrost_model_lib.a` | 272 | 272 |
| `build/src/panfrost/shared/libpanfrost_shared.a` | 552 | — |
| `build/src/panfrost/vulkan/libpanvk_v10.a` | — | 8252 |
| `build/src/panfrost/vulkan/libpanvk_v12.a` | — | 8252 |
| `build/src/panfrost/vulkan/libpanvk_v13.a` | — | 8252 |
| `build/src/panfrost/vulkan/libpanvk_v14.a` | — | 8172 |
| `build/src/panfrost/vulkan/libpanvk_v6.a` | — | 7146 |
| `build/src/panfrost/vulkan/libpanvk_v7.a` | — | 7146 |
| `build/src/panfrost/vulkan/libvulkan_panfrost.a` | — | 30757842 |
| `build/src/util/libxmlconfig.a` | 488 | — |
| `sysroot/lib/libc.a` | 6687464 | 6687464 |

No required pthread creation, dl, expat or driconf symbol remains unresolved in these links. This is link closure evidence only: existing libc exports can still refuse operations at runtime. G7l/G7m must prove their behavior.

The plan's `_glapi_get_dispatch` is named `_mesa_glapi_get_dispatch` in this pinned source. GL retains that actual symbol, `panfrost_create_screen`, `st_api_create_context` and `glClear`; Vulkan retains `vk_icdGetInstanceProcAddr`.
