//! P5-sched: scheduler-driven user threads.
//!
//! Replaces the synchronous `enter_user_image` → `kumo_enter_el0` → `kumo_resume_kernel`
//! boot detour. A user thread is a real `Thread` in `sched::Dispatcher`, entered via
//! `kumo_user_enter` (which loads [`UserState`] and `eret`s to EL0 with DAIF unmasked so
//! the timer preempts). `ProcessExit` terminates the thread and switches to the next
//! runnable thread via the scheduler — no more `kumo_resume_kernel`.
//!
//! ChannelRead blocking (empty inbox → park → wake on write) arrives in P5-sora.

//j871
//j873
//j875
//j877
//j880

use core::cell::UnsafeCell;

use kumo_abi::{Errno, KoId, Status};
use kumo_hal::active::{switch_context, ThreadContext, TowerContext, TowerThreadRole, UserState};

use crate::mm::PAGE_SIZE;
use crate::object::ObjectManager;
use crate::sched::{ClassId, Decision, Dispatcher, Priority};

//j726 — ready-to-run lateness. The J724 SVC payer readies a blocked Sora, but the
//strict-priority dispatcher keeps the current child (Priority 63 outranks her 64), so the
//wake completes only at a moment when no child is runnable. These counters time that gap:
//when she was made ready, whether the dispatcher kept the child, and how late she resumed.
// — TIMBERDOODLE
#[cfg(target_os = "none")]
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
#[cfg(target_os = "none")]
static USER_READY_AT: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "none")]
static USER_WAKE_READIES: AtomicU32 = AtomicU32::new(0);
#[cfg(target_os = "none")]
static USER_WAKE_KEPT_CHILD: AtomicU32 = AtomicU32::new(0);
#[cfg(target_os = "none")]
static USER_RESUME_MAX_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "none")]
static USER_RESUME_OVER_1S: AtomicU32 = AtomicU32::new(0);
//j729
//j822
#[cfg(target_os = "none")]
static SORA_AWAY_AT: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "none")]
static SORA_AWAY_MAX_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "none")]
static SORA_AWAY_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "none")]
static SORA_AWAY_RESUMES: AtomicU32 = AtomicU32::new(0);
#[cfg(target_os = "none")]
static SORA_PREEMPT_COUNT: AtomicU32 = AtomicU32::new(0);
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
/// The compile-time ceiling on resident children. The live bound is the Job policy mirror
/// [`UserSched::resident_limit`], which starts at this value and can be set anywhere in
/// `1..=MAX_RESIDENT_CHILDREN` (it can rise again after a temporary narrowing). `children`
/// reserves this capacity up front so a boot wave does not reallocate
/// while a child awaits first entry; AX4's boxed entries make that a heap-churn preference,
/// not a correctness requirement. Every stored child is one boxed allocation.
const MAX_RESIDENT_CHILDREN: usize = kumo_abi::actor::RESIDENT_MAX;
const _: () = assert!(MAX_RESIDENT_CHILDREN == kumo_abi::actor::RESIDENT_MAX);
/// P10-g: async child preempts Sora via reschedule_current in process_wait.
/// Same priority as the blocking child — more urgent than Sora's boot admit (64), less
/// urgent than every later wake of hers (`USER_WAKE_PRIORITY`, J730).
const CHILD_ASYNC_PRIORITY: Priority = Priority(63);
//j730
/// The priority a woken user thread is admitted at, on every wake path: the SVC-tail payer,
/// the idle-floor pump, and the direct wake. At her boot 64 Sora lost every dispatcher
/// comparison to a runnable child (63), so under sustained child load her serve loop ran
/// only when no child could (J726: >100k kept-child decisions; J729: 224 preemptions in
/// three minutes). The dispatcher keeps a running thread's priority across preemption, so
/// after her first wake she stays at 62 and children run when she parks. Metal (J730): the
/// clock samples stayed under 100 ms late through three churn sessions, where J721-J729 saw
/// seconds. — TIMBERDOODLE
const USER_WAKE_PRIORITY: Priority = Priority(62);

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

impl WaitTarget {
    /// A kind name and the object koid (a futex's user address) for a kill report.
    fn describe(self) -> (&'static str, u64) {
        match self {
            WaitTarget::Channel(koid) => ("channel", koid.0),
            WaitTarget::Port(koid) => ("port", koid.0),
            WaitTarget::Interrupt(koid) => ("interrupt", koid.0),
            WaitTarget::DeviceCtx(koid) => ("device-ctx", koid.0),
            WaitTarget::Futex { addr, .. } => ("futex", addr),
        }
    }
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

    /// What `thread` is parked on, if it is parked at all.
    fn target_of(&self, thread: KoId) -> Option<WaitTarget> {
        self.entries
            .iter()
            .find(|e| e.thread == thread)
            .map(|e| e.target)
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
    /// The process that admitted this child (SH4). `ProcessWait` reports a reaped child's
    /// status to its parent only, so a launcher's pumps never drain a sibling parent's
    /// statuses; Sora's own launches carry Sora. — TEAL
    parent: KoId,
    /// The child exit code (valid once `done`).
    exit_code: u64,
    /// Whether this record's exit is the PROCESS's status (SH5). A process's first thread
    /// reports; a sibling admitted by `ThreadSpawn` does not, and a group exit or kill moves
    /// the report onto exactly one record, so `ProcessWait` sees one status per process.
    reports: bool,
    /// True once the child thread has terminated.
    done: bool,
    /// AX2 reply routing: the call record this child last consumed when serving a call.
    /// It survives a re-park without a reply and is overwritten only by the next delivered
    /// call (the abandoned caller then resolves at its deadline). — TIMBERDOODLE
    current_call: Option<usize>,
    /// SH6 synchronous-fault entry (`FaultHandlerSet`): the EL0 trampoline this thread's
    /// next fault is redirected to, or 0. One-shot: delivery clears it. — TEAL
    fault_entry: u64,
    /// The alternate stack top the trampoline starts on, or 0 for "below the faulting SP".
    fault_stack_top: u64,
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
    /// per-child state. Bounded by the live policy bound (`resident_limit`); looked up by
    /// thread koid. Each entry is boxed: the J169 entry context bakes a `&UserState` into
    /// the child's saved x19, so the `ResidentChild`'s address must be stable from install
    /// until reap even while this Vec grows. A `Vec<Box<_>>` moves pointers, never the
    /// pointee; `reap_done` is the only dropper, and it removes only after the thread has
    /// terminated and is no longer current. — TIMBERDOODLE
    children: alloc::vec::Vec<alloc::boxed::Box<ResidentChild>>,
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
    /// AX2 reply routing for Sora's own served call.
    sora_current_call: Option<usize>,
    /// AX4 resident bound mirrored from Sora's root Job policy. Admission compares the
    /// resident children count against this value; it is the authoritative live gate because
    /// the scheduler owns the resident set. Always in `1..=MAX_RESIDENT_CHILDREN`.
    resident_limit: u32,
    /// Exit statuses of children reaped at the wait boundaries, oldest first (SH4): the
    /// parent koid, the process koid and the exit code, drained one per `ProcessWait` by
    /// the parent they belong to, so a parent can collect every child's status, killed
    /// ones included. A small fixed ring — the oldest entry falls off if nobody asks.
    reaped: [(KoId, KoId, u64); 8],
    reaped_len: usize,
}

struct UserSchedCell(UnsafeCell<Option<UserSched>>);
unsafe impl Sync for UserSchedCell {}
static USER_SCHED: UserSchedCell = UserSchedCell(UnsafeCell::new(None));

/// Read the user thread's exit code (valid after the thread has terminated).
pub fn exit_code() -> u64 {
    let p = sched_ptr();
    unsafe { (&*p).exit_code }
}

/// Pop the oldest reaped child of `parent` as (process koid, exit code), for
/// `ProcessWait`'s x1/x2. Entries of other parents stay queued for their own pumps.
pub fn take_reaped(parent: KoId) -> Option<(KoId, u64)> {
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        let at = (0..s.reaped_len).find(|&at| s.reaped[at].0 == parent)?;
        let (_, process, code) = s.reaped[at];
        s.reaped.copy_within(at + 1..s.reaped_len, at);
        s.reaped_len -= 1;
        Some((process, code))
    }
}

/// `ProcessWait` for a resident child acting as a parent (SH4): reap, then report whether
/// any of the caller's own children are still live. No scheduler pump — a child blocks on
/// its own port (bound to the child process, which signals `TERMINATED`) and pumps here
/// only to collect; Sora's pump path (`process_wait`) stays the launcher's alone. — TEAL
pub fn process_wait_for(parent: KoId) -> Status {
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        reap_done(s);
        if s.children.iter().any(|c| c.parent == parent && !c.done) {
            Errno::ShouldWait.status()
        } else {
            Errno::Ok.status()
        }
    }
}

/// True when the current thread belongs to a child that a live resident process (not
/// Sora) admitted (SH4). Such a child's exit status is data its parent collects through
/// `ProcessWait`, so a non-zero exit is not a silent failure and raises no TOWER; once the
/// parent is gone nobody will collect it and the abend report stands. — TEAL
pub fn current_exit_is_collected_by_parent() -> bool {
    let p = sched_ptr();
    unsafe {
        let s = &*p;
        let Some(current) = s.dispatcher.current() else {
            return false;
        };
        let Some(child) = s.children.iter().find(|c| c.thread.koid() == current) else {
            return false;
        };
        child.parent != s.user_thread.process()
            && s.children
                .iter()
                .any(|c| c.process == child.parent && !c.done)
    }
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

/// Capacity applies to resident child threads; Sora and the idle floor are excluded.
/// Copy everything before a caller can print, allocate or re-enter. — TIMBERDOODLE
pub fn resident_snapshot_wide() -> kumo_abi::actor::SnapshotWide {
    let mut result = kumo_abi::actor::SnapshotWide {
        capacity: MAX_RESIDENT_CHILDREN as u32,
        ..Default::default()
    };
    if !is_started() {
        return result;
    }
    let s = unsafe { &*sched_ptr() };
    result.capacity = s.resident_limit;
    result.stored = s.children.len() as u32;
    for child in &s.children {
        if child.done {
            continue;
        }
        result.processes[result.live as usize] = child.process.0;
        result.live += 1;
        match child.thread.state() {
            ThreadState::Blocked => result.blocked += 1,
            ThreadState::Running => result.running += 1,
            _ => result.ready += 1,
        }
    }
    result
}

/// AX0's legacy 192-byte view: capacity 12 and the first 12 resident koids. `None` when the
/// true stored or live count does not fit that frozen frame — AXB1 must refuse rather than
/// emit a frame whose own decoder rejects it. AX4 probes use [`resident_snapshot_wide`].
pub fn resident_snapshot() -> Option<kumo_abi::actor::Snapshot> {
    let wide = resident_snapshot_wide();
    if wide.live as usize > kumo_abi::actor::RESIDENT_LIMIT
        || wide.stored as usize > kumo_abi::actor::RESIDENT_LIMIT
    {
        return None;
    }
    let mut result = kumo_abi::actor::Snapshot {
        enabled: wide.enabled,
        capacity: kumo_abi::actor::RESIDENT_LIMIT as u32,
        live: wide.live,
        stored: wide.stored,
        blocked: wide.blocked,
        ready: wide.ready,
        running: wide.running,
        frames_issued: wide.frames_issued,
        heap_used: wide.heap_used,
        roots: wide.roots,
        root_ns: wide.root_ns,
        root_min_ns: wide.root_min_ns,
        root_max_ns: wide.root_max_ns,
        syscalls: wide.syscalls,
        switches: wide.switches,
        ..Default::default()
    };
    result.processes[..wide.live as usize].copy_from_slice(&wide.processes[..wide.live as usize]);
    Some(result)
}

/// AX4: the mirrored resident bound, or the compile-time ceiling before the scheduler runs.
pub fn resident_limit() -> u32 {
    if is_started() {
        unsafe { (&*sched_ptr()).resident_limit }
    } else {
        MAX_RESIDENT_CHILDREN as u32
    }
}

/// AX4: whether one more resident child would be admitted right now. The `ProcessRun` front
/// end checks this before staging any handle grants, so a capacity refusal never consumes a
/// transfer; the authoritative gate remains inside `run_prepared_child`/`spawn_child_async`,
/// which re-run `reap_done` immediately before admission. Single-threaded SVC execution means
/// the answer cannot change between the preflight and the gate.
pub fn resident_has_capacity() -> bool {
    if !is_started() {
        return true;
    }
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        reap_done(s);
        s.children.len() < s.resident_limit as usize
    }
}

///j825
/// Free kernel-heap bytes a spawn must find before it starts allocating. The kernel stack
/// alone is 16 KiB; the resident record, handle table, mapping and dispatcher entries need
/// room on top. The 8 MiB heap is fixed and cannot grow, so a spawn at the ceiling must be
/// refused with `NoMemory` — the status Sora can answer — and not reach `alloc`'s panic
/// path, which is a silent halt on the boot/teardown path. — TIMBERDOODLE
const SPAWN_HEAP_RESERVE: usize = 64 * 1024;

///j825
/// Whether a spawn's fixed allocations fit in what remains of the kernel heap. Checked
/// before `Thread::new` (which allocates the kernel stack) on every spawn path. — TIMBERDOODLE
pub fn spawn_heap_ready() -> bool {
    crate::mm::heap::headroom() >= SPAWN_HEAP_RESERVE
}

/// AX4: resident children that have not terminated, measured the way the admission gate
/// measures them: terminated entries are reaped first. A policy bound at this value can
/// therefore never produce a snapshot whose stored count exceeds its capacity.
pub fn resident_live() -> u32 {
    if !is_started() {
        return 0;
    }
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        reap_done(s);
        s.children.len() as u32
    }
}

/// AX4: install a resident bound, re-checking the engine's decision against the resident set
/// as it is at this instant. A bound outside `1..=MAX_RESIDENT_CHILDREN` or below the stored
/// children (terminated entries are reaped first) is refused rather than clamped, so the
/// scheduler invariant `stored <= capacity` cannot be broken by a direct caller. Returns
/// false before the scheduler exists, where there is no resident set for a policy to govern.
pub fn set_resident_limit(limit: u32) -> bool {
    if !is_started() {
        return false;
    }
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        reap_done(s);
        if limit == 0 || limit as usize > MAX_RESIDENT_CHILDREN || s.children.len() > limit as usize
        {
            return false;
        }
        s.resident_limit = limit;
    }
    true
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
    let measuring = crate::actorstat::enabled();
    let begin = if measuring {
        kumo_hal::active::monotonic_nanos()
    } else {
        0
    };
    unsafe { kumo_hal::active::set_user_aspace_root(root) };
    if measuring {
        crate::actorstat::record_root(kumo_hal::active::monotonic_nanos().saturating_sub(begin));
    }
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
            sora_current_call: None,
            resident_limit: job.resident_limit(),
            reaped: [(KoId(0), KoId(0), 0); 8],
            reaped_len: 0,
        });
    }
    kumo_hal::active::set_tower_context_hook(tower_context);
    Ok(())
}

/// What the EL1 exception TOWER needs from the kernel: the running thread, and where the
/// faulting `sp` and the image are. It runs from a fatal exception, so it reads the scheduler
/// through the raw cell and never panics, allocates, locks, or switches (DESIGN/014 §4): an
/// uninitialised scheduler reports `Unknown` instead of `sched_ptr`'s panic.
extern "C" fn tower_context(sp: u64) -> TowerContext {
    let map = crate::diag::current_map();
    let stack_limit = match crate::diag::place(&map, sp) {
        crate::diag::Placement::Inside {
            region: crate::diag::Region::BootStack,
            ..
        } => map.boot_stack.1,
        crate::diag::Placement::Inside {
            region: crate::diag::Region::Heap,
            ..
        } => map.heap.1,
        _ => 0,
    };
    let mut context = TowerContext {
        stack_limit,
        image_start: map.image.0,
        image_end: map.image.1,
        ..TowerContext::default()
    };
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    let Some(s) = (unsafe { (*opt).as_ref() }) else {
        return context;
    };
    context.switches = s.switches;
    let Some(current) = s.dispatcher.current() else {
        return context;
    };
    let (role, process) = if current == s.idle.koid() {
        (TowerThreadRole::Floor, 0)
    } else if current == s.user_thread.koid() {
        (TowerThreadRole::Sora, s.user_thread.process().0)
    } else if let Some(child) = s.children.iter().find(|c| c.thread.koid() == current) {
        (TowerThreadRole::Child, child.process.0)
    } else {
        (TowerThreadRole::Unknown, 0)
    };
    context.thread_koid = current.0;
    context.process_koid = process;
    context.role = role;
    context
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

//j824
/// The koid of the scheduler thread currently running, when it is Sora or a resident child.
pub fn current_thread_koid() -> Option<KoId> {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if unsafe { (&*opt).is_none() } {
        return None;
    }
    let p = sched_ptr();
    unsafe {
        let s = &*p;
        let current = s.dispatcher.current()?;
        if current == s.user_thread.koid() || s.children.iter().any(|c| c.thread.koid() == current)
        {
            Some(current)
        } else {
            None
        }
    }
}

/// Sora's scheduler thread koid. Only meaningful once the harness is initialised.
pub fn sora_thread_koid() -> KoId {
    let p = sched_ptr();
    unsafe { (&*p).user_thread.koid() }
}

/// The AX2 call record `thread` last consumed, if any. The slot is deliberately not
/// cleared by a re-park, so a server may read again before replying. — TIMBERDOODLE
pub fn current_call_of(thread: KoId) -> Option<usize> {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if unsafe { (&*opt).is_none() } {
        return None;
    }
    let p = sched_ptr();
    unsafe {
        let s = &*p;
        if thread == s.user_thread.koid() {
            s.sora_current_call
        } else {
            s.children
                .iter()
                .find(|c| c.thread.koid() == thread)
                .and_then(|c| c.current_call)
        }
    }
}

/// Sora's own reply slot, read inside her `with_sora_mut` reply arm.
pub fn sora_current_call() -> Option<usize> {
    let p = sched_ptr();
    unsafe { (&*p).sora_current_call }
}

/// Record the call a receiver just consumed. A new delivered call overwrites the slot;
/// the previous caller is then abandoned until its deadline (documented v1 semantic).
pub fn set_current_call(thread: KoId, call: usize) {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if unsafe { (&*opt).is_none() } {
        return;
    }
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        if thread == s.user_thread.koid() {
            s.sora_current_call = Some(call);
        } else if let Some(child) = s.children.iter_mut().find(|c| c.thread.koid() == thread) {
            child.current_call = Some(call);
        }
    }
}

/// The resident child parked in `ChannelRead` on `channel_koid`, if one is. Sora never
/// enters the wait queue; her parked state is checked separately by the caller. — TIMBERDOODLE
pub fn channel_call_waiter(channel_koid: KoId) -> Option<KoId> {
    if !is_started() {
        return None;
    }
    let p = sched_ptr();
    unsafe {
        (&*p)
            .wait_queue
            .waiter_for(WaitTarget::Channel(channel_koid))
    }
}

/// Make a parked receiver runnable WITHOUT switching. The caller's next
/// `block_current` then selects this thread: that is the AX2 direct switch. Returns the
/// admission priority used, for the donation record; `None` when the receiver is not a
/// parked Sora/resident child (the call simply stays queued).
pub fn pre_admit_call_receiver(thread: KoId) -> Option<u8> {
    if !is_started() {
        return None;
    }
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        if s.done || s.dispatcher.current() == Some(thread) {
            return None;
        }
        if thread == s.user_thread.koid() {
            if !matches!(s.user_thread.state(), ThreadState::Blocked) {
                return None;
            }
            s.user_thread.ready();
            s.dispatcher.admit(thread, USER_WAKE_PRIORITY);
            Some(USER_WAKE_PRIORITY.0)
        } else {
            let index = s.children.iter().position(|c| c.thread.koid() == thread)?;
            if s.children[index].done
                || !matches!(s.children[index].thread.state(), ThreadState::Blocked)
            {
                return None;
            }
            s.wait_queue.remove_thread(thread);
            s.children[index].thread.ready();
            s.dispatcher.admit(thread, CHILD_PRIORITY);
            Some(CHILD_PRIORITY.0)
        }
    }
}

/// Record the sched-1 slot handoff. Called exactly once per direct switch, before the
/// caller blocks; sched-2 will read the same record for the level donation. — TIMBERDOODLE
pub fn donate_call_slot(from: KoId, to: KoId, priority: u8) {
    if !is_started() {
        return;
    }
    let p = sched_ptr();
    unsafe {
        (&mut *p)
            .dispatcher
            .on_donation(from, to, Priority(priority));
    }
}

/// Ask the deferred user-wake path to make parked Sora runnable at the next safe boundary.
/// Completions that cannot switch (IRQ deadline sweep, ChannelReply, a legacy write) use
/// this; the SVC payer or the IRQ epilogue performs the actual reschedule.
pub fn request_user_wake() {
    if !is_started() {
        return;
    }
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        if s.done || !matches!(s.user_thread.state(), ThreadState::Blocked) {
            return;
        }
        s.user_wake_pending = true;
    }
}

/// AX2 completion wake: make a parked child caller runnable and mark an explicit safe
/// reschedule. `WaitTarget::Channel` is not in the safe-reschedule class, so a completion
/// path that must not switch (IRQ sweep) or that runs under a Sora borrow asks for it here.
pub fn wake_child_waiting_on_channel_for_call(channel_koid: KoId) {
    if wake_child_waiting_on(WaitTarget::Channel(channel_koid)) {
        let p = sched_ptr();
        unsafe {
            (&mut *p).safe_reschedule_pending = true;
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
    // The admitting process is whoever is executing this SVC: a resident child spawning
    // its own child, or Sora (also the fallback before any thread runs, as on the host).
    let parent = match s.dispatcher.current() {
        Some(current) if current != s.user_thread.koid() => s
            .children
            .iter()
            .find(|c| c.thread.koid() == current)
            .map_or(s.user_thread.process(), |c| c.process),
        _ => s.user_thread.process(),
    };
    let mut child = alloc::boxed::Box::new(ResidentChild {
        thread: new_child,
        fresh: true,
        ttbr0,
        process: proc_koid,
        parent,
        exit_code: 0,
        reports: true,
        done: false,
        current_call: None,
        fault_entry: 0,
        fault_stack_top: 0,
    });
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
        if s.children[i].done && !s.children[i].reports {
            // SH5: a sibling thread's end is not a process status.
            let koid = s.children[i].thread.koid();
            s.wait_queue.remove_thread(koid);
            s.children.remove(i);
        } else if s.children[i].done {
            let koid = s.children[i].thread.koid();
            // SH4: the exit status must survive the record — the parent's next ProcessWait
            // collects it from x1/x2.
            let entry = (
                s.children[i].parent,
                s.children[i].process,
                s.children[i].exit_code,
            );
            if s.reaped_len == s.reaped.len() {
                s.reaped.copy_within(1.., 0);
                s.reaped[s.reaped.len() - 1] = entry;
            } else {
                s.reaped[s.reaped_len] = entry;
                s.reaped_len += 1;
            }
            s.wait_queue.remove_thread(koid);
            s.children.remove(i);
        } else {
            i += 1;
        }
    }
    //j825
    // AX4: a capacity fill leaves these two vectors at their burst high-water marks even
    // after every child is reaped. They are kernel-heap state that survives the cycle, so
    // drop the slack here, where the resident set has just fallen; the sparse threshold
    // keeps ordinary spawn traffic from reallocating on every reap. — TIMBERDOODLE
    if s.children.capacity() > s.children.len().saturating_mul(2).saturating_add(16) {
        s.children.shrink_to_fit();
    }
    if s.wait_queue.entries.capacity()
        > s.wait_queue
            .entries
            .len()
            .saturating_mul(2)
            .saturating_add(16)
    {
        s.wait_queue.entries.shrink_to_fit();
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
    //j825
    // AX4: ask the fixed heap before taking a 16 KiB stack out of it. `KernelStack` also
    // reports allocation failure, but refusing here keeps the whole spawn — stack, record,
    // handle table — inside a known reserve. — TIMBERDOODLE
    if !spawn_heap_ready() {
        return Errno::NoMemory.status();
    }
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
        // AX4: the policy bound, not the compile-time ceiling, is the admission gate. A
        // refusal is final for this state (a policy cap never clears on retry), so it is
        // `BadState` rather than the old slot-pressure `ShouldWait`. Refusal precedes
        // install and admission, so no child, wait entry or dispatcher slot is created.
        if s.children.len() >= s.resident_limit as usize {
            return Errno::BadState.status();
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
        // The sync runner consumed this child's status as its return; drop the queue copy
        // so a later ProcessWait does not report it twice (SH4).
        let mut at = 0;
        while at < s.reaped_len {
            if s.reaped[at].1 == proc_koid {
                s.reaped.copy_within(at + 1..s.reaped_len, at);
                s.reaped_len -= 1;
                break;
            }
            at += 1;
        }
        code
    };
    if exit_code == 0 {
        Errno::Ok.status()
    } else {
        Errno::Internal.status()
    }
}

/// P10-g: spawn a child asynchronously using the proven resident-child slot.
/// The child is admitted at CHILD_ASYNC_PRIORITY (63, more urgent than Sora's boot 64,
/// less urgent than her woken 62) but does NOT preempt immediately. Returns immediately.
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
    match admit_resident(objects, proc_koid, root_vmar, ttbr0, entry, sp, arg, arg2) {
        Ok(_) => Errno::Ok.status(),
        Err(status) => status,
    }
}

/// SH5 `ThreadSpawn`: admit a sibling thread into the CALLER's own process. It shares the
/// caller's address space (`ttbr0`) and futex domain, inherits the caller's parent (so a
/// group exit reports to the right `ProcessWait` and a collected status still raises no
/// TOWER), and never reports a status of its own. Returns the new thread's koid. — TEAL
pub fn spawn_thread(
    objects: &mut ObjectManager,
    proc_koid: KoId,
    root_vmar: crate::mm::Vmar,
    ttbr0: u64,
    entry: u64,
    sp: u64,
    arg: u64,
) -> Result<KoId, Status> {
    let p = sched_ptr();
    let creator = unsafe {
        let s = &*p;
        let current = s.dispatcher.current();
        s.children
            .iter()
            .find(|c| Some(c.thread.koid()) == current && c.process == proc_koid)
            .map(|c| (c.parent, c.fault_entry))
    };
    let Some((parent, fault_entry)) = creator else {
        return Err(Errno::NotSupported.status());
    };
    let koid = admit_resident(objects, proc_koid, root_vmar, ttbr0, entry, sp, arg, 0)?;
    unsafe {
        let s = &mut *p;
        if let Some(record) = s.children.iter_mut().find(|c| c.thread.koid() == koid) {
            record.parent = parent;
            record.reports = false;
            // SH6: handlers are per process, alternate stacks per thread (POSIX).
            record.fault_entry = fault_entry;
            record.fault_stack_top = 0;
        }
    }
    Ok(koid)
}

/// SH6 `FaultHandlerSet` for the record `thread`: `entry` 0 clears. False when no
/// resident record is that thread (the supervisor has none).
fn set_fault_entry_for(s: &mut UserSched, thread: KoId, entry: u64, stack_top: u64) -> bool {
    let Some(record) = s.children.iter_mut().find(|c| c.thread.koid() == thread) else {
        return false;
    };
    record.fault_entry = entry;
    record.fault_stack_top = if entry == 0 { 0 } else { stack_top };
    true
}

/// SH6: take, and clear, `thread`'s fault entry for one delivery.
fn take_fault_entry_for(s: &mut UserSched, thread: KoId) -> Option<(u64, u64)> {
    let record = s.children.iter_mut().find(|c| c.thread.koid() == thread)?;
    if record.fault_entry == 0 {
        return None;
    }
    let taken = (record.fault_entry, record.fault_stack_top);
    record.fault_entry = 0;
    record.fault_stack_top = 0;
    Some(taken)
}

/// SH6 `FaultHandlerSet` for the CURRENT thread. — TEAL
pub fn set_current_fault_entry(entry: u64, stack_top: u64) -> bool {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if !unsafe { (&*opt).is_some() } {
        return false;
    }
    let s = unsafe { &mut *sched_ptr() };
    let Some(current) = s.dispatcher.current() else {
        return false;
    };
    set_fault_entry_for(s, current, entry, stack_top)
}

/// SH6: the CURRENT thread's fault entry, taken for one delivery (see `FaultHandlerSet`).
pub fn take_current_fault_entry() -> Option<(u64, u64)> {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if !unsafe { (&*opt).is_some() } {
        return None;
    }
    let s = unsafe { &mut *sched_ptr() };
    let current = s.dispatcher.current()?;
    take_fault_entry_for(s, current)
}

/// SH6-b: every live process below `root` in the spawn tree (each resident record names
/// the process that admitted it), nearest first. `root` itself is not included. This is
/// the KUMO stand-in for a process group: a spawned child leads the group of everything
/// it spawned in turn. — TEAL
pub fn process_descendants(root: KoId) -> alloc::vec::Vec<KoId> {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if !unsafe { (&*opt).is_some() } {
        return alloc::vec::Vec::new();
    }
    let s = unsafe { &*sched_ptr() };
    descendants_in(&s.children, root)
}

fn descendants_in(
    children: &[alloc::boxed::Box<ResidentChild>],
    root: KoId,
) -> alloc::vec::Vec<KoId> {
    let mut found: alloc::vec::Vec<KoId> = alloc::vec::Vec::new();
    let mut frontier = 0;
    let mut parents = alloc::vec![root];
    while frontier < parents.len() {
        let parent = parents[frontier];
        frontier += 1;
        for child in children.iter().filter(|c| !c.done && c.parent == parent) {
            if child.process != root && !found.contains(&child.process) {
                found.push(child.process);
                parents.push(child.process);
            }
        }
    }
    found
}

/// Live (not yet exited) threads of `process` among the resident records (SH5).
pub fn process_live_threads(process: KoId) -> usize {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if !unsafe { (&*opt).is_some() } {
        return 0;
    }
    let p = sched_ptr();
    unsafe {
        (&*p)
            .children
            .iter()
            .filter(|c| c.process == process && !c.done)
            .count()
    }
}

/// SH5 group exit: before the current thread's process is torn down, end every sibling
/// thread exactly as `ProcessKill` would, and make the current record the one that reports
/// the process's status. Returns how many siblings were ended. A no-op for the supervisor.
pub fn end_sibling_threads_for_exit() -> u32 {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if !unsafe { (&*opt).is_some() } {
        return 0;
    }
    let p = sched_ptr();
    let process = unsafe {
        let s = &*p;
        let current = s.dispatcher.current();
        s.children
            .iter()
            .find(|c| Some(c.thread.koid()) == current)
            .map(|c| c.process)
    };
    let Some(process) = process else {
        return 0;
    };
    let report = kill_process_threads(process);
    unsafe {
        let s = &mut *p;
        let current = s.dispatcher.current();
        for child in s.children.iter_mut().filter(|c| c.process == process) {
            child.reports = Some(child.thread.koid()) == current;
        }
    }
    report.killed
}

/// SH5 `ThreadExit` of a thread that is not its process's last: this record's end is not a
/// process status. The caller then ends the thread through [`exit_current_user`].
pub fn silence_current_thread() {
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        let current = s.dispatcher.current();
        if let Some(child) = s
            .children
            .iter_mut()
            .find(|c| Some(c.thread.koid()) == current)
        {
            child.reports = false;
        }
    }
}

/// The resident-admission core shared by `ProcessRun` ASYNC and `ThreadSpawn` (SH5).
fn admit_resident(
    objects: &mut ObjectManager,
    proc_koid: KoId,
    root_vmar: crate::mm::Vmar,
    ttbr0: u64,
    entry: u64,
    sp: u64,
    arg: u64,
    arg2: u64,
) -> Result<KoId, Status> {
    let p = sched_ptr();
    //j825
    // AX4: reap and gate BEFORE allocating the 16 KiB kernel stack. The old order built the
    // Thread first, so an over-limit spawn still consumed (and, on a full heap, could not
    // free) a stack, and the capacity refusal depended on allocation success. The preflight
    // is authoritative here: single-core SVC execution means nothing can grow the resident
    // set between this check and the install below, and the second check keeps refusal
    // ahead of install even if that ever changes. — TIMBERDOODLE
    unsafe {
        let s = &mut *p;
        reap_done(s);
        // AX4: the policy bound, not the compile-time ceiling, is the admission gate. A
        // refusal is final for this state (a policy cap never clears on retry), so it is
        // `BadState` rather than the old slot-pressure `ShouldWait`. Refusal precedes
        // install and admission, so no child, wait entry or dispatcher slot is created.
        if s.children.len() >= s.resident_limit as usize {
            return Err(Errno::BadState.status());
        }
    }
    if !spawn_heap_ready() {
        return Err(Errno::NoMemory.status());
    }

    let temp_process = Process::from_parts(proc_koid, root_vmar);
    let new_child = match Thread::new(objects, &temp_process, 0, 0, DEFAULT_KERNEL_STACK_SIZE) {
        Ok(thread) => thread,
        Err(_) => return Err(Errno::NoMemory.status()),
    };

    let koid = unsafe {
        let s = &mut *p;
        if s.children.len() >= s.resident_limit as usize {
            return Err(Errno::BadState.status());
        }

        let koid = install_child(s, new_child, proc_koid, ttbr0, entry, sp, arg, arg2);
        // Admit at CHILD_ASYNC_PRIORITY (63) but do NOT reschedule — Sora continues.
        s.dispatcher.admit(koid, CHILD_ASYNC_PRIORITY);
        koid
    };
    Ok(koid)
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

/// Exit code recorded for a thread `ProcessKill` ended: 128 + 9, what a Unix shell reports for a
/// process killed by `SIGKILL`.
pub const KILLED_EXIT_CODE: u64 = 137;

const _: () = assert!(CHILD_PRIORITY.0 == CHILD_ASYNC_PRIORITY.0);

/// What a killed process's threads were doing when [`kill_process_threads`] took them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KillReport {
    /// Threads taken off the scheduler.
    pub killed: u32,
    /// Of those, how many were waiting for the CPU rather than parked.
    pub ready: u32,
    /// The first parked thread's wait: what kind of object, and its koid (a futex's address).
    pub parked_on: Option<(&'static str, u64)>,
    /// That wait's channel endpoint, when it was a channel, so the caller can ask whether
    /// messages were already waiting there.
    pub parked_channel: Option<KoId>,
}

/// The scheduler half of `ProcessKill` (J556): take every live resident thread of `process` out
/// of its wait or off the run queue, terminate it, and leave it `done` for the next reap, as if it
/// had exited. The caller holds no `SoraState` borrow, and closes the process's handles next;
/// that is what wakes its peers. The running thread is never touched: it is the killer, and the
/// killer may not name its own process.
///
/// A parked thread is left suspended in its syscall on its own kernel stack, and that frame is
/// never resumed. That is safe for the same reason preempting one is: it holds no `SoraState`
/// borrow while parked. — TIMBERDOODLE
pub fn kill_process_threads(process: KoId) -> KillReport {
    kill_process_threads_with(process, KILLED_EXIT_CODE)
}

/// [`kill_process_threads`] with the status the kill reports (SH6-b: a signal death is
/// `0x100 | signo`, which relibc's `waitpid` turns into WIFSIGNALED). — TEAL
pub fn kill_process_threads_with(process: KoId, code: u64) -> KillReport {
    let mut report = KillReport::default();
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    if !unsafe { (&*opt).is_some() } {
        return report;
    }
    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        let current = s.dispatcher.current();
        for child in s.children.iter_mut() {
            let thread = child.thread.koid();
            if child.process != process || child.done || Some(thread) == current {
                continue;
            }
            match s.wait_queue.target_of(thread) {
                Some(target) => {
                    s.wait_queue.remove_thread(thread);
                    if report.parked_on.is_none() {
                        report.parked_on = Some(target.describe());
                        if let WaitTarget::Channel(koid) = target {
                            report.parked_channel = Some(koid);
                        }
                    }
                }
                // Blocking and async children share one priority (asserted below).
                None => {
                    if s.dispatcher.remove_ready_rt(thread, CHILD_PRIORITY) {
                        report.ready += 1;
                    }
                }
            }
            child.exit_code = code;
            child.done = true;
            child.thread.terminate();
            report.killed += 1;
        }
        // SH5: the process's status rides exactly one record. If its reporting thread had
        // already ended on its own (`ThreadExit`), the first killed sibling carries the kill.
        if report.killed > 0 && !s.children.iter().any(|c| c.process == process && c.reports) {
            if let Some(first) = s
                .children
                .iter_mut()
                .find(|c| c.process == process && c.done && c.exit_code == code)
            {
                first.reports = true;
            }
        }
    }
    report
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

fn wake_child_waiting_on(target: WaitTarget) -> bool {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    let started = unsafe { (&*opt).is_some() };
    if !started {
        return false;
    }

    let p = sched_ptr();
    unsafe {
        let s = &mut *p;
        let Some(waiter) = s.wait_queue.waiter_for(target) else {
            return false;
        };
        // The wait queue keys on object koid, so `waiter` is exactly the child parked on
        // this object — wake that one, leaving any sibling resident on its own object asleep.
        let Some(child) = s
            .children
            .iter_mut()
            .find(|c| c.thread.koid() == waiter)
            .map(|c| &mut c.thread)
        else {
            return false;
        };
        if !matches!(child.state(), ThreadState::Blocked) {
            return false;
        }
        s.wait_queue.remove_thread(waiter);
        child.ready();
        s.dispatcher.admit(child.koid(), CHILD_PRIORITY);
        if wait_target_needs_safe_reschedule(target) {
            s.safe_reschedule_pending = true;
        }
        true
    }
}

/// Dispatch a resident child that was made runnable by a timer/device wake once we are
/// back at a safe syscall boundary. The SVC entry frame already owns the interrupted
/// EL0 GPR/FP state on the current thread's kernel stack; this avoids the J287/J288
/// broad IRQ-preemption corruption while still giving timer-backed `PortWait` a
/// deterministic resume point. — KESTREL
//j724
/// The deferred user-wake decision: a pending wake is due exactly when the scheduler is
/// live, not done, and Sora is still blocked. Pure so the host tests pin it. — TIMBERDOODLE
fn deferred_user_wake_due(done: bool, pending: bool, user_blocked: bool) -> bool {
    !done && pending && user_blocked
}

pub fn reschedule_if_pending_after_svc() {
    let opt: *const Option<UserSched> = USER_SCHED.0.get();
    let started = unsafe { (&*opt).is_some() };
    if !started {
        return;
    }

    let p = sched_ptr();
    let switch = unsafe {
        let s = &mut *p;
        if s.done {
            return;
        }
        //j724
        // Pay a deferred user wake at this SVC boundary. `wake_user_after_irq_signal` can
        // only record `user_wake_pending` when the IRQ interrupted a child, and the flag was
        // previously pumped only by `pump_idle_floor` — which needs the idle floor to be
        // current, and sustained churn keeps a child runnable at every scheduling point, so a
        // parked Sora's wake starved for seconds (J723's metal verdict). This is the SVC-side
        // payer the doctrine always described: make her ready here and let the ordinary
        // reschedule machinery switch at this safe boundary. — TIMBERDOODLE
        if deferred_user_wake_due(
            false,
            s.user_wake_pending,
            matches!(s.user_thread.state(), ThreadState::Blocked),
        ) {
            s.user_wake_pending = false;
            s.user_thread.ready();
            s.dispatcher.admit(s.user_thread.koid(), USER_WAKE_PRIORITY);
            s.safe_reschedule_pending = true;
            //j726
            #[cfg(target_os = "none")]
            {
                USER_READY_AT.store(kumo_hal::active::monotonic_nanos(), Ordering::Relaxed);
                let readies = USER_WAKE_READIES
                    .fetch_add(1, Ordering::Relaxed)
                    .wrapping_add(1);
                if readies & 0x3f == 0 {
                    crate::klog_at!(
                        kumo_abi::sys::log_level::NOTICE,
                        "sched: user-wake readies={} kept-child={} ready-to-run-max={}ms over1s={}\n",
                        readies,
                        USER_WAKE_KEPT_CHILD.load(Ordering::Relaxed),
                        USER_RESUME_MAX_NS.load(Ordering::Relaxed) / 1_000_000,
                        USER_RESUME_OVER_1S.load(Ordering::Relaxed)
                    );
                }
            }
        }
        if !s.safe_reschedule_pending {
            return;
        }
        let decision = s.dispatcher.reschedule_current();
        let switch = dispatch_context(s, decision);
        //j726
        #[cfg(target_os = "none")]
        if switch.is_none() && s.user_thread.state() == ThreadState::Ready {
            USER_WAKE_KEPT_CHILD.fetch_add(1, Ordering::Relaxed);
        }
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
                s.dispatcher.admit(s.user_thread.koid(), USER_WAKE_PRIORITY);
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
        s.dispatcher.admit(s.user_thread.koid(), USER_WAKE_PRIORITY);
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
            //j729
            #[cfg(target_os = "none")]
            let user_was_running = matches!(s.user_thread.state(), ThreadState::Running);
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
            //j729
            #[cfg(target_os = "none")]
            {
                let user_koid = s.user_thread.koid();
                if from == Some(user_koid) && to != user_koid {
                    if user_was_running {
                        SORA_AWAY_AT.store(kumo_hal::active::monotonic_nanos(), Ordering::Relaxed);
                        SORA_PREEMPT_COUNT.fetch_add(1, Ordering::Relaxed);
                    }
                } else if to == user_koid {
                    let away_at = SORA_AWAY_AT.swap(0, Ordering::Relaxed);
                    if away_at != 0 {
                        let delta = kumo_hal::active::monotonic_nanos().saturating_sub(away_at);
                        SORA_AWAY_TOTAL_NS.fetch_add(delta, Ordering::Relaxed);
                        SORA_AWAY_MAX_NS.fetch_max(delta, Ordering::Relaxed);
                        let n = SORA_AWAY_RESUMES
                            .fetch_add(1, Ordering::Relaxed)
                            .wrapping_add(1);
                        if n & 0x1f == 0 {
                            crate::klog_at!(
                                kumo_abi::sys::log_level::NOTICE,
                                "sched: user-away resumes={} preempts={} max={}ms total={}ms\n",
                                n,
                                SORA_PREEMPT_COUNT.load(Ordering::Relaxed),
                                SORA_AWAY_MAX_NS.load(Ordering::Relaxed) / 1_000_000,
                                SORA_AWAY_TOTAL_NS.load(Ordering::Relaxed) / 1_000_000
                            );
                        }
                    }
                }
            }
            //j726
            #[cfg(target_os = "none")]
            if to == s.user_thread.koid() {
                let ready_at = USER_READY_AT.swap(0, Ordering::Relaxed);
                if ready_at != 0 {
                    let delta = kumo_hal::active::monotonic_nanos().saturating_sub(ready_at);
                    USER_RESUME_MAX_NS.fetch_max(delta, Ordering::Relaxed);
                    if delta >= 1_000_000_000 {
                        let over = USER_RESUME_OVER_1S.fetch_add(1, Ordering::Relaxed);
                        if over & 0x3 == 0 {
                            crate::klog_at!(
                                kumo_abi::sys::log_level::NOTICE,
                                "sched: user resumed ready-to-run={}ms late (over1s={})\n",
                                delta / 1_000_000,
                                over + 1
                            );
                        }
                    }
                }
            }
            Some((prev, next))
        }
        Decision::Idle | Decision::Continue(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{wait_target_needs_safe_reschedule, KoId, WaitQueue, WaitTarget};

    //j724
    #[test]
    fn deferred_user_wake_is_due_only_for_a_blocked_live_scheduler() {
        assert!(super::deferred_user_wake_due(false, true, true));
        assert!(!super::deferred_user_wake_due(false, true, false));
        assert!(!super::deferred_user_wake_due(false, false, true));
        assert!(!super::deferred_user_wake_due(true, true, true));
    }

    #[test]
    fn target_of_reports_where_a_thread_is_parked() {
        // What a kill report prints: the object the thread was parked on (J556).
        let mut q = WaitQueue::new();
        q.park(KoId(7), WaitTarget::Channel(KoId(42)));
        assert_eq!(q.target_of(KoId(7)), Some(WaitTarget::Channel(KoId(42))));
        assert_eq!(q.target_of(KoId(8)), None);
        assert_eq!(WaitTarget::Channel(KoId(42)).describe(), ("channel", 42));
        let futex = WaitTarget::Futex {
            process: KoId(1),
            addr: 0x1000,
        };
        assert_eq!(futex.describe(), ("futex", 0x1000));
        q.remove_thread(KoId(7));
        assert_eq!(q.target_of(KoId(7)), None);
    }

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

//j825
/// AX4 capacity and steadiness tests. They share the one process-global UserSched, so every
/// test here takes the same lock and the whole acceptance sequence runs in one #[test].
#[cfg(test)]
mod ax4_capacity_tests {
    use super::*;
    use crate::mm::Vmar;

    static AX4_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn sched() -> &'static UserSched {
        unsafe { &*super::sched_ptr() }
    }

    fn sched_mut() -> &'static mut UserSched {
        unsafe { &mut *super::sched_ptr() }
    }

    /// Spawn one never-run resident actor. `proc_koid` identifies the process for
    /// `kill_process_threads`; entry/sp are never entered on the host.
    fn spawn_actor(objects: &mut ObjectManager, proc_koid: u64) -> Status {
        let base = 0x1000_0000 + (proc_koid % 0x1000) * 0x10_0000;
        let vmar = Vmar::new(base, PAGE_SIZE * 16).unwrap();
        super::spawn_child_async(
            objects,
            KoId(proc_koid),
            vmar,
            0,
            0x1000 + (proc_koid & 0xfff) * 0x10,
            0x2000_0000,
            proc_koid,
            0,
        )
    }

    fn state_addr(child: &ResidentChild) -> u64 {
        child.thread.user_state.as_ref().unwrap() as *const UserState as u64
    }

    #[test]
    fn policy_gate_reap_and_repeated_waves_keep_a_flat_census() {
        let _guard = AX4_LOCK.lock().unwrap();
        let mut objects = ObjectManager::new();
        let vmar = Vmar::new(0xffff_0000_0000_0000, PAGE_SIZE * 256).unwrap();
        init(&mut objects, KoId(7), vmar, 0).unwrap();

        // The root Job's default is the resident ceiling and init mirrors it.
        assert_eq!(resident_limit(), kumo_abi::actor::RESIDENT_MAX as u32);
        assert_eq!(super::MAX_RESIDENT_CHILDREN, 288);
        assert!(set_resident_limit(3));

        // Exactly at the policy limit is admitted.
        for proc_koid in [100, 101, 102] {
            assert_eq!(spawn_actor(&mut objects, proc_koid), Errno::Ok.status());
        }
        let at_limit = resident_snapshot_wide();
        assert_eq!(
            (at_limit.capacity, at_limit.live, at_limit.stored),
            (3, 3, 3)
        );
        assert_eq!(at_limit.blocked + at_limit.ready + at_limit.running, 3);
        assert_eq!(sched().dispatcher.runnable_count(), 3);
        let addresses = [
            state_addr(&sched().children[0]),
            state_addr(&sched().children[1]),
            state_addr(&sched().children[2]),
        ];

        // One over is refused with BadState, and the refusal changes nothing: same census,
        // same koid list, no dispatcher slot, no wait entry, no panic.
        assert!(!resident_has_capacity());
        assert_eq!(spawn_actor(&mut objects, 200), Errno::BadState.status());
        let refused = resident_snapshot_wide();
        assert_eq!((refused.capacity, refused.live, refused.stored), (3, 3, 3));
        assert_eq!(refused.processes[..3], at_limit.processes[..3]);
        assert_eq!(sched().dispatcher.runnable_count(), 3);
        assert!(sched().wait_queue.entries.is_empty());
        assert_eq!(sched().children.len(), 3);
        assert_eq!(resident_live(), 3);

        // A parked child is removed from the wait queue (and never woken) when its process
        // is killed: the reaped koid cannot reappear at a later channel wake.
        let parked = sched().children[0].thread.koid();
        assert!(sched_mut()
            .dispatcher
            .remove_ready_rt(parked, super::CHILD_PRIORITY));
        sched_mut()
            .wait_queue
            .park(parked, WaitTarget::Channel(KoId(777)));
        let kill_report = kill_process_threads(KoId(100));
        assert_eq!(kill_report.killed, 1);
        assert_eq!(kill_report.parked_on, Some(("channel", 777)));
        assert_eq!(kill_report.parked_channel, Some(KoId(777)));
        assert!(sched().wait_queue.entries.is_empty());
        assert_eq!(sched().dispatcher.runnable_count(), 2);
        // The policy count prunes the terminated child first, so a bound at this value can
        // never yield a snapshot whose stored count exceeds its capacity (AXR2 validity).
        assert_eq!(sched().children.len(), 3);
        assert_eq!(resident_live(), 2);
        assert_eq!(sched().children.len(), 2);
        let pruned = resident_snapshot_wide();
        assert_eq!((pruned.capacity, pruned.live, pruned.stored), (3, 2, 2));
        // The preflight sees the reaped slot immediately (it reaps before answering).
        assert!(resident_has_capacity());

        // Reap one, then the same spawn succeeds: admission runs after reap_done, so a
        // finished child frees its slot on the very next attempt.
        assert_eq!(spawn_actor(&mut objects, 200), Errno::Ok.status());
        assert_eq!(sched().children.len(), 3);
        assert_eq!(resident_live(), 3);
        assert_eq!(sched().dispatcher.runnable_count(), 3);
        // J169: removing the first box shifted the remaining BOXES, not their pointees.
        // The inline UserState addresses baked into x19 at install are unchanged.
        assert_eq!(state_addr(&sched().children[0]), addresses[1]);
        assert_eq!(state_addr(&sched().children[1]), addresses[2]);
        assert_ne!(state_addr(&sched().children[2]), 0);

        // The frozen AX0 view still fits exactly twelve, and refuses cleanly above it.
        assert!(set_resident_limit(kumo_abi::actor::RESIDENT_MAX as u32));
        for proc_koid in [101, 102, 200] {
            assert_eq!(kill_process_threads(KoId(proc_koid)).killed, 1);
        }
        assert_eq!(process_wait(), Errno::Ok.status());
        assert!(sched().children.is_empty());
        for i in 0..12u64 {
            assert_eq!(spawn_actor(&mut objects, 300 + i), Errno::Ok.status());
        }
        let twelve = resident_snapshot().expect("12 slots fit AXR1");
        assert_eq!((twelve.capacity, twelve.live, twelve.stored), (12, 12, 12));
        assert_eq!(twelve.processes[11], 311);
        assert_eq!(spawn_actor(&mut objects, 312), Errno::Ok.status());
        assert_eq!(resident_snapshot(), None, "AXB1 must fail, not truncate");
        assert_eq!(resident_snapshot_wide().live, 13);
        for i in 0..13u64 {
            assert_eq!(kill_process_threads(KoId(300 + i)).killed, 1);
        }
        assert_eq!(process_wait(), Errno::Ok.status());

        // The cap itself is the last admissible value; one past it is InvalidArgs at the
        // policy layer, and the compile-time array is never exceeded. The scheduler mirror
        // re-checks independently: no direct caller can install 0, >288, or a bound below
        // the stored set, so the AXR2 invariant `stored <= capacity` always holds.
        assert_eq!(
            crate::task::validate_resident_limit(289, 0),
            Err(Errno::InvalidArgs)
        );
        assert_eq!(crate::task::validate_resident_limit(288, 288), Ok(()));
        assert_eq!(
            crate::task::validate_resident_limit(288, 289),
            Err(Errno::BadState)
        );
        assert!(!set_resident_limit(0));
        assert!(!set_resident_limit(289));
        for i in 0..3u64 {
            assert_eq!(spawn_actor(&mut objects, 400 + i), Errno::Ok.status());
        }
        assert!(!set_resident_limit(2));
        assert_eq!(resident_limit(), 288);
        for i in 0..3u64 {
            assert_eq!(kill_process_threads(KoId(400 + i)).killed, 1);
        }
        assert_eq!(process_wait(), Errno::Ok.status());
        assert!(sched().children.is_empty());

        // Repeated 256-actor waves: each cycle must return to exactly the pre-wave state.
        let frame_floor = crate::mm::frames_issued();
        let proc_base = 10_000u64;
        for wave in 0..4u64 {
            for i in 0..256u64 {
                let proc_koid = proc_base + wave * 256 + i;
                assert_eq!(spawn_actor(&mut objects, proc_koid), Errno::Ok.status());
            }
            let peak = resident_snapshot_wide();
            assert_eq!(
                (peak.capacity, peak.live, peak.stored),
                (288, 256, 256),
                "wave {wave}"
            );
            assert_eq!(peak.blocked + peak.ready + peak.running, 256);
            assert_eq!(sched().dispatcher.runnable_count(), 256);
            assert_eq!(resident_snapshot(), None);
            // Host actor threads are Vec-backed; no frame is issued or retained by the
            // resident set itself.
            assert_eq!(crate::mm::frames_issued(), frame_floor);

            for i in 0..256u64 {
                let proc_koid = proc_base + wave * 256 + i;
                assert_eq!(kill_process_threads(KoId(proc_koid)).killed, 1);
            }
            assert_eq!(process_wait(), Errno::Ok.status());
            let clean = resident_snapshot_wide();
            assert_eq!(
                (
                    clean.live,
                    clean.stored,
                    clean.blocked,
                    clean.ready,
                    clean.running
                ),
                (0, 0, 0, 0, 0),
                "wave {wave} did not drain"
            );
            assert!(sched().children.is_empty());
            assert!(sched().wait_queue.entries.is_empty());
            //j825
            // A 256-actor wave grows both vectors to a 512 high-water mark. `reap_done`
            // must release that slack when the set drains; otherwise every wave keeps
            // ~16 KiB of dead capacity in the fixed kernel heap. — TIMBERDOODLE
            assert!(
                sched().children.capacity() <= 64,
                "wave {wave}: children slack"
            );
            assert!(
                sched().wait_queue.entries.capacity() <= 64,
                "wave {wave}: wait-queue slack"
            );
            assert_eq!(sched().dispatcher.runnable_count(), 0);
            assert_eq!(resident_live(), 0);
            assert_eq!(crate::mm::frames_issued(), frame_floor);
        }

        // 256 runnables rotate without starving a more urgent waiter: Sora's wake preempts
        // the whole class immediately (the same property actorstat's dispatcher test proves
        // on a private instance).
        for i in 0..256u64 {
            assert_eq!(spawn_actor(&mut objects, 20_000 + i), Errno::Ok.status());
        }
        assert_eq!(sched().dispatcher.runnable_count(), 256);
        match sched_mut().dispatcher.wake_rt(KoId(90_000), Priority(62)) {
            Decision::Switch { to, .. } => assert_eq!(to, KoId(90_000)),
            other => panic!("a more urgent Sora wake was starved by 256 actors: {other:?}"),
        }
        let mut remaining = 256;
        for i in 0..256u64 {
            let report = kill_process_threads(KoId(20_000 + i));
            remaining -= report.killed;
        }
        assert_eq!(remaining, 0);
        assert_eq!(process_wait(), Errno::Ok.status());
        assert!(sched().children.is_empty());
        assert_eq!(resident_live(), 0);
    }

    //j871
    /// SH4: a reaped child's status reaches its own parent's `ProcessWait` and no other
    /// pump — Sora's launches and a resident parent's spawns share one ring but never
    /// each other's entries. — TEAL
    #[test]
    fn reaped_statuses_are_reported_to_their_parent_only() {
        let _guard = AX4_LOCK.lock().unwrap();
        let mut objects = ObjectManager::new();
        let vmar = Vmar::new(0xffff_0000_0000_0000, PAGE_SIZE * 256).unwrap();
        init(&mut objects, KoId(7), vmar, 0).unwrap();
        let sora = sched().user_thread.process();

        // Three children: two admitted by resident parent 500, one by Sora (the host has no
        // running SVC context, so install_child records Sora; re-parent two by hand).
        for proc_koid in [600, 601, 602] {
            assert_eq!(spawn_actor(&mut objects, proc_koid), Errno::Ok.status());
        }
        assert!(sched().children.iter().all(|c| c.parent == sora));
        sched_mut().children[0].parent = KoId(500);
        sched_mut().children[1].parent = KoId(500);

        // Nothing reaped yet: both parents still have live children.
        assert_eq!(process_wait_for(KoId(500)), Errno::ShouldWait.status());
        assert_eq!(take_reaped(KoId(500)), None);
        assert_eq!(take_reaped(sora), None);

        // Kill 600 and Sora's 602. The resident parent sees exactly its own child; Sora's
        // pump sees exactly its own, in either order of asking.
        assert_eq!(kill_process_threads(KoId(600)).killed, 1);
        assert_eq!(kill_process_threads(KoId(602)).killed, 1);
        assert_eq!(process_wait_for(KoId(500)), Errno::ShouldWait.status());
        assert_eq!(take_reaped(KoId(999)), None);
        assert_eq!(take_reaped(sora), Some((KoId(602), KILLED_EXIT_CODE)));
        assert_eq!(take_reaped(sora), None);
        assert_eq!(take_reaped(KoId(500)), Some((KoId(600), KILLED_EXIT_CODE)));
        assert_eq!(take_reaped(KoId(500)), None);

        // The last child of 500 ends: its wait reports Ok and the entry is still delivered.
        assert_eq!(kill_process_threads(KoId(601)).killed, 1);
        assert_eq!(process_wait_for(KoId(500)), Errno::Ok.status());
        assert_eq!(take_reaped(KoId(500)), Some((KoId(601), KILLED_EXIT_CODE)));
        assert_eq!(take_reaped(KoId(500)), None);
        assert_eq!(sched().reaped_len, 0);
        assert_eq!(process_wait(), Errno::Ok.status());
        assert!(sched().children.is_empty());
    }
    //j873
    /// SH5: sibling threads share their process's status. A kill of a two-thread process
    /// reports once; a sibling's own exit reports nothing; and when the reporting thread
    /// ended first (`pthread_exit` on the main thread), a later kill still reports exactly
    /// once. `spawn_thread` itself needs a running creator, so the host models its result:
    /// a second record of the same process, silenced and carrying the creator's parent.
    /// — TEAL
    #[test]
    fn sibling_threads_report_one_status_per_process() {
        let _guard = AX4_LOCK.lock().unwrap();
        let mut objects = ObjectManager::new();
        let vmar = Vmar::new(0xffff_0000_0000_0000, PAGE_SIZE * 256).unwrap();
        init(&mut objects, KoId(7), vmar, 0).unwrap();
        let sora = sched().user_thread.process();
        let add_thread = |objects: &mut ObjectManager, proc_koid: u64| {
            assert_eq!(spawn_actor(objects, proc_koid), Errno::Ok.status());
            let record = sched_mut().children.last_mut().unwrap();
            record.reports = false;
        };

        // Two threads of 700: one kill, one status, both records gone after the reap.
        assert_eq!(spawn_actor(&mut objects, 700), Errno::Ok.status());
        add_thread(&mut objects, 700);
        assert_eq!(process_live_threads(KoId(700)), 2);
        assert_eq!(kill_process_threads(KoId(700)).killed, 2);
        assert_eq!(process_live_threads(KoId(700)), 0);
        assert_eq!(process_wait(), Errno::Ok.status());
        assert_eq!(take_reaped(sora), Some((KoId(700), KILLED_EXIT_CODE)));
        assert_eq!(take_reaped(sora), None);
        assert!(sched().children.is_empty());

        // A sibling of 701 exits on its own: its record is reaped with no status, and the
        // process is still live with one thread.
        assert_eq!(spawn_actor(&mut objects, 701), Errno::Ok.status());
        add_thread(&mut objects, 701);
        {
            let sibling = &mut sched_mut().children[1];
            let koid = sibling.thread.koid();
            assert!(sched_mut()
                .dispatcher
                .remove_ready_rt(koid, super::CHILD_PRIORITY));
            let sibling = &mut sched_mut().children[1];
            sibling.done = true;
            sibling.thread.terminate();
        }
        assert_eq!(process_live_threads(KoId(701)), 1);
        // Reap without a scheduler pump: on the host a pump would make 701 current.
        assert_eq!(process_wait_for(sora), Errno::ShouldWait.status());
        assert_eq!(sched().children.len(), 1);
        assert_eq!(take_reaped(sora), None);

        // 702's reporting (first) thread ends silently first, as pthread_exit does; the kill
        // of the survivor still yields exactly one status.
        assert_eq!(spawn_actor(&mut objects, 702), Errno::Ok.status());
        add_thread(&mut objects, 702);
        {
            let first = sched()
                .children
                .iter()
                .position(|c| c.process == KoId(702) && c.reports)
                .unwrap();
            let koid = sched().children[first].thread.koid();
            assert!(sched_mut()
                .dispatcher
                .remove_ready_rt(koid, super::CHILD_PRIORITY));
            let record = &mut sched_mut().children[first];
            record.reports = false;
            record.done = true;
            record.thread.terminate();
        }
        assert_eq!(process_live_threads(KoId(702)), 1);
        assert_eq!(kill_process_threads(KoId(702)).killed, 1);
        assert_eq!(kill_process_threads(KoId(701)).killed, 1);
        assert_eq!(process_wait(), Errno::Ok.status());
        let mut statuses = [take_reaped(sora), take_reaped(sora), take_reaped(sora)];
        statuses.sort();
        assert_eq!(
            statuses,
            [
                None,
                Some((KoId(701), KILLED_EXIT_CODE)),
                Some((KoId(702), KILLED_EXIT_CODE)),
            ]
        );
        assert!(sched().children.is_empty());
    }

    //j877
    /// SH6-b: the spawn tree below a process is found through each record's parent, a
    /// kill reports the status it was given, and a killed tree reports one status per
    /// process. — TEAL
    #[test]
    fn descendants_follow_the_spawn_tree_and_kills_carry_their_status() {
        let _guard = AX4_LOCK.lock().unwrap();
        let mut objects = ObjectManager::new();
        let vmar = Vmar::new(0xffff_0000_0000_0000, PAGE_SIZE * 256).unwrap();
        init(&mut objects, KoId(7), vmar, 0).unwrap();
        let sora = sched().user_thread.process();
        // 720 spawned 721 and 722; 721 spawned 723; 724 is unrelated.
        for koid in [720, 721, 722, 723, 724] {
            assert_eq!(spawn_actor(&mut objects, koid), Errno::Ok.status());
        }
        let set_parent = |process: u64, parent: u64| {
            for c in sched_mut()
                .children
                .iter_mut()
                .filter(|c| c.process == KoId(process))
            {
                c.parent = KoId(parent);
            }
        };
        set_parent(721, 720);
        set_parent(722, 720);
        set_parent(723, 721);
        let mut tree = process_descendants(KoId(720));
        tree.sort();
        assert_eq!(tree, [KoId(721), KoId(722), KoId(723)]);
        assert!(process_descendants(KoId(723)).is_empty());
        assert!(process_descendants(KoId(724)).is_empty());

        const SIGINT_DEATH: u64 = 0x100 | 2;
        for koid in [723, 722, 721, 720] {
            assert_eq!(
                kill_process_threads_with(KoId(koid), SIGINT_DEATH).killed,
                1
            );
        }
        // Dead records are not tree members.
        assert!(process_descendants(KoId(720)).is_empty());
        assert_eq!(kill_process_threads(KoId(724)).killed, 1);
        assert_eq!(process_wait(), Errno::Ok.status());
        assert_eq!(take_reaped(sora), Some((KoId(720), SIGINT_DEATH)));
        assert_eq!(take_reaped(sora), Some((KoId(724), KILLED_EXIT_CODE)));
        assert_eq!(
            take_reaped(KoId(720)).map(|(_, code)| code),
            Some(SIGINT_DEATH)
        );
        assert_eq!(
            take_reaped(KoId(720)).map(|(_, code)| code),
            Some(SIGINT_DEATH)
        );
        assert_eq!(take_reaped(KoId(721)), Some((KoId(723), SIGINT_DEATH)));
        assert!(sched().children.is_empty());
    }

    /// SH6: a fault registration is per thread, one-shot, and cleared by entry 0; the
    /// supervisor (no resident record) cannot register. — TEAL
    #[test]
    fn fault_entry_is_per_thread_and_one_shot() {
        let _guard = AX4_LOCK.lock().unwrap();
        let mut objects = ObjectManager::new();
        let vmar = Vmar::new(0xffff_0000_0000_0000, PAGE_SIZE * 256).unwrap();
        init(&mut objects, KoId(7), vmar, 0).unwrap();
        assert_eq!(spawn_actor(&mut objects, 710), Errno::Ok.status());
        assert_eq!(spawn_actor(&mut objects, 711), Errno::Ok.status());
        let a = sched().children[0].thread.koid();
        let b = sched().children[1].thread.koid();
        let sora_thread = sched().user_thread.koid();

        assert!(!set_fault_entry_for(sched_mut(), sora_thread, 0x40_1000, 0));
        assert!(set_fault_entry_for(sched_mut(), a, 0x40_1000, 0x7000_0000));
        assert_eq!(take_fault_entry_for(sched_mut(), b), None);
        assert_eq!(
            take_fault_entry_for(sched_mut(), a),
            Some((0x40_1000, 0x7000_0000))
        );
        // One-shot: the delivery consumed it.
        assert_eq!(take_fault_entry_for(sched_mut(), a), None);
        // Entry 0 clears, stack and all.
        assert!(set_fault_entry_for(sched_mut(), b, 0x40_2000, 0x7100_0000));
        assert!(set_fault_entry_for(sched_mut(), b, 0, 0x7100_0000));
        assert_eq!(take_fault_entry_for(sched_mut(), b), None);

        assert_eq!(kill_process_threads(KoId(710)).killed, 1);
        assert_eq!(kill_process_threads(KoId(711)).killed, 1);
        assert_eq!(process_wait(), Errno::Ok.status());
        while take_reaped(sched().user_thread.process()).is_some() {}
        assert!(sched().children.is_empty());
    }
}
