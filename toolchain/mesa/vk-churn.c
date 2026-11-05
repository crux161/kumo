/* SPDX-License-Identifier: MIT */
/*j787*/
/* VK5-a (PLAN/015 §8.46): Vulkan texture churn, offscreen, one session. The static-ICD client
 * of VK2-e brings up panvk on the session's kmod device, then runs GL4's frame through Vulkan
 * 100 times (D1a, ratified 2026-11-04):
 * - VK5's pinned SPIR-V: a viewport-covering triangle placed by gl_VertexIndex, and a
 *   fragment stage copying texel (x, y) to pixel (x, y) by texelFetch;
 * - a persistent 256x160 linear render target (cleared to 0 each frame, with a poisoned 4 KiB
 *   guard past it) and a persistent 160 KiB host-visible staging buffer;
 * - each frame writes GL4's pattern (R = x, G = y, B = frame, A = 0xff) into the staging
 *   buffer, creates a 256x160 optimal R8G8B8A8 SAMPLED|TRANSFER_DST image with its own memory,
 *   copies the buffer into it (panvk's vk_meta copy), draws with it, makes the color writes
 *   visible to the host, submits once with a fence, checks every pixel and the guard, and
 *   destroys the image, its view and its memory: 16,000 KiB churned in all;
 * - every native submission and buffer is counted per frame, so the first frame's split and
 *   the steady one are measured, and every buffer created is freed by teardown.
 * The native API is observed in place, and three libdrm entries are wrapped at link time, so
 * any Panthor ioctl, DRM device listing or syncobj creation fails the proof. */
#include "kumo_kmod.h"
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <vulkan/vulkan_core.h>
#include "vk-churn-frag.spv.h"
#include "vk-churn-vert.spv.h"
#include "vk-memory-policy.h"

PFN_vkVoidFunction vk_icdGetInstanceProcAddr(VkInstance instance, const char *name);
bool panvk_kumo_offer_device(struct pan_kmod_dev *kmod);
/* panvk's own AFBC predicate for a new image (panvk_image.c); a VkPhysicalDevice handle is its
 * panvk_physical_device. It names the layout the copy writes rather than guessing it. */
struct panvk_physical_device;
bool panvk_image_can_use_afbc(struct panvk_physical_device *phys_dev, VkFormat fmt, VkImageUsageFlags usage,
                              VkImageType type, VkImageTiling tiling, VkImageCreateFlags flags);
unsigned kumo_vk_churn_cases(const struct kumo_gpu_api *api, void (*say)(const char *));

#define CHECK(test, code) do { if (!(test)) { failed = (code); goto done; } } while (0)

#define WIDTH 256u
#define HEIGHT 160u
#define PIXELS (WIDTH * HEIGHT)
#define FRAMES 100u
#define POISON UINT32_C(0xdeadbeef)
#define GUARD UINT32_C(0xa5a5a5a5)
#define GUARD_BYTES 4096u
#define TEXTURE_USAGE (VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT)

static void report(void (*say)(const char *), const char *format, ...)
{
    char line[256];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof line, format, args);
    va_end(args);
    say(line);
}

/* Texel (x, y) of the frame tagged `tag`, as packed RGBA8: R = x, G = y, B = tag, A = 0xff
 * (GL4's pattern). The draw copies texel (x, y) to pixel (x, y), so it is also the readback. */
static uint32_t pattern(unsigned tag, unsigned x, unsigned y)
{
    return UINT32_C(0xff000000) | ((uint32_t)(tag & 0xff) << 16) | ((uint32_t)y << 8) | x;
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
    /* Per PREPARE since the client's last frame mark: its streams' subqueue mask, in order. */
    unsigned prepare_mask[MAX_PREPARES], frame_prepares;
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
        if (seen.frame_prepares < MAX_PREPARES)
            seen.prepare_mask[seen.frame_prepares] = mask;
        ++seen.frame_prepares;
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

static const VkShaderModuleCreateInfo vertex_info = {
    .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
    .codeSize = sizeof kumo_vk_churn_vert_spv,
    .pCode = kumo_vk_churn_vert_spv,
};
static const VkShaderModuleCreateInfo fragment_info = {
    .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
    .codeSize = sizeof kumo_vk_churn_frag_spv,
    .pCode = kumo_vk_churn_frag_spv,
};

static uint32_t memory_type(const VkPhysicalDeviceMemoryProperties *memory, uint32_t bits, VkMemoryPropertyFlags wanted)
{
    for (uint32_t i = 0; i < memory->memoryTypeCount; ++i)
        if ((bits & (1u << i)) && (memory->memoryTypes[i].propertyFlags & wanted) == wanted)
            return i;
    return memory->memoryTypeCount;
}

/* "[0 and 1] then [0, 1 and 2]" for the PREPAREs since the last frame mark. */
static void describe_split(char *out, size_t bytes)
{
    size_t used = 0;
    out[0] = 0;
    for (unsigned i = 0; i < seen.frame_prepares && i < MAX_PREPARES && used < bytes; ++i) {
        char queues[32];
        describe_mask(queues, sizeof queues, seen.prepare_mask[i]);
        used += (size_t)snprintf(out + used, bytes - used, "%s[%s]", i ? " then " : "", queues);
    }
}

unsigned kumo_vk_churn_cases(const struct kumo_gpu_api *api, void (*say)(const char *))
{
    unsigned failed = 0;
    VkInstance instance = VK_NULL_HANDLE;
    VkDevice device = VK_NULL_HANDLE;
    PFN_vkDestroyInstance destroy_instance = NULL;
    PFN_vkDestroyDevice destroy_device = NULL;
    PFN_vkGetDeviceProcAddr get_device_proc = NULL;
    VkShaderModule modules[2] = {VK_NULL_HANDLE, VK_NULL_HANDLE};
    VkDescriptorSetLayout set_layout = VK_NULL_HANDLE;
    VkPipelineLayout layout = VK_NULL_HANDLE;
    VkRenderPass pass = VK_NULL_HANDLE;
    VkPipeline pipeline = VK_NULL_HANDLE;
    VkSampler sampler = VK_NULL_HANDLE;
    VkDescriptorPool descriptor_pool = VK_NULL_HANDLE;
    VkImage target = VK_NULL_HANDLE, texture = VK_NULL_HANDLE;
    VkImageView target_view = VK_NULL_HANDLE, texture_view = VK_NULL_HANDLE;
    VkFramebuffer framebuffer = VK_NULL_HANDLE;
    VkDeviceMemory target_memory = VK_NULL_HANDLE, staging_memory = VK_NULL_HANDLE,
                   texture_memory = VK_NULL_HANDLE;
    VkBuffer staging = VK_NULL_HANDLE;
    VkCommandPool command_pool = VK_NULL_HANDLE;
    VkFence fence = VK_NULL_HANDLE;
    void *target_map = NULL, *staging_map = NULL;

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

    /* The pinned modules (vk-spirv.py --verify checks their hashes on the host). */
    if (sizeof kumo_vk_churn_vert_spv != 285 * sizeof(uint32_t) ||
        sizeof kumo_vk_churn_frag_spv != 170 * sizeof(uint32_t) ||
        kumo_vk_churn_vert_spv[0] != 0x07230203u || kumo_vk_churn_frag_spv[0] != 0x07230203u)
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
        .pApplicationName = "kumo-vk5-a",
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
    INSTANCE_FN(vkGetPhysicalDeviceFormatProperties);
    INSTANCE_FN(vkCreateDevice);
    INSTANCE_FN(vkGetDeviceProcAddr);
    CHECK(destroy_instance && vkEnumeratePhysicalDevices && vkGetPhysicalDeviceProperties &&
          vkGetPhysicalDeviceMemoryProperties && vkGetPhysicalDeviceFormatProperties && vkCreateDevice &&
          vkGetDeviceProcAddr, 6);

    uint32_t count = 1;
    VkPhysicalDevice physical = VK_NULL_HANDLE;
    CHECK(vkEnumeratePhysicalDevices(instance, &count, &physical) == VK_SUCCESS && count == 1 && physical, 7);
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(physical, &props);
    CHECK(props.vendorID == 0x13b5 && strstr(props.deviceName, "Mali-G610"), 8);
    VkFormatProperties format;
    vkGetPhysicalDeviceFormatProperties(physical, VK_FORMAT_R8G8B8A8_UNORM, &format);
    CHECK((format.linearTilingFeatures & VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT) &&
          (format.optimalTilingFeatures & VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT) &&
          (format.optimalTilingFeatures & VK_FORMAT_FEATURE_TRANSFER_DST_BIT), 9);
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceMemoryProperties(physical, &memory);
    CHECK(kumo_vk_memory_types_valid(&memory), 10);
    const bool afbc = panvk_image_can_use_afbc((struct panvk_physical_device *)physical, VK_FORMAT_R8G8B8A8_UNORM,
                                               TEXTURE_USAGE, VK_IMAGE_TYPE_2D, VK_IMAGE_TILING_OPTIMAL, 0);

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
    DEVICE_FN(vkQueueSubmit);
    DEVICE_FN(vkCreateFence);
    DEVICE_FN(vkWaitForFences);
    DEVICE_FN(vkResetFences);
    DEVICE_FN(vkCreateShaderModule);
    DEVICE_FN(vkDestroyShaderModule);
    DEVICE_FN(vkCreateDescriptorSetLayout);
    DEVICE_FN(vkCreatePipelineLayout);
    DEVICE_FN(vkCreateRenderPass);
    DEVICE_FN(vkCreateGraphicsPipelines);
    DEVICE_FN(vkCreateSampler);
    DEVICE_FN(vkCreateDescriptorPool);
    DEVICE_FN(vkAllocateDescriptorSets);
    DEVICE_FN(vkUpdateDescriptorSets);
    DEVICE_FN(vkCreateImage);
    DEVICE_FN(vkDestroyImage);
    DEVICE_FN(vkGetImageMemoryRequirements);
    DEVICE_FN(vkGetImageSubresourceLayout);
    DEVICE_FN(vkBindImageMemory);
    DEVICE_FN(vkCreateImageView);
    DEVICE_FN(vkDestroyImageView);
    DEVICE_FN(vkCreateFramebuffer);
    DEVICE_FN(vkCreateBuffer);
    DEVICE_FN(vkGetBufferMemoryRequirements);
    DEVICE_FN(vkBindBufferMemory);
    DEVICE_FN(vkAllocateMemory);
    DEVICE_FN(vkFreeMemory);
    DEVICE_FN(vkMapMemory);
    DEVICE_FN(vkCreateCommandPool);
    DEVICE_FN(vkAllocateCommandBuffers);
    DEVICE_FN(vkResetCommandBuffer);
    DEVICE_FN(vkBeginCommandBuffer);
    DEVICE_FN(vkEndCommandBuffer);
    DEVICE_FN(vkCmdPipelineBarrier);
    DEVICE_FN(vkCmdCopyBufferToImage);
    DEVICE_FN(vkCmdBeginRenderPass);
    DEVICE_FN(vkCmdEndRenderPass);
    DEVICE_FN(vkCmdBindPipeline);
    DEVICE_FN(vkCmdBindDescriptorSets);
    DEVICE_FN(vkCmdDraw);
    CHECK(destroy_device && vkGetDeviceQueue && vkQueueSubmit && vkCreateFence && vkWaitForFences &&
          vkResetFences && vkCreateShaderModule && vkDestroyShaderModule && vkCreateDescriptorSetLayout &&
          vkCreatePipelineLayout && vkCreateRenderPass && vkCreateGraphicsPipelines && vkCreateSampler &&
          vkCreateDescriptorPool && vkAllocateDescriptorSets && vkUpdateDescriptorSets && vkCreateImage &&
          vkDestroyImage && vkGetImageMemoryRequirements && vkGetImageSubresourceLayout && vkBindImageMemory &&
          vkCreateImageView && vkDestroyImageView && vkCreateFramebuffer && vkCreateBuffer &&
          vkGetBufferMemoryRequirements && vkBindBufferMemory && vkAllocateMemory && vkFreeMemory && vkMapMemory &&
          vkCreateCommandPool && vkAllocateCommandBuffers && vkResetCommandBuffer && vkBeginCommandBuffer &&
          vkEndCommandBuffer && vkCmdPipelineBarrier && vkCmdCopyBufferToImage && vkCmdBeginRenderPass &&
          vkCmdEndRenderPass && vkCmdBindPipeline && vkCmdBindDescriptorSets && vkCmdDraw, 12);
    /* VK2-e's device: one three-queue group, one heap, three single-stream init submissions. */
    CHECK(seen.groups == 1 && seen.group_queues == 1 && seen.heaps == 1 && seen.prepares == 3 &&
          seen.submitted == 3 && seen.blocked == 0, 13);
    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(device, 0, 0, &queue);
    CHECK(queue, 14);
    report(say, "VK5-a device: %s through the static ICD, one three-queue native group and %u init submissions",
           props.deviceName, seen.submitted);

    /* The persistent target: a linear image with a poisoned guard past it, as VK4-a's. */
    const VkImageCreateInfo target_info = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = VK_FORMAT_R8G8B8A8_UNORM,
        .extent = {WIDTH, HEIGHT, 1},
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_LINEAR,
        .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    CHECK(vkCreateImage(device, &target_info, NULL, &target) == VK_SUCCESS, 15);
    VkMemoryRequirements target_needs;
    vkGetImageMemoryRequirements(device, target, &target_needs);
    const VkImageSubresource color = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT};
    VkSubresourceLayout rows;
    vkGetImageSubresourceLayout(device, target, &color, &rows);
    CHECK(rows.rowPitch >= WIDTH * 4u && rows.rowPitch % 4 == 0 &&
          rows.offset + rows.rowPitch * (HEIGHT - 1) + WIDTH * 4u <= target_needs.size, 16);
    const VkMemoryPropertyFlags host = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    const VkDeviceSize target_bytes = target_needs.size + GUARD_BYTES;
    const VkMemoryAllocateInfo target_allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = target_bytes,
        .memoryTypeIndex = memory_type(&memory, target_needs.memoryTypeBits, host),
    };
    CHECK(target_allocation.memoryTypeIndex < memory.memoryTypeCount &&
          vkAllocateMemory(device, &target_allocation, NULL, &target_memory) == VK_SUCCESS &&
          vkBindImageMemory(device, target, target_memory, 0) == VK_SUCCESS, 17);
    CHECK(vkMapMemory(device, target_memory, 0, VK_WHOLE_SIZE, 0, &target_map) == VK_SUCCESS && target_map, 18);
    volatile uint32_t *const target_words = target_map;
    for (VkDeviceSize i = 0; i < target_needs.size / 4; ++i)
        target_words[i] = POISON;
    for (VkDeviceSize i = target_needs.size / 4; i < target_bytes / 4; ++i)
        target_words[i] = GUARD;
    const VkImageViewCreateInfo target_view_info = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
        .image = target,
        .viewType = VK_IMAGE_VIEW_TYPE_2D,
        .format = VK_FORMAT_R8G8B8A8_UNORM,
        .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
    };
    CHECK(vkCreateImageView(device, &target_view_info, NULL, &target_view) == VK_SUCCESS, 19);

    /* The persistent staging buffer: 160 KiB, host-visible. */
    const VkBufferCreateInfo staging_info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = PIXELS * 4u,
        .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    CHECK(vkCreateBuffer(device, &staging_info, NULL, &staging) == VK_SUCCESS, 20);
    VkMemoryRequirements staging_needs;
    vkGetBufferMemoryRequirements(device, staging, &staging_needs);
    const VkMemoryAllocateInfo staging_allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = staging_needs.size,
        .memoryTypeIndex = memory_type(&memory, staging_needs.memoryTypeBits, host),
    };
    CHECK(staging_allocation.memoryTypeIndex < memory.memoryTypeCount &&
          vkAllocateMemory(device, &staging_allocation, NULL, &staging_memory) == VK_SUCCESS &&
          vkBindBufferMemory(device, staging, staging_memory, 0) == VK_SUCCESS, 21);
    CHECK(vkMapMemory(device, staging_memory, 0, VK_WHOLE_SIZE, 0, &staging_map) == VK_SUCCESS && staging_map, 22);
    uint32_t *const texels = staging_map;

    /* The render pass, sampler, descriptor and pipeline. */
    const VkAttachmentDescription attachment = {
        .format = VK_FORMAT_R8G8B8A8_UNORM,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
        .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
        .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
        .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
        .finalLayout = VK_IMAGE_LAYOUT_GENERAL,
    };
    const VkAttachmentReference reference = {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    const VkSubpassDescription subpass = {
        .pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
        .colorAttachmentCount = 1,
        .pColorAttachments = &reference,
    };
    const VkRenderPassCreateInfo pass_info = {
        .sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
        .attachmentCount = 1,
        .pAttachments = &attachment,
        .subpassCount = 1,
        .pSubpasses = &subpass,
    };
    CHECK(vkCreateRenderPass(device, &pass_info, NULL, &pass) == VK_SUCCESS, 23);
    const VkFramebufferCreateInfo framebuffer_info = {
        .sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
        .renderPass = pass,
        .attachmentCount = 1,
        .pAttachments = &target_view,
        .width = WIDTH,
        .height = HEIGHT,
        .layers = 1,
    };
    CHECK(vkCreateFramebuffer(device, &framebuffer_info, NULL, &framebuffer) == VK_SUCCESS, 24);
    const VkSamplerCreateInfo sampler_info = {
        .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
        .magFilter = VK_FILTER_NEAREST,
        .minFilter = VK_FILTER_NEAREST,
        .mipmapMode = VK_SAMPLER_MIPMAP_MODE_NEAREST,
        .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
    };
    CHECK(vkCreateSampler(device, &sampler_info, NULL, &sampler) == VK_SUCCESS, 25);
    const VkDescriptorSetLayoutBinding binding = {
        .binding = 0,
        .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
        .descriptorCount = 1,
        .stageFlags = VK_SHADER_STAGE_FRAGMENT_BIT,
    };
    const VkDescriptorSetLayoutCreateInfo set_layout_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 1,
        .pBindings = &binding,
    };
    CHECK(vkCreateDescriptorSetLayout(device, &set_layout_info, NULL, &set_layout) == VK_SUCCESS, 26);
    const VkDescriptorPoolSize pool_size = {VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1};
    const VkDescriptorPoolCreateInfo descriptor_pool_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
        .maxSets = 1,
        .poolSizeCount = 1,
        .pPoolSizes = &pool_size,
    };
    CHECK(vkCreateDescriptorPool(device, &descriptor_pool_info, NULL, &descriptor_pool) == VK_SUCCESS, 27);
    const VkDescriptorSetAllocateInfo set_info = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
        .descriptorPool = descriptor_pool,
        .descriptorSetCount = 1,
        .pSetLayouts = &set_layout,
    };
    VkDescriptorSet set = VK_NULL_HANDLE;
    CHECK(vkAllocateDescriptorSets(device, &set_info, &set) == VK_SUCCESS && set, 28);
    const VkPipelineLayoutCreateInfo layout_info = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1,
        .pSetLayouts = &set_layout,
    };
    CHECK(vkCreatePipelineLayout(device, &layout_info, NULL, &layout) == VK_SUCCESS, 29);
    CHECK(vkCreateShaderModule(device, &vertex_info, NULL, &modules[0]) == VK_SUCCESS &&
          vkCreateShaderModule(device, &fragment_info, NULL, &modules[1]) == VK_SUCCESS, 30);
    const VkPipelineShaderStageCreateInfo stages[2] = {
        {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT,
         .module = modules[0], .pName = "main"},
        {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
         .module = modules[1], .pName = "main"},
    };
    const VkPipelineVertexInputStateCreateInfo input = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
    };
    const VkPipelineInputAssemblyStateCreateInfo assembly = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
        .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
    };
    const VkViewport viewport = {0.0f, 0.0f, (float)WIDTH, (float)HEIGHT, 0.0f, 1.0f};
    const VkRect2D scissor = {{0, 0}, {WIDTH, HEIGHT}};
    const VkPipelineViewportStateCreateInfo viewport_state = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
        .viewportCount = 1,
        .pViewports = &viewport,
        .scissorCount = 1,
        .pScissors = &scissor,
    };
    const VkPipelineRasterizationStateCreateInfo raster = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
        .polygonMode = VK_POLYGON_MODE_FILL,
        .cullMode = VK_CULL_MODE_NONE,
        .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE,
        .lineWidth = 1.0f,
    };
    const VkPipelineMultisampleStateCreateInfo multisample = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
        .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT,
    };
    const VkPipelineColorBlendAttachmentState blend_attachment = {
        .colorWriteMask = VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT | VK_COLOR_COMPONENT_B_BIT |
                          VK_COLOR_COMPONENT_A_BIT,
    };
    const VkPipelineColorBlendStateCreateInfo blend = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
        .attachmentCount = 1,
        .pAttachments = &blend_attachment,
    };
    const VkGraphicsPipelineCreateInfo pipeline_info = {
        .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
        .stageCount = 2,
        .pStages = stages,
        .pVertexInputState = &input,
        .pInputAssemblyState = &assembly,
        .pViewportState = &viewport_state,
        .pRasterizationState = &raster,
        .pMultisampleState = &multisample,
        .pColorBlendState = &blend,
        .layout = layout,
        .renderPass = pass,
        .subpass = 0,
    };
    const unsigned before_pipeline = seen.prepares;
    CHECK(vkCreateGraphicsPipelines(device, VK_NULL_HANDLE, 1, &pipeline_info, NULL, &pipeline) == VK_SUCCESS &&
          pipeline, 31);
    CHECK(seen.prepares == before_pipeline, 32);
    vkDestroyShaderModule(device, modules[0], NULL);
    vkDestroyShaderModule(device, modules[1], NULL);
    modules[0] = modules[1] = VK_NULL_HANDLE;
    report(say, "VK5-a setup: the pinned %u-word vertex and %u-word texelFetch fragment SPIR-V compiled by panvk "
           "into a graphics pipeline; a 256x160 linear target (row pitch %u) and a %u-byte staging buffer persist",
           (unsigned)(sizeof kumo_vk_churn_vert_spv / 4), (unsigned)(sizeof kumo_vk_churn_frag_spv / 4),
           (unsigned)rows.rowPitch, (unsigned)staging_needs.size);

    const VkCommandPoolCreateInfo command_pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
        .queueFamilyIndex = 0,
    };
    CHECK(vkCreateCommandPool(device, &command_pool_info, NULL, &command_pool) == VK_SUCCESS, 33);
    const VkCommandBufferAllocateInfo command_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = command_pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    VkCommandBuffer commands = VK_NULL_HANDLE;
    CHECK(vkAllocateCommandBuffers(device, &command_info, &commands) == VK_SUCCESS && commands, 34);
    const VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    CHECK(vkCreateFence(device, &fence_info, NULL, &fence) == VK_SUCCESS, 35);

    /* The churn. */
    const VkImageCreateInfo texture_info = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = VK_FORMAT_R8G8B8A8_UNORM,
        .extent = {WIDTH, HEIGHT, 1},
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_OPTIMAL,
        .usage = TEXTURE_USAGE,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };
    const VkCommandBufferBeginInfo begin = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    const VkClearValue clear = {.color = {.float32 = {0.0f, 0.0f, 0.0f, 0.0f}}};
    const VkRenderPassBeginInfo pass_begin = {
        .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
        .renderPass = pass,
        .framebuffer = framebuffer,
        .renderArea = scissor,
        .clearValueCount = 1,
        .pClearValues = &clear,
    };
    const VkBufferImageCopy region = {
        .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
        .imageExtent = {WIDTH, HEIGHT, 1},
    };
    const VkMemoryBarrier to_host = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
    };
    char first_split[96] = "", steady_split[96] = "";
    unsigned first_submissions = 0, steady_submissions = 0, steady_frames = 0;
    unsigned first_created = 0, first_freed = 0, steady_created = 0, steady_freed = 0;
    unsigned uneven = 0, wrong = 0, guards = 0;
    VkDeviceSize texture_bytes = 0;
    const unsigned churn_prepares = seen.prepares, churn_buffers = seen.buffers;
    for (unsigned f = 1; f <= FRAMES; ++f) {
        const unsigned created = seen.buffers, freed = seen.buffers_released;
        seen.frame_prepares = 0;
        for (unsigned y = 0; y < HEIGHT; ++y)
            for (unsigned x = 0; x < WIDTH; ++x)
                texels[y * WIDTH + x] = pattern(f, x, y);
        CHECK(vkCreateImage(device, &texture_info, NULL, &texture) == VK_SUCCESS, 36);
        VkMemoryRequirements texture_needs;
        vkGetImageMemoryRequirements(device, texture, &texture_needs);
        uint32_t type = memory_type(&memory, texture_needs.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
        if (type >= memory.memoryTypeCount)
            type = memory_type(&memory, texture_needs.memoryTypeBits, 0);
        const VkMemoryAllocateInfo texture_allocation = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            .allocationSize = texture_needs.size,
            .memoryTypeIndex = type,
        };
        CHECK(type < memory.memoryTypeCount &&
              vkAllocateMemory(device, &texture_allocation, NULL, &texture_memory) == VK_SUCCESS &&
              vkBindImageMemory(device, texture, texture_memory, 0) == VK_SUCCESS, 37);
        texture_bytes = texture_needs.size;
        const VkImageViewCreateInfo texture_view_info = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
            .image = texture,
            .viewType = VK_IMAGE_VIEW_TYPE_2D,
            .format = VK_FORMAT_R8G8B8A8_UNORM,
            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
        };
        CHECK(vkCreateImageView(device, &texture_view_info, NULL, &texture_view) == VK_SUCCESS, 38);
        const VkDescriptorImageInfo described = {sampler, texture_view, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL};
        const VkWriteDescriptorSet write = {
            .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
            .dstSet = set,
            .dstBinding = 0,
            .descriptorCount = 1,
            .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
            .pImageInfo = &described,
        };
        vkUpdateDescriptorSets(device, 1, &write, 0, NULL);

        const VkImageMemoryBarrier to_copy = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
            .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
            .newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = texture,
            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
        };
        const VkImageMemoryBarrier to_sample = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
            .dstAccessMask = VK_ACCESS_SHADER_READ_BIT,
            .oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            .newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = texture,
            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
        };
        CHECK(vkResetCommandBuffer(commands, 0) == VK_SUCCESS &&
              vkBeginCommandBuffer(commands, &begin) == VK_SUCCESS, 39);
        vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0,
                             NULL, 1, &to_copy);
        vkCmdCopyBufferToImage(commands, staging, texture, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &region);
        vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, 0, NULL,
                             0, NULL, 1, &to_sample);
        vkCmdBeginRenderPass(commands, &pass_begin, VK_SUBPASS_CONTENTS_INLINE);
        vkCmdBindPipeline(commands, VK_PIPELINE_BIND_POINT_GRAPHICS, pipeline);
        vkCmdBindDescriptorSets(commands, VK_PIPELINE_BIND_POINT_GRAPHICS, layout, 0, 1, &set, 0, NULL);
        vkCmdDraw(commands, 3, 1, 0, 0);
        vkCmdEndRenderPass(commands);
        vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1,
                             &to_host, 0, NULL, 0, NULL);
        CHECK(vkEndCommandBuffer(commands) == VK_SUCCESS, 40);
        const VkSubmitInfo submit = {
            .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
            .commandBufferCount = 1,
            .pCommandBuffers = &commands,
        };
        CHECK(vkResetFences(device, 1, &fence) == VK_SUCCESS, 41);
        CHECK(vkQueueSubmit(queue, 1, &submit, fence) == VK_SUCCESS, 42);
        CHECK(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX) == VK_SUCCESS, 43);
        CHECK(seen.refused == 0 && api->device_status(api->context) == KUMO_GPU_OK, 44);

        unsigned frame_wrong = 0, frame_guards = 0;
        const volatile uint8_t *const bytes = target_map;
        for (uint32_t y = 0; y < HEIGHT; ++y)
            for (uint32_t x = 0; x < WIDTH; ++x)
                frame_wrong += *(const volatile uint32_t *)(bytes + rows.offset + y * rows.rowPitch + 4u * x) !=
                               pattern(f, x, y);
        for (VkDeviceSize i = target_needs.size / 4; i < target_bytes / 4; ++i)
            frame_guards += target_words[i] != GUARD;
        wrong += frame_wrong;
        guards += frame_guards;

        vkDestroyImageView(device, texture_view, NULL);
        texture_view = VK_NULL_HANDLE;
        vkDestroyImage(device, texture, NULL);
        texture = VK_NULL_HANDLE;
        vkFreeMemory(device, texture_memory, NULL);
        texture_memory = VK_NULL_HANDLE;

        char split[96];
        describe_split(split, sizeof split);
        const unsigned frame_created = seen.buffers - created, frame_freed = seen.buffers_released - freed;
        if (f == 1) {
            memcpy(first_split, split, sizeof split);
            first_submissions = seen.frame_prepares;
            first_created = frame_created;
            first_freed = frame_freed;
        } else if (f == 2) {
            memcpy(steady_split, split, sizeof split);
            steady_submissions = seen.frame_prepares;
            steady_created = frame_created;
            steady_freed = frame_freed;
            steady_frames = 1;
        } else if (strcmp(split, steady_split) || seen.frame_prepares != steady_submissions ||
                   frame_created != steady_created || frame_freed != steady_freed) {
            if (!uneven)
                report(say, "VK5-a frame %u differs from frame 2: %u submissions %s, buffers +%u/-%u", f,
                       seen.frame_prepares, split, frame_created, frame_freed);
            ++uneven;
        } else {
            ++steady_frames;
        }
        if (frame_wrong || frame_guards) {
            report(say, "VK5-a frame %u failed: %u of %u pixels differ, %u guard words changed (first 0x%08x)", f,
                   frame_wrong, PIXELS, frame_guards,
                   *(const volatile uint32_t *)(bytes + rows.offset));
            CHECK(false, 45);
        }
    }
    CHECK(seen.submitted == seen.prepares && seen.blocked == 0, 46);
    report(say, "VK5-a texture: each frame's 256x160 optimal R8G8B8A8 image took %u bytes of its own memory; panvk's "
           "AFBC predicate says %s, so the copy ran vk_meta's %s path", (unsigned)texture_bytes, afbc ? "yes" : "no",
           afbc ? "graphics" : "compute");
    report(say, "VK5-a submits: frame 1 as %u native submission%s (%s); frames 2-100 each as %u (%s); %u in all",
           first_submissions, first_submissions == 1 ? "" : "s", first_split, steady_submissions, steady_split,
           seen.prepares - churn_prepares);
    report(say, "VK5-a storage: frame 1 created %u and freed %u native buffers; frames 2-100 each created %u and "
           "freed %u; %u created during the churn", first_created, first_freed, steady_created, steady_freed,
           seen.buffers - churn_buffers);
    report(say, "VK5-a frames: %u frames each created, uploaded, drew with and destroyed one 160 KiB texture "
           "(16000 KiB in all); %u of %u pixels differ; %u guard words changed (Mali: every texel copied exactly)",
           FRAMES, wrong, FRAMES * PIXELS, guards);
    _Static_assert(FRAMES * PIXELS == 4096000u, "the metal line names the exact pixel count");
    CHECK(!uneven && steady_frames == FRAMES - 1 && !wrong && !guards, 47);

done:
    if (device && destroy_device && get_device_proc) {
        /* Nothing may still run when its objects go. */
        PFN_vkDeviceWaitIdle wait_idle = (PFN_vkDeviceWaitIdle)get_device_proc(device, "vkDeviceWaitIdle");
        if (!wait_idle || wait_idle(device) != VK_SUCCESS)
            failed = failed ? failed : 48;
#define RELEASE(fn, object) do { if (object) { PFN_##fn release = (PFN_##fn)get_device_proc(device, #fn); \
                                                 release(device, object, NULL); } } while (0)
        RELEASE(vkDestroyFence, fence);
        RELEASE(vkDestroyCommandPool, command_pool);
        RELEASE(vkDestroyImageView, texture_view);
        RELEASE(vkDestroyImage, texture);
        RELEASE(vkFreeMemory, texture_memory);
        if (staging_map)
            ((PFN_vkUnmapMemory)get_device_proc(device, "vkUnmapMemory"))(device, staging_memory);
        if (target_map)
            ((PFN_vkUnmapMemory)get_device_proc(device, "vkUnmapMemory"))(device, target_memory);
        RELEASE(vkDestroyPipeline, pipeline);
        RELEASE(vkDestroyFramebuffer, framebuffer);
        RELEASE(vkDestroyRenderPass, pass);
        RELEASE(vkDestroyImageView, target_view);
        RELEASE(vkDestroyImage, target);
        RELEASE(vkDestroyBuffer, staging);
        RELEASE(vkFreeMemory, staging_memory);
        RELEASE(vkFreeMemory, target_memory);
        RELEASE(vkDestroyDescriptorPool, descriptor_pool);
        RELEASE(vkDestroySampler, sampler);
        RELEASE(vkDestroyPipelineLayout, layout);
        RELEASE(vkDestroyDescriptorSetLayout, set_layout);
        RELEASE(vkDestroyShaderModule, modules[0]);
        RELEASE(vkDestroyShaderModule, modules[1]);
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
        return 49;
    if (seen.buffers_released != seen.buffers)
        return 50;
    if (seen.drm_ioctls || seen.drm_listings || seen.drm_syncobjs)
        return 51;
    report(say, "VK5-a teardown: every object, the device and the instance destroyed; %u of %u native buffers "
           "and the heap released, only the session's group left; no Panthor ioctl, DRM listing or syncobj",
           seen.buffers_released, seen.buffers);
    return 0;
}

#ifdef KUMO_VK_CHURN_MAIN
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_vk_churn_cases(api, kumo_metal_log);
}

int main(void)
{
    kumo_metal_log("VK5-a metal churn begin: 100 Vulkan frames of 160 KiB texture churn through panvk, offscreen");
    const unsigned failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "VK5-a metal churn FAILED at case %u", failed);
        return 1;
    }
    kumo_metal_log("VK5-a metal churn green: 100 exact Vulkan frames, each creating, uploading, drawing with and "
                   "destroying a 160 KiB texture, and clean teardown");
    return 0;
}
#endif
