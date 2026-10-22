#![no_std]
//j166
//j413
//j414
//j545
//j550

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
/// Command byte: flush/sync. A no-op ack for the RAM-backed ramdisk (data is already durable
/// in RAM); it exists so a filesystem client can request a barrier without special-casing the
/// backend. Carries no data; `lba`/`count` are ignored.
pub const CMD_FLUSH: u8 = 0x02;

/// Response status: success.
pub const STATUS_OK: u8 = 0x00;
/// Response status: LBA out of range (or beyond the backing store's mapped extent).
pub const STATUS_BAD_LBA: u8 = 0x01;
/// Response status: a write frame's data length did not match `count` blocks.
pub const STATUS_BAD_LEN: u8 = 0x02;
/// Response status: this device serves reads only (J545: a USB stick before WRITE(10) lands).
pub const STATUS_READ_ONLY: u8 = 0x03;
/// Response status: the device failed the transfer (a USB read that did not complete).
pub const STATUS_IO_ERROR: u8 = 0x04;

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

    /// A flush/sync request. `lba`/`count` are unused (zero).
    pub const fn flush() -> Request {
        Request {
            cmd: CMD_FLUSH,
            lba: 0,
            count: 0,
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

// --- Control channel between a block driver and Sora (J545) ---
//
// A driver that owns removable media (drv-xhci's USB stick) does not know who will read it.
// It tells Sora over a control channel when a volume is ready; Sora then hands it one fresh
// channel per client, each of which this block protocol runs over. The driver narrows every
// client to the volume: LBA 0 on that channel is the partition's first block.

/// Control message, driver to Sora: a volume is ready to serve. See [`MediaReady`].
pub const CONTROL_MEDIA_READY: u8 = b'V';
/// Control message, Sora to driver: the one transferred handle is a new client's channel.
pub const CONTROL_CONNECT: u8 = b'C';
/// Wire length of a connect request, excluding its transferred channel handle.
pub const CONTROL_CONNECT_LEN: usize = 2;

/// Authority fixed on one block channel when Sora connects it to the driver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BlockAccess {
    #[default]
    ReadOnly,
    ReadWrite,
}

/// Attach the requested authority to a newly transferred block channel.
pub const fn encode_connect(access: BlockAccess) -> [u8; CONTROL_CONNECT_LEN] {
    [
        CONTROL_CONNECT,
        match access {
            BlockAccess::ReadOnly => 0,
            BlockAccess::ReadWrite => 1,
        },
    ]
}

/// Decode only known access modes. The old one-byte connect grants read access only;
/// write authority must be explicit, never inferred from a writable volume. — KESTREL
pub fn decode_connect(raw: &[u8]) -> Option<BlockAccess> {
    match raw {
        [CONTROL_CONNECT] | [CONTROL_CONNECT, 0] => Some(BlockAccess::ReadOnly),
        [CONTROL_CONNECT, 1] => Some(BlockAccess::ReadWrite),
        _ => None,
    }
}

/// Wire length of [`MediaReady`]: `[tag][flags][blocks: u64][serial: u32][label: 11]`.
pub const MEDIA_READY_LEN: usize = 25;
const MEDIA_HAS_LABEL: u8 = 1 << 0;
const MEDIA_READ_ONLY: u8 = 1 << 1;

/// What a driver announces about a volume it can serve: enough for Sora to name its mount point
/// (FHS `/media/<label>`, DESIGN/012 §2.1) and bound its clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaReady {
    /// Blocks of [`BLOCK_SIZE`] bytes a client may address, from 0.
    pub block_count: u64,
    /// The filesystem's serial (FAT32 `BS_VolID`), the name of last resort.
    pub serial: u32,
    /// The volume label, space-padded, when the volume has one.
    pub label: Option<[u8; 11]>,
    /// Whether writes are refused with [`STATUS_READ_ONLY`].
    pub read_only: bool,
}

impl MediaReady {
    pub fn encode(&self) -> [u8; MEDIA_READY_LEN] {
        let mut buf = [0u8; MEDIA_READY_LEN];
        buf[0] = CONTROL_MEDIA_READY;
        buf[1] = if self.label.is_some() {
            MEDIA_HAS_LABEL
        } else {
            0
        } | if self.read_only { MEDIA_READ_ONLY } else { 0 };
        buf[2..10].copy_from_slice(&self.block_count.to_le_bytes());
        buf[10..14].copy_from_slice(&self.serial.to_le_bytes());
        buf[14..25].copy_from_slice(&self.label.unwrap_or([b' '; 11]));
        buf
    }

    /// Decode an announcement; `None` for a short frame, another tag, or unknown flags.
    pub fn decode(raw: &[u8]) -> Option<MediaReady> {
        let raw = raw.get(..MEDIA_READY_LEN)?;
        let flags = raw[1];
        if raw[0] != CONTROL_MEDIA_READY || flags & !(MEDIA_HAS_LABEL | MEDIA_READ_ONLY) != 0 {
            return None;
        }
        Some(MediaReady {
            block_count: u64::from_le_bytes(raw[2..10].try_into().ok()?),
            serial: u32::from_le_bytes(raw[10..14].try_into().ok()?),
            label: (flags & MEDIA_HAS_LABEL != 0).then(|| raw[14..25].try_into().unwrap()),
            read_only: flags & MEDIA_READ_ONLY != 0,
        })
    }
}

/// First byte of the bootstrap a block-backed filesystem server receives (J545):
/// `[CLIENT_BOOTSTRAP_TAG][block_count: u64 LE]`, carrying the client channel as its handle.
pub const CLIENT_BOOTSTRAP_TAG: u8 = b'B';
/// Wire length of a client bootstrap.
pub const CLIENT_BOOTSTRAP_LEN: usize = 9;

/// Encode the bootstrap that hands a filesystem server a volume of `block_count` blocks.
pub fn encode_client_bootstrap(block_count: u64) -> [u8; CLIENT_BOOTSTRAP_LEN] {
    let mut buf = [0u8; CLIENT_BOOTSTRAP_LEN];
    buf[0] = CLIENT_BOOTSTRAP_TAG;
    buf[1..].copy_from_slice(&block_count.to_le_bytes());
    buf
}

/// The volume's block count from a client bootstrap; `None` for any other message, such as the
/// RAM image's `[base][len]` header, which is 16 bytes.
pub fn decode_client_bootstrap(raw: &[u8]) -> Option<u64> {
    if raw.len() != CLIENT_BOOTSTRAP_LEN || raw[0] != CLIENT_BOOTSTRAP_TAG {
        return None;
    }
    Some(u64::from_le_bytes(raw[1..].try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_grants_only_its_explicit_access() {
        assert_eq!(BlockAccess::default(), BlockAccess::ReadOnly);
        assert_eq!(encode_connect(BlockAccess::ReadOnly), [CONTROL_CONNECT, 0]);
        assert_eq!(encode_connect(BlockAccess::ReadWrite), [CONTROL_CONNECT, 1]);
        for access in [BlockAccess::ReadOnly, BlockAccess::ReadWrite] {
            assert_eq!(decode_connect(&encode_connect(access)), Some(access));
        }
        assert_eq!(
            decode_connect(&[CONTROL_CONNECT]),
            Some(BlockAccess::ReadOnly)
        );
        for raw in [
            &[][..],
            &[CONTROL_CONNECT, 2],
            &[CONTROL_CONNECT, 0xFF],
            &[CONTROL_CONNECT, 1, 0],
            &[b'X', 1],
        ] {
            assert_eq!(decode_connect(raw), None);
        }
    }

    #[test]
    fn a_client_bootstrap_round_trips_and_is_told_from_the_image_header() {
        let wire = encode_client_bootstrap(0x732_3800);
        assert_eq!(decode_client_bootstrap(&wire), Some(0x732_3800));
        // The RAM-image bootstrap fatfs also accepts: `[base: u64][len: u64]`.
        let mut image_header = [0u8; 16];
        image_header[0] = CLIENT_BOOTSTRAP_TAG;
        assert_eq!(decode_client_bootstrap(&image_header), None);
        assert_eq!(decode_client_bootstrap(&wire[..8]), None);
    }

    #[test]
    fn a_media_announcement_round_trips() {
        let stick = MediaReady {
            block_count: 0x732_3800,
            serial: 0x4567_13f4,
            label: Some(*b"STICKY     "),
            read_only: true,
        };
        let wire = stick.encode();
        assert_eq!(wire[0], CONTROL_MEDIA_READY);
        assert_eq!(MediaReady::decode(&wire), Some(stick));
        let unnamed = MediaReady {
            label: None,
            read_only: false,
            ..stick
        };
        assert_eq!(MediaReady::decode(&unnamed.encode()), Some(unnamed));
    }

    #[test]
    fn a_malformed_announcement_is_refused() {
        let wire = MediaReady {
            block_count: 1,
            serial: 2,
            label: None,
            read_only: true,
        }
        .encode();
        assert_eq!(MediaReady::decode(&wire[..MEDIA_READY_LEN - 1]), None);
        let mut other_tag = wire;
        other_tag[0] = CONTROL_CONNECT;
        assert_eq!(MediaReady::decode(&other_tag), None);
        let mut unknown_flag = wire;
        unknown_flag[1] |= 0x80;
        assert_eq!(MediaReady::decode(&unknown_flag), None);
    }

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

    #[test]
    fn flush_request_round_trips() {
        let req = Request::flush();
        assert_eq!(req.cmd, CMD_FLUSH);
        assert_eq!(Request::decode(&req.encode()), Some(req));
    }
}
