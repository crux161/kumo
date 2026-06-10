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

// x86_64 freestanding entry: booted by GRUB (Multiboot) or `qemu-system-x86_64 -kernel`
// (Multiboot1). The loader enters `_start` in 32-bit protected mode with `eax = boot
// magic` and `ebx = *multiboot_info`; the trampoline builds an identity-mapped long
// mode and tail-calls Rust with `(rdi = mbi, rsi = magic)`.
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
        ".long 0x100000",                   // load_addr (start of the image)
        ".long __load_end",                 // load_end_addr (end of file data)
        ".long __bss_end",                  // bss_end_addr (loader zeroes up to here)
        ".long _start",                     // entry_addr
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
        "  call x86_kernel_entry",
        "3:",
        "  hlt",
        "  jmp 3b",
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
        // ---- boot scratch: page tables, stack, saved handoff registers ---------------
        ".section .bss",
        ".align 4096",
        "pml4: .skip 4096",
        "pdpt: .skip 4096",
        "pd:   .skip 4096",
        ".align 16",
        "boot_stack: .skip 0x4000",
        "boot_stack_top:",
        "mb_info_ptr: .skip 4",
        "mb_magic:    .skip 4",
        options(att_syntax),
    );

    #[no_mangle]
    extern "C" fn x86_kernel_entry(mbi: u64, magic: u64) -> ! {
        kernel::x86_first_light(mbi, magic)
    }

    #[panic_handler]
    fn panic(_info: &PanicInfo<'_>) -> ! {
        kumo_hal::active::halt()
    }
}
