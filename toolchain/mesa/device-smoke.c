/* SPDX-License-Identifier: MIT */
/*j745*/
#include "kumo_kmod.h"
#include <stdio.h>
#include <sys/mman.h>

#define CHECK(test, number) do { if (!(test)) return (number); } while (0)
static void *fail_alloc(const struct pan_kmod_allocator *a, size_t size, bool transient)
{ (void)a; (void)size; (void)transient; return NULL; }
static void unused_free(const struct pan_kmod_allocator *a, void *p) { (void)a; (void)p; }

unsigned kumo_mesa_device_cases(const struct kumo_gpu_api *api, unsigned mode)
{
    struct kumo_gpu_api bad = *api;
    bad.version = 11;
    CHECK(!pan_kmod_kumo_dev_create(&bad, NULL), 1);
    bad = *api; bad.vm_bind = NULL;
    CHECK(!pan_kmod_kumo_dev_create(&bad, NULL), 2);
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
    puts("G7o-d native device green: complete props, Mesa device/BO/VM lifecycle, immediate binds, generations, loss and contained close");
    return 0;
}
