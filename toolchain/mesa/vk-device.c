/* SPDX-License-Identifier: MIT */
/*j780*/
/* VK2-e (PLAN/015 §8.2): the static-ICD Vulkan client, with no loader and no WSI. Every entry
 * point comes from vk_icdGetInstanceProcAddr. The session's kmod device is offered to panvk
 * (VK2-b), and the client checks, in order:
 * - one instance, enumerating exactly one physical device: the Mali-G610, with KUMO's one
 *   queue, the session-budget heap and no FD or DRM extension;
 * - a device whose queue runs panvk's three subqueue init streams as three native
 *   submissions on the three-queue group (VK2-d);
 * - fence and emulated-timeline round trips (VK2-c), an idle queue and an idle device;
 * - teardown that leaves only the session's group, after which a second instance enumerates
 *   no device.
 * The native API is observed in place, and three libdrm entries are wrapped at link time, so
 * any Panthor ioctl, DRM device listing or syncobj creation fails the proof. */
#include "kumo_kmod.h"
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <vulkan/vulkan_core.h>
#include "vk-memory-policy.h"

PFN_vkVoidFunction vk_icdGetInstanceProcAddr(VkInstance instance, const char *name);
bool panvk_kumo_offer_device(struct pan_kmod_dev *kmod);
unsigned kumo_vk_device_cases(const struct kumo_gpu_api *api, void (*say)(const char *));

#define CHECK(test, code) do { if (!(test)) { failed = (code); goto done; } } while (0)

static void report(void (*say)(const char *), const char *format, ...)
{
    char line[240];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof line, format, args);
    va_end(args);
    say(line);
}

/* Every native request passes through here on its way to the session owner. */
static struct kumo_gpu_api tracked;
static uint32_t (*real_native)(void *, const struct kumo_gpu_native_request *, struct kumo_gpu_native_reply *);
static uint32_t (*real_heap_create)(void *, const struct kumo_gpu_heap_request *, struct kumo_gpu_heap_info *);
static uint32_t (*real_heap_destroy)(void *, uint32_t, uint64_t *);
static struct {
    unsigned groups, group_queues, prepares, streams, steps, submitted, blocked;
    unsigned binaries, heaps, heaps_released, refused;
    unsigned stream_queue[KUMO_NATIVE_MAX_STREAMS];
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
        ++seen.binaries;
        ++seen.live;
        break;
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
    case KUMO_NATIVE_PREPARE:
        ++seen.prepares;
        seen.streams += request->stream_count;
        for (uint32_t i = 0; i < request->stream_count && request->streams; ++i)
            if (request->streams[i].queue < KUMO_NATIVE_MAX_STREAMS)
                ++seen.stream_queue[request->streams[i].queue];
        ++seen.live;
        break;
    case KUMO_NATIVE_STEP:
        ++seen.steps;
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

static bool has_extension(const VkExtensionProperties *list, uint32_t count, const char *name)
{
    for (uint32_t i = 0; i < count; ++i)
        if (!strcmp(list[i].extensionName, name))
            return true;
    return false;
}

#define INSTANCE_FN(name) PFN_##name name = (PFN_##name)vk_icdGetInstanceProcAddr(instance, #name)
#define DEVICE_FN(name) PFN_##name name = (PFN_##name)get_device_proc(device, #name)

unsigned kumo_vk_device_cases(const struct kumo_gpu_api *api, void (*say)(const char *))
{
    static const char *const absent[] = {
        "VK_KHR_external_memory_fd", "VK_KHR_external_semaphore_fd", "VK_KHR_external_fence_fd",
        "VK_EXT_external_memory_dma_buf", "VK_EXT_physical_device_drm",
        "VK_ARM_scheduling_controls", "VK_KHR_swapchain",
    };
    static VkExtensionProperties extensions[512];
    unsigned failed = 0;
    VkInstance instance = VK_NULL_HANDLE;
    VkDevice device = VK_NULL_HANDLE;
    PFN_vkDestroyInstance destroy_instance = NULL;
    PFN_vkDestroyDevice destroy_device = NULL;

    memset(&seen, 0, sizeof seen);
    tracked = *api;
    real_native = api->native;
    real_heap_create = api->heap_create;
    real_heap_destroy = api->heap_destroy;
    tracked.native = observe_native;
    tracked.heap_create = observe_heap_create;
    tracked.heap_destroy = observe_heap_destroy;

    struct pan_kmod_dev *kmod = pan_kmod_kumo_dev_create(&tracked, NULL);
    if (!kmod)
        return 1;
    if (panvk_kumo_offer_device(NULL) || !panvk_kumo_offer_device(kmod)) {
        pan_kmod_dev_destroy(kmod);
        return 2;
    }
    /* panvk owns kmod from here on. A second offer while one is untaken is refused. */
    if (panvk_kumo_offer_device(kmod))
        return 3;

    PFN_vkCreateInstance create_instance =
        (PFN_vkCreateInstance)vk_icdGetInstanceProcAddr(VK_NULL_HANDLE, "vkCreateInstance");
    CHECK(create_instance, 4);
    const VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "kumo-vk2-e",
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
    INSTANCE_FN(vkGetPhysicalDeviceFeatures);
    INSTANCE_FN(vkGetPhysicalDeviceQueueFamilyProperties);
    INSTANCE_FN(vkGetPhysicalDeviceMemoryProperties2);
    INSTANCE_FN(vkEnumerateDeviceExtensionProperties);
    INSTANCE_FN(vkCreateDevice);
    INSTANCE_FN(vkGetDeviceProcAddr);
    CHECK(destroy_instance && vkEnumeratePhysicalDevices && vkGetPhysicalDeviceProperties &&
          vkGetPhysicalDeviceFeatures && vkGetPhysicalDeviceQueueFamilyProperties &&
          vkGetPhysicalDeviceMemoryProperties2 && vkEnumerateDeviceExtensionProperties &&
          vkCreateDevice && vkGetDeviceProcAddr, 6);
    say("VK2-e instance: created through vk_icdGetInstanceProcAddr, no loader, no WSI");

    uint32_t count = 0;
    VkPhysicalDevice physical[2];
    CHECK(vkEnumeratePhysicalDevices(instance, &count, NULL) == VK_SUCCESS && count == 1, 7);
    CHECK(vkEnumeratePhysicalDevices(instance, &count, physical) == VK_SUCCESS && count == 1, 8);
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(physical[0], &props);
    CHECK(props.vendorID == 0x13b5 && props.deviceType == VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU &&
          strstr(props.deviceName, "Mali-G610"), 9);
    report(say, "VK2-e physical device: exactly one, %s, Vulkan %u.%u.%u, vendor 0x%x device 0x%x",
           props.deviceName, VK_API_VERSION_MAJOR(props.apiVersion),
           VK_API_VERSION_MINOR(props.apiVersion), VK_API_VERSION_PATCH(props.apiVersion),
           props.vendorID, props.deviceID);

    VkQueueFamilyProperties families[4];
    count = 4;
    vkGetPhysicalDeviceQueueFamilyProperties(physical[0], &count, families);
    const VkQueueFlags gpu = VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT | VK_QUEUE_TRANSFER_BIT;
    CHECK(count == 1 && families[0].queueCount == 1 && (families[0].queueFlags & gpu) == gpu &&
          !(families[0].queueFlags & VK_QUEUE_SPARSE_BINDING_BIT), 10);
    VkPhysicalDeviceFeatures features;
    vkGetPhysicalDeviceFeatures(physical[0], &features);
    CHECK(!features.sparseBinding, 11);

    VkPhysicalDeviceMemoryBudgetPropertiesEXT budget = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_BUDGET_PROPERTIES_EXT,
    };
    VkPhysicalDeviceMemoryProperties2 memory = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MEMORY_PROPERTIES_2,
        .pNext = &budget,
    };
    vkGetPhysicalDeviceMemoryProperties2(physical[0], &memory);
    CHECK(memory.memoryProperties.memoryHeapCount == 1 &&
          memory.memoryProperties.memoryHeaps[0].size == KUMO_GPU_SESSION_BUDGET_BYTES &&
          budget.heapBudget[0] == KUMO_GPU_SESSION_BUDGET_BYTES, 12);
    CHECK(memory.memoryProperties.memoryTypeCount > 0, 43);
    CHECK(kumo_vk_memory_types_valid(&memory.memoryProperties), 44);
    report(say, "VK2-e queues and memory: one family of one queue (graphics, compute, transfer), "
           "no sparse binding; one %llu MiB heap with that budget",
           (unsigned long long)(KUMO_GPU_SESSION_BUDGET_BYTES >> 20));

    count = 0;
    CHECK(vkEnumerateDeviceExtensionProperties(physical[0], NULL, &count, NULL) == VK_SUCCESS &&
          count <= sizeof extensions / sizeof extensions[0], 13);
    CHECK(vkEnumerateDeviceExtensionProperties(physical[0], NULL, &count, extensions) == VK_SUCCESS, 14);
    for (unsigned i = 0; i < sizeof absent / sizeof absent[0]; ++i)
        CHECK(!has_extension(extensions, count, absent[i]), 15);
    CHECK(has_extension(extensions, count, "VK_KHR_timeline_semaphore"), 16);
    report(say, "VK2-e extensions: %u device extensions, none for FDs, dma-bufs, DRM, core control or "
           "swapchains; VK_KHR_timeline_semaphore present", count);

    const float priority = 1.0f;
    const VkDeviceQueueCreateInfo queue_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = 0,
        .queueCount = 1,
        .pQueuePriorities = &priority,
    };
    VkPhysicalDeviceTimelineSemaphoreFeatures timeline_features = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_TIMELINE_SEMAPHORE_FEATURES,
        .timelineSemaphore = VK_TRUE,
    };
    const char *const enabled[] = {"VK_KHR_timeline_semaphore"};
    const VkDeviceCreateInfo device_info = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .pNext = &timeline_features,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &queue_info,
        .enabledExtensionCount = 1,
        .ppEnabledExtensionNames = enabled,
    };
    CHECK(vkCreateDevice(physical[0], &device_info, NULL, &device) == VK_SUCCESS && device, 17);
    PFN_vkGetDeviceProcAddr get_device_proc = vkGetDeviceProcAddr;
    destroy_device = (PFN_vkDestroyDevice)get_device_proc(device, "vkDestroyDevice");
    DEVICE_FN(vkGetDeviceQueue);
    DEVICE_FN(vkQueueWaitIdle);
    DEVICE_FN(vkDeviceWaitIdle);
    DEVICE_FN(vkQueueSubmit);
    DEVICE_FN(vkCreateFence);
    DEVICE_FN(vkDestroyFence);
    DEVICE_FN(vkWaitForFences);
    DEVICE_FN(vkResetFences);
    DEVICE_FN(vkGetFenceStatus);
    DEVICE_FN(vkCreateSemaphore);
    DEVICE_FN(vkDestroySemaphore);
    DEVICE_FN(vkSignalSemaphoreKHR);
    DEVICE_FN(vkWaitSemaphoresKHR);
    DEVICE_FN(vkGetSemaphoreCounterValueKHR);
    CHECK(destroy_device && vkGetDeviceQueue && vkQueueWaitIdle && vkDeviceWaitIdle && vkQueueSubmit &&
          vkCreateFence && vkDestroyFence && vkWaitForFences && vkResetFences && vkGetFenceStatus &&
          vkCreateSemaphore && vkDestroySemaphore && vkSignalSemaphoreKHR && vkWaitSemaphoresKHR &&
          vkGetSemaphoreCounterValueKHR, 18);
    /* Device creation alone: the group, the heap, and one init stream per subqueue. */
    CHECK(seen.groups == 1 && seen.group_queues == 1 && seen.heaps == 1, 19);
    CHECK(seen.prepares == 3 && seen.streams == 3 && seen.submitted == 3 && seen.blocked == 0 &&
          seen.stream_queue[0] == 1 && seen.stream_queue[1] == 1 && seen.stream_queue[2] == 1, 20);
    report(say, "VK2-e device: one three-queue native group, one tiler heap, and the three subqueue "
           "init streams as %u native submissions of one stream each", seen.submitted);

    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(device, 0, 0, &queue);
    CHECK(queue, 21);
    CHECK(vkQueueWaitIdle(queue) == VK_SUCCESS, 22);

    VkFence fence = VK_NULL_HANDLE;
    const VkFenceCreateInfo fence_info = {
        .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
        .flags = VK_FENCE_CREATE_SIGNALED_BIT,
    };
    CHECK(vkCreateFence(device, &fence_info, NULL, &fence) == VK_SUCCESS, 23);
    CHECK(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX) == VK_SUCCESS, 24);
    CHECK(vkResetFences(device, 1, &fence) == VK_SUCCESS && vkGetFenceStatus(device, fence) == VK_NOT_READY, 25);
    CHECK(vkQueueSubmit(queue, 0, NULL, fence) == VK_SUCCESS, 26);
    CHECK(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX) == VK_SUCCESS &&
          vkGetFenceStatus(device, fence) == VK_SUCCESS, 27);
    say("VK2-e fence: signaled at creation, reset, then signaled again by an empty submission");

    VkSemaphoreTypeCreateInfo type_info = {
        .sType = VK_STRUCTURE_TYPE_SEMAPHORE_TYPE_CREATE_INFO,
        .semaphoreType = VK_SEMAPHORE_TYPE_TIMELINE,
        .initialValue = 0,
    };
    const VkSemaphoreCreateInfo semaphore_info = {
        .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO,
        .pNext = &type_info,
    };
    VkSemaphore timeline = VK_NULL_HANDLE;
    uint64_t value = ~0ull;
    CHECK(vkCreateSemaphore(device, &semaphore_info, NULL, &timeline) == VK_SUCCESS, 28);
    CHECK(vkGetSemaphoreCounterValueKHR(device, timeline, &value) == VK_SUCCESS && value == 0, 29);
    const VkSemaphoreSignalInfo signal_info = {
        .sType = VK_STRUCTURE_TYPE_SEMAPHORE_SIGNAL_INFO,
        .semaphore = timeline,
        .value = 5,
    };
    CHECK(vkSignalSemaphoreKHR(device, &signal_info) == VK_SUCCESS, 30);
    const uint64_t five = 5, seven = 7;
    const VkSemaphoreWaitInfo wait_info = {
        .sType = VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO,
        .semaphoreCount = 1,
        .pSemaphores = &timeline,
        .pValues = &five,
    };
    CHECK(vkWaitSemaphoresKHR(device, &wait_info, UINT64_MAX) == VK_SUCCESS, 31);
    /* An empty submission that waits for 5 and signals 7: a point of the emulated timeline,
     * signaled once everything before it on the queue has completed. */
    const VkTimelineSemaphoreSubmitInfo timeline_submit = {
        .sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
        .waitSemaphoreValueCount = 1,
        .pWaitSemaphoreValues = &five,
        .signalSemaphoreValueCount = 1,
        .pSignalSemaphoreValues = &seven,
    };
    const VkPipelineStageFlags stage = VK_PIPELINE_STAGE_ALL_COMMANDS_BIT;
    const VkSubmitInfo submit = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .pNext = &timeline_submit,
        .waitSemaphoreCount = 1,
        .pWaitSemaphores = &timeline,
        .pWaitDstStageMask = &stage,
        .signalSemaphoreCount = 1,
        .pSignalSemaphores = &timeline,
    };
    CHECK(vkResetFences(device, 1, &fence) == VK_SUCCESS, 32);
    CHECK(vkQueueSubmit(queue, 1, &submit, fence) == VK_SUCCESS, 33);
    CHECK(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX) == VK_SUCCESS, 34);
    CHECK(vkGetSemaphoreCounterValueKHR(device, timeline, &value) == VK_SUCCESS && value == 7, 35);
    say("VK2-e timeline: host signal to 5, then an empty submission waiting for 5 and signaling 7");

    CHECK(vkQueueWaitIdle(queue) == VK_SUCCESS && vkDeviceWaitIdle(device) == VK_SUCCESS, 36);
    /* The empty submissions are met on the CPU: no further native submission. The session is
     * live until teardown closes it with the kmod device. */
    CHECK(seen.prepares == 3 && seen.refused == 0, 37);
    CHECK(api->device_status(api->context) == KUMO_GPU_OK, 42);
    vkDestroySemaphore(device, timeline, NULL);
    vkDestroyFence(device, fence, NULL);
    say("VK2-e idle: queue and device idle, with no native submission beyond the init streams");

done:
    if (device && destroy_device)
        destroy_device(device, NULL);
    device = VK_NULL_HANDLE;
    if (instance && destroy_instance)
        destroy_instance(instance, NULL);
    instance = VK_NULL_HANDLE;
    if (failed)
        return failed;
    /* Teardown keeps only the session's group, which lasts until the session closes. */
    if (seen.heaps_released != 1 || seen.live != 1)
        return 38;
    report(say, "VK2-e teardown: device and instance destroyed, the heap released, %u native binaries "
           "created and every native object dropped but the session's group", seen.binaries);

    /* The offered device is gone with the first instance: a second enumerates nothing. */
    CHECK(create_instance(&instance_info, NULL, &instance) == VK_SUCCESS && instance, 39);
    destroy_instance = (PFN_vkDestroyInstance)vk_icdGetInstanceProcAddr(instance, "vkDestroyInstance");
    {
        INSTANCE_FN(vkEnumeratePhysicalDevices);
        count = 1;
        CHECK(vkEnumeratePhysicalDevices && vkEnumeratePhysicalDevices(instance, &count, NULL) == VK_SUCCESS &&
              count == 0, 40);
    }
    destroy_instance(instance, NULL);
    instance = VK_NULL_HANDLE;
    CHECK(seen.drm_ioctls == 0 && seen.drm_listings == 0 && seen.drm_syncobjs == 0, 41);
    say("VK2-e second instance: no physical device; no Panthor ioctl, DRM device listing or syncobj at any point");
    return 0;
}

#ifdef KUMO_VK_DEVICE_MAIN
extern void kumo_metal_log(const char *line);
extern unsigned kumo_metal_proof(void);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api);
unsigned kumo_metal_cases(const struct kumo_gpu_api *api)
{
    return kumo_vk_device_cases(api, kumo_metal_log);
}

int main(void)
{
    kumo_metal_log("VK2-e metal device begin: panvk through vk_icdGetInstanceProcAddr on the native session");
    const unsigned failed = kumo_metal_proof();
    if (failed) {
        report(kumo_metal_log, "VK2-e metal device FAILED at case %u", failed);
        return 1;
    }
    kumo_metal_log("VK2-e metal device green: one instance, the Mali-G610, a device on the three-queue group, "
                   "an idle queue and clean teardown");
    return 0;
}
#endif
