#![no_std]
#![no_main]
//j493

//! `threads` — the D2 proof: two threads, one address space, handing off through a futex.
//!
//! Run it from the shell with `threads`. Sora loads this one ELF, maps it once, gives each thread
//! its **own** stack, and starts two residents in the same process — so both threads execute the
//! code below, distinguished only by the index in `x0`.
//!
//! What each part is actually proving, because "hello from thread A" would prove almost none of it:
//!
//! - **Two threads really run.** The output interleaves. One thread printing twice in a row would
//!   mean the other never started.
//! - **They share an address space.** `TURN` and `LOG` are ordinary statics in this image's `.bss`.
//!   The image is mapped once; if the two residents did not share it, they would be incrementing
//!   different words and the handshake could never complete.
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
//! The exit line reports every check, so a partial failure is legible rather than a hang.

use kumo_abi::Handle;
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

/// Size of the on-stack canary. Large enough to span more than one cache line and to be obviously
/// wrong if two threads shared a stack.
const CANARY: usize = 64;

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

    // Thread 1 finishes last in a correct run; whichever thread sees the full count reports.
    if COMPLETED.load(core::sync::atomic::Ordering::Acquire) >= ROUNDS * 2 {
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
    }

    process_exit(0)
}
