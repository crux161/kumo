//j456
//j457

//! Two-context CPL3 proof for eager x86 FP/SIMD ownership.
//!
//! Each private address space runs the same tiny payload with a different vector sentinel. On AVX
//! systems it lives in YMM0's upper half; otherwise the legacy XMM0 proof remains available. The
//! first `int 0x80` switches to the peer and the second reports survival after resume. — KESTREL

use core::cell::UnsafeCell;

use kumo_abi::BootInfo;
use kumo_hal::active::{ThreadContext, UserImageError, UserState};

use crate::mm;

const CONTEXTS: usize = 2;
const OP_YIELD: u64 = 0xf0;
const OP_DONE: u64 = 0xf1;
const SENTINELS: [u64; CONTEXTS] = [0x1111_aaaa_5555_c001, 0x2222_bbbb_6666_c002];
const KERNEL_STACK_BYTES: usize = 0x4000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    pub calls: u32,
    pub avx: bool,
    pub yielded: [bool; CONTEXTS],
    pub survived: [bool; CONTEXTS],
    pub roots: [u64; CONTEXTS],
}

impl Report {
    pub const fn is_live(self) -> bool {
        self.calls == 4
            && self.yielded[0]
            && self.yielded[1]
            && self.survived[0]
            && self.survived[1]
            && self.roots[0] != self.roots[1]
    }
}

#[repr(C, align(16))]
struct KernelStack([u8; KERNEL_STACK_BYTES]);

static mut STACK_0: KernelStack = KernelStack([0; KERNEL_STACK_BYTES]);
static mut STACK_1: KernelStack = KernelStack([0; KERNEL_STACK_BYTES]);

struct State {
    boot_ctx: ThreadContext,
    user_ctx: [ThreadContext; CONTEXTS],
    user_state: [UserState; CONTEXTS],
    kernel_root: u64,
    current: usize,
    yielded: [bool; CONTEXTS],
    survived: [bool; CONTEXTS],
    done: [bool; CONTEXTS],
}

struct StateCell(UnsafeCell<Option<State>>);
unsafe impl Sync for StateCell {}
static STATE: StateCell = StateCell(UnsafeCell::new(None));

fn stack_top(index: usize) -> usize {
    let base = match index {
        0 => core::ptr::addr_of_mut!(STACK_0).cast::<u8>(),
        _ => core::ptr::addr_of_mut!(STACK_1).cast::<u8>(),
    };
    base as usize + KERNEL_STACK_BYTES
}

fn state_ptr() -> *mut State {
    let slot = STATE.0.get();
    unsafe {
        (&mut *slot)
            .as_mut()
            .expect("x86 FP/SIMD smoke not initialized") as *mut State
    }
}

/// Switch between the two live CPL3 interrupt continuations. No Rust reference crosses the raw
/// context switch: all pointers and target metadata are copied out first. — KESTREL
fn switch_user(from: usize, to: usize) {
    let state = state_ptr();
    let (prev, next, root, kernel_stack) = unsafe {
        let state = &mut *state;
        state.current = to;
        (
            &mut state.user_ctx[from] as *mut ThreadContext,
            &state.user_ctx[to] as *const ThreadContext,
            state.user_state[to].ttbr0,
            stack_top(to),
        )
    };
    unsafe { kumo_hal::active::set_user_aspace_root(root) };
    kumo_hal::active::set_user_kernel_stack(kernel_stack);
    unsafe { kumo_hal::active::switch_context(prev, next) };
}

fn finish_user(from: usize, survived: bool) -> ! {
    let state = state_ptr();
    let (prev, next, root, kernel_stack) = unsafe {
        let state = &mut *state;
        state.survived[from] = survived;
        state.done[from] = true;
        let peer = 1 - from;
        if !state.done[peer] {
            state.current = peer;
            (
                &mut state.user_ctx[from] as *mut ThreadContext,
                &state.user_ctx[peer] as *const ThreadContext,
                state.user_state[peer].ttbr0,
                Some(stack_top(peer)),
            )
        } else {
            (
                &mut state.user_ctx[from] as *mut ThreadContext,
                &state.boot_ctx as *const ThreadContext,
                state.kernel_root,
                None,
            )
        }
    };
    unsafe { kumo_hal::active::set_user_aspace_root(root) };
    if let Some(kernel_stack) = kernel_stack {
        kumo_hal::active::set_user_kernel_stack(kernel_stack);
    }
    unsafe { kumo_hal::active::switch_context(prev, next) };
    loop {
        kumo_hal::active::spin_once();
    }
}

extern "C" fn syscall_hook(regs: *mut u64) {
    let operation = unsafe { *regs.add(8) };
    let status = unsafe { *regs };
    let state = state_ptr();
    let current = unsafe { (*state).current };
    match operation {
        OP_YIELD => {
            unsafe { (*state).yielded[current] = true };
            switch_user(current, 1 - current);
        }
        OP_DONE => finish_user(current, status == 0),
        _ => finish_user(current, false),
    }
}

pub fn run(boot: &BootInfo, avx: bool) -> Result<Report, UserImageError> {
    let kernel_root = kumo_hal::active::read_user_aspace_root();
    let mut alloc = || unsafe { mm::alloc_zeroed_frame(boot) };
    let states = [
        kumo_hal::active::prepare_scheduled_fpsimd_smoke(SENTINELS[0], avx, &mut alloc)?,
        kumo_hal::active::prepare_scheduled_fpsimd_smoke(SENTINELS[1], avx, &mut alloc)?,
    ];

    unsafe {
        let slot = &mut *STATE.0.get();
        *slot = Some(State {
            boot_ctx: ThreadContext::default(),
            user_ctx: [ThreadContext::default(); CONTEXTS],
            user_state: states,
            kernel_root,
            current: 0,
            yielded: [false; CONTEXTS],
            survived: [false; CONTEXTS],
            done: [false; CONTEXTS],
        });
        let state = slot.as_mut().expect("just initialized");
        for index in 0..CONTEXTS {
            state.user_ctx[index] = kumo_hal::active::user_entry_context(
                &state.user_state[index] as *const UserState,
                stack_top(index),
            );
        }
    }

    kumo_hal::active::set_svc_hook(syscall_hook);
    let state = state_ptr();
    let (boot_ctx, first_ctx) = unsafe {
        (
            &mut (*state).boot_ctx as *mut ThreadContext,
            &(*state).user_ctx[0] as *const ThreadContext,
        )
    };
    kumo_hal::active::set_user_kernel_stack(stack_top(0));
    unsafe { kumo_hal::active::switch_context(boot_ctx, first_ctx) };

    let state = state_ptr();
    Ok(unsafe {
        Report {
            calls: kumo_hal::active::syscall_count(),
            avx,
            yielded: (*state).yielded,
            survived: (*state).survived,
            roots: [(*state).user_state[0].ttbr0, (*state).user_state[1].ttbr0],
        }
    })
}
