 GUIDANCE   kernel    targets     build.sh   PLAN.md      vars.fd

crux  …/KUMO   main    v1.96.0  ♥ 23:20  cargo clean
     Removed 0 files

crux  …/KUMO   main    v1.96.0  ♥ 23:20  git status
On branch main
Your branch is up to date with 'origin/main'.

nothing to commit, working tree clean

crux  …/KUMO   main    v1.96.0  ♥ 23:20  nvim README

crux  …/KUMO   main    v1.96.0  ♥ 23:21  nvim README.md

crux  …/KUMO   main !   v1.96.0  ♥ 23:21  gsed -E 's/\[cite: [0-9]+\]//g' README.md
  <img src="resources/kumo_full-color.jpg" alt="KUMO Logo" width="400"/>

  # KUMO (雲)
  **A Serene, Capability-Based Microkernel in Rust**
</div>

---

> *Outwardly, the system is KUMO — a drifting cloud. Inwardly, the privileged core is Ziwei (紫微) — t

**KUMO** is a clean-room, `#![no_std]` Rust rewrite of the `soso` monolithic kernel, reimagined as a m

## 🏛 Architecture

*   **Capability Microkernel:** Minimal Trusted Computing Base (TCB). All resources (memory, IPC, inte
*   **Nijigumo (虹雲):** A UEFI-first staged bootloader providing a stable, arch-neutral `BootInfo` ha
*   **Sora (空):** The root server and service plane supervisor. It brokers capabilities and restarts
*   **Hardware Abstraction Layer (HAL):** Clean separation of architecture-specific glue (`kumo-hal-aa

## 🚀 Current Status (Milestone 4 - P5-mmu-a)

KUMO is in active, early-stage development, currently executing in the highest exception levels on **a

**Recent execution milestones:**
*   **Higher-Half Kernel:** Permanent TTBR0/TTBR1 split established. Kernel linked at `0xffff800048000
*   **Bidirectional IPC:** The core `Ziwei` and the root server `Sora` now successfully communicate ov
*   **Entry ABI:** Bootstrap capabilities are now securely passed in `x0` upon ring3/EL0 entry.

**Next in the Forge:**
*   **P5-console-cjk:** Migrating the Stage-A console to support native Japanese/Chinese diagnostics (
*   **P5-mmu-b:** Per-process TTBR0 trees, W^X enforcement, and user pointer validation.

## 💻 Hardware Targets

The genesis hardware target is the **Lenovo ThinkPad X13s Gen 1** (Snapdragon 8cx Gen 3 / SC8280XP). Bare-metal validation is prioritized on this specific arm64 SoC, utilizing GICv3, the ARM generic timer, and UEFI/DTB handoffs.

*QEMU `virt` (AAVMF) and `q35` (OVMF) are used for continuous integration, but real silicon dictates the critical path.*

<div align="center">
  <img src="resources/kumo_silhouette.jpg" alt="KUMO Silhouette" width="200"/>
</div>

## 🛠 Building and Running

The project is orchestrated via a Cargo `xtask` workspace, eliminating complex Makefiles.

```bash
# Run the QEMU smoke test on the primary architecture (aarch64)
cargo xtask run --arch aarch64

# Build the bootable GPT/UEFI disk image
cargo xtask image --arch aarch64

# Run the full test suite (exercises both x86_64 and aarch64 backends)
cargo xtask test
