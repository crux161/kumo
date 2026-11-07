//j832
//! NET5a's finite read-only register set. No page selection, power/clock setup or
//! EEPROM access. Unavailable/page-dependent PHY values remain explicit. — TIMBERDOODLE
use crate::control::{MacRegister, Request};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    NodeId,
    UsbLink,
    SiliconStatus,
    PhyPowerReset,
    Clock,
    PhyPage,
    PhyId1,
    PhyId2,
    PhyBasicStatus,
    PhyPhysicalStatus,
}

pub const READS: [Field; 10] = [
    Field::NodeId,
    Field::UsbLink,
    Field::SiliconStatus,
    Field::PhyPowerReset,
    Field::Clock,
    Field::PhyPage,
    Field::PhyId1,
    Field::PhyId2,
    Field::PhyBasicStatus,
    Field::PhyPhysicalStatus,
];

impl Field {
    pub const fn name(self) -> &'static str {
        match self {
            Self::NodeId => "node_id",
            Self::UsbLink => "usb_link",
            Self::SiliconStatus => "silicon_status",
            Self::PhyPowerReset => "phy_power_reset",
            Self::Clock => "clock",
            Self::PhyPage => "phy_page",
            Self::PhyId1 => "phy_id1",
            Self::PhyId2 => "phy_id2",
            Self::PhyBasicStatus => "phy_basic_status",
            Self::PhyPhysicalStatus => "phy_physical_status",
        }
    }
    pub fn request(self) -> Request {
        match self {
            Self::NodeId => Request::mac_read(MacRegister::NodeId),
            Self::UsbLink => Request::mac_read(MacRegister::UsbLink),
            Self::SiliconStatus => Request::mac_read(MacRegister::SiliconStatus),
            Self::PhyPowerReset => Request::mac_read(MacRegister::PhyPowerReset),
            Self::Clock => Request::mac_read(MacRegister::Clock),
            Self::PhyPage => Request::phy_read(31).expect("literal PHY register"),
            Self::PhyId1 => Request::phy_read(2).expect("literal PHY register"),
            Self::PhyId2 => Request::phy_read(3).expect("literal PHY register"),
            Self::PhyBasicStatus => Request::phy_read(1).expect("literal PHY register"),
            Self::PhyPhysicalStatus => Request::phy_read(17).expect("literal PHY register"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhyStatus {
    Unavailable,
    UnknownPage(u16),
    Page0 {
        id: u32,
        link: bool,
        speed_mbps: Option<u16>,
        full_duplex: bool,
    },
}

/// Decode PHYSR only with valid ID registers and a directly observed page zero.
/// Raw zero/all-ones IDs do not establish a powered PHY or a cable-down result.
/// These are reported register bits, not a negotiated-peer proof. — TIMBERDOODLE
pub fn phy_status(page: u16, id1: u16, id2: u16, physical: u16) -> PhyStatus {
    let id = (u32::from(id1) << 16) | u32::from(id2);
    if id == 0 || id == u32::MAX || page == u16::MAX || physical == u16::MAX {
        return PhyStatus::Unavailable;
    }
    if page != 0 {
        return PhyStatus::UnknownPage(page);
    }
    let link = physical & 0x400 != 0;
    let speed_mbps = if link {
        match physical & 0xc000 {
            0 => Some(10),
            0x4000 => Some(100),
            0x8000 => Some(1000),
            _ => None,
        }
    } else {
        None
    };
    PhyStatus::Page0 {
        id,
        link,
        speed_mbps,
        full_duplex: physical & 0x2000 != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_literal_mac_and_phy_in_requests_are_representable() {
        let expected = [
            [0xc0, 1, 0x10, 0, 6, 0, 6, 0],
            [0xc0, 1, 2, 0, 1, 0, 1, 0],
            [0xc0, 1, 3, 0, 1, 0, 1, 0],
            [0xc0, 1, 0x26, 0, 2, 0, 2, 0],
            [0xc0, 1, 0x33, 0, 1, 0, 1, 0],
            [0xc0, 2, 3, 0, 31, 0, 2, 0],
            [0xc0, 2, 3, 0, 2, 0, 2, 0],
            [0xc0, 2, 3, 0, 3, 0, 2, 0],
            [0xc0, 2, 3, 0, 1, 0, 2, 0],
            [0xc0, 2, 3, 0, 17, 0, 2, 0],
        ];
        for (field, wire) in READS.into_iter().zip(expected) {
            let request = field.request();
            assert_eq!(request.setup(), wire);
            assert!(request.outbound().is_empty());
            for actual in 0..=7 {
                assert_eq!(
                    request.validate_transferred(actual).is_ok(),
                    actual == wire[6] as usize
                );
            }
        }
    }
    #[test]
    fn unpowered_and_unknown_page_never_mean_link_down() {
        assert_eq!(phy_status(0, 0, 0, 0), PhyStatus::Unavailable);
        assert_eq!(
            phy_status(0xffff, 0xffff, 0xffff, 0xffff),
            PhyStatus::Unavailable
        );
        assert_eq!(
            phy_status(7, 0x1234, 0x5678, 0xa400),
            PhyStatus::UnknownPage(7)
        );
        assert_eq!(
            phy_status(0, 0x1234, 0x5678, 0xffff),
            PhyStatus::Unavailable
        );
    }
    #[test]
    fn page_zero_reports_bits_without_inventing_reserved_speed() {
        for (physical, link, speed, full) in [
            (0, false, None, false),
            (0x400, true, Some(10), false),
            (0x6400, true, Some(100), true),
            (0xa400, true, Some(1000), true),
            (0xc400, true, None, false),
        ] {
            assert_eq!(
                phy_status(0, 0x1234, 0x5678, physical),
                PhyStatus::Page0 {
                    id: 0x12345678,
                    link,
                    speed_mbps: speed,
                    full_duplex: full
                }
            );
        }
    }
}
