//j826
//j835
//! Explicit vendor/device control setups and little-endian payloads. Constructors
//! expose MAC and PHY access only; no EEPROM, eFuse or reload opcode is representable.
//! No request is submitted here. PHY page selection is never implicit. — TIMBERDOODLE

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    In,
    Out,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    ReadOnly,
    Length,
    PhyRegister,
    CompletionLength,
    InvalidMac,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacRegister {
    UsbLink,
    SiliconStatus,
    RxControl,
    NodeId,
    Medium,
    PhyPowerReset,
    RxBulkQueue,
    RxChecksum,
    TxChecksum,
    Clock,
}

impl MacRegister {
    pub const fn address(self) -> u16 {
        match self {
            Self::UsbLink => 0x02,
            Self::SiliconStatus => 0x03,
            Self::RxControl => 0x0b,
            Self::NodeId => 0x10,
            Self::Medium => 0x22,
            Self::PhyPowerReset => 0x26,
            Self::RxBulkQueue => 0x2e,
            Self::RxChecksum => 0x34,
            Self::TxChecksum => 0x35,
            Self::Clock => 0x33,
        }
    }
    pub const fn width(self) -> usize {
        match self {
            Self::RxControl | Self::Medium | Self::PhyPowerReset => 2,
            Self::NodeId => 6,
            Self::RxBulkQueue => 5,
            _ => 1,
        }
    }
    const fn writable(self) -> bool {
        !matches!(self, Self::UsbLink | Self::SiliconStatus | Self::NodeId)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    setup: [u8; 8],
    payload: [u8; 6],
}

impl Request {
    fn new(direction: Direction, command: u8, value: u16, index: u16, length: u16) -> Self {
        let v = value.to_le_bytes();
        let i = index.to_le_bytes();
        let n = length.to_le_bytes();
        Self {
            setup: [
                if direction == Direction::In {
                    0xc0
                } else {
                    0x40
                },
                command,
                v[0],
                v[1],
                i[0],
                i[1],
                n[0],
                n[1],
            ],
            payload: [0; 6],
        }
    }
    pub fn mac_read(register: MacRegister) -> Self {
        let width = register.width() as u16;
        Self::new(Direction::In, 1, register.address(), width, width)
    }
    /// Raw register bytes, not host-endian integers. Callers select and document the
    /// volatile write sequence/readback in NET5b; this is only its wire codec.
    pub fn mac_write(register: MacRegister, payload: &[u8]) -> Result<Self, Error> {
        if !register.writable() {
            return Err(Error::ReadOnly);
        }
        if payload.len() != register.width() {
            return Err(Error::Length);
        }
        let mut request = Self::mac_read(register);
        request.setup[0] = 0x40;
        request.payload[..payload.len()].copy_from_slice(payload);
        Ok(request)
    }
    /// The reference link_reset()'s TX FIFO full/empty poll: vendor request 0x81,
    /// register 0x8c, four bytes, read-only status. No state changes.
    pub fn fifo_status() -> Self {
        Self::new(Direction::In, 0x81, 0x8c, 0, 4)
    }
    pub fn phy_read(register: u8) -> Result<Self, Error> {
        if register > 31 {
            return Err(Error::PhyRegister);
        }
        Ok(Self::new(Direction::In, 2, 3, u16::from(register), 2))
    }
    pub fn phy_write(register: u8, value: u16) -> Result<Self, Error> {
        let mut request = Self::phy_read(register)?;
        request.setup[0] = 0x40;
        request.payload[..2].copy_from_slice(&value.to_le_bytes());
        Ok(request)
    }
    pub const fn setup(&self) -> [u8; 8] {
        self.setup
    }
    pub const fn direction(&self) -> Direction {
        if self.setup[0] & 0x80 != 0 {
            Direction::In
        } else {
            Direction::Out
        }
    }
    pub fn transfer_len(&self) -> usize {
        usize::from(u16::from_le_bytes([self.setup[6], self.setup[7]]))
    }
    pub fn outbound(&self) -> &[u8] {
        if self.direction() == Direction::Out {
            &self.payload[..self.transfer_len()]
        } else {
            &[]
        }
    }
    /// Call only after a successful data AND status stage. USB transport owns those
    /// completion/error/deadline proofs; short reads/writes are never padded as success.
    pub fn validate_transferred(&self, actual: usize) -> Result<(), Error> {
        if actual == self.transfer_len() {
            Ok(())
        } else {
            Err(Error::CompletionLength)
        }
    }
}

pub fn decode_mac(bytes: &[u8]) -> Result<[u8; 6], Error> {
    let address: [u8; 6] = bytes.try_into().map_err(|_| Error::Length)?;
    if address[0] & 1 != 0 || address == [0; 6] {
        return Err(Error::InvalidMac);
    }
    Ok(address)
}

#[cfg(test)]
mod tests;
