//j703

//! DWC3 global-register policy for the RK3588 xHCI cores.
//!
//! The Orange Pi 5 Plus controllers are Synopsys DWC3 cores: the xHCI register block starts at
//! the core base and the DWC3 globals live at +0xC100, outside the xHCI window. PLAN_VI trusted
//! firmware for this init; metal then showed root-port links dropping. This module is the pure,
//! host-tested write plan the driver applies before HCRST. — TIMBERDOODLE

use crate::probe::{
    QUIRK_DIS_DEL_PHY_POWER_CHG, QUIRK_DIS_ENBLSLPM, QUIRK_DIS_RXDET_INP3,
    QUIRK_DIS_U2_FREECLK_EXISTS,
};
use crate::RegisterIo;

/// DWC3 global control.
pub const GCTL: usize = 0xC110;
/// USB2 PHY configuration for the first port.
pub const GUSB2PHYCFG0: usize = 0xC200;
/// USB3 PIPE control for the first port.
pub const GUSB3PIPECTL0: usize = 0xC2C0;

const GCTL_PRTCAPDIR_MASK: u32 = 0x3 << 12;
const GCTL_PRTCAP_HOST: u32 = 1 << 12;
const GCTL_SCALEDOWN_MASK: u32 = 0x3 << 4;
const GCTL_DISSCRAMBLE: u32 = 1 << 3;
const GCTL_DSBLCLKGTNG: u32 = 1 << 0;

const GUSB2PHY_SUSPHY: u32 = 1 << 6;
const GUSB2PHY_ENBLSLPM: u32 = 1 << 8;
const GUSB2PHY_U2_FREECLK_EXISTS: u32 = 1 << 30;

const GUSB3PIPE_SUSPHY: u32 = 1 << 17;
const GUSB3PIPE_DEPOCHANGE: u32 = 1 << 18;
const GUSB3PIPE_UX_EXIT_PX: u32 = 1 << 27;
const GUSB3PIPE_DISRXDETINP3: u32 = 1 << 28;

/// One read-modify-write: clear `clear`, then set `set`, preserving every other bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterUpdate {
    pub offset: usize,
    pub clear: u32,
    pub set: u32,
}

/// The DWC3 host-mode write plan for one controller.
///
/// PHY suspend is left disabled: KUMO has no runtime PM, and a suspended PHY is exactly the
/// state a link must not enter while a port is being enumerated. Linux clears SUSPHY before
/// PHY init and re-enables it only under runtime PM. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostInit {
    pub gctl: RegisterUpdate,
    pub usb2: RegisterUpdate,
    pub usb3: RegisterUpdate,
}

impl HostInit {
    pub const fn for_flags(flags: u32) -> Self {
        let mut usb2_clear = GUSB2PHY_SUSPHY;
        if flags & QUIRK_DIS_U2_FREECLK_EXISTS != 0 {
            usb2_clear |= GUSB2PHY_U2_FREECLK_EXISTS;
        }
        let usb2_set = if flags & QUIRK_DIS_ENBLSLPM != 0 {
            usb2_clear |= GUSB2PHY_ENBLSLPM;
            0
        } else {
            GUSB2PHY_ENBLSLPM
        };

        let mut usb3_clear = GUSB3PIPE_SUSPHY | GUSB3PIPE_UX_EXIT_PX;
        if flags & QUIRK_DIS_DEL_PHY_POWER_CHG != 0 {
            usb3_clear |= GUSB3PIPE_DEPOCHANGE;
        }
        let usb3_set = if flags & QUIRK_DIS_RXDET_INP3 != 0 {
            GUSB3PIPE_DISRXDETINP3
        } else {
            0
        };

        Self {
            gctl: RegisterUpdate {
                offset: GCTL,
                clear: GCTL_PRTCAPDIR_MASK
                    | GCTL_SCALEDOWN_MASK
                    | GCTL_DISSCRAMBLE
                    | GCTL_DSBLCLKGTNG,
                set: GCTL_PRTCAP_HOST,
            },
            usb2: RegisterUpdate {
                offset: GUSB2PHYCFG0,
                clear: usb2_clear,
                set: usb2_set,
            },
            usb3: RegisterUpdate {
                offset: GUSB3PIPECTL0,
                clear: usb3_clear,
                set: usb3_set,
            },
        }
    }

    /// Apply the three updates through any register port.
    pub fn apply<IO: RegisterIo>(self, io: &mut IO) {
        for update in [self.gctl, self.usb2, self.usb3] {
            let value = io.read32(update.offset);
            io.write32(update.offset, (value & !update.clear) | update.set);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{
        QUIRK_DIS_DEL_PHY_POWER_CHG, QUIRK_DIS_ENBLSLPM, QUIRK_DIS_RXDET_INP3,
        QUIRK_DIS_U2_FREECLK_EXISTS,
    };

    struct Bus {
        gctl: u32,
        usb2: u32,
        usb3: u32,
    }

    impl RegisterIo for Bus {
        fn read32(&mut self, offset: usize) -> u32 {
            match offset {
                GCTL => self.gctl,
                GUSB2PHYCFG0 => self.usb2,
                GUSB3PIPECTL0 => self.usb3,
                _ => 0,
            }
        }

        fn write32(&mut self, offset: usize, value: u32) {
            match offset {
                GCTL => self.gctl = value,
                GUSB2PHYCFG0 => self.usb2 = value,
                GUSB3PIPECTL0 => self.usb3 = value,
                _ => {}
            }
        }
    }

    #[test]
    fn the_plan_sets_host_mode_and_clears_the_five_global_bits() {
        let plan = HostInit::for_flags(0);
        assert_eq!(plan.gctl.offset, 0xC110);
        assert_eq!(
            plan.gctl.clear,
            (0x3 << 12) | (0x3 << 4) | (1 << 3) | (1 << 0)
        );
        assert_eq!(plan.gctl.set, 1 << 12);
        assert_eq!(plan.usb2.offset, 0xC200);
        assert_eq!(plan.usb3.offset, 0xC2C0);
    }

    #[test]
    fn susp_is_always_cleared_and_enblslpm_follows_the_quirk() {
        let plain = HostInit::for_flags(0);
        assert_ne!(plain.usb2.clear & (1 << 6), 0);
        assert_ne!(plain.usb2.set & (1 << 8), 0);
        assert_ne!(plain.usb3.clear & (1 << 17), 0);

        let quirky = HostInit::for_flags(QUIRK_DIS_ENBLSLPM | QUIRK_DIS_U2_FREECLK_EXISTS);
        assert_eq!(quirky.usb2.set & (1 << 8), 0);
        assert_ne!(quirky.usb2.clear & (1 << 8), 0);
        assert_ne!(quirky.usb2.clear & (1 << 30), 0);
    }

    #[test]
    fn rxdetect_and_depchange_follow_the_quirks() {
        let plain = HostInit::for_flags(0);
        assert_eq!(plain.usb3.set & (1 << 28), 0);
        assert_eq!(plain.usb3.clear & (1 << 18), 0);

        let quirky = HostInit::for_flags(QUIRK_DIS_RXDET_INP3 | QUIRK_DIS_DEL_PHY_POWER_CHG);
        assert_ne!(quirky.usb3.set & (1 << 28), 0);
        assert_ne!(quirky.usb3.clear & (1 << 18), 0);
    }

    #[test]
    fn apply_preserves_bits_outside_the_plan() {
        let mut bus = Bus {
            gctl: 0x8000_0000 | (0x2 << 12) | (1 << 19),
            usb2: 0x4000_0000 | (1 << 8) | (1 << 4),
            usb3: 0x8000_0000 | (1 << 17) | (1 << 27) | (1 << 2),
        };
        HostInit::for_flags(QUIRK_DIS_DEL_PHY_POWER_CHG | QUIRK_DIS_RXDET_INP3).apply(&mut bus);
        assert_eq!(bus.gctl & (0x3 << 12), 1 << 12);
        assert_eq!(bus.gctl & (0x3 << 4), 0);
        assert_ne!(bus.gctl & (1 << 19), 0);
        assert_ne!(bus.gctl & (1 << 31), 0);
        assert_eq!(bus.usb2 & (1 << 6), 0);
        assert_ne!(bus.usb2 & (1 << 4), 0);
        assert_eq!(bus.usb3 & (1 << 17), 0);
        assert_eq!(bus.usb3 & (1 << 27), 0);
        assert_eq!(bus.usb3 & (1 << 18), 0);
        assert_ne!(bus.usb3 & (1 << 28), 0);
        assert_ne!(bus.usb3 & (1 << 2), 0);
    }
}
