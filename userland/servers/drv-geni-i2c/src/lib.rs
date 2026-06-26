#![no_std]

pub use kumo_geni_i2c::SourceClock;

const MAGIC: [u8; 4] = *b"I2G1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    Truncated,
    BadMagic,
    InvalidMmio,
    UnsupportedBusFrequency,
    UnsupportedSourceClock,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeConfig {
    pub mmio_base: u64,
    pub mmio_length: u64,
    pub bus_frequency_hz: u32,
    pub source_clock: SourceClock,
}

impl ProbeConfig {
    pub const BYTES: usize = 32;

    pub fn encode(self) -> [u8; Self::BYTES] {
        let mut raw = [0u8; Self::BYTES];
        raw[..4].copy_from_slice(&MAGIC);
        raw[4..12].copy_from_slice(&self.mmio_base.to_le_bytes());
        raw[12..20].copy_from_slice(&self.mmio_length.to_le_bytes());
        raw[20..24].copy_from_slice(&self.bus_frequency_hz.to_le_bytes());
        raw[24..28].copy_from_slice(&source_clock_hz(self.source_clock).to_le_bytes());
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
        Ok(())
    }
}

const fn source_clock_hz(clock: SourceClock) -> u32 {
    match clock {
        SourceClock::Mhz19_2 => 19_200_000,
        SourceClock::Mhz32 => 32_000_000,
    }
}
// — OSPREY 2026-06-26 (d007)
