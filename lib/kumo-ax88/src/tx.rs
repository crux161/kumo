//j826
//! TX has two LE32 words before the unchanged Ethernet bytes. TSO/MSS is zero;
//! the exact USB max-packet boundary sets firmware padding controls, not extra
//! bytes or a guessed USB zero-length packet. — TIMBERDOODLE

use crate::{FRAME_MAX, FRAME_MIN};

pub const PREFIX_LEN: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    FrameLength,
    MaxPacket,
    OutputShort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    frame_len: usize,
    pad: bool,
}

impl Header {
    pub fn new(frame_len: usize, max_packet: u16) -> Result<Self, Error> {
        if !(FRAME_MIN..=FRAME_MAX).contains(&frame_len) {
            return Err(Error::FrameLength);
        }
        if !matches!(max_packet, 64 | 512 | 1024) {
            return Err(Error::MaxPacket);
        }
        Ok(Self {
            frame_len,
            pad: (frame_len + PREFIX_LEN) % usize::from(max_packet) == 0,
        })
    }
    pub const fn transfer_len(self) -> usize {
        self.frame_len + PREFIX_LEN
    }
    pub fn encode(self) -> [u8; PREFIX_LEN] {
        let length = (self.frame_len as u32).to_le_bytes();
        let control = if self.pad { 0x8000_8000u32 } else { 0 };
        let control = control.to_le_bytes();
        [
            length[0], length[1], length[2], length[3], control[0], control[1], control[2],
            control[3],
        ]
    }
}

/// Error leaves the output untouched; success returns only the initialized span.
pub fn encode(frame: &[u8], max_packet: u16, output: &mut [u8]) -> Result<usize, Error> {
    let header = Header::new(frame.len(), max_packet)?;
    let size = header.transfer_len();
    let out = output.get_mut(..size).ok_or(Error::OutputShort)?;
    out[..PREFIX_LEN].copy_from_slice(&header.encode());
    out[PREFIX_LEN..].copy_from_slice(frame);
    Ok(size)
}

#[cfg(test)]
mod tests;
