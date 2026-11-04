/* SPDX-License-Identifier: MIT */
/*j782*/
/* VK3-b (PLAN/015 §8.2): the first Vulkan output. The static-ICD client of VK2-e brings up
 * panvk on the session's kmod device, then runs VK3-a's pinned compute shader:
 * - the 308-word SPIR-V module becomes a compute pipeline, compiled by panvk on KUMO with no
 *   pipeline cache and no native submission;
 * - a host-visible, coherent storage buffer of 1088 words, poisoned from the CPU before each pass;
 * - two command buffers, each binding the pipeline and the buffer, pushing k and count,
 *   dispatching 16 workgroups of 64 and making the shader writes visible to the host;
 * - one fenced vkQueueSubmit per pass, counted as native submissions per subqueue. The first
 *   carries panvk's one-time compute REQ_RESOURCE stream ahead of the command buffer;
 * - after each pass, the CPU checks words[i] = i * 3 + k for every i below count, and that the
 *   24 invocations at or above count and the 64 guard words past the dispatch left the poison;
 * - teardown releases every native buffer and object but the session's group.
 * The native API is observed in place, and three libdrm entries are wrapped at link time, so
 * any Panthor ioctl, DRM device listing or syncobj creation fails the proof. */
#include "kumo_kmod.h"
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <vulkan/vulkan_core.h>
#include "vk-compute.spv.h"
#include "vk-memory-policy.h"

PFN_vkVoidFunction vk_icdGetInstanceProcAddr(VkInstance instance, const char *name);
bool panvk_kumo_offer_device(struct pan_kmod_dev *kmod);
unsigned kumo_vk_compute_cases(const struct kumo_gpu_api *api, void (*say)(const char *));

#define CHECK(test, code) do { if (!(test)) { failed = (code); goto done; } } while (0)

/* vk-compute.comp's local size, the dispatch and the check's shape. */
#define LOCAL_SIZE 64u
#define GROUPS 16u
#define COUNT 1000u
#define GUARDS 64u
#define WORDS (GROUPS * LOCAL_SIZE + GUARDS)
#define K 0x6b750000u
#define K2 0x6b760000u
#define PASSES 2u
#define POISON 0xdeadbeefu

static void report(void (*say)(const char *), const char *format, ...)
{
    char line[256];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof line, format, args);
    va_end(args);
    say(line);
}

/* Every native request and buffer passes through here on its way to the session owner. */
static struct kumo_gpu_api tracked;
static uint32_t (*real_native)(void *, const struct kumo_gpu_native_request *, struct kumo_gpu_native_reply *);
static uint32_t (*real_heap_create)(void *, const struct kumo_gpu_heap_request *, struct kumo_gpu_heap_info *);
static uint32_t (*real_heap_destroy)(void *, uint32_t, uint64_t *);
static uint32_t (*real_buffer_create)(void *, uint64_t, uint32_t, struct kumo_gpu_buffer_info *);
static uint32_t (*real_buffer_destroy)(void *, uint32_t);
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

/* "queues 0, 1 and 2" style, for the submission report. */
static void describe_mask(char *out, size_t bytes, unsigned mask)
{
    unsigned count = 0, total = __builtin_popcount(mask);
    size_t used = 0;
    out[0] = 0;
    for (unsigned q = 0; q < KUMO_NATIVE_MAX_STREAMS && used < bytes; ++q) {
        if (!(mask & (1u << q)))
            continue;
        const char *sep = !count ? "" : count + 1 == total ? " and " : ", ";
        used += (size_t)snprintf(out + used, bytes - used, "%s%u", sep, q);
        ++count;
    }
}

#define INSTANCE_FN(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(instance, #name)
#define DEVICE_FN(name) PFN_##name name = (PFN_##name)get_device_proc(device, #name)

unsigned kumo_vk_compute_cases(const struct kumo_gpu_api *api, void (*say)(const char *))
{
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
    void *mapped = NULL;

    memset(&seen, 0, sizeof seen);
    tracked = *api;
    real_native = api->native;
    real_heap_create = api->heap_create;
    real_heap_destroy = api->heap_destroy;
    real_buffer_create = api->buffer_create;
    real_buffer_destroy = api->buffer_destroy;
    tracked.native = observe_native;
    tracked.heap_create = observe_heap_create;
    tracked.heap_destroy = observe_heap_destroy;
    tracked.buffer_create = observe_buffer_create;
    tracked.buffer_destroy = observe_buffer_destroy;

    /* The pinned module (vk-spirv.py --verify checks its hashes on the host). */
    if (sizeof kumo_vk_compute_spv != 308 * sizeof(uint32_t) || kumo_vk_compute_spv[0] != 0x07230203u)
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
        .pApplicationName = "kumo-vk3-b",
        .apiVersion = VK_API_VERSION_1_1,
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
    INSTANCE_FN(vkCreateDevice);
    INSTANCE_FN(vkGetDeviceProcAddr);
    CHECK(destroy_instance && vkEnumeratePhysicalDevices && vkGetPhysicalDeviceProperties &&
          vkGetPhysicalDeviceMemoryProperties && vkCreateDevice && vkGetDeviceProcAddr, 6);

    uint32_t count = 1;
    VkPhysicalDevice physical = VK_NULL_HANDLE;
    CHECK(vkEnumeratePhysicalDevices(instance, &count, &physical) == VK_SUCCESS && count == 1 && physical, 7);
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(physical, &props);
    CHECK(props.vendorID == 0x13b5 && strstr(props.deviceName, "Mali-G610"), 8);
    CHECK(props.limits.maxComputeWorkGroupSize[0] >= LOCAL_SIZE &&
          props.limits.maxComputeWorkGroupCount[0] >= GROUPS &&
          props.limits.maxPushConstantsSize >= 2 * sizeof(uint32_t), 9);
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceMemoryProperties(physical, &memory);
    CHECK(kumo_vk_memory_types_valid(&memory), 10);

    const float priority = 1.0f;
    const VkDeviceQueueCreateInfo queue_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = 0,
        .queueCount = 1,
        .pQueuePriorities = &priority,
    };
    const VkDeviceCreateInfo device_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
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
    DEVICE_FN(vkDestroyFence);
    DEVICE_FN(vkWaitForFences);
    DEVICE_FN(vkCreateShaderModule);
    DEVICE_FN(vkDestroyShaderModule);
    DEVICE_FN(vkCreateDescriptorSetLayout);
    DEVICE_FN(vkDestroyDescriptorSetLayout);
    DEVICE_FN(vkCreatePipelineLayout);
    DEVICE_FN(vkDestroyPipelineLayout);
    DEVICE_FN(vkCreateComputePipelines);
    DEVICE_FN(vkDestroyPipeline);
    DEVICE_FN(vkCreateBuffer);
    DEVICE_FN(vkDestroyBuffer);
    DEVICE_FN(vkGetBufferMemoryRequirements);
    DEVICE_FN(vkAllocateMemory);
    DEVICE_FN(vkFreeMemory);
    DEVICE_FN(vkBindBufferMemory);
    DEVICE_FN(vkMapMemory);
    DEVICE_FN(vkUnmapMemory);
    DEVICE_FN(vkCreateDescriptorPool);
    DEVICE_FN(vkDestroyDescriptorPool);
    DEVICE_FN(vkAllocateDescriptorSets);
    DEVICE_FN(vkUpdateDescriptorSets);
    DEVICE_FN(vkCreateCommandPool);
    DEVICE_FN(vkDestroyCommandPool);
    DEVICE_FN(vkAllocateCommandBuffers);
    DEVICE_FN(vkBeginCommandBuffer);
    DEVICE_FN(vkEndCommandBuffer);
    DEVICE_FN(vkCmdBindPipeline);
    DEVICE_FN(vkCmdBindDescriptorSets);
    DEVICE_FN(vkCmdPushConstants);
    DEVICE_FN(vkCmdDispatch);
    DEVICE_FN(vkCmdPipelineBarrier);
    CHECK(destroy_device && vkGetDeviceQueue && vkDeviceWaitIdle && vkQueueSubmit && vkCreateFence &&
          vkDestroyFence && vkWaitForFences && vkCreateShaderModule && vkDestroyShaderModule &&
          vkCreateDescriptorSetLayout && vkDestroyDescriptorSetLayout && vkCreatePipelineLayout &&
          vkDestroyPipelineLayout && vkCreateComputePipelines && vkDestroyPipeline && vkCreateBuffer &&
          vkDestroyBuffer && vkGetBufferMemoryRequirements && vkAllocateMemory && vkFreeMemory &&
          vkBindBufferMemory && vkMapMemory && vkUnmapMemory && vkCreateDescriptorPool &&
          vkDestroyDescriptorPool && vkAllocateDescriptorSets && vkUpdateDescriptorSets &&
          vkCreateCommandPool && vkDestroyCommandPool && vkAllocateCommandBuffers && vkBeginCommandBuffer &&
          vkEndCommandBuffer && vkCmdBindPipeline && vkCmdBindDescriptorSets && vkCmdPushConstants &&
          vkCmdDispatch && vkCmdPipelineBarrier, 12);
    /* VK2-e's device: one three-queue group, one heap, three single-stream init submissions. */
    CHECK(seen.groups == 1 && seen.group_queues == 1 && seen.heaps == 1 && seen.prepares == 3 &&
          seen.submitted == 3 && seen.blocked == 0, 13);
    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(device, 0, 0, &queue);
    CHECK(queue, 14);
    report(say, "VK3-b device: %s through the static ICD, one three-queue native group and %u init submissions",
           props.deviceName, seen.submitted);

    /* The pipeline: compiled from SPIR-V at runtime, with no cache and nothing submitted. */
    const VkShaderModuleCreateInfo module_info = {
        .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
        .codeSize = sizeof kumo_vk_compute_spv,
        .pCode = kumo_vk_compute_spv,
    };
    CHECK(vkCreateShaderModule(device, &module_info, NULL, &module) == VK_SUCCESS && module, 15);
    const VkDescriptorSetLayoutBinding binding = {
        .binding = 0,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .descriptorCount = 1,
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
    };
    const VkDescriptorSetLayoutCreateInfo set_layout_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 1,
        .pBindings = &binding,
    };
    CHECK(vkCreateDescriptorSetLayout(device, &set_layout_info, NULL, &set_layout) == VK_SUCCESS, 16);
    const VkPushConstantRange push_range = {
        .stageFlags = VK_SHADER_STAGE_COMPUTE_BIT,
        .offset = 0,
        .size = 2 * sizeof(uint32_t),
    };
    const VkPipelineLayoutCreateInfo layout_info = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1,
        .pSetLayouts = &set_layout,
        .pushConstantRangeCount = 1,
        .pPushConstantRanges = &push_range,
    };
    CHECK(vkCreatePipelineLayout(device, &layout_info, NULL, &layout) == VK_SUCCESS, 17);
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
    const unsigned before_pipeline = seen.prepares;
    CHECK(vkCreateComputePipelines(device, VK_NULL_HANDLE, 1, &pipeline_info, NULL, &pipeline) == VK_SUCCESS &&
          pipeline, 18);
    CHECK(seen.prepares == before_pipeline, 19);
    /* The module may go once the pipeline exists. */
    vkDestroyShaderModule(device, module, NULL);
    module = VK_NULL_HANDLE;
    report(say, "VK3-b pipeline: the pinned %u-word SPIR-V module compiled by panvk on KUMO into a compute "
           "pipeline (local size %u, no pipeline cache), with no native submission",
           (unsigned)(sizeof kumo_vk_compute_spv / sizeof(uint32_t)), LOCAL_SIZE);

    /* The storage buffer: host-visible and coherent, poisoned from the CPU. */
    const VkBufferCreateInfo buffer_info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = WORDS * sizeof(uint32_t),
        .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    CHECK(vkCreateBuffer(device, &buffer_info, NULL, &buffer) == VK_SUCCESS && buffer, 20);
    VkMemoryRequirements requirements;
    vkGetBufferMemoryRequirements(device, buffer, &requirements);
    const VkMemoryPropertyFlags wanted = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    uint32_t type = memory.memoryTypeCount;
    for (uint32_t i = 0; i < memory.memoryTypeCount && type == memory.memoryTypeCount; ++i)
        if ((requirements.memoryTypeBits & (1u << i)) &&
            (memory.memoryTypes[i].propertyFlags & wanted) == wanted)
            type = i;
    CHECK(type < memory.memoryTypeCount && requirements.size >= buffer_info.size, 21);
    const VkMemoryAllocateInfo allocate_info = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = requirements.size,
        .memoryTypeIndex = type,
    };
    CHECK(vkAllocateMemory(device, &allocate_info, NULL, &backing) == VK_SUCCESS && backing, 22);
    CHECK(vkBindBufferMemory(device, buffer, backing, 0) == VK_SUCCESS, 23);
    CHECK(vkMapMemory(device, backing, 0, VK_WHOLE_SIZE, 0, &mapped) == VK_SUCCESS && mapped, 24);
    volatile uint32_t *const words = mapped;
    for (uint32_t i = 0; i < WORDS; ++i)
        words[i] = POISON;
    report(say, "VK3-b buffer: %u storage words (%u-byte allocation, memory type %u: host-visible, coherent, "
           "uncached) poisoned to 0x%08x from the CPU", WORDS, (unsigned)requirements.size, type, POISON);

    /* The descriptor set and the command buffer. */
    const VkDescriptorPoolSize pool_size = {
        .type = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .descriptorCount = 1,
    };
    const VkDescriptorPoolCreateInfo descriptor_pool_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
        .maxSets = 1,
        .poolSizeCount = 1,
        .pPoolSizes = &pool_size,
    };
    CHECK(vkCreateDescriptorPool(device, &descriptor_pool_info, NULL, &descriptor_pool) == VK_SUCCESS, 25);
    const VkDescriptorSetAllocateInfo set_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
        .descriptorPool = descriptor_pool,
        .descriptorSetCount = 1,
        .pSetLayouts = &set_layout,
    };
    VkDescriptorSet set = VK_NULL_HANDLE;
    CHECK(vkAllocateDescriptorSets(device, &set_info, &set) == VK_SUCCESS && set, 26);
    const VkDescriptorBufferInfo described = {
        .buffer = buffer,
        .offset = 0,
        .range = VK_WHOLE_SIZE,
    };
    const VkWriteDescriptorSet write = {
        .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
        .dstSet = set,
        .dstBinding = 0,
        .descriptorCount = 1,
        .descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
        .pBufferInfo = &described,
    };
    vkUpdateDescriptorSets(device, 1, &write, 0, NULL);
    const VkCommandPoolCreateInfo command_pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = 0,
    };
    CHECK(vkCreateCommandPool(device, &command_pool_info, NULL, &command_pool) == VK_SUCCESS, 27);
    const VkCommandBufferAllocateInfo command_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = command_pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = PASSES,
    };
    VkCommandBuffer commands[PASSES] = {VK_NULL_HANDLE};
    CHECK(vkAllocateCommandBuffers(device, &command_info, commands) == VK_SUCCESS && commands[PASSES - 1], 28);
    const VkCommandBufferBeginInfo begin = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    const VkMemoryBarrier to_host = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
    };
    static const uint32_t k[PASSES] = {K, K2};
    for (unsigned pass = 0; pass < PASSES; ++pass) {
        const uint32_t push[2] = {k[pass], COUNT};
        CHECK(vkBeginCommandBuffer(commands[pass], &begin) == VK_SUCCESS, 29);
        vkCmdBindPipeline(commands[pass], VK_PIPELINE_BIND_POINT_COMPUTE, pipeline);
        vkCmdBindDescriptorSets(commands[pass], VK_PIPELINE_BIND_POINT_COMPUTE, layout, 0, 1, &set, 0, NULL);
        vkCmdPushConstants(commands[pass], layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof push, push);
        vkCmdDispatch(commands[pass], GROUPS, 1, 1);
        vkCmdPipelineBarrier(commands[pass], VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1,
                             &to_host, 0, NULL, 0, NULL);
        CHECK(vkEndCommandBuffer(commands[pass]) == VK_SUCCESS, 30);
    }
    CHECK(seen.prepares == before_pipeline, 31);
    report(say, "VK3-b record: two command buffers, each bind, push k (0x%08x, then 0x%08x) and count=%u, dispatch "
           "%u workgroups of %u, compute-to-host barrier", K, K2, COUNT, GROUPS, LOCAL_SIZE);

    const VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    CHECK(vkCreateFence(device, &fence_info, NULL, &fence) == VK_SUCCESS, 32);
    DEVICE_FN(vkResetFences);
    CHECK(vkResetFences, 33);
    for (unsigned pass = 0; pass < PASSES; ++pass) {
        /* Each pass starts from the CPU's poison, so its readback is that pass's dispatch alone. */
        for (uint32_t i = 0; i < WORDS; ++i)
            words[i] = POISON;
        const VkSubmitInfo submit = {
            .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
            .commandBufferCount = 1,
            .pCommandBuffers = &commands[pass],
        };
        const unsigned first = seen.prepares, streams = seen.streams, compute = seen.stream_queue[2];
        CHECK(vkResetFences(device, 1, &fence) == VK_SUCCESS, 34);
        CHECK(vkQueueSubmit(queue, 1, &submit, fence) == VK_SUCCESS, 35);
        CHECK(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX) == VK_SUCCESS, 36);
        CHECK(seen.refused == 0 && api->device_status(api->context) == KUMO_GPU_OK, 37);

        /* Measured first, checked after: the split and the readback print even when a check fails. */
        const unsigned submissions = seen.prepares - first, carried = seen.streams - streams;
        char split[96];
        size_t used = 0;
        split[0] = 0;
        for (unsigned i = first; i < seen.prepares && i < MAX_PREPARES && used < sizeof split; ++i) {
            char queues[32];
            describe_mask(queues, sizeof queues, seen.prepare_mask[i]);
            used += (size_t)snprintf(split + used, sizeof split - used, "%s[%s]", i == first ? "" : " then ", queues);
        }
        report(say, "VK3-b submit %u: one fenced vkQueueSubmit as %u native submission%s of %u stream%s: "
               "subqueues %s; the fence completed", pass + 1, submissions, submissions == 1 ? "" : "s", carried,
               carried == 1 ? "" : "s", submissions ? split : "none");

        unsigned wrong = 0, poisoned = 0, guards = 0;
        for (uint32_t i = 0; i < COUNT; ++i)
            wrong += words[i] != i * 3u + k[pass];
        for (uint32_t i = COUNT; i < GROUPS * LOCAL_SIZE; ++i)
            poisoned += words[i] != POISON;
        for (uint32_t i = GROUPS * LOCAL_SIZE; i < WORDS; ++i)
            guards += words[i] != POISON;
        report(say, "VK3-b readback %u: %u of %u words differ from i*3+k; %u of %u invocations at or above count "
               "and %u of %u guards changed (Mali: first=0x%08x last=0x%08x)", pass + 1, wrong, COUNT, poisoned,
               GROUPS * LOCAL_SIZE - COUNT, guards, GUARDS, words[0], words[COUNT - 1]);
        CHECK(seen.submitted == seen.prepares, 38);
        /* The first dispatch adds panvk's one-time compute resource request (REQ_RESOURCE) on
         * subqueue 2 ahead of the command buffer. Subqueue 2 repeats, so KUMO's split makes it
         * its own native submission, as Panthor would run it first on that queue. Later
         * dispatches are the command buffer alone: one submission of three streams. */
        if (pass == 0)
            CHECK(submissions == 2 && carried == 4 && seen.prepare_mask[first] == 0x4 &&
                  seen.prepare_mask[first + 1] == 0x7 && seen.stream_queue[2] == compute + 2, 39);
        else
            CHECK(submissions == 1 && carried == 3 && first < MAX_PREPARES && seen.prepare_mask[first] == 0x7 &&
                  seen.stream_queue[2] == compute + 1, 40);
        CHECK(!wrong && !poisoned && !guards, 41);
        CHECK(words[0] == k[pass] && words[COUNT - 1] == (COUNT - 1) * 3u + k[pass], 42);
    }

done:
    if (device && destroy_device && get_device_proc) {
        /* Nothing may still run when its objects go. */
        PFN_vkDeviceWaitIdle wait_idle = (PFN_vkDeviceWaitIdle)get_device_proc(device, "vkDeviceWaitIdle");
        if (!wait_idle || wait_idle(device) != VK_SUCCESS)
            failed = failed ? failed : 43;
#define RELEASE(fn, object) do { if (object) { PFN_##fn release = (PFN_##fn)get_device_proc(device, #fn); \
                                                 release(device, object, NULL); } } while (0)
        RELEASE(vkDestroyFence, fence);
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
    /* Teardown keeps only the session's group, which lasts until the session closes. */
    if (seen.heaps_released != 1 || seen.live != 1)
        return 44;
    if (seen.buffers_released != seen.buffers)
        return 45;
    if (seen.drm_ioctls || seen.drm_listings || seen.drm_syncobjs)
        return 46;
    report(say, "VK3-b teardown: every object, the device and the instance destroyed; %u of %u native buffers "
           "and the heap released, only the session's group left; no Panthor ioctl, DRM listing or syncobj",
           seen.buffers_released, seen.buffers);
    return 0;
}

#ifdef KUMO_VK_COMPUTE_MAIN
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_vk_compute_cases(api, kumo_metal_log);
}

int main(void)
{
    kumo_metal_log("VK3-b metal compute begin: VK3-a's pinned SPIR-V through panvk on the native session");
    const unsigned failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "VK3-b metal compute FAILED at case %u", failed);
        return 1;
    }
    kumo_metal_log("VK3-b metal compute green: a runtime-compiled Vulkan compute pipeline wrote all 1000 storage "
                   "words on Mali twice, the bound and guards intact, and clean teardown");
    return 0;
}
#endif
