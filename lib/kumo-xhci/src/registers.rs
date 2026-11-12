//j426
//j529
//j538
//j703
//j886

use crate::{ContextSize, Error};

const MIN_CAPLENGTH: u8 = 0x20;
const PORT_REGISTER_SET_OFFSET: usize = 0x400;
const PORT_REGISTER_STRIDE: usize = 0x10;

// PORTSC bits the root-port helpers need (xHCI 5.4.8). — TIMBERDOODLE
const PORTSC_CONNECT: u32 = 1 << 0;
const PORTSC_PED: u32 = 1 << 1;
const PORTSC_RESET: u32 = 1 << 4;
const PORTSC_PLS_MASK: u32 = 0xf << 5;
const PORTSC_POWER: u32 = 1 << 9;
const PORTSC_WRC: u32 = 1 << 19;
const PORTSC_PRC: u32 = 1 << 21;
const PORTSC_CAS: u32 = 1 << 24;
const PORTSC_WR: u32 = 1 << 31;
/// Read-only status bits a neutral write saves (Linux `XHCI_PORT_RO`).
const PORTSC_RO: u32 = (1 << 0) | (1 << 3) | (0xf << 10) | (1 << 30);
/// Read/write bits where writing the read value leaves the bit unchanged (`XHCI_PORT_RWS`).
const PORTSC_RWS: u32 = (0xf << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);

/// xHCI capability registers needed before touching controller state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityRegisters {
    caplength: u8,
    hciversion: u16,
    hcsparams1: u32,
    hccparams1: u32,
}

impl CapabilityRegisters {
    pub fn from_words(
        caplength_hciversion: u32,
        hcsparams1: u32,
        hccparams1: u32,
    ) -> Result<Self, Error> {
        let caplength = (caplength_hciversion & 0xff) as u8;
        let hciversion = (caplength_hciversion >> 16) as u16;
        if caplength < MIN_CAPLENGTH || hciversion == 0 {
            return Err(Error::InvalidField);
        }
        Ok(Self {
            caplength,
            hciversion,
            hcsparams1,
            hccparams1,
        })
    }

    pub const fn caplength(self) -> u8 {
        self.caplength
    }

    pub const fn hciversion(self) -> u16 {
        self.hciversion
    }

    pub const fn max_slots(self) -> u8 {
        (self.hcsparams1 & 0xff) as u8
    }

    pub const fn max_interrupters(self) -> u16 {
        ((self.hcsparams1 >> 8) & 0x7ff) as u16
    }

    pub const fn max_ports(self) -> u8 {
        ((self.hcsparams1 >> 24) & 0xff) as u8
    }

    pub const fn context_size(self) -> ContextSize {
        if self.hccparams1 & (1 << 2) == 0 {
            ContextSize::Bytes32
        } else {
            ContextSize::Bytes64
        }
    }

    pub const fn extended_capabilities_offset(self) -> u16 {
        ((self.hccparams1 >> 16) & 0xffff) as u16
    }
}

/// Compute the PORTSC offset for one zero-based root-hub port index.
pub fn portsc_offset(caplength: u8, port_index: u8) -> Option<usize> {
    (caplength as usize)
        .checked_add(PORT_REGISTER_SET_OFFSET)?
        .checked_add((port_index as usize).checked_mul(PORT_REGISTER_STRIDE)?)
}

/// The read-back state that can be written to PORTSC without changing the port (Linux
/// `xhci_port_state_to_neutral`). PED and every RW1C change bit are zeroed and the reserved bits
/// are cleared, so a reset or power write cannot disable a port or clear a change latch.
/// — TIMBERDOODLE
pub const fn port_state_to_neutral(raw: u32) -> u32 {
    (raw & PORTSC_RO) | (raw & PORTSC_RWS)
}

/// A neutral PORTSC value that initiates a port reset (bit 4). — TIMBERDOODLE
pub const fn port_reset_value(raw: u32) -> u32 {
    port_state_to_neutral(raw) | PORTSC_RESET
}

/// A neutral PORTSC value that clears a latched Port Reset Change (PRC, bit 21, RW1C) and touches
/// nothing else.
pub const fn port_clear_reset_change_value(raw: u32) -> u32 {
    port_state_to_neutral(raw) | PORTSC_PRC
}

/// Whether the driver must bus-reset this root port before addressing what is on it.
///
/// Every connected port gets exactly one reset from the driver, **enabled or not**. A port the
/// firmware enumerated stays enabled across HCRST on the RK3588's DWC3, and the device behind it
/// keeps the address the firmware gave it. Without a bus reset our Address Device (SET_ADDRESS
/// sent to address 0) reaches nobody and completes with a USB transaction error (cc 4): the
/// Orange Pi 5 Plus keyboard and USB stick were invisible exactly that way (2026-11-12). Linux
/// likewise resets a port before addressing its device. — TEAL
pub const fn needs_bus_reset(status: PortStatus, already_reset: bool) -> bool {
    status.connected() && !already_reset
}

/// A neutral PORTSC value that initiates a warm reset (bit 31). — TIMBERDOODLE
pub const fn port_warm_reset_value(raw: u32) -> u32 {
    port_state_to_neutral(raw) | PORTSC_WR
}

/// A neutral PORTSC value that powers the port (bit 9). — TIMBERDOODLE
pub const fn port_power_value(raw: u32) -> u32 {
    port_state_to_neutral(raw) | PORTSC_POWER
}

/// Parsed PORTSC fields used by first-light logging.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortStatus {
    raw: u32,
}

impl PortStatus {
    pub const fn new(raw: u32) -> Self {
        Self { raw }
    }

    pub const fn raw(self) -> u32 {
        self.raw
    }

    pub const fn connected(self) -> bool {
        self.raw & PORTSC_CONNECT != 0
    }

    pub const fn enabled(self) -> bool {
        self.raw & PORTSC_PED != 0
    }

    pub const fn link_state(self) -> u8 {
        ((self.raw & PORTSC_PLS_MASK) >> 5) as u8
    }

    pub const fn speed(self) -> u8 {
        ((self.raw >> 10) & 0xf) as u8
    }

    pub const fn powered(self) -> bool {
        self.raw & (1 << 9) != 0
    }

    /// Enabled at a USB2 speed (Full, Low, or High). These are the ports a boot keyboard
    /// enumerates on, even when it is plugged into a blue USB3 connector.
    pub const fn usb2_ready(self) -> bool {
        self.connected() && self.enabled() && matches!(self.speed(), 1..=3)
    }

    /// A port reset is in progress.
    pub const fn reset_in_progress(self) -> bool {
        self.raw & PORTSC_RESET != 0
    }

    /// A port reset completed since the last change-bit clear (PRC).
    pub const fn reset_done(self) -> bool {
        self.raw & PORTSC_PRC != 0
    }

    /// A warm reset completed since the last change-bit clear.
    pub const fn warm_reset_done(self) -> bool {
        self.raw & PORTSC_WRC != 0
    }

    /// The link wants a warm reset: SS.Inactive, Compliance, or a latched warm-reset change.
    pub const fn warm_reset_required(self) -> bool {
        self.raw & PORTSC_CAS != 0 || matches!(self.link_state(), 6 | 10)
    }
}

/// One pass over every root-hub PORTSC while a driver waits for a port to come up after
/// HCRST. Feed each port to [`RootPortScan::observe`], then ask [`RootPortScan::choice`]
/// whether to enumerate now or keep polling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RootPortScan {
    first_ready: Option<(u8, u8)>,
    first_usb2_ready: Option<(u8, u8)>,
    first_superspeed_ready: Option<(u8, u8)>,
    reset_pending: bool,
    held_for_reset: bool,
}

impl RootPortScan {
    pub const fn new() -> Self {
        Self {
            first_ready: None,
            first_usb2_ready: None,
            first_superspeed_ready: None,
            reset_pending: false,
            held_for_reset: false,
        }
    }

    /// Record one port. `port_number` is 1-based, as the xHCI spec numbers root ports.
    pub fn observe(&mut self, port_number: u8, status: PortStatus) {
        if !status.connected() {
            return;
        }
        if !status.enabled() || status.reset_in_progress() {
            self.reset_pending = true;
            return;
        }
        let ready = (port_number, status.speed());
        if self.first_ready.is_none() {
            self.first_ready = Some(ready);
        }
        if status.usb2_ready() && self.first_usb2_ready.is_none() {
            self.first_usb2_ready = Some(ready);
        }
        if status.speed() >= 4 && self.first_superspeed_ready.is_none() {
            self.first_superspeed_ready = Some(ready);
        }
    }

    /// Treat the pass as still waiting on a reset the caller issued, even if the port's bits
    /// already read enabled (PED can read 1 from before the reset took effect).
    pub fn hold_for_reset(&mut self) {
        self.held_for_reset = true;
    }

    /// Lowest-numbered connected + enabled port, as `(port_number, speed)`, of any speed.
    pub const fn first_ready(&self) -> Option<(u8, u8)> {
        self.first_ready
    }

    /// Lowest-numbered connected + enabled SuperSpeed (or faster) port. When [`Self::choice`] has
    /// taken a USB2 companion, this is the other half of the same blue connector: on the Orange Pi
    /// 5 Plus front panel, the hub port a USB3 stick links on.
    pub const fn first_superspeed_ready(&self) -> Option<(u8, u8)> {
        self.first_superspeed_ready
    }

    /// The port to enumerate now, or `None` to keep polling.
    ///
    /// A ready USB2 port wins at once. A ready SuperSpeed port wins only when no connected
    /// port is still resetting, because the USB2 companion of the same physical connector may
    /// be the one still resetting. Taking the SuperSpeed half early selected an empty
    /// `05e3:0620` hub on the Orange Pi 5 Plus front panel and skipped the High-Speed hub
    /// behind it, so every USB2 keyboard there was invisible. — TIMBERDOODLE
    pub const fn choice(&self) -> Option<(u8, u8)> {
        if self.held_for_reset {
            return None;
        }
        if self.first_usb2_ready.is_some() {
            return self.first_usb2_ready;
        }
        if self.reset_pending {
            return None;
        }
        self.first_ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_words_expose_first_light_fields() {
        let caps = CapabilityRegisters::from_words(0x0110_0040, 0x0200_0840, 0x0001_0004).unwrap();
        assert_eq!(caps.caplength(), 0x40);
        assert_eq!(caps.hciversion(), 0x0110);
        assert_eq!(caps.max_slots(), 0x40);
        assert_eq!(caps.max_interrupters(), 8);
        assert_eq!(caps.max_ports(), 2);
        assert_eq!(caps.context_size(), ContextSize::Bytes64);
        assert_eq!(caps.extended_capabilities_offset(), 1);
        assert_eq!(portsc_offset(caps.caplength(), 1), Some(0x450));
    }

    #[test]
    fn portsc_fields_match_xhci_bit_assignments() {
        let port = PortStatus::new(1 | (1 << 1) | (1 << 9) | (3 << 5) | (4 << 10));
        assert!(port.connected());
        assert!(port.enabled());
        assert!(port.powered());
        assert_eq!(port.link_state(), 3);
        assert_eq!(port.speed(), 4);
    }

    #[test]
    fn neutral_writes_never_disable_or_clear_a_port() {
        // 0xe03 = CCS|PED|PP|U0 at High Speed; 0x1203 is the same at SuperSpeed with CSC latched.
        for raw in [0xe03u32, 0x1203, 0x2a0] {
            let neutral = port_state_to_neutral(raw);
            assert_eq!(neutral & (1 << 1), 0, "PED must be zero in a neutral write");
            assert_eq!(neutral & (1 << 17), 0, "change bits must be zero");
            assert_eq!(neutral & (0xf << 28), 0, "reserved bits must be zero");
            assert_eq!(neutral & (1 << 24), 0);
            assert_eq!(neutral & 1, raw & 1);
            assert_eq!(neutral & (0xf << 5), raw & (0xf << 5));
            assert_eq!(neutral & (1 << 9), raw & (1 << 9));
        }
    }

    #[test]
    fn reset_power_and_warm_reset_values_add_only_their_strobe() {
        let raw = 0xe03;
        assert_ne!(port_reset_value(raw) & (1 << 4), 0);
        assert_ne!(port_power_value(raw) & (1 << 9), 0);
        assert_ne!(port_warm_reset_value(raw) & (1 << 31), 0);
        assert_eq!(port_power_value(raw) & (1 << 4), 0);
        assert_eq!(port_warm_reset_value(raw) & (1 << 4), 0);
    }

    #[test]
    fn warm_reset_is_required_for_inactive_compliance_and_cas() {
        let inactive = PortStatus::new(1 | (1 << 9) | (6 << 5));
        let compliance = PortStatus::new(1 | (1 << 9) | (10 << 5));
        let cas = PortStatus::new(1 | (1 << 24));
        let polling = PortStatus::new(1 | (1 << 9) | (7 << 5));
        assert!(inactive.warm_reset_required());
        assert!(compliance.warm_reset_required());
        assert!(cas.warm_reset_required());
        assert!(!polling.warm_reset_required());
        assert!(PortStatus::new(1 | (1 << 19)).warm_reset_done());
    }

    // PORTSC values as read on the Orange Pi 5 Plus front panel on 2026-07-28: the
    // High-Speed hub is on root port 1 and its SuperSpeed sibling on root port 2.
    const HS_HUB_ENABLED: u32 = 0xe03;
    const SS_HUB_ENABLED: u32 = 0x1203;
    const HS_HUB_RESETTING: u32 = HS_HUB_ENABLED & !(1 << 1);

    fn scan(ports: &[u32]) -> RootPortScan {
        let mut scan = RootPortScan::new();
        for (index, &raw) in ports.iter().enumerate() {
            scan.observe(index as u8 + 1, PortStatus::new(raw));
        }
        scan
    }

    #[test]
    fn front_panel_values_decode_as_high_and_super_speed() {
        assert!(PortStatus::new(HS_HUB_ENABLED).usb2_ready());
        assert_eq!(PortStatus::new(HS_HUB_ENABLED).speed(), 3);
        assert!(!PortStatus::new(SS_HUB_ENABLED).usb2_ready());
        assert_eq!(PortStatus::new(SS_HUB_ENABLED).speed(), 4);
        assert!(!PortStatus::new(HS_HUB_RESETTING).enabled());
    }

    #[test]
    fn superspeed_sibling_waits_while_the_usb2_companion_resets() {
        let first_pass = scan(&[HS_HUB_RESETTING, SS_HUB_ENABLED]);
        assert_eq!(first_pass.choice(), None);
        assert_eq!(first_pass.first_ready(), Some((2, 4)));
    }

    #[test]
    fn usb2_companion_wins_once_enabled_even_after_a_ready_superspeed_port() {
        assert_eq!(
            scan(&[HS_HUB_ENABLED, SS_HUB_ENABLED]).choice(),
            Some((1, 3))
        );
        assert_eq!(
            scan(&[SS_HUB_ENABLED, HS_HUB_ENABLED]).choice(),
            Some((2, 3))
        );
    }

    #[test]
    fn a_ready_usb2_port_does_not_wait_for_an_unrelated_reset() {
        assert_eq!(
            scan(&[HS_HUB_ENABLED, HS_HUB_RESETTING]).choice(),
            Some((1, 3))
        );
    }

    #[test]
    fn a_lone_superspeed_device_is_chosen_when_nothing_is_resetting() {
        assert_eq!(scan(&[0, SS_HUB_ENABLED]).choice(), Some((2, 4)));
    }

    #[test]
    fn the_superspeed_half_is_reported_beside_the_usb2_choice() {
        let front = scan(&[HS_HUB_ENABLED, SS_HUB_ENABLED]);
        assert_eq!(front.choice(), Some((1, 3)));
        assert_eq!(front.first_superspeed_ready(), Some((2, 4)));
        assert_eq!(
            scan(&[HS_HUB_ENABLED, 0x2a0]).first_superspeed_ready(),
            None
        );
        assert_eq!(
            scan(&[0, HS_HUB_RESETTING, SS_HUB_ENABLED]).first_superspeed_ready(),
            Some((3, 4))
        );
    }

    #[test]
    fn nothing_connected_keeps_polling() {
        let empty = scan(&[0, 0x2a0]);
        assert_eq!(empty.choice(), None);
        assert_eq!(empty.first_ready(), None);
    }

    #[test]
    fn every_connected_port_is_reset_once_even_if_enabled() {
        // 0xe03: connected, enabled, High Speed: what the firmware leaves behind.
        let firmware_enabled = PortStatus::new(0xe03);
        assert!(needs_bus_reset(firmware_enabled, false));
        assert!(!needs_bus_reset(firmware_enabled, true));
        assert!(
            !needs_bus_reset(PortStatus::new(0x2a0), false),
            "nothing connected"
        );
    }

    #[test]
    fn a_resetting_port_is_not_ready() {
        let mut scan = RootPortScan::new();
        scan.observe(1, PortStatus::new(0xe03 | PORTSC_RESET));
        assert_eq!(scan.first_ready(), None);
        assert_eq!(scan.choice(), None);
        let mut held = RootPortScan::new();
        held.observe(1, PortStatus::new(0xe03));
        held.hold_for_reset();
        assert_eq!(
            held.choice(),
            None,
            "a pass waiting on our reset picks nothing"
        );
        let mut done = RootPortScan::new();
        done.observe(1, PortStatus::new(0xe03));
        assert_eq!(done.choice(), Some((1, 3)));
    }

    #[test]
    fn clearing_the_reset_change_touches_nothing_else() {
        let raw = 0xe03 | PORTSC_PRC | PORTSC_WRC | (1 << 17);
        let v = port_clear_reset_change_value(raw);
        assert_eq!(v & PORTSC_PRC, PORTSC_PRC);
        assert_eq!(v & (PORTSC_PED | PORTSC_WRC | (1 << 17) | PORTSC_RESET), 0);
        assert!(PortStatus::new(raw).reset_done());
    }
}
