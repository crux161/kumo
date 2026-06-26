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

use crate::task::ProcessLabel;

/// Why a process died — the payload of a TOWER event.
#[derive(Clone, Copy)]
pub enum Cause {
    /// An EL0 synchronous CPU fault (bad memory access, illegal instruction, …),
    /// carrying the AArch64 syndrome registers: `ESR` (exception class + fault status),
    /// `ELR` (faulting instruction VA), `FAR` (faulting data address), `LR` (the EL0
    /// `x30` return address — the *caller* of the faulting function, which pins the
    /// callsite when `ELR` lands inside a shared leaf like `memcmp`), and `SP` (the EL0
    /// stack pointer — a wild value flags stack corruption as the fault's true source,
    /// independent of where `ELR`/`FAR` happen to land).
    /// Plus a few EL0 GP registers captured from the trap frame: `x0`/`x1` (the first
    /// arg / sret pointer — the value a faulting `str [xN,#off]` is using), `x19` (a common
    /// callee-saved pointer holding `self`/sret across calls), and `x29` (the frame pointer,
    /// to sanity-check the stack). These turn "x0 must be 0x9 (inferred from FAR-offset)"
    /// into a directly-observed value.
    Fault {
        esr: u64,
        elr: u64,
        far: u64,
        lr: u64,
        sp: u64,
        x0: u64,
        x1: u64,
        x19: u64,
        x29: u64,
    },
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
    /// Best-effort diagnostic process label copied at process creation. It grants no authority;
    /// it only makes metal QR captures name the dying service. — KESTREL
    pub victim_label: ProcessLabel,
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
    victim_label: ProcessLabel,
    fb_owner: bool,
    sora_koid: Option<KoId>,
    emit: fn(&[u8]),
) {
    match classify(victim, sora_koid) {
        Severity::Critical => critical(cause, victim, victim_label, fb_owner, emit),
        Severity::Inverted => raise_inverted(cause, victim, victim_label, fb_owner, emit),
    }
}

/// Raise a reversed (contained) TOWER unconditionally — for a victim already known to be a
/// restartable microservice (the caller has ruled out the supervisor root). Never halts;
/// always returns. Use this on paths that cannot consult [`classify`] because the supervisor
/// koid is unavailable — notably the Linux-persona child dispatcher, which runs *inside* a
/// `SoraState` borrow and is reached only for a child (`cp_koid != Sora`).
pub fn raise_inverted(
    cause: Cause,
    victim: Option<KoId>,
    victim_label: ProcessLabel,
    fb_owner: bool,
    emit: fn(&[u8]),
) {
    let ev = record(Severity::Inverted, cause, victim, victim_label, fb_owner);
    emit_banner(&ev, None, emit);
    // Contained path: one banner, one QR, then return. Unlike the critical halt-loop,
    // this must not arm the QR emissary replay latch: a live system's later console
    // output should be able to reclaim the screen after the failure has been made loud.
    emit_qr(&ev, emit, false);
}

/// The upright-Tower path: log, then halt and repaint the banner forever. Never returns.
fn critical(
    cause: Cause,
    victim: Option<KoId>,
    victim_label: ProcessLabel,
    fb_owner: bool,
    emit: fn(&[u8]),
) -> ! {
    let ev = record(Severity::Critical, cause, victim, victim_label, fb_owner);
    let mut pass: u64 = 0;
    loop {
        emit_critical_frame(&ev, pass, emit);
        pass = pass.wrapping_add(1);
        // Crude spin so each repaint lingers across several capture frames before the next
        // scrolls it up; no timer/IRQ is safe to touch from a fault/halt path. Mirrors the
        // EL1 `kumo_exception_entry` cadence.
        for _ in 0..500_000_000u64 {
            core::hint::spin_loop();
        }
    }
}

/// Render one halt-loop frame. The QR is deliberately emitted last so any banner repaint that
/// overlaps its pixels is repaired before the frame lingers for capture.
fn emit_critical_frame(ev: &Event, pass: u64, emit: fn(&[u8])) {
    emit_banner(ev, Some(pass), emit);
    emit_qr(ev, emit, qr_sticky_for(ev.severity));
}

fn qr_sticky_for(severity: Severity) -> bool {
    matches!(severity, Severity::Critical)
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
    emit(b"  label=");
    emit_label(ev.victim_label, emit);
    emit(if ev.fb_owner {
        b"  fb-owner=yes\r\n"
    } else {
        b"  fb-owner=no\r\n"
    });

    match ev.cause {
        Cause::Fault {
            esr,
            elr,
            far,
            lr,
            sp,
            x0,
            x1,
            x19,
            x29,
        } => {
            emit(b"  cause=FAULT  ESR=");
            emit(&hex64(esr));
            emit(b" ELR=");
            emit(&hex64(elr));
            emit(b" FAR=");
            emit(&hex64(far));
            emit(b" LR=");
            emit(&hex64(lr));
            emit(b" SP=");
            emit(&hex64(sp));
            emit(b"\r\n  x0=");
            emit(&hex64(x0));
            emit(b" x1=");
            emit(&hex64(x1));
            emit(b" x19=");
            emit(&hex64(x19));
            emit(b" x29=");
            emit(&hex64(x29));
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

/// Max bytes of the QR diagnostic payload. The largest event (a `Fault` with all 9 registers)
/// is ~260 bytes of `label=0x…hex`; 512 leaves generous headroom. Stack-only — no alloc, in
/// keeping with the fault-context discipline (the QR encoder itself runs in the HAL, off this
/// frame).
const QR_PAYLOAD_CAP: usize = 512;

/// Build a compact ASCII dump of the event into a stack buffer and hand it to the HAL to render
/// as an on-screen QR (top-right of the framebuffer). Same fields as the text banner, so a
/// single phone photo of the panic screen decodes to the *exact* dump — no hand-transcribing
/// blurry green zeros (the X13s has no usable serial). No-op where there is no framebuffer
/// (the HAL gates on that). Best-effort: the payload is silently truncated if it ever overflows,
/// and the text banner — emitted first — is always the reliable channel.
fn emit_qr(ev: &Event, emit: fn(&[u8]), sticky: bool) {
    struct Buf {
        bytes: [u8; QR_PAYLOAD_CAP],
        len: usize,
    }
    impl Buf {
        fn push(&mut self, s: &[u8]) {
            let n = s.len().min(QR_PAYLOAD_CAP - self.len);
            self.bytes[self.len..self.len + n].copy_from_slice(&s[..n]);
            self.len += n;
        }
    }
    let mut b = Buf {
        bytes: [0; QR_PAYLOAD_CAP],
        len: 0,
    };

    // Framebuffer geometry: print it in the (reliable) text banner *and* fold it into the QR,
    // so even a boot where the QR lands off-screen still reveals the real panel dimensions —
    // ground truth for tuning placement. Hex to reuse the no-alloc `hex64`.
    let (fb_w, fb_h, fb_stride) = kumo_hal::active::fb_geometry();
    emit(b"  fbgeom w=");
    emit(&hex64(fb_w as u64));
    emit(b" h=");
    emit(&hex64(fb_h as u64));
    emit(b" stride=");
    emit(&hex64(fb_stride as u64));
    emit(b"\r\n");

    b.push(b"KUMO TOWER\n");
    b.push(b"fbgeom w=");
    b.push(&hex64(fb_w as u64));
    b.push(b" h=");
    b.push(&hex64(fb_h as u64));
    b.push(b" stride=");
    b.push(&hex64(fb_stride as u64));
    b.push(b"\n");
    b.push(match ev.severity {
        Severity::Critical => b"sev=CRITICAL\n" as &[u8],
        Severity::Inverted => b"sev=INVERTED\n",
    });
    b.push(b"koid=");
    match ev.victim {
        Some(k) => b.push(&hex64(k.0)),
        None => b.push(b"<none>"),
    }
    b.push(b" label=");
    if ev.victim_label.is_empty() {
        b.push(b"<unlabeled>");
    } else {
        b.push(ev.victim_label.as_bytes());
    }
    b.push(if ev.fb_owner {
        b" fbowner=1\n"
    } else {
        b" fbowner=0\n"
    });

    match ev.cause {
        Cause::Fault {
            esr,
            elr,
            far,
            lr,
            sp,
            x0,
            x1,
            x19,
            x29,
        } => {
            b.push(b"FAULT\nesr=");
            b.push(&hex64(esr));
            b.push(b" elr=");
            b.push(&hex64(elr));
            b.push(b" far=");
            b.push(&hex64(far));
            b.push(b"\nlr=");
            b.push(&hex64(lr));
            b.push(b" sp=");
            b.push(&hex64(sp));
            b.push(b"\nx0=");
            b.push(&hex64(x0));
            b.push(b" x1=");
            b.push(&hex64(x1));
            b.push(b"\nx19=");
            b.push(&hex64(x19));
            b.push(b" x29=");
            b.push(&hex64(x29));
            b.push(b"\n");
        }
        Cause::Abend { exit_code } => {
            b.push(b"ABEND exit=");
            b.push(&hex64(exit_code));
            b.push(b"\n");
        }
    }
    b.push(b"seq=");
    b.push(&hex64(ev.seq));

    if sticky {
        kumo_hal::active::render_qr_diag(&b.bytes[..b.len]);
    } else {
        kumo_hal::active::render_qr_diag_once(&b.bytes[..b.len]);
    }
}

fn emit_label(label: ProcessLabel, emit: fn(&[u8])) {
    if label.is_empty() {
        emit(b"<unlabeled>");
    } else {
        emit(label.as_bytes());
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
fn record(
    severity: Severity,
    cause: Cause,
    victim: Option<KoId>,
    victim_label: ProcessLabel,
    fb_owner: bool,
) -> Event {
    // SAFETY: single-core; no other code holds a reference to LEDGER across this call.
    let l = unsafe { &mut *LEDGER.0.get() };
    let seq = l.next;
    l.next = l.next.wrapping_add(1);
    let ev = Event {
        seq,
        severity,
        cause,
        victim,
        victim_label,
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
            ProcessLabel::from_bytes(b"drv-i2c-hid"),
            false,
        );
        record(
            Severity::Critical,
            Cause::Fault {
                esr: 2,
                elr: 3,
                far: 4,
                lr: 5,
                sp: 6,
                x0: 7,
                x1: 8,
                x19: 9,
                x29: 10,
            },
            Some(KoId(2)),
            ProcessLabel::from_bytes(b"ttyd"),
            true,
        );
        assert_eq!(count(), before + 2);
        let mut seqs = alloc::vec::Vec::new();
        for_each_recent(|ev| seqs.push(ev.seq));
        // Visited oldest-first and strictly increasing.
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
        assert!(seqs.contains(&before));
    }

    #[test]
    fn critical_frame_emits_qr_after_banner() {
        use std::sync::Mutex;

        static EMIT_BUF: Mutex<alloc::vec::Vec<u8>> = Mutex::new(alloc::vec::Vec::new());

        fn capture_emit(bytes: &[u8]) {
            EMIT_BUF.lock().unwrap().extend_from_slice(bytes);
        }

        let ev = Event {
            seq: 0,
            severity: Severity::Critical,
            cause: Cause::Abend { exit_code: 0x2a },
            victim: Some(KoId(2)),
            victim_label: ProcessLabel::from_bytes(b"drv-i2c-hid"),
            fb_owner: true,
        };

        {
            let mut buf = EMIT_BUF.lock().unwrap();
            buf.clear();
        }
        emit_critical_frame(&ev, 3, capture_emit);
        let buf = EMIT_BUF.lock().unwrap();

        let banner = find_bytes(&buf, b"TOWER: SYSTEM-CRITICAL PROCESS DEATH").unwrap();
        let label = find_bytes(&buf, b"label=drv-i2c-hid").unwrap();
        let repaint = find_bytes(&buf, b"HALT (repaint=0x0000000000000003)").unwrap();
        let qr_diag = find_bytes(&buf, b"fbgeom w=").unwrap();

        assert!(banner < label);
        assert!(label < repaint);
        assert!(repaint < qr_diag);
    }

    #[test]
    fn qr_replay_latch_is_only_for_critical_tower() {
        assert!(qr_sticky_for(Severity::Critical));
        assert!(!qr_sticky_for(Severity::Inverted));
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
}
