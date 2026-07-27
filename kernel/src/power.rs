//! Bringing the machine down on purpose.
//!
//! `reboot` used to be a bare PSCI `SYSTEM_RESET` issued straight from the shell, which is
//! fine only because a reset does not care what state it interrupts. Power-off does: the
//! board keeps its rails up until firmware cuts them, so anything still running — an xHCI
//! ring mid-DMA, a driver child waiting on a level-triggered line — runs right up to the
//! edge. This module is the ordered bring-down every power command now goes through.
//!
//! "Graceful" here means what the kernel can actually prove today:
//!
//!   1. the console is detached from Sora, so every line below reaches the UART directly
//!      rather than switching into a userspace thread we are about to strand;
//!   2. every device interrupt line held by a driver binding is masked, which stops the
//!      resident (IRQ-driven) drivers where they stand and stops their controllers from
//!      re-asserting into the GIC;
//!   3. only then does firmware get the call.
//!
//! What it does *not* mean: userspace is never *notified*. There is no shutdown message in
//! the ABI for a server to acknowledge, so servers are stopped rather than asked to stop —
//! adequate while nothing above the kernel owns dirty state, and the obvious next slice
//! once something does.

use core::fmt::Write;

/// How the operator asked for the machine to stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerAction {
    /// Cut the rails through firmware (PSCI `SYSTEM_OFF`).
    PowerOff,
    /// Stop this CPU and leave the board powered.
    Halt,
    /// Warm reset through firmware (PSCI `SYSTEM_RESET`).
    Reset,
}

impl PowerAction {
    /// Map a shell verb onto an action. `None` for anything else, so the shell's unknown
    /// command path still owns everything that is not a power command.
    pub fn from_command(cmd: &str) -> Option<Self> {
        match cmd {
            "shutdown" | "poweroff" => Some(Self::PowerOff),
            "halt" => Some(Self::Halt),
            "reboot" | "reset" => Some(Self::Reset),
            _ => None,
        }
    }

    /// The line printed before quiesce starts — the last message that still rides the
    /// console route, and therefore the last one the framebuffer can show.
    pub const fn notice(self) -> &'static str {
        match self {
            Self::PowerOff => "shutting down: PSCI SYSTEM_OFF",
            Self::Halt => "halting: stopping this CPU",
            Self::Reset => "rebooting: PSCI SYSTEM_RESET",
        }
    }

    /// Whether a firmware refusal should still stop the CPU.
    ///
    /// Power-off and halt are terminal: the operator asked for the machine to stop, so a
    /// firmware that declines must not quietly leave it running. A refused reset is
    /// recoverable — the shell that issued it is still there to report the failure — so
    /// that path puts back everything quiesce took and returns to the prompt.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Reset)
    }
}

/// Run the bring-down. Returns only for a reset that firmware declined.
pub fn execute(action: PowerAction, out: &mut dyn Write) {
    let _ = write!(out, "{}\r\n", action.notice());

    let was_routed = crate::usermode::console_route_enabled();
    crate::usermode::disable_console_route();
    match crate::usermode::quiesce_device_interrupts() {
        crate::usermode::Quiesce::Masked(n) => {
            let _ = write!(out, "quiesced: {n} device interrupt line(s) masked\r\n");
        }
        crate::usermode::Quiesce::Unavailable => {
            // Say it plainly: the machine is going down without the device quiesce, and anything
            // mid-DMA is still mid-DMA. Silence would look identical to a clean pass.
            let _ =
                out.write_str("quiesce SKIPPED: kernel state busy - device lines left live\r\n");
        }
    }

    // Both sinks' `putc` waits for room, not for completion, so everything above may still be
    // in a FIFO. Drain before handing control to firmware — a power-off that truncates its own
    // last line leaves the operator guessing whether it got that far.
    kumo_hal::active::console_drain();

    match action {
        PowerAction::PowerOff => {
            kumo_hal::active::system_off();
            let _ = out.write_str("firmware declined SYSTEM_OFF\r\n");
        }
        PowerAction::Reset => {
            kumo_hal::active::system_reset();
            let _ = out.write_str("firmware declined SYSTEM_RESET\r\n");
        }
        PowerAction::Halt => {}
    }

    if !action.is_terminal() {
        let restored = crate::usermode::restore_device_interrupts();
        if was_routed {
            crate::usermode::enable_console_route();
        }
        match restored {
            crate::usermode::Quiesce::Masked(n) => {
                let _ = write!(out, "resumed: {n} device interrupt line(s) unmasked\r\n");
            }
            crate::usermode::Quiesce::Unavailable => {
                let _ = out.write_str("resume SKIPPED: kernel state busy\r\n");
            }
        }
        return;
    }

    let _ = out.write_str("system halted.\r\n");
    kumo_hal::active::console_drain();
    stop_cpu();
}

/// Mask interrupts and park the core. On the host this is a no-op so the shell's dispatch
/// tests can run the real command path without wedging the test binary.
#[cfg(target_os = "none")]
fn stop_cpu() -> ! {
    kumo_hal::active::halt_cpu()
}

#[cfg(not(target_os = "none"))]
fn stop_cpu() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_verbs_map_to_actions() {
        assert_eq!(
            PowerAction::from_command("shutdown"),
            Some(PowerAction::PowerOff)
        );
        assert_eq!(
            PowerAction::from_command("poweroff"),
            Some(PowerAction::PowerOff)
        );
        assert_eq!(PowerAction::from_command("halt"), Some(PowerAction::Halt));
        assert_eq!(
            PowerAction::from_command("reboot"),
            Some(PowerAction::Reset)
        );
        assert_eq!(PowerAction::from_command("reset"), Some(PowerAction::Reset));
    }

    #[test]
    fn non_power_verbs_are_not_claimed() {
        for cmd in ["help", "ps", "shutdownx", "shut", "", "halted", "rebooting"] {
            assert_eq!(PowerAction::from_command(cmd), None, "claimed '{cmd}'");
        }
    }

    #[test]
    fn stopping_the_machine_is_terminal_but_a_reset_is_recoverable() {
        assert!(PowerAction::PowerOff.is_terminal());
        assert!(PowerAction::Halt.is_terminal());
        assert!(!PowerAction::Reset.is_terminal());
    }

    #[test]
    fn every_action_names_itself_before_quiesce() {
        assert!(PowerAction::PowerOff.notice().contains("SYSTEM_OFF"));
        assert!(PowerAction::Halt.notice().contains("halting"));
        assert!(PowerAction::Reset.notice().contains("SYSTEM_RESET"));
    }
}
