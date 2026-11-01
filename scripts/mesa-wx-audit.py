#!/usr/bin/env python3
#j746
"""Measure Gallium panfrost's and panvk's executable BOs against KUMO's GPU W^X rule (G7o-e).

Reads the pinned Mesa 26.2.5 tarball in place: no extraction, container, network, image or
hardware. Each check below is one claim of toolchain/mesa/WX-INVENTORY.md. A Mesa bump that
moves any of them fails here until the inventory is measured again.
"""
from collections import Counter
import hashlib
from pathlib import Path
import re
import tarfile

ROOT = Path(__file__).resolve().parent.parent
TAR = ROOT / "build/mesa-native/mesa.tar.xz"
TAR_SHA = "794b3d1729075dae7ebd47c7283b7ca0cdb5dec668f7f11cc0a5d82f6138a0a5"
TREES = ("src/gallium/drivers/panfrost/", "src/gallium/winsys/panfrost/",
         "src/panfrost/vulkan/", "src/panfrost/lib/")
GL, VK, KMOD = "src/gallium/drivers/panfrost/", "src/panfrost/vulkan/", "src/panfrost/lib/kmod/"
GREEN = ("G7o-e W^X audit green: Gallium 2 and panvk 1 executable pools, CPU-written shader "
         "binaries only, RW shared maps, Panthor's read-only rule, one 4 GiB code region")

# Every line in either driver that names the executable flag, by role. — TIMBERDOODLE
EXECUTABLE_SITES = {
    (GL + "pan_bo.h", "#define PAN_BO_EXECUTE (1 << 0)"): "definition",
    (GL + "pan_bo.c", "if (flags & PAN_BO_EXECUTE)"): "translation",
    (GL + "pan_bo.c", "kmod_bo_flags |= PAN_KMOD_BO_FLAG_EXECUTABLE;"): "translation",
    (GL + "pan_context.c", "panfrost_pool_init(&ctx->shaders, ctx, dev, PAN_BO_EXECUTE, 4096,"): "pool",
    (GL + "pan_screen.c", "if (panfrost_pool_init(&screen->mempools.bin, NULL, dev, PAN_BO_EXECUTE,"): "pool",
    (VK + "panvk_vX_device.c", "dev, PAN_KMOD_BO_FLAG_EXECUTABLE | PAN_KMOD_BO_FLAG_WB_MMAP),"): "pool",
    (KMOD + "pan_kmod.h", "PAN_KMOD_BO_FLAG_EXECUTABLE = BITFIELD_BIT(0),"): "definition",
    (KMOD + "panthor_kmod.c", ".supported_bo_flags = PAN_KMOD_BO_FLAG_EXECUTABLE |"): "backend",
    (KMOD + "panthor_kmod.c", "if (!(bo->flags & PAN_KMOD_BO_FLAG_EXECUTABLE)) {"): "backend",
    (KMOD + "panthor_kmod.c", "if (ops[i].map.bo->flags & PAN_KMOD_BO_FLAG_EXECUTABLE)"): "backend",
    (KMOD + "panfrost_kmod.c", "props->supported_bo_flags = PAN_KMOD_BO_FLAG_EXECUTABLE |"): "backend (JM)",
    (KMOD + "panfrost_kmod.c", "if (!(flags & PAN_KMOD_BO_FLAG_EXECUTABLE))"): "backend (JM)",
}

# Every line that reaches one of the three executable pools: their setup, their teardown and
# the only allocations from them, each a CPU copy of a compiled binary.
POOL_SITES = Counter({
    (GL + "pan_context.c", "panfrost_pool_cleanup(&panfrost->shaders);"): 1,
    (GL + "pan_shader.c", "panfrost_shader_get(ctx->base.screen, &ctx->shaders, &ctx->descs, uncompiled,"): 1,
    (GL + "pan_shader.c", "panfrost_shader_get(ctx->base.screen, &ctx->shaders, &ctx->descs, so,"): 1,
    (GL + "pan_shader.c", "panfrost_shader_get(pctx->screen, &ctx->shaders, &ctx->descs, so,"): 1,
    (GL + "pan_shader.c", "pan_pool_upload_aligned(&shader_pool->base, res.binary.data,"): 1,
    (GL + "pan_screen.c", "panfrost_pool_cleanup(&screen->mempools.bin);"): 1,
    (GL + "pan_cmdstream.c", "&screen->mempools.bin.base);"): 1,
    (GL + "pan_cmdstream.c", "&screen->mempools.bin.base, &screen->mempools.desc.base);"): 1,
    (GL + "pan_precomp.c", "cache->bin_pool = &screen->mempools.bin.base;"): 1,
    (GL + "pan_precomp.c", "pan_pool_alloc_aligned(cache->bin_pool, info->binary_size, 64);"): 1,
    (GL + "pan_blend_cso.c", "cache->bin_pool = bin_pool;"): 1,
    (GL + "pan_blend_cso.c", "pan_pool_alloc_aligned(cache->bin_pool, binary.size, 64);"): 1,
    (GL + "pan_fb_preload.c", "cache->shaders.pool = bin_pool;"): 1,
    (GL + "pan_fb_preload.c", "pan_pool_upload_aligned(cache->shaders.pool, binary.data,"): 1,
    (VK + "panvk_vX_device.c", "panvk_pool_init(&dev->mempools.exec, dev, NULL, NULL, &exec_pool_props);"): 1,
    (VK + "panvk_vX_device.c", "panvk_pool_cleanup(&dev->mempools.exec);"): 1,
    (VK + "panvk_vX_shader.c", "&dev->mempools.exec, shader->bin_ptr, shader->bin_size, 128);"): 1,
    (VK + "panvk_vX_shader.c", "shader->code_mem = panvk_pool_upload_aligned(&dev->mempools.exec,"): 1,
})
POOL_PATTERN = re.compile(r"&ctx->shaders|&panfrost->shaders|mempools\.(bin|exec)\b|\bbin_pool\b|"
                          r"shaders\.pool\b|&shader_pool->base")
# The two allocate-then-copy sites copy the whole binary on the next statement.
COPIES = {GL + "pan_precomp.c": "memcpy(bin.cpu, binary, info->binary_size);",
          GL + "pan_blend_cso.c": "memcpy(bin.cpu, binary.data, binary.size);"}

# Every CPU mapping either driver makes, with its exact protection, flags and placement.
MAPS = Counter({
    (GL + "pan_bo.c", "bo->kmod_bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL"): 1,
    (VK + "panvk_priv_bo.c", "bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL"): 1,
    (VK + "csf/panvk_vX_gpu_queue.c", "ringbuf->bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL"): 1,
    (VK + "csf/panvk_vX_gpu_queue.c", "subq->tracebuf.bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL"): 1,
    (VK + "panvk_device_memory.c", "mem->bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL"): 1,
    (VK + "panvk_device_memory.c", "mem->bo, PROT_READ | PROT_WRITE, map_flags, host_addr"): 1,
    (VK + "panvk_host_copy.c", "img->planes[p].mem->bo, prot, MAP_SHARED, NULL"): 1,
})

# Text that must appear verbatim (whitespace-normalized) in the pinned source.
REQUIRED = [
    # Panthor's bind rule: executable means read-only, anything else no-execute.
    (KMOD + "panthor_kmod.c", "if (ops[i].map.bo->flags & PAN_KMOD_BO_FLAG_EXECUTABLE) "
     "bind_ops[i].flags |= DRM_PANTHOR_VM_BIND_OP_MAP_READONLY; else "
     "bind_ops[i].flags |= DRM_PANTHOR_VM_BIND_OP_MAP_NOEXEC;"),
    # The pools allocate through the BO paths whose mappings are listed above.
    (GL + "pan_mempool.c", "panfrost_bo_create(pool->dev, bo_sz, pool->create_flags, pool->label);"),
    (VK + "panvk_mempool.c", "panvk_priv_bo_create(pool->dev, bo_sz, pool->props.create_flags,"),
    (GL + "pan_bo.c", "struct pan_kmod_vm *exclusive_vm = !(flags & PAN_BO_SHAREABLE) ? dev->kmod.vm : NULL;"),
    # panvk drops WB_MMAP when the device does not offer it, so its executable pool is
    # EXECUTABLE alone on KUMO.
    (VK + "panvk_device.h", "const bool allow_wb_mmap = (pdev->kmod.dev->props.supported_bo_flags & "
     "PAN_KMOD_BO_FLAG_WB_MMAP) && !PANVK_DEBUG(DUMP) && !PANVK_DEBUG(NO_WB_MMAP); "
     "if (!allow_wb_mmap) bo_flags &= ~PAN_KMOD_BO_FLAG_WB_MMAP;"),
    # Valhall keeps 32 bits of a blend shader's PC; panvk asserts the shared 4 GiB region,
    # Gallium on v10 truncates without a check, and Panthor's auto-VA places every
    # executable BO in the first 4 GiB-aligned window of the user range.
    ("src/panfrost/genxml/v10.xml", '<struct name="Blend Shader" size="2"> '
     '<field name="PC" size="28" start="1:4" type="uint" modifier="shr(4)"/>'),
    (VK + "panvk_vX_blend.c", "assert((blend_shader >> 32) == (fs_code >> 32));"),
    (VK + "panvk_vX_blend.c", "cfg.internal.shader.pc = (uint32_t)blend_shader;"),
    (GL + "pan_cmdstream.c", "#if PAN_ARCH <= 7 /* The blend shader's address needs to be at "
     "* the same top 32 bit as the fragment shader."),
    (GL + "pan_cmdstream.c", "cfg.shader.pc = (uint32_t)blend_shaders[i];"),
    (KMOD + "panthor_kmod.c", "uint64_t any_heap_va_end = MIN2(user_va_end, "
     "ALIGN_POT(user_va_start + 1, 1ull << 32));"),
]


def normalize(text):
    return " ".join(text.split())


def load():
    digest = hashlib.sha256(TAR.read_bytes()).hexdigest()
    assert digest == TAR_SHA, f"{TAR} is not the pinned Mesa 26.2.5 tarball"
    files = {}
    with tarfile.open(TAR, "r:xz") as archive:
        for member in archive:
            name = member.name.split("/", 1)[1] if "/" in member.name else ""
            wanted = name.startswith(TREES) and name.endswith((".c", ".h", ".cpp"))
            if member.isfile() and (wanted or name == "src/panfrost/genxml/v10.xml"):
                files[name] = archive.extractfile(member).read().decode()
    return files


def lines(files):
    for path, text in files.items():
        if path.endswith(".xml"):
            continue
        for number, line in enumerate(text.splitlines(), 1):
            yield path, number, line.strip()


def mmap_calls(text):
    for match in re.finditer(r"pan_kmod_bo_mmap\(", text):
        depth, end = 1, match.end()
        while depth:
            depth += {"(": 1, ")": -1}.get(text[end], 0)
            end += 1
        yield normalize(text[match.end():end - 1])


def main():
    files = load()
    failures = []

    found = Counter((p, l) for p, _, l in lines(files)
                    if re.search(r"\bPAN_BO_EXECUTE\b|\bPAN_KMOD_BO_FLAG_EXECUTABLE\b", l))
    expected = Counter(EXECUTABLE_SITES.keys())
    if found != expected:
        failures.append(f"executable flag sites changed: {sorted((found - expected).elements())} "
                        f"new, {sorted((expected - found).elements())} missing")

    pools = Counter((p, l) for p, _, l in lines(files)
                    if POOL_PATTERN.search(l) and not re.match(r"(struct|\*|//)", l)
                    and not p.endswith(".h") and "panfrost_pool_init(" not in l)
    if pools != POOL_SITES:
        failures.append(f"executable pool consumers changed: {sorted((pools - POOL_SITES).elements())} "
                        f"new, {sorted((POOL_SITES - pools).elements())} missing")
    for path, copy in COPIES.items():
        body = normalize(files[path])
        allocation = [a for (p, a) in POOL_SITES if p == path and "pan_pool_alloc_aligned" in a][0]
        following = body.split(normalize(allocation), 1)[1].split(";")[:3]
        if normalize(copy).rstrip(";") not in ";".join(following):
            failures.append(f"{path}: allocation from the executable pool is not followed by {copy!r}")

    maps = Counter((p, call) for p, text in files.items() if p != KMOD + "pan_kmod.h"
                   for call in mmap_calls(text))
    if maps != MAPS:
        failures.append(f"CPU mappings changed: {sorted((maps - MAPS).elements())} new, "
                        f"{sorted((MAPS - maps).elements())} missing")
    executable_maps = [p for p, _, l in lines(files) if "PROT_EXEC" in l]
    if executable_maps:
        failures.append(f"PROT_EXEC appears in {sorted(set(executable_maps))}")

    for path, text in REQUIRED:
        if normalize(text) not in normalize(files.get(path, "")):
            failures.append(f"{path}: expected {text!r}")

    # KUMO's side of the same rule, from this tree.
    kumo = normalize((ROOT / "lib/kumo-gpu/src/bo.rs").read_text())
    if normalize("if flags & EXECUTABLE != 0 { BindFlags::READ_ONLY } else { BindFlags::NO_EXECUTE }") not in kumo:
        failures.append("lib/kumo-gpu/src/bo.rs: bind_flags no longer derives READ_ONLY/NO_EXECUTE from EXECUTABLE")
    window = (ROOT / "lib/kumo-gpu/src/lib.rs").read_text()
    start, end = (int(re.search(rf"pub const {name}: u64 = (0x[0-9a-f_]+);", window)[1].replace("_", ""), 16)
                  for name in ("VA_START", "VA_END"))
    if start >> 32 != (end - 1) >> 32:
        failures.append(f"KUMO's GPU VA window {start:#x}..{end:#x} crosses a 4 GiB boundary")
    flags = (ROOT / "toolchain/mesa/kumo_kmod.c").read_text()
    if "PAN_KMOD_BO_FLAG_WB_MMAP" in flags.split("#define KUMO_SUPPORTED_BO_FLAGS", 1)[1].split("\n\n")[0]:
        failures.append("KUMO now offers WB_MMAP; re-measure panvk's executable pool flags")

    for failure in failures:
        print("G7o-e W^X audit FAILED: " + failure)
    if failures:
        raise SystemExit(1)
    roles = Counter(EXECUTABLE_SITES.values())
    print(f"executable flag sites: {sum(roles.values())} ({', '.join(f'{n} {r}' for r, n in sorted(roles.items()))})")
    print(f"executable pool consumers: {sum(POOL_SITES.values())}; CPU mappings: {sum(MAPS.values())}, none PROT_EXEC")
    print(f"KUMO GPU VA window {start:#x}..{end:#x}: one 4 GiB region")
    print(GREEN)


if __name__ == "__main__":
    main()
