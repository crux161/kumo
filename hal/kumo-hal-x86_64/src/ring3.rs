//! x86_64 ring-3 transition and `int 0x80` dispatch mechanics.
//!
//! Address-space construction lives in `userspace`; this module owns the privilege boundary:
//! `iretq` entry, a dedicated TSS.RSP0 kernel stack, the tiny first-light payload, and return to
//! the suspended kernel flow. — KESTREL

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub const SYSCALL_VECTOR: usize = 0x80;
pub const PING_TOKEN: u64 = 0x4b55_4d4f_c0de_cafe;

const OP_PING: u64 = 1;
const OP_EXIT: u64 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ring3Error {
    Unsupported,
    UserImage(crate::UserImageError),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Ring3Report {
    pub entered: bool,
    pub calls: u32,
    pub ping_echo: u64,
    pub exit_code: u64,
    pub code_address: u64,
    pub stack_top: u64,
}

impl Ring3Report {
    pub const fn is_live(self) -> bool {
        self.entered && self.calls == 2 && self.ping_echo == PING_TOKEN && self.exit_code == 0
    }
}

pub(crate) enum Dispatch {
    Return(u64),
    Exit(u64),
}

static CALLS: AtomicU32 = AtomicU32::new(0);
static PING_ECHO: AtomicU64 = AtomicU64::new(0);
static SVC_HOOK: AtomicUsize = AtomicUsize::new(0);
static FAULT_HOOK: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn set_svc_hook(hook: extern "C" fn(*mut u64)) {
    SVC_HOOK.store(hook as usize, Ordering::Release);
}

pub(crate) fn svc_hook() -> Option<extern "C" fn(*mut u64)> {
    let hook = SVC_HOOK.load(Ordering::Acquire);
    (hook != 0).then(|| unsafe { core::mem::transmute(hook) })
}

pub(crate) fn set_fault_hook(hook: extern "C" fn(u64, u64, u64, u64, u64, *const u64) -> !) {
    FAULT_HOOK.store(hook as usize, Ordering::Release);
}

pub(crate) fn fault_hook() -> Option<extern "C" fn(u64, u64, u64, u64, u64, *const u64) -> !> {
    let hook = FAULT_HOOK.load(Ordering::Acquire);
    (hook != 0).then(|| unsafe { core::mem::transmute(hook) })
}

pub(crate) fn record_call() {
    CALLS.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn dispatch(operation: u64, argument: u64) -> Dispatch {
    record_call();
    match operation {
        OP_PING => {
            PING_ECHO.store(argument, Ordering::Relaxed);
            Dispatch::Return(argument)
        }
        OP_EXIT => Dispatch::Exit(argument),
        _ => Dispatch::Exit(u64::MAX),
    }
}

#[cfg(target_os = "none")]
pub(crate) fn reset_counters() {
    CALLS.store(0, Ordering::Relaxed);
    PING_ECHO.store(0, Ordering::Relaxed);
}

pub(crate) fn syscall_count() -> u32 {
    CALLS.load(Ordering::Relaxed)
}

#[cfg(target_os = "none")]
pub(crate) fn ping_echo() -> u64 {
    PING_ECHO.load(Ordering::Relaxed)
}

#[cfg(target_os = "none")]
mod metal {
    #[repr(C, align(16))]
    struct KernelEntryStack([u8; 0x4000]);

    static mut KERNEL_ENTRY_STACK: KernelEntryStack = KernelEntryStack([0; 0x4000]);

    /// Kernel callee-saved state plus the suspended stack pointer. Assembly-owned.
    #[no_mangle]
    #[used]
    static mut KUMO_RING3_RESUME: [u64; 7] = [0; 7];

    extern "C" {
        static ring3_payload_start: u8;
        static ring3_payload_end: u8;
        fn kumo_enter_ring3(entry: u64, user_sp: u64, arg0: u64) -> u64;
        fn kumo_ring3_resume(code: u64) -> !;
    }

    core::arch::global_asm!(
        ".section .text.ring3_payload",
        ".code64",
        ".globl ring3_payload_start",
        ".globl ring3_payload_end",
        "ring3_payload_start:",
        "  mov $1, %eax",
        "  movabs $0x4b554d4fc0decafe, %rdi",
        "  int $0x80",
        "  movabs $0x4b554d4fc0decafe, %rcx",
        "  cmp %rcx, %rax",
        "  jne 1f",
        "  mov $2, %eax",
        "  xor %edi, %edi",
        "  int $0x80",
        "1:",
        "  mov $2, %eax",
        "  mov $1, %edi",
        "  int $0x80",
        "2: hlt",
        "  jmp 2b",
        "ring3_payload_end:",
        // Save the suspended kernel flow and build the five-word privilege-return frame.
        ".section .text",
        ".globl kumo_enter_ring3",
        "kumo_enter_ring3:",
        "  lea KUMO_RING3_RESUME(%rip), %rax",
        "  mov %rbx, 0(%rax)",
        "  mov %rbp, 8(%rax)",
        "  mov %r12, 16(%rax)",
        "  mov %r13, 24(%rax)",
        "  mov %r14, 32(%rax)",
        "  mov %r15, 40(%rax)",
        "  mov %rsp, 48(%rax)",
        "  mov %rdi, %r8",
        "  mov %rdx, %rdi",
        "  mov $0x1b, %ax",
        "  mov %ax, %ds",
        "  mov %ax, %es",
        "  pushq $0x1b",
        "  push %rsi",
        "  pushq $0x2",
        "  pushq $0x23",
        "  push %r8",
        "  iretq",
        // Called from the exit interrupt on the dedicated TSS stack. Abandon that frame,
        // restore the suspended kernel stack, and return from `kumo_enter_ring3`.
        ".globl kumo_ring3_resume",
        "kumo_ring3_resume:",
        "  mov %rdi, %rax",
        "  mov $0x10, %cx",
        "  mov %cx, %ds",
        "  mov %cx, %es",
        "  lea KUMO_RING3_RESUME(%rip), %rdx",
        "  mov 0(%rdx), %rbx",
        "  mov 8(%rdx), %rbp",
        "  mov 16(%rdx), %r12",
        "  mov 24(%rdx), %r13",
        "  mov 32(%rdx), %r14",
        "  mov 40(%rdx), %r15",
        "  mov 48(%rdx), %rsp",
        "  ret",
        options(att_syntax),
    );

    pub unsafe fn enter(entry: u64, user_sp: u64, arg0: u64) -> u64 {
        let kernel_stack = core::ptr::addr_of_mut!(KERNEL_ENTRY_STACK).cast::<u8>() as u64
            + core::mem::size_of::<KernelEntryStack>() as u64;
        crate::gdt::set_kernel_stack(kernel_stack);
        unsafe { kumo_enter_ring3(entry, user_sp, arg0) }
    }

    pub fn resume(code: u64) -> ! {
        unsafe { kumo_ring3_resume(code) }
    }

    pub fn payload() -> &'static [u8] {
        let start = core::ptr::addr_of!(ring3_payload_start) as usize;
        let end = core::ptr::addr_of!(ring3_payload_end) as usize;
        unsafe { core::slice::from_raw_parts(start as *const u8, end - start) }
    }
}

#[cfg(target_os = "none")]
pub(crate) unsafe fn enter(entry: u64, user_sp: u64, arg0: u64) -> u64 {
    unsafe { metal::enter(entry, user_sp, arg0) }
}

#[cfg(target_os = "none")]
pub(crate) fn resume(code: u64) -> ! {
    metal::resume(code)
}

#[cfg(target_os = "none")]
pub(crate) fn payload() -> &'static [u8] {
    metal::payload()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatcher_echoes_ping_and_exits_explicitly() {
        assert!(matches!(
            dispatch(OP_PING, PING_TOKEN),
            Dispatch::Return(PING_TOKEN)
        ));
        assert!(matches!(dispatch(OP_EXIT, 7), Dispatch::Exit(7)));
        assert!(matches!(dispatch(99, 0), Dispatch::Exit(u64::MAX)));
    }
}
