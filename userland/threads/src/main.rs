#![no_std]
#![no_main]
//j493
//j494
//j495

//! `threads` — the D2/D3/D4 proof: futex peers, fair compute, then a serial work queue.
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

use kumo_abi::Handle;
use kumo_dispatch::SerialQueue;
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

    process_exit(0)
}
