#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//j452
//j454
//j455
//j456
//j457
//j472
//j474

mod fpsimd;
mod gdt;
pub mod idt;
mod io_apic;
mod legacy_irq;
mod local_apic;
mod paging;
mod platform_acpi;
mod ring3;
mod userspace;

pub use fpsimd::{prove_fpsimd_boundary, FpSimdReport};
pub use gdt::{install as install_descriptor_tables, DescriptorTableReport};
pub use io_apic::{
    apply_boot_io_apic_timer, inspect_boot_io_apic, plan_boot_io_apic_timer,
    unmask_boot_io_apic_timer, IoApicRedirectionEntry, IoApicReport, IoApicTimerApplied,
    IoApicTimerPlan,
};
pub use platform_acpi::{
    discover_acpi_madt, discover_acpi_root, inspect_acpi_rsdp, AcpiLegacyIrqRoute, AcpiMadtReport,
    AcpiRootReport,
};
pub use ring3::{Ring3Error, Ring3Report, PING_TOKEN as RING3_PING_TOKEN};
pub use userspace::first_light_smoke as run_ring3_smoke;
pub use userspace::prepare_scheduled_fpsimd_smoke;
pub use userspace::prepare_scheduled_smoke as prepare_scheduled_ring3_smoke;
pub use userspace::prepare_scheduled_user_image;

pub const ARCH: &str = "x86_64";

pub fn arch_name() -> &'static str {
    ARCH
}

const XSTATE_BYTES: usize = 832;

/// Standard-format x87/SSE/AVX state. The first 512 bytes remain a valid `FXRSTOR64` image for
/// processors without AVX, while the full area covers the AVX component ending at byte 832.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
struct XState {
    bytes: [u8; XSTATE_BYTES],
}

impl XState {
    const fn initial() -> Self {
        let mut bytes = [0; XSTATE_BYTES];
        bytes[0] = 0x7f;
        bytes[1] = 0x03;
        bytes[24] = 0x80;
        bytes[25] = 0x1f;
        Self { bytes }
    }
}

/// x86_64 thread context. `r12_entry` and `r13_arg` seed a fresh thread; after first entry they
/// are ordinary callee-saved registers. Every switch also eagerly saves/restores the complete
/// enabled x87/SSE/AVX image, so user threads cannot inherit one another's vector state. CPUs
/// without AVX retain the architectural `FXSAVE64` fallback. — KESTREL
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(64))]
pub struct ThreadContext {
    r12_entry: u64,
    r13_arg: u64,
    rbx: u64,
    r14: u64,
    r15: u64,
    rbp: u64,
    rip: u64,
    rsp: u64,
    xstate: XState,
    user: bool,
}

impl Default for ThreadContext {
    fn default() -> Self {
        Self {
            r12_entry: 0,
            r13_arg: 0,
            rbx: 0,
            r14: 0,
            r15: 0,
            rbp: 0,
            rip: 0,
            rsp: 0,
            xstate: XState::initial(),
            user: false,
        }
    }
}

// Written once during single-core first light, before the first context switch. Assembly reads
// these symbols to select the AVX-capable XSAVE path without imposing AVX on baseline x86_64 CPUs.
// — KESTREL
#[no_mangle]
static mut KUMO_XSTATE_MASK: u64 = 0;
#[no_mangle]
static mut KUMO_XSAVE_ENABLED: u8 = 0;

pub(crate) unsafe fn configure_xsave(mask: u64) {
    unsafe {
        core::ptr::addr_of_mut!(KUMO_XSTATE_MASK).write_volatile(mask);
        core::ptr::addr_of_mut!(KUMO_XSAVE_ENABLED).write_volatile(1);
    }
}

impl ThreadContext {
    pub fn new(entry: usize, arg: usize, stack_top: usize, user: bool) -> Self {
        Self {
            r12_entry: entry as u64,
            r13_arg: arg as u64,
            rip: context_trampoline_addr(),
            rsp: stack_top as u64,
            user,
            ..Self::default()
        }
    }

    pub const fn entry(self) -> u64 {
        self.r12_entry
    }

    pub const fn arg(self) -> u64 {
        self.r13_arg
    }

    pub const fn stack_top(self) -> u64 {
        self.rsp
    }

    pub const fn is_user(self) -> bool {
        self.user
    }
}

#[cfg(target_os = "none")]
fn context_trampoline_addr() -> u64 {
    extern "C" {
        fn kumo_context_trampoline();
    }
    kumo_context_trampoline as *const () as usize as u64
}

#[cfg(not(target_os = "none"))]
fn context_trampoline_addr() -> u64 {
    0
}

#[cfg(target_os = "none")]
core::arch::global_asm!(
    ".section .text.kumo_context,\"ax\",@progbits",
    ".code64",
    ".global kumo_context_switch",
    ".type kumo_context_switch,@function",
    "kumo_context_switch:",
    // Preserve the SysV AMD64 callee-saved set in `prev`.
    "  mov %r12, 0(%rdi)",
    "  mov %r13, 8(%rdi)",
    "  mov %rbx, 16(%rdi)",
    "  mov %r14, 24(%rdi)",
    "  mov %r15, 32(%rdi)",
    "  mov %rbp, 40(%rdi)",
    // Save the caller continuation and the post-return stack pointer. Restoring with
    // `jmp` then has exactly the stack effect of returning from this call.
    "  mov (%rsp), %rax",
    "  mov %rax, 48(%rdi)",
    "  lea 8(%rsp), %rax",
    "  mov %rax, 56(%rdi)",
    // Eager FP ownership. Use standard-format XSAVE when first light enabled AVX; retain
    // FXSAVE for baseline x86_64 CPUs. Every state slot is 64-byte aligned.
    "  cmpb $0, KUMO_XSAVE_ENABLED(%rip)",
    "  je 8f",
    "  mov KUMO_XSTATE_MASK(%rip), %rax",
    "  mov %rax, %rdx",
    "  shr $32, %rdx",
    "  xsave64 64(%rdi)",
    "  mov KUMO_XSTATE_MASK(%rip), %rax",
    "  mov %rax, %rdx",
    "  shr $32, %rdx",
    "  xrstor64 64(%rsi)",
    "  jmp 9f",
    "8:",
    "  fxsave64 64(%rdi)",
    "  fxrstor64 64(%rsi)",
    "9:",
    // Load the next context. RSI remains the context pointer until the final indirect jump.
    "  mov 0(%rsi), %r12",
    "  mov 8(%rsi), %r13",
    "  mov 16(%rsi), %rbx",
    "  mov 24(%rsi), %r14",
    "  mov 32(%rsi), %r15",
    "  mov 40(%rsi), %rbp",
    "  mov 56(%rsi), %rsp",
    "  jmp *48(%rsi)",
    ".size kumo_context_switch,.-kumo_context_switch",
    ".global kumo_context_trampoline",
    ".type kumo_context_trampoline,@function",
    "kumo_context_trampoline:",
    // A fresh thread may have been selected from interrupt context. Re-enable maskable
    // interrupts before entering its body, then pass the seeded argument in SysV RDI.
    "  sti",
    "  mov %r13, %rdi",
    "  call *%r12",
    // Thread bodies are expected to terminate through the scheduler. A stray return must
    // not run into adjacent text.
    "1:",
    "  cli",
    "  hlt",
    "  jmp 1b",
    ".size kumo_context_trampoline,.-kumo_context_trampoline",
    ".global kumo_user_enter",
    ".type kumo_user_enter,@function",
    "kumo_user_enter:",
    // R12 points at UserState and R13 carries this thread's kernel-stack top. Install
    // TSS.RSP0 before CPL3 can take an interrupt, then switch to the process PML4.
    "  mov %r13, %rdi",
    "  call x86_set_user_kernel_stack",
    "  mov %r12, %r15",
    "  mov 272(%r15), %rax",
    "  mov %rax, %cr3",
    // Build the hardware privilege-return frame. Only arithmetic flags from `spsr` are
    // admitted; IF and the architecturally fixed bit are always set, while IOPL/NT/VM stay clear.
    "  mov $0x1b, %ax",
    "  mov %ax, %ds",
    "  mov %ax, %es",
    "  pushq $0x1b",
    "  pushq 264(%r15)",
    "  mov 256(%r15), %rax",
    "  and $0xcd5, %rax",
    "  or $0x202, %rax",
    "  push %rax",
    "  pushq $0x23",
    "  pushq 248(%r15)",
    // x[0..14] is Kumo's architecture-neutral initial register image. On AMD64 it
    // materializes as RDI,RSI,RDX,RCX,R8,R9,R10,R11,RAX,RBX,RBP,R12-R15.
    "  mov 0(%r15), %rdi",
    "  mov 8(%r15), %rsi",
    "  mov 16(%r15), %rdx",
    "  mov 24(%r15), %rcx",
    "  mov 32(%r15), %r8",
    "  mov 40(%r15), %r9",
    "  mov 48(%r15), %r10",
    "  mov 56(%r15), %r11",
    "  mov 64(%r15), %rax",
    "  mov 72(%r15), %rbx",
    "  mov 80(%r15), %rbp",
    "  mov 88(%r15), %r12",
    "  mov 96(%r15), %r13",
    "  mov 104(%r15), %r14",
    "  mov 112(%r15), %r15",
    "  iretq",
    ".size kumo_user_enter,.-kumo_user_enter",
    options(att_syntax),
);

#[cfg(target_os = "none")]
#[no_mangle]
extern "C" fn x86_set_user_kernel_stack(stack_top: u64) {
    gdt::set_kernel_stack(stack_top);
}

/// Switch from `prev` to `next`, returning only when another context restores `prev`.
///
/// # Safety
/// Both pointers must identify live, non-aliasing contexts. `next.rsp` must name a valid
/// stack and `next.rip` a valid continuation or the fresh-thread trampoline.
#[cfg(target_os = "none")]
pub unsafe fn switch_context(prev: *mut ThreadContext, next: *const ThreadContext) {
    extern "C" {
        fn kumo_context_switch(prev: *mut ThreadContext, next: *const ThreadContext);
    }
    unsafe { kumo_context_switch(prev, next) };
}

#[cfg(not(target_os = "none"))]
pub unsafe fn switch_context(_prev: *mut ThreadContext, _next: *const ThreadContext) {
    // Host stub — context switching is a no-op in tests.
}

const KERNEL_PAGE_PRESENT: u64 = 1 << 0;
const KERNEL_PAGE_RW: u64 = 1 << 1;
const KERNEL_PAGE_WRITE_THROUGH: u64 = 1 << 3;
const KERNEL_PAGE_CACHE_DISABLE: u64 = 1 << 4;
const KERNEL_PAGE_PS: u64 = 1 << 7;
const KERNEL_PAGE_GLOBAL: u64 = 1 << 8;
const KERNEL_PAGE_NO_EXECUTE: u64 = 1 << 63;
const KERNEL_PAGE_SIZE: u64 = 0x1000;
const KERNEL_PAGE_2M: u64 = 0x20_0000;
const KERNEL_PAGE_1G: u64 = 0x4000_0000;
const KERNEL_MIN_MAP_TOP: u64 = 4 * KERNEL_PAGE_1G;
const KERNEL_MAP_LIMIT: u64 = paging::USER_BASE;
const KERNEL_TABLE_ENTRIES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KernelMapPlan {
    top: u64,
    page_directories: usize,
    tables: usize,
}

fn kernel_map_plan(requested_top: u64) -> Option<KernelMapPlan> {
    if requested_top > KERNEL_MAP_LIMIT {
        return None;
    }
    let requested_top = requested_top.max(KERNEL_MIN_MAP_TOP);
    let top = requested_top.checked_add(KERNEL_PAGE_1G - 1)? & !(KERNEL_PAGE_1G - 1);
    if top > KERNEL_MAP_LIMIT {
        return None;
    }
    let page_directories = usize::try_from(top / KERNEL_PAGE_1G).ok()?;
    Some(KernelMapPlan {
        top,
        page_directories,
        // One PML4, one PDPT, then one page directory per mapped GiB.
        tables: 2usize.checked_add(page_directories)?,
    })
}

fn kernel_leaf_flags(is_ram: bool, is_framebuffer: bool, is_kernel: bool) -> u64 {
    let mut flags = KERNEL_PAGE_PRESENT | KERNEL_PAGE_RW | KERNEL_PAGE_PS | KERNEL_PAGE_GLOBAL;
    if !is_kernel {
        flags |= KERNEL_PAGE_NO_EXECUTE;
    }
    if !is_ram || is_framebuffer {
        // Safe first-light policy that does not depend on firmware PAT programming.
        flags |= KERNEL_PAGE_WRITE_THROUGH | KERNEL_PAGE_CACHE_DISABLE;
    }
    flags
}

fn ranges_overlap(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
    a_start < b_start.saturating_add(b_len) && b_start < a_start.saturating_add(a_len)
}

#[cfg(target_os = "none")]
fn alloc_kernel_table(
    alloc: &mut dyn FnMut() -> Option<u64>,
    tables: &mut usize,
) -> Result<u64, ()> {
    let frame = alloc().ok_or(())?;
    if frame == 0 || frame & (KERNEL_PAGE_SIZE - 1) != 0 {
        return Err(());
    }
    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, KERNEL_PAGE_SIZE as usize) };
    *tables = tables.checked_add(1).ok_or(())?;
    Ok(frame)
}

/// Build the permanent x86_64 supervisor identity map with 2 MiB leaves and switch CR3.
/// The map always covers the legacy 32-bit MMIO aperture, including the local and I/O APICs,
/// and may grow to (but never overlap) the PML4[1] userspace arena at 512 GiB.
/// Returns `(tables_used, bytes_mapped)`.
///
/// # Safety
/// Every frame returned by `alloc` must be unique, writable through the active identity map,
/// and unavailable to other owners. The caller must be executing in long mode with paging on.
#[cfg(target_os = "none")]
pub unsafe fn enable_kernel_mmu(
    top: u64,
    kernel_phys: u64,
    _kernel_virt: u64,
    kernel_len: u64,
    fb_phys: u64,
    fb_len: u64,
    is_ram: &dyn Fn(u64) -> bool,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<(usize, u64), ()> {
    let plan = kernel_map_plan(top).ok_or(())?;
    let mut tables = 0usize;
    let pml4_frame = alloc_kernel_table(alloc, &mut tables)?;
    let pdpt_frame = alloc_kernel_table(alloc, &mut tables)?;
    let pml4 = pml4_frame as *mut u64;
    let pdpt = pdpt_frame as *mut u64;
    unsafe {
        pml4.write_volatile(pdpt_frame | KERNEL_PAGE_PRESENT | KERNEL_PAGE_RW);
    }

    for directory_index in 0..plan.page_directories {
        let pd_frame = alloc_kernel_table(alloc, &mut tables)?;
        unsafe {
            pdpt.add(directory_index)
                .write_volatile(pd_frame | KERNEL_PAGE_PRESENT | KERNEL_PAGE_RW);
        }
        let pd = pd_frame as *mut u64;
        for leaf_index in 0..KERNEL_TABLE_ENTRIES {
            let phys = (directory_index as u64)
                .saturating_mul(KERNEL_PAGE_1G)
                .saturating_add((leaf_index as u64).saturating_mul(KERNEL_PAGE_2M));
            let framebuffer = ranges_overlap(phys, KERNEL_PAGE_2M, fb_phys, fb_len);
            let kernel = ranges_overlap(phys, KERNEL_PAGE_2M, kernel_phys, kernel_len);
            let flags = kernel_leaf_flags(is_ram(phys), framebuffer, kernel);
            unsafe { pd.add(leaf_index).write_volatile(phys | flags) };
        }
    }

    if tables != plan.tables {
        return Err(());
    }

    // NX is used by every non-kernel leaf. Long mode and CR0.PG are already active here.
    paging::enable_execute_disable();
    unsafe {
        core::arch::asm!(
            "mov cr3, {root}",
            root = in(reg) pml4_frame,
            options(nostack, preserves_flags),
        );
    }

    Ok((tables, plan.top))
}

/// Host stub: paging setup is a no-op (tests don't run on bare metal).
#[cfg(not(target_os = "none"))]
pub unsafe fn enable_kernel_mmu(
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

/// Accepted and ignored: a PL011 is an ARM peripheral and no x86_64 board has one — this
/// backend's console is the 16550 COM1 it already owns. Mirrored from the aarch64 backend so
/// the shared `stage_a` can inject a board's console base unconditionally (DESIGN/017 §4.3).
pub fn console_set_pl011_base(_base: u64) {}

/// Accepted and ignored: a Synopsys DW-APB UART is an ARM SoC peripheral (the RK3588 debug UART).
/// This backend's console is COM1 at a fixed port, so it needs no injected MMIO base. Mirrored
/// from the aarch64 backend so the shared `stage_a` can inject unconditionally (DESIGN/017 §4.3).
pub fn console_set_dw8250_base(_base: u64) {}

/// Accepted and ignored: the GIC is an ARM interrupt controller. This backend routes interrupts
/// through the APIC/IO-APIC chain it discovers from ACPI, which needs no board fallback.
/// Mirrored from the aarch64 backend so the shared `stage_a` can inject unconditionally
/// (DESIGN/017 §4.4).
pub fn gic_set_no_dtb_fallback(_distributor_base: u64, _redistributor_base: u64, _cpu_base: u64) {}

/// Diagnostic one-way latch mirroring the aarch64 backend: once set, [`early_console_write`]
/// drops output. Set via [`freeze_console`].
static CONSOLE_FROZEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn early_console_write(bytes: &[u8]) {
    if CONSOLE_FROZEN.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // On the freestanding kernel, drive the 16550 UART at COM1 (0x3F8) — the console
    // GRUB/QEMU leaves usable — so `klog!` produces real output. On the host (CI test
    // build) port I/O is privileged and meaningless, so this is a no-op there.
    #[cfg(target_os = "none")]
    serial::write(bytes);
    #[cfg(not(target_os = "none"))]
    let _ = bytes;
}

/// Freeze the console (drop subsequent output) — parity with the aarch64 backend so the
/// shared kernel fault hook resolves on both targets.
pub fn freeze_console() {
    CONSOLE_FROZEN.store(true, core::sync::atomic::Ordering::Relaxed);
}

pub fn handoff_framebuffer_console(_phys_base: u64, _len_bytes: u64) -> bool {
    false
}

pub fn framebuffer_console_owned_by_kernel() -> bool {
    true
}

pub fn reclaim_framebuffer_console() {}

/// 16550 UART (COM1) early console for the freestanding x86_64 kernel.
#[cfg(target_os = "none")]
pub(crate) mod serial {
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

pub fn clean_dcache_to_poc(_addr: usize, _len: usize) {
    // x86_64 data caches are hardware-coherent across cores (snooping), so a CPU writer
    // and a CPU reader of the same physical frame need no explicit clean. The aarch64
    // backend documents the hand-off hazard this guards against; here it is a no-op.
}

pub fn sync_icache_to_pou(_addr: usize, _len: usize) {
    // x86_64 instruction fetch snoops the (coherent) D-cache, so freshly-written code is
    // visible without an explicit I-cache invalidate. The aarch64 backend must clean to
    // PoC + invalidate the I-cache for VmarMap-loaded child code; here it is a no-op.
}

/// On-screen TOWER QR diagnostic — aarch64-only (X13s has no serial). No framebuffer panic
/// renderer on this backend, so it is a no-op; the human-readable banner still prints.
pub fn render_qr_diag(_text: &[u8]) {}

/// One-shot TOWER QR diagnostic — aarch64-only. No-op on x86_64.
pub fn render_qr_diag_once(_text: &[u8]) {}

/// Framebuffer geometry getter — no framebuffer console on this backend, so always zeros.
pub fn fb_geometry() -> (u32, u32, u32) {
    (0, 0, 0)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct El0Report {
    pub entered: bool,
    pub syscalls: u32,
    pub ping_echo: u64,
    pub exit_code: u64,
}

pub fn build_user_tables(
    image: &UserImage<'_>,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<u64, UserImageError> {
    userspace::build_user_tables(image, alloc)
}

pub fn run_el0_smoke(
    base: u64,
    stack_top: u64,
    stack_size: u64,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<El0Report, UserImageError> {
    userspace::run_el0_smoke(base, stack_top, stack_size, alloc)
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

/// Initial userspace execution context for scheduler-driven CPL3 entry.
///
/// The shared kernel names the register image `x` to keep one syscall-frame contract. AMD64
/// materializes x0..x14 as RDI, RSI, RDX, RCX, R8, R9, R10, R11, RAX, RBX, RBP, R12..R15.
/// `elr`, `spsr`, `sp_el0`, and `ttbr0` correspond to RIP, sanitized RFLAGS, RSP, and CR3.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UserState {
    pub x: [u64; 31],
    pub elr: u64,
    pub spsr: u64,
    pub sp_el0: u64,
    pub ttbr0: u64,
}

/// Build the kernel context that enters `state` at CPL3 on its first dispatch.
pub fn user_entry_context(state: *const UserState, kernel_stack_top: usize) -> ThreadContext {
    #[cfg(target_os = "none")]
    let rip = {
        extern "C" {
            fn kumo_user_enter();
        }
        kumo_user_enter as *const () as usize as u64
    };
    #[cfg(not(target_os = "none"))]
    let rip = 0;

    ThreadContext {
        r12_entry: state as u64,
        r13_arg: kernel_stack_top as u64,
        rip,
        rsp: kernel_stack_top as u64,
        user: true,
        ..ThreadContext::default()
    }
}

/// Select the ring-0 entry stack for the next scheduled userspace thread.
pub fn set_user_kernel_stack(kernel_stack_top: usize) {
    #[cfg(target_os = "none")]
    gdt::set_kernel_stack(kernel_stack_top as u64);
    #[cfg(not(target_os = "none"))]
    let _ = kernel_stack_top;
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
    pub uncached: bool,
    pub executable: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserImage<'a> {
    pub entry: u64,
    pub stack_top: u64,
    pub stack_size: u64,
    /// Bootstrap handle passed to the process in RDI at entry.
    pub bootstrap: u64,
    pub segments: &'a [UserLoadSegment<'a>],
    /// Extra physical mappings (RAM, framebuffer, or MMIO), materialized as 4 KiB leaves.
    pub extra_mappings: &'a [UserMapping],
}

pub fn run_el0_image(
    image: UserImage<'_>,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<El0Report, UserImageError> {
    userspace::run_el0_image(image, alloc)
}

pub fn set_svc_hook(hook: extern "C" fn(*mut u64)) {
    ring3::set_svc_hook(hook);
}

/// Register the containment hook for ring-3 CPU exceptions such as #PF, #UD, and #GP.
pub fn set_fault_hook(hook: extern "C" fn(u64, u64, u64, u64, u64, *const u64) -> !) {
    ring3::set_fault_hook(hook);
}

pub fn el0_exit(code: u64) -> ! {
    #[cfg(target_os = "none")]
    {
        ring3::resume(code)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = code;
        halt()
    }
}

pub fn syscall_count() -> u32 {
    ring3::syscall_count()
}

/// Install the x86_64 IDT ("the Tower"): 32 CPU-exception vectors report through the j435 handler,
/// and 16 remapped legacy-PIC vectors route through the silent j436 IRQ path.
pub fn install_exception_vectors() {
    #[cfg(target_os = "none")]
    idt::install();
}

/// How many CPU exceptions the Tower has fielded — a boot-time liveness proof for the IDT.
pub fn exceptions_seen() -> u64 {
    #[cfg(target_os = "none")]
    {
        idt::exceptions_seen()
    }
    #[cfg(not(target_os = "none"))]
    {
        0
    }
}

/// How many interrupts have reached the dedicated bootstrap I/O APIC timer vector.
pub fn io_apic_timer_irq_count() -> u64 {
    io_apic::timer_interrupt_count()
}

/// Exercise the installed I/O APIC timer vector without applying its masked route.
pub fn probe_io_apic_timer_interrupt() {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!(
            "int {vector}",
            vector = const io_apic::TIMER_VECTOR
        );
    }
}

/// Mask IRQ0 on the legacy PIC so the PIT stops delivering through vector 0x20 — a precondition for
/// unmasking the I/O APIC timer route so the timer is not delivered on two paths at once.
pub fn mask_pic_timer_source() {
    #[cfg(target_os = "none")]
    legacy_irq::mask_timer_source();
}

/// Wait (bounded) for the I/O APIC timer vector to reach `start + needed` controller-delivered
/// interrupts. The bound is a hang guard; the running local-APIC timer supplies the `hlt` beat.
pub fn wait_for_io_apic_timer_irqs(start: u64, needed: u64) -> u64 {
    #[cfg(target_os = "none")]
    {
        io_apic::wait_for_timer_interrupts(start, needed, 1 << 12)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = start;
        needed
    }
}

/// A hook the local-APIC timer IRQ calls (after EOI) to drive preemptive scheduling. Raw
/// `extern "C" fn()` address; 0 means none. Mirrors the aarch64 spine's `PREEMPT_HOOK`.
static PREEMPT_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Install the preemption hook (the scheduler tick). It runs in timer-IRQ context after the
/// local-APIC EOI and may context-switch.
pub fn set_preempt_hook(hook: extern "C" fn()) {
    PREEMPT_HOOK.store(hook as usize, core::sync::atomic::Ordering::Relaxed);
}

/// Stop calling the preemption hook (back to plain timer ticks).
pub fn clear_preempt_hook() {
    PREEMPT_HOOK.store(0, core::sync::atomic::Ordering::Relaxed);
}

/// Run the installed preemption hook, if any — called by the local-APIC timer ISR after EOI.
#[cfg(any(target_os = "none", test))]
pub(crate) fn run_preempt_hook() {
    let hook = PREEMPT_HOOK.load(core::sync::atomic::Ordering::Relaxed);
    if hook != 0 {
        // SAFETY: only ever set from `set_preempt_hook` with a real `extern "C" fn()`.
        let hook: extern "C" fn() = unsafe { core::mem::transmute(hook) };
        hook();
    }
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
    BadPeriod,
    Unsupported,
    Calibration,
}

pub fn init_timer_interrupts(_dtb: u64, period_hz: u64) -> Result<TimerIrqReport, TimerIrqError> {
    let setup = legacy_irq::timer_setup(period_hz).ok_or(TimerIrqError::BadPeriod)?;
    #[cfg(target_os = "none")]
    {
        legacy_irq::initialize(setup);
        irq_unmask();
    }
    Ok(TimerIrqReport {
        counter_hz: setup.input_hz,
        period_hz: setup.actual_hz,
        irq: 0,
        distributor_base: 0,
        redistributor_base: 0,
    })
}

pub fn timer_irq_count() -> u64 {
    legacy_irq::count()
}

/// Parity stub for the aarch64 GIC/TIMER heartbeat-failure probe (J472). The x86 timer
/// gate is PIC/PIT → IO APIC → local APIC; there is no GICv3 virtual-timer delivery
/// chain to snapshot, so the report is a fixed not-applicable line. The shared Stage-A
/// text only calls it on the aarch64 path; the symbol exists so `kumo_hal::active`
/// resolves identically under either backend. — PLOVER 2026-07-22
pub struct GicTimerGateReport {
    text: &'static str,
}

impl GicTimerGateReport {
    pub fn as_str(&self) -> &str {
        self.text
    }
}

pub fn gic_timer_gate_report(_seen: u64) -> GicTimerGateReport {
    GicTimerGateReport {
        text: "GIC / TIMER        Probe      n/a (x86_64 timer gate is PIC/APIC)\n",
    }
}

pub fn wait_for_timer_irqs(_start: u64, needed: u64, _timeout_ns: u64) -> u64 {
    #[cfg(target_os = "none")]
    {
        // The PIT is the first live x86 clock, so this proof has no independent deadline yet;
        // TSC/HPET calibration will make `timeout_ns` enforceable with the APIC timer lane.
        // HLT keeps the successful path idle between ticks. — KESTREL 2026-07-14
        legacy_irq::wait(_start, needed)
    }
    #[cfg(not(target_os = "none"))]
    {
        needed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalTimerReport {
    pub counter_hz: u64,
    pub period_hz: u64,
    pub vector: u32,
}

pub fn init_local_timer_from_reference(
    reference_hz: u64,
    period_hz: u64,
) -> Result<LocalTimerReport, TimerIrqError> {
    if reference_hz == 0 || period_hz == 0 {
        return Err(TimerIrqError::BadPeriod);
    }
    #[cfg(target_os = "none")]
    let setup = local_apic::initialize(reference_hz, period_hz).map_err(|err| match err {
        local_apic::Error::Unsupported => TimerIrqError::Unsupported,
        local_apic::Error::Calibration => TimerIrqError::Calibration,
    })?;
    #[cfg(not(target_os = "none"))]
    let setup = local_apic::TimerSetup {
        counter_hz: reference_hz,
        period_hz,
        initial_count: 1,
    };
    Ok(LocalTimerReport {
        counter_hz: setup.counter_hz,
        period_hz: setup.period_hz,
        vector: u32::from(local_apic::TIMER_VECTOR),
    })
}

pub fn local_timer_irq_count() -> u64 {
    local_apic::count()
}

pub fn wait_for_local_timer_irqs(_start: u64, needed: u64) -> u64 {
    #[cfg(target_os = "none")]
    {
        local_apic::wait(_start, needed)
    }
    #[cfg(not(target_os = "none"))]
    {
        needed
    }
}

pub fn irq_unmask() {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
}

/// Stub: the x86_64 physmap console migration lands with its paging slice.
pub fn console_use_physmap() {}

/// Stub: physical memory read not yet wired for x86_64.
pub fn read_phys(_phys: u64, _dest: &mut [u8]) {}

// ---- x86 user page-table primitives -------------------------------------------------

pub fn user_page_desc(executable: bool, writable: bool) -> u64 {
    paging::user_page_desc(executable, writable)
}

pub fn user_device_page_desc(writable: bool) -> u64 {
    paging::user_device_page_desc(writable)
}

pub fn user_nc_page_desc(writable: bool) -> u64 {
    paging::user_nc_page_desc(writable)
}

/// Map one 4 KiB leaf in an x86 process page-table root.
///
/// # Safety
/// `root`, `pa`, and frames returned by `alloc` must be identity-accessible writable RAM.
pub unsafe fn map_user_page(
    root: u64,
    va: u64,
    pa: u64,
    desc: u64,
    alloc: &mut dyn FnMut() -> Option<u64>,
    tables: &mut usize,
) -> Result<(), ()> {
    unsafe { paging::map_user_page(root, va, pa, desc, alloc, tables) }
}

/// Map a 2 MiB device window using page-granular uncached user leaves.
///
/// # Safety
/// `root`, `pa`, and frames returned by `alloc` must be identity-accessible writable RAM.
pub unsafe fn map_user_device_block(
    root: u64,
    va: u64,
    pa: u64,
    nc: bool,
    writable: bool,
    alloc: &mut dyn FnMut() -> Option<u64>,
    tables: &mut usize,
) -> Result<(), ()> {
    unsafe { paging::map_user_device_block(root, va, pa, nc, writable, alloc, tables) }
}

pub fn read_ttbr0() -> u64 {
    paging::read_root()
}

/// # Safety
/// `root` must identify a live, identity-accessible x86 PML4.
pub unsafe fn set_ttbr0(root: u64) {
    unsafe { paging::set_root(root) };
}

/// Arch-neutral name the kernel uses to switch the user address-space root (CR3 here).
///
/// # Safety
/// `root` must identify a live, identity-accessible x86 PML4.
pub unsafe fn set_user_aspace_root(root: u64) {
    unsafe { paging::set_root(root) };
}

/// Arch-neutral name the kernel uses to read the current user address-space root (CR3 here).
pub fn read_user_aspace_root() -> u64 {
    paging::read_root()
}

pub fn halt() -> ! {
    loop {
        #[cfg(target_os = "none")]
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack));
        }
        core::hint::spin_loop();
    }
}

pub fn spin_once() {
    core::hint::spin_loop();
}

pub fn configure_tlmm_gpio_interrupt(_pin: u32, _flags: u32, _irq_key: u32) -> bool {
    false
}

/// x86 has no GIC; device interrupt routing here is IOAPIC-based and configured elsewhere, so this
/// is a no-op success (the interrupt binding still registers). Exists so the arch-generic kernel
/// can name one `configure_spi_interrupt`.
/// No PSCI on x86; a reset here would go through the 0xcf9 reset control register or ACPI.
/// Exists so arch-generic kernel code can name one `system_reset`.
pub fn system_reset() {}

/// Wait for the console transmitter to drain. The x86 console is the 16550 debug port driven by
/// the same polled `putc`; nothing here yet distinguishes "queued" from "sent", so this is the
/// honest no-op until it does.
pub fn console_drain() {}

/// No PSCI on x86; power-off would go through ACPI. Returns so the caller falls back to a halt.
pub fn system_off() {}

/// Stop this CPU: mask interrupts, then park in `hlt`.
#[cfg(target_os = "none")]
pub fn halt_cpu() -> ! {
    unsafe {
        core::arch::asm!("cli", options(nostack, nomem));
        loop {
            core::arch::asm!("hlt", options(nostack, nomem));
        }
    }
}

#[cfg(not(target_os = "none"))]
pub fn halt_cpu() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

pub fn configure_spi_interrupt(_irq: u32) -> bool {
    true
}

/// No GIC on x86; device-line masking is IOAPIC-side and handled elsewhere.
pub fn mask_spi_interrupt(_irq: u32) -> bool {
    true
}

pub fn unmask_spi_interrupt(_irq: u32) -> bool {
    true
}

pub fn configure_i2c21_tlmm_pinctrl_from_dtb(_dtb: u64) -> Option<usize> {
    None
}

/// SMMU register window, mirrored from the aarch64 backend so arch-generic kernel code names one
/// type. There is no MMU-500 on the x86 backend; SMMU discovery is aarch64/X13s-only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppsSmmuTopology {
    pub base: u64,
    pub length: u64,
}

pub fn smmu_apps_discover_from_dtb(_dtb: u64) -> Option<AppsSmmuTopology> {
    None
}

pub fn complete_tlmm_gpio_interrupt(_irq_key: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn reports_arch_name() {
        assert_eq!(arch_name(), "x86_64");
    }

    #[test]
    fn kernel_map_plan_uses_one_page_directory_per_gib() {
        assert_eq!(
            kernel_map_plan(KERNEL_PAGE_1G),
            Some(KernelMapPlan {
                top: KERNEL_MIN_MAP_TOP,
                page_directories: 4,
                tables: 6,
            })
        );
        assert_eq!(
            kernel_map_plan(32 * KERNEL_PAGE_1G),
            Some(KernelMapPlan {
                top: 32 * KERNEL_PAGE_1G,
                page_directories: 32,
                tables: 34,
            })
        );
        assert_eq!(
            kernel_map_plan(KERNEL_MAP_LIMIT).unwrap().tables,
            KERNEL_TABLE_ENTRIES + 2
        );
        assert_eq!(kernel_map_plan(KERNEL_MAP_LIMIT + 1), None);
    }

    #[test]
    fn kernel_leaf_policy_is_wx_and_uncaches_devices() {
        let kernel_ram = kernel_leaf_flags(true, false, true);
        assert_eq!(kernel_ram & KERNEL_PAGE_NO_EXECUTE, 0);
        assert_eq!(
            kernel_ram & (KERNEL_PAGE_WRITE_THROUGH | KERNEL_PAGE_CACHE_DISABLE),
            0
        );

        let ordinary_ram = kernel_leaf_flags(true, false, false);
        assert_ne!(ordinary_ram & KERNEL_PAGE_NO_EXECUTE, 0);
        assert_eq!(ordinary_ram & KERNEL_PAGE_CACHE_DISABLE, 0);

        for device in [
            kernel_leaf_flags(false, false, false),
            kernel_leaf_flags(true, true, false),
        ] {
            assert_ne!(device & KERNEL_PAGE_NO_EXECUTE, 0);
            assert_eq!(
                device & (KERNEL_PAGE_WRITE_THROUGH | KERNEL_PAGE_CACHE_DISABLE),
                KERNEL_PAGE_WRITE_THROUGH | KERNEL_PAGE_CACHE_DISABLE
            );
        }
    }

    #[test]
    fn thread_context_layout_matches_switch_assembly() {
        assert_eq!(core::mem::offset_of!(ThreadContext, r12_entry), 0);
        assert_eq!(core::mem::offset_of!(ThreadContext, r13_arg), 8);
        assert_eq!(core::mem::offset_of!(ThreadContext, rbx), 16);
        assert_eq!(core::mem::offset_of!(ThreadContext, r14), 24);
        assert_eq!(core::mem::offset_of!(ThreadContext, r15), 32);
        assert_eq!(core::mem::offset_of!(ThreadContext, rbp), 40);
        assert_eq!(core::mem::offset_of!(ThreadContext, rip), 48);
        assert_eq!(core::mem::offset_of!(ThreadContext, rsp), 56);
        assert_eq!(core::mem::offset_of!(ThreadContext, xstate), 64);
        assert_eq!(core::mem::offset_of!(ThreadContext, user), 896);
        assert_eq!(core::mem::align_of::<ThreadContext>(), 64);
        assert_eq!(core::mem::size_of::<ThreadContext>(), 960);
    }

    #[test]
    fn fresh_thread_xstate_supports_fx_fallback_and_xsave_init() {
        let context = ThreadContext::default();
        assert_eq!(&context.xstate.bytes[..2], &0x037fu16.to_le_bytes());
        assert_eq!(&context.xstate.bytes[24..28], &0x1f80u32.to_le_bytes());
        assert!(context.xstate.bytes[28..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn fresh_thread_context_retains_entry_argument_and_stack() {
        let context = ThreadContext::new(0x1234, 0x5678, 0x9000, false);
        assert_eq!(context.entry(), 0x1234);
        assert_eq!(context.arg(), 0x5678);
        assert_eq!(context.stack_top(), 0x9000);
        assert!(!context.is_user());
        assert_eq!(context.rip, 0, "host trampoline is deliberately absent");
    }

    #[test]
    fn user_context_layout_matches_entry_assembly() {
        let state = UserState {
            x: [0; 31],
            elr: 0x8000_1234,
            spsr: 0,
            sp_el0: 0x1000_0000_0000,
            ttbr0: 0x2000,
        };
        assert_eq!(core::mem::offset_of!(UserState, elr), 248);
        assert_eq!(core::mem::offset_of!(UserState, spsr), 256);
        assert_eq!(core::mem::offset_of!(UserState, sp_el0), 264);
        assert_eq!(core::mem::offset_of!(UserState, ttbr0), 272);
        assert_eq!(core::mem::size_of::<UserState>(), 280);

        let context = user_entry_context(&state, 0x9000);
        assert_eq!(context.entry(), &state as *const UserState as u64);
        assert_eq!(context.arg(), 0x9000);
        assert_eq!(context.stack_top(), 0x9000);
        assert!(context.is_user());
        assert_eq!(
            context.rip, 0,
            "host user trampoline is deliberately absent"
        );
    }

    static PROBE: AtomicU64 = AtomicU64::new(0);

    extern "C" fn probe() {
        PROBE.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn preempt_hook_runs_only_while_installed() {
        // No hook installed -> running it does nothing.
        PROBE.store(0, Ordering::Relaxed);
        clear_preempt_hook();
        run_preempt_hook();
        assert_eq!(PROBE.load(Ordering::Relaxed), 0);

        // Installed -> each run fires it exactly once.
        set_preempt_hook(probe);
        run_preempt_hook();
        run_preempt_hook();
        assert_eq!(PROBE.load(Ordering::Relaxed), 2);

        // Cleared -> stops firing.
        clear_preempt_hook();
        run_preempt_hook();
        assert_eq!(PROBE.load(Ordering::Relaxed), 2);
    }
}

pub fn iommu_init(_kind: u32, _phys_base: u64, _len: u64) -> bool {
    false
}

pub fn iommu_create_device_context(
    _kind: u32,
    _phys_base: u64,
    _stream_id: u32,
    _pgd_phys: u64,
) -> bool {
    false
}

pub fn iommu_destroy_device_context(_kind: u32, _phys_base: u64, _stream_id: u32) {}

pub fn iommu_map_device_page(
    _kind: u32,
    _phys_base: u64,
    _stream_id: u32,
    _pgd_phys: u64,
    _iova: u64,
    _phys: u64,
    _rights: u32,
) -> bool {
    false
}

pub fn iommu_unmap_device_range(
    _kind: u32,
    _phys_base: u64,
    _stream_id: u32,
    _pgd_phys: u64,
    _iova: u64,
    _len: u64,
) -> bool {
    false
}
