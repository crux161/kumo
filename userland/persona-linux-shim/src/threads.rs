//j916
//j917
//j919
//j924
//j925
//! SH7-d3c-2: the minimum Linux thread (the ratified SH7-d3 cut, T1).
//!
//! **Threads.** A Linux `clone` with musl's pthread flag set becomes a KUMO `ThreadSpawn` in
//! this process. The new thread starts in the shim (`persona_thread_entry`) on a trap stack of
//! its own, registers itself (`PersonaShimSet` is per thread), and has the kernel open a call
//! at its parent's `svc` (`PersonaContext` SEED) with the stack the program chose. Its first
//! `PersonaReturn` then enters the program exactly where `clone` returns, with every register
//! its parent had there and `x0` = 0, as on Linux.
//!
//! **Per-thread state** lives in a [`ThreadRec`] at the top of each thread's trap stack. The
//! kernel enters the shim with SP = that stack's top, so the trap path finds its record without
//! a lookup or a TLS register (TPIDR_EL0 belongs to the program).
//!
//! **The shim lock.** Everything the shim keeps for the process (the descriptor table, the vfs
//! session, the program break) is behind one lock, taken for every call except the ones here
//! that touch only the calling thread or the kernel. A call that must block (a pipe or stdin
//! read with nothing to read) parks with the lock released, so a sibling can make it ready.
//! — TEAL

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use kumo_rt::{
    clock_get, futex_wait, futex_wake, process_exit, sleep_ns, thread_self, thread_yield,
    vmar_map_anywhere, vmar_unmap,
};
use persona_linux::thread::{self, Deadline, FutexCall, FutexRefusal};

use crate::{errno, log, vmo};

/// One thread's shim stack. A call's deepest path (a pipe read parks a 4 KiB message on it)
/// stays well inside.
pub const TRAP_STACK_BYTES: usize = 64 * 1024;

/// One Linux thread's shim state, at the top of its trap stack (the address the kernel puts in
/// SP on every redirected call). The first fields are what `persona_thread_entry` reads; their
/// offsets are fixed by the assertions below.
#[repr(C, align(16))]
pub struct ThreadRec {
    /// A clone child's first registers: its parent's x0..x30 at the `clone` svc.
    pub frame: [u64; 31],
    /// The child's TPIDR_EL0: CLONE_SETTLS's `tls`, else the parent's.
    pub tls: u64,
    /// The foreign context the child's first `PersonaReturn` resumes: the parent's PC after the
    /// svc, the stack the program gave `clone`, and x8.
    pub pc: u64,
    pub sp: u64,
    pub x8: u64,
    /// The Linux tid (the thread's koid).
    pub tid: u64,
    /// CLONE_CHILD_CLEARTID / `set_tid_address`: zeroed and woken when the thread exits.
    pub clear_tid: u64,
    /// `set_robust_list`'s head, recorded (the list is not walked at exit in this slice).
    pub robust_head: u64,
    /// The trap stack's own mapping, released by `ThreadExit`; 0 for the main thread's static.
    pub region_base: u64,
    pub region_len: u64,
    /// Where the child itself writes its tid before the program runs (PARENT/CHILD_SETTID).
    pub parent_tid_at: u64,
    pub child_tid_at: u64,
    /// j917: this thread's blocked-signal mask (`rt_sigprocmask`), inherited by its clones.
    pub sigmask: u64,
    /// j917: this thread's `sigaltstack` (sp, flags, size); a clone starts with none.
    pub alt_sp: u64,
    pub alt_flags: u64,
    pub alt_size: u64,
    /// j919 (SH7-e2): signals sent to this thread and not yet delivered (bit `sig - 1`), and
    /// which of them came from `kill` (SI_USER) rather than `tkill` (SI_TKILL).
    pub pending: AtomicU64,
    pub pending_user: AtomicU64,
    /// The futex word this thread is parked on in an untimed wait, or 0: a sender wakes it.
    pub waiting_on: AtomicU64,
    /// This thread's alert channel: senders write `alert_send`, its waits watch `alert_recv`.
    pub alert_recv: u32,
    pub alert_send: u32,
    /// j924 (SH7-f): the vfork child slot this thread runs as (processes.rs), 0 for the
    /// program's own threads. Its descriptors and dispositions are that slot's.
    pub child: u64,
}

const _: () = {
    assert!(core::mem::offset_of!(ThreadRec, tls) == 248);
    assert!(core::mem::size_of::<ThreadRec>() % 16 == 0);
};

impl ThreadRec {
    pub const EMPTY: ThreadRec = ThreadRec {
        frame: [0; 31],
        tls: 0,
        pc: 0,
        sp: 0,
        x8: 0,
        tid: 0,
        clear_tid: 0,
        robust_head: 0,
        region_base: 0,
        region_len: 0,
        parent_tid_at: 0,
        child_tid_at: 0,
        sigmask: 0,
        alt_sp: 0,
        alt_flags: crate::signals::SS_DISABLE,
        alt_size: 0,
        pending: AtomicU64::new(0),
        pending_user: AtomicU64::new(0),
        waiting_on: AtomicU64::new(0),
        alert_recv: 0,
        alert_send: 0,
        child: 0,
    };
}

/// The main thread's trap stack with its record on top (the record's address is the stack top).
#[repr(C, align(16))]
pub struct MainTrap {
    stack: [u8; TRAP_STACK_BYTES],
    pub rec: ThreadRec,
}

pub static mut MAIN_TRAP: MainTrap = MainTrap {
    stack: [0; TRAP_STACK_BYTES],
    rec: ThreadRec::EMPTY,
};

/// The process's Linux pid: the main thread's tid, as on Linux.
static PID: AtomicU32 = AtomicU32::new(0);

// ---- the shim lock -------------------------------------------------------------------------

/// 0 free, 1 held, 2 held with sleepers (a futex mutex; the kernel futex is per process).
static LOCK: AtomicU32 = AtomicU32::new(0);

pub fn lock() {
    if LOCK
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return;
    }
    while LOCK.swap(2, Ordering::Acquire) != 0 {
        let _ = futex_wait(LOCK.as_ptr(), 2);
    }
}

pub fn unlock() {
    if LOCK.swap(0, Ordering::Release) == 2 {
        let _ = futex_wake(LOCK.as_ptr(), 1);
    }
}

/// Run `wait` with the shim lock released: the caller holds it and gets it back.
pub fn unlocked<T>(wait: impl FnOnce() -> T) -> T {
    // j924: another thread's call may switch the active descriptor table meanwhile.
    let active = crate::files::active();
    unlock();
    let out = wait();
    lock();
    crate::files::set_active(active);
    out
}

/// Park until `word` is no longer `busy`, with the shim lock released (a reader waiting its
/// turn behind a sibling already parked on the same descriptor).
pub fn wait_turn(word: &AtomicU32) {
    unlocked(|| {
        while word.load(Ordering::Acquire) != 0 {
            let _ = futex_wait(word.as_ptr(), 1);
        }
    });
}

// ---- KUMO calls this module needs ----------------------------------------------------------

const SYSCALL_THREAD_SPAWN: u64 = 58;
const SYSCALL_THREAD_EXIT: u64 = 59;
const SYSCALL_PERSONA_CONTEXT: u64 = 65;

fn thread_spawn(entry: u64, sp: u64, arg: u64) -> (u64, u64) {
    let status: u64;
    let koid: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_THREAD_SPAWN,
            inlateout("x0") entry => status,
            inlateout("x1") sp => koid,
            in("x2") arg,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (status, koid)
}

fn thread_exit(stack_base: u64, stack_len: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "svc #0",
            "brk #0x52",
            in("x8") SYSCALL_THREAD_EXIT,
            in("x0") stack_base,
            in("x1") stack_len,
            options(noreturn, nostack),
        );
    }
}

/// `PersonaContext`: `(status, pc, sp, x8)`.
pub fn persona_context(op: u64, pc: u64, sp: u64, x8: u64) -> (u64, u64, u64, u64) {
    let status: u64;
    let (out_pc, out_sp, out_x8): (u64, u64, u64);
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_PERSONA_CONTEXT,
            inlateout("x0") op => status,
            inlateout("x1") pc => out_pc,
            inlateout("x2") sp => out_sp,
            inlateout("x3") x8 => out_x8,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (status, out_pc, out_sp, out_x8)
}

unsafe extern "C" {
    fn persona_thread_entry();
}

core::arch::global_asm!(
    ".text",
    // A clone child's first instruction (ThreadSpawn: x0 = SP = its ThreadRec). The Rust half
    // registers the thread and seeds its open call; then the program's registers are loaded
    // from the record and PersonaReturn enters the program where its parent's svc returns.
    ".global persona_thread_entry",
    ".type persona_thread_entry, %function",
    "persona_thread_entry:",
    "  mov x19, x0",
    "  bl persona_thread_start",
    "  ldr x0, [x19, #248]",
    "  msr tpidr_el0, x0",
    "  ldp x0, x1, [x19, #0]",
    "  ldp x2, x3, [x19, #16]",
    "  ldp x4, x5, [x19, #32]",
    "  ldp x6, x7, [x19, #48]",
    "  ldr x9, [x19, #72]",
    "  ldp x10, x11, [x19, #80]",
    "  ldp x12, x13, [x19, #96]",
    "  ldp x14, x15, [x19, #112]",
    "  ldp x16, x17, [x19, #128]",
    "  ldp x20, x21, [x19, #160]",
    "  ldp x22, x23, [x19, #176]",
    "  ldp x24, x25, [x19, #192]",
    "  ldp x26, x27, [x19, #208]",
    "  ldp x28, x29, [x19, #224]",
    "  ldr x30, [x19, #240]",
    "  mov x0, xzr", // clone returns 0 in the child
    "  mov x8, #64", // PersonaReturn (the kernel restores the seeded x8)
    "  ldp x18, x19, [x19, #144]",
    "  svc #0",
    "  brk #0x51",
);

// ---- registration --------------------------------------------------------------------------

/// Register the calling thread with the kernel: its foreign `svc`s now enter the shim on the
/// stack below `rec`.
fn register(rec: *mut ThreadRec) -> bool {
    let (code_base, code_len) = crate::shim_code();
    crate::persona_shim_set(crate::trap_entry_address(), code_base, code_len, rec as u64) == 0
}

/// The main thread, before it enters the program: its tid (also the pid) and registration.
/// False when its koid is no Linux tid or the kernel refused.
pub fn register_main() -> bool {
    let rec = unsafe { &mut *core::ptr::addr_of_mut!(MAIN_TRAP.rec) };
    let Some(tid) = thread::tid_of_koid(thread_self().0) else {
        return false;
    };
    rec.tid = tid as u64;
    // j924: a spawned process keeps the pid its parent's `clone` returned.
    if PID.load(Ordering::Relaxed) == 0 {
        PID.store(tid as u32, Ordering::Relaxed);
    }
    // j919: the program has no other thread yet, so the registry needs no lock here.
    crate::signals::register(rec) && register(rec)
}

/// The Rust half of `persona_thread_entry`. Returns only when the child can enter the program.
#[unsafe(no_mangle)]
extern "C" fn persona_thread_start(rec: *mut ThreadRec) {
    let r = unsafe { &mut *rec };
    let Some(tid) = thread::tid_of_koid(thread_self().0) else {
        child_failed("a thread koid does not fit a Linux tid");
    };
    r.tid = tid as u64;
    lock();
    let listed = crate::signals::register(r);
    unlock();
    if !listed {
        child_failed("no room or no alert channel for a thread");
    }
    if !register(rec) {
        child_failed("PersonaShimSet refused a thread");
    }
    if persona_context(kumo_abi::sys::PERSONA_CONTEXT_SEED, r.pc, r.sp, r.x8).0 != 0 {
        child_failed("PersonaContext refused a clone child's seed");
    }
    // PARENT_SETTID and CHILD_SETTID: on Linux both are written before either side runs. The
    // parent writes the parent copy too; this one makes sure the child never sees it unset.
    for at in [r.parent_tid_at, r.child_tid_at] {
        if at != 0 {
            unsafe { (at as *const AtomicU32).as_ref() }
                .unwrap()
                .store(tid as u32, Ordering::Release);
        }
    }
}

/// A thread the program already holds a tid for cannot start: Linux never fails here, so no
/// program can recover. End the process loudly rather than leave a joiner waiting forever.
fn child_failed(what: &str) -> ! {
    log(format_args!("persona-linux: clone child: {}\n", what));
    process_exit(127)
}

// ---- the calls -----------------------------------------------------------------------------

/// Linux `clone(flags, stack, parent_tid, tls, child_tid)`: a sibling thread, or (j924) the
/// `CLONE_VM | CLONE_VFORK` child musl's `posix_spawn` makes (processes.rs).
pub fn clone(
    parent: &ThreadRec,
    frame: &[u64; 31],
    flags: u64,
    stack: u64,
    ptid: u64,
    tls: u64,
    ctid: u64,
) -> i64 {
    if crate::processes::is_vfork(flags) {
        return crate::processes::vfork(parent, frame, stack);
    }
    // j925: no shared memory, no new thread: `fork` (musl's is `clone(SIGCHLD, 0)`).
    if flags & (CLONE_VM | CLONE_THREAD | CLONE_VFORK) == 0 && stack == 0 {
        return crate::processes::fork(parent, frame);
    }
    let Some(kind) = thread::thread_clone(flags) else {
        log(format_args!(
            "persona-linux: clone flags {:#x} are not a thread (-EINVAL)\n",
            flags
        ));
        return -errno::EINVAL;
    };
    let current: u64;
    unsafe { core::arch::asm!("mrs {}, tpidr_el0", out(reg) current) };
    let setup = Start {
        tls: if kind.set_tls { tls } else { current },
        stack,
        clear_tid: if kind.child_clear_tid { ctid } else { 0 },
        parent_tid_at: if kind.parent_set_tid { ptid } else { 0 },
        child_tid_at: if kind.child_set_tid { ctid } else { 0 },
        child: parent.child,
    };
    let tid = match start(parent, frame, &setup) {
        Ok(tid) => tid,
        Err(e) => return e,
    };
    if kind.parent_set_tid && ptid != 0 {
        unsafe { (ptid as *const AtomicU32).as_ref() }
            .unwrap()
            .store(tid as u32, Ordering::Release);
    }
    tid as i64
}

const CLONE_VM: u64 = 0x100;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_THREAD: u64 = 0x1_0000;

/// j925: the record and trap stack a fork child's one thread starts on, filled here so the
/// snapshot copies it: the parent's registers at the `clone` svc, its TLS, its open call's
/// resume point and its mask. `(record address, region base, region length)`; the parent
/// unmaps its own copy once the child is started.
pub fn prepare_fork_thread(parent: &ThreadRec, frame: &[u64; 31]) -> Result<(u64, u64, u64), i64> {
    let (status, pc, sp, x8) = persona_context(kumo_abi::sys::PERSONA_CONTEXT_GET, 0, 0, 0);
    if status != 0 {
        return Err(-errno::EINVAL);
    }
    let region_len = (TRAP_STACK_BYTES + core::mem::size_of::<ThreadRec>())
        .next_multiple_of(crate::PAGE as usize) as u64;
    let Some(region) = vmo(region_len) else {
        return Err(-errno::EAGAIN);
    };
    let mapped = vmar_map_anywhere(region, region_len, crate::MAP_R | crate::MAP_W);
    let _ = kumo_rt::handle_close(region);
    let region_base = match mapped {
        (0, at) if at != 0 => at,
        _ => return Err(-errno::EAGAIN),
    };
    let rec_at = (region_base + region_len - core::mem::size_of::<ThreadRec>() as u64) & !15;
    let rec = unsafe { &mut *(rec_at as *mut ThreadRec) };
    *rec = ThreadRec::EMPTY;
    rec.frame = *frame;
    unsafe { core::arch::asm!("mrs {}, tpidr_el0", out(reg) rec.tls) };
    rec.pc = pc;
    rec.sp = sp;
    rec.x8 = x8;
    rec.region_base = region_base;
    rec.region_len = region_len;
    rec.sigmask = parent.sigmask;
    rec.alt_sp = parent.alt_sp;
    rec.alt_flags = parent.alt_flags;
    rec.alt_size = parent.alt_size;
    Ok((rec_at, region_base, region_len))
}

/// j925: in a fork child, before its thread starts: no lock holder, its own pid.
pub fn reset_for_fork(pid: u32) {
    LOCK.store(0, Ordering::Relaxed);
    PID.store(pid, Ordering::Relaxed);
}

/// j925: `execve` in this process: the main record starts over (the pid stays).
pub fn reset_for_exec() {
    let rec = unsafe { &mut *core::ptr::addr_of_mut!(MAIN_TRAP.rec) };
    *rec = ThreadRec::EMPTY;
}

/// The Linux tid (and pid) a koid names.
pub fn tid_of(koid: u64) -> Option<u32> {
    thread::tid_of_koid(koid).map(|tid| tid as u32)
}

/// How a new shim thread starts: what `clone` gives it beyond the parent's registers.
pub struct Start {
    pub tls: u64,
    /// The program's stack for it, or 0 for the parent's.
    pub stack: u64,
    pub clear_tid: u64,
    pub parent_tid_at: u64,
    pub child_tid_at: u64,
    pub child: u64,
}

/// Start a thread that resumes the program where `parent`'s open call returns, with x0 = 0:
/// its own trap stack with its record on top. Returns its Linux tid.
pub fn start(parent: &ThreadRec, frame: &[u64; 31], setup: &Start) -> Result<u32, i64> {
    let (status, pc, parent_sp, x8) = persona_context(kumo_abi::sys::PERSONA_CONTEXT_GET, 0, 0, 0);
    if status != 0 {
        return Err(-errno::EINVAL);
    }
    // The child's trap stack, with its record on top.
    let region_len = (TRAP_STACK_BYTES + core::mem::size_of::<ThreadRec>())
        .next_multiple_of(crate::PAGE as usize) as u64;
    let Some(region) = vmo(region_len) else {
        return Err(-errno::EAGAIN);
    };
    let mapped = vmar_map_anywhere(region, region_len, crate::MAP_R | crate::MAP_W);
    let _ = kumo_rt::handle_close(region);
    let region_base = match mapped {
        (0, at) if at != 0 => at,
        _ => return Err(-errno::EAGAIN),
    };
    let rec_at = (region_base + region_len - core::mem::size_of::<ThreadRec>() as u64) & !15;
    let rec = unsafe { &mut *(rec_at as *mut ThreadRec) };
    *rec = ThreadRec::EMPTY;
    rec.frame = *frame;
    rec.tls = setup.tls;
    rec.pc = pc;
    rec.sp = if setup.stack == 0 {
        parent_sp
    } else {
        setup.stack
    };
    rec.x8 = x8;
    rec.clear_tid = setup.clear_tid;
    rec.region_base = region_base;
    rec.region_len = region_len;
    rec.parent_tid_at = setup.parent_tid_at;
    rec.child_tid_at = setup.child_tid_at;
    rec.sigmask = parent.sigmask;
    rec.child = setup.child;
    // A clone starts with nothing pending and no alternate stack (Linux, CLONE_VM).

    let (status, koid) = thread_spawn(persona_thread_entry as *const () as u64, rec_at, rec_at);
    if status != 0 {
        let _ = vmar_unmap(region_base, region_len);
        return Err(-errno::EAGAIN);
    }
    // When the koid is no tid, the child ends the process itself (child_failed).
    thread::tid_of_koid(koid)
        .map(|tid| tid as u32)
        .ok_or(-errno::EAGAIN)
}

/// Linux `exit`: this thread only. CLONE_CHILD_CLEARTID's word is zeroed and woken (musl's
/// `pthread_join` and its thread-list lock wait on it), then the thread ends and its trap stack
/// goes with it. The last thread ending this way ends the process with its status.
pub fn exit_thread(rec: &ThreadRec, status: u64) -> ! {
    if rec.child != 0 {
        // j924: a vfork child's `_exit` ends that child, not the program it borrowed.
        crate::processes::child_exit(rec, status);
    }
    if rec.clear_tid != 0 {
        let word = unsafe { &*(rec.clear_tid as *const AtomicU32) };
        word.store(0, Ordering::Release);
        let _ = futex_wake(word.as_ptr(), 1);
    }
    if thread_self().1 <= 1 {
        process_exit(status & 0xff);
    }
    // j919: no sender may find this record once its trap stack is gone.
    lock();
    crate::signals::unregister(rec);
    unlock();
    thread_exit(rec.region_base, rec.region_len)
}

/// j924: end the calling thread (lock not held) and free its trap stack: a vfork child after
/// its `execve` started the new process, or after its `_exit`.
pub fn retire(rec: &ThreadRec) -> ! {
    lock();
    crate::signals::unregister(rec);
    unlock();
    thread_exit(rec.region_base, rec.region_len)
}

/// j924: in a process an `execve` started, before its program: one thread, no lock held, no
/// registry, the pid the parent gave it (processes.rs).
pub fn reset_for_spawn(pid: u32) {
    LOCK.store(0, Ordering::Relaxed);
    PID.store(pid, Ordering::Relaxed);
    let rec = unsafe { &mut *core::ptr::addr_of_mut!(MAIN_TRAP.rec) };
    *rec = ThreadRec::EMPTY;
}

pub fn gettid(rec: &ThreadRec) -> i64 {
    rec.tid as i64
}

pub fn getpid() -> i64 {
    PID.load(Ordering::Relaxed) as i64
}

pub fn set_tid_address(rec: &mut ThreadRec, at: u64) -> i64 {
    rec.clear_tid = at;
    rec.tid as i64
}

pub fn set_robust_list(rec: &mut ThreadRec, head: u64, len: u64) -> i64 {
    // sizeof(struct robust_list_head) on aarch64.
    if len != 24 {
        return -errno::EINVAL;
    }
    rec.robust_head = head;
    0
}

pub fn sched_yield() -> i64 {
    let _ = thread_yield();
    0
}

/// Linux `futex` on the kernel futex (one domain per process, keyed by address).
pub fn futex(rec: &ThreadRec, uaddr: u64, op: u64, val: u64, timeout: u64, val3: u64) -> i64 {
    if uaddr == 0 || uaddr % 4 != 0 {
        return -errno::EINVAL;
    }
    let word = unsafe { &*(uaddr as *const AtomicU32) };
    match thread::futex_call(op, val, timeout, val3) {
        Err(FutexRefusal::Invalid) => -errno::EINVAL,
        Err(FutexRefusal::Unsupported) => {
            log(format_args!(
                "persona-linux: futex op {:#x} unsupported (-ENOSYS)\n",
                op
            ));
            -errno::ENOSYS
        }
        Ok(FutexCall::Wake { count }) => {
            if count == 0 {
                0
            } else {
                futex_wake(word.as_ptr(), count) as i64
            }
        }
        Ok(FutexCall::Requeue {
            wake,
            requeue,
            compare,
        }) => {
            if compare.is_some_and(|value| word.load(Ordering::Acquire) != value) {
                return -errno::EAGAIN;
            }
            let count = wake.saturating_add(requeue);
            if count == 0 {
                0
            } else {
                futex_wake(word.as_ptr(), count) as i64
            }
        }
        Ok(FutexCall::Wait {
            expected,
            deadline: Deadline::Never,
        }) => {
            // j919: interruptible. The word is recorded first, so a sender that sets a pending
            // bit after the check below wakes this word; one that set it before is seen here.
            rec.waiting_on.store(uaddr, Ordering::Release);
            let status = if crate::signals::deliverable(rec) != 0 {
                rec.waiting_on.store(0, Ordering::Release);
                return -crate::signals::ERESTARTSYS;
            } else {
                futex_wait(word.as_ptr(), expected)
            };
            rec.waiting_on.store(0, Ordering::Release);
            if crate::signals::deliverable(rec) != 0 {
                -crate::signals::ERESTARTSYS
            } else if status == kumo_abi::Errno::Ok.status() {
                0
            } else if status == kumo_abi::Errno::ShouldWait.status() {
                -errno::EAGAIN
            } else {
                -errno::EINVAL
            }
        }
        Ok(FutexCall::Wait { expected, deadline }) => {
            let Some(ns) = read_timespec(timeout) else {
                return -errno::EINVAL;
            };
            let until = match deadline {
                Deadline::Relative => clock_get().saturating_add(ns),
                _ => ns,
            };
            timed_wait(rec, word, expected, until)
        }
    }
}

/// How long a timed futex wait sleeps between looks at its word.
const TIMED_WAIT_STEP_NS: u64 = 1_000_000;

/// A futex wait with a deadline. The kernel futex has none, so this one sleeps on a kernel
/// timer (one step at a time; the kernel rounds a step up to its scheduler tick) until the word
/// changes or the deadline passes. It never spins: a child that yield-polled here would starve
/// Sora, which children outrank (first written that way, the POST probes running beside the
/// probe failed). A wake that does not change the word is seen only at the deadline (Linux
/// callers re-check their condition), and one that does is seen up to a tick late. The real fix
/// is a deadline on the kernel's FutexWait (a later slice).
fn timed_wait(rec: &ThreadRec, word: &AtomicU32, expected: u32, until: u64) -> i64 {
    if word.load(Ordering::Acquire) != expected {
        return -errno::EAGAIN;
    }
    loop {
        let now = clock_get();
        if now >= until {
            return -errno::ETIMEDOUT;
        }
        // j919: each step also ends for a deliverable signal (the call then restarts or EINTRs).
        match crate::signals::sleep_or_alert(rec, (until - now).min(TIMED_WAIT_STEP_NS)) {
            Some(true) => return -crate::signals::ERESTARTSYS,
            Some(false) => {}
            // No timer to sleep on (out of handles): time out early rather than spin or hang.
            None => return -errno::ETIMEDOUT,
        }
        if word.load(Ordering::Acquire) != expected {
            return 0;
        }
    }
}

/// A `struct timespec` the program passed, in nanoseconds.
fn read_timespec(at: u64) -> Option<u64> {
    if at == 0 || at % 8 != 0 {
        return None;
    }
    let (sec, nsec) = unsafe { (*(at as *const i64), *((at + 8) as *const i64)) };
    thread::timespec_ns(sec, nsec)
}

fn write_timespec(at: u64, ns: u64) {
    unsafe {
        *(at as *mut i64) = (ns / 1_000_000_000) as i64;
        *((at + 8) as *mut i64) = (ns % 1_000_000_000) as i64;
    }
}

const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_MONOTONIC_RAW: u64 = 4;
const CLOCK_REALTIME_COARSE: u64 = 5;
const CLOCK_MONOTONIC_COARSE: u64 = 6;
const CLOCK_BOOTTIME: u64 = 7;
const TIMER_ABSTIME: u64 = 1;

fn wall_clock(clock: u64) -> bool {
    matches!(
        clock,
        CLOCK_REALTIME
            | CLOCK_MONOTONIC
            | CLOCK_MONOTONIC_RAW
            | CLOCK_REALTIME_COARSE
            | CLOCK_MONOTONIC_COARSE
            | CLOCK_BOOTTIME
    )
}

/// Linux `clock_gettime`: every wall clock reads the kernel's monotonic nanoseconds (KUMO keeps
/// no calendar time yet, so CLOCK_REALTIME counts from boot). CPU-time clocks are EINVAL.
pub fn clock_gettime(clock: u64, at: u64) -> i64 {
    if !wall_clock(clock) {
        return -errno::EINVAL;
    }
    if at == 0 {
        return -errno::EFAULT;
    }
    write_timespec(at, clock_get());
    0
}

/// Linux `nanosleep` and `clock_nanosleep` (relative or TIMER_ABSTIME). j919: a deliverable
/// signal ends the sleep with EINTR and the time left in `remain`, as on Linux.
pub fn clock_nanosleep(rec: &ThreadRec, clock: u64, flags: u64, request: u64, remain: u64) -> i64 {
    if !wall_clock(clock) || flags & !TIMER_ABSTIME != 0 {
        return -errno::EINVAL;
    }
    let Some(ns) = read_timespec(request) else {
        return -errno::EINVAL;
    };
    let delay = if flags & TIMER_ABSTIME != 0 {
        ns.saturating_sub(clock_get())
    } else {
        ns
    };
    let start = clock_get();
    let interrupted = if delay == 0 {
        let _ = thread_yield();
        false
    } else {
        match crate::signals::sleep_or_alert(rec, delay) {
            Some(interrupted) => interrupted,
            None => {
                // No timer: the old blocking sleep, uninterruptible.
                let _ = sleep_ns(delay);
                false
            }
        }
    };
    if interrupted {
        if remain != 0 && flags & TIMER_ABSTIME == 0 {
            write_timespec(remain, delay.saturating_sub(clock_get() - start));
        }
        return -errno::EINTR;
    }
    if remain != 0 && flags & TIMER_ABSTIME == 0 {
        write_timespec(remain, 0);
    }
    0
}

// ---- j917 (SH7-d3d): what rustc's startup asks of the process (signals: signals.rs) -----

const PR_SET_NAME: u64 = 15;
const PR_GET_NAME: u64 = 16;

/// Linux `prctl`: a thread naming itself (std's `thread::Builder::name`) is accepted; reading a
/// name back answers "linux"; nothing else is served.
pub fn prctl(option: u64, arg: u64) -> i64 {
    match option {
        PR_SET_NAME => 0,
        PR_GET_NAME if arg != 0 => {
            unsafe { core::ptr::copy_nonoverlapping(b"linux\0".as_ptr(), arg as *mut u8, 6) };
            0
        }
        _ => -errno::EINVAL,
    }
}

/// Bits of state for [`getrandom`]: splitmix64 over the kernel clock.
static RANDOM: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Linux `getrandom`. KUMO has no entropy source a user program can read yet, so this is a
/// splitmix64 stream reseeded from the monotonic clock on every call: unpredictable enough for
/// HashMap keys (what Rust's runtime asks for), **not** for keys or nonces. Never blocks.
pub fn getrandom(buf: u64, len: u64, flags: u64) -> i64 {
    // GRND_NONBLOCK 1, GRND_RANDOM 2, GRND_INSECURE 4.
    if flags & !7 != 0 {
        return -errno::EINVAL;
    }
    let len = len.min(i32::MAX as u64);
    let mut done = 0u64;
    while done < len {
        let mut z = RANDOM
            .fetch_add(clock_get() | 1, Ordering::Relaxed)
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let take = (len - done).min(8);
        unsafe {
            core::ptr::copy_nonoverlapping(
                z.to_le_bytes().as_ptr(),
                (buf + done) as *mut u8,
                take as usize,
            )
        };
        done += take;
    }
    len as i64
}

const MEMBARRIER_CMD_QUERY: u64 = 0;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED: u64 = 8;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: u64 = 16;

/// Linux `membarrier`, private-expedited only (musl registers at its first `pthread_create`
/// and uses it when a dlopen installs TLS). **Correct only while KUMO runs one CPU** (SMP is
/// PLAN/014): every other thread of the process is then off-CPU and has passed a context
/// switch, so a full barrier here is the barrier on all of them.
pub fn membarrier(cmd: u64, flags: u64) -> i64 {
    if flags != 0 {
        return -errno::EINVAL;
    }
    match cmd {
        MEMBARRIER_CMD_QUERY => {
            (MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED) as i64
        }
        MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED => {
            unsafe { core::arch::asm!("dsb ish", options(nostack)) };
            0
        }
        _ => -errno::EINVAL,
    }
}
