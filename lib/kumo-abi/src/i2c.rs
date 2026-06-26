//! I2C ABI for KUMO.
//!
//! Provides the IPC message structures for interacting with `drv-geni-i2c` and `svc-i2c`.
//!
//! // — OSPREY 2026-06-26 (d005)

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I2cOpcode {
    BusList = 1,
    Funcs = 2,
    SmbusReadByte = 3,
    SmbusWriteQuick = 4,
    Transfer = 5,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I2cRequestHeader {
    pub opcode: I2cOpcode,
    pub bus: u32,
    pub address: u16,
    pub _pad: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I2cSmbusReadByteResponse {
    pub status: i32,
    pub value: u8,
    pub _pad: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I2cSmbusWriteQuickResponse {
    pub status: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I2cFuncsResponse {
    pub status: i32,
    pub funcs: u32,
}

pub const I2C_FUNC_I2C: u32 = 0x00000001;
pub const I2C_FUNC_SMBUS_QUICK: u32 = 0x00010000;
pub const I2C_FUNC_SMBUS_READ_BYTE: u32 = 0x00020000;
pub const I2C_FUNC_I2C_TRANSFER: u32 = 0x00040000;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I2cTransferRequest {
    pub header: I2cRequestHeader,
    pub write_len: u16,
    pub read_len: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct I2cTransferResponse {
    pub status: i32,
    pub read_len: u16,
    pub _pad: u16,
}
