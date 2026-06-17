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
/// Command byte: write blocks (read-only ramdisk — returns OK but ignores data).
pub const CMD_WRITE: u8 = 0x01;

/// Response status: success.
pub const STATUS_OK: u8 = 0x00;
/// Response status: LBA out of range.
pub const STATUS_BAD_LBA: u8 = 0x01;

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
}
