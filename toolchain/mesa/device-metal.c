/* SPDX-License-Identifier: MIT */
/*j747*/
/* G7o-f: the native Mesa device, BOs and VM on the Orange Pi, over the live drv-gpu session.
 * Real props, real DMA pages, real page tables. The GPU itself checks the VM path: CS stores
 * through Mesa-made bindings, a live unbind and rebind, and a final store aimed at an
 * executable binding, which must fault and end the session (G7o-e's W^X rule on hardware).
 * — TIMBERDOODLE */
#include "kumo_kmod.h"
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <sys/mman.h>

/* device-metal.rs: one kernel-log line (a Sora-launched client has no stdio), and one CS
 * store through the session's native group. */
extern void kumo_metal_log(const char *line);
extern uint32_t kumo_metal_store(void *context, volatile uint64_t *stream_cpu, uint64_t stream_va,
                                 uint64_t target, uint32_t value);

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

unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
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
     * (EXECUTABLE is Mesa's only read-only binding, and KUMO's Submit reads its stream from
     * one), a data BO and the 64 MiB cap. */
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

extern unsigned kumo_metal_proof(void);
int main(void)
{
    kumo_metal_log("G7o-f metal device begin: live drv-gpu session, real Mesa lifecycle, DMA pages and page tables");
    unsigned failed = kumo_metal_proof();
    if (failed) {
        say("G7o-f metal device FAILED at case %u", failed);
        return 1;
    }
    kumo_metal_log("G7o-f metal device green: live props, Mesa BOs and VM, GPU stores through Mesa bindings, live rebind, W^X store faulted and contained");
    return 0;
}
