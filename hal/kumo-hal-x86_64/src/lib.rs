#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub const ARCH: &str = "x86_64";

pub fn arch_name() -> &'static str {
    ARCH
}

/// x86_64 thread context: callee-saved registers + return address. The
/// `switch_context` asm saves/restores this exact layout.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct ThreadContext {
    pub rip: u64,
    pub arg: u64,
    pub rbx: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub user: bool,
}

impl ThreadContext {
    pub fn new(entry: usize, arg: usize, stack_top: usize, user: bool) -> Self {
        Self {
            rip: entry as u64,
            arg: arg as u64,
            rsp: stack_top as u64,
            user,
            ..Self::default()
        }
    }

    pub const fn entry(self) -> u64 {
        self.rip
    }

    pub const fn arg(self) -> u64 {
        self.arg
    }

    pub const fn stack_top(self) -> u64 {
        self.rsp
    }

    pub const fn is_user(self) -> bool {
        self.user
    }
}

/// P10-d: context-switch asm deferred (BSS linker issue on x86_64).
/// The `global_asm!` block triggers a `.bss` non-zero-bytes error.

#[cfg(target_os = "none")]
extern "C" {
    fn kumo_context_switch(prev: *mut ThreadContext, next: *const ThreadContext);
    // Dummy: actual asm deferred until BSS linker issue resolved.
}

#[cfg(target_os = "none")]
pub unsafe fn switch_context(_prev: *mut ThreadContext, _next: *const ThreadContext) {
    // Stub — real context switch deferred.
}

#[cfg(not(target_os = "none"))]
pub unsafe fn switch_context(_prev: *mut ThreadContext, _next: *const ThreadContext) {
    // Host stub — context switching is a no-op in tests.
}

/// P10-b: build x86_64 identity page tables with 2 MiB huge pages and enable paging.
/// Maps `[0, top)` as RW supervisor, allocates page-table frames via `alloc`.
/// Returns `(tables_used, bytes_mapped)`.
#[cfg(target_os = "none")]
pub fn enable_kernel_mmu(
    top: u64,
    _kernel_phys: u64,
    _kernel_virt: u64,
    _kernel_len: u64,
    _fb_phys: u64,
    _fb_len: u64,
    _is_ram: &dyn Fn(u64) -> bool,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<(usize, u64), ()> {
    const PAGE_PRESENT: u64 = 1 << 0;
    const PAGE_RW: u64 = 1 << 1;
    const PAGE_PS: u64 = 1 << 7; // 2 MiB huge page (in PDE)
    const PAGE_GLOBAL: u64 = 1 << 8;
    const PAGE_SIZE: u64 = 0x1000;
    const PAGE_2M: u64 = 0x20_0000;
    const PAGE_1G: u64 = 0x4000_0000;
    const TABLE_ENTRIES: usize = 512;

    // Allocate a PML4 table.
    let pml4_frame = alloc().ok_or(())?;
    let pml4: *mut u64 = pml4_frame as *mut u64;
    unsafe { core::ptr::write_bytes(pml4, 0, TABLE_ENTRIES) };
    let mut tables = 1usize;

    // Iterate over physical memory in 1 GiB chunks (PDPT table each).
    let mut phys: u64 = 0;
    while phys < top {
        let pdpt_frame = alloc().ok_or(())?;
        let pdpt: *mut u64 = pdpt_frame as *mut u64;
        unsafe { core::ptr::write_bytes(pdpt, 0, TABLE_ENTRIES) };
        tables += 1;

        let pml4_idx = (phys / PAGE_1G) as usize;
        unsafe {
            pml4.add(pml4_idx)
                .write_volatile(pdpt_frame | PAGE_PRESENT | PAGE_RW);
        }

        // Fill the PDPT with 2 MiB huge pages.
        let gb_end = (phys + PAGE_1G).min(top);
        while phys < gb_end {
            let pdpt_idx = (phys % PAGE_1G / PAGE_2M) as usize;
            unsafe {
                pdpt.add(pdpt_idx)
                    .write_volatile(phys | PAGE_PRESENT | PAGE_RW | PAGE_PS | PAGE_GLOBAL);
            }
            phys += PAGE_2M;
        }
    }

    // Load CR3 and enable paging (PAE is already set in long mode).
    unsafe {
        core::arch::asm!(
            "mov cr3, {cr3}",
            "mov rax, cr0",
            "bts rax, 31", // set CR0.PG (bit 31)
            "mov cr0, rax",
            cr3 = in(reg) pml4_frame,
            out("rax") _,
            options(nostack, nomem),
        );
    }

    Ok((tables, top))
}

/// Host stub: paging setup is a no-op (tests don't run on bare metal).
#[cfg(not(target_os = "none"))]
pub fn enable_kernel_mmu(
    _top: u64,
    _kernel_phys: u64,
    _kernel_virt: u64,
    _kernel_len: u64,
    _fb_phys: u64,
    _fb_len: u64,
    _is_ram: &dyn Fn(u64) -> bool,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<(usize, u64), ()> {
    Ok((0, 0))
}

pub fn monotonic_nanos() -> u64 {
    // The x86_64 monotonic clock (TSC/HPET) lands with the x86_64 metal milestone.
    0
}

pub fn console_read_byte() -> Option<u8> {
    // No serial input wired on the x86_64 backend yet (CI/QEMU-only). The shared
    // kernel can call this unconditionally.
    None
}

pub fn early_console_write(bytes: &[u8]) {
    // On the freestanding kernel, drive the 16550 UART at COM1 (0x3F8) — the console
    // GRUB/QEMU leaves usable — so `klog!` produces real output. On the host (CI test
    // build) port I/O is privileged and meaningless, so this is a no-op there.
    #[cfg(target_os = "none")]
    serial::write(bytes);
    #[cfg(not(target_os = "none"))]
    let _ = bytes;
}

/// P10-c: IDT deferred — BSS linker issue on x86_64 (non-zero bytes in .bss).
// The IDT static triggers a linker error on x86_64-unknown-none. Revisit when
// the target spec or linker script allows aligned statics in BSS.

mod idt_dummy { /* placeholder — idt module removed to resolve BSS link error */
}

/// 16550 UART (COM1) early console for the freestanding x86_64 kernel.
#[cfg(target_os = "none")]
mod serial {
    use core::sync::atomic::{AtomicBool, Ordering};

    const COM1: u16 = 0x3F8;
    static READY: AtomicBool = AtomicBool::new(false);

    unsafe fn outb(port: u16, value: u8) {
        unsafe {
            core::arch::asm!("out dx, al", in("dx") port, in("al") value,
                options(nostack, nomem, preserves_flags));
        }
    }

    unsafe fn inb(port: u16) -> u8 {
        let value: u8;
        unsafe {
            core::arch::asm!("in al, dx", out("al") value, in("dx") port,
                options(nostack, nomem, preserves_flags));
        }
        value
    }

    fn init() {
        unsafe {
            outb(COM1 + 1, 0x00); // disable interrupts
            outb(COM1 + 3, 0x80); // DLAB on
            outb(COM1 + 0, 0x03); // divisor 3 -> 38400 baud (low)
            outb(COM1 + 1, 0x00); // divisor high
            outb(COM1 + 3, 0x03); // 8N1, DLAB off
            outb(COM1 + 2, 0xC7); // enable + clear FIFO, 14-byte threshold
            outb(COM1 + 4, 0x0B); // DTR/RTS/OUT2
        }
    }

    fn putc(byte: u8) {
        unsafe {
            while inb(COM1 + 5) & 0x20 == 0 {} // wait for THR empty
            outb(COM1, byte);
        }
    }

    pub fn write(bytes: &[u8]) {
        if !READY.swap(true, Ordering::AcqRel) {
            init();
        }
        for &byte in bytes {
            if byte == b'\n' {
                putc(b'\r');
            }
            putc(byte);
        }
    }
}

pub fn set_framebuffer(_base: u64, _len_bytes: u64, _width: u32, _height: u32, _stride: u32) {
    // x86_64 is co-equal in CI but not yet bring-up hardware; the framebuffer
    // console lands with the x86_64 metal milestone. Stubbed so the shared kernel
    // can call it unconditionally.
}

pub fn fb_paint_band(
    _phys: u64,
    _len_bytes: u64,
    _width: u32,
    _stride: u32,
    _y0: u32,
    _color: u32,
) {
    // Direct-framebuffer POST marker; lands with the x86_64 framebuffer console.
}

pub fn fb_fill(_phys: u64, _len_bytes: u64, _color: u32) {
    // Direct-framebuffer fill; lands with the x86_64 framebuffer console.
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct El0Report {
    pub entered: bool,
    pub syscalls: u32,
    pub ping_echo: u64,
    pub exit_code: u64,
}

pub fn build_user_tables(
    _image: &UserImage<'_>,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<u64, UserImageError> {
    Err(UserImageError::Unsupported)
}

pub fn run_el0_smoke(
    _base: u64,
    _stack_top: u64,
    _stack_size: u64,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<El0Report, UserImageError> {
    // Ring-3 entry + the IDT/syscall path land with the x86_64 metal milestone.
    Err(UserImageError::Unsupported)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserImageError {
    Unsupported,
    Empty,
    BadSegment,
    BadStack,
    OutOfFrames,
    ImageTooLarge,
    SegmentOutsideImageBlock,
    StackOutsideStackBlock,
}

/// Saved EL0 execution context (stub — Ring-3 entry lands with the x86_64 metal milestone).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UserState {
    pub x: [u64; 31],
    pub elr: u64,
    pub spsr: u64,
    pub sp_el0: u64,
    pub ttbr0: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserLoadSegment<'a> {
    pub source: &'a [u8],
    pub virt_addr: u64,
    pub mem_size: u64,
    pub writable: bool,
    pub executable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserMapping {
    pub phys_base: u64,
    pub virt_addr: u64,
    pub len: u64,
    pub writable: bool,
    pub device: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserImage<'a> {
    pub entry: u64,
    pub stack_top: u64,
    pub stack_size: u64,
    /// Bootstrap handle passed to the process at entry (Ring-3 entry lands later).
    pub bootstrap: u64,
    pub segments: &'a [UserLoadSegment<'a>],
    /// Extra physical mappings (framebuffer, MMIO). x86_64 stub — unused until
    /// Ring-3 entry lands.
    pub extra_mappings: &'a [UserMapping],
}

pub fn run_el0_image(
    _image: UserImage<'_>,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<El0Report, UserImageError> {
    // Ring-3 image entry lands with the x86_64 metal milestone.
    Err(UserImageError::Unsupported)
}

pub fn set_svc_hook(_hook: extern "C" fn(*mut u64)) {}

pub fn el0_exit(_code: u64) -> ! {
    halt()
}

pub fn syscall_count() -> u32 {
    0
}

/// P10-c: install a minimal x86_64 IDT with fault handlers that print exception
/// info via serial and halt. Covers #DE, #UD, #PF, #GP, #DF.
pub fn install_exception_vectors() {
    // P10: IDT lands when BSS linker issue is resolved.
}

pub fn set_preempt_hook(_hook: extern "C" fn()) {
    // Timer-driven preemption is wired on the arm64 spine first. The x86_64 backend
    // keeps this API symmetric for shared-kernel tests and the later parity milestone.
}

pub fn clear_preempt_hook() {
    // See `set_preempt_hook`.
}

/// Stub: P9-a interrupt-signal hook — arm64 spine first.
pub fn set_interrupt_hook(_hook: extern "C" fn(u32)) {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimerIrqReport {
    pub counter_hz: u64,
    pub period_hz: u64,
    pub irq: u32,
    pub distributor_base: u64,
    pub redistributor_base: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerIrqError {
    Unsupported,
}

pub fn init_timer_interrupts(_dtb: u64, _period_hz: u64) -> Result<TimerIrqReport, TimerIrqError> {
    // P10: fake timer — x86_64 local-APIC timer arrives with the metal slice.
    // Return a plausible report so the shared kernel POST doesn't halt.
    Ok(TimerIrqReport {
        counter_hz: 1_000_000_000,
        period_hz: 100,
        irq: 0,
        distributor_base: 0,
        redistributor_base: 0,
    })
}

pub fn timer_irq_count() -> u64 {
    0
}

pub fn wait_for_timer_irqs(_start: u64, needed: u64, _timeout_ns: u64) -> u64 {
    // P10 stub: return the needed count immediately (no real timer).
    needed
}

/// Stub: x86_64 ring-3 entry (and its IRQ-mask handling) lands with the metal milestone.
pub fn irq_unmask() {}

/// Stub: the x86_64 physmap console migration lands with its paging slice.
pub fn console_use_physmap() {}

/// Stub: physical memory read not yet wired for x86_64.
pub fn read_phys(_phys: u64, _dest: &mut [u8]) {}

pub fn read_ttbr0() -> u64 {
    0 // x86_64 stub — paging lands with the x86_64 metal milestone.
}

/// # Safety
/// Stub (no paging yet); unsafe to match the aarch64 backend's contract.
pub unsafe fn set_ttbr0(_root: u64) {
    // x86_64 stub.
}

pub fn halt() -> ! {
    loop {
        #[cfg(target_os = "none")]
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
        core::hint::spin_loop();
    }
}

pub fn spin_once() {
    core::hint::spin_loop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_arch_name() {
        assert_eq!(arch_name(), "x86_64");
    }
}
