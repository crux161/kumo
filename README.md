<div align="center">
  <img src="resources/kumo_full-color.jpg" alt="KUMO Logo" width="400"/>[cite: 1]

  # KUMO (雲)
  **A Serene, Capability-Based Microkernel in Rust**
</div>

---

> *Outwardly, the system is KUMO — a drifting cloud. Inwardly, the privileged core is Ziwei (紫微) — the still seat residing in Pleroma. Nijigumo bridges earth to Heaven at boot; thereafter, Ziwei reigns at the still center and, should all else fall apart, remains to reconstruct the whole.*

**KUMO** is a clean-room, `#![no_std]` Rust rewrite of the `soso` monolithic kernel, reimagined as a modern, capability-based microkernel. It strips the privileged kernel down to the irreducible minimum—address spaces, scheduling, IPC, capabilities, and MMU plumbing—while pushing all other services (drivers, filesystems, network, TTY) into fault-isolated, restartable userspace servers.

## 🏛️ Architecture

*   **Capability Microkernel:** Minimal Trusted Computing Base (TCB)[cite: 2]. All resources (memory, IPC, interrupts) are exposed as Objects[cite: 2]. Process authority is strictly defined by unforgeable, capability-typed **Handles**[cite: 2].
*   **Nijigumo (虹雲):** A UEFI-first staged bootloader providing a stable, arch-neutral `BootInfo` handoff[cite: 2].
*   **Sora (空):** The root server and service plane supervisor[cite: 2]. It brokers capabilities and restarts crashed servers from their zero-state recipes[cite: 2].
*   **Hardware Abstraction Layer (HAL):** Clean separation of architecture-specific glue (`kumo-hal-aarch64`, `kumo-hal-x86_64`) from the generic core[cite: 2].

## 🚀 Current Status (Milestone 4 - P5-mmu-a)

KUMO is in active, early-stage development, currently executing in the highest exception levels on **aarch64** (with x86_64 running co-equal in CI)[cite: 2, 3]. 

**Recent execution milestones:**
*   **Higher-Half Kernel:** Permanent TTBR0/TTBR1 split established[cite: 3]. Kernel linked at `0xffff800048000000` with 4KiB granules[cite: 3].
*   **Bidirectional IPC:** The core `Ziwei` and the root server `Sora` now successfully communicate over full-duplex capability channels via EL0 `SVC` calls[cite: 4].
*   **Entry ABI:** Bootstrap capabilities are now securely passed in `x0` upon ring3/EL0 entry[cite: 5].

**Next in the Forge:** 
*   **P5-console-cjk:** Migrating the Stage-A console to support native Japanese/Chinese diagnostics (e.g., 虹雲, 紫微) directly to the UEFI GOP framebuffer using a sparse, binary-searched GNU Unifont asset and a lightweight UTF-8 state machine.
*   **P5-mmu-b:** Per-process TTBR0 trees, W^X enforcement, and user pointer validation[cite: 3].

## 💻 Hardware Targets

The genesis hardware target is the **Lenovo ThinkPad X13s Gen 1** (Snapdragon 8cx Gen 3 / SC8280XP)[cite: 2]. Bare-metal validation is prioritized on this specific arm64 SoC, utilizing GICv3, the ARM generic timer, and UEFI/DTB handoffs[cite: 2]. 

*QEMU `virt` (AAVMF) and `q35` (OVMF) are used for continuous integration, but real silicon dictates the critical path.*[cite: 2]

<div align="center">
  <img src="resources/kumo_silhouette.jpg" alt="KUMO Silhouette" width="200"/>[cite: 1]
</div>

## 🛠️ Building and Running

The project is orchestrated via a Cargo `xtask` workspace, eliminating complex Makefiles[cite: 2].

```bash
# Run the QEMU smoke test on the primary architecture (aarch64)
cargo xtask run --arch aarch64

# Build the bootable GPT/UEFI disk image
cargo xtask image --arch aarch64

# Run the full test suite (exercises both x86_64 and aarch64 backends)
cargo xtask test
