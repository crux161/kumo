/* SPDX-License-Identifier: MIT */
/*j780*/
#ifndef KUMO_VK_MEMORY_POLICY_H
#define KUMO_VK_MEMORY_POLICY_H

#include <stdbool.h>
#include <vulkan/vulkan_core.h>

/* Native GPU CPU maps are Normal-NC. Device-only memory needs no host coherency;
 * every host-visible type must be coherent and uncached. — TIMBERDOODLE */
static inline bool
kumo_vk_memory_types_valid(const VkPhysicalDeviceMemoryProperties *memory)
{
    if (!memory || !memory->memoryTypeCount || memory->memoryTypeCount > VK_MAX_MEMORY_TYPES)
        return false;
    bool host_visible = false;
    for (uint32_t i = 0; i < memory->memoryTypeCount; ++i) {
        const VkMemoryPropertyFlags flags = memory->memoryTypes[i].propertyFlags;
        if (!(flags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT))
            continue;
        host_visible = true;
        if (!(flags & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT) ||
            (flags & VK_MEMORY_PROPERTY_HOST_CACHED_BIT))
            return false;
    }
    return host_visible;
}

#endif
