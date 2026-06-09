//! M3 cooperative kernel-thread smoke.
//!
//! This is the first time KUMO runs more than one thread of control. It builds a
//! root `Job`, a kernel `Process`, and a couple of `Thread`s, then drives them with
//! the HAL context switch: switch into a thread, let it do a little work and
//! voluntarily `kyield()` back, pick the next ready thread, repeat. It proves the
//! `kumo_context_switch` / trampoline path actually saves and restores kernel
//! thread state on real silicon.
//!
//! Deliberately *cooperative* for the `kthread_body` threads. The preemption probe
//! below drives the real `sched::Dispatcher` (Discipline A, the O(1) strict-priority
//! class) from the timer IRQ and rotates thread *state* in interrupt context; the
//! remaining step — switching thread *bodies* from the IRQ (real preemptive
//! multitasking) — is the next slice. The switch itself only exists on the
//! freestanding target; the scheduler *policy* is host-tested in `sched::tests`.
//!
//! Soundness note: the demo state lives in a `static` and is reached reentrantly
//! (the scheduler loop and a running thread's `kyield` both touch it). To stay
//! sound, no Rust reference to it is ever held across a `switch_context` — each
//! access takes a transient reference, computes the raw context pointers, drops the
//! reference, and only then switches. Single-core, cooperative: no concurrency.

use alloc::vec::Vec;
use core::cell::UnsafeCell;

use kumo_hal::active::{switch_context, ThreadContext};

use crate::mm::{Vmar, PAGE_SIZE};
use crate::object::ObjectManager;
use crate::sched::{ClassId, Decision, Dispatcher, Priority};
use crate::task::{Job, Process, Thread, ThreadState, DEFAULT_KERNEL_STACK_SIZE};

const KTHREADS: usize = 2;
const YIELDS_PER_THREAD: u32 = 3;

struct Demo {
    /// The scheduler-loop context we return to whenever a thread yields or exits.
    main_ctx: ThreadContext,
    threads: Vec<Thread>,
    /// A thread that is created but never scheduled — the future idle thread. Kept so
    /// `ps` can show a task in a state other than `terminated`.
    idle: Thread,
    /// Timer-driven scheduler smoke threads. These are deliberately not entered yet:
    /// the hook proves IRQ -> scheduler wiring and strict-priority state rotation,
    /// while full interrupt-context execution waits for the next slice.
    preempt_threads: Vec<Thread>,
    preempt_scheduler: Dispatcher,
    job_koid: u64,
    process_koid: u64,
    current: usize,
    switches: u64,
    work: u64,
    preempt_ticks: u64,
    preempt_switches: u64,
    preempt_hook_installed: bool,
}

struct DemoCell(UnsafeCell<Option<Demo>>);
// Boot-path only, single core, cooperative: there is never concurrent access.
unsafe impl Sync for DemoCell {}

static DEMO: DemoCell = DemoCell(UnsafeCell::new(None));

fn demo_ptr() -> *mut Demo {
    let opt: *mut Option<Demo> = DEMO.0.get();
    // SAFETY: `run` initializes DEMO before anything calls this; single-core boot.
    unsafe { (&mut *opt).as_mut().expect("kdemo not initialized") as *mut Demo }
}

/// A kernel thread body: do a few units of work, yielding between each, then exit.
extern "C" fn kthread_body(_arg: usize) {
    let mut done = 0;
    while done < YIELDS_PER_THREAD {
        let p = demo_ptr();
        // SAFETY: transient &mut, dropped before any switch.
        unsafe {
            (*p).work += 1;
        }
        kyield();
        done += 1;
    }
    kthread_exit();
}

/// Voluntarily hand the CPU back to the scheduler loop. Resumes here when the loop
/// next switches into this thread.
fn kyield() {
    let p = demo_ptr();
    // SAFETY: take a transient &mut, capture raw context pointers, drop the &mut,
    // then switch using only raw pointers (no live reference across the switch).
    let (cur, main): (*mut ThreadContext, *const ThreadContext) = unsafe {
        let demo = &mut *p;
        demo.switches += 1;
        let i = demo.current;
        let cur = demo.threads[i].context_mut() as *mut ThreadContext;
        let main = &demo.main_ctx as *const ThreadContext;
        (cur, main)
    };
    unsafe { switch_context(cur, main) };
}

/// Terminate the current thread and return to the scheduler loop for good.
fn kthread_exit() -> ! {
    let p = demo_ptr();
    let (cur, main): (*mut ThreadContext, *const ThreadContext) = unsafe {
        let demo = &mut *p;
        let i = demo.current;
        demo.threads[i].terminate();
        demo.switches += 1;
        let cur = demo.threads[i].context_mut() as *mut ThreadContext;
        let main = &demo.main_ctx as *const ThreadContext;
        (cur, main)
    };
    unsafe { switch_context(cur, main) };
    // The scheduler never switches back into a terminated thread.
    loop {
        kumo_hal::active::spin_once();
    }
}

pub struct DemoReport {
    pub threads: usize,
    pub switches: u64,
    pub work: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PreemptStats {
    pub ticks: u64,
    pub switches: u64,
}

fn state_str(state: ThreadState) -> &'static str {
    match state {
        ThreadState::New => "new",
        ThreadState::Ready => "ready",
        ThreadState::Running => "running",
        ThreadState::Blocked => "blocked",
        ThreadState::Terminated => "terminated",
    }
}

/// A snapshot of the task objects this demo created, for the shell's `ps` command.
/// Must be called after `run`.
pub fn tasks() -> Vec<crate::shell::TaskInfo> {
    use crate::shell::TaskInfo;
    let hook_installed = {
        let p = demo_ptr();
        unsafe { (&*p).preempt_hook_installed }
    };
    if hook_installed {
        kumo_hal::active::clear_preempt_hook();
    }

    let p = demo_ptr();
    // SAFETY: read-only snapshot, after `run`, single-core.
    let d = unsafe { &*p };
    let mut out = Vec::new();
    out.push(TaskInfo {
        koid: d.job_koid,
        kind: "job",
        state: "-",
        label: "root",
    });
    out.push(TaskInfo {
        koid: d.process_koid,
        kind: "process",
        state: "-",
        label: "kernel",
    });
    let labels = ["kdemo-a", "kdemo-b"];
    for (i, thread) in d.threads.iter().enumerate() {
        out.push(TaskInfo {
            koid: thread.koid().0,
            kind: "thread",
            state: state_str(thread.state()),
            label: labels.get(i).copied().unwrap_or("kdemo"),
        });
    }
    out.push(TaskInfo {
        koid: d.idle.koid().0,
        kind: "thread",
        state: state_str(d.idle.state()),
        label: "idle",
    });
    let labels = ["preempt-a", "preempt-b"];
    for (i, thread) in d.preempt_threads.iter().enumerate() {
        out.push(TaskInfo {
            koid: thread.koid().0,
            kind: "thread",
            state: state_str(thread.state()),
            label: labels.get(i).copied().unwrap_or("preempt"),
        });
    }

    if hook_installed {
        kumo_hal::active::set_preempt_hook(preempt_tick);
    }
    out
}

pub fn preempt_stats() -> PreemptStats {
    let hook_installed = {
        let p = demo_ptr();
        unsafe { (&*p).preempt_hook_installed }
    };
    if hook_installed {
        kumo_hal::active::clear_preempt_hook();
    }

    let p = demo_ptr();
    let stats = unsafe {
        let d = &*p;
        PreemptStats {
            ticks: d.preempt_ticks,
            switches: d.preempt_switches,
        }
    };

    if hook_installed {
        kumo_hal::active::set_preempt_hook(preempt_tick);
    }
    stats
}

pub fn install_preemption_probe() {
    let p = demo_ptr();
    let should_install = unsafe {
        let d = &mut *p;
        if d.preempt_hook_installed {
            false
        } else {
            d.preempt_hook_installed = true;
            true
        }
    };
    if should_install {
        kumo_hal::active::set_preempt_hook(preempt_tick);
    }
}

extern "C" fn preempt_tick() {
    let p = demo_ptr();
    unsafe {
        let d = &mut *p;
        d.preempt_ticks = d.preempt_ticks.saturating_add(1);
        match d.preempt_scheduler.on_timer_tick() {
            Decision::Idle | Decision::Continue(_) => {}
            Decision::Switch { from, to } => {
                d.preempt_switches = d.preempt_switches.saturating_add(1);
                if let Some(from) = from {
                    set_thread_state(&mut d.preempt_threads, from, ThreadState::Ready);
                }
                set_thread_state(&mut d.preempt_threads, to, ThreadState::Running);
            }
        }
    }
}

fn set_thread_state(threads: &mut [Thread], koid: kumo_abi::KoId, state: ThreadState) {
    for thread in threads {
        if thread.koid() == koid {
            match state {
                ThreadState::Ready => thread.ready(),
                ThreadState::Running => thread.run(),
                ThreadState::Blocked => thread.block(),
                ThreadState::Terminated => thread.terminate(),
                ThreadState::New => {}
            }
            break;
        }
    }
}

/// Run the cooperative kernel-thread demo and report what happened.
pub fn run() -> DemoReport {
    let mut objects = ObjectManager::new();
    let job = Job::root(&mut objects);
    let vmar = Vmar::new(0xffff_0000_0000_0000, PAGE_SIZE * 256).expect("kernel vmar");
    let process = Process::new(&mut objects, &job, vmar);

    let entry = kthread_body as extern "C" fn(usize) as usize;
    let mut threads = Vec::new();
    let mut i = 0;
    while i < KTHREADS {
        let thread = Thread::new(&mut objects, &process, entry, i, DEFAULT_KERNEL_STACK_SIZE)
            .expect("kernel thread");
        threads.push(thread);
        i += 1;
    }
    // An extra thread we never schedule, so `ps` shows a live (New) task alongside the
    // terminated demo threads.
    let idle = Thread::new(
        &mut objects,
        &process,
        entry,
        usize::MAX,
        DEFAULT_KERNEL_STACK_SIZE,
    )
    .expect("idle thread");
    let mut preempt_threads = Vec::new();
    let mut i = 0;
    while i < KTHREADS {
        let thread = Thread::new(
            &mut objects,
            &process,
            entry,
            0x100 + i,
            DEFAULT_KERNEL_STACK_SIZE,
        )
        .expect("preempt thread");
        preempt_threads.push(thread);
        i += 1;
    }
    // Two equal-priority RT threads: with a 1-tick quantum they round-robin every
    // timer tick under Discipline A (the O(1) strict-priority class).
    preempt_threads[0].run();
    preempt_threads[1].ready();
    let mut preempt_scheduler = Dispatcher::new(1);
    preempt_scheduler.set_running(preempt_threads[0].koid(), Priority::DEFAULT, ClassId::Rt);
    preempt_scheduler.admit(preempt_threads[1].koid(), Priority::DEFAULT);

    // SAFETY: first writer; nothing else touches DEMO yet.
    unsafe {
        *DEMO.0.get() = Some(Demo {
            main_ctx: ThreadContext::default(),
            threads,
            idle,
            preempt_threads,
            preempt_scheduler,
            job_koid: job.koid().0,
            process_koid: process.koid().0,
            current: 0,
            switches: 0,
            work: 0,
            preempt_ticks: 0,
            preempt_switches: 0,
            preempt_hook_installed: false,
        });
    }

    let p = demo_ptr();
    // Cooperative round-robin. `last` starts at the end so the first pick is thread 0.
    let mut last = KTHREADS - 1;
    loop {
        // Pick the next ready thread (transient shared reference).
        let pick = unsafe {
            let demo = &*p;
            let mut chosen = None;
            let mut k = 0;
            while k < KTHREADS {
                let idx = (last + 1 + k) % KTHREADS;
                if demo.threads[idx].state() != ThreadState::Terminated {
                    chosen = Some(idx);
                    break;
                }
                k += 1;
            }
            chosen
        };
        let Some(idx) = pick else { break };
        last = idx;

        // Capture raw context pointers, drop the &mut, then switch.
        let (main, next): (*mut ThreadContext, *const ThreadContext) = unsafe {
            let demo = &mut *p;
            demo.current = idx;
            demo.switches += 1;
            let main = &mut demo.main_ctx as *mut ThreadContext;
            let next = demo.threads[idx].context() as *const ThreadContext;
            (main, next)
        };
        unsafe { switch_context(main, next) };
    }

    // SAFETY: all threads terminated; only this code touches DEMO now.
    unsafe {
        let demo = &*p;
        DemoReport {
            threads: demo.threads.len(),
            switches: demo.switches,
            work: demo.work,
        }
    }
}
