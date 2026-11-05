#version 450
// SPDX-License-Identifier: MIT
// VK5 (PLAN/015 §8.46): one triangle covering the viewport, placed by gl_VertexIndex, with no
// vertex buffer: (-1,-1), (3,-1), (-1,3). GL4's vertex stage in Vulkan GLSL.
// vk-spirv.py turns this into vk-churn-vert.spv.h; panvk compiles the SPIR-V at runtime.
void main()
{
    vec2 corner = vec2(float((gl_VertexIndex & 1) * 4) - 1.0, float((gl_VertexIndex & 2) * 2) - 1.0);
    gl_Position = vec4(corner, 0.0, 1.0);
}
