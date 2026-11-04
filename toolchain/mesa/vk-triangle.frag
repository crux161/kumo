#version 450
// SPDX-License-Identifier: MIT
// VK4 (PLAN/015 §8.2): the first Vulkan triangle's fragment stage writes the vertex color.
// vk-spirv.py turns this into vk-triangle-frag.spv.h; panvk compiles the SPIR-V at runtime.
layout(location = 0) in vec4 shade;

layout(location = 0) out vec4 fragment;

void main()
{
    fragment = shade;
}
