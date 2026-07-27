//! P5-sched: scheduler-driven user threads.
//!
//! Replaces the synchronous `enter_user_image` → `kumo_enter_el0` → `kumo_resume_kernel`
//! boot detour. A user thread is a real `Thread` in `sched::Dispatcher`, entered via
//! `kumo_user_enter` (which loads [`UserState`] and `eret`s to EL0 with DAIF unmasked so
//! the timer preempts). `ProcessExit` terminates the thread and switches to the next
//! runnable thread via the scheduler — no more `kumo_resume_kernel`.
//!
//! ChannelRead blocking (empty inbox → park → wake on write) arrives in P5-sora.

//j408
//j422
//j425
//j493
//j494

use core::cell::UnsafeCell;

use kumo_abi::{Errno, KoId, Status};
use kumo_hal::active::{switch_context, ThreadContext, UserState};

use crate::mm::PAGE_SIZE;
use crate::object::ObjectManager;
use crate::sched::{ClassId, Decision, Dispatcher, Priority};
use crate::task::{Job, Process, Thread, ThreadState, DEFAULT_KERNEL_STACK_SIZE};

const USER_PRIORITY: Priority = Priority(64);
const CHILD_PRIORITY: Priority = Priority(63);
/// Upper bound on simultaneously-resident children. Two proved independent per-thread waits
/// across distinct child koids (Journal 134), but a live boot now has a real case for more:
/// the two persistent async drivers (`drv-serial`, `drv-blk`) hold a slot each for the rest
/// of boot, and the `svc-health` pair smoke needs two *more* concurrent residents on top
/// (2 drivers + 2 servers = 4). With only two slots both drivers saturated the table, so
/// every later child spawn — the blocking `M10` runs and the async `svc-health` smokes alike
/// — returned `ShouldWait` and "failed" (it was the slot gate, not frames). `children`
/// reserves this capacity up front (no realloc while a child awaits first entry — the J169
/// invariant). Raised to 12 for the RK3588 USB bring-up: sora spawns one async `drv-xhci` per
/// xHCI controller (usb0/usb1/host2) to find which port has the keyboard, and at 4 those
/// briefly-concurrent enumeration children saturated the table on top of drv-blk/drv-fb — the
/// third spawn and every later blocking `M10` then failed the slot gate, and the pending async
/// children never got a yield to run. 12 covers the persistent drivers + a per-controller
/// enumeration fan-out + the svc-health/M10 transients with headroom.
const MAX_RESIDENT_CHILDREN: usize = 12;
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
    Interrupt(KoId),
    DeviceCtx(KoId),
    /// A futex word, keyed by the address space it lives in and its user virtual address.
    ///
    /// Keyed on the *process* rather than a koid because a futex is not a kernel object — it is a
    /// plain `u32` in userspace, and its identity is where it lives. Two processes with the same
    /// virtual address are different futexes, which is correct for the private (in-process) form a
    /// worker pool needs. A shared futex over a VMO would key on the physical address instead; that
    /// is a later extension and deliberately not this slice.
    Futex {
        process: KoId,
        addr: u64,
    },
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
    /// Fill `out` with the threads parked on `target`, in park order, and return how many.
    ///
    /// `waiter_for` returns only the first, which suits a channel (one reader) but not a futex,
    /// where waking N is the whole operation. Fills a caller slice rather than allocating: this
    /// runs inside the `SoraState` borrow, where allocating is the hazard j484 was.
    fn waiters_for(&self, target: WaitTarget, out: &mut [KoId]) -> usize {
        let mut len = 0;
        for entry in &self.entries {
            if len == out.len() {
                break;
            }
            if entry.target == target {
                out[len] = entry.thread;
                len += 1;
            }
        }
        len
    }

    fn remove_thread(&mut self, thread: KoId) {
        self.entries.retain(|e| e.thread != thread);
    }
}

/// A scheduler-hosted resident child and all the per-child state the harness tracks for it.
/// One element of the `children` collection (Journal 134): because every field is per child,
/// two residents keep independent `ttbr0`/`fresh`/`done` state and never clobber each other.
struct ResidentChild {
    /// The child's scheduler thread.
    thread: Thread,
    /// True until the first switch into the child's `kumo_user_enter` trampoline.
    fresh: bool,
    /// The child process's TTBR0 tree, restored on every resume after the first.
    ttbr0: u64,
    /// The child process koid.
    process: KoId,
    /// The child exit code (valid once `done`).
    exit_code: u64,
    /// True once the child thread has terminated.
    done: bool,
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
    /// The resident children spawned by Sora's `ProcessRun` demos, each with its own
    /// per-child state. Bounded by `MAX_RESIDENT_CHILDREN`; looked up by thread koid.
    children: alloc::vec::Vec<ResidentChild>,
    /// Threads parked on a typed wait target, keyed by thread and object koid.
    wait_queue: WaitQueue,
    /// A timer/device wake admitted a resident child while some other EL0 thread kept
    /// running. Pay this at the next SVC boundary, after SoraState borrows have dropped,
    /// rather than preempting arbitrary EL0 from IRQ context. — KESTREL
    safe_reschedule_pending: bool,
    /// An IRQ woke parked Sora while the boot/idle floor was current. The boot loop
    /// pays this after the IRQ epilogue returns instead of switching from `kumo_irq_common`.
    user_wake_pending: bool,
    /// Whether the user thread has been started (admitted to the scheduler).
    pub started: bool,
    /// Accumulated context-switch count.
    pub switches: u64,
    runaway_koid: Option<KoId>,
    runaway_ticks: u32,
    preempted_runaways: u64,
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

/// Number of scheduler context switches performed by the user-thread harness.
pub fn switch_count() -> u64 {
    let p = sched_ptr();
    unsafe { (&*p).switches }
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

/// Create the architecture-specific first-entry context for a user thread.
fn user_entry_context(user_state: *const UserState, kernel_sp: usize) -> ThreadContext {
    kumo_hal::active::user_entry_context(user_state, kernel_sp)
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
            children: alloc::vec::Vec::with_capacity(MAX_RESIDENT_CHILDREN),
            wait_queue: WaitQueue::new(),
            safe_reschedule_pending: false,
            user_wake_pending: false,
            started: false,
            switches: 0,
            runaway_koid: None,
            runaway_ticks: 0,
            preempted_runaways: 0,
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
        } else {
            s.children
                .iter()
                .find(|c| c.thread.koid() == current)
                .map(|c| c.process)
        }
    }
}

/// Install a fresh resident child into the harness: build its `ResidentChild` record, its
/// `UserState` from the supplied entry/stack/args, and its first-entry kernel context, then
/// mark it ready. The caller admits it to the dispatcher at the priority it chooses
/// (`run_prepared_child` reschedules immediately; `spawn_child_async` does not).
/// Returns the koid of the installed child, which the caller admits to the dispatcher. No
/// wait-queue reset: a fresh child has a fresh koid, so it owns no stale entry, and a sibling
/// resident's entry must survive — only `reap_done` drops entries, and only by exact koid.
#[allow(clippy::too_many_arguments)]
fn install_child(
    s: &mut UserSched,
    new_child: Thread,
    proc_koid: KoId,
    ttbr0: u64,
    entry: u64,
    sp: u64,
    arg: u64,
    arg2: u64,
) -> KoId {
    let mut child = ResidentChild {
        thread: new_child,
        fresh: true,
        ttbr0,
        process: proc_koid,
        exit_code: 0,
        done: false,
    };
    child.thread.user_state = Some(UserState {
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
    let kernel_sp = child.thread.stack().top();
    child.thread.ready();
    let koid = child.thread.koid();
    s.children.push(child);
    // Build the entry context only AFTER the child (and its inline `UserState`) is in its
    // final heap slot: `kumo_user_enter` reads `elr`/`ttbr0`/args through the `&UserState`
    // baked into x19, so that pointer must outlive the move. Capturing it from the
    // stack-local `child` before `push` left x19 dangling at a reused stack frame — both
    // resident children then entered the last-installed image (J169).
    let installed = s.children.last_mut().expect("just pushed child");
    let state_ptr = installed
        .thread
        .user_state
        .as_ref()
        .expect("child user state") as *const UserState;
    *installed.thread.context_mut() = user_entry_context(state_ptr, kernel_sp);
    koid
}

/// Remove resident children that have terminated, dropping any stale wait entry. Called at
/// the `ProcessWait` / sync-run boundaries, where Sora has observed the exit.
fn reap_done(s: &mut UserSched) {
    let mut i = 0;
    while i < s.children.len() {
        if s.children[i].done {
            let koid = s.children[i].thread.koid();
            s.wait_queue.remove_thread(koid);
            s.children.remove(i);
        } else {
            i += 1;
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
    let (koid, next_ctx) = unsafe {
        let s = &mut *p;
        reap_done(s);
        if s.children.len() >= MAX_RESIDENT_CHILDREN {
            return Errno::ShouldWait.status();
        }

        let koid = install_child(s, new_child, proc_koid, ttbr0, entry, sp, arg, arg2);
        s.dispatcher.admit(koid, CHILD_PRIORITY);
        let decision = s.dispatcher.reschedule_current();
        (koid, dispatch_context(s, decision))
    };

    if let Some((prev, next)) = next_ctx {
        unsafe { switch_context(prev, next) };
    }

    let exit_code = unsafe {
        let s = &mut *p;
        let code = s
            .children
            .iter()
            .find(|c| c.thread.koid() == koid)
            .map_or(0, |c| c.exit_code);
        reap_done(s);
        code
    };
    if exit_code == 0 {
        Errno::Ok.status()
    } else {
        Errno::Internal.status()
    }
}

/// P10-g: spawn a child asynchronously using the proven resident-child slot.
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
        reap_done(s);
        if s.children.len() >= MAX_RESIDENT_CHILDREN {
            return Errno::ShouldWait.status();
        }

        let koid = install_child(s, new_child, proc_koid, ttbr0, entry, sp, arg, arg2);
        // Admit at CHILD_ASYNC_PRIORITY (63) but do NOT reschedule — Sora continues.
        s.dispatcher.admit(koid, CHILD_ASYNC_PRIORITY);
    }
    Errno::Ok.status()
}

/// Install the live Sora/resident-child scheduler tick.
///
/// Ordinary timer/device wakes remain deferred to [`pump_idle_floor`] or the next SVC
/// boundary. Equal-priority peer rotation and the runaway fallback may switch only when
/// the interrupted context was EL0 and the architecture trap frame has preserved the
/// complete per-thread user state.
#[cfg(target_os = "none")]
pub fn install_preemption_hook() {
    kumo_hal::active::set_preempt_hook(preempt_tick);
}

#[cfg(target_os = "none")]
extern "C" fn preempt_tick() {}

/// P10-g: pump the resident children. Reschedules so every runnable child runs until it
/// blocks or exits, then returns: `ShouldWait` while any child is still resident (blocked),
/// `Ok` once all have exited. A single pump drains all woken children because each one's
/// park hands the CPU to the next runnable child before control returns to Sora.
pub fn process_wait() -> Status {
    let p = sched_ptr();
    let has_live = unsafe {
        let s = &*p;
        s.children.iter().any(|c| !c.done)
    };
    if !has_live {
        unsafe {
            let s = &mut *p;
            reap_done(s);
        }
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
    // Reap children that exited during the pump; resident (still-blocked) children stay so a
    // later channel write can wake one and another ProcessWait can pump it.
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        reap_done(s);
        if s.children.is_empty() {
            Errno::Ok.status()
        } else {
            Errno::ShouldWait.status()
        }
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
/// Sora's PortWait parking path; each resident child parks on its own object.
pub fn park_current_child_on_port(port_koid: KoId) {
    park_current_child_on(WaitTarget::Port(port_koid));
}

pub fn park_current_child_on_interrupt(interrupt_koid: KoId) {
    park_current_child_on(WaitTarget::Interrupt(interrupt_koid));
}

pub fn park_current_child_on_device_ctx_fault(ctx_koid: KoId) {
    park_current_child_on(WaitTarget::DeviceCtx(ctx_koid));
}

fn park_current_child_on(target: WaitTarget) {
    let p = sched_ptr();
    let switch = unsafe {
        let s = &mut *p;
        let Some(current) = s.dispatcher.current() else {
            return;
        };
        // Park whichever resident child is the one currently running.
        let Some(child) = s
            .children
            .iter_mut()
            .find(|c| c.thread.koid() == current)
            .map(|c| &mut c.thread)
        else {
            return;
        };
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

pub fn wake_child_waiting_on_interrupt(interrupt_koid: KoId) {
    wake_child_waiting_on(WaitTarget::Interrupt(interrupt_koid));
}

pub fn wake_child_waiting_on_device_ctx_fault(ctx_koid: KoId) {
    wake_child_waiting_on(WaitTarget::DeviceCtx(ctx_koid));
}

/// Largest number of threads one `FutexWake` will release in a single call.
pub const MAX_FUTEX_WAKE: usize = 32;

pub fn park_current_child_on_futex(process: KoId, addr: u64) {
    park_current_child_on(WaitTarget::Futex { process, addr });
}

/// Wake up to `count` threads parked on a futex word; returns how many were woken.
///
/// Waking nothing is not an error and is not remembered — a futex is not a counting semaphore, so a
/// wake that races ahead of its waiter is simply lost. That is why the *caller* must re-check the
/// word after `FutexWait` returns, and why the value compare lives inside the wait syscall rather
/// than in userspace.
pub fn wake_children_waiting_on_futex(process: KoId, addr: u64, count: u32) -> usize {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if !unsafe { (&*opt).is_some() } {
        return 0;
    }
    let limit = (count as usize).min(MAX_FUTEX_WAKE);
    if limit == 0 {
        return 0;
    }
    let target = WaitTarget::Futex { process, addr };
    let mut woken = [KoId(0); MAX_FUTEX_WAKE];

    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        let n = s.wait_queue.waiters_for(target, &mut woken[..limit]);
        let mut released = 0;
        for waiter in &woken[..n] {
            let Some(child) = s
                .children
                .iter_mut()
                .find(|c| c.thread.koid() == *waiter)
                .map(|c| &mut c.thread)
            else {
                continue;
            };
            if !matches!(child.state(), ThreadState::Blocked) {
                continue;
            }
            s.wait_queue.remove_thread(*waiter);
            child.ready();
            s.dispatcher.admit(child.koid(), CHILD_PRIORITY);
            released += 1;
        }
        if released > 0 {
            // The waker is mid-syscall, so the woken threads are dispatched at its SVC boundary —
            // the same deterministic resume point the timer/port wakes use.
            s.safe_reschedule_pending = true;
        }
        released
    }
}

fn wait_target_needs_safe_reschedule(target: WaitTarget) -> bool {
    matches!(
        target,
        WaitTarget::Port(_) | WaitTarget::Interrupt(_) | WaitTarget::DeviceCtx(_)
    )
}

/// Consecutive timer ticks one child may hold the CPU before the scheduler takes it back.
///
/// At the current tick rate this is on the order of tens of milliseconds — far beyond any
/// well-behaved driver's service time, so nothing that works today changes behaviour, and far
/// below a human's perception, so a runaway program looks like a slow program rather than a dead
/// machine.
const RUNAWAY_TICK_BUDGET: u32 = 8;

/// Charge one timer tick to a running child and schedule an equal-priority peer.
///
/// `irq_handoff_allowed` deliberately refuses to switch out of a child's IRQ frame, and that
/// refusal is why a spinning EL0 child takes the whole machine: Sora never runs, the boot floor
/// never runs, and nothing is left that could stop it. J288 named the blocker as the missing
/// full-register save/restore boundary. That boundary now exists — `kumo_irq_common` saves x0-x30,
/// `ELR_EL1`, `SPSR_EL1`, `SP_EL0`, and FP/SIMD state into a frame on the interrupted thread's
/// **own** kernel stack, and its epilogue restores all of it — and `on_irq` issues EOI *before*
/// calling any hook, precisely so a preempting switch cannot leave an interrupt active across
/// threads.
///
/// So this is allowed only under conditions that make the original hazard unreachable:
///
/// 1. **the interrupted context was EL0** — a thread in userspace holds no `SoraState` borrow and
///    no raw pointer into the process table, which is what made switching from a *kernel* frame
///    unsafe;
/// 2. **the `SoraState` borrow is free** — belt and braces for (1);
/// 3. **the current thread is a child**, not Sora and not the idle floor, both of which already
///    have working paths;
/// 4. **the dispatcher makes the policy decision** — its one-tick quantum rotates only to a ready
///    thread at the same priority; less-urgent work never preempts a child. With no peer, the
///    existing [`RUNAWAY_TICK_BUDGET`] fallback remains in force.
///
/// The IRQ frame stays on the displaced thread's own kernel stack. `dispatch_context` changes the
/// active userspace root and kernel-entry stack, then the ordinary context switch resumes the
/// peer; when this thread returns, the IRQ epilogue restores its complete saved EL0 frame.
/// — KESTREL 2026-07-26
pub fn preempt_child_on_timer_tick_if_safe() {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if !unsafe { (&*opt).is_some() } {
        return;
    }
    if !kumo_hal::active::interrupted_at_el0() {
        return;
    }

    let p = sched_ptr();
    let switch = unsafe {
        let s = &mut *p;
        if s.done {
            return;
        }
        let Some(current) = s.dispatcher.current() else {
            return;
        };
        if current == s.user_thread.koid()
            || current == s.idle.koid()
            || !s
                .children
                .iter()
                .any(|child| child.thread.koid() == current)
        {
            s.runaway_ticks = 0;
            s.runaway_koid = None;
            return;
        }
        // Checked outside the RefCell borrow it is testing: if kernel state is busy this tick,
        // leave dispatcher accounting unchanged and try again on the next one.
        if !crate::usermode::sora_state_is_free() {
            return;
        }

        let decision = s.dispatcher.on_timer_tick();
        if matches!(decision, Decision::Switch { .. }) {
            s.runaway_ticks = 0;
            s.runaway_koid = None;
            dispatch_context(s, decision)
        } else {
            // No equal or more-urgent peer was ready. Preserve the bounded watchdog used by
            // D1/D2; it is separate from the ordinary one-tick peer rotation above.
            if s.runaway_koid != Some(current) {
                s.runaway_koid = Some(current);
                s.runaway_ticks = 0;
            }
            s.runaway_ticks = s.runaway_ticks.saturating_add(1);
            if s.runaway_ticks < RUNAWAY_TICK_BUDGET {
                return;
            }
            s.runaway_ticks = 0;
            let decision = s.dispatcher.yield_current();
            let switch = dispatch_context(s, decision);
            if switch.is_some() {
                s.preempted_runaways = s.preempted_runaways.saturating_add(1);
            }
            switch
        }
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
    }
}

/// How many times a child has been preempted for monopolising the CPU. A rising count is a
/// misbehaving program, and it should be visible rather than silently absorbed.
pub fn preempted_runaways() -> u64 {
    let p = sched_ptr();
    unsafe { (&*p).preempted_runaways }
}

/// IRQ-context handoff gate. A device/timer IRQ can make a resident child or Sora runnable, but
/// ordinary device-wake handoff is permitted only when the interrupted context is Sora — the
/// supervisor pump that lent the child its resident slot. Active children keep deterministic
/// device-delivery ordering by paying the wake at their next SVC/park boundary; the separate
/// runaway watchdog has its own EL0/full-frame safety gate. The idle floor is paid by
/// [`pump_idle_floor`] after `eret`.
fn irq_handoff_allowed(current: KoId, idle: KoId, user: KoId) -> bool {
    let _ = idle;
    current == user
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
        // The wait queue keys on object koid, so `waiter` is exactly the child parked on
        // this object — wake that one, leaving any sibling resident on its own object asleep.
        let Some(child) = s
            .children
            .iter_mut()
            .find(|c| c.thread.koid() == waiter)
            .map(|c| &mut c.thread)
        else {
            return;
        };
        if !matches!(child.state(), ThreadState::Blocked) {
            return;
        }
        s.wait_queue.remove_thread(waiter);
        child.ready();
        s.dispatcher.admit(child.koid(), CHILD_PRIORITY);
        if wait_target_needs_safe_reschedule(target) {
            s.safe_reschedule_pending = true;
        }
    }
}

/// Dispatch a resident child that was made runnable by a timer/device wake once we are
/// back at a safe syscall boundary. The SVC entry frame already owns the interrupted
/// EL0 GPR/FP state on the current thread's kernel stack; this avoids the J287/J288
/// broad IRQ-preemption corruption while still giving timer-backed `PortWait` a
/// deterministic resume point. — KESTREL
pub fn reschedule_if_pending_after_svc() {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    let started = unsafe { (&*opt).is_some() };
    if !started {
        return;
    }

    let p = sched_ptr();
    let switch = unsafe {
        let s = &mut *p;
        if s.done || !s.safe_reschedule_pending {
            return;
        }
        let decision = s.dispatcher.reschedule_current();
        let switch = dispatch_context(s, decision);
        if switch.is_some() || s.dispatcher.runnable_count() == 0 {
            s.safe_reschedule_pending = false;
        }
        switch
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
    }
}

/// Dispatch a timer/device-woken resident child immediately after the IRQ signal path has queued
/// its port/interrupt packet. Pays the wake only when [`irq_handoff_allowed`] holds — i.e. Sora is
/// the interrupted context — so it never preempts an active driver child (the J288 corruption
/// window) and never switches from the idle floor (paid by [`pump_idle_floor`] after `eret`
/// instead). Without this, a keyboard IRQ that fires while Sora is mid-serve strands the wake and
/// the serve loop halts (j422 over-defer). Restored from the SANITARY reference. — CORVUS
pub fn reschedule_pending_after_irq_signal_if_safe() {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    let started = unsafe { (&*opt).is_some() };
    if !started {
        return;
    }

    let p = sched_ptr();
    let switch = unsafe {
        let s = &mut *p;
        if s.done || !s.safe_reschedule_pending {
            return;
        }
        let Some(current) = s.dispatcher.current() else {
            return;
        };
        if !irq_handoff_allowed(current, s.idle.koid(), s.user_thread.koid()) {
            return;
        }
        let decision = s.dispatcher.reschedule_current();
        let switch = dispatch_context(s, decision);
        if switch.is_some() || s.dispatcher.runnable_count() == 0 {
            s.safe_reschedule_pending = false;
        }
        switch
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
    }
}

/// Wake Sora from an IRQ signal path without switching away from the Current-EL IRQ
/// frame. The framebuffer/serial boot loop calls [`pump_idle_floor`] after exception
/// return to pay deferred user wakes.
pub fn wake_user_after_irq_signal() {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    let started = unsafe { (&*opt).is_some() };
    if !started {
        return;
    }

    let p = sched_ptr();
    let should_wake_now = unsafe {
        let s = &mut *p;
        if s.done || !matches!(s.user_thread.state(), ThreadState::Blocked) {
            s.user_wake_pending = false;
            return;
        }
        // Sora interrupted itself while parked mid-transition: it is the supervisor pump, so a
        // switch back into it is safe now. From the idle floor or a driver child, defer to
        // `pump_idle_floor` after `eret` instead. Restored from SANITARY. — CORVUS
        match s.dispatcher.current() {
            Some(current) if current == s.idle.koid() => {
                s.user_wake_pending = true;
                false
            }
            Some(current) if current == s.user_thread.koid() => true,
            _ => {
                s.user_wake_pending = true;
                false
            }
        }
    };
    if should_wake_now {
        wake_user();
    }
}

/// Run one deferred user/resident-child handoff from the boot/idle floor. This is the
/// safe replacement for switching directly out of `kumo_irq_common` when a timer or
/// device IRQ makes EL0 work runnable.
pub fn pump_idle_floor() {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    let started = unsafe { (&*opt).is_some() };
    if !started {
        return;
    }

    let p = sched_ptr();
    let switch = unsafe {
        let s = &mut *p;
        if s.done || s.dispatcher.current() != Some(s.idle.koid()) {
            return;
        }
        if s.user_wake_pending {
            s.user_wake_pending = false;
            if matches!(s.user_thread.state(), ThreadState::Blocked) {
                s.user_thread.ready();
                s.dispatcher.admit(s.user_thread.koid(), USER_PRIORITY);
            }
        }
        if !s.safe_reschedule_pending && s.dispatcher.runnable_count() == 0 {
            return;
        }
        let decision = s.dispatcher.reschedule_current();
        let switch = dispatch_context(s, decision);
        if switch.is_some() || s.dispatcher.runnable_count() == 0 {
            s.safe_reschedule_pending = false;
        }
        switch
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
        boot_flow_resumed(p);
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
            Some(current) if s.children.iter().any(|c| c.thread.koid() == current) => {
                s.wait_queue.remove_thread(current);
                if let Some(child) = s.children.iter_mut().find(|c| c.thread.koid() == current) {
                    child.exit_code = exit_code;
                    child.done = true;
                    child.thread.terminate();
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
                } else if let Some(child) =
                    s.children.iter_mut().find(|c| c.thread.koid() == from_id)
                {
                    if matches!(child.thread.state(), ThreadState::Running) {
                        child.thread.ready();
                    }
                    child.thread.context_mut() as *mut ThreadContext
                } else {
                    // idle (or unknown) = the boot flow's save slot.
                    &mut s.boot_ctx as *mut ThreadContext
                }
            } else {
                &mut s.boot_ctx as *mut ThreadContext
            };
            let next = if to == s.user_thread.koid() {
                s.user_thread.run();
                kumo_hal::active::set_user_kernel_stack(s.user_thread.stack().top());
                if s.user_fresh {
                    s.user_fresh = false;
                } else {
                    set_active_aspace_root(s.user_ttbr0);
                }
                s.user_thread.context() as *const ThreadContext
            } else if let Some(child) = s.children.iter_mut().find(|c| c.thread.koid() == to) {
                child.thread.run();
                kumo_hal::active::set_user_kernel_stack(child.thread.stack().top());
                if child.fresh {
                    child.fresh = false;
                } else {
                    set_active_aspace_root(child.ttbr0);
                }
                child.thread.context() as *const ThreadContext
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
    use super::{wait_target_needs_safe_reschedule, KoId, WaitQueue, WaitTarget};

    #[test]
    fn park_then_waiter_for_finds_thread_by_object() {
        let mut q = WaitQueue::new();
        q.park(KoId(7), WaitTarget::Port(KoId(42)));
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(42))), Some(KoId(7)));
        // A different object koid is a miss even with the same variant.
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(43))), None);
        // The channel/port distinction is part of the key.
        assert_eq!(q.waiter_for(WaitTarget::Channel(KoId(42))), None);
        assert_eq!(q.waiter_for(WaitTarget::DeviceCtx(KoId(42))), None);
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
    fn futex_waiters_are_keyed_by_address_space_and_address() {
        let mut q = WaitQueue::new();
        let a = WaitTarget::Futex {
            process: KoId(1),
            addr: 0x1000,
        };
        // Same address, different process: a different futex. The private form must not alias.
        let b = WaitTarget::Futex {
            process: KoId(2),
            addr: 0x1000,
        };
        // Same process, different address: also different.
        let c = WaitTarget::Futex {
            process: KoId(1),
            addr: 0x1008,
        };
        q.park(KoId(10), a);
        q.park(KoId(11), b);
        q.park(KoId(12), c);

        let mut out = [KoId(0); 4];
        assert_eq!(q.waiters_for(a, &mut out), 1);
        assert_eq!(out[0], KoId(10));
        assert_eq!(q.waiters_for(b, &mut out), 1);
        assert_eq!(out[0], KoId(11));
        assert_eq!(q.waiters_for(c, &mut out), 1);
        assert_eq!(out[0], KoId(12));
    }

    #[test]
    fn waiters_are_returned_in_park_order_and_bounded_by_the_caller_slice() {
        let mut q = WaitQueue::new();
        let target = WaitTarget::Futex {
            process: KoId(1),
            addr: 0x2000,
        };
        for id in 20..25 {
            q.park(KoId(id), target);
        }
        let mut all = [KoId(0); 8];
        assert_eq!(q.waiters_for(target, &mut all), 5);
        assert_eq!(
            &all[..5],
            &[KoId(20), KoId(21), KoId(22), KoId(23), KoId(24)]
        );

        // Waking two must take the two that have waited longest, not an arbitrary pair.
        let mut two = [KoId(0); 2];
        assert_eq!(q.waiters_for(target, &mut two), 2);
        assert_eq!(two, [KoId(20), KoId(21)]);
    }

    #[test]
    fn a_wake_with_no_waiters_finds_nothing_and_is_not_remembered() {
        // A futex is not a counting semaphore: a wake that races ahead of its waiter is lost, which
        // is exactly why the caller must re-check the word after `FutexWait` returns.
        let q = WaitQueue::new();
        let mut out = [KoId(0); 4];
        assert_eq!(
            q.waiters_for(
                WaitTarget::Futex {
                    process: KoId(1),
                    addr: 0x3000
                },
                &mut out
            ),
            0
        );
    }

    #[test]
    fn a_dying_thread_leaves_no_futex_entry_behind() {
        let mut q = WaitQueue::new();
        let target = WaitTarget::Futex {
            process: KoId(1),
            addr: 0x4000,
        };
        q.park(KoId(30), target);
        q.park(KoId(31), target);
        q.remove_thread(KoId(30));
        let mut out = [KoId(0); 4];
        assert_eq!(q.waiters_for(target, &mut out), 1);
        assert_eq!(out[0], KoId(31));
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
    fn two_threads_park_on_distinct_objects_and_wake_independently() {
        // The multi-resident invariant (Journal 134): two parked threads are routed by
        // object koid, so waking one by its object leaves the other untouched.
        let mut q = WaitQueue::new();
        q.park(KoId(7), WaitTarget::Port(KoId(42)));
        q.park(KoId(8), WaitTarget::Port(KoId(43)));
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(42))), Some(KoId(7)));
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(43))), Some(KoId(8)));
        // Wake thread 7 (remove it); thread 8 stays parked on its own object.
        q.remove_thread(KoId(7));
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(42))), None);
        assert_eq!(q.waiter_for(WaitTarget::Port(KoId(43))), Some(KoId(8)));
    }

    #[test]
    fn timer_and_device_waits_request_safe_reschedule() {
        assert!(wait_target_needs_safe_reschedule(WaitTarget::Port(KoId(
            42
        ))));
        assert!(wait_target_needs_safe_reschedule(WaitTarget::Interrupt(
            KoId(43)
        )));
        assert!(wait_target_needs_safe_reschedule(WaitTarget::DeviceCtx(
            KoId(45)
        )));
        assert!(!wait_target_needs_safe_reschedule(WaitTarget::Channel(
            KoId(44)
        )));
    }
}
