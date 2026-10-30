//j426
//j481
//j703

pub const XHCI_PROBE_CONFIG_LEN: usize = 32;
pub const XHCI_NO_STREAM_ID: u32 = u32::MAX;
const XHCI_PROBE_MAGIC: [u8; 4] = *b"XHCI";

/// The controller is a Synopsys DWC3 core whose globals live at +0xC100, outside the xHCI
/// register block. Host-mode init must reach them. — TIMBERDOODLE
pub const FLAG_DWC3: u32 = 1 << 31;

/// DTB `snps,dis_enblslpm-quirk`: clear GUSB2PHYCFG.ENBLSLPM. — TIMBERDOODLE
pub const QUIRK_DIS_ENBLSLPM: u32 = 1 << 0;
/// DTB `snps,dis_u1_entry_quirk`. Carried for the record; mainline host ignores it. — TIMBERDOODLE
pub const QUIRK_DIS_U1_ENTRY: u32 = 1 << 1;
/// DTB `snps,dis_u2_entry_quirk`. Carried for the record; mainline host ignores it. — TIMBERDOODLE
pub const QUIRK_DIS_U2_ENTRY: u32 = 1 << 2;
/// DTB `snps,dis_u2_freeclk_exists-quirk`: clear GUSB2PHYCFG.U2_FREECLK_EXISTS. — TIMBERDOODLE
pub const QUIRK_DIS_U2_FREECLK_EXISTS: u32 = 1 << 3;
/// DTB `snps,dis-del-phy-power-chg-quirk`: clear GUSB3PIPECTL.DEPOCHANGE. — TIMBERDOODLE
pub const QUIRK_DIS_DEL_PHY_POWER_CHG: u32 = 1 << 4;
/// DTB `snps,dis-tx-ipgap-linecheck-quirk`. No host-mode register is defined here yet.
/// — TIMBERDOODLE
pub const QUIRK_DIS_TX_IPGAP_LINECHECK: u32 = 1 << 5;
/// DTB `snps,dis_rxdet_inp3_quirk`: set GUSB3PIPECTL.DISRXDETINP3. — TIMBERDOODLE
pub const QUIRK_DIS_RXDET_INP3: u32 = 1 << 6;

/// Bootstrap payload Sora sends to the first-light xHCI driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciProbeConfig {
    pub mmio_base: u64,
    pub mmio_length: u64,
    pub irq: u32,
    pub stream_id: u32,
    /// [`FLAG_DWC3`] plus any `QUIRK_*` bits the live DTB declares. Zero means the historical
    /// firmware-trust behavior. — TIMBERDOODLE
    pub flags: u32,
}

impl XhciProbeConfig {
    pub const fn new(mmio_base: u64, mmio_length: u64, irq: u32, stream_id: u32) -> Self {
        Self {
            mmio_base,
            mmio_length,
            irq,
            stream_id,
            flags: 0,
        }
    }

    pub const fn with_flags(self, flags: u32) -> Self {
        Self {
            mmio_base: self.mmio_base,
            mmio_length: self.mmio_length,
            irq: self.irq,
            stream_id: self.stream_id,
            flags,
        }
    }

    pub fn encode(self) -> [u8; XHCI_PROBE_CONFIG_LEN] {
        let mut out = [0u8; XHCI_PROBE_CONFIG_LEN];
        out[..4].copy_from_slice(&XHCI_PROBE_MAGIC);
        out[4..12].copy_from_slice(&self.mmio_base.to_le_bytes());
        out[12..20].copy_from_slice(&self.mmio_length.to_le_bytes());
        out[20..24].copy_from_slice(&self.irq.to_le_bytes());
        out[24..28].copy_from_slice(&self.stream_id.to_le_bytes());
        out[28..32].copy_from_slice(&self.flags.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < XHCI_PROBE_CONFIG_LEN || bytes[..4] != XHCI_PROBE_MAGIC {
            return None;
        }
        let mmio_base = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
        let mmio_length = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
        let irq = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
        let stream_id = u32::from_le_bytes(bytes[24..28].try_into().ok()?);
        let flags = u32::from_le_bytes(bytes[28..32].try_into().ok()?);
        if mmio_base == 0 || mmio_length == 0 {
            return None;
        }
        Some(Self {
            mmio_base,
            mmio_length,
            irq,
            stream_id,
            flags,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_config_round_trips_through_bootstrap_bytes() {
        let config = XhciProbeConfig::new(0x0a60_0000, 0xd950, 835, 0x820);
        assert_eq!(config.flags, 0);
        assert_eq!(XhciProbeConfig::decode(&config.encode()), Some(config));
        assert_eq!(XhciProbeConfig::decode(b"short"), None);
    }

    #[test]
    fn flags_ride_the_spare_word_and_older_senders_read_as_zero() {
        let flags =
            FLAG_DWC3 | QUIRK_DIS_ENBLSLPM | QUIRK_DIS_U2_FREECLK_EXISTS | QUIRK_DIS_RXDET_INP3;
        let config =
            XhciProbeConfig::new(0xfc40_0000, 0xd000, 253, XHCI_NO_STREAM_ID).with_flags(flags);
        assert_eq!(XhciProbeConfig::decode(&config.encode()), Some(config));

        let mut legacy = config.encode();
        legacy[28..32].fill(0);
        let decoded = XhciProbeConfig::decode(&legacy).unwrap();
        assert_eq!(decoded.flags, 0);
        assert_eq!(decoded.mmio_base, 0xfc40_0000);
    }

    #[test]
    fn quirk_bits_are_distinct() {
        let all = [
            QUIRK_DIS_ENBLSLPM,
            QUIRK_DIS_U1_ENTRY,
            QUIRK_DIS_U2_ENTRY,
            QUIRK_DIS_U2_FREECLK_EXISTS,
            QUIRK_DIS_DEL_PHY_POWER_CHG,
            QUIRK_DIS_TX_IPGAP_LINECHECK,
            QUIRK_DIS_RXDET_INP3,
        ];
        for (index, bit) in all.iter().enumerate() {
            assert_eq!(bit.count_ones(), 1);
            for other in &all[index + 1..] {
                assert_eq!(bit & other, 0);
            }
            assert_eq!(bit & FLAG_DWC3, 0);
        }
    }
}
