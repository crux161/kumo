#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub const ARCH: &str = "x86_64";

pub fn arch_name() -> &'static str {
    ARCH
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ThreadContext {
    rip: u64,
    arg: u64,
    rsp: u64,
    user: bool,
}

impl ThreadContext {
    pub fn new(entry: usize, arg: usize, stack_top: usize, user: bool) -> Self {
        Self {
            rip: entry as u64,
            arg: arg as u64,
            rsp: stack_top as u64,
            user,
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

pub unsafe fn switch_context(_prev: *mut ThreadContext, _next: *const ThreadContext) {
    // x86_64 context switching lands with the x86 backend parity milestone.
}

pub fn early_console_write(_bytes: &[u8]) {
    // Kept as a later backend checkpoint; no x86 port I/O is wired in M0.
}

pub fn set_framebuffer(_base: u64, _len_bytes: u64, _width: u32, _height: u32, _stride: u32) {
    // x86_64 is co-equal in CI but not yet bring-up hardware; the framebuffer
    // console lands with the x86_64 metal milestone. Stubbed so the shared kernel
    // can call it unconditionally.
}

pub fn install_exception_vectors() {
    // The x86_64 IDT (and its fault handlers) land with the x86_64 metal milestone.
    // Stubbed so the shared kernel can call it unconditionally.
}

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
    Err(TimerIrqError::Unsupported)
}

pub fn timer_irq_count() -> u64 {
    0
}

pub fn wait_for_timer_irqs(_start: u64, _needed: u64, _timeout_ns: u64) -> u64 {
    0
}

pub fn halt() -> ! {
    loop {
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
