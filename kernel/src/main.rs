//! Freestanding aarch64 boot shim for the KUMO microkernel image.
//!
//! Nijigumo loads this ELF, exits boot services, and branches to `_start` with
//! `x0` holding the `BootInfo` pointer (the handoff ABI). `_start` installs a
//! stack and tail-calls [`kernel::kmain`]. For any other target (the host test
//! build) this file is just an empty `main` so the workspace still builds and
//! tests with `std`.

//j447
//j825
//j904
//j887

#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(not(target_os = "none"))]
fn main() {}

///j825
/// Report a panic on the earliest console before halting. Both handlers used to halt in
/// silence, so any panic — a full fixed heap at the AX4 capacity fill was the first one seen
/// on metal — looked like a wedged machine: 100% CPU and no serial output. This path must not
/// allocate: `core::fmt::write` formats into a sink that hands each fragment straight to the
/// alloc-free UART/framebuffer writer. — TIMBERDOODLE
#[cfg(target_os = "none")]
fn panic_report(info: &core::panic::PanicInfo<'_>) -> ! {
    struct Console;

    impl core::fmt::Write for Console {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            kumo_hal::active::early_console_write(text.as_bytes());
            Ok(())
        }
    }

    let _ = core::fmt::write(
        &mut Console,
        format_args!("\nTOWER: KERNEL PANIC: {info}\n"),
    );
    // j887: the callers on the panicking stack — the panic location alone names only the
    // `expect` that fired. Symbolize with `llvm-addr2line -f -C -e kumo-kernel.elf`.
    #[cfg(target_arch = "aarch64")]
    {
        let mut callers = [0u64; 16];
        let count = kernel::diag::panic_return_addresses(&mut callers);
        let _ = core::fmt::write(&mut Console, format_args!("ret addrs on stack:"));
        for address in &callers[..count] {
            let _ = core::fmt::write(&mut Console, format_args!(" {address:#x}"));
        }
        let _ = core::fmt::write(&mut Console, format_args!("\n"));
    }
    kumo_hal::active::halt()
}

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
        // `_start` is the only code that knows where the boot stack is; publish it before anything
        // can fault, so the Tower can tell a boot-stack address from a heap one.
        let lo = core::ptr::addr_of!(KERNEL_STACK) as u64;
        kernel::diag::set_boot_stack(lo, lo + 0x1_0000);
        kernel::kmain(boot)
    }

    #[panic_handler]
    fn panic(info: &PanicInfo<'_>) -> ! {
        crate::panic_report(info)
    }
}

// x86_64 freestanding entries. Two loaders, one kernel image:
// - GRUB (Multiboot2) or `qemu-system-x86_64 -kernel` (Multiboot1) enter `_start` in 32-bit
//   protected mode with `eax = boot magic` and `ebx = *multiboot_info`; the trampoline builds
//   an identity-mapped long mode and calls Rust with `(rdi = mbi, rsi = magic,
//   rdx = kernel_stack_top)`. Both Multiboot headers name `_start` explicitly.
// - Nijigumo (UEFI, PLAN/025 X1) enters the ELF entry `_start_bootinfo` already in long
//   mode on the firmware's identity page tables, with `rdi = *BootInfo` (the arm64 ABI).
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
mod boot_x86 {
    use core::panic::PanicInfo;

    core::arch::global_asm!(
        // ---- Multiboot1 header, must be 4-byte aligned in the first 8 KiB. The a.out
        //      "address kludge" (flag bit 16) gives explicit load addresses so the kernel
        //      can ship as a flat 64-bit binary — QEMU's `-kernel` Multiboot loader (and
        //      GRUB's `multiboot`) refuse a 64-bit ELF, but load a flat image fine.
        ".section .multiboot_header",
        ".align 4",
        "mb_header:",
        ".long 0x1BADB002",                 // MULTIBOOT_MAGIC
        ".long 0x00010003",                 // flags: AOUT_KLUDGE | ALIGN | MEMINFO
        ".long -(0x1BADB002 + 0x00010003)", // checksum
        ".long mb_header",                  // header_addr
        ".long __kernel_start",             // load_addr (start of the image, 16 MiB)
        ".long __load_end",                 // load_end_addr (end of file data)
        ".long __bss_end",                  // bss_end_addr (loader zeroes up to here)
        ".long _start",                     // entry_addr
        // ---- Multiboot2 header -------------------------------------------------------
        // GRUB's UEFI path cannot expose ACPI through Multiboot1. Keep both headers so QEMU's
        // direct MB1 loader remains a fast smoke while real UEFI/GRUB boots carry ACPI tags.
        ".align 8",
        "mb2_header:",
        ".long 0xe85250d6", // Multiboot2 header magic
        ".long 0",          // i386 protected-mode architecture
        ".long mb2_header_end - mb2_header",
        ".long -(0xe85250d6 + (mb2_header_end - mb2_header))",
        // Optional information request: prefer the ACPI v1/v2 tags when firmware has them.
        ".short 1",
        ".short 1",
        ".long 16",
        ".long 14",
        ".long 15",
        // Address tag for the same flat image used by the Multiboot1 a.out kludge.
        ".short 2",
        ".short 0",
        ".long 24",
        ".long mb2_header",
        ".long __kernel_start",
        ".long __load_end",
        ".long __bss_end",
        // Enter the common 32-bit trampoline.
        ".short 3",
        ".short 0",
        ".long 12",
        ".long _start",
        ".long 0", // 8-byte tag padding
        ".short 0",
        ".short 0",
        ".long 8",
        "mb2_header_end:",
        // ---- 32-bit entry stub --------------------------------------------------------
        ".section .text._start",
        ".code32",
        ".globl _start",
        "_start:",
        "  cli",
        "  movl $boot_stack_top, %esp",
        "  movl %ebx, mb_info_ptr", // stash multiboot info pointer
        "  movl %eax, mb_magic",    // stash boot magic
        // pml4[0] = pdpt | (PRESENT|WRITE)
        "  movl $pdpt, %eax",
        "  orl  $0x3, %eax",
        "  movl %eax, pml4",
        // pdpt[0] = pd | (PRESENT|WRITE)
        "  movl $pd, %eax",
        "  orl  $0x3, %eax",
        "  movl %eax, pdpt",
        // pdpt[3] + pd_ioapic[502] identity-map only 0xfec00000..0xfee00000.
        // This makes the firmware-described QEMU I/O APIC readable without mapping the
        // rest of the 3-4 GiB PCI/MMIO hole.
        "  movl $pd_ioapic, %eax",
        "  orl  $0x3, %eax",
        "  movl %eax, pdpt+24",
        "  movl $0xfec00083, pd_ioapic+4016",
        "  movl $0, pd_ioapic+4020",
        // pd[i] = (i*2MiB) | (PRESENT|WRITE|HUGE), i in 0..512  -> identity-map low 1 GiB
        "  xorl %ecx, %ecx",
        "  movl $0x83, %eax",
        "2:",
        "  movl %eax, pd(,%ecx,8)",
        "  movl $0, pd+4(,%ecx,8)",
        "  addl $0x200000, %eax",
        "  incl %ecx",
        "  cmpl $512, %ecx",
        "  jb   2b",
        // cr3 = pml4
        "  movl $pml4, %eax",
        "  movl %eax, %cr3",
        // cr4.PAE = 1
        "  movl %cr4, %eax",
        "  orl  $0x20, %eax",
        "  movl %eax, %cr4",
        // EFER.LME = 1  (MSR 0xC0000080, bit 8)
        "  movl $0xC0000080, %ecx",
        "  rdmsr",
        "  orl  $0x100, %eax",
        "  wrmsr",
        // cr0.PG = 1  (paging on -> long mode active)
        "  movl %cr0, %eax",
        "  orl  $0x80000000, %eax",
        "  movl %eax, %cr0",
        // load the 64-bit GDT and far-jump into the 64-bit code segment
        "  lgdt gdt64_ptr",
        "  ljmp $0x08, $long_mode_entry",
        // ---- 64-bit entry -------------------------------------------------------------
        ".code64",
        "long_mode_entry:",
        "  movw $0x10, %ax",
        "  movw %ax, %ss",
        "  movw %ax, %ds",
        "  movw %ax, %es",
        "  movw %ax, %fs",
        "  movw %ax, %gs",
        "  movq $boot_stack_top, %rsp",
        "  movl mb_info_ptr, %edi", // rdi = multiboot info (zero-extended)
        "  movl mb_magic, %esi",    // rsi = boot magic
        "  movq $boot_stack_top, %rdx", // rdx = TSS RSP0 for future ring-3 transitions
        "  call x86_kernel_entry",
        "3:",
        "  hlt",
        "  jmp 3b",
        // ---- 64-bit Nijigumo entry (ELF e_entry) -------------------------------------
        // Firmware's GDT selectors are unknown, so load the same GDT the trampoline uses and
        // reload CS with a far return before any Rust runs. RDI (BootInfo*) is preserved.
        ".section .text._start_bootinfo, \"ax\"",
        ".code64",
        ".globl _start_bootinfo",
        "_start_bootinfo:",
        "  cli",
        "  cld",
        "  movq $boot_stack_top, %rsp",
        "  lgdt gdt64_ptr64(%rip)",
        "  pushq $0x08",
        "  leaq 4f(%rip), %rax",
        "  pushq %rax",
        "  lretq",
        "4:",
        "  movw $0x10, %ax",
        "  movw %ax, %ss",
        "  movw %ax, %ds",
        "  movw %ax, %es",
        "  movw %ax, %fs",
        "  movw %ax, %gs",
        "  movq $boot_stack_top, %rsi", // rsi = TSS RSP0 for future ring-3 transitions
        "  call x86_kernel_entry_bootinfo",
        "5:",
        "  hlt",
        "  jmp 5b",
        // ---- 64-bit GDT: null, ring0 code (L=1), ring0 data ---------------------------
        ".section .rodata",
        ".align 8",
        "gdt64:",
        "  .quad 0",
        "  .quad 0x00AF9A000000FFFF",
        "  .quad 0x00CF92000000FFFF",
        "gdt64_end:",
        "gdt64_ptr:",
        "  .word gdt64_end - gdt64 - 1",
        "  .long gdt64",
        // 64-bit `lgdt` reads a 10-byte pseudo-descriptor (8-byte base).
        "gdt64_ptr64:",
        "  .word gdt64_end - gdt64 - 1",
        "  .quad gdt64",
        // ---- boot scratch: page tables, stack, saved handoff registers ---------------
        ".section .bss",
        ".align 4096",
        "pml4: .skip 4096",
        "pdpt: .skip 4096",
        "pd:   .skip 4096",
        "pd_ioapic: .skip 4096",
        ".align 16",
        "boot_stack: .skip 0x4000",
        "boot_stack_top:",
        "mb_info_ptr: .skip 4",
        "mb_magic:    .skip 4",
        options(att_syntax),
    );

    #[no_mangle]
    extern "C" fn x86_kernel_entry(mbi: u64, magic: u64, kernel_stack_top: u64) -> ! {
        kernel::x86_first_light(mbi, magic, kernel_stack_top)
    }

    #[no_mangle]
    extern "C" fn x86_kernel_entry_bootinfo(
        boot: *const kumo_abi::BootInfo,
        kernel_stack_top: u64,
    ) -> ! {
        kernel::x86_first_light_bootinfo(boot, kernel_stack_top)
    }

    #[panic_handler]
    fn panic(info: &PanicInfo<'_>) -> ! {
        crate::panic_report(info)
    }
}
