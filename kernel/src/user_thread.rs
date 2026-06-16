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
/// P10-g: async child preempts Sora via reschedule_current in process_wait.
/// Same priority as the blocking child — more urgent than Sora (64).
const CHILD_ASYNC_PRIORITY: Priority = Priority(63);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserSchedError {
    OutOfFrames,
    EmptyImage,
    BadSegment,
    BadStack,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitTarget {
    Channel(KoId),
    Port(KoId),
}

/// One parked thread and the typed object it blocked on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WaitEntry {
    thread: KoId,
    target: WaitTarget,
}

/// A small wait queue of threads parked on typed wait targets.
///
/// This replaces the single `child_wait: Option<WaitTarget>` slot (Journal 130) with a
/// structure indexable by **thread koid** (which target is a thread parked on; remove a
/// thread) and **object koid** (`WaitTarget`) (which thread to wake when an object is
/// signalled). It is the shape real per-thread wait queues grow from (DESIGN/003's
/// `OwnedWaitQueue` lineage; `PLAN §5.4` Stage-C service migration). The scheduler harness
/// still hosts a single resident child this slice, so the queue holds at most one entry —
/// but the operations no longer assume that.
#[derive(Default)]
struct WaitQueue {
    entries: alloc::vec::Vec<WaitEntry>,
}

impl WaitQueue {
    const fn new() -> Self {
        Self {
            entries: alloc::vec::Vec::new(),
        }
    }

    /// Park `thread` on `target`. Per-thread: an existing entry for the same thread is
    /// retargeted in place rather than duplicated (a thread waits on one object at a time).
    fn park(&mut self, thread: KoId, target: WaitTarget) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.thread == thread) {
            entry.target = target;
        } else {
            self.entries.push(WaitEntry { thread, target });
        }
    }

    /// The first thread parked on `target`, without removing it. Lets the caller validate
    /// the waiter (still the resident child, still `Blocked`) before committing the wake.
    fn waiter_for(&self, target: WaitTarget) -> Option<KoId> {
        self.entries
            .iter()
            .find(|e| e.target == target)
            .map(|e| e.thread)
    }

    /// Remove any entry for `thread` (woken, exited, or being torn down).
    fn remove_thread(&mut self, thread: KoId) {
        self.entries.retain(|e| e.thread != thread);
    }

    /// Drop every entry. Used when a child generation is (re)installed or torn down; with
    /// the single-child scaffold the queue only holds that child, so this matches the old
    /// `child_wait = None` reset exactly.
    fn clear(&mut self) {
        self.entries.clear();
    }
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
    /// Threads parked on a typed wait target, keyed by thread and object koid.
    wait_queue: WaitQueue,
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

fn set_active_aspace_root(root: u64) {
    // Switching the user address-space root is arch-specific and inherently unsafe; the
    // HAL validates the root (arm64: TTBR0_EL1; x86: cr3). One uniform call across backends.
    unsafe { kumo_hal::active::set_user_aspace_root(root) };
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
            wait_queue: WaitQueue::new(),
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
    arg2: u64,
) -> Status {
    let temp_process = Process::from_parts(proc_koid, root_vmar);
    let new_child = match Thread::new(objects, &temp_process, 0, 0, DEFAULT_KERNEL_STACK_SIZE) {
        Ok(thread) => thread,
        Err(_) => return Errno::NoMemory.status(),
    };
    run_prepared_child(new_child, proc_koid, root_vmar, ttbr0, entry, sp, arg, arg2)
}

/// Run a child thread that has already been allocated. This keeps object-manager and
/// Sora-state borrows out of the context-switch window, so child SVCs can re-enter Sora.
pub fn run_prepared_child(
    new_child: Thread,
    proc_koid: KoId,
    _root_vmar: crate::mm::Vmar,
    ttbr0: u64,
    entry: u64,
    sp: u64,
    arg: u64,
    arg2: u64,
) -> Status {
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
        s.wait_queue.clear();
        s.child_exit_code = 0;
        s.child_done = false;

        let child = s.child_thread.as_mut().expect("child just installed");
        child.user_state = Some(UserState {
            x: {
                let mut x = [0u64; 31];
                x[0] = arg;
                x[1] = arg2;
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
        s.wait_queue.clear();
        s.child_done = false;
        code
    };
    if exit_code == 0 {
        Errno::Ok.status()
    } else {
        Errno::Internal.status()
    }
}

/// P10-g: spawn a child asynchronously using the proven child_thread slot.
/// The child is admitted at CHILD_ASYNC_PRIORITY (63, more urgent than Sora at 64)
/// but does NOT preempt immediately. Returns immediately.
pub fn spawn_child_async(
    objects: &mut ObjectManager,
    proc_koid: KoId,
    root_vmar: crate::mm::Vmar,
    ttbr0: u64,
    entry: u64,
    sp: u64,
    arg: u64,
    arg2: u64,
) -> Status {
    let temp_process = Process::from_parts(proc_koid, root_vmar);
    let new_child = match Thread::new(objects, &temp_process, 0, 0, DEFAULT_KERNEL_STACK_SIZE) {
        Ok(thread) => thread,
        Err(_) => return Errno::NoMemory.status(),
    };

    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        if s.child_thread.is_some() && !s.child_done {
            return Errno::ShouldWait.status();
        }

        s.child_thread = Some(new_child);
        s.child_fresh = true;
        s.child_ttbr0 = ttbr0;
        s.child_process = Some(proc_koid);
        s.wait_queue.clear();
        s.child_exit_code = 0;
        s.child_done = false;

        let child = s.child_thread.as_mut().expect("child just installed");
        child.user_state = Some(UserState {
            x: {
                let mut x = [0u64; 31];
                x[0] = arg;
                x[1] = arg2;
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

        // Admit at CHILD_ASYNC_PRIORITY (63) but do NOT reschedule — Sora continues.
        s.dispatcher.admit(child.koid(), CHILD_ASYNC_PRIORITY);
    }
    Errno::Ok.status()
}

/// P10-g: block until the async child (stored in child_thread) terminates.
/// Uses the proven run_child reschedule pattern.
pub fn process_wait() -> Status {
    let p = sched_ptr();
    let has_child = unsafe {
        let s = &*p;
        s.child_thread.is_some() && !s.child_done
    };
    if !has_child {
        return Errno::Ok.status();
    }
    let switch = unsafe {
        let s = &mut *p;
        let decision = s.dispatcher.reschedule_current();
        dispatch_context(s, decision)
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
    }
    // After child exits, clean up. If it merely blocked, leave it resident so a
    // later channel write can wake it and another ProcessWait can pump it.
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        if !s.child_done {
            return Errno::ShouldWait.status();
        }
        s.child_thread = None;
        s.child_fresh = false;
        s.child_ttbr0 = 0;
        s.child_process = None;
        s.wait_queue.clear();
        s.child_done = false;
    }
    Errno::Ok.status()
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
    set_active_aspace_root(kernel_ttbr0);
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

/// Park the current child thread on an empty channel endpoint. Called from the child
/// SVC path after dropping any Sora borrow; returns when the endpoint is written and
/// Sora pumps the child again via ProcessWait.
pub fn park_current_child_on_channel(channel_koid: KoId) {
    park_current_child_on(WaitTarget::Channel(channel_koid));
}

/// Park the current child thread on an empty port. This is the non-Sora mirror of
/// Sora's PortWait parking path, still scoped to the single child slot.
pub fn park_current_child_on_port(port_koid: KoId) {
    park_current_child_on(WaitTarget::Port(port_koid));
}

fn park_current_child_on(target: WaitTarget) {
    let p = sched_ptr();
    let switch = unsafe {
        let s = &mut *p;
        let Some(current) = s.dispatcher.current() else {
            return;
        };
        let Some(child) = s.child_thread.as_mut() else {
            return;
        };
        if current != child.koid() {
            return;
        }
        let child_koid = child.koid();
        child.block();
        s.wait_queue.park(child_koid, target);
        let decision = s.dispatcher.block_current();
        dispatch_context(s, decision)
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
    }
}

/// Mark a resident child runnable when a peer writes to the endpoint it blocked on.
/// This deliberately does not switch immediately: most callers are in Sora's syscall
/// path, so Sora explicitly pumps the child with ProcessWait after its borrow drops.
pub fn wake_child_waiting_on_channel(channel_koid: KoId) {
    wake_child_waiting_on(WaitTarget::Channel(channel_koid));
}

/// Mark a resident child runnable when a port it was waiting on receives a packet.
pub fn wake_child_waiting_on_port(port_koid: KoId) {
    wake_child_waiting_on(WaitTarget::Port(port_koid));
}

fn wake_child_waiting_on(target: WaitTarget) {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    let started = unsafe { (&*opt).is_some() };
    if !started {
        return;
    }

    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        let Some(waiter) = s.wait_queue.waiter_for(target) else {
            return;
        };
        // This slice still hosts only the single resident child; the waiter must be it.
        let Some(child) = s.child_thread.as_mut() else {
            return;
        };
        if child.koid() != waiter || !matches!(child.state(), ThreadState::Blocked) {
            return;
        }
        s.wait_queue.remove_thread(waiter);
        child.ready();
        s.dispatcher.admit(child.koid(), CHILD_PRIORITY);
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
                s.wait_queue.remove_thread(current);
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
                    set_active_aspace_root(s.user_ttbr0);
                }
                s.user_thread.context() as *const ThreadContext
            } else if let Some(child) = s.child_thread.as_mut() {
                if to == child.koid() {
                    child.run();
                    if s.child_fresh {
                        s.child_fresh = false;
                    } else {
                        set_active_aspace_root(s.child_ttbr0);
                    }
                    child.context() as *const ThreadContext
                } else {
                    if to == s.idle.koid() {
                        s.idle.run();
                    }
                    set_active_aspace_root(s.kernel_ttbr0);
                    &s.boot_ctx as *const ThreadContext
                }
            } else {
                // idle (or unknown) = resume the suspended boot flow.
                if to == s.idle.koid() {
                    s.idle.run(); // dispatcher bookkeeping only; idle_body never executes
                }
                set_active_aspace_root(s.kernel_ttbr0);
                &s.boot_ctx as *const ThreadContext
            };
            Some((prev, next))
        }
        Decision::Idle | Decision::Continue(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{KoId, WaitQueue, WaitTarget};

    #[test]
    fn park_then_waiter_for_finds_thread_by_object() {
        let mut q = WaitQueue::new();
        q.park(KoId(7), WaitTarget::Port(KoId(42)));
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(42))), Some(KoId(7)));
        // A different object koid is a miss even with the same variant.
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(43))), None);
        // The channel/port distinction is part of the key.
        assert_eq!(q.waiter_for(WaitTarget::Channel(KoId(42))), None);
    }

    #[test]
    fn park_is_per_thread_and_retargets_in_place() {
        let mut q = WaitQueue::new();
        q.park(KoId(7), WaitTarget::Channel(KoId(1)));
        // Re-parking the same thread moves it to the new target, not a second entry.
        q.park(KoId(7), WaitTarget::Port(KoId(2)));
        assert_eq!(q.waiter_for(WaitTarget::Channel(KoId(1))), None);
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(2))), Some(KoId(7)));
        assert_eq!(q.entries.len(), 1);
    }

    #[test]
    fn remove_thread_clears_only_that_thread() {
        let mut q = WaitQueue::new();
        q.park(KoId(7), WaitTarget::Port(KoId(42)));
        q.park(KoId(8), WaitTarget::Channel(KoId(9)));
        q.remove_thread(KoId(7));
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(42))), None);
        assert_eq!(q.waiter_for(WaitTarget::Channel(KoId(9))), Some(KoId(8)));
        // Removing an absent thread is a no-op.
        q.remove_thread(KoId(7));
        assert_eq!(q.entries.len(), 1);
    }

    #[test]
    fn clear_drops_every_entry() {
        let mut q = WaitQueue::new();
        q.park(KoId(7), WaitTarget::Port(KoId(42)));
        q.park(KoId(8), WaitTarget::Channel(KoId(9)));
        q.clear();
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(42))), None);
        assert_eq!(q.waiter_for(WaitTarget::Channel(KoId(9))), None);
        assert!(q.entries.is_empty());
    }
}
