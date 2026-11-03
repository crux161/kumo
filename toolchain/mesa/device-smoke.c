/* SPDX-License-Identifier: MIT */
/*j765*/
/*j768*/
/*j769*/
/*j770*/
/*j772*/
#include "kumo_kmod.h"
#include <string.h>
#include <stdio.h>
#include <sys/mman.h>

#define CHECK(test, number) do { if (!(test)) return (number); } while (0)
static void *fail_alloc(const struct pan_kmod_allocator *a, size_t size, bool transient)
{ (void)a; (void)size; (void)transient; return NULL; }
static void unused_free(const struct pan_kmod_allocator *a, void *p) { (void)a; (void)p; }
static uint32_t (*real_query)(void *context, struct kumo_gpu_info *out);
static uint32_t csif_queries;
static uint32_t csif_reg_field;
static uint32_t csif_query(void *context, struct kumo_gpu_info *out)
{
    ++csif_queries;
    uint32_t status = real_query(context, out);
    if (status == KUMO_GPU_OK && csif_reg_field != UINT32_MAX)
        out->cs_features = (out->cs_features & ~0xffu) | csif_reg_field;
    return status;
}
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


/* These controls drive the modeled producer between callbacks, never the native bridge.
 * Mesa lifecycle, callbacks, VMOs and wake/Port/timer objects are real. — MILVUS */
extern uint32_t kumo_mesa_native_control(uint32_t action);
static uint32_t native_op(struct pan_kmod_dev *dev, uint32_t op, uint64_t id, uint64_t value,
                           struct kumo_gpu_native_reply *out)
{
    const struct kumo_gpu_native_request request = {.operation = op, .id = id, .value = value};
    *out = (struct kumo_gpu_native_reply){UINT64_MAX, UINT32_MAX, UINT32_MAX, UINT64_MAX};
    return pan_kmod_kumo_native(dev, &request, out);
}
static uint32_t native_prepare(struct pan_kmod_dev *dev, uint32_t vm, uint64_t group, uint64_t point,
    const struct kumo_gpu_native_point *waits, uint32_t wait_count,
    const struct kumo_gpu_native_point *signals, uint32_t signal_count,
    struct kumo_gpu_native_reply *out)
{
    const struct kumo_gpu_native_request request = {
        .operation = KUMO_NATIVE_PREPARE, .id = group,
        .job = {vm, 32, UINT64_C(0x20000000), point},
        .waits = waits, .signals = signals, .wait_count = wait_count, .signal_count = signal_count,
    };
    return pan_kmod_kumo_native(dev, &request, out);
}
static unsigned native_sync_cases(const struct kumo_gpu_api *api, unsigned mode)
{
    struct kumo_gpu_api bad = *api;
    bad.version = 14;
    CHECK(!pan_kmod_kumo_dev_create(&bad, NULL), 100);
    bad = *api; bad.bytes = 128;
    CHECK(!pan_kmod_kumo_dev_create(&bad, NULL), 101);
    bad = *api; bad.native = NULL;
    CHECK(!pan_kmod_kumo_dev_create(&bad, NULL), 102);
    struct pan_kmod_dev *dev = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(dev && api->version == 15 && api->bytes == 136, 103);
    struct pan_kmod_vm *vm = pan_kmod_vm_create(dev, 0, UINT64_C(0x20000000), UINT64_C(0x10000000));
    CHECK(vm, 104);
    struct kumo_gpu_native_reply out;
    CHECK(native_op(dev, KUMO_NATIVE_CREATE_BINARY, 0, 0, &out) == KUMO_GPU_OK, 105);
    uint64_t input = out.id;
    static uint64_t old_id;
    if (old_id) {
        CHECK(native_op(dev, KUMO_NATIVE_PENDING, old_id, 0, &out) == KUMO_GPU_UNKNOWN_OBJECT && !out.id && !out.state, 106);
    }
    old_id = input;
    CHECK(native_op(dev, KUMO_NATIVE_CREATE_BINARY, 0, 0, &out) == KUMO_GPU_OK, 107);
    uint64_t output = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_CREATE_TIMELINE, 0, 0, &out) == KUMO_GPU_OK, 108);
    uint64_t timeline = out.id;
    /* BO/map and sync calls interleave on the same owner and request sequence. */
    struct pan_kmod_bo *bo = pan_kmod_bo_alloc(dev, vm, 4096, PAN_KMOD_BO_FLAG_EXECUTABLE);
    CHECK(bo, 109);
    volatile uint32_t *cpu = pan_kmod_bo_mmap(bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    CHECK(cpu != MAP_FAILED, 110);
    *cpu = UINT32_C(0x7510cafe);
    struct pan_kmod_vm_op binding = {.type = PAN_KMOD_VM_OP_TYPE_MAP,
        .va = {UINT64_C(0x20000000), 4096}, .map = {bo, 0}};
    CHECK(!pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &binding, 1), 111);
    CHECK(native_op(dev, KUMO_NATIVE_GROUP_CREATE, 0, vm->handle, &out) == KUMO_GPU_OK, 112);
    uint64_t group = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_GROUP_STATE, group, 0, &out) == KUMO_GPU_OK && out.value == KUMO_GROUP_USABLE, 161);
    CHECK(native_op(dev, KUMO_NATIVE_DROP, group, 0, &out) == KUMO_GPU_SERVER_ERROR + 7, 113);
    struct kumo_gpu_native_point first_signals[] = {{input, 0}, {timeline, 42}};
    struct kumo_gpu_native_point later_signals[] = {{output, 0}, {timeline, 100}};
    CHECK(native_prepare(dev, vm->handle, group, 1, first_signals, 1, later_signals, 1, &out) == KUMO_GPU_UNSUBMITTED && !out.id && !out.value, 114);
    CHECK(native_prepare(dev, vm->handle, group, 1, NULL, 0, first_signals, 2, &out) == KUMO_GPU_OK, 115);
    uint64_t first = out.id;
    if (mode == 11) {
        CHECK(!kumo_mesa_native_control(4), 116);
        CHECK(native_op(dev, KUMO_NATIVE_STEP, first, 0, &out) == KUMO_GPU_TRANSPORT && !out.id, 117);
        CHECK(pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_FAULTY, 118);
        CHECK(native_op(dev, KUMO_NATIVE_STEP, first, 0, &out) == KUMO_GPU_DEVICE_LOST, 119);
        CHECK(native_op(dev, KUMO_NATIVE_GROUP_STATE, group, 0, &out) == KUMO_GPU_OK && out.value == KUMO_GROUP_UNKNOWN, 162);
        pan_kmod_dev_destroy(dev);
        return 0;
    }
    CHECK(native_op(dev, KUMO_NATIVE_STEP, first, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_SUBMITTED, 120);
    uint64_t first_snapshot = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_PENDING, timeline, 42, &out) == KUMO_GPU_OK && out.value == 1, 121);
    CHECK(native_prepare(dev, vm->handle, group, 2, first_signals, 2, later_signals, 2, &out) == KUMO_GPU_OK, 122);
    uint64_t second = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_RESET, input, 0, &out) == KUMO_GPU_OK, 123);
    CHECK(native_op(dev, KUMO_NATIVE_DROP, input, 0, &out) == KUMO_GPU_OK, 124);
    CHECK(native_op(dev, KUMO_NATIVE_STEP, second, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_AVAILABLE, 125);
    uint64_t wake = out.id;
    if (mode >= 12) {
        CHECK(!kumo_mesa_native_control(mode == 12 ? 8 : mode == 13 ? 9 : 10), 163);
        CHECK(native_op(dev, KUMO_NATIVE_STEP, second, 0, &out) == KUMO_GPU_TRANSPORT, 164);
        uint64_t expected = mode == 12 ? KUMO_GROUP_TIMED_OUT : mode == 13 ? KUMO_GROUP_FATAL : KUMO_GROUP_UNKNOWN;
        CHECK(native_op(dev, KUMO_NATIVE_GROUP_STATE, group, 0, &out) == KUMO_GPU_OK && out.value == expected && !out.gpu_status, 165);
        CHECK(!kumo_mesa_native_control(7) && !kumo_mesa_native_control(11), 166);
        CHECK(native_op(dev, KUMO_NATIVE_GROUP_STATE, group, 0, &out) == KUMO_GPU_OK && out.value == expected, 167);
        pan_kmod_dev_destroy(dev);
        return 0;
    }
    if (mode == 10) {
        CHECK(native_op(dev, KUMO_NATIVE_ARM, first_snapshot, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_AVAILABLE, 159);
        CHECK(!kumo_mesa_native_control(2), 126);
        CHECK(native_op(dev, KUMO_NATIVE_WAIT, wake, UINT64_C(5000000000), &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_CLOSED, 127);
        const struct kumo_gpu_native_request failed_step = {.operation = KUMO_NATIVE_STEP, .id = second};
        CHECK(api->native(api->context, &failed_step, &out) == KUMO_GPU_DEVICE_LOST && out.gpu_status == 0xbeef && !out.id, 128);
        CHECK(pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_FAULTY, 129);
        CHECK(native_op(dev, KUMO_NATIVE_STEP, second, 0, &out) == KUMO_GPU_DEVICE_LOST, 130);
        CHECK(!kumo_mesa_native_control(7), 160); /* known loss closed the shared owner now */
        CHECK(native_op(dev, KUMO_NATIVE_GROUP_STATE, group, 0, &out) == KUMO_GPU_OK && out.value == KUMO_GROUP_FATAL && out.gpu_status == 0xbeef, 168);
        pan_kmod_dev_destroy(dev);
        return 0;
    }
    CHECK(native_op(dev, KUMO_NATIVE_WAIT, wake, 2000000, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_TIMED_OUT, 131);
    CHECK(native_op(dev, KUMO_NATIVE_WAIT, wake, 2000000, &out) == KUMO_GPU_UNKNOWN_OBJECT, 132);
    CHECK(native_op(dev, KUMO_NATIVE_PENDING, output, 0, &out) == KUMO_GPU_OK && !out.value, 133);
    CHECK(native_op(dev, KUMO_NATIVE_STEP, second, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_AVAILABLE, 134);
    wake = out.id;
    CHECK(!kumo_mesa_native_control(5), 135); /* wake byte without GPU completion */
    CHECK(native_op(dev, KUMO_NATIVE_WAIT, wake, UINT64_C(5000000000), &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_WOKEN, 136);
    CHECK(native_op(dev, KUMO_NATIVE_STEP, second, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_AVAILABLE, 137);
    wake = out.id;
    CHECK(!kumo_mesa_native_control(6), 138); /* producer close without a completion */
    CHECK(native_op(dev, KUMO_NATIVE_WAIT, wake, UINT64_C(5000000000), &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_CLOSED, 139);
    CHECK(native_op(dev, KUMO_NATIVE_PENDING, output, 0, &out) == KUMO_GPU_OK && !out.value, 140);
    CHECK(native_op(dev, KUMO_NATIVE_STEP, second, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_AVAILABLE, 141);
    wake = out.id;
    CHECK(!kumo_mesa_native_control(1), 142);
    CHECK(native_op(dev, KUMO_NATIVE_WAIT, wake, UINT64_C(5000000000), &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_WOKEN, 143);
    CHECK(!kumo_mesa_native_control(3), 144);
    CHECK(native_op(dev, KUMO_NATIVE_STEP, second, 0, &out) == KUMO_GPU_SERVER_ERROR + 5 && !out.id, 145);
    CHECK(native_op(dev, KUMO_NATIVE_PENDING, output, 0, &out) == KUMO_GPU_OK && !out.value, 146);
    CHECK(native_op(dev, KUMO_NATIVE_STEP, second, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_SUBMITTED, 147);
    uint64_t snapshot = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_STEP, second, 0, &out) == KUMO_GPU_INVALID_REQUEST && !out.id, 148);
    CHECK(native_op(dev, KUMO_NATIVE_ARM, snapshot, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_AVAILABLE, 149);
    wake = out.id;
    CHECK(!kumo_mesa_native_control(1), 150);
    CHECK(native_op(dev, KUMO_NATIVE_WAIT, wake, UINT64_C(5000000000), &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_WOKEN, 151);
    CHECK(native_op(dev, KUMO_NATIVE_ARM, snapshot, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_COMPLETE, 152);
    CHECK(native_op(dev, KUMO_NATIVE_SIGNAL, timeline, 200, &out) == KUMO_GPU_OK, 153);
    CHECK(*cpu == UINT32_C(0x7510cafe), 154);
    CHECK(native_prepare(dev, vm->handle, group, 3, NULL, 0, NULL, 0, &out) == KUMO_GPU_OK, 155);
    uint64_t old_intent = out.id;
    pan_kmod_vm_destroy(vm);
    CHECK(native_op(dev, KUMO_NATIVE_STEP, old_intent, 0, &out) == KUMO_GPU_UNKNOWN_BUFFER, 156);
    vm = pan_kmod_vm_create(dev, 0, UINT64_C(0x20000000), UINT64_C(0x10000000));
    CHECK(vm && vm->handle == 2, 157);
    CHECK(native_prepare(dev, vm->handle, group, 3, NULL, 0, NULL, 0, &out) == KUMO_GPU_INVALID_REQUEST, 158);
    pan_kmod_bo_put(bo);
    pan_kmod_vm_destroy(vm);
    pan_kmod_dev_destroy(dev);
    return 0;
}

/* G7q-c: tiler heaps through the published device. — TIMBERDOODLE */
static unsigned heap_cases(const struct kumo_gpu_api *api)
{
    struct kumo_gpu_api bad = *api;
    bad.heap_create = NULL;
    CHECK(!pan_kmod_kumo_dev_create(&bad, NULL), 170);
    bad = *api; bad.heap_destroy = NULL;
    CHECK(!pan_kmod_kumo_dev_create(&bad, NULL), 171);
    struct pan_kmod_dev *dev = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(dev, 172);
    struct pan_kmod_vm *vm = pan_kmod_vm_create(dev, 0, UINT64_C(0x20000000), UINT64_C(0x10000000));
    CHECK(vm, 173);
    const uint32_t mib2 = 2u * 1024 * 1024;
    struct kumo_gpu_heap_request request = {vm->handle, mib2, 5, 64, 65535, 0};
    struct kumo_gpu_heap_info info, empty = {0};
    CHECK(pan_kmod_kumo_heap_create(dev, &request, &info) == KUMO_GPU_OK && info.heap == 1 &&
          info.reserved == 0 && info.context_va >= UINT64_C(0x30000000) &&
          info.first_chunk_va == info.context_va + 4096 + 4u * (uint64_t)mib2, 174);
    const uint32_t first = info.heap;
    /* Panthor's refusals never reach the wire, and leave a zeroed answer. */
    struct kumo_gpu_heap_request refused[] = {
        {vm->handle, mib2, 5, 64, 65535, 1},
        {vm->handle, 64u * 1024, 1, 1, 1, 0},
        {vm->handle, mib2, 0, 64, 1, 0},
        {vm->handle, mib2, 5, 4, 1, 0},
        {vm->handle + 1, mib2, 1, 1, 1, 0},
    };
    for (unsigned i = 0; i < sizeof refused / sizeof refused[0]; ++i) {
        CHECK(pan_kmod_kumo_heap_create(dev, &refused[i], &info) != KUMO_GPU_OK, 175);
        CHECK(!memcmp(&info, &empty, sizeof info), 176);
    }
    uint64_t bytes = 1;
    CHECK(pan_kmod_kumo_heap_destroy(dev, 99, &bytes) == KUMO_GPU_UNKNOWN_OBJECT && bytes == 0, 177);
    request.chunk_bytes = 128u * 1024; request.initial_chunks = 1; request.max_chunks = 2;
    CHECK(pan_kmod_kumo_heap_create(dev, &request, &info) == KUMO_GPU_OK && info.heap == 2, 178);
    CHECK(pan_kmod_kumo_heap_destroy(dev, first, &bytes) == KUMO_GPU_OK &&
          bytes == 4096 + 5u * (uint64_t)mib2, 179);
    CHECK(pan_kmod_kumo_heap_destroy(dev, first, &bytes) == KUMO_GPU_UNKNOWN_OBJECT, 180);
    /* The logical VM's teardown destroys heap 2; the next VM starts with none. */
    pan_kmod_vm_destroy(vm);
    vm = pan_kmod_vm_create(dev, 0, UINT64_C(0x20000000), UINT64_C(0x10000000));
    CHECK(vm && vm->handle == 2, 181);
    CHECK(pan_kmod_kumo_heap_destroy(dev, 2, &bytes) == KUMO_GPU_UNKNOWN_OBJECT, 182);
    request.vm = vm->handle;
    CHECK(pan_kmod_kumo_heap_create(dev, &request, &info) == KUMO_GPU_OK && info.heap == 3, 183);
    pan_kmod_vm_destroy(vm);
    pan_kmod_dev_destroy(dev);
    return 0;
}

/* GL1-b: the AUTO_VA VM Gallium creates. Addresses come from the lowest fit, a freed range is
 * reused, a caller-chosen address is refused, and a full window refuses a map with its marker
 * restored and nothing sent. The window (256 MiB) outgrows the 128 MiB budget, so it is filled
 * by mapping one 64 MiB BO several times. — TIMBERDOODLE */
static unsigned auto_va_cases(const struct kumo_gpu_api *api)
{
    struct pan_kmod_dev *dev = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(dev, 190);
    struct pan_kmod_va_range range = pan_kmod_dev_query_user_va_range(dev);
    struct pan_kmod_vm *vm = pan_kmod_vm_create(
        dev, PAN_KMOD_VM_FLAG_AUTO_VA | PAN_KMOD_VM_FLAG_TRACK_ACTIVITY, range.start, range.size);
    CHECK(vm && vm->flags == (PAN_KMOD_VM_FLAG_AUTO_VA | PAN_KMOD_VM_FLAG_TRACK_ACTIVITY), 191);
    struct pan_kmod_bo *data = pan_kmod_bo_alloc(dev, vm, 3 * 4096, 0);
    struct pan_kmod_bo *code = pan_kmod_bo_alloc(dev, vm, 4096, PAN_KMOD_BO_FLAG_EXECUTABLE);
    struct pan_kmod_bo *big = pan_kmod_bo_alloc(dev, vm, UINT64_C(64) << 20, 0);
    CHECK(data && code && big, 192);
    struct pan_kmod_vm_op manual = {.type = PAN_KMOD_VM_OP_TYPE_MAP,
        .va = {range.start, data->size}, .map = {data, 0}};
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &manual, 1) < 0, 193);
    struct pan_kmod_vm_op ops[3] = {
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {PAN_KMOD_VM_MAP_AUTO_VA, data->size}, .map = {data, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {PAN_KMOD_VM_MAP_AUTO_VA, code->size}, .map = {code, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {PAN_KMOD_VM_MAP_AUTO_VA, big->size}, .map = {big, 0}},
    };
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, ops, 3) == 0, 194);
    CHECK(ops[0].va.start == range.start && ops[1].va.start == range.start + data->size &&
          ops[2].va.start == ops[1].va.start + code->size, 195);
    struct pan_kmod_vm_op unmap = {.type = PAN_KMOD_VM_OP_TYPE_UNMAP, .va = {ops[1].va.start, code->size}};
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &unmap, 1) == 0, 196);
    struct pan_kmod_vm_op again = {.type = PAN_KMOD_VM_OP_TYPE_MAP,
        .va = {PAN_KMOD_VM_MAP_AUTO_VA, code->size}, .map = {code, 0}};
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &again, 1) == 0 &&
          again.va.start == ops[1].va.start, 197);
    /* GL1-d: with no submission behind it, Gallium's deferred unmap waits for nothing and frees
     * the range for the next map. A BO starts with no activity, and a point no submission
     * reached is refused. — TIMBERDOODLE */
    struct pan_kmod_vm_op deferred = {.type = PAN_KMOD_VM_OP_TYPE_UNMAP, .va = {again.va.start, code->size}};
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_DEFER_TO_NEXT_IDLE_POINT, &deferred, 1) == 0, 204);
    struct pan_kmod_vm_op third = {.type = PAN_KMOD_VM_OP_TYPE_MAP,
        .va = {PAN_KMOD_VM_MAP_AUTO_VA, code->size}, .map = {code, 0}};
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &third, 1) == 0 &&
          third.va.start == again.va.start, 205);
    CHECK(pan_kmod_bo_wait(code, INT64_MAX, false) && pan_kmod_kumo_vm_wait(vm, 0, 0) == 1, 206);
    CHECK(pan_kmod_kumo_bo_attach_sync_point(code, 1, true) < 0, 207);
    unsigned maps = 0;
    for (;;) {
        struct pan_kmod_vm_op fill = {.type = PAN_KMOD_VM_OP_TYPE_MAP,
            .va = {PAN_KMOD_VM_MAP_AUTO_VA, big->size}, .map = {big, 0}};
        if (pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &fill, 1)) {
            CHECK(fill.va.start == PAN_KMOD_VM_MAP_AUTO_VA, 198);
            break;
        }
        CHECK(fill.va.start >= range.start && fill.va.start + big->size <= range.start + range.size, 199);
        CHECK(++maps < 8, 200);
    }
    CHECK(maps == 2 && pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_USABLE, 201);
    pan_kmod_bo_put(big);
    pan_kmod_bo_put(code);
    pan_kmod_bo_put(data);
    pan_kmod_vm_destroy(vm);
    pan_kmod_dev_destroy(dev);
    return 0;
}

#ifdef KUMO_GL_SCREEN
/* GL1-b: mode 17 runs Gallium's screen, and exists only in the build that links Gallium. */
extern unsigned kumo_gl_screen_cases(const struct kumo_gpu_api *api, void (*say)(const char *));
/* GL1-d: mode 20 runs a clear, a fence and a readback against the modeled producer. */
extern unsigned kumo_gl_batch_cases(const struct kumo_gpu_api *api, void (*say)(const char *),
                                    unsigned (*control)(unsigned));
extern unsigned kumo_gl_context_sync_cases(const struct kumo_gpu_api *api);
extern unsigned kumo_gl_context_init_cases(const struct kumo_gpu_api *api, void (*say)(const char *));
extern unsigned kumo_gl_frontend_cases(const struct kumo_gpu_api *api, void (*say)(const char *));
extern unsigned kumo_gl_client_cases(const struct kumo_gpu_api *api, void (*say)(const char *));
extern unsigned kumo_gl_compute_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled);
extern unsigned kumo_gl_clear_cases(const struct kumo_gpu_api *api, void (*say)(const char *), bool modeled);
static void say_line(const char *line) { puts(line); fflush(stdout); }
#endif
unsigned kumo_mesa_device_modes(void)
{
#ifdef KUMO_GL_SCREEN
    return 25;
#else
    return 17;
#endif
}

unsigned kumo_mesa_device_cases(const struct kumo_gpu_api *api, unsigned mode)
{
#ifdef KUMO_GL_SCREEN
    if (mode == 24) return kumo_gl_clear_cases(api, say_line, true);
    if (mode == 23) return kumo_gl_compute_cases(api, say_line, true);
    if (mode == 22) return kumo_gl_client_cases(api, say_line);
    if (mode == 21) return kumo_gl_frontend_cases(api, say_line);
    if (mode == 20) return kumo_gl_batch_cases(api, say_line, kumo_mesa_native_control);
    if (mode == 19) return kumo_gl_context_init_cases(api, say_line);
    if (mode == 18) return kumo_gl_context_sync_cases(api);
    if (mode == 17) return kumo_gl_screen_cases(api, say_line);
#endif
    if (mode == 16) return auto_va_cases(api);
    if (mode == 15) return heap_cases(api);
    if (mode >= 9) return native_sync_cases(api, mode);
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
    struct kumo_gpu_api queried = *api;
    queried.query = csif_query;
    csif_queries = 0;
    csif_reg_field = mode == 0 ? 0xff : UINT32_MAX;
    struct pan_kmod_dev *dev = pan_kmod_kumo_dev_create(&queried, NULL);
    if (mode == 1 || mode == 5) { CHECK(!dev, 4); return 0; }
    CHECK(dev && dev->ops == &kumo_kmod_ops && dev->fd == -1 && !(dev->flags & PAN_KMOD_DEV_FLAG_OWNS_FD), 5);
    /* The 0xff edge must stay 256, not truncate to a byte or copy the metal's 96. The
     * accessor must use this device's original query, not fetch changing props. — KESTREL */
    uint32_t cs_reg_count = mode == 0 ? 256 : 96;
    csif_reg_field = 0;
    CHECK(pan_kmod_kumo_cs_reg_count(dev) == cs_reg_count &&
          pan_kmod_kumo_cs_reg_count(dev) == cs_reg_count && csif_queries == 1, 202);
    const struct pan_kmod_dev foreign = {0};
    CHECK(!pan_kmod_kumo_cs_reg_count(NULL) && !pan_kmod_kumo_cs_reg_count(&foreign), 203);
    if (mode == 0)
        puts("GL1-c1 native CSIF green: attested register counts, 256-register edge, cached query, foreign device refusal");
    CHECK(dev->props.mem_features == 0x301 && dev->props.l2_features == 0x07120306 &&
          dev->props.pgsize_bitmap == 4096 && !dev->props.is_io_coherent &&
          dev->props.supported_bo_flags == KUMO_GPU_BO_FLAGS && !dev->props.supported_vm_op_flags, 6);
    CHECK(!dev->ops->dev_create(17, 0, NULL, NULL), 7);
    struct pan_kmod_va_range range = pan_kmod_dev_query_user_va_range(dev);
    CHECK(range.start == 0x20000000 && range.size == 0x10000000, 8);
    /* GL1-b accepts AUTO_VA and TRACK_ACTIVITY (mode 16); an unknown flag and a range outside
     * the window are still refused. — TIMBERDOODLE */
    CHECK(!pan_kmod_vm_create(dev, 1u << 5, range.start, range.size), 9);
    CHECK(!pan_kmod_vm_create(dev, PAN_KMOD_VM_FLAG_AUTO_VA, range.start - 4096, range.size), 10);
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
    puts("G7p-c native C sync begin: actual Mesa device/BO/VM and shared callback owner");
    unsigned failed = kumo_mesa_device_proof();
    if (failed) { printf("G7o-d native device FAILED at case %u\n", failed); return 1; }
    puts("G7p-d group state green: READ-only status, fatal/timeout/unknown after channel close, cached C reports, no replay");
#ifdef KUMO_GL_SCREEN
    puts("GL1-b screen green: Gallium's panfrost screen over the native device, its caps, a render-target texture, and a clean destroy");
#endif
    puts("GL1-b auto-VA green: AUTO_VA and TRACK_ACTIVITY VMs, lowest-fit addresses, a freed range reused, caller addresses refused, a full window refused with nothing sent");
    puts("G7q-c native heaps green: ABI 15 heap callbacks, Panthor's refusals with no I/O, server-VM heaps, VM teardown destroys them, exact bytes back");
    puts("G7p-c native C sync green: ABI 15, shared BO/sync owner, captured submits, bounded wakes, refusal/loss containment; zero SyncRead");
    puts("G7o-e W^X driver shapes green: Gallium and panvk executable pools bind read-only, RW maps only, WB_MMAP masked, one 4 GiB window");
    puts("G7o-d native device green: complete props, Mesa device/BO/VM lifecycle, immediate binds, generations, loss and contained close");
    return 0;
}
