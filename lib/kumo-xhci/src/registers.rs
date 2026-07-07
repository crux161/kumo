//j426

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
}
