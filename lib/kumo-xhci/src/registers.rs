//j426
//j529
//j538

use crate::{ContextSize, Error};

const MIN_CAPLENGTH: u8 = 0x20;
const PORT_REGISTER_SET_OFFSET: usize = 0x400;
const PORT_REGISTER_STRIDE: usize = 0x10;

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
        self.raw & 1 != 0
    }

    pub const fn enabled(self) -> bool {
        self.raw & (1 << 1) != 0
    }

    pub const fn link_state(self) -> u8 {
        ((self.raw >> 5) & 0xf) as u8
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
}

impl RootPortScan {
    pub const fn new() -> Self {
        Self {
            first_ready: None,
            first_usb2_ready: None,
            first_superspeed_ready: None,
            reset_pending: false,
        }
    }

    /// Record one port. `port_number` is 1-based, as the xHCI spec numbers root ports.
    pub fn observe(&mut self, port_number: u8, status: PortStatus) {
        if !status.connected() {
            return;
        }
        if !status.enabled() {
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
}
