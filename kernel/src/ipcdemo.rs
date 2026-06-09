//! P4 blocking-IPC smoke: real blocking between two running kernel threads.
//!
//! Two cooperative kernel threads share a real [`crate::ipc::ChannelPair`]. The
//! consumer tries to read; on an empty channel it does not spin — it **blocks**
//! (transitions to `Blocked` and parks off the run queue). The producer writes a
//! message and **wakes** the consumer (back to `Ready`), after which the consumer
//! resumes and receives the payload. This is the scheduler-integrated block/wake the
//! syscall layer will reuse for `ChannelRead`/`PortWait`; here it enters through
//! `sched::Dispatcher`'s production-shaped block/wake/finish points and then uses the
//! proven context switch to move directly to the chosen thread.
//!
//! Soundness: same discipline as `kdemo` — the demo state lives in a `static`, and no
//! Rust reference to it is ever held across a `switch_context`. Single core, and the
//! preemption hook is not installed during this demo, so timer IRQs only tick.

use core::cell::UnsafeCell;

use kumo_hal::active::{switch_context, ThreadContext};

use crate::ipc::{ChannelEnd, ChannelPair, IpcError, KernelMessage};
use crate::mm::{Vmar, PAGE_SIZE};
use crate::object::ObjectManager;
use crate::sched::{Decision, Dispatcher, Priority};
use crate::task::{Job, Process, Thread, ThreadState, DEFAULT_KERNEL_STACK_SIZE};

const CONSUMER: usize = 0;
const PRODUCER: usize = 1;
const PAYLOAD: &[u8] = b"hello from producer";

struct Demo {
    /// Context to return to (the scheduler loop) when a thread yields, blocks, or exits.
    main_ctx: ThreadContext,
    threads: [Thread; 2],
    scheduler: Dispatcher,
    channel: ChannelPair,
    switches: u64,
    consumer_blocks: u64,
    wakes: u64,
    received: usize,
    delivered: bool,
}

struct DemoCell(UnsafeCell<Option<Demo>>);
// Boot-path only, single core, cooperative: there is never concurrent access.
unsafe impl Sync for DemoCell {}

static DEMO: DemoCell = DemoCell(UnsafeCell::new(None));

fn demo_ptr() -> *mut Demo {
    let opt: *mut Option<Demo> = DEMO.0.get();
    // SAFETY: `run` initializes DEMO before anything calls this; single-core boot.
    unsafe { (&mut *opt).as_mut().expect("ipcdemo not initialized") as *mut Demo }
}

/// Park the current thread and switch directly to the dispatcher's chosen successor.
fn block_current_thread() {
    let p = demo_ptr();
    let switch = unsafe {
        let d = &mut *p;
        let Some(cur) = d.scheduler.current() else {
            return;
        };
        let Some(cur_idx) = thread_index(d, cur) else {
            return;
        };
        d.threads[cur_idx].block();
        let decision = d.scheduler.block_current();
        switch_for_decision(d, decision).or_else(|| switch_thread_to_main(d, cur_idx))
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
    }
}

fn maybe_switch_after_wake(decision: Decision) {
    let p = demo_ptr();
    let switch = unsafe {
        let d = &mut *p;
        switch_for_decision(d, decision)
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
    }
}

fn switch_for_decision(
    d: &mut Demo,
    decision: Decision,
) -> Option<(*mut ThreadContext, *const ThreadContext)> {
    match decision {
        Decision::Switch { from, to } => {
            let to_idx = thread_index(d, to)?;
            let prev = if let Some(from) = from {
                let from_idx = thread_index(d, from)?;
                if matches!(d.threads[from_idx].state(), ThreadState::Running) {
                    d.threads[from_idx].ready();
                }
                d.threads[from_idx].context_mut() as *mut ThreadContext
            } else {
                &mut d.main_ctx as *mut ThreadContext
            };
            d.threads[to_idx].run();
            d.switches = d.switches.saturating_add(1);
            let next = d.threads[to_idx].context() as *const ThreadContext;
            Some((prev, next))
        }
        Decision::Idle | Decision::Continue(_) => None,
    }
}

fn switch_thread_to_main(
    d: &mut Demo,
    index: usize,
) -> Option<(*mut ThreadContext, *const ThreadContext)> {
    d.switches = d.switches.saturating_add(1);
    let cur = d.threads[index].context_mut() as *mut ThreadContext;
    let main = &d.main_ctx as *const ThreadContext;
    Some((cur, main))
}

fn thread_index(d: &Demo, koid: kumo_abi::KoId) -> Option<usize> {
    d.threads.iter().position(|thread| thread.koid() == koid)
}

extern "C" fn consumer_body(_arg: usize) {
    loop {
        let result = {
            let p = demo_ptr();
            // SAFETY: transient &mut, dropped before any switch.
            unsafe { (&mut *p).channel.read(ChannelEnd::Right) }
        };
        match result {
            Ok(message) => {
                let p = demo_ptr();
                unsafe {
                    let d = &mut *p;
                    d.received = message.bytes().len();
                    d.delivered = message.bytes() == PAYLOAD;
                }
                break;
            }
            Err(IpcError::ShouldWait) => {
                {
                    let p = demo_ptr();
                    unsafe {
                        (*p).consumer_blocks += 1;
                    }
                }
                block_current_thread();
            }
            Err(_) => break,
        }
    }
    thread_exit(CONSUMER)
}

extern "C" fn producer_body(_arg: usize) {
    let wake_decision = {
        let p = demo_ptr();
        // SAFETY: transient &mut, dropped before any switch.
        unsafe {
            let d = &mut *p;
            if let Ok(message) = KernelMessage::new(1, PAYLOAD, &[]) {
                let _ = d.channel.write(ChannelEnd::Left, message);
            }
            if matches!(d.threads[CONSUMER].state(), ThreadState::Blocked) {
                d.threads[CONSUMER].ready();
                d.wakes += 1;
                let decision = d
                    .scheduler
                    .wake_rt(d.threads[CONSUMER].koid(), Priority::DEFAULT);
                Some(decision)
            } else {
                None
            }
        }
    };
    if let Some(decision) = wake_decision {
        maybe_switch_after_wake(decision);
    }
    thread_exit(PRODUCER)
}

fn thread_exit(index: usize) -> ! {
    let p = demo_ptr();
    let switch = unsafe {
        let d = &mut *p;
        d.threads[index].terminate();
        let decision = d.scheduler.finish_current();
        switch_for_decision(d, decision).or_else(|| switch_thread_to_main(d, index))
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
    }
    loop {
        kumo_hal::active::spin_once();
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcDemoReport {
    pub switches: u64,
    pub consumer_blocks: u64,
    pub wakes: u64,
    pub received: usize,
    pub delivered: bool,
}

/// Run the blocking-IPC demo and report what happened.
pub fn run() -> IpcDemoReport {
    let mut objects = ObjectManager::new();
    let job = Job::root(&mut objects);
    let vmar = Vmar::new(0xffff_0000_0000_0000, PAGE_SIZE * 256).expect("ipc vmar");
    let process = Process::new(&mut objects, &job, vmar);
    let channel = ChannelPair::new(&mut objects);

    let consumer = Thread::new(
        &mut objects,
        &process,
        consumer_body as extern "C" fn(usize) as usize,
        0,
        DEFAULT_KERNEL_STACK_SIZE,
    )
    .expect("consumer thread");
    let producer = Thread::new(
        &mut objects,
        &process,
        producer_body as extern "C" fn(usize) as usize,
        0,
        DEFAULT_KERNEL_STACK_SIZE,
    )
    .expect("producer thread");

    let mut scheduler = Dispatcher::new(1);
    scheduler.admit(consumer.koid(), Priority::DEFAULT);
    scheduler.admit(producer.koid(), Priority::DEFAULT);

    // SAFETY: first writer; nothing else touches DEMO yet.
    unsafe {
        *DEMO.0.get() = Some(Demo {
            main_ctx: ThreadContext::default(),
            threads: [consumer, producer],
            scheduler,
            channel,
            switches: 0,
            consumer_blocks: 0,
            wakes: 0,
            received: 0,
            delivered: false,
        });
    }

    let p = demo_ptr();
    // Start on the first dispatcher pick (the consumer), then let block/wake/finish
    // decisions switch directly between thread bodies until the last thread returns
    // here through `main_ctx`.
    let switch = unsafe {
        let d = &mut *p;
        let decision = d.scheduler.reschedule_current();
        switch_for_decision(d, decision)
    };
    if let Some((prev, next)) = switch {
        unsafe { switch_context(prev, next) };
    }

    // SAFETY: both threads terminated; only this code touches DEMO now.
    unsafe {
        let d = &*p;
        IpcDemoReport {
            switches: d.switches,
            consumer_blocks: d.consumer_blocks,
            wakes: d.wakes,
            received: d.received,
            delivered: d.delivered,
        }
    }
}
