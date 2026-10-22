//! Console input, drained from interrupt context.
//!
//! The escape hatch's first version lived in the boot floor's REPL loop, and it did not work —
//! because the boot floor is exactly what stops running. `irq_handoff_allowed` is `current == user`
//! (see `user_thread`), so a **child** at EL0 is never preempted: `run persona-linux-hello` spins,
//! and the CPU never returns to Sora, to the boot floor, or to anything that could read a key.
//!
//! One thing does keep running: the timer interrupt. The GIC delivers, `signal_irq` executes, and
//! control returns to the child. So the last piece of the machine still able to notice a keystroke
//! is the IRQ handler — which is precisely why Linux services SysRq from interrupt context too. An
//! escape hatch has to live below everything it might need to escape.
//!
//! So the UART is drained here, on every interrupt, and there is exactly **one** reader. Bytes that
//! are not part of an escape sequence go into a ring the REPL loop drains at its leisure, which
//! keeps ordinary typing working and, as a side effect, stops the shell's input being a pure poll.
//!
//! What may run in this context is tightly bounded: no allocation, and never the panicking
//! `SoraState` borrow (j484's freeze). The power actions qualify — they quiesce through the
//! non-panicking borrow and print through the console fallback — and they never return, so the
//! interrupted child is simply never resumed. Anything heavier is deferred to the REPL loop, which
//! is a best-effort path by definition: if the floor is wedged, only the bounded actions fire, and
//! those are the ones that matter.

//j536

use core::sync::atomic::{AtomicUsize, Ordering};

/// Bytes buffered between the interrupt that read them and the REPL loop that consumes them.
/// A human types a few bytes per tick at most; this is slack, not a design constraint.
pub const RING_BYTES: usize = 256;

/// A single-producer single-consumer byte ring: the IRQ handler pushes, the boot floor pops.
///
/// Head and tail are separate atomics so neither side needs a lock — the producer only advances
/// `tail`, the consumer only advances `head`, and a full ring drops the newest byte rather than
/// overwriting one the consumer has not seen.
pub struct Ring {
    buf: [core::cell::UnsafeCell<u8>; RING_BYTES],
    head: AtomicUsize,
    tail: AtomicUsize,
}

// SAFETY: single core, and the two ends touch disjoint slots — the producer writes only at `tail`
// and the consumer reads only at `head`, with the index stores ordering the byte accesses.
unsafe impl Sync for Ring {}

impl Ring {
    #[allow(clippy::declare_interior_mutable_const)]
    const SLOT: core::cell::UnsafeCell<u8> = core::cell::UnsafeCell::new(0);

    pub const fn new() -> Self {
        Self {
            buf: [Self::SLOT; RING_BYTES],
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Producer side. Returns false when the ring is full and the byte was dropped.
    pub fn push(&self, byte: u8) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let next = (tail + 1) % RING_BYTES;
        if next == self.head.load(Ordering::Acquire) {
            return false;
        }
        unsafe { *self.buf[tail].get() = byte };
        self.tail.store(next, Ordering::Release);
        true
    }

    /// Consumer side.
    pub fn pop(&self) -> Option<u8> {
        let head = self.head.load(Ordering::Relaxed);
        if head == self.tail.load(Ordering::Acquire) {
            return None;
        }
        let byte = unsafe { *self.buf[head].get() };
        self.head.store((head + 1) % RING_BYTES, Ordering::Release);
        Some(byte)
    }

    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Acquire)
    }
}

static RING: Ring = Ring::new();

/// The escape recogniser, shared by both contexts.
///
/// One instance, not two: a prefix noticed in the IRQ handler and a command byte arriving at the
/// REPL loop (or the reverse) must still resolve. Two recognisers would each see half a sequence
/// and neither would fire.
static SYSRQ: crate::sysrq::SysrqCell = crate::sysrq::SysrqCell::new();

/// Largest number of bytes drained from the UART in one interrupt, so a stuck-high receiver cannot
/// hold the handler indefinitely.
const DRAIN_BUDGET: usize = 16;

/// Drain the UART from interrupt context: recognise escape sequences, queue everything else.
///
/// Returns any action that the caller should perform. Kept separate from performing it so the
/// bounded-context rules stay legible at the call site.
#[must_use]
pub fn poll_from_irq() -> Option<crate::sysrq::Action> {
    let mut action = None;
    for _ in 0..DRAIN_BUDGET {
        let Some(byte) = kumo_hal::active::console_read_byte() else {
            break;
        };
        let (consumed, found) = SYSRQ.feed(byte);
        if found.is_some() {
            action = found;
        }
        if !consumed {
            // A dropped byte is a lost keystroke, which is preferable to overwriting one the shell
            // has not read yet — and the ring is far larger than any human's burst.
            let _ = RING.push(byte);
        }
        if let Some(found) = action {
            if !safe_in_irq(found) {
                defer(found);
                action = None;
            }
            break;
        }
    }
    action
}

/// Actions too heavy for interrupt context, handed to the boot floor instead.
///
/// `Tasks` walks the scheduler and allocates a `Vec`; that is fine on the floor and unacceptable in
/// an IRQ. Deferring it is best-effort by construction — if the floor is wedged it never runs — and
/// that is the right trade, because the actions which must work when the floor is wedged are the
/// power ones, and those are handled inline.
static DEFERRED: AtomicUsize = AtomicUsize::new(0);

fn defer(action: crate::sysrq::Action) {
    let tag = match action {
        crate::sysrq::Action::Tasks => 1,
        _ => return,
    };
    DEFERRED.store(tag, Ordering::Release);
}

pub fn take_deferred_action() -> Option<crate::sysrq::Action> {
    match DEFERRED.swap(0, Ordering::Acquire) {
        1 => Some(crate::sysrq::Action::Tasks),
        _ => None,
    }
}

/// Whether an action may be performed from interrupt context.
pub const fn safe_in_irq(action: crate::sysrq::Action) -> bool {
    !matches!(action, crate::sysrq::Action::Tasks)
}

/// Perform an escape action from interrupt context.
///
/// Only actions [`safe_in_irq`] admits reach here. The power arms never return, so the spinning
/// child that provoked all this is simply never resumed — which is the outcome the operator asked
/// for. Output goes through the console fallback, which is allocation-free and takes only the
/// non-panicking `SoraState` borrow.
pub fn run_action_in_irq(action: crate::sysrq::Action) {
    use core::fmt::Write;
    let out = &mut crate::bootstrap::console::Writer;
    match action {
        crate::sysrq::Action::Armed => {
            let _ = out.write_str(crate::sysrq::ARMED);
        }
        crate::sysrq::Action::Help => {
            let _ = out.write_str(crate::sysrq::HELP);
        }
        crate::sysrq::Action::Reboot => {
            let _ = out.write_str("\r\nsysrq: reboot\r\n");
            crate::power::execute(crate::power::PowerAction::Reset, out);
        }
        crate::sysrq::Action::Shutdown => {
            let _ = out.write_str("\r\nsysrq: shutdown\r\n");
            crate::power::execute(crate::power::PowerAction::PowerOff, out);
        }
        crate::sysrq::Action::Halt => {
            let _ = out.write_str("\r\nsysrq: halt\r\n");
            crate::power::execute(crate::power::PowerAction::Halt, out);
        }
        crate::sysrq::Action::Unknown(byte) => {
            let _ = write!(out, "\r\nsysrq: no command for {byte:#04x}; try '?'\r\n");
        }
        // Deferred to the floor by `poll_from_irq`; never delivered here.
        crate::sysrq::Action::Tasks => {}
    }
}

/// Whether the boot floor forwards a queued serial byte to userland.
///
/// Every C0 control byte goes through, the same set a USB keyboard's Ctrl chords already deliver
/// (`kumo_hid::ctrl_ascii`): kilo's Ctrl-S/Ctrl-Q/Ctrl-F and ttyd's Ctrl-C are ordinary input. The
/// old allowlist forwarded only BS/TAB/ESC/CR/LF/DEL beside printable ASCII, so over serial kilo
/// could neither save nor quit. Two bytes stay out. `sysrq::PREFIX` (Ctrl-\) belongs to the escape
/// hatch, whose IRQ-side recogniser consumes it before it is queued, and this keeps that true even
/// if the recogniser changes. NUL is what a UART break or line noise reads as, so it is not taken
/// as a keystroke. Bytes above DEL have no encoding defined here and are dropped. — TIMBERDOODLE
pub const fn forwarded_to_userland(byte: u8) -> bool {
    match byte {
        0x00 => false,
        crate::sysrq::PREFIX => false,
        0x01..=0x7f => true,
        _ => false,
    }
}

/// Consumer side for the boot floor's REPL loop.
pub fn next_byte() -> Option<u8> {
    RING.pop()
}

pub fn pending() -> bool {
    !RING.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_round_trips_in_order() {
        let ring = Ring::new();
        for b in b"hello" {
            assert!(ring.push(*b));
        }
        let mut out = alloc::vec::Vec::new();
        while let Some(b) = ring.pop() {
            out.push(b);
        }
        assert_eq!(out.as_slice(), b"hello");
    }

    #[test]
    fn a_full_ring_drops_the_newest_rather_than_the_unread() {
        let ring = Ring::new();
        // Capacity is RING_BYTES - 1: one slot is sacrificed so full and empty stay distinct.
        for i in 0..RING_BYTES - 1 {
            assert!(ring.push(i as u8), "push {i} failed early");
        }
        assert!(!ring.push(0xff));
        // The bytes already queued are intact — the consumer's view was never overwritten.
        assert_eq!(ring.pop(), Some(0));
        assert_eq!(ring.pop(), Some(1));
    }

    #[test]
    fn draining_and_refilling_wraps_cleanly() {
        let ring = Ring::new();
        for _ in 0..3 {
            for b in b"abcd" {
                assert!(ring.push(*b));
            }
            for b in b"abcd" {
                assert_eq!(ring.pop(), Some(*b));
            }
        }
        assert!(ring.is_empty());
    }

    #[test]
    fn kilo_and_ttyd_control_keys_reach_userland() {
        // Ctrl-S save, Ctrl-Q quit, Ctrl-F find, Ctrl-C cancel, Ctrl-D, plus the keys the old
        // allowlist already carried.
        for byte in [
            0x13, 0x11, 0x06, 0x03, 0x04, 0x08, 0x09, 0x1b, b'\r', b'\n', 0x7f,
        ] {
            assert!(
                forwarded_to_userland(byte),
                "{byte:#04x} must reach userland"
            );
        }
        for byte in 0x20..=0x7e {
            assert!(forwarded_to_userland(byte));
        }
    }

    #[test]
    fn the_escape_prefix_nul_and_eight_bit_bytes_stay_out() {
        assert!(!forwarded_to_userland(crate::sysrq::PREFIX));
        assert!(!forwarded_to_userland(0x00));
        for byte in 0x80..=0xff {
            assert!(
                !forwarded_to_userland(byte),
                "{byte:#04x} has no encoding here"
            );
        }
    }

    #[test]
    fn every_control_key_a_usb_keyboard_can_send_also_crosses_serial() {
        // Parity with `kumo_hid::ctrl_ascii`: Ctrl-A..Ctrl-Z, Ctrl-[, Ctrl-], Ctrl-^, Ctrl-_.
        // Ctrl-\ and Ctrl-@/Ctrl-Space (NUL) are the two deliberate exceptions over serial.
        for byte in (0x01..=0x1a).chain([0x1b, 0x1d, 0x1e, 0x1f]) {
            assert!(forwarded_to_userland(byte), "{byte:#04x}");
        }
    }

    #[test]
    fn ctrl_backslash_still_arms_the_escape_hatch_and_is_never_queued() {
        let mut sysrq = crate::sysrq::Sysrq::new();
        let (consumed, action) = sysrq.feed(crate::sysrq::PREFIX);
        assert!(consumed);
        assert_eq!(action, Some(crate::sysrq::Action::Armed));
        // Ctrl-Q while armed is a command byte for the hatch, not a key for kilo.
        let (consumed, _) = sysrq.feed(0x11);
        assert!(consumed);
        // Disarmed, Ctrl-Q is ordinary input again and the floor forwards it.
        let (consumed, action) = sysrq.feed(0x11);
        assert!(!consumed && action.is_none());
        assert!(forwarded_to_userland(0x11));
    }

    #[test]
    fn an_empty_ring_yields_nothing() {
        let ring = Ring::new();
        assert!(ring.is_empty());
        assert_eq!(ring.pop(), None);
    }
}
