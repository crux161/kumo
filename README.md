# KUMO (雲)

KUMO is a modular, multi-architecture, capability-based microkernel written in `#![no_std]` Rust[cite: 2]. Conceived as a clean-room rewrite of the `soso` monolithic kernel, KUMO maintains soso's Unix-like semantics and UX but completely replaces its underlying mechanism[cite: 2]. It draws architectural lineage from Zircon (Fuchsia), XNU/Mach, and motor-os[cite: 2].

## Current Project Status

KUMO is currently at **Milestone 3 (M3): Thread Context Substrate**. 

* **Hardware Gate Cleared:** The M2 hardware gate has been successfully passed, with the AArch64 GIC/timer heartbeat confirmed running on the bare-metal primary target, the Lenovo ThinkPad X13s[cite: 3].
* **Task Substrate:** The `Process` construct now explicitly owns a root `Vmar`[cite: 3]. The `Thread` construct manages a checked 16-byte-aligned kernel stack, an explicit execution state, and a HAL `ThreadContext`[cite: 3].
* **Context Switching:** The `kumo-hal-aarch64` backend includes the saved context shape and assembly for context switching (`kumo_context_switch` and `kumo_context_trampoline`), which currently sits dormant awaiting scheduler wiring[cite: 3].
* **CI Symmetry:** The x86_64 backend maintains a symmetric placeholder `ThreadContext` and continues to pass full `xtask test` guardrails in CI[cite: 3].

## Core Architecture

* **Capability Microkernel (The TCB):** The privileged core (Ziwei) is reduced to the irreducible minimum: address spaces, threads/scheduling, IPC, capabilities, and low-level interrupt/timer plumbing[cite: 2]. Handles are unforgeable and act as the sole source of authority[cite: 2].
* **Isolated Userspace Servers:** Everything outside the core—drivers, filesystems, the network stack, and the TTY—runs as fault-isolated userspace servers[cite: 2]. These servers communicate over capability-mediated message channels[cite: 2].
* **No Kernel Co-location:** To preserve resilience, KUMO strictly avoids XNU-style kernel co-location for IPC fast-paths[cite: 2]. Performance is achieved through synchronous calls, zero-copy `Vmo` transfers, and shared-memory rings established between isolated processes[cite: 2].
* **Two ABI Layers:** The kernel exposes a small, stable, capability-typed object ABI[cite: 2]. A POSIX/Unix personality lives entirely in userspace, with a future Starnix-style Linux personality planned to support unmodified Linux binaries[cite: 2].
* **Supervised Restart:** Built for resilience, KUMO follows a crash-only/microreboot design[cite: 2]. The root server (`Sora`) holds construction recipes and respawns crashed service processes without disrupting the core[cite: 2].

## Supported Hardware

* **AArch64 (Primary/Genesis):** The production hardware target[cite: 2]. Specifically targeted and tested on the Lenovo ThinkPad X13s (Snapdragon 8cx Gen 3) using a UEFI + DTB + GICv3 boot path[cite: 2, 3].
* **x86_64 (Co-equal in CI):** Compiles and passes QEMU/host tests symmetrically with AArch64 every phase, serving as an anti-fork guarantee[cite: 2]. Bare-metal testing is planned for later phases[cite: 2].

## Workspace Structure

The system is organized into a Cargo virtual workspace[cite: 2]:

* `kernel/`: The KUMO microkernel core (object manager, scheduler, IPC, MM)[cite: 2].
* `boot/`: The Nijigumo UEFI-first staged bootloader[cite: 2].
* `hal/`: Architecture abstraction (`kumo-hal`) with backends for `aarch64` and `x86_64`[cite: 2].
* `userland/`: Contains the runtime (`kumo-rt`), root server (`sora`), TUI shell (`kumoza`), and grouped subsystem servers (the Siyu)[cite: 2].
* `lib/`: Shared `no_std` libraries including `kumo-abi` and `kumo-ipc`[cite: 2].
* `xtask/` & `imager/`: Build orchestration and GPT/UEFI disk image generation[cite: 2].

## Building and Testing

KUMO relies on a pinned nightly Rust toolchain[cite: 2]. Build and orchestration are handled via `xtask`.

**Example Build & Image Creation:**
```bash
cargo xtask test --arch aarch64
cargo xtask image --arch aarch64 --hardware x13s
cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64
