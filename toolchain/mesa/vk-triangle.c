/* SPDX-License-Identifier: MIT */
/*j784*/
/* VK4-a (PLAN/015 §8.43): the first Vulkan triangle, offscreen. The static-ICD client of
 * VK2-e brings up panvk on the session's kmod device, then draws GL3-a's triangle:
 * - VK4's pinned vertex and fragment SPIR-V become a graphics pipeline, compiled by panvk on
 *   KUMO with no pipeline cache and no native submission;
 * - a 256x128 R8G8B8A8_UNORM linear image is the render pass's one color attachment (clear,
 *   store, final GENERAL), bound at the start of a host-visible, coherent allocation whose
 *   tail past the image is a CPU-poisoned guard;
 * - one vertex buffer holds (-1,-1), (1,-1), (-1,1), each colored (0.8, 0.2, 0.4, 1);
 * - one command buffer clears to (0.2, 0.4, 0.6, 1), draws the triangle and makes the color
 *   writes visible to the host; one fenced vkQueueSubmit carries it, counted per subqueue;
 * - the CPU reads every pixel of the mapped image. Vulkan's origin is top-left, and the
 *   hypotenuse x + 2y = 256 holds no pixel center, so pixel (x, y) is the shade exactly when
 *   x + 2y <= 254 and the clear color otherwise; the guard must be intact;
 * - teardown releases every native buffer and object but the session's group.
 * The native API is observed in place, and three libdrm entries are wrapped at link time, so
 * any Panthor ioctl, DRM device listing or syncobj creation fails the proof. */
#include "kumo_kmod.h"
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <vulkan/vulkan_core.h>
#include "vk-memory-policy.h"
#include "vk-triangle-frag.spv.h"
#include "vk-triangle-vert.spv.h"

PFN_vkVoidFunction vk_icdGetInstanceProcAddr(VkInstance instance, const char *name);
bool panvk_kumo_offer_device(struct pan_kmod_dev *kmod);
unsigned kumo_vk_triangle_cases(const struct kumo_gpu_api *api, void (*say)(const char *));

#define CHECK(test, code) do { if (!(test)) { failed = (code); goto done; } } while (0)

#define WIDTH 256u
#define HEIGHT 128u
#define PIXELS (WIDTH * HEIGHT)
#define CLEARED UINT32_C(0xff996633)
#define SHADED UINT32_C(0xff6633cc)
#define POISON UINT32_C(0xdeadbeef)
#define GUARD UINT32_C(0xa5a5a5a5)
#define GUARD_BYTES 4096u
#define INSIDE(x, y) ((x) + 2u * (y) <= 254u)

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

static const VkShaderModuleCreateInfo vertex_info = {
    .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
    .codeSize = sizeof kumo_vk_triangle_vert_spv,
    .pCode = kumo_vk_triangle_vert_spv,
};
static const VkShaderModuleCreateInfo fragment_info = {
    .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
    .codeSize = sizeof kumo_vk_triangle_frag_spv,
    .pCode = kumo_vk_triangle_frag_spv,
};

/* Position (x, y) then color (r, g, b, a): the color is equal at every vertex. */
static const float vertices[3][6] = {
    {-1.0f, -1.0f, 0.8f, 0.2f, 0.4f, 1.0f},
    {1.0f, -1.0f, 0.8f, 0.2f, 0.4f, 1.0f},
    {-1.0f, 1.0f, 0.8f, 0.2f, 0.4f, 1.0f},
};

static uint32_t host_type(const VkPhysicalDeviceMemoryProperties *memory, uint32_t bits)
{
    const VkMemoryPropertyFlags wanted = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    for (uint32_t i = 0; i < memory->memoryTypeCount; ++i)
        if ((bits & (1u << i)) && (memory->memoryTypes[i].propertyFlags & wanted) == wanted)
            return i;
    return memory->memoryTypeCount;
}

unsigned kumo_vk_triangle_cases(const struct kumo_gpu_api *api, void (*say)(const char *))
{
    unsigned failed = 0;
    VkInstance instance = VK_NULL_HANDLE;
    VkDevice device = VK_NULL_HANDLE;
    PFN_vkDestroyInstance destroy_instance = NULL;
    PFN_vkDestroyDevice destroy_device = NULL;
    PFN_vkGetDeviceProcAddr get_device_proc = NULL;
    VkShaderModule modules[2] = {VK_NULL_HANDLE, VK_NULL_HANDLE};
    VkPipelineLayout layout = VK_NULL_HANDLE;
    VkRenderPass pass = VK_NULL_HANDLE;
    VkPipeline pipeline = VK_NULL_HANDLE;
    VkImage image = VK_NULL_HANDLE;
    VkImageView view = VK_NULL_HANDLE;
    VkFramebuffer framebuffer = VK_NULL_HANDLE;
    VkDeviceMemory image_memory = VK_NULL_HANDLE, vertex_memory = VK_NULL_HANDLE;
    VkBuffer vertex_buffer = VK_NULL_HANDLE;
    VkCommandPool command_pool = VK_NULL_HANDLE;
    VkFence fence = VK_NULL_HANDLE;
    void *image_map = NULL, *vertex_map = NULL;

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
    if (sizeof kumo_vk_triangle_vert_spv != 243 * sizeof(uint32_t) ||
        sizeof kumo_vk_triangle_frag_spv != 94 * sizeof(uint32_t) ||
        kumo_vk_triangle_vert_spv[0] != 0x07230203u || kumo_vk_triangle_frag_spv[0] != 0x07230203u)
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
        .pApplicationName = "kumo-vk4-a",
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
    CHECK(props.limits.maxFramebufferWidth >= WIDTH && props.limits.maxFramebufferHeight >= HEIGHT, 9);
    VkFormatProperties format;
    vkGetPhysicalDeviceFormatProperties(physical, VK_FORMAT_R8G8B8A8_UNORM, &format);
    CHECK(format.linearTilingFeatures & VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT, 10);
    VkPhysicalDeviceMemoryProperties memory;
    vkGetPhysicalDeviceMemoryProperties(physical, &memory);
    CHECK(kumo_vk_memory_types_valid(&memory), 11);

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
    CHECK(vkCreateDevice(physical, &device_info, NULL, &device) == VK_SUCCESS && device, 12);
    get_device_proc = vkGetDeviceProcAddr;
    destroy_device = (PFN_vkDestroyDevice)get_device_proc(device, "vkDestroyDevice");
    DEVICE_FN(vkGetDeviceQueue);
    DEVICE_FN(vkQueueSubmit);
    DEVICE_FN(vkCreateFence);
    DEVICE_FN(vkWaitForFences);
    DEVICE_FN(vkCreateShaderModule);
    DEVICE_FN(vkDestroyShaderModule);
    DEVICE_FN(vkCreatePipelineLayout);
    DEVICE_FN(vkCreateRenderPass);
    DEVICE_FN(vkCreateGraphicsPipelines);
    DEVICE_FN(vkCreateImage);
    DEVICE_FN(vkGetImageMemoryRequirements);
    DEVICE_FN(vkGetImageSubresourceLayout);
    DEVICE_FN(vkBindImageMemory);
    DEVICE_FN(vkCreateImageView);
    DEVICE_FN(vkCreateFramebuffer);
    DEVICE_FN(vkCreateBuffer);
    DEVICE_FN(vkGetBufferMemoryRequirements);
    DEVICE_FN(vkBindBufferMemory);
    DEVICE_FN(vkAllocateMemory);
    DEVICE_FN(vkMapMemory);
    DEVICE_FN(vkCreateCommandPool);
    DEVICE_FN(vkAllocateCommandBuffers);
    DEVICE_FN(vkBeginCommandBuffer);
    DEVICE_FN(vkEndCommandBuffer);
    DEVICE_FN(vkCmdBeginRenderPass);
    DEVICE_FN(vkCmdEndRenderPass);
    DEVICE_FN(vkCmdBindPipeline);
    DEVICE_FN(vkCmdBindVertexBuffers);
    DEVICE_FN(vkCmdDraw);
    DEVICE_FN(vkCmdPipelineBarrier);
    CHECK(destroy_device && vkGetDeviceQueue && vkQueueSubmit && vkCreateFence && vkWaitForFences &&
          vkCreateShaderModule && vkDestroyShaderModule && vkCreatePipelineLayout && vkCreateRenderPass &&
          vkCreateGraphicsPipelines && vkCreateImage && vkGetImageMemoryRequirements &&
          vkGetImageSubresourceLayout && vkBindImageMemory && vkCreateImageView && vkCreateFramebuffer &&
          vkCreateBuffer && vkGetBufferMemoryRequirements && vkBindBufferMemory && vkAllocateMemory &&
          vkMapMemory && vkCreateCommandPool && vkAllocateCommandBuffers && vkBeginCommandBuffer &&
          vkEndCommandBuffer && vkCmdBeginRenderPass && vkCmdEndRenderPass && vkCmdBindPipeline &&
          vkCmdBindVertexBuffers && vkCmdDraw && vkCmdPipelineBarrier, 13);
    /* VK2-e's device: one three-queue group, one heap, three single-stream init submissions. */
    CHECK(seen.groups == 1 && seen.group_queues == 1 && seen.heaps == 1 && seen.prepares == 3 &&
          seen.submitted == 3 && seen.blocked == 0, 14);
    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(device, 0, 0, &queue);
    CHECK(queue, 15);
    report(say, "VK4-a device: %s through the static ICD, one three-queue native group and %u init submissions",
           props.deviceName, seen.submitted);

    /* The target: a linear image at the start of a larger, host-visible allocation. */
    const VkImageCreateInfo image_info = {
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
    CHECK(vkCreateImage(device, &image_info, NULL, &image) == VK_SUCCESS && image, 16);
    VkMemoryRequirements image_needs;
    vkGetImageMemoryRequirements(device, image, &image_needs);
    const VkImageSubresource color = {.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT};
    VkSubresourceLayout rows;
    vkGetImageSubresourceLayout(device, image, &color, &rows);
    CHECK(rows.rowPitch >= WIDTH * 4u && rows.rowPitch % 4 == 0 &&
          rows.offset + rows.rowPitch * (HEIGHT - 1) + WIDTH * 4u <= image_needs.size, 17);
    const uint32_t image_type = host_type(&memory, image_needs.memoryTypeBits);
    CHECK(image_type < memory.memoryTypeCount, 18);
    const VkDeviceSize image_bytes = image_needs.size + GUARD_BYTES;
    const VkMemoryAllocateInfo image_allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = image_bytes,
        .memoryTypeIndex = image_type,
    };
    CHECK(vkAllocateMemory(device, &image_allocation, NULL, &image_memory) == VK_SUCCESS, 19);
    CHECK(vkBindImageMemory(device, image, image_memory, 0) == VK_SUCCESS, 20);
    CHECK(vkMapMemory(device, image_memory, 0, VK_WHOLE_SIZE, 0, &image_map) == VK_SUCCESS && image_map, 21);
    volatile uint32_t *const target = image_map;
    for (VkDeviceSize i = 0; i < image_needs.size / 4; ++i)
        target[i] = POISON;
    for (VkDeviceSize i = image_needs.size / 4; i < image_bytes / 4; ++i)
        target[i] = GUARD;
    report(say, "VK4-a target: 256x128 R8G8B8A8_UNORM linear image, row pitch %u, offset %u, %u bytes "
           "(memory type %u), poisoned to 0x%08x with a %u-byte guard past it",
           (unsigned)rows.rowPitch, (unsigned)rows.offset, (unsigned)image_needs.size, image_type, POISON,
           GUARD_BYTES);

    const VkImageViewCreateInfo view_info = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
        .image = image,
        .viewType = VK_IMAGE_VIEW_TYPE_2D,
        .format = VK_FORMAT_R8G8B8A8_UNORM,
        .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
    };
    CHECK(vkCreateImageView(device, &view_info, NULL, &view) == VK_SUCCESS, 22);
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
        .pAttachments = &view,
        .width = WIDTH,
        .height = HEIGHT,
        .layers = 1,
    };
    CHECK(vkCreateFramebuffer(device, &framebuffer_info, NULL, &framebuffer) == VK_SUCCESS, 24);

    /* The pipeline: compiled from SPIR-V at runtime, with no cache and nothing submitted. */
    CHECK(vkCreateShaderModule(device, &vertex_info, NULL, &modules[0]) == VK_SUCCESS &&
          vkCreateShaderModule(device, &fragment_info, NULL, &modules[1]) == VK_SUCCESS, 25);
    const VkPipelineLayoutCreateInfo layout_info = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
    CHECK(vkCreatePipelineLayout(device, &layout_info, NULL, &layout) == VK_SUCCESS, 26);
    const VkPipelineShaderStageCreateInfo stages[2] = {
        {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT,
         .module = modules[0], .pName = "main"},
        {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
         .module = modules[1], .pName = "main"},
    };
    const VkVertexInputBindingDescription binding = {0, sizeof vertices[0], VK_VERTEX_INPUT_RATE_VERTEX};
    const VkVertexInputAttributeDescription attributes[2] = {
        {0, 0, VK_FORMAT_R32G32_SFLOAT, 0},
        {1, 0, VK_FORMAT_R32G32B32A32_SFLOAT, 2 * sizeof(float)},
    };
    const VkPipelineVertexInputStateCreateInfo input = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
        .vertexBindingDescriptionCount = 1,
        .pVertexBindingDescriptions = &binding,
        .vertexAttributeDescriptionCount = 2,
        .pVertexAttributeDescriptions = attributes,
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
          pipeline, 27);
    CHECK(seen.prepares == before_pipeline, 28);
    vkDestroyShaderModule(device, modules[0], NULL);
    vkDestroyShaderModule(device, modules[1], NULL);
    modules[0] = modules[1] = VK_NULL_HANDLE;
    report(say, "VK4-a pipeline: the pinned %u-word vertex and %u-word fragment SPIR-V compiled by panvk on KUMO "
           "into a graphics pipeline (no pipeline cache), with no native submission",
           (unsigned)(sizeof kumo_vk_triangle_vert_spv / 4), (unsigned)(sizeof kumo_vk_triangle_frag_spv / 4));

    /* The vertex buffer. */
    const VkBufferCreateInfo vertex_buffer_info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = sizeof vertices,
        .usage = VK_BUFFER_USAGE_VERTEX_BUFFER_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    CHECK(vkCreateBuffer(device, &vertex_buffer_info, NULL, &vertex_buffer) == VK_SUCCESS, 29);
    VkMemoryRequirements vertex_needs;
    vkGetBufferMemoryRequirements(device, vertex_buffer, &vertex_needs);
    const VkMemoryAllocateInfo vertex_allocation = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize = vertex_needs.size,
        .memoryTypeIndex = host_type(&memory, vertex_needs.memoryTypeBits),
    };
    CHECK(vertex_allocation.memoryTypeIndex < memory.memoryTypeCount &&
          vkAllocateMemory(device, &vertex_allocation, NULL, &vertex_memory) == VK_SUCCESS &&
          vkBindBufferMemory(device, vertex_buffer, vertex_memory, 0) == VK_SUCCESS, 30);
    CHECK(vkMapMemory(device, vertex_memory, 0, VK_WHOLE_SIZE, 0, &vertex_map) == VK_SUCCESS && vertex_map, 31);
    memcpy(vertex_map, vertices, sizeof vertices);

    /* The command buffer: clear, one draw, color writes made visible to the host. */
    const VkCommandPoolCreateInfo command_pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = 0,
    };
    CHECK(vkCreateCommandPool(device, &command_pool_info, NULL, &command_pool) == VK_SUCCESS, 32);
    const VkCommandBufferAllocateInfo command_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = command_pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    VkCommandBuffer commands = VK_NULL_HANDLE;
    CHECK(vkAllocateCommandBuffers(device, &command_info, &commands) == VK_SUCCESS && commands, 33);
    const VkCommandBufferBeginInfo begin = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    const VkClearValue clear = {.color = {.float32 = {0.2f, 0.4f, 0.6f, 1.0f}}};
    const VkRenderPassBeginInfo pass_begin = {
        .sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
        .renderPass = pass,
        .framebuffer = framebuffer,
        .renderArea = scissor,
        .clearValueCount = 1,
        .pClearValues = &clear,
    };
    const VkMemoryBarrier to_host = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
        .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
        .dstAccessMask = VK_ACCESS_HOST_READ_BIT,
    };
    const VkDeviceSize zero = 0;
    CHECK(vkBeginCommandBuffer(commands, &begin) == VK_SUCCESS, 34);
    vkCmdBeginRenderPass(commands, &pass_begin, VK_SUBPASS_CONTENTS_INLINE);
    vkCmdBindPipeline(commands, VK_PIPELINE_BIND_POINT_GRAPHICS, pipeline);
    vkCmdBindVertexBuffers(commands, 0, 1, &vertex_buffer, &zero);
    vkCmdDraw(commands, 3, 1, 0, 0);
    vkCmdEndRenderPass(commands);
    vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1,
                         &to_host, 0, NULL, 0, NULL);
    CHECK(vkEndCommandBuffer(commands) == VK_SUCCESS, 35);
    CHECK(seen.prepares == before_pipeline, 36);
    say("VK4-a record: render pass clear to 0xff996633, one vkCmdDraw of 3 vertices (shade 0xff6633cc), "
        "color-to-host barrier");

    const VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    CHECK(vkCreateFence(device, &fence_info, NULL, &fence) == VK_SUCCESS, 37);
    const VkSubmitInfo submit = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .commandBufferCount = 1,
        .pCommandBuffers = &commands,
    };
    const unsigned first = seen.prepares, streams = seen.streams;
    CHECK(vkQueueSubmit(queue, 1, &submit, fence) == VK_SUCCESS, 38);
    CHECK(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX) == VK_SUCCESS, 39);
    CHECK(seen.refused == 0 && api->device_status(api->context) == KUMO_GPU_OK, 40);

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
    report(say, "VK4-a submit: one fenced vkQueueSubmit as %u native submission%s of %u stream%s: subqueues %s; "
           "the fence completed", submissions, submissions == 1 ? "" : "s", carried, carried == 1 ? "" : "s",
           submissions ? split : "none");

    unsigned wrong = 0, inside = 0, guards = 0;
    const volatile uint8_t *const bytes = image_map;
#define PIXEL(x, y) (*(const volatile uint32_t *)(bytes + rows.offset + (y) * rows.rowPitch + 4u * (x)))
    for (uint32_t y = 0; y < HEIGHT; ++y)
        for (uint32_t x = 0; x < WIDTH; ++x) {
            inside += INSIDE(x, y);
            wrong += PIXEL(x, y) != (INSIDE(x, y) ? SHADED : CLEARED);
        }
    for (VkDeviceSize i = image_needs.size / 4; i < image_bytes / 4; ++i)
        guards += target[i] != GUARD;
    report(say, "VK4-a readback: %u of %u pixels differ; %u of %u guard words changed (Mali triangle 0x%08x over "
           "clear 0x%08x)", wrong, PIXELS, guards, GUARD_BYTES / 4, SHADED, CLEARED);
    report(say, "VK4-a coverage: %u pixels inside x+2y<=254; samples (0,0)=0x%08x (254,0)=0x%08x (255,0)=0x%08x "
           "(0,127)=0x%08x (255,127)=0x%08x", inside, PIXEL(0, 0), PIXEL(254, 0), PIXEL(255, 0), PIXEL(0, 127),
           PIXEL(255, 127));
#undef PIXEL
    CHECK(seen.submitted == seen.prepares && submissions >= 1 && seen.stream_queue[0] && seen.stream_queue[1], 41);
    CHECK(!wrong && !guards && inside == PIXELS / 2, 42);

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
        if (vertex_map)
            ((PFN_vkUnmapMemory)get_device_proc(device, "vkUnmapMemory"))(device, vertex_memory);
        if (image_map)
            ((PFN_vkUnmapMemory)get_device_proc(device, "vkUnmapMemory"))(device, image_memory);
        RELEASE(vkDestroyPipeline, pipeline);
        RELEASE(vkDestroyFramebuffer, framebuffer);
        RELEASE(vkDestroyRenderPass, pass);
        RELEASE(vkDestroyImageView, view);
        RELEASE(vkDestroyImage, image);
        RELEASE(vkDestroyBuffer, vertex_buffer);
        RELEASE(vkFreeMemory, vertex_memory);
        RELEASE(vkFreeMemory, image_memory);
        RELEASE(vkDestroyPipelineLayout, layout);
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
        return 44;
    if (seen.buffers_released != seen.buffers)
        return 45;
    if (seen.drm_ioctls || seen.drm_listings || seen.drm_syncobjs)
        return 46;
    report(say, "VK4-a teardown: every object, the device and the instance destroyed; %u of %u native buffers "
           "and the heap released, only the session's group left; no Panthor ioctl, DRM listing or syncobj",
           seen.buffers_released, seen.buffers);
    return 0;
}

#ifdef KUMO_VK_TRIANGLE_MAIN
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_vk_triangle_cases(api, kumo_metal_log);
}

int main(void)
{
    kumo_metal_log("VK4-a metal triangle begin: VK4's pinned vertex/fragment SPIR-V and one render pass through panvk");
    const unsigned failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "VK4-a metal triangle FAILED at case %u", failed);
        return 1;
    }
    kumo_metal_log("VK4-a metal triangle green: Mali rasterized the Vulkan triangle exactly into the linear image, "
                   "all 32768 pixels and the guard checked, and clean teardown");
    return 0;
}
#endif
