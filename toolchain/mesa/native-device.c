/* SPDX-License-Identifier: MIT */
/*j760*/
/*j761*/
/*j762*/
/*j765*/
/*j780*/
#include "kumo_kmod.h"
#include "drm-uapi/panthor_drm.h"
/* Pinned upstream perf helpers use GNU void-pointer arithmetic. — KESTREL */
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wgnu-pointer-arith"
#include "kmod/pan_kmod_backend.h"
#pragma GCC diagnostic pop
#include <stdlib.h>
#include <sys/mman.h>
#include "pan_props.h"
#include "util/os_time.h"
#include "util/vma.h"

/* GL1-b: an AUTO_VA VM hands out addresses from one heap over its whole range. The window
 * is one 4 GiB region, so G7o-e's executable rule holds for any address it picks. — TIMBERDOODLE */
struct kumo_vm { struct pan_kmod_vm base; uint64_t start, end; struct util_vma_heap heap;
                 /* GL1-d: the native timeline (0 until first use) and its last point. */
                 uint64_t timeline, point; };
#define KUMO_VM_FLAGS (PAN_KMOD_VM_FLAG_AUTO_VA | PAN_KMOD_VM_FLAG_TRACK_ACTIVITY)

struct kumo_device {
    struct pan_kmod_dev base;
    struct kumo_gpu_api api;
    struct pan_kmod_va_range range;
    uint32_t cs_reg_count;
    /* VK2-d: Panthor's CSIF block, as panvk reads it, from the attested CS_FEATURES. */
    struct drm_panthor_csif_info csif;
    struct kumo_mesa_bo *bos;
    struct pan_kmod_vm *vm;
    bool lost;
};

static struct kumo_device *device(const struct pan_kmod_dev *base)
{
    return base && base->ops == &kumo_kmod_ops ? (struct kumo_device *)base : NULL;
}
static bool ready(struct kumo_device *dev)
{
    return dev && !dev->lost && dev->api.device_status(dev->api.context) == KUMO_GPU_OK;
}
uint32_t pan_kmod_kumo_cs_reg_count(const struct pan_kmod_dev *base)
{
    const struct kumo_device *dev = device(base);
    return dev ? dev->cs_reg_count : 0;
}
const struct drm_panthor_csif_info *pan_kmod_kumo_csif_props(const struct pan_kmod_dev *base)
{
    const struct kumo_device *dev = device(base);
    return dev ? &dev->csif : NULL;
}
static void poison(struct kumo_device *dev)
{
    if (!dev->lost) {
        dev->lost = true;
        dev->api.device_close(dev->api.context);
    }
}
void pan_kmod_kumo_dev_lost(struct pan_kmod_dev *base)
{
    struct kumo_device *dev = device(base);
    if (dev) poison(dev);
}
static void *zalloc(const struct pan_kmod_allocator *allocator, size_t bytes, bool transient)
{
    (void)allocator; (void)transient;
    return calloc(1, bytes);
}
static void release(const struct pan_kmod_allocator *allocator, void *pointer)
{
    (void)allocator;
    free(pointer);
}
static const struct pan_kmod_allocator default_allocator = {.zalloc = zalloc, .free = release};

static bool api_valid(const struct kumo_gpu_api *api)
{
    return api && api->version == KUMO_GPU_API_VERSION && api->bytes == sizeof(*api) &&
           api->context && api->query && api->buffer_create && api->buffer_destroy &&
           api->buffer_map && api->buffer_unmap && api->device_open && api->device_close &&
           api->vm_create && api->vm_destroy && api->vm_bind && api->vm_unbind &&
           api->device_status && api->native && api->heap_create && api->heap_destroy;
}
uint32_t pan_kmod_kumo_native(struct pan_kmod_dev *base,
                             const struct kumo_gpu_native_request *request,
                             struct kumo_gpu_native_reply *out)
{
    if (!out) return KUMO_GPU_BAD_ARGUMENT;
    *out = (struct kumo_gpu_native_reply){0};
    if (!request) return KUMO_GPU_BAD_ARGUMENT;
    struct kumo_device *dev = device(base);
    if (!dev) return KUMO_GPU_INVALID_REQUEST;
    if (!ready(dev) && request->operation != KUMO_NATIVE_GROUP_STATE) {
        poison(dev); return KUMO_GPU_DEVICE_LOST;
    }
    uint32_t status = dev->api.native(dev->api.context, request, out);
    /* Loss is terminal across BO, VM and sync callbacks. Closing retains the server's
     * reset-before-DMA-release obligation; C never fabricates quiescence. — MILVUS */
    if (dev->api.device_status(dev->api.context) != KUMO_GPU_OK) poison(dev);
    return status;
}
int pan_kmod_kumo_sync_create(struct pan_kmod_dev *base, bool signaled, uint64_t *id)
{
    if (!id) return -1;
    *id = 0;
    const struct kumo_gpu_native_request request = {
        .operation = KUMO_NATIVE_CREATE_BINARY, .value = signaled,
    };
    struct kumo_gpu_native_reply out;
    if (pan_kmod_kumo_native(base, &request, &out) != KUMO_GPU_OK) return -1;
    if (!out.id) {
        struct kumo_device *dev = device(base);
        if (dev) poison(dev);
        return -1;
    }
    *id = out.id;
    return 0;
}
void pan_kmod_kumo_sync_destroy(struct pan_kmod_dev *base, uint64_t id)
{
    if (!id) return;
    const struct kumo_gpu_native_request request = {.operation = KUMO_NATIVE_DROP, .id = id};
    struct kumo_gpu_native_reply out;
    if (pan_kmod_kumo_native(base, &request, &out) != KUMO_GPU_OK) {
        struct kumo_device *dev = device(base);
        if (dev) poison(dev);
    }
}
uint32_t pan_kmod_kumo_heap_create(struct pan_kmod_dev *base,
                                   const struct kumo_gpu_heap_request *request,
                                   struct kumo_gpu_heap_info *out)
{
    if (!out) return KUMO_GPU_BAD_ARGUMENT;
    *out = (struct kumo_gpu_heap_info){0};
    if (!request) return KUMO_GPU_BAD_ARGUMENT;
    struct kumo_device *dev = device(base);
    if (!dev) return KUMO_GPU_INVALID_REQUEST;
    if (!ready(dev)) {
        poison(dev); return KUMO_GPU_DEVICE_LOST;
    }
    uint32_t status = dev->api.heap_create(dev->api.context, request, out);
    if (dev->api.device_status(dev->api.context) != KUMO_GPU_OK) poison(dev);
    return status;
}
uint32_t pan_kmod_kumo_heap_destroy(struct pan_kmod_dev *base, uint32_t heap, uint64_t *bytes)
{
    if (!bytes) return KUMO_GPU_BAD_ARGUMENT;
    *bytes = 0;
    struct kumo_device *dev = device(base);
    if (!dev) return KUMO_GPU_INVALID_REQUEST;
    if (!ready(dev)) {
        poison(dev); return KUMO_GPU_DEVICE_LOST;
    }
    uint32_t status = dev->api.heap_destroy(dev->api.context, heap, bytes);
    if (dev->api.device_status(dev->api.context) != KUMO_GPU_OK) poison(dev);
    return status;
}
static uint32_t native_op(struct pan_kmod_dev *base, uint32_t operation, uint64_t id, uint64_t value,
                          struct kumo_gpu_native_reply *out)
{
    const struct kumo_gpu_native_request request = {.operation = operation, .id = id, .value = value};
    return pan_kmod_kumo_native(base, &request, out);
}
static struct kumo_vm *tracked_vm(struct pan_kmod_vm *vm)
{
    struct kumo_device *dev = device(vm ? vm->dev : NULL);
    return dev && dev->vm == vm && (vm->flags & PAN_KMOD_VM_FLAG_TRACK_ACTIVITY) ? (struct kumo_vm *)vm : NULL;
}
/* GL1-d: the timeline is created at the first submission, so a screen that never submits
 * (and GL1-c2's observed context refusals) holds no extra native object. — TIMBERDOODLE */
uint64_t pan_kmod_kumo_vm_sync_lock(struct pan_kmod_vm *vm, uint64_t *timeline)
{
    struct kumo_vm *kvm = tracked_vm(vm);
    *timeline = 0;
    if (!kvm) return 0;
    if (!kvm->timeline) {
        struct kumo_gpu_native_reply out;
        if (native_op(vm->dev, KUMO_NATIVE_CREATE_TIMELINE, 0, 0, &out) != KUMO_GPU_OK || !out.id) {
            poison(device(vm->dev));
            return 0;
        }
        kvm->timeline = out.id;
    }
    *timeline = kvm->timeline;
    return kvm->point;
}
void pan_kmod_kumo_vm_sync_unlock(struct pan_kmod_vm *vm, uint64_t point)
{
    struct kumo_vm *kvm = tracked_vm(vm);
    if (kvm && point > kvm->point) kvm->point = point;
}
int pan_kmod_kumo_bo_attach_sync_point(struct pan_kmod_bo *base, uint64_t point, bool written)
{
    struct kumo_device *dev = device(base ? base->dev : NULL);
    if (!dev || !point || !dev->vm || point > ((struct kumo_vm *)dev->vm)->point) return -1;
    struct kumo_mesa_bo *bo = (struct kumo_mesa_bo *)base;
    bo->read_point = MAX2(bo->read_point, point);
    if (written) bo->write_point = MAX2(bo->write_point, point);
    return 0;
}
int pan_kmod_kumo_sync_snapshot(struct pan_kmod_dev *base, uint64_t sync, uint64_t value,
                                uint64_t *snapshot)
{
    *snapshot = 0;
    struct kumo_gpu_native_reply out;
    if (native_op(base, KUMO_NATIVE_SNAPSHOT, sync, value, &out) != KUMO_GPU_OK) {
        struct kumo_device *dev = device(base);
        if (dev) poison(dev);
        return -1;
    }
    if (out.state != KUMO_NATIVE_AVAILABLE || !out.id) return -1;
    *snapshot = out.id;
    return 0;
}
int pan_kmod_kumo_snapshot_wait(struct pan_kmod_dev *base, uint64_t snapshot, int64_t timeout_ns)
{
    struct kumo_device *dev = device(base);
    if (!dev) return -1;
    const int64_t start = os_time_get_nano();
    const int64_t deadline = timeout_ns < 0 || timeout_ns > INT64_MAX - start ? INT64_MAX
                                                                                : start + timeout_ns;
    for (unsigned closed = 0; snapshot;) {
        struct kumo_gpu_native_reply out;
        if (native_op(base, KUMO_NATIVE_ARM, snapshot, 0, &out) != KUMO_GPU_OK) break;
        if (out.state == KUMO_NATIVE_COMPLETE) return 1;
        if (out.state != KUMO_NATIVE_AVAILABLE || !out.id) break;
        const uint64_t wake = out.id;
        const int64_t left = deadline == INT64_MAX ? INT64_MAX : deadline - os_time_get_nano();
        if (left <= 0) {
            /* Pending at the deadline: give the wake back and report the timeout. */
            if (native_op(base, KUMO_NATIVE_DROP, wake, 0, &out) != KUMO_GPU_OK) break;
            return 0;
        }
        const uint64_t bound = MIN2((uint64_t)left, KUMO_NATIVE_MAX_TIMEOUT_NS);
        if (native_op(base, KUMO_NATIVE_WAIT, wake, bound, &out) != KUMO_GPU_OK) break;
        /* Two peer closes in a row with no completion between them is a broken producer. */
        if (out.state == KUMO_NATIVE_CLOSED) {
            if (++closed > 1) break;
        } else if (out.state == KUMO_NATIVE_WOKEN || out.state == KUMO_NATIVE_TIMED_OUT) {
            closed = 0;
        } else {
            break;
        }
    }
    /* A failed fence, a refused wait or an unknown snapshot: no completion can be claimed
     * and no BO can be read, so the device is lost before the caller continues. */
    poison(dev);
    return -1;
}
int pan_kmod_kumo_vm_wait(struct pan_kmod_vm *vm, uint64_t point, int64_t timeout_ns)
{
    struct kumo_vm *kvm = tracked_vm(vm);
    if (!kvm) return -1;
    if (!point) return 1;
    /* Points are attached only after their submission, so one with nothing behind it is a
     * broken record rather than work still to come. */
    uint64_t snapshot;
    if (!kvm->timeline || point > kvm->point ||
        pan_kmod_kumo_sync_snapshot(vm->dev, kvm->timeline, point, &snapshot)) {
        poison(device(vm->dev));
        return -1;
    }
    const int result = pan_kmod_kumo_snapshot_wait(vm->dev, snapshot, timeout_ns);
    struct kumo_gpu_native_reply out;
    if (native_op(vm->dev, KUMO_NATIVE_DROP, snapshot, 0, &out) != KUMO_GPU_OK) {
        poison(device(vm->dev));
        return -1;
    }
    return result;
}
struct pan_kmod_dev *
pan_kmod_kumo_dev_create(const struct kumo_gpu_api *api, const struct pan_kmod_allocator *allocator)
{
    if (!api_valid(api) || (allocator && (!allocator->zalloc || !allocator->free)))
        return NULL;
    struct kumo_mesa_report report;
    struct pan_kmod_dev_props props;
    if (kumo_mesa_query_properties(api, &report, &props) != KUMO_MESA_INCOMPLETE ||
        report.missing_properties || !report.model_resolved || props.is_io_coherent ||
        props.pgsize_bitmap != PAN_PGSIZE_4K || !report.info.va_start ||
        report.info.va_start >= report.info.va_end ||
        report.info.va_start % PAN_PGSIZE_4K || report.info.va_end % PAN_PGSIZE_4K ||
        report.info.va_start >> 32 != (report.info.va_end - 1) >> 32)
        return NULL;
    /* Gallium (v10, unchecked) and panvk (asserted) store a blend shader's PC as 32 bits, so
     * all executable BOs must share one 4 GiB region. The window is the drivers' whole user
     * VA range; one crossing a 4 GiB boundary could split their code, so it is refused, not
     * published. An auto-VA backend must keep this invariant (G7o-e). — TIMBERDOODLE */
    if (!allocator) allocator = &default_allocator;
    struct kumo_device *dev = allocator->zalloc(allocator, sizeof(*dev), false);
    if (!dev) return NULL;
    /* No DRM version is fabricated and no FD is owned. Native selection is this explicit
     * capability factory; the FD-based vtable entry always refuses. — KESTREL */
    const struct pan_kmod_driver driver = {0};
    pan_kmod_dev_init(&dev->base, -1, PAN_KMOD_DEV_FLAG_MMAP_SYNC_THROUGH_KERNEL,
                      &driver, &kumo_kmod_ops, allocator);
    dev->base.props = props;
    dev->cs_reg_count = (report.info.cs_features & 0xff) + 1;
    /* CS_FEATURES[15:8] is the scoreboard count. The session's one group has three queues,
     * and the Submit wrapper keeps the last four registers, as Panthor's kernel does. */
    dev->csif = (struct drm_panthor_csif_info){
        .csg_slot_count = 1, .cs_slot_count = KUMO_NATIVE_MAX_STREAMS,
        .cs_reg_count = dev->cs_reg_count,
        .scoreboard_slot_count = (report.info.cs_features >> 8) & 0xff,
        .unpreserved_cs_reg_count = 4,
    };
    dev->api = *api;
    dev->range = (struct pan_kmod_va_range){report.info.va_start, report.info.va_end - report.info.va_start};
    if (api->device_open(api->context) != KUMO_GPU_OK) {
        pan_kmod_dev_cleanup(&dev->base);
        allocator->free(allocator, dev);
        return NULL;
    }
    return &dev->base;
}
static struct pan_kmod_dev *refuse_fd(int fd, uint32_t flags, const struct pan_kmod_driver *driver,
                                     const struct pan_kmod_allocator *allocator)
{
    (void)fd; (void)flags; (void)driver; (void)allocator;
    return NULL;
}
static void destroy_device(struct pan_kmod_dev *base)
{
    struct kumo_device *dev = device(base);
    if (!dev) return;
    /* Device destruction invalidates all child objects. Local aliases/grants close before
     * the endpoint; the server still owns DMA and resets before freeing it. — KESTREL */
    dev->api.device_close(dev->api.context);
    if (dev->vm) {
        pan_kmod_vm_cleanup(dev->vm);
        pan_kmod_dev_free(base, dev->vm);
    }
    while (dev->bos) {
        struct kumo_mesa_bo *bo = dev->bos;
        dev->bos = bo->next;
        pan_kmod_dev_free(base, bo);
    }
    const struct pan_kmod_allocator *allocator = base->allocator;
    pan_kmod_dev_cleanup(base);
    allocator->free(allocator, dev);
}
static struct pan_kmod_va_range user_range(const struct pan_kmod_dev *base)
{
    return device(base)->range;
}
static struct pan_kmod_bo *allocate_bo(struct pan_kmod_dev *base, struct pan_kmod_vm *exclusive,
                                      uint64_t bytes, uint32_t flags)
{
    struct kumo_device *dev = device(base);
    if (!ready(dev) || (exclusive && dev->vm != exclusive)) return NULL;
    struct kumo_mesa_bo *bo = pan_kmod_dev_alloc(base, sizeof(*bo));
    if (!bo) return NULL;
    if (kumo_mesa_bo_alloc(&dev->api, bytes, flags, bo) != KUMO_GPU_OK) {
        pan_kmod_dev_free(base, bo);
        return NULL;
    }
    pan_kmod_bo_init(&bo->base, base, exclusive, bo->base.size, flags, bo->base.handle);
    bo->exclusive_vm = exclusive ? exclusive->handle : 0;
    bo->next = dev->bos; dev->bos = bo;
    return &bo->base;
}
static void free_bo(struct pan_kmod_bo *base)
{
    struct kumo_device *dev = device(base->dev);
    struct kumo_mesa_bo *bo = (struct kumo_mesa_bo *)base;
    struct kumo_mesa_bo **link = &dev->bos;
    while (*link && *link != bo) link = &(*link)->next;
    if (!*link) return;
    *link = bo->next;
    pan_kmod_bo_cleanup(base);
    /* A void Mesa destructor cannot preserve a retryable BO for its caller. On failure
     * close the session and expose loss, retaining server DMA until reset. — KESTREL */
    if (kumo_mesa_bo_free(&dev->api, bo) != KUMO_GPU_OK) poison(dev);
    pan_kmod_dev_free(&dev->base, bo);
}
static struct pan_kmod_bo *refuse_import(struct pan_kmod_dev *dev, uint32_t handle, uint64_t bytes)
{
    (void)dev; (void)handle; (void)bytes;
    return NULL;
}
static off_t no_mmap_offset(struct pan_kmod_bo *bo) { (void)bo; return -1; }
/* GL1-d: Panthor's rule over the BO's points: a reader waits for the last write, a writer for
 * every access. A BO with no point was never in a submission that attached one; a native
 * caller outside Gallium's batches (G7p's proofs) owns its own completion. On a lost device
 * nothing completes, because the BO's CPU aliases are already gone. — TIMBERDOODLE */
static bool wait_bo(struct pan_kmod_bo *base, int64_t timeout, bool read_only)
{
    const struct kumo_mesa_bo *bo = (const struct kumo_mesa_bo *)base;
    const uint64_t point = read_only ? bo->write_point : MAX2(bo->write_point, bo->read_point);
    if (!point) return true;
    struct kumo_device *dev = device(base->dev);
    return dev && dev->vm && pan_kmod_kumo_vm_wait(dev->vm, point, timeout) == 1;
}
static int flush_nc(struct pan_kmod_dev *base)
{
    if (!ready(device(base))) return -1;
    /* All supported CPU maps are Normal-NC: no cache lines need cleaning. Order writes;
     * submission's server-side barrier remains mandatory before publication. — KESTREL */
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    return 0;
}
void *pan_kmod_kumo_bo_mmap(struct pan_kmod_bo *base, int prot, int flags, void *host_addr)
{
    struct kumo_device *dev = device(base ? base->dev : NULL);
    void *address = NULL;
    if (!ready(dev) || kumo_mesa_bo_mmap(&dev->api, (struct kumo_mesa_bo *)base,
                                         prot, flags, host_addr, &address) != KUMO_GPU_OK)
        return (void *)-1;
    return address;
}
int pan_kmod_kumo_bo_munmap(struct pan_kmod_bo *base)
{
    struct kumo_device *dev = device(base ? base->dev : NULL);
    return dev && kumo_mesa_bo_munmap(&dev->api, (struct kumo_mesa_bo *)base) == KUMO_GPU_OK ? 0 : -1;
}
static struct pan_kmod_vm *create_vm(struct pan_kmod_dev *base, uint32_t flags,
                                    uint64_t start, uint64_t bytes)
{
    struct kumo_device *dev = device(base);
    /* Gallium asks for AUTO_VA | TRACK_ACTIVITY. GL1-c's initialization explicitly waits for
     * its native completion before releasing BOs. — KESTREL */
    /* GL1-d tracks activity: the timeline, each BO's points, bo_wait and deferred unmaps.
     * — TIMBERDOODLE */
    if (!ready(dev) || dev->vm || (flags & ~KUMO_VM_FLAGS) || !bytes || start % PAN_PGSIZE_4K || bytes % PAN_PGSIZE_4K ||
        start < dev->range.start || start > UINT64_MAX - bytes ||
        start + bytes > dev->range.start + dev->range.size) return NULL;
    struct kumo_vm *storage = pan_kmod_dev_alloc(base, sizeof(*storage));
    struct pan_kmod_vm *vm = storage ? &storage->base : NULL;
    if (!vm) return NULL;
    uint32_t id = 0;
    if (dev->api.vm_create(dev->api.context, start, bytes, &id) != KUMO_GPU_OK) {
        pan_kmod_dev_free(base, vm); return NULL;
    }
    pan_kmod_vm_init(vm, base, id, flags);
    storage->start = start; storage->end = start + bytes;
    if (flags & PAN_KMOD_VM_FLAG_AUTO_VA) {
        util_vma_heap_init(&storage->heap, start, bytes);
        storage->heap.alloc_high = false;
    }
    dev->vm = vm;
    return vm;
}
static void destroy_vm(struct pan_kmod_vm *vm)
{
    struct kumo_device *dev = device(vm->dev);
    if (!dev || dev->vm != vm) return;
    pan_kmod_kumo_sync_destroy(vm->dev, ((struct kumo_vm *)vm)->timeline);
    if (dev->api.vm_destroy(dev->api.context, vm->handle) != KUMO_GPU_OK) poison(dev);
    for (struct kumo_mesa_bo *bo = dev->bos; bo; bo = bo->next)
        if (bo->base.exclusive_vm == vm) bo->base.exclusive_vm = NULL;
    if (vm->flags & PAN_KMOD_VM_FLAG_AUTO_VA)
        util_vma_heap_finish(&((struct kumo_vm *)vm)->heap);
    pan_kmod_vm_cleanup(vm);
    dev->vm = NULL;
    pan_kmod_dev_free(&dev->base, vm);
}
static enum pan_kmod_vm_state vm_state(struct pan_kmod_vm *vm)
{
    struct kumo_device *dev = device(vm->dev);
    return ready(dev) && dev->vm == vm ? PAN_KMOD_VM_USABLE : PAN_KMOD_VM_FAULTY;
}
static int bind_vm(struct pan_kmod_vm *vm, enum pan_kmod_vm_op_mode mode,
                   struct pan_kmod_vm_op *ops, uint32_t count)
{
    struct kumo_device *dev = device(vm->dev);
    const bool deferred = mode == PAN_KMOD_VM_OP_MODE_DEFER_TO_NEXT_IDLE_POINT;
    if (!ready(dev) || dev->vm != vm || (mode != PAN_KMOD_VM_OP_MODE_IMMEDIATE && !deferred) ||
        (deferred && !(vm->flags & PAN_KMOD_VM_FLAG_TRACK_ACTIVITY)) || (count && !ops)) return -1;
    struct kumo_vm *kvm = (struct kumo_vm *)vm;
    const bool auto_va = vm->flags & PAN_KMOD_VM_FLAG_AUTO_VA;
    /* Validate every row's shape and owner before the first request. If a later runtime
     * failure leaves a successful prefix, retire rather than pretend batch atomicity. — KESTREL */
    for (uint32_t i = 0; i < count; ++i) {
        struct pan_kmod_vm_op *op = &ops[i];
        /* GL1-b: in an AUTO_VA VM every map asks for an address and gets one below; a caller
         * that names its own is refused, as pan_kmod's contract requires. — TIMBERDOODLE */
        const bool asks = op->type == PAN_KMOD_VM_OP_TYPE_MAP && op->va.start == PAN_KMOD_VM_MAP_AUTO_VA;
        if (op->type == PAN_KMOD_VM_OP_TYPE_MAP && asks != auto_va) return -1;
        if (op->flags || op->signal.count || op->wait.count || !op->va.size ||
            op->va.size % PAN_PGSIZE_4K) return -1;
        if (!asks && (op->va.start % PAN_PGSIZE_4K ||
            op->va.start < kvm->start || op->va.start > UINT64_MAX - op->va.size ||
            op->va.start + op->va.size > kvm->end)) return -1;
        if (op->type == PAN_KMOD_VM_OP_TYPE_MAP) {
            struct kumo_mesa_bo *bo = (struct kumo_mesa_bo *)op->map.bo;
            if (!bo || bo->base.dev != &dev->base || bo->owner != dev->api.context ||
                (bo->exclusive_vm && bo->exclusive_vm != vm->handle) ||
                op->map.bo_offset < 0 || (uint64_t)op->map.bo_offset % PAN_PGSIZE_4K ||
                (uint64_t)op->map.bo_offset > bo->base.size ||
                op->va.size > bo->base.size - (uint64_t)op->map.bo_offset) return -1;
        } else if (op->type != PAN_KMOD_VM_OP_TYPE_UNMAP) return -1;
    }
    /* GL1-d: panfrost_bo_free unmaps with DEFER_TO_NEXT_IDLE_POINT. Panthor queues the
     * operation behind the VM's last point in the kernel; KUMO waits for that point here and
     * then applies it immediately, so nothing the GPU may still use is unmapped. Until GL1-d
     * this mode was refused and Gallium ignored the error, so every freed BO kept its auto-VA
     * range and only the server's buffer destroy dropped the binding. — TIMBERDOODLE */
    if (deferred && pan_kmod_kumo_vm_wait(vm, kvm->point, INT64_MAX) != 1) return -1;
    /* Every address is chosen before the first request, so a full window refuses the whole
     * batch with nothing sent. */
    for (uint32_t i = 0; auto_va && i < count; ++i) {
        struct pan_kmod_vm_op *op = &ops[i];
        if (op->type != PAN_KMOD_VM_OP_TYPE_MAP) continue;
        const uint64_t va = util_vma_heap_alloc(&kvm->heap, op->va.size,
                                                pan_choose_gpu_va_alignment(vm, op->va.size));
        if (!va) {
            for (uint32_t j = 0; j < i; ++j) {
                if (ops[j].type != PAN_KMOD_VM_OP_TYPE_MAP) continue;
                util_vma_heap_free(&kvm->heap, ops[j].va.start, ops[j].va.size);
                ops[j].va.start = PAN_KMOD_VM_MAP_AUTO_VA;
            }
            return -1;
        }
        op->va.start = va;
    }
    for (uint32_t i = 0; i < count; ++i) {
        struct pan_kmod_vm_op *op = &ops[i];
        uint32_t status;
        if (op->type == PAN_KMOD_VM_OP_TYPE_MAP) {
            const struct kumo_gpu_vm_binding request = {vm->handle, op->map.bo->handle,
                op->va.start, (uint64_t)op->map.bo_offset, op->va.size};
            status = dev->api.vm_bind(dev->api.context, &request);
        } else {
            status = dev->api.vm_unbind(dev->api.context, vm->handle, op->va.start, op->va.size);
        }
        if (status != KUMO_GPU_OK) {
            /* Rows not yet bound give their addresses back; a bound prefix poisons below. */
            for (uint32_t j = i; auto_va && j < count; ++j) {
                if (ops[j].type != PAN_KMOD_VM_OP_TYPE_MAP) continue;
                util_vma_heap_free(&kvm->heap, ops[j].va.start, ops[j].va.size);
                ops[j].va.start = PAN_KMOD_VM_MAP_AUTO_VA;
            }
            if (i || dev->api.device_status(dev->api.context) != KUMO_GPU_OK) poison(dev);
            return -1;
        }
        /* An unmapped range is free again for the next map. */
        if (auto_va && op->type == PAN_KMOD_VM_OP_TYPE_UNMAP)
            util_vma_heap_free(&kvm->heap, op->va.start, op->va.size);
    }
    return 0;
}
static uint64_t no_timestamp(const struct pan_kmod_dev *dev) { (void)dev; return 0; }
static struct pan_kmod_perf_session *no_perf(struct pan_kmod_dev *dev) { (void)dev; return NULL; }
static int no_perf_enable(struct pan_kmod_perf_session *s, const struct pan_kmod_perf_config *c)
{ (void)s; (void)c; return -1; }
static int no_perf_disable(struct pan_kmod_perf_session *s) { (void)s; return -1; }
static void no_perf_op(struct pan_kmod_perf_session *s) { (void)s; }
static void no_perf_dump(struct pan_kmod_perf_session *s, struct mali_perf_dump_info *i) { (void)s; (void)i; }

const struct pan_kmod_ops kumo_kmod_ops = {
    .dev_create = refuse_fd, .dev_destroy = destroy_device, .dev_query_user_va_range = user_range,
    .bo_alloc = allocate_bo, .bo_free = free_bo, .bo_import = refuse_import,
    .bo_get_mmap_offset = no_mmap_offset, .flush_bo_map_syncs = flush_nc, .bo_wait = wait_bo,
    .vm_create = create_vm, .vm_destroy = destroy_vm, .vm_bind = bind_vm, .vm_query_state = vm_state,
    .query_timestamp = no_timestamp, .perf_create = no_perf, .perf_enable = no_perf_enable,
    .perf_disable = no_perf_disable, .perf_dump = no_perf_dump, .perf_destroy = no_perf_op,
};
