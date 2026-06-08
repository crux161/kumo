//! Freestanding aarch64 boot shim for the KUMO microkernel image.
//!
//! Nijigumo loads this ELF, exits boot services, and branches to `_start` with
//! `x0` holding the `BootInfo` pointer (the handoff ABI). `_start` installs a
//! stack and tail-calls [`kernel::kmain`]. For any other target (the host test
//! build) this file is just an empty `main` so the workspace still builds and
//! tests with `std`.

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
mod boot {
    use core::panic::PanicInfo;

    /// 64 KiB boot stack, 16-byte aligned so the very first `stp` is legal.
    #[repr(align(16))]
    #[allow(dead_code)]
    struct Stack([u8; 0x1_0000]);

    #[used]
    static mut KERNEL_STACK: Stack = Stack([0; 0x1_0000]);

    // _start: point SP at the top of KERNEL_STACK, keep x0 (BootInfo*), branch to
    // the Rust entry. Symbol references go through `sym` so this works without
    // assuming a mangling and at whatever address Nijigumo loaded us.
    core::arch::global_asm!(
        ".section .text._start",
        ".globl _start",
        "_start:",
        // Mask Debug/SError/IRQ/FIQ until we own the GIC + our own vectors. UEFI
        // hands off with interrupts live and a timer armed; an unmasked tick would
        // vector through a now-stale VBAR_EL1 and reset the machine.
        "  msr  daifset, #0xf",
        "  adrp x1, {stack}",
        "  add  x1, x1, :lo12:{stack}",
        "  mov  x2, #0x10000",
        "  add  sp, x1, x2",
        "  b    {entry}",
        stack = sym KERNEL_STACK,
        entry = sym kernel_entry,
    );

    extern "C" fn kernel_entry(boot: *const kumo_abi::BootInfo) -> ! {
        kernel::kmain(boot)
    }

    #[panic_handler]
    fn panic(_info: &PanicInfo<'_>) -> ! {
        kumo_hal::active::halt()
    }
}
