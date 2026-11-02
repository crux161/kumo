/* SPDX-License-Identifier: MIT */
/*j758*/
/*j759*/
/*j762*/
/*j765*/
/*j768*/
/* G7o-f: the native Mesa device, BOs and VM on the Orange Pi, over the live drv-gpu session.
 * Real props, real DMA pages, real page tables. The GPU itself checks the VM path: CS stores
 * through Mesa-made bindings, a live unbind and rebind, and a final store aimed at an
 * executable binding, which must fault and end the session (G7o-e's W^X rule on hardware).
 * — TIMBERDOODLE */
/* G7p-e: the same vehicle runs MILVUS's native sync and group state (J749-J752) on Mali.
 * KUMO_METAL_PROOF picks the proof at build time: 0 is G7o-f, 1 is G7p-e ending in a fatal
 * fault, 2 is G7p-e ending in a timeout, 3 is G7q-a's tiler heap, 4 is the same heap through
 * G7q-c's C bridge (pan_kmod_kumo_heap_create/destroy), 5 is GL1's no-execute command streams
 * (J758), 6 is GL1-b's Gallium screen (gl-screen.c, linked with Gallium). A failed group ends its
 * session, and Sora regrants only on the churn image, so each failure needs its own boot.
 * — TIMBERDOODLE */
/* 7 is GL1-c's Gallium context (KESTREL) and 8 GL1-d's clear, fence and readback (gl-batch.c).
 * — TIMBERDOODLE */
/* 9 makes Mesa's GLES2 context current through the KUMO frontend (GL1-e). — KESTREL */
#include "kumo_kmod.h"
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <sys/mman.h>

#ifndef KUMO_METAL_PROOF
#define KUMO_METAL_PROOF 0
#endif

/* device-metal.rs: one kernel-log line (a Sora-launched client has no stdio), one CS store
 * through the session's own group (G7o-f only), and the monotonic clock. */
extern void kumo_metal_log(const char *line);
extern uint32_t kumo_metal_store(void *context, volatile uint64_t *stream_cpu, uint64_t stream_va,
                                 uint64_t target, uint32_t value);
extern uint64_t kumo_metal_now_ns(void);
#if KUMO_METAL_PROOF == 6
extern unsigned kumo_gl_screen_cases(const struct kumo_gpu_api *api, void (*say)(const char *));
#endif
#if KUMO_METAL_PROOF == 7
extern unsigned kumo_gl_context_init_cases(const struct kumo_gpu_api *api, void (*say)(const char *));
#endif
#if KUMO_METAL_PROOF == 8
extern unsigned kumo_gl_batch_cases(const struct kumo_gpu_api *api, void (*say)(const char *),
                                    unsigned (*control)(unsigned));
#endif
#if KUMO_METAL_PROOF == 9
extern unsigned kumo_gl_frontend_cases(const struct kumo_gpu_api *api, void (*say)(const char *));
#endif
extern uint32_t kumo_metal_run(void *context, volatile uint64_t *stream_cpu, uint64_t stream_va,
                               const uint64_t *words, uint32_t count);
extern uint32_t kumo_metal_heap_create(void *context, uint32_t chunk_bytes, uint32_t initial_chunks,
                                       uint32_t max_chunks, uint64_t *out);
extern uint32_t kumo_metal_heap_destroy(void *context, uint32_t heap, uint64_t *bytes);

static void say(const char *format, ...)
{
    char line[240];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof line, format, args);
    va_end(args);
    kumo_metal_log(line);
}
#define CHECK(test, number) do { if (!(test)) return (number); } while (0)
#define MIB (UINT64_C(1) << 20)
#define BIG (64 * MIB) /* the per-BO cap: pages scattered over many 2 MiB tables (G7o-b) */
#define PATTERN(i, w) (0x7e000000u ^ ((uint32_t)(w) * 0x9e37u + (uint32_t)(i)))

static unsigned fill_and_verify(struct pan_kmod_bo *bo, uint32_t seed, volatile uint32_t **out)
{
    volatile uint32_t *cpu = pan_kmod_bo_mmap(bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    CHECK(cpu != MAP_FAILED && cpu[0] == 0, 1);
    /* First and last word of every page: enough to catch a wrong or shared page. */
    for (uint64_t page = 0; page < bo->size / 4096; ++page) {
        cpu[page * 1024] = PATTERN(seed, page * 2);
        cpu[page * 1024 + 1023] = PATTERN(seed, page * 2 + 1);
    }
    CHECK(!pan_kmod_kumo_bo_munmap(bo), 2);
    cpu = pan_kmod_bo_mmap(bo, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    CHECK(cpu != MAP_FAILED, 3);
    for (uint64_t page = 0; page < bo->size / 4096; ++page)
        CHECK(cpu[page * 1024] == PATTERN(seed, page * 2) &&
              cpu[page * 1024 + 1023] == PATTERN(seed, page * 2 + 1), 4);
    *out = cpu;
    return 0;
}

/* On a missed store: where did `value` land, and which pages lost their fill pattern?
 * Scans every word of `bo`. Diagnostic only; the case still fails. */
static void diagnose(const char *name, struct pan_kmod_bo *bo, volatile uint32_t *cpu,
                     uint32_t seed, uint32_t value)
{
    uint64_t words = bo->size / 4, hits = 0, first[4] = {0}, broken = 0, broken_first[4] = {0};
    for (uint64_t w = 0; w < words; ++w) {
        if (cpu[w] == value) {
            if (hits < 4) first[hits] = w;
            ++hits;
        }
    }
    for (uint64_t page = 0; page < bo->size / 4096; ++page) {
        if (cpu[page * 1024] != PATTERN(seed, page * 2) ||
            cpu[page * 1024 + 1023] != PATTERN(seed, page * 2 + 1)) {
            if (broken < 4) broken_first[broken] = page;
            ++broken;
        }
    }
    say("G7o-f diag %s: value 0x%08x found %llu times, first words %llu %llu %llu %llu; %llu pages lost a pattern word, first %llu %llu %llu %llu",
        name, value, (unsigned long long)hits, (unsigned long long)first[0],
        (unsigned long long)first[1], (unsigned long long)first[2], (unsigned long long)first[3],
        (unsigned long long)broken, (unsigned long long)broken_first[0],
        (unsigned long long)broken_first[1], (unsigned long long)broken_first[2],
        (unsigned long long)broken_first[3]);
}

static unsigned device_cases(const struct kumo_gpu_api *api)
{
    struct pan_kmod_dev *dev = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(dev, 1);
    const struct pan_kmod_dev_props *props = &dev->props;
    say("G7o-f device: gpu_id=0x%llx shader_present=0x%llx L2=0x%08x MEM=0x%08x MMU=0x%08x bo_flags=0x%x pgsize=0x%llx io_coherent=%u",
        (unsigned long long)props->gpu_id, (unsigned long long)props->shader_present,
        props->l2_features, props->mem_features, props->mmu_features, props->supported_bo_flags,
        (unsigned long long)props->pgsize_bitmap, (unsigned)props->is_io_coherent);
    /* J742 measured these words on this board; the service policy is G7o-a's. */
    CHECK(props->l2_features == 0x07120306 && props->mem_features == 0x301 &&
          props->supported_bo_flags == KUMO_GPU_BO_FLAGS && props->pgsize_bitmap == 4096 &&
          !props->is_io_coherent, 2);
    struct pan_kmod_va_range range = pan_kmod_dev_query_user_va_range(dev);
    CHECK(range.start == 0x20000000 && range.size == 0x10000000, 3);
    struct pan_kmod_vm *vm = pan_kmod_vm_create(dev, 0, range.start, range.size);
    CHECK(vm && pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_USABLE, 4);

    /* Gallium's shader slab, panvk's shader slab (its WB_MMAP masked away), a CS stream BO
     * (EXECUTABLE is Mesa's only read-only binding; until J758 KUMO's Submit read its stream
     * only from one), a data BO and the 64 MiB cap. */
    struct pan_kmod_bo *gallium = pan_kmod_bo_alloc(dev, vm, 4096, PAN_KMOD_BO_FLAG_EXECUTABLE);
    struct pan_kmod_bo *panvk = pan_kmod_bo_alloc(dev, vm, 16 * 1024, PAN_KMOD_BO_FLAG_EXECUTABLE);
    struct pan_kmod_bo *stream = pan_kmod_bo_alloc(dev, vm, 4096, PAN_KMOD_BO_FLAG_EXECUTABLE);
    struct pan_kmod_bo *data = pan_kmod_bo_alloc(dev, vm, 4096, 0);
    struct pan_kmod_bo *big = pan_kmod_bo_alloc(dev, vm, BIG, 0);
    CHECK(gallium && panvk && stream && data && big && big->size == BIG, 5);
    volatile uint32_t *gallium_cpu, *panvk_cpu, *data_cpu, *big_cpu;
    unsigned failed;
    if ((failed = fill_and_verify(gallium, 1, &gallium_cpu))) return 30 + failed;
    if ((failed = fill_and_verify(panvk, 2, &panvk_cpu))) return 35 + failed;
    if ((failed = fill_and_verify(data, 3, &data_cpu))) return 40 + failed;
    if ((failed = fill_and_verify(big, 4, &big_cpu))) return 45 + failed;
    volatile uint64_t *stream_cpu = pan_kmod_bo_mmap(stream, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    CHECK(stream_cpu != MAP_FAILED && stream_cpu[0] == 0, 6);
    CHECK(pan_kmod_bo_mmap(gallium, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_SHARED, NULL) == MAP_FAILED, 7);
    say("G7o-f BOs: 5 live (4 KiB + 16 KiB executable, 4 KiB stream, 4 KiB data, %llu MiB), every page filled, remapped and verified",
        (unsigned long long)(BIG / MIB));

    const uint64_t data_va = range.start, gallium_va = range.start + 1 * MIB,
                   panvk_va = range.start + 2 * MIB, stream_va = range.start + 3 * MIB,
                   big_va = range.start + 64 * MIB, moved_va = range.start + 4 * MIB;
    struct pan_kmod_vm_op ops[5] = {
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {data_va, data->size}, .map = {data, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {gallium_va, gallium->size}, .map = {gallium, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {panvk_va, panvk->size}, .map = {panvk, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {stream_va, stream->size}, .map = {stream, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {big_va, big->size}, .map = {big, 0}},
    };
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, ops, 5) == 0, 8);
    say("G7o-f VM: 5 immediate binds through pan_kmod_vm_bind, data 0x%llx, code 0x%llx/0x%llx, stream 0x%llx, 64 MiB at 0x%llx",
        (unsigned long long)data_va, (unsigned long long)gallium_va, (unsigned long long)panvk_va,
        (unsigned long long)stream_va, (unsigned long long)big_va);

    /* Each store runs from its own 64-byte stream line. SYNC_SET32 writes a 32-bit sync
     * object, {value, status}: the value at the 8-byte-aligned target and status 0 in the next
     * word (J747's third boot). The CPU first writes a sentinel there, so status 0 is the GPU's
     * write. Every target's 64-byte line also holds a CPU word, which must survive. */
#define SENTINEL 0xa5a5a5a5u
    unsigned slot = 0;
#define STORE(target, value) \
    kumo_metal_store(api->context, stream_cpu + 8 * slot, stream_va + 64 * slot, (target), (value)); ++slot
    data_cpu[5] = SENTINEL;
    uint32_t status = STORE(data_va + 16, 0x7f0a0001u);
    CHECK(status == 0, 100 + status);
    CHECK(data_cpu[4] == 0x7f0a0001u && data_cpu[5] == 0 && data_cpu[0] == PATTERN(3, 0), 20);
    /* 17 pages across the 64 MiB, alternating the first line (CPU word 0 below the object)
     * and the last line (CPU word 1023 above it), the BO's last page included. */
    const uint64_t last = BIG / 4096 - 1;
    unsigned pages = 0;
    for (uint64_t k = 0; k <= 16; ++k) {
        const uint64_t page = k == 16 ? last : k * (last / 16);
        const bool tail = k & 1;
        const uint64_t word = page * 1024 + (tail ? 1020 : 2);
        const uint64_t kept = page * 1024 + (tail ? 1023 : 0);
        const uint32_t expect = PATTERN(4, page * 2 + (tail ? 1 : 0));
        const uint32_t value = 0x7f0b0000u | (uint32_t)k;
        big_cpu[word + 1] = SENTINEL;
        status = STORE(big_va + word * 4, value);
        CHECK(status == 0, 110 + status);
        if (big_cpu[word] != value || big_cpu[word + 1] != 0 || big_cpu[kept] != expect) {
            say("G7o-f diag: page %llu word %llu read back 0x%08x 0x%08x (want 0x%08x 0), CPU word 0x%08x (want 0x%08x)",
                (unsigned long long)page, (unsigned long long)(word % 1024), big_cpu[word],
                big_cpu[word + 1], value, big_cpu[kept], expect);
            diagnose("big", big, big_cpu, 4, value);
            return 21;
        }
        ++pages;
    }
    say("G7o-f GPU: CS sync stores through Mesa bindings landed {value, status 0} in the data BO and %u pages across 64 MiB; CPU words beside them intact",
        pages);

    /* Live VM mutation through Mesa: exact unbind, rebind elsewhere, store at the new VA. */
    struct pan_kmod_vm_op unmap = {.type = PAN_KMOD_VM_OP_TYPE_UNMAP, .va = {data_va, data->size}};
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &unmap, 1) == 0, 22);
    struct pan_kmod_vm_op moved = {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {moved_va, data->size}, .map = {data, 0}};
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, &moved, 1) == 0, 23);
    data_cpu[9] = SENTINEL;
    status = STORE(moved_va + 32, 0x7f0a0004u);
    CHECK(status == 0, 130 + status);
    CHECK(data_cpu[8] == 0x7f0a0004u && data_cpu[9] == 0 && data_cpu[4] == 0x7f0a0001u, 24);
    say("G7o-f VM: data BO unbound from 0x%llx and rebound at 0x%llx; the GPU store at the new VA landed",
        (unsigned long long)data_va, (unsigned long long)moved_va);

    /* W^X on hardware: the executable binding is read-only, so this store must fault. The
     * session ends; drv-gpu logs the AS1 fault and contains the close with a reset. */
    gallium_cpu[32] = PATTERN(1, 32);
    say("G7o-f W^X: CS store aimed at the executable binding 0x%llx; expecting a fault and a contained close",
        (unsigned long long)(gallium_va + 128));
    status = STORE(gallium_va + 128, 0x7f0a00ffu);
    CHECK(status != 0, 25);
    CHECK(gallium_cpu[32] == PATTERN(1, 32), 26);
    CHECK(pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_FAULTY, 27);
    say("G7o-f W^X: store refused (status %u), executable bytes unchanged, VM faulty", status);
#undef STORE
#undef SENTINEL

    pan_kmod_bo_put(big);
    pan_kmod_bo_put(data);
    pan_kmod_bo_put(stream);
    pan_kmod_bo_put(panvk);
    pan_kmod_bo_put(gallium);
    pan_kmod_vm_destroy(vm);
    pan_kmod_dev_destroy(dev);
    return 0;
}

/* G7p-e. drv-gpu serves one job at a time: it replies to a Submit, runs the job to completion
 * or to its 1 s deadline, and only then reads the next request. Every fence a later request
 * asks about is therefore already settled by the hardware. Dependency steps, ARM and
 * wait-pending run against real completions here. A wake that has to wait for a completion
 * cannot happen on this lane, so WAIT's Port/timer path stays proven in QEMU only.
 * — TIMBERDOODLE */
static uint32_t native_op(struct pan_kmod_dev *dev, uint32_t op, uint64_t id, uint64_t value,
                          struct kumo_gpu_native_reply *out)
{
    const struct kumo_gpu_native_request request = {.operation = op, .id = id, .value = value};
    *out = (struct kumo_gpu_native_reply){UINT64_MAX, UINT32_MAX, UINT32_MAX, UINT64_MAX};
    return pan_kmod_kumo_native(dev, &request, out);
}
static uint32_t native_prepare(struct pan_kmod_dev *dev, uint32_t vm, uint64_t group,
                               uint64_t stream_va, uint32_t stream_bytes, uint64_t point,
                               const struct kumo_gpu_native_point *waits, uint32_t wait_count,
                               const struct kumo_gpu_native_point *signals, uint32_t signal_count,
                               struct kumo_gpu_native_reply *out)
{
    const struct kumo_gpu_native_request request = {
        .operation = KUMO_NATIVE_PREPARE, .id = group,
        .job = {vm, stream_bytes, stream_va, point},
        .waits = waits, .signals = signals, .wait_count = wait_count, .signal_count = signal_count,
    };
    *out = (struct kumo_gpu_native_reply){UINT64_MAX, UINT32_MAX, UINT32_MAX, UINT64_MAX};
    return pan_kmod_kumo_native(dev, &request, out);
}

/* Mali v10 CS words (Mesa genxml/v10.xml), the encodings J747's store already uses. */
#define CS_MOVE48(reg, value) ((UINT64_C(1) << 56) | ((uint64_t)(reg) << 48) | ((value) & UINT64_C(0xffffffffffff)))
#define CS_MOVE32(reg, value) ((UINT64_C(2) << 56) | ((uint64_t)(reg) << 48) | (uint32_t)(value))
#define CS_WAIT_SLOT0 ((UINT64_C(3) << 56) | (UINT64_C(1) << 16))
#define CS_SYNC_SET32(address, data) \
    ((UINT64_C(38) << 56) | ((uint64_t)(address) << 40) | ((uint64_t)(data) << 32))
/* Continue once the 32-bit word at the address pair is GREATER than the data register. */
#define CS_SYNC_WAIT32_GREATER(address, data) \
    ((UINT64_C(39) << 56) | ((uint64_t)(address) << 40) | ((uint64_t)(data) << 32) | (UINT64_C(1) << 28))

/* A store job on its own 64-byte stream line. SYNC_SET32 writes the 8-byte sync object
 * {value, status 0} (J747). Returns the stream's length in bytes. */
static uint32_t store_stream(volatile uint64_t *line, uint64_t target, uint32_t value)
{
    line[0] = CS_MOVE48(0, target);
    line[1] = CS_MOVE32(2, value);
    line[2] = CS_SYNC_SET32(0, 2);
    line[3] = CS_WAIT_SLOT0;
    return 32;
}

/* A job that never finishes and never faults: it waits for `gate` to exceed 0x7fffffff. The
 * CPU wrote 0 there, and 0 is not greater whether the CS compares signed or unsigned. The
 * store after the wait would run only if the wait let go; drv-gpu's CS-status line shows where
 * the stream stopped. */
static uint32_t stall_stream(volatile uint64_t *line, uint64_t gate, uint64_t target, uint32_t value)
{
    line[0] = CS_MOVE48(0, gate);
    line[1] = CS_MOVE32(2, 0x7fffffffu);
    line[2] = CS_SYNC_WAIT32_GREATER(0, 2);
    line[3] = CS_MOVE48(4, target);
    line[4] = CS_MOVE32(6, value);
    line[5] = CS_SYNC_SET32(4, 6);
    line[6] = CS_WAIT_SLOT0;
    return 56;
}

#define SENTINEL 0xa5a5a5a5u
#define VALUE_A 0x7f0e0001u
#define VALUE_B 0x7f0e0002u
#define VALUE_C 0x7f0e00ffu
#define BUSY (KUMO_GPU_SERVER_ERROR + 4u) /* kumo_gpu::Status::Busy */

static unsigned sync_cases(const struct kumo_gpu_api *api, bool timeout)
{
    struct pan_kmod_dev *dev = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(dev && api->version == 15 && api->bytes == 136, 1);
    say("G7p-e device: gpu_id=0x%llx, native C ABI %u with a %u-byte table",
        (unsigned long long)dev->props.gpu_id, api->version, api->bytes);
    struct pan_kmod_va_range range = pan_kmod_dev_query_user_va_range(dev);
    struct pan_kmod_vm *vm = pan_kmod_vm_create(dev, 0, range.start, range.size);
    CHECK(vm && pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_USABLE, 2);

    /* A stream BO (read-only, as Submit requires), an executable BO the fatal job writes to,
     * and a data BO. */
    struct pan_kmod_bo *stream = pan_kmod_bo_alloc(dev, vm, 4096, PAN_KMOD_BO_FLAG_EXECUTABLE);
    struct pan_kmod_bo *code = pan_kmod_bo_alloc(dev, vm, 4096, PAN_KMOD_BO_FLAG_EXECUTABLE);
    struct pan_kmod_bo *data = pan_kmod_bo_alloc(dev, vm, 4096, 0);
    CHECK(stream && code && data, 3);
    volatile uint32_t *code_cpu, *data_cpu;
    unsigned failed;
    if ((failed = fill_and_verify(code, 1, &code_cpu))) return 10 + failed;
    if ((failed = fill_and_verify(data, 3, &data_cpu))) return 15 + failed;
    volatile uint64_t *stream_cpu = pan_kmod_bo_mmap(stream, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    CHECK(stream_cpu != MAP_FAILED && stream_cpu[0] == 0, 4);
    const uint64_t data_va = range.start, code_va = range.start + MIB, stream_va = range.start + 2 * MIB;
    struct pan_kmod_vm_op ops[3] = {
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {data_va, data->size}, .map = {data, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {code_va, code->size}, .map = {code, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {stream_va, stream->size}, .map = {stream, 0}},
    };
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, ops, 3) == 0, 5);

    /* All three streams are written before the GPU runs any of them. A line the GPU might
     * prefetch while an earlier job runs then already holds its final words (J734). */
    data_cpu[5] = SENTINEL;
    data_cpu[9] = SENTINEL;
    data_cpu[16] = 0;
    data_cpu[20] = SENTINEL;
    data_cpu[21] = SENTINEL;
    code_cpu[32] = PATTERN(1, 32);
    const uint32_t a_bytes = store_stream(stream_cpu, data_va + 16, VALUE_A);
    const uint32_t b_bytes = store_stream(stream_cpu + 8, data_va + 32, VALUE_B);
    const uint32_t c_bytes = timeout ? stall_stream(stream_cpu + 16, data_va + 64, data_va + 80, VALUE_C)
                                     : store_stream(stream_cpu + 16, code_va + 128, VALUE_C);
    __asm__ volatile("dsb sy" ::: "memory");

    struct kumo_gpu_native_reply out;
    CHECK(native_op(dev, KUMO_NATIVE_CREATE_BINARY, 0, 0, &out) == KUMO_GPU_OK, 20);
    const uint64_t input = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_CREATE_BINARY, 0, 0, &out) == KUMO_GPU_OK, 21);
    const uint64_t output = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_CREATE_BINARY, 0, 0, &out) == KUMO_GPU_OK, 22);
    const uint64_t cpu = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_CREATE_TIMELINE, 0, 0, &out) == KUMO_GPU_OK, 23);
    const uint64_t timeline = out.id;
    /* The first GroupWatch on hardware: drv-gpu's READ-only status page must decode. */
    CHECK(native_op(dev, KUMO_NATIVE_GROUP_CREATE, 0, vm->handle, &out) == KUMO_GPU_OK &&
          out.value == KUMO_GROUP_USABLE, 24);
    const uint64_t group = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_GROUP_STATE, group, 0, &out) == KUMO_GPU_OK &&
          out.value == KUMO_GROUP_USABLE && out.gpu_status == 0, 25);
    CHECK(native_op(dev, KUMO_NATIVE_SIGNAL, cpu, 0, &out) == KUMO_GPU_OK, 26);
    CHECK(native_op(dev, KUMO_NATIVE_PENDING, cpu, 0, &out) == KUMO_GPU_OK && out.value == 1, 27);
    CHECK(native_op(dev, KUMO_NATIVE_RESET, cpu, 0, &out) == KUMO_GPU_OK, 28);
    CHECK(native_op(dev, KUMO_NATIVE_PENDING, cpu, 0, &out) == KUMO_GPU_OK && out.value == 0, 29);
    say("G7p-e group: native group on VM %u, drv-gpu's status page reads USABLE; a CPU binary signals and resets",
        vm->handle);

    /* A, point 1: signals the binary `input` and timeline 42. */
    const struct kumo_gpu_native_point a_signals[] = {{input, 0}, {timeline, 42}};
    CHECK(native_prepare(dev, vm->handle, group, stream_va, a_bytes, 1, NULL, 0, a_signals, 2, &out) == KUMO_GPU_OK, 30);
    const uint64_t a = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_STEP, a, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_SUBMITTED, 31);
    const uint64_t a_done = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_PENDING, timeline, 42, &out) == KUMO_GPU_OK && out.value == 1, 32);
    say("G7p-e A: point 1 submitted natively, signalling a binary and timeline 42; 42 is wait-pending");

    /* B, point 2: waits on both of A's signals. Nothing has observed A yet, so B captures
     * A's fence, and its step must ask drv-gpu about point 1 before it may submit. */
    const struct kumo_gpu_native_point b_waits[] = {{input, 0}, {timeline, 42}};
    const struct kumo_gpu_native_point b_signals[] = {{output, 0}, {timeline, 100}};
    CHECK(native_prepare(dev, vm->handle, group, stream_va + 64, b_bytes, 2, b_waits, 2, b_signals, 2, &out) == KUMO_GPU_OK, 33);
    const uint64_t b = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_STEP, b, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_SUBMITTED, 34);
    const uint64_t b_done = out.id;
    /* A CPU signal must not overtake B's unobserved GPU point on the same timeline. */
    CHECK(native_op(dev, KUMO_NATIVE_SIGNAL, timeline, 200, &out) == BUSY, 35);
    say("G7p-e B: point 2 captured A's unobserved fence, its step found point 1 done on the GPU and submitted; CPU timeline 200 refused busy");

    CHECK(native_op(dev, KUMO_NATIVE_ARM, a_done, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_COMPLETE, 36);
    CHECK(native_op(dev, KUMO_NATIVE_ARM, b_done, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_COMPLETE, 37);
    if (data_cpu[4] != VALUE_A || data_cpu[5] != 0 || data_cpu[8] != VALUE_B || data_cpu[9] != 0 ||
        data_cpu[0] != PATTERN(3, 0)) {
        say("G7p-e diag: data words 0x%08x 0x%08x 0x%08x 0x%08x 0x%08x", data_cpu[4], data_cpu[5],
            data_cpu[8], data_cpu[9], data_cpu[0]);
        return 38;
    }
    CHECK(native_op(dev, KUMO_NATIVE_PENDING, output, 0, &out) == KUMO_GPU_OK && out.value == 1, 39);
    CHECK(native_op(dev, KUMO_NATIVE_PENDING, timeline, 100, &out) == KUMO_GPU_OK && out.value == 1, 40);
    CHECK(native_op(dev, KUMO_NATIVE_SIGNAL, timeline, 200, &out) == KUMO_GPU_OK, 41);
    CHECK(native_op(dev, KUMO_NATIVE_PENDING, timeline, 200, &out) == KUMO_GPU_OK && out.value == 1, 42);
    CHECK(native_op(dev, KUMO_NATIVE_GROUP_STATE, group, 0, &out) == KUMO_GPU_OK && out.value == KUMO_GROUP_USABLE, 43);
    say("G7p-e ARM: points 1 and 2 complete on the GPU's fences, both {value, status 0} landed; timeline 200 accepted after; group USABLE");

    /* C, point 3: built to fail. drv-gpu accepts it, then fails it after its reply, publishes
     * the group's status and closes the session. ARM's request is never answered; it sees the
     * close. */
    if (timeout)
        say("G7p-e timeout: point 3 waits on a word nobody writes; expecting TIMED_OUT at drv-gpu's 1 s deadline");
    else
        say("G7p-e fatal: point 3 stores into the executable binding 0x%llx; expecting FATAL",
            (unsigned long long)(code_va + 128));
    const struct kumo_gpu_native_point c_signals[] = {{timeline, 300}};
    CHECK(native_prepare(dev, vm->handle, group, stream_va + 128, c_bytes, 3, NULL, 0, c_signals, 1, &out) == KUMO_GPU_OK, 44);
    const uint64_t c = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_STEP, c, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_SUBMITTED, 45);
    const uint64_t c_done = out.id;
    const uint64_t begin = kumo_metal_now_ns();
    const uint32_t lost = native_op(dev, KUMO_NATIVE_ARM, c_done, 0, &out);
    const uint64_t waited_ms = (kumo_metal_now_ns() - begin) / 1000000;
    CHECK(lost == KUMO_GPU_TRANSPORT || lost == KUMO_GPU_DEVICE_LOST, 46);
    CHECK(pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_FAULTY, 47);
    CHECK(native_op(dev, KUMO_NATIVE_GROUP_STATE, group, 0, &out) == KUMO_GPU_OK, 48);
    const uint64_t expected = timeout ? KUMO_GROUP_TIMED_OUT : KUMO_GROUP_FATAL;
    const uint64_t state = out.value;
    const uint32_t gpu_status = out.gpu_status;
    if (state != expected) {
        say("G7p-e diag: group state %llu gpu_status 0x%x after ARM returned %u in %llu ms",
            (unsigned long long)state, gpu_status, lost, (unsigned long long)waited_ms);
        return 49;
    }
    /* Asked again after the loss, the cached fact must not change. */
    CHECK(native_op(dev, KUMO_NATIVE_GROUP_STATE, group, 0, &out) == KUMO_GPU_OK &&
          out.value == state && out.gpu_status == gpu_status, 50);
    /* No BO is read past this point. A known loss closes the shared owner, and that unmaps
     * every local CPU alias (J751); J753's first boot read code_cpu here and took an EL0
     * translation fault. The driver's fault and CS-status lines are the evidence of what the
     * GPU did. */
    if (timeout) {
        CHECK(waited_ms >= 900, 51);
        say("G7p-e timeout: ARM saw the session close (%u) after %llu ms; GROUP_STATE TIMED_OUT gpu_status=0x%x, the same after loss; VM faulty",
            lost, (unsigned long long)waited_ms, gpu_status);
    } else {
        say("G7p-e fatal: ARM saw the session close (%u) after %llu ms; GROUP_STATE FATAL gpu_status=0x%x, the same after loss; VM faulty",
            lost, (unsigned long long)waited_ms, gpu_status);
    }

    pan_kmod_bo_put(data);
    pan_kmod_bo_put(code);
    pan_kmod_bo_put(stream);
    pan_kmod_vm_destroy(vm);
    pan_kmod_dev_destroy(dev);
    return 0;
}
#undef SENTINEL

/* GL1's first rule (J758, CRUX 2026-11-02): Submit takes a stream from a writable no-execute
 * binding, where Gallium's and panvk's command-stream pools keep them. Three jobs through the
 * native bridge. The first runs from a cached NX BO. The second rewrites that same 64-byte line
 * and runs again; Gallium recycles CS chunks, and J747 only ever used fresh lines, so this asks
 * whether a stale copy of the old line survives anywhere between jobs. The third runs from a
 * GPU-uncached NX BO. A stream crossing its binding's end is still refused. — TIMBERDOODLE */
#define NX_SENTINEL 0xa5a5a5a5u
static unsigned nx_stream_cases(const struct kumo_gpu_api *api)
{
    struct pan_kmod_dev *dev = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(dev, 1);
    struct pan_kmod_va_range range = pan_kmod_dev_query_user_va_range(dev);
    struct pan_kmod_vm *vm = pan_kmod_vm_create(dev, 0, range.start, range.size);
    CHECK(vm, 2);
    struct pan_kmod_bo *cached = pan_kmod_bo_alloc(dev, vm, 4096, 0);
    struct pan_kmod_bo *uncached = pan_kmod_bo_alloc(dev, vm, 4096, PAN_KMOD_BO_FLAG_GPU_UNCACHED);
    struct pan_kmod_bo *data = pan_kmod_bo_alloc(dev, vm, 4096, 0);
    CHECK(cached && uncached && data, 3);
    volatile uint64_t *cached_cpu = pan_kmod_bo_mmap(cached, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    volatile uint64_t *uncached_cpu = pan_kmod_bo_mmap(uncached, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    volatile uint32_t *data_cpu = pan_kmod_bo_mmap(data, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    CHECK(cached_cpu != MAP_FAILED && uncached_cpu != MAP_FAILED && data_cpu != MAP_FAILED, 4);
    const uint64_t data_va = range.start, cached_va = range.start + MIB, uncached_va = range.start + 2 * MIB;
    struct pan_kmod_vm_op ops[3] = {
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {data_va, data->size}, .map = {data, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {cached_va, cached->size}, .map = {cached, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {uncached_va, uncached->size}, .map = {uncached, 0}},
    };
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, ops, 3) == 0, 5);

    struct kumo_gpu_native_reply out;
    CHECK(native_op(dev, KUMO_NATIVE_CREATE_TIMELINE, 0, 0, &out) == KUMO_GPU_OK, 10);
    const uint64_t timeline = out.id;
    CHECK(native_op(dev, KUMO_NATIVE_GROUP_CREATE, 0, vm->handle, &out) == KUMO_GPU_OK &&
          out.value == KUMO_GROUP_USABLE, 11);
    const uint64_t group = out.id;
    say("GL1-a streams: two writable no-execute BOs (GPU-cached at 0x%llx, uncached at 0x%llx) hold the command streams",
        (unsigned long long)cached_va, (unsigned long long)uncached_va);

    /* One job: the store stream at `stream_cpu`/`stream_va`, timeline point `point * 10`. */
    const uint64_t streams_va[3] = {cached_va, cached_va, uncached_va};
    volatile uint64_t *streams_cpu[3] = {cached_cpu, cached_cpu, uncached_cpu};
    const uint32_t values[3] = {0x7f1a0001u, 0x7f1a0002u, 0x7f1a0003u};
    for (unsigned job = 0; job < 3; ++job) {
        const unsigned word = 4 + 4 * job;
        data_cpu[word + 1] = NX_SENTINEL;
        const uint32_t bytes = store_stream(streams_cpu[job], data_va + 4 * word, values[job]);
        __asm__ volatile("dsb sy" ::: "memory");
        const struct kumo_gpu_native_point signals[] = {{timeline, 10 * (job + 1)}};
        CHECK(native_prepare(dev, vm->handle, group, streams_va[job], bytes, job + 1, NULL, 0, signals, 1, &out) ==
              KUMO_GPU_OK, 20 + job);
        const uint64_t intent = out.id;
        CHECK(native_op(dev, KUMO_NATIVE_STEP, intent, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_SUBMITTED,
              23 + job);
        CHECK(native_op(dev, KUMO_NATIVE_ARM, out.id, 0, &out) == KUMO_GPU_OK && out.state == KUMO_NATIVE_COMPLETE,
              26 + job);
        if (data_cpu[word] != values[job] || data_cpu[word + 1] != 0) {
            say("GL1-a diag: job %u read back 0x%08x 0x%08x (want 0x%08x 0); earlier words 0x%08x 0x%08x",
                job + 1, data_cpu[word], data_cpu[word + 1], values[job], data_cpu[4], data_cpu[8]);
            return 30 + job;
        }
    }
    CHECK(data_cpu[4] == values[0] && data_cpu[8] == values[1], 33);
    say("GL1-a cached: a stream in the cached NX BO ran and {0x%08x, status 0} landed", values[0]);
    say("GL1-a rewrite: the same line rewritten ran its new words; {0x%08x, status 0} landed, the first store intact",
        values[1]);
    say("GL1-a uncached: a stream in the uncached NX BO ran and {0x%08x, status 0} landed", values[2]);

    /* Still one binding per stream: crossing the cached BO's end is refused locally. */
    const struct kumo_gpu_native_point late[] = {{timeline, 40}};
    CHECK(native_prepare(dev, vm->handle, group, cached_va + 4096 - 64, 128, 4, NULL, 0, late, 1, &out) ==
          KUMO_GPU_INVALID_REQUEST, 40);
    say("GL1-a refusal: a stream crossing its binding's end was refused");

    pan_kmod_bo_put(data);
    pan_kmod_bo_put(uncached);
    pan_kmod_bo_put(cached);
    pan_kmod_vm_destroy(vm);
    pan_kmod_dev_destroy(dev);
    return 0;
}
#undef NX_SENTINEL

/* G7q-a. The heap's memory is the driver's and the client cannot map it, so the GPU reads it:
 * each job loads one chunk header and stores it into the data BO, and the next job follows the
 * link the GPU reported. The walk is therefore the chain as the GPU sees it, not the layout
 * the driver meant. After HeapDestroy, a load from the old head must fault. — TIMBERDOODLE */
#define CS_LOAD_MULTIPLE(base, address, mask) \
    ((UINT64_C(20) << 56) | ((uint64_t)(base) << 48) | ((uint64_t)(address) << 40) | ((uint64_t)(mask) << 16))
#define CS_STORE_MULTIPLE(base, address, mask) \
    ((UINT64_C(21) << 56) | ((uint64_t)(base) << 48) | ((uint64_t)(address) << 40) | ((uint64_t)(mask) << 16))
#define CS_WAIT_ALL ((UINT64_C(3) << 56) | (UINT64_C(0xff) << 16))
/* kumo_gpu::HEAP_VA_START/END: no C header carries them until G7q-c's ABI. */
#define KUMO_HEAP_VA_START UINT64_C(0x30000000)
#define KUMO_HEAP_VA_END UINT64_C(0x38000000)
#define HEAP_CHUNK (128u * 1024u)
#define HEAP_CHUNKS 3u

/* Copy `words` 32-bit words from `source` into the data BO at `target` through r10 onwards. */
static uint32_t copy_job(void *context, volatile uint64_t *stream_cpu, uint64_t stream_va,
                         uint64_t source, uint64_t target, unsigned words)
{
    const uint64_t job[] = {
        CS_MOVE48(0, source),
        CS_LOAD_MULTIPLE(10, 0, (1u << words) - 1),
        CS_WAIT_ALL,
        CS_MOVE48(2, target),
        CS_STORE_MULTIPLE(10, 2, (1u << words) - 1),
        CS_WAIT_ALL,
    };
    return kumo_metal_run(context, stream_cpu, stream_va, job, sizeof job / sizeof job[0]);
}

/* Proof 3 creates and destroys the heap on the session's Client; proof 4 goes through the
 * published device's ABI 15 entry points, as GL1's pan_csf.c seam will. — TIMBERDOODLE */
static unsigned heap_cases(const struct kumo_gpu_api *api, bool bridge)
{
    const char *tag = bridge ? "G7q-c" : "G7q-a";
    struct pan_kmod_dev *dev = pan_kmod_kumo_dev_create(api, NULL);
    CHECK(dev, 1);
    struct pan_kmod_va_range range = pan_kmod_dev_query_user_va_range(dev);
    struct pan_kmod_vm *vm = pan_kmod_vm_create(dev, 0, range.start, range.size);
    CHECK(vm && vm->handle == 1, 2);
    struct pan_kmod_bo *stream = pan_kmod_bo_alloc(dev, vm, 4096, PAN_KMOD_BO_FLAG_EXECUTABLE);
    struct pan_kmod_bo *data = pan_kmod_bo_alloc(dev, vm, 4096, 0);
    CHECK(stream && data, 3);
    volatile uint64_t *stream_cpu = pan_kmod_bo_mmap(stream, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    volatile uint64_t *data_cpu = pan_kmod_bo_mmap(data, PROT_READ | PROT_WRITE, MAP_SHARED, NULL);
    CHECK(stream_cpu != MAP_FAILED && data_cpu != MAP_FAILED, 4);
    const uint64_t data_va = range.start, stream_va = range.start + 2 * MIB;
    struct pan_kmod_vm_op ops[2] = {
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {data_va, data->size}, .map = {data, 0}},
        {.type = PAN_KMOD_VM_OP_TYPE_MAP, .va = {stream_va, stream->size}, .map = {stream, 0}},
    };
    CHECK(pan_kmod_vm_bind(vm, PAN_KMOD_VM_OP_MODE_IMMEDIATE, ops, 2) == 0, 5);

    uint64_t heap[3] = {0};
    if (bridge) {
        const struct kumo_gpu_heap_request request = {vm->handle, HEAP_CHUNK, HEAP_CHUNKS, HEAP_CHUNKS + 1, 65535, 0};
        struct kumo_gpu_heap_info info;
        CHECK(pan_kmod_kumo_heap_create(dev, &request, &info) == KUMO_GPU_OK && info.reserved == 0, 10);
        heap[0] = info.heap, heap[1] = info.context_va, heap[2] = info.first_chunk_va;
    } else {
        CHECK(kumo_metal_heap_create(api->context, HEAP_CHUNK, HEAP_CHUNKS, HEAP_CHUNKS + 1, heap) == 0, 10);
    }
    const uint64_t context_va = heap[1], head = heap[2];
    CHECK(context_va >= KUMO_HEAP_VA_START && head < KUMO_HEAP_VA_END && context_va % 4096 == 0 &&
          head % 4096 == 0 && head != context_va, 11);
    say("%s heap: %u chunks of %u bytes, context 0x%llx, head 0x%llx", tag,
        HEAP_CHUNKS, HEAP_CHUNK, (unsigned long long)context_va, (unsigned long long)head);

    /* Walk from the head: every link the GPU reads must carry the chunk size in pages and
     * name a page in the window; the last must be zero. */
    unsigned line = 0, links = 0;
    uint64_t va = head, seen[HEAP_CHUNKS] = {0};
    for (;;) {
        CHECK(links < HEAP_CHUNKS, 20);
        seen[links] = va;
        data_cpu[links] = UINT64_C(0xa5a5a5a5a5a5a5a5);
        const uint32_t status = copy_job(api->context, stream_cpu + 8 * line, stream_va + 64 * line,
                                         va, data_va + 8 * links, 2);
        ++line;
        CHECK(status == 0, 30 + status);
        const uint64_t word = data_cpu[links];
        ++links;
        if (word == 0)
            break;
        if ((word & 0xfff) != HEAP_CHUNK / 4096 || (word & ~UINT64_C(0xfff)) < KUMO_HEAP_VA_START ||
            (word & ~UINT64_C(0xfff)) >= KUMO_HEAP_VA_END) {
            say("%s diag: link %u at 0x%llx read 0x%llx", tag, links, (unsigned long long)va,
                (unsigned long long)word);
            return 21;
        }
        va = word & ~UINT64_C(0xfff);
    }
    CHECK(links == HEAP_CHUNKS, 22);
    /* Distinct chunks that never overlap one another or the context page. */
    for (unsigned i = 0; i < links; ++i) {
        CHECK(seen[i] + HEAP_CHUNK <= context_va || seen[i] >= context_va + 4096, 23);
        for (unsigned j = i + 1; j < links; ++j)
            CHECK(seen[i] + HEAP_CHUNK <= seen[j] || seen[j] + HEAP_CHUNK <= seen[i], 24);
    }
    say("%s walk: the GPU followed %u links from the head through 0x%llx and 0x%llx; each carried the chunk size, the last was 0",
        tag, links, (unsigned long long)seen[1], (unsigned long long)seen[2]);

    /* The context is the GPU's and starts zeroed. */
    for (unsigned i = 8; i < 12; ++i)
        data_cpu[i] = UINT64_C(0xa5a5a5a5a5a5a5a5);
    const uint32_t status = copy_job(api->context, stream_cpu + 8 * line, stream_va + 64 * line,
                                     context_va, data_va + 64, 8);
    ++line;
    CHECK(status == 0, 40 + status);
    CHECK(data_cpu[8] == 0 && data_cpu[9] == 0 && data_cpu[10] == 0 && data_cpu[11] == 0, 41);
    say("%s context: the GPU read its 32 bytes at 0x%llx, all zero", tag, (unsigned long long)context_va);

    uint64_t bytes = 0;
    if (bridge) {
        CHECK(pan_kmod_kumo_heap_destroy(dev, (uint32_t)heap[0], &bytes) == KUMO_GPU_OK, 50);
        uint64_t again = 1;
        CHECK(pan_kmod_kumo_heap_destroy(dev, (uint32_t)heap[0], &again) == KUMO_GPU_UNKNOWN_OBJECT && again == 0, 54);
    } else {
        CHECK(kumo_metal_heap_destroy(api->context, (uint32_t)heap[0], &bytes) == 0, 50);
    }
    CHECK(bytes == 4096 + HEAP_CHUNKS * HEAP_CHUNK, 51);
    say("%s destroy: %llu bytes returned; a load from the old head 0x%llx must now fault", tag,
        (unsigned long long)bytes, (unsigned long long)head);
    /* No BO is read after this: the fault ends the session and unmaps its aliases. */
    const uint32_t lost = copy_job(api->context, stream_cpu + 8 * line, stream_va + 64 * line, head,
                                   data_va + 128, 2);
    CHECK(lost != 0, 52);
    CHECK(pan_kmod_vm_query_state(vm) == PAN_KMOD_VM_FAULTY, 53);
    say("%s destroy: the load faulted (status %u) and the VM is faulty", tag, lost);

    pan_kmod_bo_put(data);
    pan_kmod_bo_put(stream);
    pan_kmod_vm_destroy(vm);
    pan_kmod_dev_destroy(dev);
    return 0;
}

unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    if (KUMO_METAL_PROOF == 3 || KUMO_METAL_PROOF == 4)
        return heap_cases(api, KUMO_METAL_PROOF == 4);
    if (KUMO_METAL_PROOF == 5)
        return nx_stream_cases(api);
#if KUMO_METAL_PROOF == 6
    return kumo_gl_screen_cases(api, kumo_metal_log);
#endif
#if KUMO_METAL_PROOF == 7
    return kumo_gl_context_init_cases(api, kumo_metal_log);
#endif
#if KUMO_METAL_PROOF == 8
    return kumo_gl_batch_cases(api, kumo_metal_log, NULL);
#endif
#if KUMO_METAL_PROOF == 9
    return kumo_gl_frontend_cases(api, kumo_metal_log);
#endif
    return KUMO_METAL_PROOF == 0 ? device_cases(api) : sync_cases(api, KUMO_METAL_PROOF == 2);
}

#if KUMO_METAL_PROOF == 0
#define BEGIN_LINE "G7o-f metal device begin: live drv-gpu session, real Mesa lifecycle, DMA pages and page tables"
#define FAILED_LINE "G7o-f metal device FAILED at case %u"
#define GREEN_LINE "G7o-f metal device green: live props, Mesa BOs and VM, GPU stores through Mesa bindings, live rebind, W^X store faulted and contained"
#elif KUMO_METAL_PROOF == 1
#define BEGIN_LINE "G7p-e metal sync begin (fatal): live drv-gpu session, native C sync and group state on Mali"
#define FAILED_LINE "G7p-e metal sync FAILED at case %u"
#define GREEN_LINE "G7p-e metal sync green (fatal): native binary and timeline syncs, a dependency checked against the GPU, ARM completions, wait-pending and FATAL group state on Mali"
#elif KUMO_METAL_PROOF == 2
#define BEGIN_LINE "G7p-e metal sync begin (timeout): live drv-gpu session, native C sync and group state on Mali"
#define FAILED_LINE "G7p-e metal sync FAILED at case %u"
#define GREEN_LINE "G7p-e metal sync green (timeout): native binary and timeline syncs, a dependency checked against the GPU, ARM completions, wait-pending and TIMED_OUT group state on Mali"
#elif KUMO_METAL_PROOF == 3
#define BEGIN_LINE "G7q-a metal heap begin: live drv-gpu session, a tiler heap the GPU walks"
#define FAILED_LINE "G7q-a metal heap FAILED at case %u"
#define GREEN_LINE "G7q-a metal heap green: HeapCreate placed and linked the chunks the GPU walked, the context read zero, and HeapDestroy unmapped them"
#elif KUMO_METAL_PROOF == 4
#define BEGIN_LINE "G7q-c metal heap begin: live drv-gpu session, a tiler heap through the C bridge the GPU walks"
#define FAILED_LINE "G7q-c metal heap FAILED at case %u"
#define GREEN_LINE "G7q-c metal heap green: pan_kmod_kumo_heap_create placed and linked the chunks the GPU walked, and pan_kmod_kumo_heap_destroy unmapped them"
#elif KUMO_METAL_PROOF == 5
#define BEGIN_LINE "GL1-a metal streams begin: live drv-gpu session, command streams in writable no-execute BOs"
#define FAILED_LINE "GL1-a metal streams FAILED at case %u"
#define GREEN_LINE "GL1-a metal streams green: Submit ran streams from cached and uncached writable no-execute bindings, a rewritten line ran its new words, and a straddling stream was refused"
#elif KUMO_METAL_PROOF == 6
#define BEGIN_LINE "GL1-b metal screen begin: live drv-gpu session, Gallium's panfrost screen over the native device"
#define FAILED_LINE "GL1-b metal screen FAILED at case %u"
#define GREEN_LINE "GL1-b metal screen green: Gallium's panfrost screen came up on Mali over the native device, its caps read, a texture made, and everything destroyed"
#elif KUMO_METAL_PROOF == 7
#define BEGIN_LINE "GL1-c metal context begin: live drv-gpu session, Gallium's CSF initialization through the native bridge"
#define FAILED_LINE "GL1-c metal context FAILED at case %u"
#define GREEN_LINE "GL1-c metal context green: Gallium's native initialization completed, a second context was refused, and the heap, context and screen were destroyed"
#elif KUMO_METAL_PROOF == 8
#define BEGIN_LINE "GL1-d metal batch begin: live drv-gpu session, Gallium's clear, flush, fence and readback over the native bridge"
#define FAILED_LINE "GL1-d metal batch FAILED at case %u"
#define GREEN_LINE "GL1-d metal batch green: Mali cleared the render target through Gallium, the native fence completed, the CPU read the clear color back, and everything was destroyed"
#elif KUMO_METAL_PROOF == 9
#define BEGIN_LINE "GL1-e metal frontend begin: live drv-gpu session, Mesa GLES2 context and KUMO drawable"
#define FAILED_LINE "GL1-e metal frontend FAILED at case %u"
#define GREEN_LINE "GL1-e metal frontend green: Mesa context made current on Mali, color/depth validated, front flush completed, context and display destroyed"
#else
#error "KUMO_METAL_PROOF must be 0 to 9"
#endif

extern unsigned kumo_metal_proof(void);
int main(void)
{
    kumo_metal_log(BEGIN_LINE);
    unsigned failed = kumo_metal_proof();
    if (failed) {
        say(FAILED_LINE, failed);
        return 1;
    }
    kumo_metal_log(GREEN_LINE);
    return 0;
}
