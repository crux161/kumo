#![no_std]

//! `drv-blk` — a minimal ramdisk block device over a VMO.
//!
//! Mirrors soso's `ramdisk.c`: a linear block store backed by a single VMO that
//! the caller (sora) passes in at bootstrap. The block size is fixed at 512 bytes
//! (the FAT sector size); a read/write at LBA `lba` translates to a VMO offset
//! at `lba * 512`.

/// Logical block size — matches FAT and the historical MBR/GPT sector.
pub const BLOCK_SIZE: u64 = 512;

/// Command byte: read blocks.
pub const CMD_READ: u8 = 0x00;
/// Command byte: write blocks. A write frame carries its data after the header (see
/// [`split_write_frame`]).
pub const CMD_WRITE: u8 = 0x01;

/// Response status: success.
pub const STATUS_OK: u8 = 0x00;
/// Response status: LBA out of range (or beyond the backing store's mapped extent).
pub const STATUS_BAD_LBA: u8 = 0x01;
/// Response status: a write frame's data length did not match `count` blocks.
pub const STATUS_BAD_LEN: u8 = 0x02;

/// A block device backed by a contiguous VMO.
pub struct BlockDevice {
    /// Total number of logical blocks.
    block_count: u64,
}

impl BlockDevice {
    /// Build a `BlockDevice` from a VMO of `vmo_len` bytes.
    pub const fn new(vmo_len: u64) -> Self {
        Self {
            block_count: vmo_len / BLOCK_SIZE,
        }
    }

    /// Number of logical blocks in the device.
    pub const fn block_count(&self) -> u64 {
        self.block_count
    }

    /// Return the VMO byte offset for `lba`.
    pub const fn offset_for_lba(lba: u64) -> u64 {
        lba * BLOCK_SIZE
    }

    /// Check whether `lba` through `lba + count` is within bounds.
    pub fn check_bounds(&self, lba: u64, count: u64) -> bool {
        count > 0 && lba < self.block_count && lba.saturating_add(count) <= self.block_count
    }

    /// Byte span `[offset, offset + count*BLOCK_SIZE)` for `count` blocks at `lba`, or `None`
    /// if the request is out of the device's block range. Shared by read and write so the two
    /// use identical bounds/offset math.
    fn byte_span(&self, lba: u64, count: u16) -> Option<(usize, usize)> {
        if !self.check_bounds(lba, count as u64) {
            return None;
        }
        let offset = Self::offset_for_lba(lba) as usize;
        let len = count as usize * BLOCK_SIZE as usize;
        Some((offset, len))
    }

    /// Read `count` blocks at `lba` out of `store` (the mapped backing). Returns the byte
    /// slice on success, or [`STATUS_BAD_LBA`] when the request is out of the device's block
    /// range *or* beyond `store`'s mapped extent (a short mapping cannot fault here — it is a
    /// clean error). Pure logic over a slice, so it is host-testable without a real VMO.
    pub fn read_blocks<'a>(&self, store: &'a [u8], lba: u64, count: u16) -> Result<&'a [u8], u8> {
        let (offset, len) = self.byte_span(lba, count).ok_or(STATUS_BAD_LBA)?;
        store.get(offset..offset + len).ok_or(STATUS_BAD_LBA)
    }

    /// Write `data` (exactly `count` blocks) at `lba` into `store` (the mapped backing).
    /// Returns [`STATUS_OK`], [`STATUS_BAD_LBA`] (out of range / beyond the mapping), or
    /// [`STATUS_BAD_LEN`] (data length != `count * BLOCK_SIZE`). Pure logic over a slice.
    pub fn write_blocks(&self, store: &mut [u8], lba: u64, count: u16, data: &[u8]) -> u8 {
        let Some((offset, len)) = self.byte_span(lba, count) else {
            return STATUS_BAD_LBA;
        };
        if data.len() != len {
            return STATUS_BAD_LEN;
        }
        match store.get_mut(offset..offset + len) {
            Some(dst) => {
                dst.copy_from_slice(data);
                STATUS_OK
            }
            None => STATUS_BAD_LBA,
        }
    }
}

/// Wire length of a request frame: `[cmd: u8][lba: u64 LE][count: u16 LE]`.
pub const REQUEST_LEN: usize = 11;

/// A block request on the wire. This is the single source of truth for the
/// request frame, shared by the `drv-blk` server (decode) and its clients —
/// the `fatfs` server / sora (encode) — so the two ends can never drift. Mirrors
/// the `svc-health` `Request`/`Response` codec pattern (PLAN §6/§12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// [`CMD_READ`] or [`CMD_WRITE`].
    pub cmd: u8,
    /// Starting logical block address.
    pub lba: u64,
    /// Number of 512-byte blocks.
    pub count: u16,
}

impl Request {
    /// A read request for `count` blocks starting at `lba`.
    pub const fn read(lba: u64, count: u16) -> Request {
        Request {
            cmd: CMD_READ,
            lba,
            count,
        }
    }

    /// A write request for `count` blocks starting at `lba`. The `count * BLOCK_SIZE` data
    /// bytes travel on the wire immediately after the [`REQUEST_LEN`]-byte header; see
    /// [`split_write_frame`].
    pub const fn write(lba: u64, count: u16) -> Request {
        Request {
            cmd: CMD_WRITE,
            lba,
            count,
        }
    }

    /// Encode into the [`REQUEST_LEN`]-byte wire form.
    pub fn encode(&self) -> [u8; REQUEST_LEN] {
        let mut buf = [0u8; REQUEST_LEN];
        buf[0] = self.cmd;
        buf[1..9].copy_from_slice(&self.lba.to_le_bytes());
        buf[9..11].copy_from_slice(&self.count.to_le_bytes());
        buf
    }

    /// Decode a request frame; `None` if the buffer is shorter than [`REQUEST_LEN`].
    pub fn decode(raw: &[u8]) -> Option<Request> {
        if raw.len() < REQUEST_LEN {
            return None;
        }
        Some(Request {
            cmd: raw[0],
            lba: u64::from_le_bytes(raw[1..9].try_into().ok()?),
            count: u16::from_le_bytes(raw[9..11].try_into().ok()?),
        })
    }
}

/// Split a write frame `[header: REQUEST_LEN][data: count*BLOCK_SIZE]` into its request and
/// data slice. The server calls this to recover the payload that rides with a [`CMD_WRITE`]
/// request in one message. Returns:
/// - `Err(STATUS_BAD_LEN)` if the frame is shorter than the header, or the trailing data is
///   not exactly `count * BLOCK_SIZE` bytes;
/// - `Err(STATUS_BAD_LBA)` if the command byte is not [`CMD_WRITE`] (wrong framing);
/// - `Ok((request, data))` otherwise.
///
/// Single source of truth for the write wire form, shared encode (client) / decode (server)
/// like [`Request`], so the two ends cannot drift.
pub fn split_write_frame(raw: &[u8]) -> Result<(Request, &[u8]), u8> {
    let request = Request::decode(raw).ok_or(STATUS_BAD_LEN)?;
    if request.cmd != CMD_WRITE {
        return Err(STATUS_BAD_LBA);
    }
    let want = request.count as usize * BLOCK_SIZE as usize;
    let data = raw
        .get(REQUEST_LEN..REQUEST_LEN + want)
        .ok_or(STATUS_BAD_LEN)?;
    Ok((request, data))
}

/// Why a read response could not be interpreted as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseError {
    /// The response frame had no status byte.
    Empty,
    /// The server returned a non-OK status (e.g. [`STATUS_BAD_LBA`]).
    Status(u8),
}

/// Interpret a read response frame `[status: u8][data...]`: returns the data
/// slice on [`STATUS_OK`], otherwise the error status. The client uses this to
/// turn a reply into bytes (or a bounded error) without re-deriving the layout.
pub fn read_payload(resp: &[u8]) -> Result<&[u8], ResponseError> {
    match resp.first() {
        None => Err(ResponseError::Empty),
        Some(&STATUS_OK) => Ok(&resp[1..]),
        Some(&status) => Err(ResponseError::Status(status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_len_device_has_zero_blocks() {
        let dev = BlockDevice::new(0);
        assert_eq!(dev.block_count(), 0);
    }

    #[test]
    fn exact_block_multiple() {
        let dev = BlockDevice::new(1024);
        assert_eq!(dev.block_count(), 2);
    }

    #[test]
    fn partial_sector_ignored() {
        let dev = BlockDevice::new(1023);
        assert_eq!(dev.block_count(), 1);
    }

    #[test]
    fn offset_is_lba_times_block_size() {
        assert_eq!(BlockDevice::offset_for_lba(0), 0);
        assert_eq!(BlockDevice::offset_for_lba(1), 512);
        assert_eq!(BlockDevice::offset_for_lba(100), 51200);
    }

    #[test]
    fn bounds_check_catches_oob_lba() {
        let dev = BlockDevice::new(1024); // 2 blocks
        assert!(dev.check_bounds(0, 1));
        assert!(dev.check_bounds(1, 1));
        assert!(dev.check_bounds(0, 2));
        assert!(!dev.check_bounds(2, 1)); // LBA out of range
        assert!(!dev.check_bounds(0, 3)); // count exceeds blocks
    }

    #[test]
    fn request_round_trips() {
        let req = Request::read(0x1234_5678_9abc, 4);
        assert_eq!(req.cmd, CMD_READ);
        assert_eq!(Request::decode(&req.encode()), Some(req));
    }

    #[test]
    fn request_wire_layout_is_le() {
        let bytes = Request::read(0x0102, 0x0304).encode();
        assert_eq!(bytes.len(), REQUEST_LEN);
        assert_eq!(bytes[0], CMD_READ);
        assert_eq!(&bytes[1..9], &0x0102u64.to_le_bytes());
        assert_eq!(&bytes[9..11], &0x0304u16.to_le_bytes());
    }

    #[test]
    fn request_decode_rejects_short_frame() {
        assert_eq!(Request::decode(&[0u8; REQUEST_LEN - 1]), None);
    }

    #[test]
    fn read_payload_returns_data_on_ok() {
        assert_eq!(read_payload(&[STATUS_OK, 1, 2, 3]), Ok(&[1u8, 2, 3][..]));
    }

    #[test]
    fn read_payload_surfaces_bad_status() {
        assert_eq!(
            read_payload(&[STATUS_BAD_LBA]),
            Err(ResponseError::Status(STATUS_BAD_LBA))
        );
        assert_eq!(read_payload(&[]), Err(ResponseError::Empty));
    }

    #[test]
    fn write_then_read_back_returns_the_written_bytes() {
        let dev = BlockDevice::new(4 * BLOCK_SIZE); // 4 blocks
        let mut store = [0u8; 4 * BLOCK_SIZE as usize];
        let mut data = [0u8; BLOCK_SIZE as usize];
        data.iter_mut().for_each(|b| *b = 0xAB);
        // Write block 2, then read it back.
        assert_eq!(dev.write_blocks(&mut store, 2, 1, &data), STATUS_OK);
        assert_eq!(dev.read_blocks(&store, 2, 1), Ok(&data[..]));
        // Neighbours stay zero — the write touched exactly its block.
        assert_eq!(
            dev.read_blocks(&store, 1, 1),
            Ok(&[0u8; BLOCK_SIZE as usize][..])
        );
        assert_eq!(
            dev.read_blocks(&store, 3, 1),
            Ok(&[0u8; BLOCK_SIZE as usize][..])
        );
    }

    #[test]
    fn read_and_write_reject_out_of_range() {
        let dev = BlockDevice::new(2 * BLOCK_SIZE); // 2 blocks
        let mut store = [0u8; 2 * BLOCK_SIZE as usize];
        let data = [0u8; BLOCK_SIZE as usize];
        assert_eq!(dev.read_blocks(&store, 2, 1), Err(STATUS_BAD_LBA));
        assert_eq!(dev.read_blocks(&store, 0, 3), Err(STATUS_BAD_LBA));
        assert_eq!(dev.write_blocks(&mut store, 2, 1, &data), STATUS_BAD_LBA);
    }

    #[test]
    fn read_rejects_a_range_beyond_the_mapped_store() {
        // Device advertises 4 blocks but only 2 are mapped: a read of an unmapped-but-in-range
        // block is a clean error, never a fault.
        let dev = BlockDevice::new(4 * BLOCK_SIZE);
        let store = [0u8; 2 * BLOCK_SIZE as usize];
        assert_eq!(
            dev.read_blocks(&store, 0, 1),
            Ok(&store[..BLOCK_SIZE as usize])
        );
        assert_eq!(dev.read_blocks(&store, 3, 1), Err(STATUS_BAD_LBA));
    }

    #[test]
    fn write_rejects_mismatched_data_length() {
        let dev = BlockDevice::new(2 * BLOCK_SIZE);
        let mut store = [0u8; 2 * BLOCK_SIZE as usize];
        let short = [0u8; BLOCK_SIZE as usize - 1];
        assert_eq!(dev.write_blocks(&mut store, 0, 1, &short), STATUS_BAD_LEN);
    }

    #[test]
    fn split_write_frame_round_trips_header_and_data() {
        let mut frame = [0u8; REQUEST_LEN + BLOCK_SIZE as usize];
        frame[..REQUEST_LEN].copy_from_slice(&Request::write(5, 1).encode());
        frame[REQUEST_LEN..].iter_mut().for_each(|b| *b = 0xCD);
        let (req, data) = split_write_frame(&frame).unwrap();
        assert_eq!(req, Request::write(5, 1));
        assert_eq!(data.len(), BLOCK_SIZE as usize);
        assert!(data.iter().all(|&b| b == 0xCD));
    }

    #[test]
    fn split_write_frame_rejects_short_data_and_wrong_command() {
        // Header says 1 block but no data follows.
        let header = Request::write(0, 1).encode();
        assert_eq!(split_write_frame(&header), Err(STATUS_BAD_LEN));
        // A read command is not a valid write frame.
        let read = Request::read(0, 1).encode();
        assert_eq!(split_write_frame(&read), Err(STATUS_BAD_LBA));
    }
}
