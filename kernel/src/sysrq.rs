//! The console escape hatch — a way down that does not go through userland.
//!
//! `run` spawns a child and then drains its stdout to EOF. A child that never exits never closes
//! stdout, so Sora parks in that read forever — and Sora is the shell, so the prompt, `shutdown`,
//! and every other command go with it. `persona-linux-hello` does exactly this. The machine is
//! still running, the kernel is still looping, keystrokes are still arriving; there is simply
//! nobody left in userland to act on them.
//!
//! The lesson is not "add Ctrl-C". Ctrl-C would be delivered *by Sora*, and Sora is the thing that
//! is stuck. **An escape hatch that runs through the component being escaped is not an escape
//! hatch.** So this one lives in the kernel's own REPL loop, on the boot floor, which keeps running
//! no matter what userland does, and it reaches the power path directly — which already quiesces
//! and prints without needing Sora alive.
//!
//! This is Linux's SysRq argument, and it holds for the same reason: the last resort has to be
//! reachable when everything above it has failed.
//!
//! Ctrl-C, process termination, and a Sora that does not block on its children are all still worth
//! building — they are what make a *program* interruptible. This is what keeps a stuck program from
//! taking the *machine* with it, and it comes first because it is the difference between an
//! annoyance and a power cycle.

/// What a completed escape sequence asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// The prefix registered and a command byte is expected. Reported so the console can *say* so —
    /// a silent escape sequence gives the operator no way to tell a missed keystroke from a
    /// mis-remembered one, which is exactly the doubt this is meant to remove.
    Armed,
    Reboot,
    Shutdown,
    Halt,
    Tasks,
    Help,
    /// The prefix was armed and the next byte meant nothing; say so rather than act.
    Unknown(u8),
}

/// The arming byte: Ctrl-\ (FS, 0x1c).
///
/// Deliberately not Ctrl-C — that one belongs to userland, and giving it to the kernel would take
/// away the key every program will eventually want. Ctrl-\ is already the "something has gone
/// wrong, get me out" key by convention, and nothing in this system's line discipline claims it.
pub const PREFIX: u8 = 0x1c;

/// The no-modifier alternative: three consecutive tildes, then a command letter.
///
/// `Ctrl-\` requires a control character to survive the whole path — a host terminal that has not
/// gone fully raw, a keymap, a HID decoder. Each is a place it can be swallowed, and when it is,
/// the operator has no way to tell whether the escape failed or their fingers did. `~~~` needs no
/// modifier at all and travels as three ordinary printable bytes, so it works wherever plain typing
/// works. The tildes themselves are **not** consumed: in a healthy shell they land in the line
/// buffer where they can be seen and erased, which is better than vanishing.
pub const TILDE: u8 = b'~';
pub const TILDE_RUN: u8 = 3;

/// Escape recogniser: a prefix, then a command letter.
///
/// A prefix rather than a single key because a single key is one stray byte away from resetting a
/// machine somebody was using. Requiring a second, chosen byte makes an accident take two.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Sysrq {
    armed: bool,
    tildes: u8,
}

impl Sysrq {
    pub const fn new() -> Self {
        Self {
            armed: false,
            tildes: 0,
        }
    }

    /// Whether the prefix has been seen and a command byte is expected.
    pub const fn armed(self) -> bool {
        self.armed
    }

    /// Offer one console byte.
    ///
    /// Returns `(consumed, action)`. `consumed` means the byte belonged to this recogniser and must
    /// **not** be forwarded to userland — otherwise a `Ctrl-\ r` would also type an `r` into
    /// whatever the shell was doing.
    pub fn feed(&mut self, byte: u8) -> (bool, Option<Action>) {
        if self.armed {
            self.armed = false;
            self.tildes = 0;
            let action = match byte {
                b'r' => Action::Reboot,
                b's' => Action::Shutdown,
                b'h' => Action::Halt,
                b't' => Action::Tasks,
                b'?' => Action::Help,
                // A second prefix re-arms rather than resolving: holding the key down should not
                // resolve to `Unknown(0x1c)` and print an error.
                PREFIX => {
                    self.armed = true;
                    return (true, Some(Action::Armed));
                }
                other => Action::Unknown(other),
            };
            return (true, Some(action));
        }
        if byte == PREFIX {
            self.armed = true;
            self.tildes = 0;
            return (true, Some(Action::Armed));
        }
        if byte == TILDE {
            self.tildes += 1;
            if self.tildes >= TILDE_RUN {
                self.armed = true;
                self.tildes = 0;
                // Not consumed: the tildes are ordinary characters and belong to whoever was
                // typing them. Only the command byte that follows is taken.
                return (false, Some(Action::Armed));
            }
            return (false, None);
        }
        self.tildes = 0;
        (false, None)
    }
}

pub const HELP: &str = "\r\nsysrq: prefix is ctrl-\\ or ~~~ , then one of:\r\n\
     r=reboot  s=shutdown  h=halt  t=tasks  ?=this list\r\n\
     (works even when userland is wedged - it runs in the kernel)\r\n";

/// Printed the moment the prefix registers, so the operator knows the escape took.
pub const ARMED: &str = "\r\nsysrq: armed - press r, s, h, t or ?\r\n";

/// A [`Sysrq`] shared between the IRQ handler and the boot floor.
///
/// One instance, not two: a prefix noticed in the interrupt and a command byte seen by the REPL
/// loop must still resolve. Single core, and `feed` is a handful of non-allocating field updates
/// that cannot block, so an `UnsafeCell` is honest here where a lock would be a lie — the IRQ
/// handler cannot wait for one, and the whole point of this path is that it runs when nothing else
/// can.
pub struct SysrqCell(core::cell::UnsafeCell<Sysrq>);

// SAFETY: single core, and `feed` is short and non-reentrant — the only other caller is the boot
// floor, which an interrupt suspends rather than races.
unsafe impl Sync for SysrqCell {}

impl SysrqCell {
    pub const fn new() -> Self {
        Self(core::cell::UnsafeCell::new(Sysrq::new()))
    }

    pub fn feed(&self, byte: u8) -> (bool, Option<Action>) {
        unsafe { (*self.0.get()).feed(byte) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(bytes: &[u8]) -> (Sysrq, alloc::vec::Vec<Action>) {
        extern crate alloc;
        let mut sysrq = Sysrq::new();
        let mut actions = alloc::vec::Vec::new();
        for &b in bytes {
            if let (_, Some(a)) = sysrq.feed(b) {
                actions.push(a);
            }
        }
        (sysrq, actions)
    }

    #[test]
    fn arming_is_announced_so_a_missed_keystroke_is_visible() {
        // The failure this fixes: pressing the prefix did nothing observable, so there was no way
        // to tell a swallowed control character from a mis-remembered sequence.
        let mut sysrq = Sysrq::new();
        assert_eq!(sysrq.feed(PREFIX).1, Some(Action::Armed));
    }

    #[test]
    fn three_tildes_arm_without_any_modifier_key() {
        let mut sysrq = Sysrq::new();
        assert_eq!(sysrq.feed(TILDE), (false, None));
        assert_eq!(sysrq.feed(TILDE), (false, None));
        // The third arms — and the tildes still pass through to userland.
        assert_eq!(sysrq.feed(TILDE), (false, Some(Action::Armed)));
        assert!(sysrq.armed());
        assert_eq!(sysrq.feed(b's'), (true, Some(Action::Shutdown)));
    }

    #[test]
    fn a_broken_tilde_run_does_not_arm() {
        let mut sysrq = Sysrq::new();
        for b in b"~~a~~" {
            assert_eq!(sysrq.feed(*b).1, None, "byte {b:#x} armed early");
        }
        assert!(!sysrq.armed());
    }

    #[test]
    fn ordinary_bytes_pass_straight_through() {
        let mut sysrq = Sysrq::new();
        for b in b"shutdown\r" {
            assert_eq!(sysrq.feed(*b), (false, None), "byte {b:#x} was swallowed");
        }
    }

    #[test]
    fn the_prefix_arms_and_the_next_byte_commands() {
        let mut sysrq = Sysrq::new();
        assert_eq!(sysrq.feed(PREFIX), (true, Some(Action::Armed)));
        assert!(sysrq.armed());
        assert_eq!(sysrq.feed(b'r'), (true, Some(Action::Reboot)));
        assert!(!sysrq.armed());
    }

    #[test]
    fn every_command_byte_is_recognised() {
        let (_, actions) = feed_all(&[
            PREFIX, b'r', PREFIX, b's', PREFIX, b'h', PREFIX, b't', PREFIX, b'?',
        ]);
        let commands: alloc::vec::Vec<_> = actions
            .into_iter()
            .filter(|a| *a != Action::Armed)
            .collect();
        assert_eq!(
            commands.as_slice(),
            [
                Action::Reboot,
                Action::Shutdown,
                Action::Halt,
                Action::Tasks,
                Action::Help
            ]
        );
    }

    #[test]
    fn a_command_byte_is_consumed_and_never_reaches_userland() {
        // The whole point: `ctrl-\ r` must not also type an `r` into the shell.
        let mut sysrq = Sysrq::new();
        sysrq.feed(PREFIX);
        let (consumed, _) = sysrq.feed(b'r');
        assert!(consumed);
    }

    #[test]
    fn an_unknown_command_reports_rather_than_guessing() {
        let mut sysrq = Sysrq::new();
        sysrq.feed(PREFIX);
        assert_eq!(sysrq.feed(b'x'), (true, Some(Action::Unknown(b'x'))));
        // And it disarms, so the next keystroke is ordinary input again.
        assert!(!sysrq.armed());
        assert_eq!(sysrq.feed(b'a'), (false, None));
    }

    #[test]
    fn holding_the_prefix_down_re_arms_instead_of_erroring() {
        let mut sysrq = Sysrq::new();
        sysrq.feed(PREFIX);
        assert_eq!(sysrq.feed(PREFIX), (true, Some(Action::Armed)));
        assert!(sysrq.armed());
        assert_eq!(sysrq.feed(b's'), (true, Some(Action::Shutdown)));
    }

    #[test]
    fn one_stray_prefix_byte_cannot_reset_the_machine() {
        // An accident has to take two deliberate bytes, which is the reason for the prefix.
        let mut sysrq = Sysrq::new();
        assert_eq!(sysrq.feed(PREFIX), (true, Some(Action::Armed)));
        assert_eq!(sysrq.feed(b'\r'), (true, Some(Action::Unknown(b'\r'))));
    }
}
