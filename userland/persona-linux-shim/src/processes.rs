//j924
//j925
//! j924 (SH7-f): the Linux processes a persona program starts, and waiting for them.
//!
//! musl's `posix_spawn` (what Rust's `Command` and rustc's linker launch use) is
//! `clone(CLONE_VM | CLONE_VFORK | SIGCHLD)`: the child runs on its own stack in the caller's
//! address space, rearranges its descriptors (`dup3`, `close`), resets its signal handlers and
//! calls `execve`, while the caller waits. Here the child is a shim thread of this process
//! with a descriptor table and dispositions of its own (files.rs, signals.rs slots). Its
//! `execve` starts a new KUMO process: a fresh persona-window VMAR holding a copy of the shim's
//! image alone (`VmarSnapshot` limited to it), its own session on the same volume, and the
//! carried descriptors' channels; the copy's `persona_spawn_entry` then loads the program as
//! `execve` does. The caller's `clone` returns the child's pid once it has exec'd or exited,
//! and `wait4` collects its status through `ProcessWait`. The kernel interprets no Linux
//! number and holds no Linux state. — TEAL

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use kumo_abi::{Handle, ProcessRunFlags};
use kumo_rt::{
    address_space_create, channel_create_pair, channel_read_with_handle, channel_write_with_handle,
    futex_wait, futex_wake, handle_close, handle_koid, process_create, process_run,
    process_wait_reaped, vmar_snapshot, vmar_snapshot_range,
};

use crate::{errno, files, log, signals, threads};

/// vfork children that may be between `clone` and `execve`/`_exit` at once.
pub const MAX_VFORK: usize = 8;
/// Started processes (and children that `_exit`ed before exec) not yet reaped by `wait4`.
const MAX_CHILDREN: usize = 32;

const CLONE_VM: u64 = 0x100;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_THREAD: u64 = 0x1_0000;
const WNOHANG: u64 = 1;

/// The persona process's address space: 64 GiB from 0, as Sora gives the shim (j917).
const PERSONA_WINDOW: u64 = 0x10_0000_0000;
/// The new process's first stack, where Sora puts the shim's (`CHILD_WINDOW_BASE + 0x1001_0000`
/// on aarch64): below the Linux program's interpreter base and stack, above its image.
const SPAWN_STACK_TOP: u64 = 0x1001_0000;
const SPAWN_STACK_SIZE: u64 = 0x4_0000;

/// A vfork slot's life: free, its child running, then exec'd or exited (the caller collects).
const FREE: u32 = 0;
const RUNNING: u32 = 1;
const EXECED: u32 = 2;
const EXITED: u32 = 3;

static SLOT_STATE: [AtomicU32; 1 + MAX_VFORK] = [const { AtomicU32::new(FREE) }; 1 + MAX_VFORK];
/// The pid a slot's child ends up with, written before its state leaves RUNNING.
static SLOT_PID: [AtomicU32; 1 + MAX_VFORK] = [const { AtomicU32::new(0) }; 1 + MAX_VFORK];

#[derive(Clone, Copy)]
struct Child {
    pid: u32,
    /// The KUMO process, 0 for a child that `_exit`ed before it exec'd.
    process: u32,
    koid: u64,
    done: bool,
    /// The `wait4` status word: `(code & 0xff) << 8`.
    status: u32,
}

static mut CHILDREN: [Option<Child>; MAX_CHILDREN] = [None; MAX_CHILDREN];

fn children() -> &'static mut [Option<Child>; MAX_CHILDREN] {
    // SAFETY: the shim lock guards the table.
    unsafe { &mut *core::ptr::addr_of_mut!(CHILDREN) }
}

pub fn is_vfork(flags: u64) -> bool {
    flags & CLONE_VFORK != 0 && flags & CLONE_VM != 0 && flags & CLONE_THREAD == 0
}

/// `clone(CLONE_VM | CLONE_VFORK | SIGCHLD, stack)`: start the child thread in a free slot and
/// sleep until it has exec'd or exited. Called without the shim lock.
pub fn vfork(parent: &threads::ThreadRec, frame: &[u64; 31], stack: u64) -> i64 {
    threads::lock();
    let Some(slot) = (1..=MAX_VFORK).find(|&slot| {
        SLOT_STATE[slot]
            .compare_exchange(FREE, RUNNING, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }) else {
        threads::unlock();
        return -errno::EAGAIN;
    };
    files::set_active(parent.child as usize);
    files::copy_bindings_to(slot);
    signals::copy_actions_to(slot);
    SLOT_PID[slot].store(0, Ordering::Relaxed);
    threads::unlock();

    let tls: u64;
    unsafe { core::arch::asm!("mrs {}, tpidr_el0", out(reg) tls) };
    let setup = threads::Start {
        tls,
        stack,
        clear_tid: 0,
        parent_tid_at: 0,
        child_tid_at: 0,
        child: slot as u64,
    };
    let tid = match threads::start(parent, frame, &setup) {
        Ok(tid) => tid,
        Err(e) => {
            threads::lock();
            files::release_bindings(slot);
            SLOT_STATE[slot].store(FREE, Ordering::Release);
            threads::unlock();
            return e;
        }
    };
    // CLONE_VFORK: the caller resumes when the child has exec'd or exited.
    let state = &SLOT_STATE[slot];
    loop {
        let now = state.load(Ordering::Acquire);
        if now != RUNNING {
            break;
        }
        let _ = futex_wait(state.as_ptr(), now);
    }
    let pid = SLOT_PID[slot].load(Ordering::Acquire);
    state.store(FREE, Ordering::Release);
    if pid != 0 {
        pid as i64
    } else {
        tid as i64
    }
}

/// Release the caller waiting on `slot` with `pid` and the slot's new state. Lock held.
fn release_caller(slot: usize, pid: u32, state: u32) {
    files::release_bindings(slot);
    SLOT_PID[slot].store(pid, Ordering::Release);
    SLOT_STATE[slot].store(state, Ordering::Release);
    let _ = futex_wake(SLOT_STATE[slot].as_ptr(), u32::MAX);
}

fn record(child: Child) -> bool {
    match children().iter_mut().find(|entry| entry.is_none()) {
        Some(entry) => {
            *entry = Some(child);
            true
        }
        None => false,
    }
}

/// A vfork child's `_exit` (or `exit_group`) before it exec'd: its status waits for `wait4`.
pub fn child_exit(rec: &threads::ThreadRec, status: u64) -> ! {
    threads::lock();
    let slot = rec.child as usize;
    let pid = rec.tid as u32;
    let _ = record(Child {
        pid,
        process: 0,
        koid: 0,
        done: true,
        status: ((status & 0xff) as u32) << 8,
    });
    release_caller(slot, pid, EXITED);
    threads::unlock();
    threads::retire(rec)
}

// ---- execve ----------------------------------------------------------------------------------

/// `execve(path, argv, envp)`. In a vfork child it starts the new process and ends the child
/// thread; in the program itself (j925: a fork child, as Cargo's `Command` with `pre_exec`
/// makes) it replaces the program in this process. Returns only with an error.
pub fn execve(rec: &threads::ThreadRec, path: u64, argv: u64, envp: u64) -> i64 {
    if rec.child == 0 {
        return execve_here(rec, path, argv, envp);
    }
    threads::lock();
    files::set_active(rec.child as usize);
    let result = stage_exec(path, argv, envp).and_then(|()| spawn(rec.tid as u32));
    match result {
        Ok((process, koid)) => {
            let pid = rec.tid as u32;
            if !record(Child {
                pid,
                process: process.0,
                koid,
                done: false,
                status: 0,
            }) {
                // No room to remember it: it still runs, and wait4 cannot report it.
                let _ = handle_close(process);
            }
            release_caller(rec.child as usize, pid, EXECED);
            threads::unlock();
            threads::retire(rec)
        }
        Err(e) => {
            threads::unlock();
            e
        }
    }
}

/// j925 (SH7-g): `execve` in this process. The new program's strings are staged first (they live
/// in the memory about to go), then everything the old program had is unmapped: each gap around
/// the shim's own image and the calling thread's trap stack, across the persona window.
/// Close-on-exec descriptors close here; the rest stay as they are. Other threads are not
/// stopped: this serves a fork child, which has one.
fn execve_here(rec: &threads::ThreadRec, path: u64, argv: u64, envp: u64) -> i64 {
    threads::lock();
    files::set_active(0);
    if let Err(e) = stage_exec(path, argv, envp) {
        threads::unlock();
        return e;
    }
    signals::prepare_exec(0);
    files::close_on_exec();
    let base = core::ptr::addr_of!(__ehdr_start) as u64;
    let end = (core::ptr::addr_of!(_end) as u64).next_multiple_of(crate::PAGE);
    // Ascending, so each gap runs from the end of one kept range to the start of the next.
    let mut keep = [
        (base, end),
        (rec.region_base, rec.region_base + rec.region_len),
    ];
    if keep[1].0 < keep[0].0 {
        keep.swap(0, 1);
    }
    let mut at = 0u64;
    for (start, stop) in keep {
        if stop > start {
            if start > at {
                let _ = kumo_rt::vmar_unmap(at, start - at);
            }
            at = at.max(stop);
        }
    }
    if PERSONA_WINDOW > at {
        let _ = kumo_rt::vmar_unmap(at, PERSONA_WINDOW - at);
    }
    files::forget_bounce();
    signals::adopt_exec();
    threads::reset_for_exec();
    threads::unlock();
    crate::exec_staged()
}

/// j925 (SH7-g): `clone(SIGCHLD)` with no new stack, i.e. `fork`. The child is a new process with
/// a private copy of every mapping (J922's whole `VmarSnapshot`), one thread that resumes the
/// program where this call returns (with 0), every descriptor (close-on-exec ones too, flags
/// kept) on handles of its own, and the same dispositions. Returns the child's pid. Called
/// without the shim lock.
pub fn fork(parent: &threads::ThreadRec, frame: &[u64; 31]) -> i64 {
    threads::lock();
    files::set_active(parent.child as usize);
    let result = fork_locked(parent, frame);
    threads::unlock();
    result
}

fn fork_locked(parent: &threads::ThreadRec, frame: &[u64; 31]) -> i64 {
    // The child's thread record and trap stack, prepared here so the snapshot carries them.
    let (rec_at, region_base, region_len) = match threads::prepare_fork_thread(parent, frame) {
        Ok(prepared) => prepared,
        Err(e) => return e,
    };
    let release = || {
        let _ = kumo_rt::vmar_unmap(region_base, region_len);
    };
    let mut handles = files::SpawnHandles::new();
    if let Err(e) = files::plan_fork(&mut handles) {
        release();
        if e == -errno::ENOSYS {
            log(format_args!(
                "persona-linux: fork needs the image mount (a `linux` launch on the image)\n"
            ));
        }
        return e;
    }
    let raw = process_create(0, PERSONA_WINDOW);
    if raw == 0 || raw == u64::MAX {
        handles.close_all();
        release();
        return -errno::EAGAIN;
    }
    let process = Handle(raw as u32);
    let koid = handle_koid(process);
    let Some(pid) = threads::tid_of(koid) else {
        handles.close_all();
        let _ = handle_close(process);
        release();
        return -errno::EAGAIN;
    };
    SPAWN_PID.store(pid, Ordering::Relaxed);
    FORK_REC.store(rec_at, Ordering::Relaxed);
    let started = vmar_snapshot(process) == 0
        && address_space_create(process, SPAWN_STACK_TOP, SPAWN_STACK_SIZE) != u64::MAX
        && start_with(
            process,
            &mut handles,
            persona_fork_entry as *const () as u64,
        );
    // The parent's copy of the child's record is not needed here.
    release();
    if !started {
        handles.close_all();
        let _ = handle_close(process);
        return -errno::EAGAIN;
    }
    if !record(Child {
        pid,
        process: process.0,
        koid,
        done: false,
        status: 0,
    }) {
        let _ = handle_close(process);
    }
    pid as i64
}

/// Queue `handles` on a bootstrap channel and run `process` at `entry` on its fresh stack with
/// the channel in x0. Closes every handle not sent.
fn start_with(process: Handle, handles: &mut files::SpawnHandles, entry: u64) -> bool {
    let (send_raw, recv_raw) = channel_create_pair();
    if send_raw == u64::MAX || recv_raw == u64::MAX {
        return false;
    }
    let send = Handle(send_raw as u32);
    let recv = Handle(recv_raw as u32);
    let mut queued = true;
    for &(tag, handle) in &handles.list[..handles.len] {
        if channel_write_with_handle(send, &tag, 1, handle) != 0 {
            queued = false;
            let _ = handle_close(handle);
        }
    }
    handles.len = 0;
    let _ = handle_close(send);
    let flags = (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG).bits();
    if !queued
        || process_run(
            process,
            entry,
            SPAWN_STACK_TOP - 16,
            recv.0 as u64,
            0,
            flags,
        ) != 0
    {
        let _ = handle_close(recv);
        return false;
    }
    true
}

/// The fork child's thread record (its address in the child), copied with the snapshot.
static FORK_REC: AtomicU64 = AtomicU64::new(0);

/// The fork child's first instruction (ProcessRun: x0 = its bootstrap channel, on the fresh
/// stack). Its memory is the parent's, as the snapshot found it: reset what belonged to the
/// parent's other threads and processes, take the capabilities, then become the thread the
/// prepared record describes and return into the program.
#[unsafe(no_mangle)]
extern "C" fn persona_fork_entry(bootstrap: u64) -> ! {
    threads::reset_for_fork(SPAWN_PID.load(Ordering::Relaxed));
    signals::adopt_fork();
    for state in &SLOT_STATE {
        state.store(FREE, Ordering::Relaxed);
    }
    *children() = [None; MAX_CHILDREN];
    take_capabilities(Handle(bootstrap as u32));
    let rec = FORK_REC.load(Ordering::Relaxed);
    // SAFETY: the record is the prepared one on its own trap stack; `persona_thread_entry`
    // registers it, seeds its open call and returns into the program with x0 = 0.
    unsafe {
        core::arch::asm!(
            "mov sp, {rec}",
            "mov x0, {rec}",
            "b persona_thread_entry",
            rec = in(reg) rec,
            options(noreturn),
        )
    }
}

/// Copy the program path, argv and envp into the exec staging area (main.rs) and check the
/// program can be opened, so a missing one is the caller's ENOENT, as on Linux.
fn stage_exec(path: u64, argv: u64, envp: u64) -> Result<(), i64> {
    let args = crate::exec_args();
    args.clear();
    let mut resolved = [0u8; kumo_vfs::MAX_PATH];
    let len = files::resolve_user(path, &mut resolved)?;
    let program = &resolved[..len];
    let (handle, _) = files::open_path(program)?;
    files::close_handle(handle);
    if !args.set_path(program) {
        return Err(-errno::ENAMETOOLONG);
    }
    for (list, env) in [(argv, false), (envp, true)] {
        if list == 0 {
            continue;
        }
        for index in 0.. {
            // SAFETY: the program's NULL-terminated pointer array.
            let at = unsafe { core::ptr::read_unaligned((list + index * 8) as *const u64) };
            if at == 0 {
                break;
            }
            // SAFETY: a NUL-terminated string the program named.
            let bytes = unsafe { core::ffi::CStr::from_ptr(at as *const core::ffi::c_char) };
            if !args.push(bytes.to_bytes(), env) {
                return Err(-errno::E2BIG);
            }
        }
    }
    Ok(())
}

unsafe extern "C" {
    static __ehdr_start: u8;
    static _end: u8;
}

/// The pid the new process's `getpid` answers: what its parent's `clone` returned.
static SPAWN_PID: AtomicU32 = AtomicU32::new(0);

/// Start the new process: the shim's image copied into a fresh persona VMAR, its stack, and a
/// bootstrap channel carrying its capabilities to `persona_spawn_entry`. Lock held: the staged
/// exec and spawn plan are copied by the snapshot exactly as written.
fn spawn(pid: u32) -> Result<(Handle, u64), i64> {
    let mut handles = files::SpawnHandles::new();
    if let Err(e) = files::plan_spawn(&mut handles) {
        if e == -errno::ENOSYS {
            log(format_args!(
                "persona-linux: execve needs the image mount (a `linux` launch on the image)\n"
            ));
        }
        return Err(e);
    }
    signals::prepare_exec(files::active());
    SPAWN_PID.store(pid, Ordering::Relaxed);

    let raw = process_create(0, PERSONA_WINDOW);
    if raw == 0 || raw == u64::MAX {
        handles.close_all();
        return Err(-errno::EAGAIN);
    }
    let process = Handle(raw as u32);
    let base = core::ptr::addr_of!(__ehdr_start) as u64;
    let end = (core::ptr::addr_of!(_end) as u64).next_multiple_of(crate::PAGE);
    let status = vmar_snapshot_range(process, base, end - base);
    if status != 0 {
        log(format_args!(
            "persona-linux: execve: VmarSnapshot refused ({:#x})\n",
            status
        ));
        handles.close_all();
        let _ = handle_close(process);
        return Err(-errno::ENOMEM);
    }
    if address_space_create(process, SPAWN_STACK_TOP, SPAWN_STACK_SIZE) == u64::MAX {
        handles.close_all();
        let _ = handle_close(process);
        return Err(-errno::ENOMEM);
    }
    let koid = handle_koid(process);
    if !start_with(
        process,
        &mut handles,
        persona_spawn_entry as *const () as u64,
    ) {
        handles.close_all();
        let _ = handle_close(process);
        return Err(-errno::EAGAIN);
    }
    Ok((process, koid))
}

/// The new process's first instruction (ProcessRun: x0 = its bootstrap channel). Its memory is
/// the parent shim's image as the snapshot found it: reset what belonged to the parent, take
/// the capabilities, and load the staged program.
#[unsafe(no_mangle)]
extern "C" fn persona_spawn_entry(bootstrap: u64) -> ! {
    threads::reset_for_spawn(SPAWN_PID.load(Ordering::Relaxed));
    signals::adopt_exec();
    for state in &SLOT_STATE {
        state.store(FREE, Ordering::Relaxed);
    }
    *children() = [None; MAX_CHILDREN];

    take_capabilities(Handle(bootstrap as u32));
    crate::exec_staged()
}

/// Read a new process's bootstrap channel to its end and rebuild its descriptors from the plan
/// the snapshot carried: `'f'` session, `'m'` mount control, `'i'`/`'o'` console ends and the
/// `'p'` pipe and socket channels in plan order.
fn take_capabilities(bootstrap: Handle) {
    let (mut fs, mut mount, mut stdin, mut stdout) = (Handle(0), Handle(0), Handle(0), Handle(0));
    let mut pipes = [Handle(0); 64];
    let mut pipe_count = 0usize;
    loop {
        let mut tag = 0u8;
        let (n, raw) = channel_read_with_handle(bootstrap, &mut tag, 1);
        if n == 0 {
            break;
        }
        if raw == 0 || raw == u64::MAX {
            continue;
        }
        let handle = Handle(raw as u32);
        match tag {
            b'f' => fs = handle,
            b'm' => mount = handle,
            b'i' => stdin = handle,
            b'o' => stdout = handle,
            b'p' if pipe_count < pipes.len() => {
                pipes[pipe_count] = handle;
                pipe_count += 1;
            }
            _ => {
                let _ = handle_close(handle);
            }
        }
    }
    let _ = handle_close(bootstrap);
    files::adopt_spawn(fs, mount, stdin, stdout, &pipes[..pipe_count]);
}

// ---- wait4 -----------------------------------------------------------------------------------

/// Move every reaped child's exit code into the table. Lock held.
fn collect() {
    for _ in 0..MAX_CHILDREN {
        let (_, koid, code) = process_wait_reaped();
        if koid == 0 {
            break;
        }
        if let Some(child) = children()
            .iter_mut()
            .flatten()
            .find(|child| child.koid == koid && !child.done)
        {
            child.done = true;
            child.status = ((code & 0xff) as u32) << 8;
            let _ = handle_close(Handle(child.process));
            child.process = 0;
        }
    }
}

/// `wait4(pid, status, options, rusage)` for this program's children: one pid or any (-1),
/// blocking or WNOHANG. Called without the shim lock; it sleeps between looks on a kernel
/// timer, never spinning (a spinning child starves Sora).
pub fn wait4(rec: &threads::ThreadRec, pid: i64, status: u64, options: u64, rusage: u64) -> i64 {
    if rec.child != 0 || pid == 0 || pid < -1 {
        // Process groups are not modelled; a vfork child has no children of its own.
        return -errno::ECHILD;
    }
    if options & !WNOHANG != 0 {
        return -errno::EINVAL;
    }
    if rusage != 0 {
        // SAFETY: the program's `struct rusage` (144 bytes on aarch64): nothing measured.
        unsafe { core::ptr::write_bytes(rusage as *mut u8, 0, 144) };
    }
    loop {
        threads::lock();
        collect();
        let mut exists = false;
        let mut reaped = None;
        for entry in children().iter_mut() {
            let Some(child) = entry else {
                continue;
            };
            if pid != -1 && pid as u32 != child.pid {
                continue;
            }
            exists = true;
            if child.done {
                reaped = Some((child.pid, child.status));
                *entry = None;
                break;
            }
        }
        threads::unlock();
        if let Some((pid, word)) = reaped {
            if status != 0 {
                // SAFETY: the program's int.
                unsafe { (status as *mut u32).write_unaligned(word) };
            }
            return pid as i64;
        }
        if !exists {
            return -errno::ECHILD;
        }
        if options & WNOHANG != 0 {
            return 0;
        }
        let _ = kumo_rt::sleep_ns(10_000_000);
    }
}
