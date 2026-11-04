#version 450
// SPDX-License-Identifier: MIT
// VK4 (PLAN/015 §8.2): the first Vulkan triangle's vertex stage. Position and color come
// from one vertex buffer; the color is equal at all three vertices, so the varying is exact.
// vk-spirv.py turns this into vk-triangle-vert.spv.h; panvk compiles the SPIR-V at runtime.
layout(location = 0) in vec2 position;
layout(location = 1) in vec4 color;

layout(location = 0) out vec4 shade;

void main()
{
    shade = color;
    gl_Position = vec4(position, 0.0, 1.0);
}
