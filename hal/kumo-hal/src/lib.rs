#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign};

#[cfg(all(feature = "arch_aarch64", feature = "arch_x86_64"))]
compile_error!("select exactly one HAL backend");

#[cfg(not(any(feature = "arch_aarch64", feature = "arch_x86_64")))]
compile_error!("select a HAL backend");

#[cfg(feature = "arch_aarch64")]
pub mod active {
    pub use kumo_hal_aarch64::{
        arch_name, build_user_tables, clean_dcache_to_poc, clear_preempt_hook, console_read_byte,
        console_use_physmap, early_console_write, el0_exit, enable_kernel_mmu, fb_fill,
        fb_paint_band, halt, init_timer_interrupts, install_exception_vectors, irq_unmask,
        map_user_device_block, map_user_page, monotonic_nanos, read_phys, read_user_aspace_root,
        run_el0_image, run_el0_smoke, set_fault_hook, set_framebuffer, set_interrupt_hook,
        set_preempt_hook, set_svc_hook, set_user_aspace_root, spin_once, switch_context,
        syscall_count, timer_irq_count, user_device_page_desc, user_nc_page_desc, user_page_desc,
        wait_for_timer_irqs, El0Report, ThreadContext, UserImage, UserImageError, UserLoadSegment,
        UserMapping, UserState, ARCH,
    };
}

#[cfg(feature = "arch_x86_64")]
pub mod active {
    pub use kumo_hal_x86_64::{
        arch_name, build_user_tables, clean_dcache_to_poc, clear_preempt_hook, console_read_byte,
        console_use_physmap, early_console_write, el0_exit, enable_kernel_mmu, fb_fill,
        fb_paint_band, halt, init_timer_interrupts, install_exception_vectors, irq_unmask,
        map_user_device_block, map_user_page, monotonic_nanos, read_phys, read_user_aspace_root,
        run_el0_image, run_el0_smoke, set_fault_hook, set_framebuffer, set_interrupt_hook,
        set_preempt_hook, set_svc_hook, set_user_aspace_root, spin_once, switch_context,
        syscall_count, timer_irq_count, user_device_page_desc, user_nc_page_desc, user_page_desc,
        wait_for_timer_irqs, El0Report, ThreadContext, UserImage, UserImageError, UserLoadSegment,
        UserMapping, UserState, ARCH,
    };
}

pub type ObjectId = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McErr {
    Unsupported,
    InvalidBlob,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapErr {
    InvalidRange,
    AccessDenied,
    NoMemory,
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageFlags(pub u64);

impl PageFlags {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const EXECUTE: Self = Self(1 << 2);
    /// EL0-accessible (user page, as opposed to kernel-only).
    pub const USER: Self = Self(1 << 3);
    /// MMIO — map as Device-nGnRnE, not Normal cacheable memory.
    pub const DEVICE: Self = Self(1 << 4);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, needed: Self) -> bool {
        self.0 & needed.0 == needed.0
    }
}

impl BitOr for PageFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for PageFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for PageFlags {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for PageFlags {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

pub trait Context: Sized {
    fn new(entry: usize, arg: usize, stack_top: usize, user: bool) -> Self;

    unsafe fn switch(prev: *mut Self, next: *const Self);
}

pub trait Mmu {
    type AddressSpace;

    fn new_kernel_space() -> Result<Self::AddressSpace, MapErr>;
    fn map(
        space: &mut Self::AddressSpace,
        virt: u64,
        phys: u64,
        len: u64,
        flags: PageFlags,
    ) -> Result<(), MapErr>;
    fn unmap(space: &mut Self::AddressSpace, virt: u64, len: u64) -> Result<(), MapErr>;
    fn activate(space: &Self::AddressSpace);
    fn flush_tlb(virt: Option<u64>);
}

pub trait InterruptController {
    fn init();
    fn eoi(irq: u32);
    fn set_mask(irq: u32, masked: bool);
    fn bind_object(irq: u32, object: ObjectId);
}

pub trait Timer {
    fn init(period_hz: u64);
    fn monotonic_nanos() -> u64;
}

pub trait Cpu {
    fn id() -> u32;
    fn halt() -> !;
    fn spin_once();
}

pub trait EarlyConsole {
    fn write(bytes: &[u8]);
}

pub trait Microcode {
    fn current_revision() -> u32;
    fn apply(blob: &[u8]) -> Result<u32, McErr>;
}
