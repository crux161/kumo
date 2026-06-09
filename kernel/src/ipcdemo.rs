//! P4 blocking-IPC smoke: real blocking between two running kernel threads.
//!
//! Two cooperative kernel threads share a real [`crate::ipc::ChannelPair`]. The
//! consumer tries to read; on an empty channel it does not spin — it **blocks**
//! (transitions to `Blocked` and parks off the run queue). The producer writes a
//! message and **wakes** the consumer (back to `Ready`), after which the consumer
//! resumes and receives the payload. This is the scheduler-integrated block/wake the
//! syscall layer will reuse for `ChannelRead`/`PortWait`; here it is driven by the
//! proven cooperative context switch rather than the (still-narrow) interrupt-context
//! path, so it touches no new architecture assembly.
//!
//! Soundness: same discipline as `kdemo` — the demo state lives in a `static`, and no
//! Rust reference to it is ever held across a `switch_context`. Single core, and the
//! preemption hook is not installed during this demo, so timer IRQs only tick.

use core::cell::UnsafeCell;

use kumo_hal::active::{switch_context, ThreadContext};

use crate::ipc::{ChannelEnd, ChannelPair, IpcError, KernelMessage};
use crate::mm::{Vmar, PAGE_SIZE};
use crate::object::ObjectManager;
use crate::task::{Job, Process, Thread, ThreadState, DEFAULT_KERNEL_STACK_SIZE};

const CONSUMER: usize = 0;
const PRODUCER: usize = 1;
const PAYLOAD: &[u8] = b"hello from producer";

struct Demo {
    /// Context to return to (the scheduler loop) when a thread yields, blocks, or exits.
    main_ctx: ThreadContext,
    threads: [Thread; 2],
    channel: ChannelPair,
    current: usize,
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

/// Hand the CPU back to the scheduler loop. If `block` is set, the current thread
/// parks (`Blocked`) and stays off the run queue until something `ready()`s it.
fn yield_to_main(block: bool) {
    let p = demo_ptr();
    let (cur, main): (*mut ThreadContext, *const ThreadContext) = unsafe {
        let d = &mut *p;
        d.switches += 1;
        let i = d.current;
        if block {
            d.threads[i].block();
        }
        let cur = d.threads[i].context_mut() as *mut ThreadContext;
        let main = &d.main_ctx as *const ThreadContext;
        (cur, main)
    };
    unsafe { switch_context(cur, main) };
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
                yield_to_main(true);
            }
            Err(_) => break,
        }
    }
    thread_exit(CONSUMER)
}

extern "C" fn producer_body(_arg: usize) {
    {
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
            }
        }
    }
    thread_exit(PRODUCER)
}

fn thread_exit(index: usize) -> ! {
    let p = demo_ptr();
    let (cur, main): (*mut ThreadContext, *const ThreadContext) = unsafe {
        let d = &mut *p;
        d.threads[index].terminate();
        d.switches += 1;
        let cur = d.threads[index].context_mut() as *mut ThreadContext;
        let main = &d.main_ctx as *const ThreadContext;
        (cur, main)
    };
    unsafe { switch_context(cur, main) };
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

    // SAFETY: first writer; nothing else touches DEMO yet.
    unsafe {
        *DEMO.0.get() = Some(Demo {
            main_ctx: ThreadContext::default(),
            threads: [consumer, producer],
            channel,
            current: 0,
            switches: 0,
            consumer_blocks: 0,
            wakes: 0,
            received: 0,
            delivered: false,
        });
    }

    let p = demo_ptr();
    // Round-robin over runnable threads (skip Blocked and Terminated). `last` starts
    // at the end so the first pick is the consumer, which blocks on the empty channel;
    // then the producer runs, writes, and wakes it; then the consumer receives.
    let mut last = 1;
    loop {
        let pick = unsafe {
            let d = &*p;
            let mut chosen = None;
            let mut k = 0;
            while k < 2 {
                let idx = (last + 1 + k) % 2;
                let state = d.threads[idx].state();
                if state != ThreadState::Terminated && state != ThreadState::Blocked {
                    chosen = Some(idx);
                    break;
                }
                k += 1;
            }
            chosen
        };
        let Some(idx) = pick else { break };
        last = idx;

        let (main, next): (*mut ThreadContext, *const ThreadContext) = unsafe {
            let d = &mut *p;
            d.current = idx;
            d.switches += 1;
            d.threads[idx].run();
            let main = &mut d.main_ctx as *mut ThreadContext;
            let next = d.threads[idx].context() as *const ThreadContext;
            (main, next)
        };
        unsafe { switch_context(main, next) };
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
