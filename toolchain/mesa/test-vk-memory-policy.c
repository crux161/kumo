/* SPDX-License-Identifier: MIT */
/*j780*/
#include "vk-memory-policy.h"
#include <assert.h>
#include <stdio.h>

int main(void)
{
    const VkMemoryPropertyFlags host = VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT |
        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    VkPhysicalDeviceMemoryProperties memory = {
        .memoryTypeCount = 2,
        .memoryTypes = {
            {.propertyFlags = VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT},
            {.propertyFlags = host},
        },
    };
    /* The real panvk layout that exposed the overly broad check: a legal device-only
     * type before an uncached, coherent host-visible type. — TIMBERDOODLE */
    assert(kumo_vk_memory_types_valid(&memory));
    memory.memoryTypes[1].propertyFlags = host | VK_MEMORY_PROPERTY_HOST_CACHED_BIT;
    assert(!kumo_vk_memory_types_valid(&memory));
    memory.memoryTypes[1].propertyFlags = host & ~VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    assert(!kumo_vk_memory_types_valid(&memory));
    memory.memoryTypes[1].propertyFlags = host;
    memory.memoryTypes[2].propertyFlags = host;
    memory.memoryTypeCount = 3;
    assert(kumo_vk_memory_types_valid(&memory));
    memory.memoryTypes[2].propertyFlags |= VK_MEMORY_PROPERTY_HOST_CACHED_BIT;
    assert(!kumo_vk_memory_types_valid(&memory));
    memory.memoryTypeCount = 1;
    assert(!kumo_vk_memory_types_valid(&memory));
    memory.memoryTypeCount = 0;
    assert(!kumo_vk_memory_types_valid(&memory));
    memory.memoryTypeCount = VK_MAX_MEMORY_TYPES + 1;
    assert(!kumo_vk_memory_types_valid(&memory));
    assert(!kumo_vk_memory_types_valid(NULL));
    puts("VK2 host memory policy green: device-only allowed; host-visible coherent and uncached required");
    return 0;
}
