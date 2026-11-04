# G7k — measured Mesa driver link inventory

Mesa 26.2.5, aarch64-unknown-none, KUMO relibc. These incomplete probe ELFs must never be executed.

All upstream per-architecture variants are built; v10 is present in both links. The legacy JM kernel backend `panfrost_kmod.c` is excluded on KUMO; Panthor remains fully inventoried.

| Driver | Required gaps | Optional weak symbols | Extracted objects | Probe ELF bytes |
|---|---:|---:|---:|---:|
| GL | 39 | 1 | 938 | 18559488 |
| VK | 31 | 6011 | 734 | 16565264 |

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
| `build_id_find_nhdr_for_addr` | VK | other: static build ID | `build/src/panfrost/vulkan/libvulkan_panfrost.a(panvk_instance.c.o)` |
| `build_id_length` | VK | other: static build ID | `build/src/panfrost/vulkan/libvulkan_panfrost.a(panvk_instance.c.o)` |
| `copy_build_id_to_sha1` | VK | other: static build ID | `build/src/panfrost/vulkan/libvulkan_panfrost.a(panvk_instance.c.o)` |
| `drmCloseBufferHandle` | GL/VK | DRM / libdrm | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/pan_kmod.c.o)` |
| `drmFreeDevices` | VK | DRM / libdrm | `build/src/panfrost/vulkan/libvulkan_panfrost.a(vk_instance.c.o)` |
| `drmFreeVersion` | GL | DRM / libdrm | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/pan_kmod.c.o)` |
| `drmGetCap` | VK | DRM / libdrm | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmGetDevices2` | VK | DRM / libdrm | `build/src/panfrost/vulkan/libvulkan_panfrost.a(vk_instance.c.o)` |
| `drmGetVersion` | GL | DRM / libdrm | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/pan_kmod.c.o)` |
| `drmIoctl` | GL/VK | Panthor ioctl user | `build/src/gallium/auxiliary/libgallium.a(libgallium.a.p/renderonly_renderonly.c.o)` |
| `drmPrimeFDToHandle` | GL/VK | DRM / libdrm | `build/src/gallium/auxiliary/libgallium.a(libgallium.a.p/renderonly_renderonly.c.o)` |
| `drmPrimeHandleToFD` | GL/VK | DRM / libdrm | `build/src/gallium/auxiliary/libgallium.a(libgallium.a.p/renderonly_renderonly.c.o)` |
| `drmSyncobjCreate` | GL/VK | syncobj / sync_file | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/panthor_kmod.c.o)` |
| `drmSyncobjDestroy` | GL/VK | syncobj / sync_file | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/panthor_kmod.c.o)` |
| `drmSyncobjExportSyncFile` | GL/VK | syncobj / sync_file | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/panthor_kmod.c.o)` |
| `drmSyncobjFDToHandle` | VK | syncobj / sync_file | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmSyncobjHandleToFD` | VK | syncobj / sync_file | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmSyncobjImportSyncFile` | GL/VK | syncobj / sync_file | `build/src/gallium/drivers/panfrost/libpanfrost-v4.a(libpanfrost-v4.a.p/pan_jm.c.o)` |
| `drmSyncobjQuery2` | VK | syncobj / sync_file | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmSyncobjReset` | GL/VK | syncobj / sync_file | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/panthor_kmod.c.o)` |
| `drmSyncobjSignal` | VK | syncobj / sync_file | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmSyncobjTimelineSignal` | VK | syncobj / sync_file | `build/src/panfrost/vulkan/libvulkan_panfrost.a(u_sync_provider.c.o)` |
| `drmSyncobjTimelineWait` | GL/VK | syncobj / sync_file | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/panthor_kmod.c.o)` |
| `drmSyncobjTransfer` | VK | syncobj / sync_file | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/panthor_kmod.c.o)` |
| `drmSyncobjWait` | GL/VK | syncobj / sync_file | `build/src/gallium/drivers/panfrost/libpanfrost-v4.a(libpanfrost-v4.a.p/pan_jm.c.o)` |
| `pan_kmod_kumo_bo_attach_sync_point` | GL | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `pan_kmod_kumo_bo_mmap` | GL/VK | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost.a(libpanfrost.a.p/pan_bo.c.o)` |
| `pan_kmod_kumo_bo_munmap` | GL | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost.a(libpanfrost.a.p/pan_bo.c.o)` |
| `pan_kmod_kumo_cs_reg_count` | GL | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `pan_kmod_kumo_csif_props` | VK | other: native capability backend | `build/src/panfrost/lib/kmod/libpankmod_lib.a(libpankmod_lib.a.p/panthor_kmod.c.o)` |
| `pan_kmod_kumo_dev_lost` | GL/VK | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `pan_kmod_kumo_heap_create` | GL/VK | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `pan_kmod_kumo_heap_destroy` | GL/VK | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `pan_kmod_kumo_native` | GL/VK | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `pan_kmod_kumo_snapshot_wait` | GL/VK | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost.a(libpanfrost.a.p/pan_fence.c.o)` |
| `pan_kmod_kumo_sync_create` | GL | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost.a(libpanfrost.a.p/pan_context.c.o)` |
| `pan_kmod_kumo_sync_destroy` | GL/VK | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `pan_kmod_kumo_sync_snapshot` | GL | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost.a(libpanfrost.a.p/pan_fence.c.o)` |
| `pan_kmod_kumo_vm_sync_lock` | GL | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `pan_kmod_kumo_vm_sync_unlock` | GL | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |
| `pan_kmod_kumo_vm_wait` | GL | other: native capability backend | `build/src/gallium/drivers/panfrost/libpanfrost-v10.a(libpanfrost-v10.a.p/pan_csf.c.o)` |

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
| `build/src/gallium/drivers/panfrost/libpanfrost.a` | 5192 | — |
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
| `build/src/panfrost/shared/libpanfrost_shared.a` | 552 | 552 |
| `build/src/panfrost/vulkan/libpanvk_v10.a` | — | 8252 |
| `build/src/panfrost/vulkan/libpanvk_v12.a` | — | 8252 |
| `build/src/panfrost/vulkan/libpanvk_v13.a` | — | 8252 |
| `build/src/panfrost/vulkan/libpanvk_v14.a` | — | 8172 |
| `build/src/panfrost/vulkan/libpanvk_v6.a` | — | 7146 |
| `build/src/panfrost/vulkan/libpanvk_v7.a` | — | 7146 |
| `build/src/panfrost/vulkan/libvulkan_panfrost.a` | — | 30753708 |
| `build/src/util/libxmlconfig.a` | 488 | — |
| `sysroot/lib/libc.a` | 6772560 | 6772560 |

No required pthread creation, dl, expat or driconf symbol remains unresolved in these links. This is link closure evidence only: existing libc exports can still refuse operations at runtime. G7l/G7m must prove their behavior.

The plan's `_glapi_get_dispatch` is named `_mesa_glapi_get_dispatch` in this pinned source. GL retains that actual symbol, `panfrost_create_screen`, `st_api_create_context` and `glClear`; Vulkan retains `vk_icdGetInstanceProcAddr`.
