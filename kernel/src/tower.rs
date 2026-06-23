//! TOWER — the kernel's loud disaster channel.
//!
//! The rule (system-wide): **no part of KUMO — not even a microservice — may fail
//! silently.** When a process dies abnormally (an EL0 CPU fault, or a non-zero exit) the
//! kernel raises a TOWER. The name is the tarot card XVI: catastrophe, the sudden ruin
//! that cannot be hidden. There are two faces of it, matching the card's two orientations:
//!
//! - **TOWER (upright)** — a *system-critical* death. The supervisor root (Sora) is gone,
//!   or a fault landed before any supervisor exists, so the kernel cannot recover. We log
//!   the event, emit the banner, and **halt + repaint forever** so the report can never
//!   scroll away (the X13s has no serial; a lossy HDMI capture drops the bottom band, so a
//!   one-shot print would be lost — see `kumo_exception_entry`).
//!
//! - **TOWER reversed (inverted)** — a *restartable microservice* died (a driver, a Siyu
//!   server: "stateless / soft-state" in DESIGN/002 §3). Reversed Tower is disaster
//!   *contained / averted*: we log the event and emit the banner just as loudly, but the
//!   kernel keeps running. The victim is reaped (its `TERMINATED` signal fires) and the
//!   supervisor restarts it (DESIGN/002 §1–2). Containment, never silence.
//!
//! Every TOWER — upright or reversed — is appended to the in-kernel [`Ledger`]. Today that
//! is a small RAM ring so a reader (a future syscall, or the next boot's crash-dump) can
//! recover the most recent events. **SEAM:** once `efs` (the no_std ext2 crate at
//! `resources/efs`) backs a read/write `/var`, [`record`] is where each event is flushed to
//! a durable on-disk ledger. The signature is built for that: events are POD and carry a
//! monotonic `seq` so the on-disk log can be appended to without coordination.
//!
//! Fault-context discipline: this module never allocates, never takes a lock, and never
//! switches threads. It formats with [`hex64`] (stack-only) and writes through a caller-
//! supplied `emit: fn(&[u8])` so the caller can pick the framebuffer-epoch-correct console
//! path (a faulting renderer's glass must be reclaimed *before* emitting).

use core::cell::UnsafeCell;
use kumo_abi::KoId;

/// Why a process died — the payload of a TOWER event.
#[derive(Clone, Copy)]
pub enum Cause {
    /// An EL0 synchronous CPU fault (bad memory access, illegal instruction, …),
    /// carrying the AArch64 syndrome registers: `ESR` (exception class + fault status),
    /// `ELR` (faulting instruction VA), `FAR` (faulting data address).
    Fault { esr: u64, elr: u64, far: u64 },
    /// An abnormal process exit: a `ProcessExit` / `exit_group` with a non-zero code.
    /// A clean `exit(0)` is *not* a disaster and never reaches here.
    Abend { exit_code: u64 },
}

/// Which face of the Tower a death wears. See the module docs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    /// Upright Tower: system-critical, unrecoverable → halt + repaint forever.
    Critical,
    /// Reversed Tower: a restartable microservice → loud, logged, but contained.
    Inverted,
}

/// One disaster, as recorded in the [`Ledger`]. POD + `Copy` so it can be flushed to the
/// future on-disk ledger verbatim.
#[derive(Clone, Copy)]
pub struct Event {
    /// Monotonic sequence number (never repeats, even after the RAM ring wraps).
    pub seq: u64,
    pub severity: Severity,
    pub cause: Cause,
    /// The dead process's koid, if the kernel could identify it. `None` means the fault
    /// landed with no current process (very early boot) — always treated as critical.
    pub victim: Option<KoId>,
    /// True if the victim owned the framebuffer (so the glass was reclaimed to render this).
    pub fb_owner: bool,
}

/// Decide which face of the Tower a death wears. The supervisor root dying — or any death
/// before a supervisor exists — is unrecoverable (`Critical`); every other process is a
/// restartable microservice (`Inverted`). When `relaunch_sora` (DESIGN/002 §4) lands, a
/// Sora death can downgrade from `Critical` to a logged relaunch; until then, halting loud
/// is the honest behavior — the userspace plane is gone.
pub fn classify(victim: Option<KoId>, sora_koid: Option<KoId>) -> Severity {
    match (victim, sora_koid) {
        (Some(v), Some(s)) if v == s => Severity::Critical, // supervisor root lost
        (Some(_), Some(_)) => Severity::Inverted,           // restartable microservice
        _ => Severity::Critical,                            // no victim id, or no supervisor yet
    }
}

/// Raise a TOWER for a dying process.
///
/// Records the event to the [`Ledger`], then renders the banner through `emit` (the
/// caller's framebuffer-epoch-correct console writer). Behavior diverges by severity:
/// - [`Severity::Critical`] → **never returns**: it halts and repaints the banner forever.
/// - [`Severity::Inverted`] → **returns**, so the caller proceeds with contained teardown
///   (reap the victim, signal `TERMINATED`, let the supervisor restart it).
pub fn raise(
    cause: Cause,
    victim: Option<KoId>,
    fb_owner: bool,
    sora_koid: Option<KoId>,
    emit: fn(&[u8]),
) {
    match classify(victim, sora_koid) {
        Severity::Critical => critical(cause, victim, fb_owner, emit),
        Severity::Inverted => raise_inverted(cause, victim, fb_owner, emit),
    }
}

/// Raise a reversed (contained) TOWER unconditionally — for a victim already known to be a
/// restartable microservice (the caller has ruled out the supervisor root). Never halts;
/// always returns. Use this on paths that cannot consult [`classify`] because the supervisor
/// koid is unavailable — notably the Linux-persona child dispatcher, which runs *inside* a
/// `SoraState` borrow and is reached only for a child (`cp_koid != Sora`).
pub fn raise_inverted(cause: Cause, victim: Option<KoId>, fb_owner: bool, emit: fn(&[u8])) {
    let ev = record(Severity::Inverted, cause, victim, fb_owner);
    emit_banner(&ev, None, emit);
}

/// The upright-Tower path: log, then halt and repaint the banner forever. Never returns.
fn critical(cause: Cause, victim: Option<KoId>, fb_owner: bool, emit: fn(&[u8])) -> ! {
    let ev = record(Severity::Critical, cause, victim, fb_owner);
    let mut pass: u64 = 0;
    loop {
        emit_banner(&ev, Some(pass), emit);
        pass = pass.wrapping_add(1);
        // Crude spin so each repaint lingers across several capture frames before the next
        // scrolls it up; no timer/IRQ is safe to touch from a fault/halt path. Mirrors the
        // EL1 `kumo_exception_entry` cadence.
        for _ in 0..500_000_000u64 {
            core::hint::spin_loop();
        }
    }
}

/// Render one copy of the TOWER banner via `emit`. `pass` is `Some(n)` for the upright
/// repaint loop (shows the repaint counter), `None` for the one-shot reversed banner.
fn emit_banner(ev: &Event, pass: Option<u64>, emit: fn(&[u8])) {
    match ev.severity {
        Severity::Critical => {
            emit(b"\r\n********** TOWER: SYSTEM-CRITICAL PROCESS DEATH **********\r\n");
        }
        Severity::Inverted => {
            emit(b"\r\n~~~~~~~~~~ TOWER (reversed): MICROSERVICE CONTAINED ~~~~~~~~~~\r\n");
        }
    }

    emit(b"  victim koid=");
    match ev.victim {
        Some(k) => emit(&hex64(k.0)),
        None => emit(b"<none>            "),
    }
    emit(if ev.fb_owner {
        b"  fb-owner=yes\r\n"
    } else {
        b"  fb-owner=no\r\n"
    });

    match ev.cause {
        Cause::Fault { esr, elr, far } => {
            emit(b"  cause=FAULT  ESR=");
            emit(&hex64(esr));
            emit(b" ELR=");
            emit(&hex64(elr));
            emit(b" FAR=");
            emit(&hex64(far));
            emit(b"\r\n");
        }
        Cause::Abend { exit_code } => {
            emit(b"  cause=ABEND  exit=");
            emit(&hex64(exit_code));
            emit(b"\r\n");
        }
    }

    emit(b"  event seq=");
    emit(&hex64(ev.seq));
    match ev.severity {
        Severity::Critical => {
            emit(b" logged - supervisor root lost; HALT (repaint=");
            emit(&hex64(pass.unwrap_or(0)));
            emit(b")\r\n");
            emit(b"**********************************************************\r\n\r\n\r\n\r\n\r\n");
        }
        Severity::Inverted => {
            emit(b" logged - reaped; supervisor will restart (DESIGN/002)\r\n");
            emit(b"~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~\r\n");
        }
    }
}

// ---------------------------------------------------------------------------------------
// Ledger — the durable-by-design record of every TOWER.
// ---------------------------------------------------------------------------------------

/// How many of the most recent events the RAM ring retains. Small: this is a crash trail,
/// not a metrics store. The durable ledger (efs, future) has no such bound.
const LEDGER_CAP: usize = 32;

struct Ledger {
    /// Ring of the most recent `LEDGER_CAP` events; index `seq % LEDGER_CAP`.
    events: [Option<Event>; LEDGER_CAP],
    /// Total count of events ever recorded (also the next `seq`). Monotonic; the ring may
    /// wrap but `seq` never does (until u64 overflow, which is not a real concern).
    next: u64,
}

struct LedgerCell(UnsafeCell<Ledger>);
// Single-core, cooperative kernel: TOWER events are recorded from fault/exit paths that do
// not run concurrently with each other. Same discipline as `SoraCell`/`UserSchedCell`.
unsafe impl Sync for LedgerCell {}

static LEDGER: LedgerCell = LedgerCell(UnsafeCell::new(Ledger {
    events: [None; LEDGER_CAP],
    next: 0,
}));

/// Append a TOWER event to the ledger and return the stamped record.
///
/// **SEAM (efs ledger):** this is the single chokepoint every TOWER passes through. When a
/// read/write `/var` exists (efs, `resources/efs`), flush `ev` to a durable append-only log
/// here — the RAM ring then becomes a write-back cache for the most recent entries.
fn record(severity: Severity, cause: Cause, victim: Option<KoId>, fb_owner: bool) -> Event {
    // SAFETY: single-core; no other code holds a reference to LEDGER across this call.
    let l = unsafe { &mut *LEDGER.0.get() };
    let seq = l.next;
    l.next = l.next.wrapping_add(1);
    let ev = Event {
        seq,
        severity,
        cause,
        victim,
        fb_owner,
    };
    l.events[(seq as usize) % LEDGER_CAP] = Some(ev);
    ev
}

/// Total number of TOWERs raised this boot. (A non-zero count after boot means something
/// died — a future health probe / `dmesg`-style syscall can surface it.)
pub fn count() -> u64 {
    // SAFETY: single-core read of a `Copy` scalar.
    unsafe { (*LEDGER.0.get()).next }
}

/// Visit the retained events oldest-first (up to the most recent `LEDGER_CAP`). The reader
/// for a future crash-dump syscall / on-disk flush.
pub fn for_each_recent<F: FnMut(&Event)>(mut f: F) {
    // SAFETY: single-core; the visitor must not itself raise a TOWER (no re-entrancy).
    let l = unsafe { &*LEDGER.0.get() };
    let total = l.next;
    let start = total.saturating_sub(LEDGER_CAP as u64);
    let mut s = start;
    while s < total {
        if let Some(ev) = &l.events[(s as usize) % LEDGER_CAP] {
            f(ev);
        }
        s += 1;
    }
}

/// Format a `u64` as `0x` + 16 lowercase hex digits with no allocation — for the TOWER
/// banner, which runs in a fault/halt context where `klog!` formatting is unsafe.
pub fn hex64(v: u64) -> [u8; 18] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [b'0', b'x', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut i = 0;
    while i < 16 {
        out[2 + i] = HEX[((v >> (60 - i * 4)) & 0xf) as usize];
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_supervisor_death_is_critical() {
        let sora = Some(KoId(7));
        assert_eq!(classify(Some(KoId(7)), sora), Severity::Critical);
    }

    #[test]
    fn classify_microservice_death_is_inverted() {
        let sora = Some(KoId(7));
        assert_eq!(classify(Some(KoId(42)), sora), Severity::Inverted);
    }

    #[test]
    fn classify_presupervisor_fault_is_critical() {
        // No supervisor yet (early boot) → nothing can restart it → critical.
        assert_eq!(classify(Some(KoId(42)), None), Severity::Critical);
        assert_eq!(classify(None, None), Severity::Critical);
    }

    #[test]
    fn hex64_pads_and_lowercases() {
        assert_eq!(&hex64(0xdead_0000), b"0x00000000dead0000");
        assert_eq!(&hex64(0), b"0x0000000000000000");
    }

    #[test]
    fn ledger_records_and_visits_in_order() {
        // Note: shares the process-global LEDGER; assert on relative ordering, not absolutes.
        let before = count();
        record(
            Severity::Inverted,
            Cause::Abend { exit_code: 1 },
            Some(KoId(1)),
            false,
        );
        record(
            Severity::Critical,
            Cause::Fault {
                esr: 2,
                elr: 3,
                far: 4,
            },
            Some(KoId(2)),
            true,
        );
        assert_eq!(count(), before + 2);
        let mut seqs = alloc::vec::Vec::new();
        for_each_recent(|ev| seqs.push(ev.seq));
        // Visited oldest-first and strictly increasing.
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
        assert!(seqs.contains(&before));
    }
}
