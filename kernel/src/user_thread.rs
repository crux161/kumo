//! P5-sched: scheduler-driven user threads.
//!
//! Replaces the synchronous `enter_user_image` → `kumo_enter_el0` → `kumo_resume_kernel`
//! boot detour. A user thread is a real `Thread` in `sched::Dispatcher`, entered via
//! `kumo_user_enter` (which loads [`UserState`] and `eret`s to EL0 with DAIF unmasked so
//! the timer preempts). `ProcessExit` terminates the thread and switches to the next
//! runnable thread via the scheduler — no more `kumo_resume_kernel`.
//!
//! ChannelRead blocking (empty inbox → park → wake on write) arrives in P5-sora.

use core::cell::UnsafeCell;

use kumo_abi::{Errno, KoId, Status};
use kumo_hal::active::{switch_context, ThreadContext, UserState};

use crate::mm::PAGE_SIZE;
use crate::object::ObjectManager;
use crate::sched::{ClassId, Decision, Dispatcher, Priority};
use crate::task::{Job, Process, Thread, ThreadState, DEFAULT_KERNEL_STACK_SIZE};

const USER_PRIORITY: Priority = Priority(64);
const CHILD_PRIORITY: Priority = Priority(63);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserSchedError {
    OutOfFrames,
    EmptyImage,
    BadSegment,
    BadStack,
}

/// The scheduler-integrated user-thread harness. Owns the dispatcher, the boot-flow
/// kernel context (so `ProcessExit` can return here), and the user thread's EL0 state.
pub struct UserSched {
    pub dispatcher: Dispatcher,
    /// The idle thread (always-runnable floor when the user thread blocks or exits).
    pub idle: Thread,
    /// Context to return to when the user thread terminates or blocks.
    pub boot_ctx: ThreadContext,
    /// The kernel identity-map TTBR0, restored whenever the boot flow resumes.
    pub kernel_ttbr0: u64,
    /// The user process's TTBR0 tree, restored before resuming the user thread.
    pub user_ttbr0: u64,
    /// The user thread.
    pub user_thread: Thread,
    /// True until the first switch into Sora's `kumo_user_enter` trampoline.
    pub user_fresh: bool,
    /// A scheduler-hosted child spawned by Sora's `ProcessRun` demo.
    pub child_thread: Option<Thread>,
    /// True until the first switch into the child's `kumo_user_enter` trampoline.
    pub child_fresh: bool,
    /// The child process's TTBR0 tree.
    pub child_ttbr0: u64,
    /// The child process koid.
    pub child_process: Option<KoId>,
    /// The child exit code.
    pub child_exit_code: u64,
    /// True once the child thread has terminated.
    pub child_done: bool,
    /// Whether the user thread has been started (admitted to the scheduler).
    pub started: bool,
    /// Accumulated context-switch count.
    pub switches: u64,
    /// The user process exit code.
    pub exit_code: u64,
    /// True once the user thread has terminated.
    pub done: bool,
}

struct UserSchedCell(UnsafeCell<Option<UserSched>>);
unsafe impl Sync for UserSchedCell {}
static USER_SCHED: UserSchedCell = UserSchedCell(UnsafeCell::new(None));

/// Read the user thread's exit code (valid after the thread has terminated).
pub fn exit_code() -> u64 {
    let p = sched_ptr();
    unsafe { (&*p).exit_code }
}

/// Whether the user thread has terminated.
pub fn is_done() -> bool {
    let p = sched_ptr();
    unsafe { (&*p).done }
}

/// Whether the user thread is parked on an empty channel (alive and serving).
pub fn is_parked() -> bool {
    let p = sched_ptr();
    unsafe { matches!((&*p).user_thread.state(), ThreadState::Blocked) }
}

/// Whether the user-thread harness has been initialised (i.e. `init` was called).
pub fn is_started() -> bool {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    unsafe { (&*opt).is_some() }
}

/// The kernel identity-map TTBR0 captured at launch. A syscall handler running on the
/// user thread (TTBR0 = the process tree) switches to this before building page tables by
/// physical address, then restores the process tree. Valid once `init` has run.
pub fn kernel_ttbr0() -> u64 {
    let p = sched_ptr();
    unsafe { (&*p).kernel_ttbr0 }
}

fn sched_ptr() -> *mut UserSched {
    let opt: *mut Option<UserSched> = USER_SCHED.0.get();
    unsafe {
        (&mut *opt)
            .as_mut()
            .expect("user_thread sched not initialized") as *mut UserSched
    }
}

fn set_active_ttbr0(root: u64) {
    #[cfg(target_os = "none")]
    unsafe {
        kumo_hal::active::set_ttbr0(root)
    };
    #[cfg(not(target_os = "none"))]
    kumo_hal::active::set_ttbr0(root);
}

/// Create a `ThreadContext` for the first entry of a user thread. `x19_entry` points at
/// the `UserState`; `x30_lr` is `kumo_user_enter` (not `kumo_context_trampoline`).
fn user_entry_context(user_state: *const UserState, kernel_sp: usize) -> ThreadContext {
    extern "C" {
        fn kumo_user_enter();
    }
    let mut ctx = ThreadContext::default();
    // Set fields directly: ThreadContext layout is x19_entry, x20_arg, x21-x28, x29_fp,
    // x30_lr, sp, user. We only need x19_entry (UserState pointer), x30_lr (user enter
    // trampoline), and sp (kernel stack).
    //
    // SAFETY: ThreadContext is repr(C) and the layout matches what kumo_context_switch
    // expects. We're writing into a freshly-defaulted struct.
    unsafe {
        let raw = &mut ctx as *mut ThreadContext as *mut u64;
        *raw = user_state as u64; // x19_entry
        *raw.add(11) = kumo_user_enter as *const () as usize as u64; // x30_lr
        *raw.add(12) = kernel_sp as u64; // sp
        *raw.add(13) = 1; // user = true
    }
    ctx
}

/// Pin the boot-flow's current execution context so the user thread can return here on
/// exit (or block). Must be called with interrupts masked / single-threaded.
///
/// # Safety
/// Caller must ensure the current stack frame is not unwound before the user thread exits.
pub unsafe fn pin_boot_context(ctx: &ThreadContext) {
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        s.boot_ctx = *ctx;
    }
}

/// Initialise the scheduler-driven user-thread harness. `boot` provides the frame
/// allocator; `objects` provides the kernel object store.
pub fn init(
    objects: &mut ObjectManager,
    user_proc_koid: KoId,
    user_root_vmar: crate::mm::Vmar,
    kernel_ttbr0: u64,
) -> Result<(), UserSchedError> {
    let job = Job::root(objects);
    let vmar = crate::mm::Vmar::new(0xffff_0000_0000_0000, PAGE_SIZE * 256).expect("kernel vmar");
    let idle_process = Process::new(objects, &job, vmar);
    let user_process = Process::from_parts(user_proc_koid, user_root_vmar);

    let idle = Thread::new(
        objects,
        &idle_process,
        idle_body as extern "C" fn(usize) as usize,
        0,
        DEFAULT_KERNEL_STACK_SIZE,
    )
    .map_err(|_| UserSchedError::OutOfFrames)?;

    let mut dispatcher = Dispatcher::new(1);
    dispatcher.set_idle(idle.koid());
    dispatcher.set_running(idle.koid(), Priority::LOWEST, ClassId::Idle);

    // SAFETY: first writer; single-threaded boot path.
    unsafe {
        *USER_SCHED.0.get() = Some(UserSched {
            dispatcher,
            idle,
            boot_ctx: ThreadContext::default(),
            kernel_ttbr0,
            user_ttbr0: 0,
            user_thread: Thread::new(
                objects,
                &user_process,
                0, // placeholder; overridden in spawn_user
                0,
                DEFAULT_KERNEL_STACK_SIZE,
            )
            .map_err(|_| UserSchedError::OutOfFrames)?,
            user_fresh: true,
            child_thread: None,
            child_fresh: false,
            child_ttbr0: 0,
            child_process: None,
            child_exit_code: 0,
            child_done: false,
            started: false,
            switches: 0,
            exit_code: 0,
            done: false,
        });
    }
    Ok(())
}

/// The process koid for the EL0 thread currently represented by the dispatcher, if any.
pub fn current_process_koid() -> Option<KoId> {
    let p = sched_ptr();
    unsafe {
        let s = &*p;
        let current = s.dispatcher.current()?;
        if current == s.user_thread.koid() {
            Some(s.user_thread.process())
        } else if let Some(child) = &s.child_thread {
            if current == child.koid() {
                Some(child.process())
            } else {
                None
            }
        } else {
            None
        }
    }
}

/// Run a child process as a real scheduler participant. Returns after the child exits
/// and Sora's `ProcessRun` syscall context is resumed.
pub fn run_child(
    objects: &mut ObjectManager,
    proc_koid: KoId,
    root_vmar: crate::mm::Vmar,
    ttbr0: u64,
    entry: u64,
    sp: u64,
    arg: u64,
) -> Status {
    let temp_process = Process::from_parts(proc_koid, root_vmar);
    let new_child = match Thread::new(objects, &temp_process, 0, 0, DEFAULT_KERNEL_STACK_SIZE) {
        Ok(thread) => thread,
        Err(_) => return Errno::NoMemory.status(),
    };

    let p = sched_ptr();
    let next_ctx = unsafe {
        let s = &mut *p;
        if s.child_thread.is_some() && !s.child_done {
            return Errno::ShouldWait.status();
        }

        s.child_thread = Some(new_child);
        s.child_fresh = true;
        s.child_ttbr0 = ttbr0;
        s.child_process = Some(proc_koid);
        s.child_exit_code = 0;
        s.child_done = false;

        let child = s.child_thread.as_mut().expect("child just installed");
        child.user_state = Some(UserState {
            x: {
                let mut x = [0u64; 31];
                x[0] = arg;
                x
            },
            elr: entry,
            spsr: 0,
            sp_el0: sp,
            ttbr0,
        });
        let state_ptr = child.user_state.as_ref().expect("child user state") as *const UserState;
        let kernel_sp = child.stack().top();
        *child.context_mut() = user_entry_context(state_ptr, kernel_sp);
        child.ready();

        s.dispatcher.admit(child.koid(), CHILD_PRIORITY);
        let decision = s.dispatcher.reschedule_current();
        dispatch_context(s, decision)
    };

    if let Some((prev, next)) = next_ctx {
        unsafe { switch_context(prev, next) };
    }

    let exit_code = unsafe {
        let s = &mut *p;
        let code = s.child_exit_code;
        s.child_thread = None;
        s.child_fresh = false;
        s.child_ttbr0 = 0;
        s.child_process = None;
        s.child_done = false;
        code
    };
    if exit_code == 0 {
        Errno::Ok.status()
    } else {
        Errno::Internal.status()
    }
}

/// Spawn a user thread: build its `UserState` from the ELF image, admit it to the
/// scheduler's RT class, and switch to it. Returns after the user thread exits
/// (ProcessExit) or blocks.
///
/// # Safety
/// Must run with the kernel identity map active in TTBR0 (so we can read/write
/// physical frames to build page tables). `boot.mem_regions` must be valid.
pub unsafe fn spawn_user(user_state: UserState, user_ttbr0: u64) {
    let p = sched_ptr();
    let next_ctx = unsafe {
        let s = &mut *p;
        s.user_ttbr0 = user_ttbr0;
        // Build a Thread for the user. Reuse the one allocated in `init`.
        s.user_thread.user_state = Some(user_state);
        let state_ptr = s
            .user_thread
            .user_state
            .as_ref()
            .expect("user state just installed") as *const UserState;
        let kernel_sp = s.user_thread.stack().top();
        let ctx = user_entry_context(state_ptr, kernel_sp);

        // Overwrite the thread's kernel context.
        *s.user_thread.context_mut() = ctx;
        s.user_thread.ready();
        s.user_thread.run();

        s.dispatcher.admit(s.user_thread.koid(), USER_PRIORITY);
        s.started = true;

        // The boot context is saved by the switch itself (`prev` = `boot_ctx`).
        let decision = s.dispatcher.reschedule_current();
        dispatch_context(s, decision)
    };

    if let Some((prev, next)) = next_ctx {
        unsafe { switch_context(prev, next) };
        boot_flow_resumed(p);
    }
    // We return here when the user thread exits or parks on an empty channel.
}

/// Housekeeping when the boot flow resumes from a user-thread switch: the switch ran
/// inside the masked SVC handler with the **user's TTBR0 still active** — restore the
/// kernel identity map (so physical-address work like the frame allocator is sound) and
/// unmask IRQs (the trampoline hazard `kumo_context_trampoline` documents).
fn boot_flow_resumed(p: *mut UserSched) {
    let kernel_ttbr0 = unsafe { (&*p).kernel_ttbr0 };
    set_active_ttbr0(kernel_ttbr0);
    kumo_hal::active::irq_unmask();
}

/// Park the current user thread on an empty inbox: block it in the dispatcher and
/// switch to the boot flow. Called from the SVC hook (the user thread's kernel stack);
/// returns when [`wake_user`] readmits the thread, after which the caller retries.
pub fn park_current_user() {
    let p = sched_ptr();
    let switch = unsafe {
        let s = &mut *p;
        s.user_thread.block();
        let decision = s.dispatcher.block_current();
        dispatch_context(s, decision)
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
        // Resumed by `wake_user`: it restored our TTBR0 before switching here; nothing
        // to do but return to the SVC hook, which retries the read.
    }
}

/// Wake a parked user thread and run it until it parks again (or exits). No-op if the
/// thread is not parked. Called from the boot flow after writing to a channel the user
/// is serving.
pub fn wake_user() {
    let p = sched_ptr();
    let switch = unsafe {
        let s = &mut *p;
        if s.done || !matches!(s.user_thread.state(), ThreadState::Blocked) {
            return;
        }
        s.user_thread.ready();
        s.dispatcher.admit(s.user_thread.koid(), USER_PRIORITY);
        let decision = s.dispatcher.reschedule_current();
        dispatch_context(s, decision)
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
        boot_flow_resumed(p);
    }
}

/// Called from the SVC hook for ProcessExit. Terminate the current user thread,
/// finish it in the scheduler, and switch to the next runnable thread (or idle).
/// Never returns to the caller — execution resumes in the boot flow.
pub fn exit_current_user(exit_code: u64) -> ! {
    let p = sched_ptr();
    let switch = unsafe {
        let s = &mut *p;
        match s.dispatcher.current() {
            Some(current) if current == s.user_thread.koid() => {
                s.exit_code = exit_code;
                s.user_thread.terminate();
                s.done = true;
            }
            Some(current)
                if s.child_thread
                    .as_ref()
                    .map(|child| child.koid() == current)
                    .unwrap_or(false) =>
            {
                s.child_exit_code = exit_code;
                s.child_done = true;
                if let Some(child) = s.child_thread.as_mut() {
                    child.terminate();
                }
            }
            _ => {}
        }
        let decision = s.dispatcher.finish_current();
        dispatch_context(s, decision)
    };

    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
    }
    // Fallback: if nothing is runnable, spin forever.
    loop {
        kumo_hal::active::spin_once();
    }
}

/// The idle thread body — spins forever, never consumed.
extern "C" fn idle_body(_arg: usize) {
    loop {
        kumo_hal::active::spin_once();
    }
}

/// Map a [`Decision`] to raw context pointers for `switch_context`.
///
/// In this Stage-A harness the **boot flow is the idle floor**: the `idle` Thread is only
/// a dispatcher token (its `idle_body` never runs). Switching *away from* idle saves the
/// live boot context into `boot_ctx`; switching *to* idle resumes `boot_ctx` — i.e. the
/// boot flow continues from wherever `spawn_user` switched away. Both sides must agree,
/// or a user exit strands the kernel in `idle_body` and the boot never completes.
fn dispatch_context(
    s: &mut UserSched,
    decision: Decision,
) -> Option<(*mut ThreadContext, *const ThreadContext)> {
    match decision {
        Decision::Switch { from, to } => {
            s.switches = s.switches.saturating_add(1);
            let prev = if let Some(from_id) = from {
                if from_id == s.user_thread.koid() {
                    if matches!(s.user_thread.state(), ThreadState::Running) {
                        s.user_thread.ready();
                    }
                    s.user_thread.context_mut() as *mut ThreadContext
                } else if let Some(child) = s.child_thread.as_mut() {
                    if from_id == child.koid() {
                        if matches!(child.state(), ThreadState::Running) {
                            child.ready();
                        }
                        child.context_mut() as *mut ThreadContext
                    } else {
                        &mut s.boot_ctx as *mut ThreadContext
                    }
                } else {
                    // idle (or unknown) = the boot flow's save slot.
                    &mut s.boot_ctx as *mut ThreadContext
                }
            } else {
                &mut s.boot_ctx as *mut ThreadContext
            };
            let next = if to == s.user_thread.koid() {
                s.user_thread.run();
                if s.user_fresh {
                    s.user_fresh = false;
                } else {
                    set_active_ttbr0(s.user_ttbr0);
                }
                s.user_thread.context() as *const ThreadContext
            } else if let Some(child) = s.child_thread.as_mut() {
                if to == child.koid() {
                    child.run();
                    if s.child_fresh {
                        s.child_fresh = false;
                    } else {
                        set_active_ttbr0(s.child_ttbr0);
                    }
                    child.context() as *const ThreadContext
                } else {
                    if to == s.idle.koid() {
                        s.idle.run(); // dispatcher bookkeeping only; idle_body never executes
                    }
                    set_active_ttbr0(s.kernel_ttbr0);
                    &s.boot_ctx as *const ThreadContext
                }
            } else {
                // idle (or unknown) = resume the suspended boot flow.
                if to == s.idle.koid() {
                    s.idle.run(); // dispatcher bookkeeping only; idle_body never executes
                }
                set_active_ttbr0(s.kernel_ttbr0);
                &s.boot_ctx as *const ThreadContext
            };
            Some((prev, next))
        }
        Decision::Idle | Decision::Continue(_) => None,
    }
}
