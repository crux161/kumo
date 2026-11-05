/* SPDX-License-Identifier: MIT */
/*j791*/
/* VK6 (PLAN/015 §8.49, ratified 2026-11-04): Vulkan recovery through panvk. One image, run on
 * the churn lane so Sora regrants a fresh, reset session after each exit:
 * - the boot session (the one holding drv-fb's native producer) loses its device on purpose.
 *   VK6-b (fatal): VK6's pinned shader stores through a buffer device address that points at
 *   the lowest page of the session's VA window that no live binding covers, so Mali's MMU
 *   faults. VK6-c (timeout): a
 *   command buffer waits on a VkEvent nobody sets. Either way drv-gpu contains the session,
 *   and the client must see VK_ERROR_DEVICE_LOST. vkDeviceWaitIdle must report it too. A
 *   mapped witness buffer must stay readable and writable (D1a, Vulkan's Lost Device rule),
 *   and every destroy and free must return;
 * - every regranted session creates a new instance and VkDevice and runs VK3-b's compute
 *   exactly, then tears down clean (D2a).
 * The native API is observed in place, VM bindings are tracked to prove the fault page
 * unbound, and three libdrm entries are wrapped at link time. — TIMBERDOODLE */
#include "kumo_kmod.h"
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <vulkan/vulkan_core.h>
#include "vk-compute.spv.h"
#include "vk-lost.spv.h"
#include "vk-memory-policy.h"

PFN_vkVoidFunction vk_icdGetInstanceProcAddr(VkInstance instance, const char *name);
bool panvk_kumo_offer_device(struct pan_kmod_dev *kmod);
enum kumo_vk_lost_role { KUMO_VK_LOSE_FATAL = 1, KUMO_VK_LOSE_TIMEOUT = 2, KUMO_VK_RECOVER = 3 };
unsigned kumo_vk_lost_cases(const struct kumo_gpu_api *api, void (*say)(const char *), enum kumo_vk_lost_role role);

#define CHECK(test, code) do { if (!(test)) { failed = (code); goto done; } } while (0)

/* kumo-gpu's session VA window (lib/kumo-gpu VA_START..VA_END). */
#define VA_START UINT64_C(0x20000000)
#define VA_END UINT64_C(0x30000000)
#define PAGE UINT64_C(4096)
#define FAULT_VALUE 0xdeadfa17u
#define WITNESS_WORDS 1024u
#define WITNESS(i) (0x5a5a0000u | (uint32_t)(i))
/* VK3-b's dispatch, for the recovery. */
#define LOCAL_SIZE 64u
#define GROUPS 16u
#define COUNT 1000u
#define GUARDS 64u
#define WORDS (GROUPS * LOCAL_SIZE + GUARDS)
#define K 0x6b770000u
#define POISON 0xdeadbeefu
/* A lost device answers within drv-gpu's containment (about a second on metal, G7p-e); the
 * bound only stops a missed loss hanging the proof. */
#define LOSS_BOUND_NS UINT64_C(30000000000)

static void report(void (*say)(const char *), const char *format, ...)
{
    char line[256];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof line, format, args);
    va_end(args);
    say(line);
}

static const char *result_name(VkResult result)
{
    switch (result) {
    case VK_SUCCESS: return "VK_SUCCESS";
    case VK_TIMEOUT: return "VK_TIMEOUT";
    case VK_NOT_READY: return "VK_NOT_READY";
    case VK_ERROR_DEVICE_LOST: return "VK_ERROR_DEVICE_LOST";
    case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
    case VK_ERROR_OUT_OF_DEVICE_MEMORY: return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
    default: return "another VkResult";
    }
}

/* Every native request and buffer passes through here on its way to the session owner. */
static struct kumo_gpu_api tracked;
static uint32_t (*real_native)(void *, const struct kumo_gpu_native_request *, struct kumo_gpu_native_reply *);
static uint32_t (*real_heap_create)(void *, const struct kumo_gpu_heap_request *, struct kumo_gpu_heap_info *);
static uint32_t (*real_heap_destroy)(void *, uint32_t, uint64_t *);
static uint32_t (*real_buffer_create)(void *, uint64_t, uint32_t, struct kumo_gpu_buffer_info *);
static uint32_t (*real_buffer_destroy)(void *, uint32_t);
static uint32_t (*real_vm_bind)(void *, const struct kumo_gpu_vm_binding *);
static uint32_t (*real_vm_unbind)(void *, uint32_t, uint64_t, uint64_t);
#define MAX_RANGES 256u
static struct {
    uint64_t start[MAX_RANGES], end[MAX_RANGES];
    unsigned count;
    bool overflow;
} bound;
#define MAX_PREPARES 8u
static struct {
    unsigned groups, group_queues, prepares, streams, submitted, blocked;
    unsigned heaps, heaps_released, refused, buffers, buffers_released;
    unsigned stream_queue[KUMO_NATIVE_MAX_STREAMS];
    /* Per PREPARE: its streams' subqueue mask, in order. */
    unsigned prepare_mask[MAX_PREPARES];
    long live; /* native objects held: every returned ID, less each DROP and consumed wake */
    unsigned drm_ioctls, drm_listings, drm_syncobjs;
} seen;

static uint32_t observe_native(void *context, const struct kumo_gpu_native_request *request,
                               struct kumo_gpu_native_reply *out)
{
    const uint32_t status = real_native(context, request, out);
    if (status != KUMO_GPU_OK) {
        ++seen.refused;
        return status;
    }
    switch (request->operation) {
    case KUMO_NATIVE_CREATE_BINARY:
    case KUMO_NATIVE_CREATE_TIMELINE:
        ++seen.live;
        break;
    case KUMO_NATIVE_DROP:
        --seen.live;
        break;
    case KUMO_NATIVE_SNAPSHOT:
    case KUMO_NATIVE_ARM:
        if (out->state == KUMO_NATIVE_AVAILABLE && out->id)
            ++seen.live;
        break;
    case KUMO_NATIVE_WAIT:
        --seen.live; /* WAIT consumes its wake on every outcome. */
        break;
    case KUMO_NATIVE_GROUP_CREATE:
        ++seen.groups;
        seen.group_queues += request->flags == KUMO_NATIVE_GROUP_QUEUES;
        ++seen.live;
        break;
    case KUMO_NATIVE_PREPARE: {
        unsigned mask = 0;
        seen.streams += request->stream_count;
        for (uint32_t i = 0; i < request->stream_count && request->streams; ++i)
            if (request->streams[i].queue < KUMO_NATIVE_MAX_STREAMS) {
                ++seen.stream_queue[request->streams[i].queue];
                mask |= 1u << request->streams[i].queue;
            }
        if (seen.prepares < MAX_PREPARES)
            seen.prepare_mask[seen.prepares] = mask;
        ++seen.prepares;
        ++seen.live;
        break;
    }
    case KUMO_NATIVE_STEP:
        seen.submitted += out->state == KUMO_NATIVE_SUBMITTED;
        seen.blocked += out->state == KUMO_NATIVE_AVAILABLE;
        ++seen.live;
        break;
    default:
        break;
    }
    return status;
}

static uint32_t observe_heap_create(void *context, const struct kumo_gpu_heap_request *request,
                                    struct kumo_gpu_heap_info *out)
{
    const uint32_t status = real_heap_create(context, request, out);
    seen.heaps += status == KUMO_GPU_OK;
    return status;
}

static uint32_t observe_heap_destroy(void *context, uint32_t heap, uint64_t *bytes)
{
    const uint32_t status = real_heap_destroy(context, heap, bytes);
    seen.heaps_released += status == KUMO_GPU_OK;
    return status;
}

static uint32_t observe_buffer_create(void *context, uint64_t bytes, uint32_t flags,
                                      struct kumo_gpu_buffer_info *out)
{
    const uint32_t status = real_buffer_create(context, bytes, flags, out);
    seen.buffers += status == KUMO_GPU_OK;
    return status;
}

static uint32_t observe_buffer_destroy(void *context, uint32_t id)
{
    const uint32_t status = real_buffer_destroy(context, id);
    seen.buffers_released += status == KUMO_GPU_OK;
    return status;
}

/* The link wraps these three; KUMO's panvk must never reach them. Each still refuses. */
int __real_drmIoctl(int fd, unsigned long request, void *arg);
int __wrap_drmIoctl(int fd, unsigned long request, void *arg);
int __wrap_drmIoctl(int fd, unsigned long request, void *arg)
{
    ++seen.drm_ioctls;
    return __real_drmIoctl(fd, request, arg);
}
struct _drmDevice;
int __real_drmGetDevices2(uint32_t flags, struct _drmDevice *devices[], int max_devices);
int __wrap_drmGetDevices2(uint32_t flags, struct _drmDevice *devices[], int max_devices);
int __wrap_drmGetDevices2(uint32_t flags, struct _drmDevice *devices[], int max_devices)
{
    ++seen.drm_listings;
    return __real_drmGetDevices2(flags, devices, max_devices);
}
int __real_drmSyncobjCreate(int fd, uint32_t flags, uint32_t *handle);
int __wrap_drmSyncobjCreate(int fd, uint32_t flags, uint32_t *handle);
int __wrap_drmSyncobjCreate(int fd, uint32_t flags, uint32_t *handle)
{
    ++seen.drm_syncobjs;
    return __real_drmSyncobjCreate(fd, flags, handle);
}

static uint32_t observe_vm_bind(void *context, const struct kumo_gpu_vm_binding *binding)
{
    const uint32_t status = real_vm_bind(context, binding);
    if (status == KUMO_GPU_OK) {
        if (bound.count < MAX_RANGES) {
            bound.start[bound.count] = binding->va;
            bound.end[bound.count] = binding->va + binding->bytes;
            ++bound.count;
        } else {
            bound.overflow = true;
        }
    }
    return status;
}

static uint32_t observe_vm_unbind(void *context, uint32_t vm, uint64_t va, uint64_t bytes)
{
    const uint32_t status = real_vm_unbind(context, vm, va, bytes);
    if (status != KUMO_GPU_OK)
        return status;
    for (unsigned i = 0; i < bound.count;) {
        if (bound.start[i] >= va && bound.end[i] <= va + bytes) {
            --bound.count;
            bound.start[i] = bound.start[bound.count];
            bound.end[i] = bound.end[bound.count];
        } else {
            ++i;
        }
    }
    return status;
}

static bool page_unbound(uint64_t va)
{
    if (bound.overflow)
        return false;
    for (unsigned i = 0; i < bound.count; ++i)
        if (va < bound.end[i] && va + PAGE > bound.start[i])
            return false;
    return true;
}

/* The lowest page of the window no live binding covers, or 0. Metal: panvk fills the window
 * from the top (11 bindings in 0x2f40_0000-0x3000_0000 at dispatch), and recording the command
 * buffer allocates more there, into any gap a top-down choice would pick (third VK6-b boot:
 * 0x2ffd6000 was free when chosen and bound by the submit). The bottom stays free. */
static uint64_t unbound_page(void)
{
    for (uint64_t va = VA_START; va < VA_END; va += PAGE)
        if (page_unbound(va))
            return va;
    return 0;
}

#define INSTANCE_FN(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(instance, #name)
#define DEVICE_FN(name) PFN_##name name = (PFN_##name)get_device_proc(device, #name)

static uint32_t host_type(const VkPhysicalDeviceMemoryProperties *memory, uint32_t bits)
{
    const VkMemoryPropertyFlags wanted = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    for (uint32_t i = 0; i < memory->memoryTypeCount; ++i)
        if ((bits & (1u << i)) && (memory->memoryTypes[i].propertyFlags & wanted) == wanted)
            return i;
    return memory->memoryTypeCount;
}

unsigned kumo_vk_lost_cases(const struct kumo_gpu_api *api, void (*say)(const char *), enum kumo_vk_lost_role role)
{
    const bool lose = role != KUMO_VK_RECOVER;
    unsigned failed = 0;
    VkInstance instance = VK_NULL_HANDLE;
    VkDevice device = VK_NULL_HANDLE;
    PFN_vkDestroyInstance destroy_instance = NULL;
    PFN_vkDestroyDevice destroy_device = NULL;
    PFN_vkGetDeviceProcAddr get_device_proc = NULL;
    VkShaderModule module = VK_NULL_HANDLE;
    VkDescriptorSetLayout set_layout = VK_NULL_HANDLE;
    VkPipelineLayout layout = VK_NULL_HANDLE;
    VkPipeline pipeline = VK_NULL_HANDLE;
    VkBuffer buffer = VK_NULL_HANDLE;
    VkDeviceMemory backing = VK_NULL_HANDLE;
    VkDescriptorPool descriptor_pool = VK_NULL_HANDLE;
    VkCommandPool command_pool = VK_NULL_HANDLE;
    VkFence fence = VK_NULL_HANDLE;
    VkEvent event = VK_NULL_HANDLE;
    void *mapped = NULL;
    VkResult submitted = VK_SUCCESS, waited = VK_SUCCESS, idle = VK_SUCCESS;

    memset(&seen, 0, sizeof seen);
    memset(&bound, 0, sizeof bound);
    tracked = *api;
    real_native = api->native;
    real_heap_create = api->heap_create;
    real_heap_destroy = api->heap_destroy;
    real_buffer_create = api->buffer_create;
    real_buffer_destroy = api->buffer_destroy;
    real_vm_bind = api->vm_bind;
    real_vm_unbind = api->vm_unbind;
    tracked.native = observe_native;
    tracked.heap_create = observe_heap_create;
    tracked.heap_destroy = observe_heap_destroy;
    tracked.buffer_create = observe_buffer_create;
    tracked.buffer_destroy = observe_buffer_destroy;
    tracked.vm_bind = observe_vm_bind;
    tracked.vm_unbind = observe_vm_unbind;

    /* The pinned modules (vk-spirv.py --verify checks their hashes on the host). */
    if (sizeof kumo_vk_lost_spv != 227 * sizeof(uint32_t) || kumo_vk_lost_spv[0] != 0x07230203u ||
        sizeof kumo_vk_compute_spv != 308 * sizeof(uint32_t))
        return 1;

    struct pan_kmod_dev *kmod = pan_kmod_kumo_dev_create(&tracked, NULL);
    if (!kmod)
        return 2;
    if (!panvk_kumo_offer_device(kmod)) {
        pan_kmod_dev_destroy(kmod);
        return 3;
    }

    PFN_vkCreateInstance create_instance =
        (PFN_vkCreateInstance)vk_icdGetInstanceProcAddr(VK_NULL_HANDLE, "vkCreateInstance");
    CHECK(create_instance, 4);
    const VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = lose ? "kumo-vk6-lose" : "kumo-vk6-recover",
        .apiVersion = VK_API_VERSION_1_2,
    };
    const VkInstanceCreateInfo instance_info = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    CHECK(create_instance(&instance_info, NULL, &instance) == VK_SUCCESS && instance, 5);
    destroy_instance = (PFN_vkDestroyInstance)vk_icdGetInstanceProcAddr(instance, "vkDestroyInstance");
    INSTANCE_FN(vkEnumeratePhysicalDevices);
    INSTANCE_FN(vkGetPhysicalDeviceProperties);
    INSTANCE_FN(vkGetPhysicalDeviceMemoryProperties);
    INSTANCE_FN(vkGetPhysicalDeviceFeatures2);
    INSTANCE_FN(vkCreateDevice);
    INSTANCE_FN(vkGetDeviceProcAddr);
    CHECK(destroy_instance && vkEnumeratePhysicalDevices && vkGetPhysicalDeviceProperties &&
          vkGetPhysicalDeviceMemoryProperties && vkGetPhysicalDeviceFeatures2 && vkCreateDevice &&
          vkGetDeviceProcAddr, 6);
    uint32_t count = 1;
    VkPhysicalDevice physical = VK_NULL_HANDLE;
    CHECK(vkEnumeratePhysicalDevices(instance, &count, &physical) == VK_SUCCESS && count == 1 && physical, 7);
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(physical, &props);
    CHECK(props.vendorID == 0x13b5 && strstr(props.deviceName, "Mali-G610"), 8);
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceMemoryProperties(physical, &memory);
    CHECK(kumo_vk_memory_types_valid(&memory), 9);
    VkPhysicalDeviceVulkan12Features vulkan12 = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES};
    VkPhysicalDeviceFeatures2 features = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &vulkan12};
    vkGetPhysicalDeviceFeatures2(physical, &features);
    CHECK(vulkan12.bufferDeviceAddress, 10);

    const float priority = 1.0f;
    const VkDeviceQueueCreateInfo queue_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = 0,
        .queueCount = 1,
        .pQueuePriorities = &priority,
    };
    VkPhysicalDeviceVulkan12Features enable12 = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
        .bufferDeviceAddress = VK_TRUE,
    };
    const VkDeviceCreateInfo device_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .pNext = lose ? &enable12 : NULL,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &queue_info,
    };
    CHECK(vkCreateDevice(physical, &device_info, NULL, &device) == VK_SUCCESS && device, 11);
    get_device_proc = vkGetDeviceProcAddr;
    destroy_device = (PFN_vkDestroyDevice)get_device_proc(device, "vkDestroyDevice");
    DEVICE_FN(vkGetDeviceQueue);
    DEVICE_FN(vkDeviceWaitIdle);
    DEVICE_FN(vkQueueSubmit);
    DEVICE_FN(vkCreateFence);
    DEVICE_FN(vkWaitForFences);
    DEVICE_FN(vkCreateEvent);
    DEVICE_FN(vkCreateShaderModule);
    DEVICE_FN(vkCreateDescriptorSetLayout);
    DEVICE_FN(vkCreatePipelineLayout);
    DEVICE_FN(vkCreateComputePipelines);
    DEVICE_FN(vkCreateBuffer);
    DEVICE_FN(vkGetBufferMemoryRequirements);
    DEVICE_FN(vkAllocateMemory);
    DEVICE_FN(vkBindBufferMemory);
    DEVICE_FN(vkMapMemory);
    DEVICE_FN(vkCreateDescriptorPool);
    DEVICE_FN(vkAllocateDescriptorSets);
    DEVICE_FN(vkUpdateDescriptorSets);
    DEVICE_FN(vkCreateCommandPool);
    DEVICE_FN(vkAllocateCommandBuffers);
    DEVICE_FN(vkBeginCommandBuffer);
    DEVICE_FN(vkEndCommandBuffer);
    DEVICE_FN(vkCmdBindPipeline);
    DEVICE_FN(vkCmdBindDescriptorSets);
    DEVICE_FN(vkCmdPushConstants);
    DEVICE_FN(vkCmdDispatch);
    DEVICE_FN(vkCmdPipelineBarrier);
    DEVICE_FN(vkCmdWaitEvents);
    CHECK(destroy_device && vkGetDeviceQueue && vkDeviceWaitIdle && vkQueueSubmit && vkCreateFence &&
          vkWaitForFences && vkCreateEvent && vkCreateShaderModule && vkCreateDescriptorSetLayout &&
          vkCreatePipelineLayout && vkCreateComputePipelines && vkCreateBuffer && vkGetBufferMemoryRequirements &&
          vkAllocateMemory && vkBindBufferMemory && vkMapMemory && vkCreateDescriptorPool &&
          vkAllocateDescriptorSets && vkUpdateDescriptorSets && vkCreateCommandPool && vkAllocateCommandBuffers &&
          vkBeginCommandBuffer && vkEndCommandBuffer && vkCmdBindPipeline && vkCmdBindDescriptorSets &&
          vkCmdPushConstants && vkCmdDispatch && vkCmdPipelineBarrier && vkCmdWaitEvents, 12);
    CHECK(seen.groups == 1 && seen.group_queues == 1 && seen.heaps == 1 && seen.prepares == 3 &&
          seen.submitted == 3 && seen.refused == 0, 13);
    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(device, 0, 0, &queue);
    CHECK(queue, 14);
    report(say, "VK6 %s device: %s through the static ICD, one three-queue native group and %u init submissions%s",
           lose ? "losing" : "recovering", props.deviceName, seen.submitted,
           lose ? ", buffer device address enabled" : "");

    /* The buffer: the recovery's storage words, or the losing session's mapped witness. */
    const uint32_t words = lose ? WITNESS_WORDS : WORDS;
    const VkBufferCreateInfo buffer_info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = words * sizeof(uint32_t),
        .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    CHECK(vkCreateBuffer(device, &buffer_info, NULL, &buffer) == VK_SUCCESS, 15);
    VkMemoryRequirements needs;
    vkGetBufferMemoryRequirements(device, buffer, &needs);
    const VkMemoryAllocateInfo allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = needs.size,
        .memoryTypeIndex = host_type(&memory, needs.memoryTypeBits),
    };
    CHECK(allocation.memoryTypeIndex < memory.memoryTypeCount &&
          vkAllocateMemory(device, &allocation, NULL, &backing) == VK_SUCCESS &&
          vkBindBufferMemory(device, buffer, backing, 0) == VK_SUCCESS, 16);
    CHECK(vkMapMemory(device, backing, 0, VK_WHOLE_SIZE, 0, &mapped) == VK_SUCCESS && mapped, 17);
    volatile uint32_t *const host = mapped;
    for (uint32_t i = 0; i < words; ++i)
        host[i] = lose ? WITNESS(i) : POISON;

    /* The pipeline: VK6's store for the losing session, VK3-a's compute for the recovery. */
    const VkShaderModuleCreateInfo module_info = {
        .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
        .codeSize = lose ? sizeof kumo_vk_lost_spv : sizeof kumo_vk_compute_spv,
        .pCode = lose ? kumo_vk_lost_spv : kumo_vk_compute_spv,
    };
    CHECK(vkCreateShaderModule(device, &module_info, NULL, &module) == VK_SUCCESS, 18);
    const VkDescriptorSetLayoutBinding binding = {
        .binding = 0,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .descriptorCount = 1,
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
    };
    const VkDescriptorSetLayoutCreateInfo set_layout_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = lose ? 0 : 1,
        .pBindings = lose ? NULL : &binding,
    };
    CHECK(vkCreateDescriptorSetLayout(device, &set_layout_info, NULL, &set_layout) == VK_SUCCESS, 19);
    const VkPushConstantRange push_range = {
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
        .size = lose ? 3 * sizeof(uint32_t) : 2 * sizeof(uint32_t),
    };
    const VkPipelineLayoutCreateInfo layout_info = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1,
        .pSetLayouts = &set_layout,
        .pushConstantRangeCount = 1,
        .pPushConstantRanges = &push_range,
    };
    CHECK(vkCreatePipelineLayout(device, &layout_info, NULL, &layout) == VK_SUCCESS, 20);
    const VkComputePipelineCreateInfo pipeline_info = {
        .sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
        .stage = {
            .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
            .stage = VK_SHADER_STAGE_COMPUTE_BIT,
            .module = module,
            .pName = "main",
        },
        .layout = layout,
    };
    CHECK(vkCreateComputePipelines(device, VK_NULL_HANDLE, 1, &pipeline_info, NULL, &pipeline) == VK_SUCCESS, 21);
    VkDescriptorSet set = VK_NULL_HANDLE;
    if (!lose) {
        const VkDescriptorPoolSize pool_size = {VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1};
        const VkDescriptorPoolCreateInfo descriptor_pool_info = {
            .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
            .maxSets = 1,
            .poolSizeCount = 1,
            .pPoolSizes = &pool_size,
        };
        CHECK(vkCreateDescriptorPool(device, &descriptor_pool_info, NULL, &descriptor_pool) == VK_SUCCESS, 22);
        const VkDescriptorSetAllocateInfo set_info = {
            .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
            .descriptorPool = descriptor_pool,
            .descriptorSetCount = 1,
            .pSetLayouts = &set_layout,
        };
        CHECK(vkAllocateDescriptorSets(device, &set_info, &set) == VK_SUCCESS && set, 23);
        const VkDescriptorBufferInfo described = {buffer, 0, VK_WHOLE_SIZE};
        const VkWriteDescriptorSet write = {
            .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
            .dstSet = set,
            .dstBinding = 0,
            .descriptorCount = 1,
            .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
            .pBufferInfo = &described,
        };
        vkUpdateDescriptorSets(device, 1, &write, 0, NULL);
    }
    if (role == KUMO_VK_LOSE_TIMEOUT) {
        const VkEventCreateInfo event_info = {.sType = VK_STRUCTURE_TYPE_EVENT_CREATE_INFO};
        CHECK(vkCreateEvent(device, &event_info, NULL, &event) == VK_SUCCESS, 24);
    }

    const VkCommandPoolCreateInfo command_pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = 0,
    };
    CHECK(vkCreateCommandPool(device, &command_pool_info, NULL, &command_pool) == VK_SUCCESS, 25);
    const VkCommandBufferAllocateInfo command_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = command_pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    VkCommandBuffer commands = VK_NULL_HANDLE;
    CHECK(vkAllocateCommandBuffers(device, &command_info, &commands) == VK_SUCCESS && commands, 26);
    const VkCommandBufferBeginInfo begin = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    /* Every allocation of this session exists by now; the pool's own chunks included. */
    uint64_t fault_va = lose ? unbound_page() : 0;
    const char *where = "inside the session window";
    uint64_t lowest = UINT64_MAX, highest = 0, covered = 0;
    for (unsigned i = 0; i < bound.count; ++i) {
        lowest = bound.start[i] < lowest ? bound.start[i] : lowest;
        highest = bound.end[i] > highest ? bound.end[i] : highest;
        covered += bound.end[i] - bound.start[i];
    }
    if (lose && !fault_va && !bound.overflow && page_unbound(VA_START - PAGE)) {
        /* No free page inside the window: the page just below it, which nothing of the
         * session binds (drv-gpu's own AS1 work area sits at 0x1000_0000). */
        fault_va = VA_START - PAGE;
        where = "just below the session window, which bindings fill";
    }
    if (lose)
        report(say, "VK6 bindings: %u live%s, 0x%llx-0x%llx, %llu bytes bound in all; fault page 0x%016llx %s",
               bound.count, bound.overflow ? " (tracker overflowed)" : "",
               (unsigned long long)(bound.count ? lowest : 0), (unsigned long long)highest,
               (unsigned long long)covered, (unsigned long long)fault_va, where);
    CHECK(vkBeginCommandBuffer(commands, &begin) == VK_SUCCESS, 27);
    vkCmdBindPipeline(commands, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline);
    if (role == KUMO_VK_LOSE_TIMEOUT) {
        /* A wait on an event nobody sets: the compute subqueue stalls before the dispatch. */
        vkCmdWaitEvents(commands, 1, &event, VK_PIPELINE_STAGE_HOST_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0, NULL, 0,
                        NULL, 0, NULL);
    }
    if (lose) {
        const uint32_t push[3] = {(uint32_t)fault_va, (uint32_t)(fault_va >> 32), FAULT_VALUE};
        vkCmdPushConstants(commands, layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof push, push);
        vkCmdDispatch(commands, 1, 1, 1);
    } else {
        const uint32_t push[2] = {K, COUNT};
        vkCmdBindDescriptorSets(commands, VK_PIPELINE_BIND_POINT_COMPUTE, layout, 0, 1, &set, 0, NULL);
        vkCmdPushConstants(commands, layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof push, push);
        vkCmdDispatch(commands, GROUPS, 1, 1);
    }
    const VkMemoryBarrier to_host = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
    };
    vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &to_host, 0,
                         NULL, 0, NULL);
    CHECK(vkEndCommandBuffer(commands) == VK_SUCCESS, 28);
    const VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    CHECK(vkCreateFence(device, &fence_info, NULL, &fence) == VK_SUCCESS, 29);
    const VkSubmitInfo submit = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .commandBufferCount = 1,
        .pCommandBuffers = &commands,
    };

    if (lose) {
        /* The fault page must be unbound when the store runs; nothing binds between here and
         * the dispatch's execution (one thread, one submission). */
        CHECK(fault_va && page_unbound(fault_va), 30);
        report(say, "VK6 %s: one dispatch %s at VA 0x%016llx (unbound among %u live VM bindings)",
               role == KUMO_VK_LOSE_FATAL ? "fatal" : "timeout",
               role == KUMO_VK_LOSE_FATAL ? "stores 0xdeadfa17 through a buffer device address"
                                          : "waits on a VkEvent nobody sets, then would store",
               (unsigned long long)fault_va, bound.count);
        submitted = vkQueueSubmit(queue, 1, &submit, fence);
        waited = submitted == VK_SUCCESS ? vkWaitForFences(device, 1, &fence, VK_TRUE, LOSS_BOUND_NS) : submitted;
        idle = vkDeviceWaitIdle(device);
        report(say, "VK6 loss: vkQueueSubmit %s, vkWaitForFences %s, vkDeviceWaitIdle %s; native status %s, %u native "
               "refusals", result_name(submitted), result_name(waited), result_name(idle),
               api->device_status(api->context) == KUMO_GPU_OK ? "OK" : "lost", seen.refused);
        CHECK((submitted == VK_ERROR_DEVICE_LOST || waited == VK_ERROR_DEVICE_LOST) &&
              idle == VK_ERROR_DEVICE_LOST && api->device_status(api->context) != KUMO_GPU_OK, 31);

        /* D1a: host access to the mapped witness is still valid after the loss. */
        unsigned intact = 0;
        for (uint32_t i = 0; i < WITNESS_WORDS; ++i)
            intact += host[i] == WITNESS(i);
        for (uint32_t i = 0; i < WITNESS_WORDS; ++i)
            host[i] = ~WITNESS(i);
        unsigned rewritten = 0;
        for (uint32_t i = 0; i < WITNESS_WORDS; ++i)
            rewritten += host[i] == ~WITNESS(i);
        report(say, "VK6 host access: the mapped witness stayed readable and writable after the loss (%u of %u words "
               "intact, %u of %u rewritten words read back)", intact, WITNESS_WORDS, rewritten, WITNESS_WORDS);
        CHECK(rewritten == WITNESS_WORDS, 32);
    } else {
        CHECK(vkQueueSubmit(queue, 1, &submit, fence) == VK_SUCCESS, 33);
        CHECK(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX) == VK_SUCCESS, 34);
        CHECK(seen.refused == 0 && api->device_status(api->context) == KUMO_GPU_OK, 35);
        unsigned wrong = 0, poisoned = 0;
        for (uint32_t i = 0; i < COUNT; ++i)
            wrong += host[i] != i * 3u + K;
        for (uint32_t i = COUNT; i < WORDS; ++i)
            poisoned += host[i] != POISON;
        report(say, "VK6 recovery compute: VK3-b's dispatch on the new device, %u of %u words differ from i*3+k, "
               "%u of %u words past count changed", wrong, COUNT, poisoned, WORDS - COUNT);
        CHECK(!wrong && !poisoned, 36);
    }

done:
    if (device && destroy_device && get_device_proc) {
        PFN_vkDeviceWaitIdle wait_idle = (PFN_vkDeviceWaitIdle)get_device_proc(device, "vkDeviceWaitIdle");
        const VkResult last = wait_idle ? wait_idle(device) : VK_ERROR_UNKNOWN;
        if (!failed && last != (lose ? VK_ERROR_DEVICE_LOST : VK_SUCCESS))
            failed = 37;
#define RELEASE(fn, object) do { if (object) { PFN_##fn release = (PFN_##fn)get_device_proc(device, #fn); \
                                                 release(device, object, NULL); } } while (0)
        RELEASE(vkDestroyFence, fence);
        RELEASE(vkDestroyEvent, event);
        RELEASE(vkDestroyCommandPool, command_pool);
        RELEASE(vkDestroyDescriptorPool, descriptor_pool);
        if (mapped)
            ((PFN_vkUnmapMemory)get_device_proc(device, "vkUnmapMemory"))(device, backing);
        RELEASE(vkFreeMemory, backing);
        RELEASE(vkDestroyBuffer, buffer);
        RELEASE(vkDestroyPipeline, pipeline);
        RELEASE(vkDestroyPipelineLayout, layout);
        RELEASE(vkDestroyDescriptorSetLayout, set_layout);
        RELEASE(vkDestroyShaderModule, module);
#undef RELEASE
        destroy_device(device, NULL);
    }
    device = VK_NULL_HANDLE;
    if (instance && destroy_instance)
        destroy_instance(instance, NULL);
    instance = VK_NULL_HANDLE;
    if (failed)
        return failed;
    /* After a loss, the device close releases every BO without a live alias at once (VK6-a),
     * so Mesa's later destroys of those find nothing: only the recovery accounts every buffer
     * through destroy. kumo_metal_proof still refuses any record left after the scope. */
    if (!lose && seen.buffers_released != seen.buffers)
        return 38;
    if (seen.drm_ioctls || seen.drm_listings || seen.drm_syncobjs)
        return 39;
    if (lose) {
        /* The heap went with the session's reset; its destroy cannot reach a closed session. */
        report(say, "VK6 teardown after loss: every destroy and free returned; of %u native buffers, %u released by "
               "destroy and the rest by the device close; heap destroys acknowledged %u; no Panthor ioctl, DRM "
               "listing or syncobj", seen.buffers, seen.buffers_released, seen.heaps_released);
    } else {
        if (seen.heaps_released != 1 || seen.live != 1)
            return 40;
        report(say, "VK6 recovery teardown: every object, the device and the instance destroyed; %u of %u native "
               "buffers and the heap released, only the session's group left", seen.buffers_released, seen.buffers);
    }
    return 0;
}

#ifdef KUMO_VK_LOST_MAIN
/* KUMO_VK_LOST_MAIN is the losing role: 1 fatal (VK6-b), 2 timeout (VK6-c). The boot session
 * (with drv-fb's native producer) loses; every regranted session recovers. */
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
extern unsigned kumo_metal_view_proof(uint32_t proof);
extern unsigned kumo_metal_churn_take(void);
extern unsigned kumo_metal_churn_native(void);
extern void kumo_metal_churn_release(void);
static enum kumo_vk_lost_role role;
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_vk_lost_cases(api, kumo_metal_log, role);
}

int main(void)
{
    const char *const name = KUMO_VK_LOST_MAIN == 1 ? "VK6-b" : "VK6-c";
    report(kumo_metal_log, "%s metal recovery begin: lose the device in the boot session, recover in every "
           "regranted one", name);
    unsigned failed = kumo_metal_view_proof(2);
    if (!failed)
        failed = kumo_metal_churn_take();
    if (failed) {
        report(kumo_metal_log, "%s metal recovery FAILED at lane step %u", name, failed);
        return 1;
    }
    role = kumo_metal_churn_native() ? (enum kumo_vk_lost_role)KUMO_VK_LOST_MAIN : KUMO_VK_RECOVER;
    /* No presentation: the view and producer go back before the device is made. */
    kumo_metal_churn_release();
    failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "%s metal recovery FAILED at case %u (%s session)", name, failed,
               role == KUMO_VK_RECOVER ? "recovering" : "losing");
        return 1;
    }
    if (role == KUMO_VK_RECOVER)
        report(kumo_metal_log, "%s metal recovery green: a new VkDevice on a regranted session computed exactly and "
               "tore down clean", name);
    else
        report(kumo_metal_log, "%s metal loss green: the %s surfaced as VK_ERROR_DEVICE_LOST, host access stayed "
               "valid and every destroy returned", name, KUMO_VK_LOST_MAIN == 1 ? "fault" : "stall");
    return 0;
}
#endif
