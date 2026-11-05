#!/usr/bin/env python3
#j782
#j784
#j785
#j787
#j788
"""Build live Mesa device and static GLES clients, stage them in the Orange Pi GPU slot,
and check a serial log of the hardware run.

  --proof NAME  select the native device, sync, heap, GL lifecycle, query or compute client.
  --stage DIR   rebuild the Orange Pi image with KUMO_CONSOLE_LOGLEVEL=5, then copy it to DIR
                (which must not exist) with only bin/gpu-client replaced. A proof that needs a
                presentation lane (GL3-b) also adds the etc/gpu-lane marker, and nothing else
                differs. Never installs or serves it; that is the operator's pxehost step.
                Rebuild the default image after the metal run.
  --check LOG   check a tio log of the board's boot for the proof's live lines.

Since J748 the default console shows WARNING and worse. The proof lines are NOTICE, so a
default image would boot the proof without printing it live. A boot whose begin line never
appears fails --check.

No download, image installation or hardware reset. See toolchain/mesa/README.md.
"""
import argparse
import hashlib
import importlib.util
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / "toolchain/mesa"
IMAGE = ROOT / "build/images/orange-pi-5-plus"
SLOT = b"bin/gpu-client"
# PLAN/015 §8.32 D1: Sora reads this marker to pick the GPU client's lane. Default images
# carry none; only a proof stage adds it, beside the slot. kumo_abi::initrd::GPU_LANE_PATH.
LANE = b"etc/gpu-lane"
# kumo_abi::initrd::INITRD_MAX_ENTRIES: the loader's fixed table bound.
MAX_ENTRIES = 48
# kumo_abi::initrd::AUTOEXEC_PATH. Sora runs it before it launches the GPU client (RE4).
AUTOEXEC = b"etc/autoexec"
# Per proof: the build define, its output, and the live lines that must each appear exactly
# once, in this order. Driver lines interleave with the client's, so `driver` lines must each
# appear once between `after` and the green line, in any order, and `absent` never in that
# window. — TIMBERDOODLE
SYNC = ["G7p-e device: gpu_id=", "G7p-e group: native group on VM", "G7p-e A: point 1 submitted natively",
        "G7p-e B: point 2 captured A's unobserved fence", "G7p-e ARM: points 1 and 2 complete"]
# GL3-a's body lines; GL3-b runs the same body before its copy. The target and layout lines
# end in the measured color modifier, so only their fixed prefixes are required. — TIMBERDOODLE
GL3A = ["GL3-a context: current GLES context over the native GPU session",
        "GL3-a target: complete owned 256x128 RGBA8 frontend framebuffer, color modifier 0x",
        "GL3-a compiler: GLSL ES 300 vertex and fragment sources compiled and linked at runtime",
        "GL3-a baseline: public glReadPixels observed an image different from the expected triangle",
        "GL3-a draw: glClear and one glDrawArrays triangle, native glFinish complete",
        "GL3-a readback: 0 of 32768 pixels differ; 0 of 32 readback guards changed (Mali triangle 0xff6633cc over clear 0xff996633)",
        "GL3-a coverage: 16384 pixels inside x+2y<=254; samples (0,0)=0xff6633cc (254,0)=0xff6633cc "
        "(255,0)=0xff996633 (0,127)=0xff6633cc (255,127)=0xff996633",
        "GL3-a layout: color modifier after readback 0x",
        "GL3-a destroy: shaders, program, vertex buffer, context, drawable and display released"]
# GL3-c (PLAN/015 §8.32, D3; G7q-d): sixteen layers of 32768 tiny triangles on a one-chunk,
# max-two heap. The samples are exact: R = x, G = 2y, and blue (the last layer's) and alpha
# (the sum over layers) both 15 x 16 = 240.
GL3C = ["GL3-c heap config: pan_csf_chunk_size=262144 pan_csf_initial_chunks=1 pan_csf_max_chunks=2 set before screen creation (proof-only overrides)",
        "GL3-c context: current GLES context over the native GPU session",
        "GL3-c target: complete owned 256x128 RGBA8 frontend framebuffer, color modifier 0x",
        "GL3-c compiler: GLSL ES 300 vertex and fragment sources compiled and linked at runtime",
        "GL3-c baseline: public glReadPixels observed an image different from the expected one",
        "GL3-c draw: glClear and one glDrawElementsInstanced of 16 layers x 32768 triangles (1x2-pixel cells), native glFinish complete",
        "GL3-c readback: 0 of 32768 pixels differ; 0 of 32 readback guards changed (Mali: last layer's RGB, alpha summed over all 16 layers)",
        "GL3-c samples: (0,0)=0xf0f00000 (255,0)=0xf0f000ff (0,127)=0xf0f0fe00 (255,127)=0xf0f0feff",
        "GL3-c destroy: shaders, program, index buffer, context, drawable and display released"]
# VK4-a's body lines; VK4-b runs the same body before its copy. The target line ends in the
# measured row pitch and size, so only its prefix is required. — TIMBERDOODLE
VK4A = ["VK4-a device: Mali-G610",
        "through the static ICD, one three-queue native group and 3 init submissions",
        "VK4-a target: 256x128 R8G8B8A8_UNORM linear image, row pitch ",
        "poisoned to 0xdeadbeef with a 4096-byte guard past it",
        "VK4-a pipeline: the pinned 243-word vertex and 94-word fragment SPIR-V compiled by panvk on KUMO "
        "into a graphics pipeline (no pipeline cache), with no native submission",
        "VK4-a record: render pass clear to 0xff996633, one vkCmdDraw of 3 vertices (shade 0xff6633cc), "
        "color-to-host barrier",
        # Measured on metal (J784): panvk's first draw adds its one-time REQ_RESOURCE streams on
        # the vertex/tiler and fragment subqueues ahead of the command buffer.
        "VK4-a submit: one fenced vkQueueSubmit as 2 native submissions of 5 streams: subqueues [0 and 1] "
        "then [0, 1 and 2]; the fence completed",
        "VK4-a readback: 0 of 32768 pixels differ; 0 of 1024 guard words changed (Mali triangle 0xff6633cc "
        "over clear 0xff996633)",
        "VK4-a coverage: 16384 pixels inside x+2y<=254; samples (0,0)=0xff6633cc (254,0)=0xff6633cc "
        "(255,0)=0xff996633 (0,127)=0xff6633cc (255,127)=0xff996633",
        "VK4-a teardown: every object, the device and the instance destroyed; "]
PROOFS = {
    # GL4 (PLAN/015 §8.2): G8's metal gate through public GL, on the churn lane. Sora's
    # RestartBudget(3) regrants after each exit, so one boot holds up to four sessions; the
    # boot session runs the GL-fed J0c (CRUX, 2026-11-03) that arms drv-fb's present gate.
    # check_sessions() owns this proof's acceptance.
    "gl4": dict(
        cases="gl-churn", runtime="device-metal", defines=["-DKUMO_GL_CHURN_MAIN=1"],
        out="gl4-metal", gl=True, marker=b"churn\n", sessions=True, begin="GL4 metal churn begin",
        # RE4: the census needs the DEBUG console. CRUX could not type `dmesg -n debug` in
        # time past the boot demos, so the stage appends it to autoexec, which Sora runs
        # before the first GPU session. — TIMBERDOODLE
        autoexec=b"dmesg -n debug\n",
        boot=["sora: gpu-lane marker selects the churn lane"],
        first=["GL4 lane: view 1 over V, S, N with the boot session's native producer",
               "GL4 J0c sequence1 retired after drv-fb's consumed load",
               "GL4 J0c sequence2 retired after drv-fb's consumed load",
               "GL4 J0c sequence3 retired after drv-fb's consumed load",
               "GL4 J0c complete: 3 GL-rendered frames through one VMO, each rewritten only after its retirement",
               "GL4 J0c: three exact GL frames presented through the native lease, each retired before the next"],
        later=["GL4 lane: view 1 over V, S, N; a regranted session with no native producer"],
        each=["GL4 context: current GLES context and an owned 256x160 RGBA8 framebuffer over the native GPU session",
              "GL4 readiness: the first Retired after ",
              "GL4 frames: 100 frames each created, uploaded, drew with and deleted one 160 KiB texture (16000 KiB in all); "
              "0 of 4096000 pixels differ (Mali: every texel copied exactly)",
              "GL4 destroy: program, context, drawable and display released",
              "GL4 presents: 100 frames on view 1, retired=100 copied=0"],
        display=["display: J0c complete: 3 original-VMO presentations retired",
                 "display: present gate armed, answering Retired on frame starts"],
        green=("GL4 metal churn green: 100 exact GL frames of texture churn presented on view 1 and clean teardown; "
               "the presents line carries the Retired/Copied record"),
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    # VK2-e (PLAN/015 §8.2): panvk through its static ICD entry, with no loader and no WSI. The
    # client counts every native request and wraps drmIoctl, drmGetDevices2 and
    # drmSyncobjCreate: one three-queue group, one heap (panvk's 5 x 2 MiB, max 64) and
    # exactly three single-stream submissions (the subqueue init streams), with no DRM call
    # at all. Empty submissions are met on the CPU. drv-gpu's own lines prove the group and
    # heap; the heap line's head address is not predicted.
    "vk2-e": dict(
        cases="vk-device", runtime="device-metal", defines=["-DKUMO_VK_DEVICE_MAIN=1"],
        out="vk2-e-metal", vk=True, begin="VK2-e metal device begin",
        lines=["VK2-e instance: created through vk_icdGetInstanceProcAddr, no loader, no WSI",
               "VK2-e physical device: exactly one, Mali-G610",
               "VK2-e queues and memory: one family of one queue (graphics, compute, transfer), no sparse binding; "
               "one 128 MiB heap with that budget",
               " device extensions, none for FDs, dma-bufs, DRM, core control or swapchains; "
               "VK_KHR_timeline_semaphore present",
               "VK2-e device: one three-queue native group, one tiler heap, and the three subqueue init streams as "
               "3 native submissions of one stream each",
               "VK2-e fence: signaled at creation, reset, then signaled again by an empty submission",
               "VK2-e timeline: host signal to 5, then an empty submission waiting for 5 and signaling 7",
               "VK2-e idle: queue and device idle, with no native submission beyond the init streams",
               "VK2-e teardown: device and instance destroyed, the heap released, ",
               "VK2-e second instance: no physical device; no Panthor ioctl, DRM device listing or syncobj at any point"],
        green=("VK2-e metal device green: one instance, the Mali-G610, a device on the three-queue group, "
               "an idle queue and clean teardown"),
        driver=["drv-gpu: client group 1 leases CSG0 queues 0-2",
                "drv-gpu: client heap 1 context 0x30000000 chunks 5x2097152 head 0x"],
        absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    # VK5-b (PLAN/015 §8.46, D2a/D3a): G8's bar through panvk, on GL4's churn lane. Session 1
    # (the boot session, with drv-fb's native producer) runs the Vulkan-fed J0c that arms the
    # present gate; Sora's RestartBudget(3) regrants the later sessions. J0c's frames spend
    # panvk's first-use resource requests in session 1, so its churn frame 1 may split
    # differently; `each` requires only the session-independent parts. check_sessions owns
    # this proof's acceptance. — TIMBERDOODLE
    "vk5-b": dict(
        cases="vk-churn", runtime="device-metal", defines=["-DKUMO_VK_CHURN_PRESENT_MAIN=1"],
        out="vk5-b-metal", vk=True, marker=b"churn\n", sessions=True, begin="VK5 metal churn begin",
        autoexec=b"dmesg -n debug\n",
        boot=["sora: gpu-lane marker selects the churn lane"],
        first=["VK5 lane: view 1 over V, S, N with the boot session's native producer",
               "VK5 J0c sequence1 retired after drv-fb's consumed load",
               "VK5 J0c sequence2 retired after drv-fb's consumed load",
               "VK5 J0c sequence3 retired after drv-fb's consumed load",
               "VK5 J0c complete: 3 Vulkan-rendered frames through one VMO, each rewritten only after its retirement",
               "VK5 J0c: three exact Vulkan frames presented through the native lease, each retired before the next"],
        later=["VK5 lane: view 1 over V, S, N; a regranted session with no native producer"],
        each=["VK5-a device: Mali-G610",
              "VK5 readiness: the first Retired after ",
              "VK5-a texture: each frame's 256x160 optimal R8G8B8A8 image took 266240 bytes of its own memory; "
              "panvk's AFBC predicate says yes, so the copy ran vk_meta's graphics path",
              "; frames 2-100 each as 1 ([0, 1 and 2]); ",
              "; frames 2-100 each created 1 and freed 1; ",
              "VK5-a frames: 100 frames each created, uploaded, drew with and destroyed one 160 KiB texture "
              "(16000 KiB in all); 0 of 4096000 pixels differ; 0 guard words changed (Mali: every texel copied "
              "exactly)",
              "VK5-a teardown: every object, the device and the instance destroyed; ",
              "VK5 presents: 100 frames on view 1, retired=100 copied=0"],
        display=["display: J0c complete: 3 original-VMO presentations retired",
                 "display: present gate armed, answering Retired on frame starts"],
        green=("VK5 metal churn green: 100 exact Vulkan frames of texture churn presented on view 1 and clean "
               "teardown; the presents line carries the Retired/Copied record"),
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    # VK5-a (PLAN/015 §8.46, D1a): 100 frames of Vulkan texture churn, offscreen; the texture,
    # submits and storage lines pin what metal measured. — TIMBERDOODLE
    "vk5-a": dict(
        cases="vk-churn", runtime="device-metal", defines=["-DKUMO_VK_CHURN_MAIN=1"],
        out="vk5-a-metal", vk=True, begin="VK5-a metal churn begin",
        lines=["VK5-a device: Mali-G610",
               "through the static ICD, one three-queue native group and 3 init submissions",
               "VK5-a setup: the pinned 285-word vertex and 170-word texelFetch fragment SPIR-V compiled by panvk "
               "into a graphics pipeline; a 256x160 linear target (row pitch ",
               # Measured on metal (J787): the AFBC image takes 266240 bytes (texels, headers, alignment),
               # its copy runs vk_meta's graphics path, the first frame adds the subqueue 0/1 resource
               # requests, and each later frame allocates and frees exactly its texture's memory.
               "VK5-a texture: each frame's 256x160 optimal R8G8B8A8 image took 266240 bytes of its own memory; "
               "panvk's AFBC predicate says yes, so the copy ran vk_meta's graphics path",
               "VK5-a submits: frame 1 as 2 native submissions ([0 and 1] then [0, 1 and 2]); frames 2-100 each as 1 "
               "([0, 1 and 2]); 101 in all",
               "VK5-a storage: frame 1 created 5 and freed 1 native buffers; frames 2-100 each created 1 and freed 1; "
               "104 created during the churn",
               "VK5-a frames: 100 frames each created, uploaded, drew with and destroyed one 160 KiB texture "
               "(16000 KiB in all); 0 of 4096000 pixels differ; 0 guard words changed (Mali: every texel copied "
               "exactly)",
               "VK5-a teardown: every object, the device and the instance destroyed; "],
        green=("VK5-a metal churn green: 100 exact Vulkan frames, each creating, uploading, drawing with and "
               "destroying a 160 KiB texture, and clean teardown"),
        driver=["drv-gpu: client group 1 leases CSG0 queues 0-2",
                "drv-gpu: client heap 1 context 0x30000000 chunks 5x2097152 head 0x"],
        absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    # VK4-b (PLAN/015 §8.43): VK4-a's image copied into view 1 in Vulkan's orientation (D2: the
    # right angle top-left) and held on HDMI through GL3-b's lane. The stage adds the `view`
    # marker; as on GL3-b, this image runs no J0c, so the presents are Copied. — TIMBERDOODLE
    "vk4-b": dict(
        cases="vk-triangle", runtime="device-metal", defines=["-DKUMO_VK_PRESENT_MAIN=1"],
        out="vk4-b-metal", vk=True, marker=b"view\n", begin="VK4-b metal present begin",
        boot=["sora: gpu-lane marker selects the view lane"],
        lines=["VK4-b lane: view 1 channel and surface received over V, S, N; the native producer is not used",
               *VK4A,
               "VK4-b copy: 32768 RGBA8 pixels written to view 1's surface as top-down XRGB8888 in Vulkan's "
               "orientation in 512 VmoWrite chunks",
               "VK4-b present: 15 of 15 presents accepted with FULL damage; retired=",
               "VK4-b release: view 1 channel and surface closed"],
        green=("VK4-b metal present green: the exact Mali Vulkan triangle was copied into view 1 with its right "
               "angle top-left and every present was accepted; Copied is the gate without J0c"),
        tail=["gpu-client: client exited; view lane returns view 1 to Sora, no regrant"],
        driver=["drv-gpu: client group 1 leases CSG0 queues 0-2",
                "drv-gpu: client heap 1 context 0x30000000 chunks 5x2097152 head 0x"],
        absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    # VK4-a (PLAN/015 §8.43): GL3-a's triangle through panvk, a render pass into a linear image
    # read back from the CPU. Vulkan's origin is top-left, so the samples match GL3-a's.
    # — TIMBERDOODLE
    "vk4-a": dict(
        cases="vk-triangle", runtime="device-metal", defines=["-DKUMO_VK_TRIANGLE_MAIN=1"],
        out="vk4-a-metal", vk=True, begin="VK4-a metal triangle begin",
        lines=VK4A,
        green=("VK4-a metal triangle green: Mali rasterized the Vulkan triangle exactly into the linear image, "
               "all 32768 pixels and the guard checked, and clean teardown"),
        driver=["drv-gpu: client group 1 leases CSG0 queues 0-2",
                "drv-gpu: client heap 1 context 0x30000000 chunks 5x2097152 head 0x"],
        absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    # VK3-b (PLAN/015 §8.2): VK3-a's pinned SPIR-V compiled by panvk at runtime and dispatched
    # on Mali. The client counts every native request and buffer and wraps the same three libdrm
    # entries as VK2-e. The submit line names the measured native split of the one fenced
    # vkQueueSubmit; the teardown line ends in the measured buffer count, so only its prefix is
    # required. — TIMBERDOODLE
    "vk3-b": dict(
        cases="vk-compute", runtime="device-metal", defines=["-DKUMO_VK_COMPUTE_MAIN=1"],
        out="vk3-b-metal", vk=True, begin="VK3-b metal compute begin",
        lines=["VK3-b device: Mali-G610",
               "through the static ICD, one three-queue native group and 3 init submissions",
               "VK3-b pipeline: the pinned 308-word SPIR-V module compiled by panvk on KUMO into a compute "
               "pipeline (local size 64, no pipeline cache), with no native submission",
               "VK3-b buffer: 1088 storage words (",
               "uncached) poisoned to 0xdeadbeef from the CPU",
               "VK3-b record: two command buffers, each bind, push k (0x6b750000, then 0x6b760000) and count=1000, "
               "dispatch 16 workgroups of 64, compute-to-host barrier",
               "VK3-b submit 1: one fenced vkQueueSubmit as 2 native submissions of 4 streams: subqueues [2] then "
               "[0, 1 and 2]; the fence completed",
               "VK3-b readback 1: 0 of 1000 words differ from i*3+k; 0 of 24 invocations at or above count and 0 of "
               "64 guards changed (Mali: first=0x6b750000 last=0x6b750bb5)",
               "VK3-b submit 2: one fenced vkQueueSubmit as 1 native submission of 3 streams: subqueues [0, 1 and 2]; "
               "the fence completed",
               "VK3-b readback 2: 0 of 1000 words differ from i*3+k; 0 of 24 invocations at or above count and 0 of "
               "64 guards changed (Mali: first=0x6b760000 last=0x6b760bb5)",
               "VK3-b teardown: every object, the device and the instance destroyed; "],
        green=("VK3-b metal compute green: a runtime-compiled Vulkan compute pipeline wrote all 1000 storage "
               "words on Mali twice, the bound and guards intact, and clean teardown"),
        driver=["drv-gpu: client group 1 leases CSG0 queues 0-2",
                "drv-gpu: client heap 1 context 0x30000000 chunks 5x2097152 head 0x"],
        absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    # GL4-a (PLAN/015 §8.32 D4): public texture allocation against the session budget. The
    # 256 MiB refusal alone is not new (the old physical-memory preflight refused every
    # texture); the 160 KiB texture's creation, clear and exact readback are.
    "gl4-a": dict(
        cases="gl-texture", runtime="device-metal", defines=["-DKUMO_GL_TEXTURE_MAIN=1"],
        out="gl4-a-metal", gl=True, begin="GL4-a metal texture begin",
        lines=["GL4-a context: current GLES context over the native GPU session",
               "GL4-a preflight: an 8192x8192 RGBA8 texture (256 MiB) refused with GL_OUT_OF_MEMORY against the 128 MiB session budget",
               "GL4-a texture: a 256x160 RGBA8 texture (160 KiB) created and attached; framebuffer complete",
               "GL4-a baseline: public glReadPixels observed texture pixels different from the requested clear color",
               "GL4-a clear: public glClear into the texture and native glFinish complete, expected RGBA8 0xff996633",
               "GL4-a readback: 0 of 40960 pixels differ; 0 of 32 readback guards changed (Mali clear color 0xff996633)",
               "GL4-a destroy: texture, framebuffer, context, drawable and display released"],
        green=("GL4-a metal texture green: a 256 MiB texture refused by the budget preflight, a 160 KiB texture "
               "cleared on Mali and read back exactly, guards intact and clean teardown"),
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    # GL3-c: the client proves its image; drv-gpu's own lines prove the heap. The fresh
    # session's first heap lands at the window's start (G7q-a), one chunk, max two; the first
    # TILER_OOM grows it to two chunks exactly once, and every later one is refused at the
    # maximum (`driver_any`: at least once), which is when Gallium's exception handler
    # renders incrementally. Too small a draw fails the refusal line: raise
    # -DKUMO_GL_HEAP_LAYERS and update GL3C's draw, readback and sample lines (each layer's
    # step is 255 / layers).
    "gl3-c": dict(
        cases="gl-heap", runtime="device-metal", defines=["-DKUMO_GL_HEAP_MAIN=1"],
        out="gl3-c-metal", gl=True, begin="GL3-c metal heap begin",
        lines=GL3C,
        green=("GL3-c metal heap green: the instanced draw's image is exact on a one-chunk, max-two tiler heap, "
               "guards intact and clean teardown"),
        driver=["drv-gpu: client heap 1 context 0x30000000 chunks 1x262144 head 0x30001000 (max 2, in flight 65535)",
                "drv-gpu: client heap 1 grew to 2 chunks: 0x"],
        driver_any=["drv-gpu: client heap 1 not grown at 2 chunks"],
        absent=["drv-gpu: AS1 fault", "drv-gpu: client point", "drv-gpu: tiler OOM names no heap"], after=None),
    # GL3-b (PLAN/015 §8.32): the GL3-a triangle copied into view 1 and held on HDMI. The stage
    # adds the `view` lane marker (D1). This image runs no J0c diagnostic, so drv-fb's present
    # gate stays unarmed and the presents are Copied: CRUX accepted that gate on 2026-11-03,
    # leaving Retired to GL4. The present line ends in the measured Retired/Copied split, so
    # only its prefix is required. `boot` lines precede the begin line; `tail` lines follow
    # the green line (Sora's record of the client's exit).
    "gl3-b": dict(
        cases="gl-triangle", runtime="device-metal", defines=["-DKUMO_GL_PRESENT_MAIN=1"],
        out="gl3-b-metal", gl=True, marker=b"view\n", begin="GL3-b metal present begin",
        boot=["sora: gpu-lane marker selects the view lane"],
        lines=["GL3-b lane: view 1 channel and surface received over V, S, N; the native producer is not used",
               *GL3A,
               "GL3-b copy: 32768 RGBA8 pixels written to view 1's surface as upright XRGB8888 in 512 VmoWrite chunks",
               "GL3-b present: 15 of 15 presents accepted with FULL damage; retired=",
               "GL3-b release: view 1 channel and surface closed"],
        green=("GL3-b metal present green: the exact Mali triangle was copied into view 1 and every present was "
               "accepted; Copied is the gate on an image without J0c"),
        tail=["gpu-client: client exited; view lane returns view 1 to Sora, no regrant"],
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    # GL3-a: the first triangle through Mali's tiler, offscreen. — TIMBERDOODLE
    "gl3-a": dict(
        cases="gl-triangle", runtime="device-metal", defines=["-DKUMO_GL_TRIANGLE_MAIN=1"],
        out="gl3-a-metal", gl=True, begin="GL3-a metal triangle begin",
        lines=GL3A,
        green=("GL3-a metal triangle green: Mali rasterized the triangle exactly, all 32768 RGBA8 pixels and 32 guards "
               "checked, native completion and clean teardown"),
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    "gl2-b": dict(
        cases="gl-clear", runtime="device-metal", defines=["-DKUMO_GL_CLEAR_MAIN=1"],
        out="gl2-b-metal", gl=True, begin="GL2-b metal clear begin",
        lines=["GL2-b context: current GLES context over the native GPU session",
               "GL2-b target: complete owned 64x48 RGBA8 frontend framebuffer",
               "GL2-b baseline: public glReadPixels observed pixels different from the requested clear color",
               "GL2-b clear: public glClear and native glFinish complete, expected RGBA8 0xff996633",
               "GL2-b readback: 0 of 3072 pixels differ; 0 of 32 readback guards changed (Mali clear color 0xff996633)",
               "GL2-b samples: first=0xff996633 last=0xff996633",
               "GL2-b destroy: context, owned framebuffer drawable and display released"],
        green="GL2-b metal clear green: Mali glClear replaced all 3072 RGBA8 pixels, glReadPixels guards intact, native completion and clean teardown",
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    "gl2-a": dict(
        cases="gl-compute", runtime="device-metal", defines=["-DKUMO_GL_COMPUTE_MAIN=1"],
        out="gl2-a-metal", gl=True, begin="GL2-a metal compute begin",
        lines=["GL2-a context: current GLES 3.1-capable context over the native GPU session",
               "GL2-a compiler: GLSL ES 310 compute source compiled and linked at runtime",
               "GL2-a buffer: 64 output words and 16 guard words initialized to 0xdeadbeef",
               "GL2-a dispatch: four groups of sixteen invocations, memory barrier and native glFinish complete",
               "GL2-a readback: 0 of 64 output words differ; 0 of 16 guards changed (Mali shader results)",
               "GL2-a destroy: shader, program, SSBO, context, drawable and display released"],
        green="GL2-a metal compute green: runtime-compiled GLES compute wrote all 64 SSBO words, guards intact, native completion and clean teardown",
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"], after=None),
    "gl1-f": dict(
        cases="gl-client", runtime="device-metal", defines=["-DKUMO_GL_CLIENT_MAIN=1"],
        out="gl1-f-metal", gl=True,
        begin="GL1-f metal client begin",
        lines=["GL1-f renderer: Mali-G610 MC4 (Panfrost)", "GL1-f version: OpenGL ES ",
               "GL1-f finish: public glFinish returned with GL_NO_ERROR and the native device live",
               "GL1-f destroy: context unbound, drawable and display destroyed"],
        green="GL1-f metal client green: public GLES2 renderer names Mali-G610, glFinish completed, all frontend resources destroyed",
        driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point 1 failed"], after=None),
    "gl1-e": dict(
        define=9, out="gl1-e-metal", gl=True,
        begin="GL1-e metal frontend begin",
        lines=["GL1-e drawable: owned 64x48 RGBA8 color and Z24/S8 depth/stencil attachments",
               "GL1-e current: Mesa's GLES2 state-tracker context made current; glthread and threaded pipe off",
               "GL1-e front: validate supplied the attachments; native flush completed before the callback; callback refusal propagated",
               "GL1-e destroy: current context unbound, drawable and display released, session context recreation refused"],
        green="GL1-e metal frontend green: Mesa context made current on Mali, color/depth validated, front flush completed, context and display destroyed",
        driver=[], absent=[], after=None),
    "g7o-f": dict(
        define=0, out="g7o-f", begin="G7o-f metal device begin",
        lines=["G7o-f device: gpu_id=", "G7o-f BOs: 5 live", "G7o-f VM: 5 immediate binds",
               "G7o-f GPU: CS sync stores through Mesa bindings landed", "G7o-f VM: data BO unbound",
               "G7o-f W^X: CS store aimed at the executable binding", "G7o-f W^X: store refused"],
        green=("G7o-f metal device green: live props, Mesa BOs and VM, GPU stores through Mesa bindings, "
               "live rebind, W^X store faulted and contained"),
        after=None, driver=[], absent=[]),
    "g7p-e-fatal": dict(
        define=1, out="g7p-e-fatal", begin="G7p-e metal sync begin (fatal)",
        lines=SYNC + ["G7p-e fatal: point 3 stores into the executable binding 0x20100080",
                      "G7p-e fatal: ARM saw the session close"],
        green=("G7p-e metal sync green (fatal): native binary and timeline syncs, a dependency checked "
               "against the GPU, ARM completions, wait-pending and FATAL group state on Mali"),
        after="G7p-e fatal: point 3 stores",
        driver=["drv-gpu: client point 3 failed:",
                "drv-gpu: AS1 fault at VA=0x0000000020100080: PERMISSION_FAULT Write",
                "drv-gpu: containment reset=ok"],
        absent=[]),
    "g7p-e-timeout": dict(
        define=2, out="g7p-e-timeout", begin="G7p-e metal sync begin (timeout)",
        lines=SYNC + ["G7p-e timeout: point 3 waits on a word nobody writes",
                      "G7p-e timeout: ARM saw the session close"],
        green=("G7p-e metal sync green (timeout): native binary and timeline syncs, a dependency checked "
               "against the GPU, ARM completions, wait-pending and TIMED_OUT group state on Mali"),
        after="G7p-e timeout: point 3 waits",
        driver=["drv-gpu: client point 3 failed: Stalled", "drv-gpu: containment reset=ok"],
        absent=["drv-gpu: AS1 fault"]),
    # G7q-a (J754): the first heap of a fresh session lands at the window's start, context
    # first, so its chunks and head are fixed. — TIMBERDOODLE
    "g7q-a": dict(
        define=3, out="g7q-a", begin="G7q-a metal heap begin",
        lines=["G7q-a heap: 3 chunks of 131072 bytes, context 0x30000000, head 0x30041000",
               "G7q-a walk: the GPU followed 3 links from the head through 0x30021000 and 0x30001000",
               "G7q-a context: the GPU read its 32 bytes at 0x30000000, all zero",
               "G7q-a destroy: 397312 bytes returned",
               "G7q-a destroy: the load faulted"],
        green=("G7q-a metal heap green: HeapCreate placed and linked the chunks the GPU walked, the "
               "context read zero, and HeapDestroy unmapped them"),
        # drv-gpu logs the destroy just before the client's line, so the window opens earlier.
        after="G7q-a context: the GPU read its 32 bytes",
        driver=["drv-gpu: client heap 1 destroyed; 4 pieces unmapped",
                "drv-gpu: AS1 fault at VA=0x0000000030041000: TRANSLATION_FAULT",
                "drv-gpu: containment reset=ok"],
        absent=[]),
    # G7q-c (J757): the same walk with the heap created and destroyed through the C bridge.
    "g7q-c": dict(
        define=4, out="g7q-c-metal", begin="G7q-c metal heap begin",
        lines=["G7q-c heap: 3 chunks of 131072 bytes, context 0x30000000, head 0x30041000",
               "G7q-c walk: the GPU followed 3 links from the head through 0x30021000 and 0x30001000",
               "G7q-c context: the GPU read its 32 bytes at 0x30000000, all zero",
               "G7q-c destroy: 397312 bytes returned",
               "G7q-c destroy: the load faulted"],
        green=("G7q-c metal heap green: pan_kmod_kumo_heap_create placed and linked the chunks the GPU "
               "walked, and pan_kmod_kumo_heap_destroy unmapped them"),
        after="G7q-c context: the GPU read its 32 bytes",
        driver=["drv-gpu: client heap 1 destroyed; 4 pieces unmapped",
                "drv-gpu: AS1 fault at VA=0x0000000030041000: TRANSLATION_FAULT",
                "drv-gpu: containment reset=ok"],
        absent=[]),
    # GL1-a (J758): command streams from writable no-execute bindings, Gallium's and panvk's
    # CS pools, cached and uncached, with one line reused. — TIMBERDOODLE
    "gl1-a": dict(
        define=5, out="gl1-a", begin="GL1-a metal streams begin",
        lines=["GL1-a streams: two writable no-execute BOs (GPU-cached at 0x20100000, uncached at 0x20200000)",
               "GL1-a cached: a stream in the cached NX BO ran and {0x7f1a0001, status 0} landed",
               "GL1-a rewrite: the same line rewritten ran its new words; {0x7f1a0002, status 0} landed",
               "GL1-a uncached: a stream in the uncached NX BO ran and {0x7f1a0003, status 0} landed",
               "GL1-a refusal: a stream crossing its binding's end was refused"],
        green=("GL1-a metal streams green: Submit ran streams from cached and uncached writable no-execute "
               "bindings, a rewritten line ran its new words, and a straddling stream was refused"),
        after=None, driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"]),
    # VK1-c (PLAN/015 §8.2): every bring-up programs three queues (CRUX 2026-11-04); this session
    # leases all three and runs one point of three subqueue streams, ordered in memory.
    "vk1-c": dict(
        define=10, out="vk1-c", begin="VK1-c metal queues begin",
        lines=["VK1-c group: three subqueue streams on CSG0's queues 0-2, one point, queue 1 gated on queue 0 in memory",
               "VK1-c order: queue 1 waited for queue 0's {0x7f1b000a, status 0}, then stored {0x7f1b000b, status 0}",
               "VK1-c independent: queue 2 stored {0x7f1b000c, status 0}; the point completed with GPU status 0"],
        green=("VK1-c metal queues green: one point carried three subqueue streams, queue 1 ran after queue 0's "
               "store, queue 2 ran on its own, all statuses 0"),
        after=None, driver=["drv-gpu: client group 1 leases CSG0 queues 0-2"],
        absent=["drv-gpu: AS1 fault", "drv-gpu: client point"]),
    # GL1-b (J759): Gallium's screen over the native device; the client links Gallium. — TIMBERDOODLE
    "gl1-b": dict(
        define=6, out="gl1-b-metal", gl=True, begin="GL1-b metal screen begin",
        lines=["GL1-b screen: Mali-G610 MC4 (Panfrost) from Mesa, over KUMO's native device with no DRM fd",
               "GL1-b caps: GLSL", "GL1-b resource: a 64x64 RGBA8 render-target texture created",
               "GL1-b destroy: the texture and the screen are gone"],
        green=("GL1-b metal screen green: Gallium's panfrost screen came up on Mali over the native device, "
               "its caps read, a texture made, and everything destroyed"),
        after=None, driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point"]),
    "gl1-c": dict(
        define=7, out="gl1-c-metal", gl=True, begin="GL1-c metal context begin",
        lines=["GL1-c initialize: creating Gallium's native CSF context",
               "GL1-c context: Gallium created its CSF context; the native initialization submission completed",
               "GL1-c admission: a second context was refused while the first remained live",
               "GL1-c cleanup: native completion observed, heap released, context destroyed",
               "GL1-c destroy: the screen and device are gone; the session-owned group was not reused"],
        green=("GL1-c metal context green: Gallium's native initialization completed, a second context was refused, "
               "and the heap, context and screen were destroyed"),
        after=None, driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point 1 failed"]),
    # GL1-d (J765): a clear through Gallium's batch, its native fence and the CPU readback. On
    # metal the poll may find the batch complete or pending; both lines start alike. — TIMBERDOODLE
    "gl1-d": dict(
        define=8, out="gl1-d-metal", gl=True, begin="GL1-d metal batch begin",
        lines=["GL1-d target: a linear 64x64 RGBA8 render target, filled from the CPU with 0xdeadbeef",
               "GL1-d batch: pipe->clear flushed as one native submission; the fence snapshots the context binary",
               "GL1-d poll: a zero-timeout fence_finish found the batch",
               "GL1-d fence: fence_finish completed natively, ",
               "GL1-d readback: 0 of 4096 pixels differ from 0xff996633 (the GPU's clear color)",
               "GL1-d state: native GROUP_STATE reports the group usable, so no reset",
               "GL1-d destroy: fence, texture, context and screen are gone; every BO left through a deferred unmap"],
        green=("GL1-d metal batch green: Mali cleared the render target through Gallium, the native fence completed, "
               "the CPU read the clear color back, and everything was destroyed"),
        after=None, driver=[], absent=["drv-gpu: AS1 fault", "drv-gpu: client point 1 failed",
                                       "drv-gpu: client point 2 failed"]),
}


def entries(image):
    data = image.read_bytes()
    assert data[:8] == b"KUMORD01" and struct.unpack_from("<I", data, 8)[0] == 1
    count = struct.unpack_from("<I", data, 12)[0]
    for index in range(count):
        entry = 16 + index * 80
        name = data[entry:entry + 64].split(b"\0", 1)[0]
        offset, size = struct.unpack_from("<QQ", data, entry + 64)
        yield name, data[offset:offset + size]


def stage(destination, elf, marker=None, autoexec=None):
    assert not destination.exists(), f"refusing to replace existing image copy: {destination}"
    # The console level is a kernel build constant (J748), so the copy is taken from an image
    # built for it, and the kernel identity check below covers that build. — TIMBERDOODLE
    subprocess.run(["cargo", "xtask", "image", "--arch", "aarch64", "--hardware", "orange-pi-5-plus"],
                   cwd=ROOT, check=True, env={**os.environ, "KUMO_CONSOLE_LOGLEVEL": "5"})
    shutil.copytree(IMAGE, destination)
    info = f"KUMO_CONSOLE_LOGLEVEL=5\ngpu-client sha256 {hashlib.sha256(elf.read_bytes()).hexdigest()}\n"
    if marker is not None:
        info += f"{LANE.decode()} {marker.decode().strip()}\n"
    if autoexec is not None:
        info += f"{AUTOEXEC.decode()} += {autoexec.decode().strip()!r}\n"
    (destination / "STAGE-INFO.txt").write_text(info)
    path = destination / "EFI/KUMO/initrd.img"
    original = path.read_bytes()
    count = struct.unpack_from("<I", original, 12)[0]
    # Existing entries keep their original name bytes; only offsets and sizes are rewritten.
    items = []
    replaced = 0
    for index, (name, data) in enumerate(entries(path)):
        if name == SLOT:
            data, replaced = elf.read_bytes(), replaced + 1
        if autoexec is not None and name == AUTOEXEC:
            data = data + autoexec
        items.append((original[16 + index * 80:16 + index * 80 + 64], data))
    assert replaced == 1, "the image has no single GPU client slot"
    if marker is not None:
        assert all(name.split(b"\0", 1)[0] != LANE for name, _ in items), "the built image already has a lane marker"
        items.append((LANE.ljust(64, b"\0"), marker))
    assert len(items) <= MAX_ENTRIES, f"{len(items)} initrd entries exceed the loader's {MAX_ENTRIES}"
    out = bytearray(original[:12] + struct.pack("<I", len(items)) + bytes(80 * len(items)))
    for index, (name, data) in enumerate(items):
        out[16 + index * 80:16 + index * 80 + 64] = name
        struct.pack_into("<QQ", out, 16 + index * 80 + 64, len(out), len(data))
        out.extend(data)
    path.write_bytes(out)
    # Prove the staged image differs from the built one in the slot (and the marker it adds),
    # and nowhere else.
    built, staged = dict(entries(IMAGE / "EFI/KUMO/initrd.img")), dict(entries(path))
    changed = [SLOT] + ([LANE] if marker is not None else []) + ([AUTOEXEC] if autoexec is not None else [])
    assert staged.keys() == built.keys() | set(changed)
    assert sorted(n for n in staged if built.get(n) != staged[n]) == sorted(changed)
    assert marker is None or staged[LANE] == marker
    # Sora reads at most 256 bytes of autoexec (main.rs `manifest`); the appended line must fit.
    assert autoexec is None or (staged[AUTOEXEC] == built[AUTOEXEC] + autoexec and len(staged[AUTOEXEC]) <= 256)
    kernel = "EFI/KUMO/kernel/kumo-kernel.elf"
    assert (IMAGE / kernel).read_bytes() == (destination / kernel).read_bytes()
    if marker is None:
        print(f"staged {destination}: only {SLOT.decode()} differs ({len(staged[SLOT])} bytes); kernel identical")
    else:
        extra = f" and {AUTOEXEC.decode()} (+{autoexec.decode().strip()!r})" if autoexec is not None else ""
        print(f"staged {destination}: only {SLOT.decode()} ({len(staged[SLOT])} bytes), {LANE.decode()} "
              f"({marker.decode().strip()!r}){extra} differ; kernel identical")
    print(f"initrd sha256 {hashlib.sha256(out).hexdigest()} ({len(out)} bytes)")


def check(log, proof):
    text = log.read_bytes().decode(errors="replace").replace("\r", "")
    # Live lines only: a dmesg replay repeats a line behind a second, kernel timestamp.
    live = [line for line in text.splitlines() if not re.match(r"^\[[0-9:.]+\] \[ *[0-9]+\.[0-9]+\]", line)]
    # A log may hold several boots: judge only the last, from its own PXE start.
    boot = max((i for i, line in enumerate(live) if "Station IP address" in line), default=0)
    live = live[boot:]
    begin = proof["begin"]
    start = next((i for i, line in enumerate(live) if begin in line), None)
    assert start is not None, f"{begin!r} never appeared live; was the image built with KUMO_CONSOLE_LOGLEVEL=5?"
    for expected in proof.get("boot", []):
        hits = [line for line in live[:start] if expected in line]
        assert len(hits) == 1, f"{expected!r} appears {len(hits)} times before {begin!r}"
    window = live[start:]
    position = 0
    for expected in [begin, *proof["lines"], proof["green"]]:
        hits = [i for i, line in enumerate(window) if expected in line]
        assert len(hits) == 1, f"{expected!r} appears {len(hits)} times"
        assert hits[0] >= position, f"{expected!r} is out of order"
        position = hits[0]
    # Without a failure window, `absent` covers the whole proof: GL1-a must fault nowhere.
    last = next(i for i, line in enumerate(window) if proof["green"] in line)
    for expected in proof.get("tail", []):
        hits = [line for line in window[last + 1:] if expected in line]
        assert len(hits) == 1, f"{expected!r} appears {len(hits)} times after the green line"
    first = next(i for i, line in enumerate(window) if proof["after"] in line) if proof["after"] else 0
    failure = window[first:last]
    for expected in proof["driver"]:
        hits = [line for line in failure if expected in line]
        assert len(hits) == 1, f"{expected!r} appears {len(hits)} times in the failure window"
    for expected in proof.get("driver_any", []):
        hits = [line for line in failure if expected in line]
        assert hits, f"{expected!r} never appears in the proof window"
        print(f"{expected!r}: {len(hits)} times")
    for unwanted in proof["absent"]:
        assert not any(unwanted in line for line in failure), f"{unwanted!r} in the failure window"
    for bad in ("TOWER", "FAILED", "ABEND"):
        assert not any(bad in line for line in live), f"{bad} in the boot"
    for line in window:
        if line.split("] ", 1)[-1].startswith(("drv-gpu: AS1 fault", "drv-gpu: client point", "drv-gpu: containment",
                                               "drv-gpu: client heap",
                                               "drv-gpu: CS status", "drv-gpu: MMU rawstat", "drv-gpu: reset",
                                               "gpu-client: client exited", "G7p-e diag", "G7o-f diag")):
            print(line)
    # Name the present gate's state, so a Copied-only run carries drv-fb's own reason.
    for line in live:
        if "display: present gate" in line or "sora: gpu-lane" in line:
            print(line)
    name = begin.split(" metal")[0]
    print(f"{name} metal check green ({proof['out']}): every proof line once, in order; no TOWER, FAILED or ABEND")


def check_sessions(log, proof):
    """GL4 or VK5: every session of one boot, in order. Session 1 runs J0c; later ones are regrants.
    Each needs readiness, 100 exact frames and 100 Retired / 0 Copied. drv-fb must have
    completed J0c and armed its gate. The census (kernel `census:` lines, enabled by the
    stage's autoexec `dmesg -n debug`, RE4) must hold DMA VMOs flat across sessions."""
    text = log.read_bytes().decode(errors="replace").replace("\r", "")
    live = [line for line in text.splitlines() if not re.match(r"^\[[0-9:.]+\] \[ *[0-9]+\.[0-9]+\]", line)]
    boot = max((i for i, line in enumerate(live) if "Station IP address" in line), default=0)
    live = live[boot:]
    name = proof["begin"].split(" metal")[0]
    begins = [i for i, line in enumerate(live) if proof["begin"] in line]
    assert len(begins) >= 2, f"{len(begins)} {name} sessions; G8's bar needs at least two in one boot"
    for expected in proof["boot"]:
        assert sum(expected in line for line in live[:begins[0]]) == 1, f"{expected!r} not once before session 1"
    for expected in proof["display"]:
        assert sum(expected in line for line in live) == 1, f"{expected!r} not once in the boot"
    for bad in ("TOWER", "FAILED", "ABEND"):
        assert not any(bad in line for line in live), f"{bad} in the boot"
    dma = {}
    for number, start in enumerate(begins, 1):
        end = begins[number] if number < len(begins) else len(live)
        window = live[start:end]
        required = proof["each"] + (proof["first"] if number == 1 else proof["later"]) + [proof["green"]]
        for expected in required:
            hits = sum(expected in line for line in window)
            assert hits == 1, f"session {number}: {expected!r} appears {hits} times"
        if number > 1:
            assert not any(f"{name} J0c" in line for line in window), f"session {number} ran J0c"
        # The census needs `dmesg -n debug`, which also shows drv-gpu's per-point success line
        # (`client point N complete on JOB ... status=0x0`); only its failure form may match the
        # `absent` prefix (RE4: K2's checker refused a green session 4 on that). — TIMBERDOODLE
        failures = [line for line in window if not re.search(r"drv-gpu: client point \d+ complete on JOB: ", line)]
        for unwanted in proof["absent"]:
            assert not any(unwanted in line for line in failures), f"session {number}: {unwanted!r}"
        # Compare like phases (RE4): the census fires at most every 10 s, when Sora wakes, so a
        # sample may land mid-frames (the client's buffers live) or after teardown. Only the
        # post-teardown footprint, sampled after a session's green line, measures a leak
        # across sessions; mid-session samples are printed but not compared. — TIMBERDOODLE
        green = next(i for i, line in enumerate(window) if proof["green"] in line)
        census = [(i, int(m.group(1))) for i, line in enumerate(window)
                  for m in [re.search(r"census: .* vmos=\d+/(\d+)dma", line)] if m]
        after = [count for i, count in census if i > green]
        during = [count for i, count in census if i < green]
        if after:
            dma[number] = after[-1]
        print(f"{name} session {number}: 100/0 Retired/Copied, census DMA VMOs after teardown "
              f"{dma.get(number, 'not sampled')}, during {during or 'not sampled'}")
    for line in live:
        if "display: present gate" in line or "sora: gpu-lane" in line or "restart budget" in line:
            print(line)
    assert len(dma) >= 2, "no post-teardown census in at least two sessions: was autoexec's `dmesg -n debug` staged?"
    # A leak shows as growth across sessions. The last session's sample may land after Sora
    # exhausts the restart budget and releases the view, falling back to the boot baseline
    # (metal: 29, 29, 29, then 12); a drop is not a leak (RE4). — TIMBERDOODLE
    first = dma[min(dma)]
    spread = max(dma.values()) - first
    assert spread <= 4, f"census DMA VMOs grew across sessions: {dma}"
    api = "Vulkan" if name.startswith("VK") else "GL"
    print(f"{name} metal check green ({proof['out']}): {len(begins)} sessions of 100 Retired / 0 Copied, "
          f"J0c {api}-fed in session 1, post-teardown census DMA VMOs never grew over sessions {sorted(dma)} "
          f"({dma}, growth {spread}); "
          f"no TOWER, FAILED or ABEND")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--proof", choices=sorted(PROOFS), default="g7o-f")
    parser.add_argument("--stage", type=Path)
    parser.add_argument("--check", type=Path)
    args = parser.parse_args()
    proof = PROOFS[args.proof]
    if args.check:
        (check_sessions if proof.get("sessions") else check)(args.check, proof)
        return
    spec = importlib.util.spec_from_file_location("device_smoke", ROOT / "scripts/mesa-device-smoke.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    defines = proof["defines"] if "defines" in proof else [f"-DKUMO_METAL_PROOF={proof['define']}"]
    elf = module.build(proof.get("cases", "device-metal"), ROOT / "build/proofs" / proof["out"],
                       TOOL / "device-metal-crt0.S", defines=defines, gl=proof.get("gl", False),
                       runtime=proof.get("runtime"), vk=proof.get("vk", False))
    print(f"{args.proof} metal client compile/link green: {elf} sha256 {hashlib.sha256(elf.read_bytes()).hexdigest()}")
    if args.stage:
        stage(args.stage, elf, proof.get("marker"), proof.get("autoexec"))


if __name__ == "__main__":
    main()
