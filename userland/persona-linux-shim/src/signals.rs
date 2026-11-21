//j919
//j924
//! SH7-e2 (PLAN/022, the SH7-e cut, S3): Linux signals, delivered.
//!
//! **Sending** (`tkill`, `tgkill`, `kill` of this process) sets a bit in the target thread's
//! [`ThreadRec::pending`] and wakes the target if it is parked: a thread waiting in the shim (a
//! pipe or stdin read, a sleep, a timed futex wait) waits on a port that also watches its own
//! *alert channel*, which the sender writes one byte to; a thread in an untimed futex wait is
//! woken on the word it recorded in [`ThreadRec::waiting_on`]. A signal whose action is to ignore
//! it is dropped at send time, as Linux does.
//!
//! **Delivery** happens where Linux delivers: on the way back to the program from a system call
//! (`on_return`, at the end of every redirected call). A deliverable signal (pending and not
//! blocked) with a handler gets a real `rt_sigframe` on the program's stack (or its sigaltstack,
//! for SA_ONSTACK): siginfo, a ucontext holding every general register, SP, PC, the signal mask
//! and the FP/SIMD state (`fpsimd_context`), and the frame record; then the open call is moved
//! (`PersonaContext` MOVE) to enter the handler with `x0 = sig`, `x1 = &info`, `x2 = &uc`,
//! `x29` = the frame record and `x30 = sa_restorer`. The handler's return runs musl's restorer,
//! whose `rt_sigreturn` puts all of it back. A call a signal interrupted answers EINTR, or restarts
//! under SA_RESTART (the frame resumes at its `svc` with its first argument).
//!
//! **Not delivered** in this slice: a thread computing in the program without making system
//! calls sees its signal at its next call (the kernel has no way to interrupt it into the shim
//! yet); synchronous faults stay SH6's contained exit; the default action "stop" is ignored. The
//! layout is Linux arm64's (checked against musl 1.2.5's headers in the pinned Alpine image:
//! siginfo 128, ucontext 4560, uc_mcontext +176, regs +8, sp +256, pc +264, __reserved +288).
//! — TEAL

use core::sync::atomic::{AtomicU64, Ordering};

use kumo_abi::Handle;
use kumo_rt::{
    channel_create_pair, channel_try_read_with_handle, channel_write, futex_wake, handle_close,
    port_bind, port_create, port_wait, process_exit, thread_self, thread_yield, timer_create,
};

use crate::threads::{self, ThreadRec};
use crate::{errno, log, TrapFrame};

/// The shim's internal "interrupted, restart unless a handler without SA_RESTART runs" result
/// (Linux's ERESTARTSYS). Never reaches the program.
pub const ERESTARTSYS: i64 = 512;

const SIGKILL: u64 = 9;
const SIGSTOP: u64 = 19;
const UNBLOCKABLE: u64 = (1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1));

const SA_SIGINFO: u64 = 0x4;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_RESTART: u64 = 0x1000_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;
const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

pub const SS_ONSTACK: u64 = 1;
pub const SS_DISABLE: u64 = 2;
const SIG_BLOCK: u64 = 0;
const SIG_UNBLOCK: u64 = 1;
const SIG_SETMASK: u64 = 2;

const SI_USER: i32 = 0;
const SI_TKILL: i32 = -6;

// The arm64 rt_sigframe, as musl 1.2.5's headers lay it out.
const SIGINFO_SIZE: u64 = 128;
const UCONTEXT_SIZE: u64 = 4560;
const UC_STACK: u64 = 16;
const UC_SIGMASK: u64 = 40;
const UC_MCONTEXT: u64 = 176;
const MC_REGS: u64 = 8;
const MC_SP: u64 = 256;
const MC_PC: u64 = 264;
const MC_PSTATE: u64 = 272;
const MC_RESERVED: u64 = 288;
const FPSIMD_MAGIC: u32 = 0x4650_8001;
const FPSIMD_SIZE: u32 = 528;
/// siginfo + ucontext, then the 16-byte frame record {fp, lr}.
const FRAME_SIZE: u64 = SIGINFO_SIZE + UCONTEXT_SIZE + 16;

fn bit(sig: u64) -> u64 {
    1 << (sig - 1)
}

/// The process's dispositions, as `rt_sigaction` set them (`struct k_sigaction` on arm64:
/// handler, flags, restorer, mask). Guarded by the shim lock.
static mut SIGACTIONS: [[u64; 4]; 65] = [[0; 4]; 65];

/// j924 (SH7-f): each vfork child's own dispositions (slot 0 unused). musl's `posix_spawn` child
/// resets every handled signal to SIG_DFL before `execve`; on Linux that child has its own
/// table (no CLONE_SIGHAND), so the program's handlers must not change. Lock-guarded.
static mut CHILD_ACTIONS: [[[u64; 4]; 65]; crate::files::MAX_BINDINGS] =
    [[[0; 4]; 65]; crate::files::MAX_BINDINGS];
/// The dispositions a process an `execve` starts begins with, copied with the snapshot.
static mut EXEC_ACTIONS: [[u64; 4]; 65] = [[0; 4]; 65];

/// A vfork child in `slot` starts with the program's dispositions.
pub fn copy_actions_to(slot: usize) {
    unsafe {
        (*core::ptr::addr_of_mut!(CHILD_ACTIONS))[slot] = *core::ptr::addr_of!(SIGACTIONS);
    }
}

/// What `execve` keeps of `slot`'s dispositions (0: the program's own): ignored stays ignored,
/// a handler becomes SIG_DFL (its code is gone). Staged for the snapshot. Lock held.
pub fn prepare_exec(slot: usize) {
    let table = if slot == 0 {
        unsafe { *core::ptr::addr_of!(SIGACTIONS) }
    } else {
        unsafe { (*core::ptr::addr_of!(CHILD_ACTIONS))[slot] }
    };
    let staged = unsafe { &mut *core::ptr::addr_of_mut!(EXEC_ACTIONS) };
    for (sig, action) in table.iter().enumerate() {
        staged[sig] = if action[0] == SIG_IGN {
            [SIG_IGN, 0, 0, 0]
        } else {
            [SIG_DFL, 0, 0, 0]
        };
    }
}

/// In the process an `execve` started: the staged dispositions and an empty thread registry.
pub fn adopt_exec() {
    unsafe {
        *core::ptr::addr_of_mut!(SIGACTIONS) = *core::ptr::addr_of!(EXEC_ACTIONS);
    }
    for slot in &THREADS {
        slot.store(0, Ordering::Relaxed);
    }
}
fn action(sig: u64) -> [u64; 4] {
    unsafe { (*core::ptr::addr_of!(SIGACTIONS))[sig as usize] }
}

/// Whether the default action of `sig` leaves the process alone (SIGCHLD, SIGCONT, SIGURG,
/// SIGWINCH; and the stop signals, which this slice does not stop on).
fn default_ignores(sig: u64) -> bool {
    matches!(sig, 17 | 18 | 23 | 28 | 19 | 20 | 21 | 22)
}

/// Whether `sig` would be discarded on arrival: ignored explicitly, or by default.
fn ignored(sig: u64) -> bool {
    let handler = action(sig)[0];
    handler == SIG_IGN || (handler == SIG_DFL && default_ignores(sig))
}

// ---- the thread registry -------------------------------------------------------------------

const MAX_THREADS: usize = 128;
/// Every live Linux thread's record, for `tkill` and `kill`. Changed only under the shim lock.
static THREADS: [AtomicU64; MAX_THREADS] = [const { AtomicU64::new(0) }; MAX_THREADS];

/// Register a thread (with the shim lock held): its record joins the registry and gets the
/// alert channel senders wake it through.
pub fn register(rec: &mut ThreadRec) -> bool {
    let (recv, send) = channel_create_pair();
    if recv == u64::MAX || send == u64::MAX {
        return false;
    }
    rec.alert_recv = recv as u32;
    rec.alert_send = send as u32;
    let at = rec as *mut ThreadRec as u64;
    THREADS.iter().any(|slot| {
        slot.compare_exchange(0, at, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    })
}

/// Leave the registry (with the shim lock held) and close the alert channel.
pub fn unregister(rec: &ThreadRec) {
    let at = rec as *const ThreadRec as u64;
    for slot in &THREADS {
        if slot
            .compare_exchange(at, 0, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
    let _ = handle_close(Handle(rec.alert_recv));
    let _ = handle_close(Handle(rec.alert_send));
}

fn find(tid: u64) -> Option<&'static ThreadRec> {
    THREADS.iter().find_map(|slot| {
        let at = slot.load(Ordering::Acquire);
        let rec = (at != 0).then(|| unsafe { &*(at as *const ThreadRec) })?;
        (rec.tid == tid).then_some(rec)
    })
}

/// The calling thread's record, for code that was not handed it (a parked read).
pub fn current() -> Option<&'static ThreadRec> {
    find(thread_self().0)
}

// ---- sending -------------------------------------------------------------------------------

/// Make `sig` pending on `rec` and wake it if it is parked. Called with the shim lock held.
fn send(rec: &ThreadRec, sig: u64, user: bool, from_self: bool) {
    if rec.sigmask & bit(sig) == 0 && ignored(sig) {
        return;
    }
    if user {
        rec.pending_user.fetch_or(bit(sig), Ordering::AcqRel);
    } else {
        rec.pending_user.fetch_and(!bit(sig), Ordering::AcqRel);
    }
    rec.pending.fetch_or(bit(sig), Ordering::AcqRel);
    if from_self || rec.sigmask & bit(sig) != 0 {
        return;
    }
    let _ = channel_write(Handle(rec.alert_send), [1u8].as_ptr(), 1);
    // A thread in an untimed futex wait: wake its word until it has left the wait (on one CPU,
    // a yield lets it reach the kernel's sleep, so the wake cannot fall in the gap before it).
    for _ in 0..64 {
        let word = rec.waiting_on.load(Ordering::Acquire);
        if word == 0 {
            break;
        }
        let _ = futex_wake(word as *const u32, u32::MAX);
        let _ = thread_yield();
    }
}

fn valid(sig: u64) -> bool {
    sig <= 64
}

/// Linux `tgkill` / `tkill`: a signal to one thread of this process (`tgid` 0 skips its check).
pub fn tgkill(tgid: u64, tid: u64, sig: u64, caller: &ThreadRec) -> i64 {
    if !valid(sig) || (tid as i64) <= 0 {
        return -errno::EINVAL;
    }
    if tgid != 0 && tgid != threads::getpid() as u64 {
        return -errno::ESRCH;
    }
    let Some(rec) = find(tid) else {
        return -errno::ESRCH;
    };
    if sig != 0 {
        send(rec, sig, false, core::ptr::eq(rec, caller));
    }
    0
}

/// Linux `kill`, for this process only (`pid` = the pid, 0 or -pid): the signal goes to a thread
/// that does not block it — the caller if it can take it.
pub fn kill(pid: i64, sig: u64, caller: &ThreadRec) -> i64 {
    if !valid(sig) {
        return -errno::EINVAL;
    }
    let me = threads::getpid();
    if pid != me && pid != 0 && pid != -me && pid != -1 {
        return -errno::ESRCH;
    }
    if sig == 0 {
        return 0;
    }
    let target = if caller.sigmask & bit(sig) == 0 {
        caller
    } else {
        THREADS
            .iter()
            .find_map(|slot| {
                let at = slot.load(Ordering::Acquire);
                let rec = (at != 0).then(|| unsafe { &*(at as *const ThreadRec) })?;
                (rec.sigmask & bit(sig) == 0).then_some(rec)
            })
            .unwrap_or(caller)
    };
    send(target, sig, true, core::ptr::eq(target, caller));
    0
}

// ---- waiting -------------------------------------------------------------------------------

/// The signals `rec` would take now.
pub fn deliverable(rec: &ThreadRec) -> u64 {
    rec.pending.load(Ordering::Acquire) & !rec.sigmask
}

/// Read the alert channel empty: the bits in `pending` are the truth; an alert only wakes.
fn drain_alerts(rec: &ThreadRec) {
    let mut byte = [0u8; 16];
    while channel_try_read_with_handle(Handle(rec.alert_recv), byte.as_mut_ptr(), byte.len())
        .is_some()
    {}
}

/// Wait until `object` (a channel or a timer) is signalled or a deliverable signal arrives.
/// `Some(true)` when a signal ended the wait, `Some(false)` when `object` did, `None` when the
/// wait could not be set up (out of handles) — the caller must not spin on that.
pub fn wait_or_alert(rec: &ThreadRec, object: Handle) -> Option<bool> {
    drain_alerts(rec);
    if deliverable(rec) != 0 {
        return Some(true);
    }
    let port = port_create();
    if port == u64::MAX {
        return None;
    }
    let port = Handle(port as u32);
    let bound = port_bind(port, object) == 0 && port_bind(port, Handle(rec.alert_recv)) == 0;
    if bound {
        let _ = port_wait(port);
    }
    let _ = handle_close(port);
    drain_alerts(rec);
    bound.then(|| deliverable(rec) != 0)
}

/// Sleep `ns`, or less if a deliverable signal arrives: as [`wait_or_alert`].
pub fn sleep_or_alert(rec: &ThreadRec, ns: u64) -> Option<bool> {
    let timer = timer_create(ns.max(1));
    if timer == u64::MAX {
        return None;
    }
    let timer = Handle(timer as u32);
    let interrupted = wait_or_alert(rec, timer);
    let _ = handle_close(timer);
    interrupted
}

// ---- the calls -----------------------------------------------------------------------------

/// Linux `rt_sigaction`. Called with the shim lock held.
pub fn rt_sigaction(sig: u64, act: u64, old: u64, size: u64) -> i64 {
    if size != 8 || sig == 0 || sig > 64 {
        return -errno::EINVAL;
    }
    if act != 0 && (sig == SIGKILL || sig == SIGSTOP) {
        return -errno::EINVAL;
    }
    let child = crate::files::active();
    if child != 0 {
        // j924: a vfork child's own table; nothing is delivered through it.
        let table = unsafe { &mut (*core::ptr::addr_of_mut!(CHILD_ACTIONS))[child] };
        let previous = table[sig as usize];
        if act != 0 {
            table[sig as usize] = unsafe { core::ptr::read_unaligned(act as *const [u64; 4]) };
        }
        if old != 0 {
            unsafe { core::ptr::write_unaligned(old as *mut [u64; 4], previous) };
        }
        return 0;
    }
    let table = unsafe { &mut *core::ptr::addr_of_mut!(SIGACTIONS) };
    let previous = table[sig as usize];
    if act != 0 {
        table[sig as usize] = unsafe { core::ptr::read_unaligned(act as *const [u64; 4]) };
        // A signal now ignored is discarded from every thread, as Linux does.
        if ignored(sig) {
            for slot in &THREADS {
                let at = slot.load(Ordering::Acquire);
                if at != 0 {
                    let rec = unsafe { &*(at as *const ThreadRec) };
                    rec.pending.fetch_and(!bit(sig), Ordering::AcqRel);
                }
            }
        }
    }
    if old != 0 {
        unsafe { core::ptr::write_unaligned(old as *mut [u64; 4], previous) };
    }
    0
}

/// Linux `rt_sigprocmask` on this thread's own mask; a signal it unblocks is delivered as the
/// call returns.
pub fn rt_sigprocmask(rec: &mut ThreadRec, how: u64, set: u64, old: u64, size: u64) -> i64 {
    if size != 8 {
        return -errno::EINVAL;
    }
    let previous = rec.sigmask;
    if set != 0 {
        let wanted = unsafe { *(set as *const u64) };
        rec.sigmask = match how {
            SIG_BLOCK => previous | wanted,
            SIG_UNBLOCK => previous & !wanted,
            SIG_SETMASK => wanted,
            _ => return -errno::EINVAL,
        } & !UNBLOCKABLE;
    }
    if old != 0 {
        unsafe { *(old as *mut u64) = previous };
    }
    0
}

/// Linux `rt_sigpending`: what is pending on this thread (blocked, or not yet delivered).
pub fn rt_sigpending(rec: &ThreadRec, set: u64, size: u64) -> i64 {
    if size != 8 {
        return -errno::EINVAL;
    }
    unsafe { *(set as *mut u64) = rec.pending.load(Ordering::Acquire) };
    0
}

/// Linux `sigaltstack` on this thread's record.
pub fn sigaltstack(rec: &mut ThreadRec, new: u64, old: u64) -> i64 {
    // stack_t { void *ss_sp; int ss_flags; size_t ss_size; } = 24 bytes on aarch64.
    let previous = (rec.alt_sp, rec.alt_flags, rec.alt_size);
    if new != 0 {
        let (sp, flags, size) = unsafe {
            (
                *(new as *const u64),
                *((new + 8) as *const u32) as u64,
                *((new + 16) as *const u64),
            )
        };
        if flags & !(SS_DISABLE | SS_ONSTACK) != 0 {
            return -errno::EINVAL;
        }
        if flags & SS_DISABLE != 0 {
            (rec.alt_sp, rec.alt_flags, rec.alt_size) = (0, SS_DISABLE, 0);
        } else {
            (rec.alt_sp, rec.alt_flags, rec.alt_size) = (sp, 0, size);
        }
    }
    if old != 0 {
        unsafe {
            *(old as *mut u64) = previous.0;
            *((old + 8) as *mut u32) = previous.1 as u32;
            *((old + 12) as *mut u32) = 0;
            *((old + 16) as *mut u64) = previous.2;
        }
    }
    0
}

fn on_altstack(rec: &ThreadRec, sp: u64) -> bool {
    rec.alt_flags & SS_DISABLE == 0 && sp > rec.alt_sp && sp <= rec.alt_sp + rec.alt_size
}

/// Linux `rt_sigreturn`: put back the context the frame at the program's SP holds (registers,
/// SP, PC, FP/SIMD, the signal mask, the altstack) and resume it. Returns its x0.
pub fn rt_sigreturn(frame: &mut TrapFrame, rec: &mut ThreadRec) -> i64 {
    let (_, _, sp, _) = threads::persona_context(kumo_abi::sys::PERSONA_CONTEXT_GET, 0, 0, 0);
    if sp % 16 != 0 {
        log(format_args!(
            "persona-linux: rt_sigreturn on a misaligned frame\n"
        ));
        process_exit(0x100 | 11);
    }
    let uc = sp + SIGINFO_SIZE;
    let mc = uc + UC_MCONTEXT;
    let read = |at: u64| unsafe { core::ptr::read_volatile(at as *const u64) };
    for (i, x) in frame.x.iter_mut().enumerate() {
        *x = read(mc + MC_REGS + 8 * i as u64);
    }
    let reserved = mc + MC_RESERVED;
    let magic = unsafe { core::ptr::read_volatile(reserved as *const u32) };
    if magic == FPSIMD_MAGIC {
        let fpsr = unsafe { core::ptr::read_volatile((reserved + 8) as *const u32) };
        let fpcr = unsafe { core::ptr::read_volatile((reserved + 12) as *const u32) };
        frame.fpsr = fpsr as u64;
        frame.fpcr = fpcr as u64;
        for (i, q) in frame.q.iter_mut().enumerate() {
            *q = unsafe {
                core::ptr::read_unaligned((reserved + 16 + 16 * i as u64) as *const u128)
            };
        }
    }
    rec.sigmask = read(uc + UC_SIGMASK) & !UNBLOCKABLE;
    let (alt_sp, alt_flags, alt_size) = (
        read(uc + UC_STACK),
        read(uc + UC_STACK + 8) & 0xffff_ffff,
        read(uc + UC_STACK + 16),
    );
    if alt_flags & SS_DISABLE != 0 {
        (rec.alt_sp, rec.alt_flags, rec.alt_size) = (0, SS_DISABLE, 0);
    } else {
        (rec.alt_sp, rec.alt_flags, rec.alt_size) = (alt_sp, 0, alt_size);
    }
    let (pc, new_sp) = (read(mc + MC_PC), read(mc + MC_SP));
    threads::persona_context(kumo_abi::sys::PERSONA_CONTEXT_MOVE, pc, new_sp, frame.x[8]);
    frame.x[0] as i64
}

/// On the way back to the program: deliver one deliverable signal, and settle a call a signal
/// interrupted (`result == -ERESTARTSYS`). `orig_x0` is the call's first argument.
pub fn on_return(
    frame: &mut TrapFrame,
    rec: &mut ThreadRec,
    orig_x0: u64,
    may_restart: bool,
    result: &mut i64,
) {
    let interrupted = may_restart && *result == -ERESTARTSYS;
    loop {
        let ready = deliverable(rec);
        if ready == 0 {
            break;
        }
        let sig = ready.trailing_zeros() as u64 + 1;
        rec.pending.fetch_and(!bit(sig), Ordering::AcqRel);
        let user = rec.pending_user.fetch_and(!bit(sig), Ordering::AcqRel) & bit(sig) != 0;
        threads::lock();
        let act = action(sig);
        if act[1] & SA_RESETHAND != 0 && act[0] > SIG_IGN {
            unsafe { (*core::ptr::addr_of_mut!(SIGACTIONS))[sig as usize][0] = SIG_DFL };
        }
        threads::unlock();
        match act[0] {
            SIG_IGN => continue,
            SIG_DFL if default_ignores(sig) => continue,
            SIG_DFL => {
                // Terminate: the KUMO shape of "killed by signal" (SH6, 0x100 | sig).
                process_exit(0x100 | sig);
            }
            handler => {
                let restart = interrupted && act[1] & SA_RESTART != 0;
                let resume_x0 = if !interrupted {
                    *result as u64
                } else if restart {
                    orig_x0
                } else {
                    -errno::EINTR as u64
                };
                deliver(frame, rec, sig, user, handler, &act, restart, resume_x0);
                *result = sig as i64;
                return;
            }
        }
    }
    if interrupted {
        // Nothing to run: the call restarts, as one interrupted by an ignored signal does.
        let (_, pc, sp, x8) = threads::persona_context(kumo_abi::sys::PERSONA_CONTEXT_GET, 0, 0, 0);
        threads::persona_context(kumo_abi::sys::PERSONA_CONTEXT_MOVE, pc - 4, sp, x8);
        *result = orig_x0 as i64;
    }
}

/// Build the rt_sigframe and move the open call into the handler.
#[allow(clippy::too_many_arguments)]
fn deliver(
    frame: &mut TrapFrame,
    rec: &mut ThreadRec,
    sig: u64,
    user: bool,
    handler: u64,
    act: &[u64; 4],
    restart: bool,
    resume_x0: u64,
) {
    let (_, pc, sp, x8) = threads::persona_context(kumo_abi::sys::PERSONA_CONTEXT_GET, 0, 0, 0);
    let resume_pc = if restart { pc - 4 } else { pc };
    let flags = act[1];
    let base =
        if flags & SA_ONSTACK != 0 && rec.alt_flags & SS_DISABLE == 0 && !on_altstack(rec, sp) {
            rec.alt_sp + rec.alt_size
        } else {
            sp
        };
    let at = (base - FRAME_SIZE) & !15;
    let write = |offset: u64, value: u64| unsafe {
        core::ptr::write_volatile((at + offset) as *mut u64, value)
    };
    // Clear the frame: unused siginfo and ucontext fields read as zero.
    unsafe { core::ptr::write_bytes(at as *mut u8, 0, FRAME_SIZE as usize) };
    // siginfo: signo, errno, code, then (for SI_USER/SI_TKILL) pid and uid.
    let code = if user { SI_USER } else { SI_TKILL };
    write(0, sig | (0 << 32));
    write(8, code as u32 as u64);
    write(16, threads::getpid() as u32 as u64);
    // ucontext
    let uc = SIGINFO_SIZE;
    write(uc + UC_STACK, rec.alt_sp);
    write(
        uc + UC_STACK + 8,
        if on_altstack(rec, sp) {
            SS_ONSTACK
        } else {
            rec.alt_flags & SS_DISABLE
        },
    );
    write(uc + UC_STACK + 16, rec.alt_size);
    write(uc + UC_SIGMASK, rec.sigmask);
    let mc = uc + UC_MCONTEXT;
    for (i, &x) in frame.x.iter().enumerate() {
        write(mc + MC_REGS + 8 * i as u64, x);
    }
    write(mc + MC_REGS, resume_x0);
    write(mc + MC_REGS + 8 * 8, x8);
    write(mc + MC_SP, sp);
    write(mc + MC_PC, resume_pc);
    write(mc + MC_PSTATE, 0);
    let reserved = at + mc + MC_RESERVED;
    unsafe {
        core::ptr::write_volatile(reserved as *mut u32, FPSIMD_MAGIC);
        core::ptr::write_volatile((reserved + 4) as *mut u32, FPSIMD_SIZE);
        core::ptr::write_volatile((reserved + 8) as *mut u32, frame.fpsr as u32);
        core::ptr::write_volatile((reserved + 12) as *mut u32, frame.fpcr as u32);
        for (i, q) in frame.q.iter().enumerate() {
            core::ptr::write_unaligned((reserved + 16 + 16 * i as u64) as *mut u128, *q);
        }
        // The terminating null context header follows (the frame is already zero there).
    }
    // The frame record: the interrupted frame pointer and PC, for unwinders.
    let record = SIGINFO_SIZE + UCONTEXT_SIZE;
    write(record, frame.x[29]);
    write(record + 8, resume_pc);

    // The handler runs with its own signal (unless SA_NODEFER) and sa_mask blocked.
    let mut mask = rec.sigmask | act[3];
    if flags & SA_NODEFER == 0 {
        mask |= bit(sig);
    }
    rec.sigmask = mask & !UNBLOCKABLE;

    let restorer = if flags & SA_RESTORER != 0 { act[2] } else { 0 };
    if restorer == 0 {
        log(format_args!(
            "persona-linux: signal {} has no SA_RESTORER; its return will fault\n",
            sig
        ));
    }
    frame.x[1] = if flags & SA_SIGINFO != 0 { at } else { 0 };
    frame.x[2] = if flags & SA_SIGINFO != 0 { at + uc } else { 0 };
    frame.x[29] = at + record;
    frame.x[30] = restorer;
    threads::persona_context(kumo_abi::sys::PERSONA_CONTEXT_MOVE, handler, at, x8);
}
