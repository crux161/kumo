#![no_std]

use kumo_i2c_hid::{KeyboardTopology, SourceClock};

const MAGIC: [u8; 4] = *b"I2H1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    Truncated,
    BadMagic,
    InvalidMmio,
    UnsupportedBusFrequency,
    UnsupportedSourceClock,
    InvalidAddress,
}

/// Capability-adjacent bootstrap data. Authority remains in the separately transferred Resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeConfig {
    pub mmio_base: u64,
    pub mmio_length: u64,
    pub bus_frequency_hz: u32,
    pub source_clock: SourceClock,
    pub i2c_address: u8,
    pub hid_descriptor_register: u16,
}

impl ProbeConfig {
    pub const BYTES: usize = 32;

    pub fn for_x13s(topology: KeyboardTopology) -> Result<Self, ConfigError> {
        let config = Self {
            mmio_base: topology.controller_mmio_base,
            mmio_length: topology.controller_mmio_length,
            bus_frequency_hz: topology.bus_frequency_hz,
            source_clock: SourceClock::Mhz19_2,
            i2c_address: topology.i2c_address,
            hid_descriptor_register: topology.hid_descriptor_register,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn encode(self) -> [u8; Self::BYTES] {
        let mut raw = [0u8; Self::BYTES];
        raw[..4].copy_from_slice(&MAGIC);
        raw[4..12].copy_from_slice(&self.mmio_base.to_le_bytes());
        raw[12..20].copy_from_slice(&self.mmio_length.to_le_bytes());
        raw[20..24].copy_from_slice(&self.bus_frequency_hz.to_le_bytes());
        raw[24..28].copy_from_slice(&source_clock_hz(self.source_clock).to_le_bytes());
        raw[28] = self.i2c_address;
        raw[29..31].copy_from_slice(&self.hid_descriptor_register.to_le_bytes());
        raw
    }

    pub fn decode(raw: &[u8]) -> Result<Self, ConfigError> {
        if raw.len() < Self::BYTES {
            return Err(ConfigError::Truncated);
        }
        if raw[..4] != MAGIC {
            return Err(ConfigError::BadMagic);
        }
        let config = Self {
            mmio_base: u64::from_le_bytes(raw[4..12].try_into().unwrap()),
            mmio_length: u64::from_le_bytes(raw[12..20].try_into().unwrap()),
            bus_frequency_hz: u32::from_le_bytes(raw[20..24].try_into().unwrap()),
            source_clock: match u32::from_le_bytes(raw[24..28].try_into().unwrap()) {
                19_200_000 => SourceClock::Mhz19_2,
                32_000_000 => SourceClock::Mhz32,
                _ => return Err(ConfigError::UnsupportedSourceClock),
            },
            i2c_address: raw[28],
            hid_descriptor_register: u16::from_le_bytes(raw[29..31].try_into().unwrap()),
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(self) -> Result<(), ConfigError> {
        if self.mmio_base == 0
            || self.mmio_base & 0xfff != 0
            || self.mmio_length < 0x1000
            || self.mmio_length & 0xfff != 0
        {
            return Err(ConfigError::InvalidMmio);
        }
        if self.bus_frequency_hz != 400_000 {
            return Err(ConfigError::UnsupportedBusFrequency);
        }
        if self.i2c_address > 0x7f {
            return Err(ConfigError::InvalidAddress);
        }
        Ok(())
    }
}

const fn source_clock_hz(clock: SourceClock) -> u32 {
    match clock {
        SourceClock::Mhz19_2 => 19_200_000,
        SourceClock::Mhz32 => 32_000_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_i2c_hid::{GicInterrupt, GpioInterrupt};

    fn topology() -> KeyboardTopology {
        KeyboardTopology {
            controller_mmio_base: 0x0089_4000,
            controller_mmio_length: 0x4000,
            controller_interrupt: GicInterrupt {
                kind: 0,
                number: 0x24b,
                flags: 4,
            },
            bus_frequency_hz: 400_000,
            i2c_address: 0x68,
            hid_descriptor_register: 1,
            keyboard_interrupt: GpioInterrupt {
                controller_phandle: 1,
                pin: 104,
                flags: 8,
            },
        }
    }

    #[test]
    fn x13s_config_round_trips_without_carrying_authority() {
        let dtb = include_bytes!("../../../../sc8280xp-lenovo-thinkpad-x13s.dtb");
        let discovered = kumo_i2c_hid::discover_keyboard(dtb).unwrap();
        let config = ProbeConfig::for_x13s(discovered).unwrap();
        assert_eq!(ProbeConfig::decode(&config.encode()), Ok(config));
        assert_eq!(config.source_clock, SourceClock::Mhz19_2);
        assert_eq!(config.mmio_base, 0x0089_4000);
        assert_eq!(config.i2c_address, 0x68);
    }

    #[test]
    fn rejects_malformed_or_dangerous_bootstrap_data() {
        assert_eq!(ProbeConfig::decode(b"short"), Err(ConfigError::Truncated));
        let mut raw = ProbeConfig::for_x13s(topology()).unwrap().encode();
        raw[4] = 1;
        assert_eq!(ProbeConfig::decode(&raw), Err(ConfigError::InvalidMmio));
    }
}
