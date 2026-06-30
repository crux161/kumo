<div align="center">
  <img src="resources/kumo_full-color.jpg" alt="KUMO Logo" width="400"/> 

  # KUMO (雲)
  **A Serene, Capability-Based Microkernel in Rust**
</div>

---

**KUMO** is a clean-room, `#![no_std]` Rust rewrite of the [soso](https://github.com/ozkl/soso) monolithic kernel, reimagined as a modern, capability-based microkernel. It strips the privileged kernel down to the irreducible minimum: address spaces, scheduling, IPC, capabilities, traps, MMU plumbing, and the small amount of hardware confinement needed to make user-mode drivers honest.

The larger "Flying Nimbus" system is intended to become a UNIX-like environment with native KUMO services, a Rust userspace, persistent storage, graphics, and Linux-application compatibility through a demand-driven persona layer. The project already boots a real EL0 root server and user processes; it is not yet a complete daily-driver OS.

I'd like to take a brief moment to thank ozkl and soso's other contributors. soso provided a lot of inspiration for KUMO's early shape and core components.

Also, the devs working on:
- [motor-os](https://github.com/moturus/motor-os) 
- Fuchia (Zircon)
- XNU/Mach 
- Redox OS 
- seL4

All of them have given excellent points about pitfalls, implementation, and have truly shaped the internal discourse surrounding this project. KUMO is still obviously in active development, but over time I hope it can become something different but still purposeful in people's lives.

Redox has paved the way for other Rust-based systems to exist without recreating every storage and libc component by hand. KUMO's longer-term storage plan leans on RedoxFS, and the libc/`std` plan builds toward a native Rust target instead of pretending the whole userspace stack appears at once.

My hope is that with some minor adjustments, capability-based seams, and a good HAL that KUMO will be highly portable, stable, performant, and above all resilient in the face of failure.

So far the code boots on real hardware, not just QEMU. Hardware bring-up has covered:

- Thinkpad x13s gen 1, Qualcomm Snapdragon 8cx Gen 3 SoC (arm64)
- Raspberry Pi 5, Broadcom BCM2712 (arm64)
- HP Z4G4, Intel Xeon W2123 (amd64)
- HP Z650, Intel Xeon E5-2620 v0 (amd64)
- Thinkpad x220, Intel i7-2640m (amd64)



## 🏛️ Architecture

*   **Capability Microkernel:** Minimal Trusted Computing Base (TCB). All resources (memory, IPC, interrupts, address spaces) are exposed as Objects. Process authority is defined by unforgeable, capability-typed **Handles**.
*   **Nijigumo (虹雲):** A UEFI-first staged bootloader providing a stable `BootInfo` handoff into MUREX.
*   **MUREX:** The privileged core: scheduler, object tables, handle rights, VMOs/VMARs, IPC, traps, and MMU construction.
*   **Sora (空):** The root server and service-plane supervisor. It receives bootstrap capabilities, hosts early services, spawns child processes from capability grants, and drives the current service smoke path.
*   **Hardware Abstraction Layer (HAL):** Clean separation of architecture-specific glue (`kumo-hal-aarch64`, `kumo-hal-x86_64`) from the generic core.
*   **Device-VMAR / IOMMU:** The active M13 work: confining DMA-capable devices to `Vmo`s explicitly mapped into their `DeviceCtx`, so user-mode drivers cannot bypass capabilities through raw physical DMA.

## 🚀 Current Status (M13 Binding Circle)

KUMO now boots through UEFI/AAVMF on **aarch64**, exits boot services, enters the kernel at EL1, launches Sora in EL0, and exercises a live userspace path through the scheduler, IPC, and serial console. The current development spine is **M13: Device-VMAR/IOMMU DMA isolation**. x86_64 remains a required build target, while full x86 runtime parity is still deferred.

**Recent execution milestones:**
*   **UEFI handoff:** Nijigumo loads the kernel ELF and initrd from the ESP, builds a validated `BootInfo`, exits boot services, and jumps to MUREX.
*   **Higher-half kernel:** MUREX runs with a TTBR0/TTBR1 split, a higher-half kernel at `0xffff800048000000`, a permanent physmap, and 4 KiB page granules.
*   **Userspace and process model:** Sora is loaded from the initrd as an ELF process, receives bootstrap handles, serves channels through ports, spawns child address spaces, and keeps the root service path alive.
*   **Capability IPC:** Channels, ports, synchronous call, handle transfer, object rights, interrupt objects, timers, and wait paths are wired through EL0 `SVC` calls.
*   **Persona Linux MVP:** The compatibility path can run a static arm64 Linux ELF through the native persona layer; expansion is demand-driven, one missing syscall at a time.
*   **Input and console:** The serial/TTY path supports line editing, history navigation, typed HID keyboard input, and a typed mouse-event forwarding path that currently drains in Sora.
*   **Device-VMAR/IOMMU:** The ABI and kernel object surface for `IoMmu` and `DeviceCtx` exists. The aarch64 HAL has an SMMUv3 binding stub, `DeviceCtxCreate` guards backend stream-width, and `DeviceVmarMap` now validates and records page-aligned IOVA mappings for VMOs. `DeviceVmarUnmap`, fault delivery, and real SMMU queue programming are still in progress.
*   **Hardware interrupt lanes:** GICv3 remains the X13s/QEMU path; GICv2/GIC-400 discovery and timer setup support Raspberry Pi 5 parity work.

<div align="center">
  <img src="resources/kumo-boot-status.png" alt="KUMO framebuffer boot status showing MUREX and Sora diagnostics" width="640"/>
  <br/>
  <sub>Earlier framebuffer smoke capture: MUREX Stage-A diagnostics, Sora handoff, IPC, scheduler, and timer checks.</sub>
</div>

**Next in the Forge:**
*   **M13 Device-VMAR:** Grow the current record-only map surface into `DeviceVmarUnmap`, backend map/unmap plumbing, and fault reporting.
*   **SMMUv3 and virtio-iommu proof:** Prove DMA confinement first in the safe QEMU IOMMU lane, then on the ThinkPad X13s SMMUv3 path.
*   **PLAN IV pillars:** Continue the independent relibc/`std`, RedoxFS/Houtu, and graphics/compositor tracks once their prerequisites are ready.

## 💻 Hardware Targets

The genesis hardware target is the **Lenovo ThinkPad X13s Gen 1** (Snapdragon 8cx Gen 3 / SC8280XP). Bare-metal validation is prioritized on this specific arm64 SoC, using GICv3, the ARM generic timer, UEFI, GOP framebuffer discovery, and DTB handoff.

QEMU `virt` (AAVMF) is the fast local smoke path for the aarch64 kernel/userspace spine. Raspberry Pi 5 is a secondary arm64 lane for GICv2/PL011 parity. x86_64 generic UEFI images are kept building as an architectural guardrail, but x86 runtime parity is not the current critical path.

<div align="center">
  <img src="resources/kumo_silhouette.jpg" alt="KUMO Silhouette" width="200"/> 
</div>

## 🛠️ Building and Running

The project is orchestrated via a Cargo `xtask` workspace, eliminating complex Makefiles.

```bash
# Build the QEMU/AAVMF image used for local arm64 smoke testing
cargo xtask image --arch aarch64 --hardware qemu-virt-aarch64

# Build the ThinkPad X13s image
cargo xtask image --arch aarch64 --hardware thinkpad-x13s-gen1

# Build the generic x86_64 UEFI image
cargo xtask image --arch x86_64 --hardware generic-uefi-x86_64

# Run the aarch64 QEMU smoke test
cargo xtask qemu-smoke --arch aarch64

# Run the core host checks
cargo fmt --check
cargo test -p kumo-abi
cargo test -p kernel

# Run the contributor preflight used by current green slices
./scripts/preflight.sh
```
