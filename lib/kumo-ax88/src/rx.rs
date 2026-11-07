//j826
//! RX: packet spans padded to eight bytes, a separate metadata table, and a final
//! little-endian count/offset trailer. The configured mode must supply the two-byte
//! IP-alignment pseudo header. No extra FCS bytes are removed. — TIMBERDOODLE

use crate::{AGGREGATE_MAX, FRAME_MAX, FRAME_MIN, METADATA_MAX};

const ALIGNMENT: usize = 2;
const CRC: u32 = 1 << 29;
const DROP: u32 = 1 << 31;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    TooShort,
    TooLong,
    Empty,
    TooManyEntries,
    MetadataBounds,
    MetadataTail,
    PacketBounds,
    DataLength,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DropReason {
    Crc,
    Hardware,
    Runt,
    Oversize,
}

/// Hardware checksum indications are telemetry, never permission to skip software
/// verification. Bits0/1 do not turn a structurally valid packet into a CRC/drop.
/// — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status(u32);

impl Status {
    pub const fn raw(self) -> u32 {
        self.0
    }
    pub const fn ip_checksum_error(self) -> bool {
        self.0 & 2 != 0
    }
    pub const fn transport_checksum_error(self) -> bool {
        self.0 & 1 != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Packet<'a> {
    Frame {
        bytes: &'a [u8],
        status: Status,
    },
    Dropped {
        wire_len: usize,
        reason: DropReason,
    },
    /// A zero-length metadata entry consumes no bytes in the packet-data region.
    Dummy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Counts {
    pub frames: usize,
    pub dropped: usize,
    pub dummy: usize,
}

/// A fully checked immutable transfer. The caller must first establish USB completion
/// and keep the backing buffer out of DMA reuse until these borrows/copies end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Aggregate<'a> {
    bytes: &'a [u8],
    metadata: usize,
    entries: usize,
    counts: Counts,
}

fn word(bytes: &[u8], at: usize) -> Result<u32, Error> {
    let end = at.checked_add(4).ok_or(Error::MetadataBounds)?;
    let b = bytes.get(at..end).ok_or(Error::MetadataBounds)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn packet<'a>(
    bytes: &'a [u8],
    metadata: usize,
    cursor: usize,
    status: u32,
) -> Result<(usize, Packet<'a>), Error> {
    let len = ((status >> 16) & 0x1fff) as usize;
    if len == 0 {
        return Ok((cursor, Packet::Dummy));
    }
    let padded = len.checked_add(7).ok_or(Error::PacketBounds)? & !7;
    let next = cursor.checked_add(padded).ok_or(Error::PacketBounds)?;
    if next > metadata {
        return Err(Error::PacketBounds);
    }
    let reason = if status & CRC != 0 {
        Some(DropReason::Crc)
    } else if status & DROP != 0 {
        Some(DropReason::Hardware)
    } else if len < ALIGNMENT + FRAME_MIN {
        Some(DropReason::Runt)
    } else if len > ALIGNMENT + FRAME_MAX {
        Some(DropReason::Oversize)
    } else {
        None
    };
    let p = if let Some(reason) = reason {
        Packet::Dropped {
            wire_len: len,
            reason,
        }
    } else {
        let end = cursor.checked_add(len).ok_or(Error::PacketBounds)?;
        let start = cursor.checked_add(ALIGNMENT).ok_or(Error::PacketBounds)?;
        let frame = bytes.get(start..end).ok_or(Error::PacketBounds)?;
        Packet::Frame {
            bytes: frame,
            status: Status(status),
        }
    };
    Ok((next, p))
}

impl<'a> Aggregate<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() < 4 {
            return Err(Error::TooShort);
        }
        if bytes.len() > AGGREGATE_MAX {
            return Err(Error::TooLong);
        }
        let trailer_at = bytes.len() - 4;
        let trailer = word(bytes, trailer_at)?;
        let entries = (trailer & 0xffff) as usize;
        let metadata = (trailer >> 16) as usize;
        if entries == 0 {
            return Err(Error::Empty);
        }
        if entries > METADATA_MAX {
            return Err(Error::TooManyEntries);
        }
        let table_bytes = entries.checked_mul(4).ok_or(Error::MetadataBounds)?;
        let table_end = metadata
            .checked_add(table_bytes)
            .ok_or(Error::MetadataBounds)?;
        if table_end > trailer_at {
            return Err(Error::MetadataBounds);
        }
        // Older firmware omits dummy words and/or this optional four-byte padding.
        // Its contents are unspecified; only its exact extent is validated. — TIMBERDOODLE
        if !matches!(trailer_at - table_end, 0 | 4) {
            return Err(Error::MetadataTail);
        }
        let mut cursor = 0;
        let mut counts = Counts {
            frames: 0,
            dropped: 0,
            dummy: 0,
        };
        for i in 0..entries {
            let status = word(bytes, metadata + i * 4)?;
            let (next, p) = packet(bytes, metadata, cursor, status)?;
            cursor = next;
            match p {
                Packet::Frame { .. } => counts.frames += 1,
                Packet::Dropped { .. } => counts.dropped += 1,
                Packet::Dummy => counts.dummy += 1,
            }
        }
        if cursor != metadata {
            return Err(Error::DataLength);
        }
        Ok(Self {
            bytes,
            metadata,
            entries,
            counts,
        })
    }

    pub const fn counts(&self) -> Counts {
        self.counts
    }
    pub const fn entries(&self) -> usize {
        self.entries
    }
    pub fn packets(&self) -> Packets<'a> {
        Packets {
            aggregate: *self,
            index: 0,
            cursor: 0,
        }
    }
}

pub struct Packets<'a> {
    aggregate: Aggregate<'a>,
    index: usize,
    cursor: usize,
}

impl<'a> Iterator for Packets<'a> {
    type Item = Packet<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.index == self.aggregate.entries {
            return None;
        }
        // Both passes read the same immutable slice and private bounds established by
        // parse; no caller can construct an unvalidated Aggregate. — TIMBERDOODLE
        let status = word(
            self.aggregate.bytes,
            self.aggregate.metadata + self.index * 4,
        )
        .ok()?;
        let (next, p) = packet(
            self.aggregate.bytes,
            self.aggregate.metadata,
            self.cursor,
            status,
        )
        .ok()?;
        self.index += 1;
        self.cursor = next;
        Some(p)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.aggregate.entries - self.index;
        (n, Some(n))
    }
}

impl ExactSizeIterator for Packets<'_> {}
impl core::iter::FusedIterator for Packets<'_> {}

#[cfg(test)]
mod tests;
