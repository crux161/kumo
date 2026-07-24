//j426
//j481

pub const XHCI_PROBE_CONFIG_LEN: usize = 32;
pub const XHCI_NO_STREAM_ID: u32 = u32::MAX;
const XHCI_PROBE_MAGIC: [u8; 4] = *b"XHCI";

/// Bootstrap payload Sora sends to the first-light xHCI driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XhciProbeConfig {
    pub mmio_base: u64,
    pub mmio_length: u64,
    pub irq: u32,
    pub stream_id: u32,
}

impl XhciProbeConfig {
    pub const fn new(mmio_base: u64, mmio_length: u64, irq: u32, stream_id: u32) -> Self {
        Self {
            mmio_base,
            mmio_length,
            irq,
            stream_id,
        }
    }

    pub fn encode(self) -> [u8; XHCI_PROBE_CONFIG_LEN] {
        let mut out = [0u8; XHCI_PROBE_CONFIG_LEN];
        out[..4].copy_from_slice(&XHCI_PROBE_MAGIC);
        out[4..12].copy_from_slice(&self.mmio_base.to_le_bytes());
        out[12..20].copy_from_slice(&self.mmio_length.to_le_bytes());
        out[20..24].copy_from_slice(&self.irq.to_le_bytes());
        out[24..28].copy_from_slice(&self.stream_id.to_le_bytes());
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
        if mmio_base == 0 || mmio_length == 0 {
            return None;
        }
        Some(Self {
            mmio_base,
            mmio_length,
            irq,
            stream_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_config_round_trips_through_bootstrap_bytes() {
        let config = XhciProbeConfig::new(0x0a60_0000, 0xd950, 835, 0x820);
        assert_eq!(XhciProbeConfig::decode(&config.encode()), Some(config));
        assert_eq!(XhciProbeConfig::decode(b"short"), None);
    }
}
