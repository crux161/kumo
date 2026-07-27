#![no_std]
#![no_main]
//j496
//j497
//j498
//j499
//j500

//! `threads` — the D2/D3/D4 proof: futex peers, fair compute, then work queues.
//!
//! Run it from the shell with `threads`. Sora loads this one ELF, maps it once, gives each thread
//! its **own** stack, and starts two residents in the same process — so both threads execute the
//! code below, distinguished only by the index in `x0`.
//!
//! What each part is actually proving, because "hello from thread A" would prove almost none of it:
//!
//! - **Two threads really run.** The output interleaves. One thread printing twice in a row would
//!   mean the other never started.
//! - **They share an address space.** `TURN` and `WROTE` are ordinary statics in this image's
//!   `.bss`. The image is mapped once; if the two residents did not share it, they would be
//!   incrementing different words and the handshake could never complete.
//! - **The futex works.** The alternation is not a spin — a thread whose turn it is not calls
//!   `futex_wait` and stops running entirely until its peer calls `futex_wake`. If the futex were
//!   broken this either deadlocks (wake lost) or burns the watchdog budget spinning, and both are
//!   visible in the output.
//! - **Their stacks are independent.** Each thread fills a local array on its own stack with a
//!   pattern derived from its index, sleeps in the middle of the handshake, and re-checks the
//!   array afterwards. A shared or clobbered stack fails this across a context switch.
//! - **Their registers are independent.** A value kept in a local across every sleep is verified at
//!   the end. If contexts were not saved per thread, it would not survive.
//!
//! After the futex proof, both residents enter a syscall-free compute loop. Each counts its own
//! work and how many distinct peer runs it observed. The last thread reports both counters and
//! rejects a ratio worse than 2:1, proving the live timer path schedules equal-priority peers
//! fairly instead of waiting for a syscall or voluntary yield.
//!
//! Finally, thread 0 submits three asynchronous jobs and one synchronous job to an in-process
//! serial queue while thread 1 is its sole worker. FIFO output `1234` plus the synchronous result
//! proves callers submit work to a queue rather than coordinating the worker thread directly.
//! Both residents then become workers for one concurrent queue and each drains one of its two jobs.
//! Thread 0 finally submits synchronously to that queue and resumes only after thread 1 completes it.
//! The residents finish by contending on one once gate and observing the same initialized result.
//! A final group wait resumes only after its peer drains both registered jobs.
//! The last waiter consumes one retained semaphore permit from its peer.

use kumo_abi::Handle;
use kumo_dispatch::{ConcurrentQueue, Group, Once, Semaphore, SerialQueue};
use kumo_rt::{channel_write, debug_write, futex_wait, futex_wake, process_exit, startup};

extern crate alloc;

kumo_rt::entry!(main);

/// How many times the two threads hand off. Enough to be obviously alternating, short enough that
/// a failure is quick.
const ROUNDS: u32 = 6;

/// Whose turn it is: 0 or 1. The futex word, and the only thing the two threads coordinate on.
static TURN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Handoffs completed, so the last thread out can tell whether the whole run happened.
static COMPLETED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Set by either thread if any check fails, so one bad round is reported rather than lost.
static FAULTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Bytes each thread wrote, proving both reached the end rather than one running twice.
static WROTE: [core::sync::atomic::AtomicU32; 2] = [
    core::sync::atomic::AtomicU32::new(0),
    core::sync::atomic::AtomicU32::new(0),
];

/// Both threads must enter the compute phase before either begins measuring.
static COMPUTE_READY: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Syscall-free work completed by each resident.
static COMPUTE_WORK: [core::sync::atomic::AtomicU32; 2] = [
    core::sync::atomic::AtomicU32::new(0),
    core::sync::atomic::AtomicU32::new(0),
];

/// How many distinct runs of its peer each resident observed.
static PEER_EPOCHS: [core::sync::atomic::AtomicU32; 2] = [
    core::sync::atomic::AtomicU32::new(0),
    core::sync::atomic::AtomicU32::new(0),
];

/// Number of compute residents that reached the final report barrier.
static COMPUTE_DONE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Requiring several observed peer runs proves repeated timer handoff, not just one lucky switch.
const FAIR_EPOCHS: u32 = 8;

/// D4a's one in-process serial queue: four bounded slots and one claimed worker.
static SERIAL_QUEUE: SerialQueue<4> = SerialQueue::new();

/// Decimal digits appended by the serial worker; `1234` proves FIFO execution.
static SERIAL_LOG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Producer sets this only after validating and reporting; the worker waits before exiting.
static DISPATCH_DONE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// D4b's shared queue: both residents own worker tokens and drain one item each.
static CONCURRENT_QUEUE: ConcurrentQueue<2> = ConcurrentQueue::new();

/// Both concurrent workers must be ready before either begins draining.
static CONCURRENT_READY: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Worker-index bits observed after each resident executes one item.
static CONCURRENT_WORKERS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Number of concurrent jobs that actually executed.
static CONCURRENT_JOBS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Sum of distinct job return values; 10 + 20 proves both queued items were consumed.
static CONCURRENT_SUM: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Submission failures, shared so either worker can produce the final report.
static CONCURRENT_FAILURES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Number of workers that finished one item.
static CONCURRENT_DONE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The first finisher waits here until the second has emitted the acceptance result.
static CONCURRENT_REPORTED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The peer advertises that it is about to park as D4c's concurrent worker.
static CONCURRENT_SYNC_READY: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Result returned from the work function through the peer worker's `run_one`.
static CONCURRENT_SYNC_WORKER_RESULT: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// The peer remains alive until the synchronous caller validates and reports.
static CONCURRENT_SYNC_REPORTED: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// D4d's one shared initialization gate.
static ONCE: Once = Once::new();

/// Number of times the winning initializer actually ran.
static ONCE_CALLS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The winner publishes this while still inside the initializer so its peer can contend.
static ONCE_INIT_STARTED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Thread 1 sets this immediately before entering the already-running once gate.
static ONCE_CONTENDER: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Result each caller received from the shared gate.
static ONCE_RESULTS: [core::sync::atomic::AtomicU32; 2] = [
    core::sync::atomic::AtomicU32::new(0),
    core::sync::atomic::AtomicU32::new(0),
];

/// Number of callers that returned from the once gate.
static ONCE_DONE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The first finisher remains alive until the acceptance line is emitted.
static ONCE_REPORTED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// D4e's finite set of registered queue work.
static GROUP: Group = Group::new();

/// Number of group jobs successfully queued for the peer worker.
static GROUP_QUEUED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Sum published by group members before they leave.
static GROUP_SUM: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Registration, submission, or leave failures.
static GROUP_FAILURES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Producer publishes the complete registered set before the peer drains it.
static GROUP_READY: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Peer remains alive until the waiting producer emits the acceptance line.
static GROUP_REPORTED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// D4f's initially empty counting semaphore.
static SEMAPHORE: Semaphore = Semaphore::new(0);

/// The waiter advertises that it is about to consume a permit.
static SEMAPHORE_READY: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Set only after the waiter returns from `Semaphore::wait`.
static SEMAPHORE_ACQUIRED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The waiter remains alive until its peer emits the acceptance line.
static SEMAPHORE_REPORTED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Size of the on-stack canary. Large enough to span more than one cache line and to be obviously
/// wrong if two threads shared a stack.
const CANARY: usize = 64;

fn record_serial_digit(digit: usize) -> usize {
    let digit = digit as u32;
    let _ = SERIAL_LOG.fetch_update(
        core::sync::atomic::Ordering::AcqRel,
        core::sync::atomic::Ordering::Acquire,
        |old| Some(old.saturating_mul(10).saturating_add(digit)),
    );
    digit as usize * 10
}

fn record_concurrent_job(value: usize) -> usize {
    CONCURRENT_JOBS.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    value
}

fn triple(value: usize) -> usize {
    value * 3
}

fn initialize_once(value: usize) -> usize {
    ONCE_CALLS.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    ONCE_INIT_STARTED.store(1, core::sync::atomic::Ordering::Release);
    futex_wake(ONCE_INIT_STARTED.as_ptr(), 1);
    loop {
        let observed = ONCE_CONTENDER.load(core::sync::atomic::Ordering::Acquire);
        if observed != 0 {
            break;
        }
        futex_wait(ONCE_CONTENDER.as_ptr(), observed);
    }
    value * 2
}

fn record_group_job(value: usize) -> usize {
    GROUP_SUM.fetch_add(value as u32, core::sync::atomic::Ordering::AcqRel);
    if GROUP.leave().is_err() {
        GROUP_FAILURES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    value
}

fn emit(stdout: Handle, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if stdout.0 == 0 {
        debug_write(bytes.as_ptr(), bytes.len());
    } else {
        let _ = channel_write(stdout, bytes.as_ptr(), bytes.len());
    }
}

fn emit_u32(stdout: Handle, mut v: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    emit(stdout, &buf[i..]);
}

#[no_mangle]
extern "C" fn main(
    index: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    kumo_rt::init();
    // Sora starts thread 0 with x0 = 0 and thread 1 with x0 = 1. Everything below is symmetric.
    let me = (index & 1) as u32;
    let peer = me ^ 1;
    let startup = startup(Handle(0));
    let stdout = startup.stdout.unwrap_or(Handle(0));

    // This array lives on *this thread's* stack. Two threads sharing a stack would corrupt each
    // other's copy the moment one of them slept inside the handshake below.
    let mut canary = [0u8; CANARY];
    for (i, slot) in canary.iter_mut().enumerate() {
        *slot = (i as u8).wrapping_mul(3).wrapping_add(me as u8 * 0x5a);
    }
    // A value that must survive every context switch in a callee-saved register or on the stack.
    let mut carried: u32 = 0x1000_0000 | me;

    for round in 0..ROUNDS {
        // Wait for our turn. Read the word, then sleep *on that value* — if it changed in between,
        // `futex_wait` returns without sleeping and this loop simply re-reads. That is the whole
        // lost-wakeup protocol, and it is why the compare lives inside the syscall.
        loop {
            let turn = TURN.load(core::sync::atomic::Ordering::Acquire);
            if turn == me {
                break;
            }
            futex_wait(TURN.as_ptr(), turn);
        }

        // Awake and holding the turn. Verify nothing of ours was disturbed while we slept.
        for (i, slot) in canary.iter().enumerate() {
            if *slot != (i as u8).wrapping_mul(3).wrapping_add(me as u8 * 0x5a) {
                FAULTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                break;
            }
        }
        if carried != (0x1000_0000 | me) {
            FAULTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        carried = carried.wrapping_add(1).wrapping_sub(1);

        emit(stdout, if me == 0 { b"A" } else { b"B" });
        emit_u32(stdout, round);
        emit(stdout, b" ");
        WROTE[me as usize].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        COMPLETED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

        // Hand the turn over and wake the peer. The store must land before the wake, or the peer
        // can be woken, re-read the old value, and go straight back to sleep.
        TURN.store(peer, core::sync::atomic::Ordering::Release);
        futex_wake(TURN.as_ptr(), 1);
    }

    // Both residents now enter a syscall-free compute phase. The first one here can make no
    // progress until a timer IRQ dispatches its peer, which is the integration edge D3 adds.
    // — KESTREL 2026-07-26
    COMPUTE_READY.fetch_or(1 << me, core::sync::atomic::Ordering::Release);
    while COMPUTE_READY.load(core::sync::atomic::Ordering::Acquire) != 0b11 {
        core::hint::spin_loop();
    }

    let mut last_peer_work =
        COMPUTE_WORK[peer as usize].load(core::sync::atomic::Ordering::Acquire);
    while PEER_EPOCHS[0].load(core::sync::atomic::Ordering::Acquire) < FAIR_EPOCHS
        || PEER_EPOCHS[1].load(core::sync::atomic::Ordering::Acquire) < FAIR_EPOCHS
    {
        COMPUTE_WORK[me as usize].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let peer_work = COMPUTE_WORK[peer as usize].load(core::sync::atomic::Ordering::Acquire);
        if peer_work != last_peer_work {
            PEER_EPOCHS[me as usize].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            last_peer_work = peer_work;
        }
    }

    // The second resident out reports both the blocking D2 proof and compute-bound D3 proof.
    if COMPUTE_DONE.fetch_add(1, core::sync::atomic::Ordering::AcqRel) + 1 == 2 {
        let faults = FAULTS.load(core::sync::atomic::Ordering::Acquire);
        let a = WROTE[0].load(core::sync::atomic::Ordering::Acquire);
        let b = WROTE[1].load(core::sync::atomic::Ordering::Acquire);
        emit(stdout, b"\nthreads: handoffs=");
        emit_u32(
            stdout,
            COMPLETED.load(core::sync::atomic::Ordering::Acquire),
        );
        emit(stdout, b" A=");
        emit_u32(stdout, a);
        emit(stdout, b" B=");
        emit_u32(stdout, b);
        emit(stdout, b" stack+reg faults=");
        emit_u32(stdout, faults);
        if faults == 0 && a == ROUNDS && b == ROUNDS {
            emit(
                stdout,
                b" -> OK: two threads, one address space, futex handoff\n",
            );
        } else {
            emit(stdout, b" -> FAIL\n");
        }

        let work_a = COMPUTE_WORK[0].load(core::sync::atomic::Ordering::Acquire);
        let work_b = COMPUTE_WORK[1].load(core::sync::atomic::Ordering::Acquire);
        let epochs_a = PEER_EPOCHS[0].load(core::sync::atomic::Ordering::Acquire);
        let epochs_b = PEER_EPOCHS[1].load(core::sync::atomic::Ordering::Acquire);
        let min_work = work_a.min(work_b);
        let max_work = work_a.max(work_b);
        let balanced = min_work > 0
            && max_work <= min_work.saturating_mul(2)
            && epochs_a >= FAIR_EPOCHS
            && epochs_b >= FAIR_EPOCHS;
        emit(stdout, b"threads: fair epochs A=");
        emit_u32(stdout, epochs_a);
        emit(stdout, b" B=");
        emit_u32(stdout, epochs_b);
        emit(stdout, b" work A=");
        emit_u32(stdout, work_a);
        emit(stdout, b" B=");
        emit_u32(stdout, work_b);
        emit(
            stdout,
            if balanced {
                b" -> OK: equal-priority compute peers made even progress\n"
            } else {
                b" -> FAIL: unfair compute progress\n"
            },
        );
    }

    // D4a: the caller knows only the queue; the sole worker owns execution. The worker parks on
    // the empty queue, and submit_sync parks the producer until job 4 publishes its result.
    // — KESTREL 2026-07-26
    if me == 1 {
        let worker = match SERIAL_QUEUE.worker() {
            Ok(worker) => worker,
            Err(_) => process_exit(1),
        };
        worker.run_n(4);
        loop {
            let observed = DISPATCH_DONE.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break;
            }
            futex_wait(DISPATCH_DONE.as_ptr(), observed);
        }
    } else {
        let mut failures = 0u32;
        for digit in 1..=3 {
            if SERIAL_QUEUE
                .submit_async(record_serial_digit, digit)
                .is_err()
            {
                failures += 1;
            }
        }
        let sync_result = match SERIAL_QUEUE.submit_sync(record_serial_digit, 4) {
            Ok(result) => result as u32,
            Err(_) => {
                failures += 1;
                0
            }
        };
        let log = SERIAL_LOG.load(core::sync::atomic::Ordering::Acquire);
        let passed = failures == 0 && log == 1234 && sync_result == 40 && SERIAL_QUEUE.is_empty();
        emit(stdout, b"threads: serial async log=");
        emit_u32(stdout, log);
        emit(stdout, b" sync=");
        emit_u32(stdout, sync_result);
        emit(
            stdout,
            if passed {
                b" -> OK: FIFO queue and synchronous completion\n"
            } else {
                b" -> FAIL: serial queue\n"
            },
        );
        DISPATCH_DONE.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(DISPATCH_DONE.as_ptr(), 1);
    }

    // D4b: both resident threads hold independent worker tokens for the same queue. Each executes
    // exactly one item; the worker mask and result sum make a single-worker imitation observable.
    // — KESTREL 2026-07-26
    if me == 0 {
        for value in [10usize, 20] {
            if CONCURRENT_QUEUE
                .submit_async(record_concurrent_job, value)
                .is_err()
            {
                CONCURRENT_FAILURES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    let worker = CONCURRENT_QUEUE.worker();
    CONCURRENT_READY.fetch_or(1 << me, core::sync::atomic::Ordering::Release);
    while CONCURRENT_READY.load(core::sync::atomic::Ordering::Acquire) != 0b11 {
        core::hint::spin_loop();
    }

    let result = worker.run_one() as u32;
    CONCURRENT_SUM.fetch_add(result, core::sync::atomic::Ordering::AcqRel);
    CONCURRENT_WORKERS.fetch_or(1 << me, core::sync::atomic::Ordering::Release);

    if CONCURRENT_DONE.fetch_add(1, core::sync::atomic::Ordering::AcqRel) + 1 == 2 {
        let workers = CONCURRENT_WORKERS.load(core::sync::atomic::Ordering::Acquire);
        let jobs = CONCURRENT_JOBS.load(core::sync::atomic::Ordering::Acquire);
        let sum = CONCURRENT_SUM.load(core::sync::atomic::Ordering::Acquire);
        let failures = CONCURRENT_FAILURES.load(core::sync::atomic::Ordering::Acquire);
        let passed = failures == 0
            && workers == 0b11
            && jobs == 2
            && sum == 30
            && CONCURRENT_QUEUE.is_empty();
        emit(stdout, b"threads: concurrent workers=");
        emit_u32(stdout, workers.count_ones());
        emit(stdout, b" jobs=");
        emit_u32(stdout, jobs);
        emit(stdout, b" sum=");
        emit_u32(stdout, sum);
        emit(
            stdout,
            if passed {
                b" -> OK: two worker contexts drained one queue\n"
            } else {
                b" -> FAIL: concurrent queue\n"
            },
        );
        CONCURRENT_REPORTED.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(CONCURRENT_REPORTED.as_ptr(), 1);
    } else {
        loop {
            let observed = CONCURRENT_REPORTED.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break;
            }
            futex_wait(CONCURRENT_REPORTED.as_ptr(), observed);
        }
    }

    // D4c: thread 0 knows only the concurrent queue. Thread 1 is the remaining execution context;
    // it runs the submitted function, publishes 42 through the shared completion, and wakes the
    // parked caller. — KESTREL 2026-07-26
    if me == 1 {
        CONCURRENT_SYNC_READY.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(CONCURRENT_SYNC_READY.as_ptr(), 1);
        let worker_result = worker.run_one() as u32;
        CONCURRENT_SYNC_WORKER_RESULT.store(worker_result, core::sync::atomic::Ordering::Release);
        futex_wake(CONCURRENT_SYNC_WORKER_RESULT.as_ptr(), 1);
        loop {
            let observed = CONCURRENT_SYNC_REPORTED.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break;
            }
            futex_wait(CONCURRENT_SYNC_REPORTED.as_ptr(), observed);
        }
    } else {
        loop {
            let observed = CONCURRENT_SYNC_READY.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break;
            }
            futex_wait(CONCURRENT_SYNC_READY.as_ptr(), observed);
        }
        let sync_result = CONCURRENT_QUEUE.submit_sync(triple, 14).unwrap_or(0) as u32;
        let worker_result = loop {
            let observed =
                CONCURRENT_SYNC_WORKER_RESULT.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break observed;
            }
            futex_wait(CONCURRENT_SYNC_WORKER_RESULT.as_ptr(), observed);
        };
        let passed = sync_result == 42 && worker_result == 42 && CONCURRENT_QUEUE.is_empty();
        emit(stdout, b"threads: concurrent sync caller=");
        emit_u32(stdout, sync_result);
        emit(stdout, b" worker=");
        emit_u32(stdout, worker_result);
        emit(
            stdout,
            if passed {
                b" -> OK: synchronous completion via peer worker\n"
            } else {
                b" -> FAIL: concurrent synchronous submission\n"
            },
        );
        CONCURRENT_SYNC_REPORTED.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(CONCURRENT_SYNC_REPORTED.as_ptr(), 1);
    }

    // D4d: thread 0 holds the initializer open until thread 1 announces its contention. Thread 1's
    // different context must be ignored; both callers return the first initializer's 42.
    // — KESTREL 2026-07-26
    let once_result = if me == 0 {
        ONCE.call_once(initialize_once, 21)
    } else {
        loop {
            let observed = ONCE_INIT_STARTED.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break;
            }
            futex_wait(ONCE_INIT_STARTED.as_ptr(), observed);
        }
        ONCE_CONTENDER.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(ONCE_CONTENDER.as_ptr(), 1);
        ONCE.call_once(initialize_once, 999)
    } as u32;
    ONCE_RESULTS[me as usize].store(once_result, core::sync::atomic::Ordering::Release);

    if ONCE_DONE.fetch_add(1, core::sync::atomic::Ordering::AcqRel) + 1 == 2 {
        let calls = ONCE_CALLS.load(core::sync::atomic::Ordering::Acquire);
        let result_a = ONCE_RESULTS[0].load(core::sync::atomic::Ordering::Acquire);
        let result_b = ONCE_RESULTS[1].load(core::sync::atomic::Ordering::Acquire);
        let passed = calls == 1 && result_a == 42 && result_b == 42 && ONCE.is_completed();
        emit(stdout, b"threads: once calls=");
        emit_u32(stdout, calls);
        emit(stdout, b" A=");
        emit_u32(stdout, result_a);
        emit(stdout, b" B=");
        emit_u32(stdout, result_b);
        emit(
            stdout,
            if passed {
                b" -> OK: one initializer, shared result\n"
            } else {
                b" -> FAIL: once gate\n"
            },
        );
        ONCE_REPORTED.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(ONCE_REPORTED.as_ptr(), 1);
    } else {
        loop {
            let observed = ONCE_REPORTED.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break;
            }
            futex_wait(ONCE_REPORTED.as_ptr(), observed);
        }
    }

    // D4e: thread 0 registers two jobs before publishing them, then parks in Group::wait. Thread 1
    // drains both queue items; only its second leave completes the generation and wakes thread 0.
    // — KESTREL 2026-07-26
    if me == 0 {
        let mut queued = 0u32;
        for value in [10usize, 20] {
            if GROUP.enter().is_err() {
                GROUP_FAILURES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                continue;
            }
            if CONCURRENT_QUEUE
                .submit_async(record_group_job, value)
                .is_err()
            {
                GROUP_FAILURES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                let _ = GROUP.leave();
            } else {
                queued += 1;
            }
        }
        GROUP_QUEUED.store(queued, core::sync::atomic::Ordering::Release);
        GROUP_READY.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(GROUP_READY.as_ptr(), 1);

        GROUP.wait();
        let sum = GROUP_SUM.load(core::sync::atomic::Ordering::Acquire);
        let pending = GROUP.pending();
        let failures = GROUP_FAILURES.load(core::sync::atomic::Ordering::Acquire);
        let passed = queued == 2
            && sum == 30
            && pending == 0
            && failures == 0
            && CONCURRENT_QUEUE.is_empty();
        emit(stdout, b"threads: group jobs=");
        emit_u32(stdout, queued);
        emit(stdout, b" sum=");
        emit_u32(stdout, sum);
        emit(stdout, b" pending=");
        emit_u32(stdout, pending);
        emit(
            stdout,
            if passed {
                b" -> OK: wait resumed after all work\n"
            } else {
                b" -> FAIL: group completion\n"
            },
        );
        GROUP_REPORTED.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(GROUP_REPORTED.as_ptr(), 1);
    } else {
        loop {
            let observed = GROUP_READY.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break;
            }
            futex_wait(GROUP_READY.as_ptr(), observed);
        }
        let queued = GROUP_QUEUED.load(core::sync::atomic::Ordering::Acquire);
        for _ in 0..queued {
            worker.run_one();
        }
        loop {
            let observed = GROUP_REPORTED.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break;
            }
            futex_wait(GROUP_REPORTED.as_ptr(), observed);
        }
    }

    // D4f: thread 0 announces its imminent zero-permit wait and parks. Thread 1 observes that no
    // acquisition happened early, signals exactly once, and requires the resumed waiter to consume
    // that sole retained permit. — KESTREL 2026-07-26
    if me == 0 {
        SEMAPHORE_READY.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(SEMAPHORE_READY.as_ptr(), 1);
        SEMAPHORE.wait();
        SEMAPHORE_ACQUIRED.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(SEMAPHORE_ACQUIRED.as_ptr(), 1);
        loop {
            let observed = SEMAPHORE_REPORTED.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break;
            }
            futex_wait(SEMAPHORE_REPORTED.as_ptr(), observed);
        }
    } else {
        loop {
            let observed = SEMAPHORE_READY.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break;
            }
            futex_wait(SEMAPHORE_READY.as_ptr(), observed);
        }
        let before = SEMAPHORE_ACQUIRED.load(core::sync::atomic::Ordering::Acquire);
        let signaled = SEMAPHORE.signal().is_ok();
        let acquired = loop {
            let observed = SEMAPHORE_ACQUIRED.load(core::sync::atomic::Ordering::Acquire);
            if observed != 0 {
                break observed;
            }
            futex_wait(SEMAPHORE_ACQUIRED.as_ptr(), observed);
        };
        let permits = SEMAPHORE.available();
        let passed = before == 0 && signaled && acquired == 1 && permits == 0;
        emit(stdout, b"threads: semaphore before=");
        emit_u32(stdout, before);
        emit(stdout, b" acquired=");
        emit_u32(stdout, acquired);
        emit(stdout, b" permits=");
        emit_u32(stdout, permits);
        emit(
            stdout,
            if passed {
                b" -> OK: one signal released one waiter\n"
            } else {
                b" -> FAIL: semaphore handoff\n"
            },
        );
        SEMAPHORE_REPORTED.store(1, core::sync::atomic::Ordering::Release);
        futex_wake(SEMAPHORE_REPORTED.as_ptr(), 1);
    }

    process_exit(0)
}
