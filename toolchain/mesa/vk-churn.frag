#version 450
// SPDX-License-Identifier: MIT
// VK5 (PLAN/015 §8.46): copy texel (x, y) of the frame's texture to pixel (x, y). Vulkan's
// fragment origin is top-left, so image row y lands on target row y.
// vk-spirv.py turns this into vk-churn-frag.spv.h; panvk compiles the SPIR-V at runtime.
layout(set = 0, binding = 0) uniform sampler2D storage;

layout(location = 0) out vec4 fragment;

void main()
{
    fragment = texelFetch(storage, ivec2(gl_FragCoord.xy), 0);
}
