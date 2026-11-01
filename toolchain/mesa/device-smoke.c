/* SPDX-License-Identifier: MIT */
/*j745*/
/*j746*/
#include "kumo_kmod.h"
#include <stdio.h>
#include <sys/mman.h>

#define CHECK(test, number) do { if (!(test)) return (number); } while (0)
static void *fail_alloc(const struct pan_kmod_allocator *a, size_t size, bool transient)
{ (void)a; (void)size; (void)transient; return NULL; }
static void unused_free(const struct pan_kmod_allocator *a, void *p) { (void)a; (void)p; }
static uint32_t (*real_query)(void *context, struct kumo_gpu_info *out);
static uint32_t straddling_query(void *context, struct kumo_gpu_info *out)
{
    uint32_t status = real_query(context, out);
    out->va_end = UINT64_C(0x100001000); // one page past the first 4 GiB boundary
    return status;
}

/* G7o-e: the executable BO requests both drivers make (toolchain/mesa/WX-INVENTORY.md),
 * through the real device. Buffer 1 is the caller's data BO; the Rust service asserts
 * READ_ONLY for buffers 2 and 3 and NO_EXECUTE for 1. — TIMBERDOODLE */
static unsigned driver_shapes(struct pan_kmod_dev *dev, struct pan_kmod_vm *vm,
                              struct pan_kmod_bo *data, struct pan_kmod_va_range range)
{
    /* panvk asks EXECUTABLE | WB_MMAP and panvk_device_adjust_bo_flags drops WB_MMAP the
     * device does not offer. Unmasked, the request fails closed before any wire I/O. */
    uint32_t panvk_flags = PAN_KMOD_BO_FLAG_EXECUTABLE | PAN_KMOD_BO_FLAG_WB_MMAP;
    CHECK(!(dev->props.supported_bo_flags & PAN_KMOD_BO_FLAG_WB_MMAP), 40);
    CHECK(!pan_kmod_bo_alloc(dev, vm, 16 * 1024, panvk_flags), 41);
    if (!(dev->props.supported_bo_flags & PAN_KMOD_BO_FLAG_WB_MMAP))
        panvk_flags &= ~PAN_KMOD_BO_FLAG_WB_MMAP;
    /* Gallium's shader pools: 4 KiB slabs of PAN_BO_EXECUTE alone, not shareable and so
     * exclusive to the context VM. panvk's executable pool: 16 KiB slabs on the device VM. */
    struct pan_kmod_bo *gallium = pan_kmod_bo_alloc(dev, vm, 4096, PAN_KMOD_BO_FLAG_EXECUTABLE);
    struct pan_kmod_bo *panvk = pan_kmod_bo_alloc(dev, vm, 16 * 1024, panvk_flags);
    CHECK(gallium && panvk && gallium->exclusive_vm == vm && panvk->exclusive_vm == vm &&
          panvk->flags == PAN_KMOD_BO_FLAG_EXECUTABLE, 42);
    struct pan_kmod_bo *code[2] = {gallium, panvk};
    for (uint32_t i = 0; i < 2; ++i) {
        /* Both drivers write their binaries through exactly this map; never PROT_EXEC. */
        CHECK(pan_kmod_bo_mmap(code[i], PROT_READ | PROT_WRITE | PROT_EXEC, MAP_SHARED, NULL) == MAP_FAILED, 43);
        volatile uint32_t *cpu = pan_kmod_bo_mmap(code[i], PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
        CHECK(cpu != MAP_FAILED, 44);
        for (uint32_t w = 0; w < code[i]->size / 4; ++w) cpu[w] = 0x7e000000u ^ (w * 0x9e37u + i);
        CHECK(!pan_kmod_kumo_bo_munmap(code[i]), 45);
        cpu = pan_kmod_bo_mmap(code[i], PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
        CHECK(cpu != MAP_FAILED, 46);
        for (uint32_t w = 0; w < code[i]->size / 4; ++w) CHECK(cpu[w] == (0x7e000000u ^ (w * 0x9e37u + i)), 47);
    }
    /* Code at both ends of the window, which is one 4 GiB region by the factory's check. */
    struct pan_kmod_vm_op ops[3] = {
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {range.start, data->size}, .map = {data, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {range.start + 0x100000, gallium->size}, .map = {gallium, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {range.start + range.size - panvk->size, panvk->size},
         .map = {panvk, 0}},
    };
    CHECK(ops[1].va.start >> 32 == (ops[2].va.start + ops[2].va.size - 1) >> 32, 48);
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, ops, 3) == 0, 49);
    pan_kmod_bo_put(panvk); pan_kmod_bo_put(gallium); pan_kmod_bo_put(data);
    pan_kmod_vm_destroy(vm);
    pan_kmod_dev_destroy(dev);
    return 0;
}

unsigned kumo_mesa_device_cases(const struct kumo_gpu_api *api, unsigned mode)
{
    struct kumo_gpu_api bad = *api;
    bad.version = 11;
    CHECK(!pan_kmod_kumo_dev_create(&bad, NULL), 1);
    bad = *api; bad.vm_bind = NULL;
    CHECK(!pan_kmod_kumo_dev_create(&bad, NULL), 2);
    bad = *api; real_query = api->query; bad.query = straddling_query;
    CHECK(!pan_kmod_kumo_dev_create(&bad, NULL), 39);
    if (mode == 6) {
        const struct pan_kmod_allocator allocator = {.zalloc = fail_alloc, .free = unused_free};
        CHECK(!pan_kmod_kumo_dev_create(api, &allocator), 3);
        return 0;
    }
    struct pan_kmod_dev *dev = pan_kmod_kumo_dev_create(api, NULL);
    if (mode == 1 || mode == 5) { CHECK(!dev, 4); return 0; }
    CHECK(dev && dev->ops == &kumo_kmod_ops && dev->fd == -1 && !(dev->flags & PAN_KMOD_DEV_FLAG_OWNS_FD), 5);
    CHECK(dev->props.mem_features == 0x301 && dev->props.l2_features == 0x07120306 &&
          dev->props.pgsize_bitmap == 4096 && !dev->props.is_io_coherent &&
          dev->props.supported_bo_flags == KUMO_GPU_BO_FLAGS && !dev->props.supported_vm_op_flags, 6);
    CHECK(!dev->ops->dev_create(17, 0, NULL, NULL), 7);
    struct pan_kmod_va_range range = pan_kmod_dev_query_user_va_range(dev);
    CHECK(range.start == 0x20000000 && range.size == 0x10000000, 8);
    CHECK(!pan_kmod_vm_create(dev, PAN_KMOD_VM_FLAG_AUTO_VA, range.start, range.size), 9);
    CHECK(!pan_kmod_vm_create(dev, PAN_KMOD_VM_FLAG_TRACK_ACTIVITY, range.start, range.size), 10);
    struct pan_kmod_bo *bo = pan_kmod_bo_alloc(dev, NULL, 4 * 4096, 0);
    CHECK(bo && bo->dev == dev && !bo->exclusive_vm && bo->size == 4 * 4096, 11);
    CHECK(pan_kmod_bo_get(bo) == bo && bo->refcnt == 2, 12);
    pan_kmod_bo_put(bo);
    CHECK(bo->refcnt == 1, 13);
    volatile uint32_t *cpu = pan_kmod_bo_mmap(bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    CHECK(cpu != MAP_FAILED && *cpu == 0, 14);
    *cpu = 0x7450cafe;
    CHECK(!pan_kmod_kumo_bo_munmap(bo), 15);
    cpu = pan_kmod_bo_mmap(bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    CHECK(cpu != MAP_FAILED && *cpu == 0x7450cafe, 16);
    struct pan_kmod_vm *vm = pan_kmod_vm_create(dev, 0, range.start, range.size);
    CHECK(vm && vm->dev == dev && vm->handle == 1 && vm->pgsize_bitmap == 4096, 17);
    CHECK(!pan_kmod_vm_create(dev, 0, range.start, range.size), 18);
    CHECK(pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_USABLE, 19);
    if (mode == 8) return driver_shapes(dev, vm, bo, range);
    struct pan_kmod_bo *exec = pan_kmod_bo_alloc(dev, vm, 2 * 4096,
        PAN_KMOD_BO_FLAG_EXECUTABLE | PAN_KMOD_BO_FLAG_GPU_UNCACHED);
    CHECK(exec && exec->exclusive_vm == vm, 20);
    struct pan_kmod_vm_op ops[2] = {
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {range.start, 2 * 4096}, .map = {bo, 4096}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {range.start + 4 * 4096, 4096}, .map = {exec, 0}},
    };
    struct pan_kmod_vm_op invalid[2] = {ops[0], ops[1]};
    invalid[1].flags = PAN_KMOD_VM_OP_OP_MAP_SPARSE;
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, invalid, 2) < 0, 21);
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_ASYNC, ops, 2) < 0, 22);
    invalid[1] = ops[1]; invalid[1].map.bo_offset = -1;
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, invalid, 2) < 0, 23);
    invalid[1] = ops[1]; invalid[1].wait.count = 1;
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, invalid, 2) < 0, 24);
    int bound = pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, ops, 2);
    if (mode == 2) {
        CHECK(bound < 0 && pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_FAULTY, 25);
        CHECK(!pan_kmod_bo_alloc(dev, NULL, 4096, 0), 26);
        pan_kmod_bo_put(exec); pan_kmod_bo_put(bo); pan_kmod_vm_destroy(vm); pan_kmod_dev_destroy(dev);
        return 0;
    }
    CHECK(bound == 0, 27);
    if (mode == 7) { pan_kmod_dev_destroy(dev); return 0; } // live children are invalidated and reclaimed
    if (mode == 3) {
        pan_kmod_bo_put(exec); // injected destroy refusal makes the void destructor terminal
        CHECK(pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_FAULTY, 28);
        pan_kmod_bo_put(bo); pan_kmod_vm_destroy(vm); pan_kmod_dev_destroy(dev); return 0;
    }
    if (mode == 4) {
        pan_kmod_vm_destroy(vm); // injected invalidation refusal closes the session
        CHECK(!pan_kmod_vm_create(dev, 0, range.start, range.size), 29);
        pan_kmod_bo_put(exec); pan_kmod_bo_put(bo); pan_kmod_dev_destroy(dev); return 0;
    }
    struct pan_kmod_vm_op unmap = {.type = PAN_KMOD_VM_OP_TYPE_UNMAP, .va = ops[0].va};
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &unmap, 1) == 0, 30);
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &unmap, 1) < 0, 31);
    CHECK(pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_USABLE, 32);
    pan_kmod_vm_destroy(vm); // removes the remaining executable binding
    vm = pan_kmod_vm_create(dev, 0, range.start + 6 * 4096, 6 * 4096);
    CHECK(vm && vm->handle == 2, 33);
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &ops[1], 1) < 0, 34); // old exclusive generation
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &ops[0], 1) < 0, 35); // outside logical subrange
    ops[0].va.start = range.start + 6 * 4096;
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &ops[0], 1) == 0, 36);
    pan_kmod_bo_put(bo); // implicit service unbind, then exact local CPU cleanup
    pan_kmod_bo_put(exec);
    CHECK(!dev->ops->bo_import(dev, 123, 4096) && !dev->ops->perf_create(dev), 37);
    CHECK(!dev->ops->flush_bo_map_syncs(dev), 38);
    pan_kmod_vm_destroy(vm);
    pan_kmod_dev_destroy(dev);
    return 0;
}

extern unsigned kumo_mesa_device_proof(void);
int main(void)
{
    puts("G7o-d native device begin: modeled GPU service, real Mesa lifecycle and VMOs");
    unsigned failed = kumo_mesa_device_proof();
    if (failed) { printf("G7o-d native device FAILED at case %u\n", failed); return 1; }
    puts("G7o-e W^X driver shapes green: Gallium and panvk executable pools bind read-only, RW maps only, WB_MMAP masked, one 4 GiB window");
    puts("G7o-d native device green: complete props, Mesa device/BO/VM lifecycle, immediate binds, generations, loss and contained close");
    return 0;
}
