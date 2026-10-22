#![no_std]
//j514
//j547

//! `kumo-vfs` — the wire protocol for KUMO's filesystem servers.
//!
//! VEIL: the storage domain is **Houtu** (后土, Sovereign of Earth); this crate is
//! the protocol its sterile-named `vfs`/`fatfs` servers and their clients speak
//! over a `Channel`. Per `DESIGN/001`, a filesystem server never runs in the
//! kernel address space — it is an ordinary userspace process answering this
//! protocol, exactly as `drv-blk` answers its block protocol.
//!
//! This crate is the single source of truth for the request/reply frames, shared
//! by the server (decode requests / encode replies) and clients (encode requests
//! / parse replies) so the two ends can never drift — the same rationale as the
//! `drv-blk` codec (J166). It is `no_std` and allocation-free: every frame is
//! built into a caller-provided buffer, so it runs in any server or `kumo-rt`
//! client.
//!
//! Scope (grown one provable slice at a time, `GUIDANCE/006 §6`): resolve/open,
//! read, create, resize, write, and flush — the bounded surface Kilo's save path
//! needs — and, since J547, listing a directory for `ls`. Namespace mutation beyond create remains a later storage slice.
//! Larger reads currently return their bytes inline in the reply (bounded by the
//! IPC inline ceiling, J174); `DESIGN/001`'s zero-copy `Vmo` file-data transfer
//! is the throughput follow-up, not this gate.

/// Op code: open a file by absolute path. Reply: `(handle, size)` on success.
pub const OP_OPEN: u8 = 0x00;
/// Op code: read bytes from an open handle. Reply: `[VFS_OK][data…]`.
pub const OP_READ: u8 = 0x01;
/// Op code: open with explicit access/create options.
pub const OP_OPEN_OPTIONS: u8 = 0x02;
/// Op code: write inline bytes to an open handle at an explicit offset.
pub const OP_WRITE: u8 = 0x03;
/// Op code: change an open file's logical length.
pub const OP_SET_LEN: u8 = 0x04;
/// Op code: make preceding writes visible to the backing store.
pub const OP_FLUSH: u8 = 0x05;
/// Op code: list a directory from one of its entries on (J547). Reply: a [`DirReplyWriter`]
/// frame, `[VFS_OK][count]` then `count` records; a count of zero means the listing is done.
pub const OP_READ_DIR: u8 = 0x06;

/// Open option: the returned handle may read.
pub const OPEN_READ: u8 = 1 << 0;
/// Open option: the returned handle may write and resize.
pub const OPEN_WRITE: u8 = 1 << 1;
/// Open option: create a missing regular file.
pub const OPEN_CREATE: u8 = 1 << 2;
const OPEN_KNOWN: u8 = OPEN_READ | OPEN_WRITE | OPEN_CREATE;

/// Reply status: success.
pub const VFS_OK: u8 = 0x00;
/// Reply status: the path did not resolve to an entry.
pub const VFS_NOT_FOUND: u8 = 0x01;
/// Reply status: the handle is not an open file.
pub const VFS_BAD_HANDLE: u8 = 0x02;
/// Reply status: the path resolved to a directory, not a readable file.
pub const VFS_IS_DIR: u8 = 0x03;
/// Reply status: the request frame was malformed or its op code unknown.
pub const VFS_BAD_REQUEST: u8 = 0x04;
/// Reply status: the volume has no free directory slot or data cluster.
pub const VFS_NO_SPACE: u8 = 0x05;
/// Reply status: the handle's open options do not permit the operation.
pub const VFS_ACCESS_DENIED: u8 = 0x06;
/// Reply status: the backing store failed an I/O operation.
pub const VFS_IO_ERROR: u8 = 0x07;
/// Reply status: a directory was asked for, and the path names a file (J547).
pub const VFS_NOT_DIR: u8 = 0x08;

/// Directory-record flag: the entry is itself a directory.
pub const DIR_RECORD_DIRECTORY: u8 = 1 << 0;
/// Fixed bytes of one directory record: `flags(1) + size(8) + name_len(1)`.
const DIR_RECORD_HEADER: usize = 10;

/// Maximum path length accepted on the wire. ESP paths are short; this keeps an
/// `Open` frame bounded and well within the IPC inline ceiling.
pub const MAX_PATH: usize = 255;

/// Wire length of a [`Request::Read`] frame: `op(1) + handle(4) + offset(8) + len(4)`.
pub const READ_REQUEST_LEN: usize = 17;
/// Maximum inline data carried by one write request.
pub const MAX_WRITE_DATA: usize = 256;
/// Write header length: `op(1) + handle(4) + offset(8) + len(2)`.
pub const WRITE_HEADER_LEN: usize = 15;
/// Wire length of a [`Request::SetLen`] frame.
pub const SET_LEN_REQUEST_LEN: usize = 13;
/// Wire length of a [`Request::Flush`] frame.
pub const FLUSH_REQUEST_LEN: usize = 5;

/// Wire length of a successful `Open` reply: `status(1) + handle(4) + size(8)`.
pub const OPEN_REPLY_LEN: usize = 13;
/// Wire length of a successful `Write` reply: `status(1) + written(4)`.
pub const WRITE_REPLY_LEN: usize = 5;

/// A filesystem request on the wire, borrowing its path from the frame buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request<'a> {
    /// Open the file at absolute `path` (e.g. `b"/EFI/BOOT/BOOTAA64.EFI"`).
    Open { path: &'a [u8] },
    /// Open with explicit access and creation options.
    OpenOptions { path: &'a [u8], options: u8 },
    /// Read up to `len` bytes from `handle` starting at byte `offset`.
    Read { handle: u32, offset: u64, len: u32 },
    /// Write `data` to `handle` starting at `offset`.
    Write {
        handle: u32,
        offset: u64,
        data: &'a [u8],
    },
    /// Change the logical length of `handle`.
    SetLen { handle: u32, len: u64 },
    /// Flush preceding mutations for `handle`.
    Flush { handle: u32 },
    /// List the directory at `path`, skipping its first `start` entries (J547).
    ReadDir { path: &'a [u8], start: u32 },
}

impl<'a> Request<'a> {
    /// An open request for `path`.
    pub const fn open(path: &'a [u8]) -> Request<'a> {
        Request::Open { path }
    }

    pub const fn open_options(path: &'a [u8], options: u8) -> Request<'a> {
        Request::OpenOptions { path, options }
    }

    /// A read request for `len` bytes of `handle` at `offset`.
    pub const fn read(handle: u32, offset: u64, len: u32) -> Request<'a> {
        Request::Read {
            handle,
            offset,
            len,
        }
    }

    pub const fn write(handle: u32, offset: u64, data: &'a [u8]) -> Request<'a> {
        Request::Write {
            handle,
            offset,
            data,
        }
    }

    pub const fn set_len(handle: u32, len: u64) -> Request<'a> {
        Request::SetLen { handle, len }
    }

    pub const fn flush(handle: u32) -> Request<'a> {
        Request::Flush { handle }
    }

    /// Encode into `buf`; returns the number of bytes written, or `None` if `buf`
    /// is too small or the path exceeds [`MAX_PATH`].
    pub fn encode_into(&self, buf: &mut [u8]) -> Option<usize> {
        match *self {
            Request::Open { path } => {
                if path.len() > MAX_PATH {
                    return None;
                }
                let n = 3 + path.len();
                let out = buf.get_mut(..n)?;
                out[0] = OP_OPEN;
                out[1..3].copy_from_slice(&(path.len() as u16).to_le_bytes());
                out[3..].copy_from_slice(path);
                Some(n)
            }
            Request::OpenOptions { path, options } => {
                if path.len() > MAX_PATH || options & !OPEN_KNOWN != 0 {
                    return None;
                }
                let n = 4 + path.len();
                let out = buf.get_mut(..n)?;
                out[0] = OP_OPEN_OPTIONS;
                out[1] = options;
                out[2..4].copy_from_slice(&(path.len() as u16).to_le_bytes());
                out[4..].copy_from_slice(path);
                Some(n)
            }
            Request::Read {
                handle,
                offset,
                len,
            } => {
                let out = buf.get_mut(..READ_REQUEST_LEN)?;
                out[0] = OP_READ;
                out[1..5].copy_from_slice(&handle.to_le_bytes());
                out[5..13].copy_from_slice(&offset.to_le_bytes());
                out[13..17].copy_from_slice(&len.to_le_bytes());
                Some(READ_REQUEST_LEN)
            }
            Request::Write {
                handle,
                offset,
                data,
            } => {
                if data.len() > MAX_WRITE_DATA {
                    return None;
                }
                let n = WRITE_HEADER_LEN + data.len();
                let out = buf.get_mut(..n)?;
                out[0] = OP_WRITE;
                out[1..5].copy_from_slice(&handle.to_le_bytes());
                out[5..13].copy_from_slice(&offset.to_le_bytes());
                out[13..15].copy_from_slice(&(data.len() as u16).to_le_bytes());
                out[WRITE_HEADER_LEN..].copy_from_slice(data);
                Some(n)
            }
            Request::SetLen { handle, len } => {
                let out = buf.get_mut(..SET_LEN_REQUEST_LEN)?;
                out[0] = OP_SET_LEN;
                out[1..5].copy_from_slice(&handle.to_le_bytes());
                out[5..13].copy_from_slice(&len.to_le_bytes());
                Some(SET_LEN_REQUEST_LEN)
            }
            Request::Flush { handle } => {
                let out = buf.get_mut(..FLUSH_REQUEST_LEN)?;
                out[0] = OP_FLUSH;
                out[1..5].copy_from_slice(&handle.to_le_bytes());
                Some(FLUSH_REQUEST_LEN)
            }
            Request::ReadDir { path, start } => {
                if path.len() > MAX_PATH {
                    return None;
                }
                let n = 7 + path.len();
                let out = buf.get_mut(..n)?;
                out[0] = OP_READ_DIR;
                out[1..5].copy_from_slice(&start.to_le_bytes());
                out[5..7].copy_from_slice(&(path.len() as u16).to_le_bytes());
                out[7..].copy_from_slice(path);
                Some(n)
            }
        }
    }

    /// Decode a request frame, borrowing the path for [`Request::Open`]. Returns
    /// `None` on a short or unknown frame.
    pub fn decode(raw: &'a [u8]) -> Option<Request<'a>> {
        match *raw.first()? {
            OP_OPEN => {
                let plen = u16::from_le_bytes(raw.get(1..3)?.try_into().ok()?) as usize;
                let path = raw.get(3..3 + plen)?;
                Some(Request::Open { path })
            }
            OP_READ => {
                let b = raw.get(..READ_REQUEST_LEN)?;
                Some(Request::Read {
                    handle: u32::from_le_bytes(b[1..5].try_into().ok()?),
                    offset: u64::from_le_bytes(b[5..13].try_into().ok()?),
                    len: u32::from_le_bytes(b[13..17].try_into().ok()?),
                })
            }
            OP_OPEN_OPTIONS => {
                let options = *raw.get(1)?;
                if options & !OPEN_KNOWN != 0 {
                    return None;
                }
                let plen = u16::from_le_bytes(raw.get(2..4)?.try_into().ok()?) as usize;
                let path = raw.get(4..4 + plen)?;
                Some(Request::OpenOptions { path, options })
            }
            OP_WRITE => {
                let header = raw.get(..WRITE_HEADER_LEN)?;
                let len = u16::from_le_bytes(header[13..15].try_into().ok()?) as usize;
                if len > MAX_WRITE_DATA {
                    return None;
                }
                Some(Request::Write {
                    handle: u32::from_le_bytes(header[1..5].try_into().ok()?),
                    offset: u64::from_le_bytes(header[5..13].try_into().ok()?),
                    data: raw.get(WRITE_HEADER_LEN..WRITE_HEADER_LEN + len)?,
                })
            }
            OP_SET_LEN => {
                let b = raw.get(..SET_LEN_REQUEST_LEN)?;
                Some(Request::SetLen {
                    handle: u32::from_le_bytes(b[1..5].try_into().ok()?),
                    len: u64::from_le_bytes(b[5..13].try_into().ok()?),
                })
            }
            OP_FLUSH => {
                let b = raw.get(..FLUSH_REQUEST_LEN)?;
                Some(Request::Flush {
                    handle: u32::from_le_bytes(b[1..5].try_into().ok()?),
                })
            }
            OP_READ_DIR => {
                let start = u32::from_le_bytes(raw.get(1..5)?.try_into().ok()?);
                let plen = u16::from_le_bytes(raw.get(5..7)?.try_into().ok()?) as usize;
                if plen > MAX_PATH {
                    return None;
                }
                Some(Request::ReadDir {
                    path: raw.get(7..7 + plen)?,
                    start,
                })
            }
            _ => None,
        }
    }
}

/// Encode a successful `Open` reply carrying the new `handle` and the file `size`.
pub fn encode_open_ok(handle: u32, size: u64) -> [u8; OPEN_REPLY_LEN] {
    let mut b = [0u8; OPEN_REPLY_LEN];
    b[0] = VFS_OK;
    b[1..5].copy_from_slice(&handle.to_le_bytes());
    b[5..13].copy_from_slice(&size.to_le_bytes());
    b
}

/// Encode a one-byte status reply — a failure code, or the framing for a reply
/// whose payload (the read data) the caller appends after [`VFS_OK`].
pub const fn encode_status(status: u8) -> [u8; 1] {
    [status]
}

/// Encode a successful write reply carrying the number of bytes committed.
pub fn encode_write_ok(written: u32) -> [u8; WRITE_REPLY_LEN] {
    let mut b = [0u8; WRITE_REPLY_LEN];
    b[0] = VFS_OK;
    b[1..5].copy_from_slice(&written.to_le_bytes());
    b
}

/// Builds a `ReadDir` reply in place: `[VFS_OK][count]`, then one record per entry,
/// `[flags][size: u64 LE][name_len][name]`, for as many entries as fit (J547).
pub struct DirReplyWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> DirReplyWriter<'a> {
    /// Start an empty listing in `buf`; `None` if it cannot hold even the header.
    pub fn new(buf: &'a mut [u8]) -> Option<DirReplyWriter<'a>> {
        let header = buf.get_mut(..2)?;
        header[0] = VFS_OK;
        header[1] = 0;
        Some(DirReplyWriter { buf, len: 2 })
    }

    /// Append one entry. Returns `false`, writing nothing, when it does not fit or the
    /// reply already holds 255 entries; the client then asks again from the next entry.
    pub fn push(&mut self, is_dir: bool, size: u64, name: &[u8]) -> bool {
        let n = DIR_RECORD_HEADER + name.len();
        if name.len() > u8::MAX as usize || self.buf[1] == u8::MAX {
            return false;
        }
        let Some(record) = self.buf.get_mut(self.len..self.len + n) else {
            return false;
        };
        record[0] = if is_dir { DIR_RECORD_DIRECTORY } else { 0 };
        record[1..9].copy_from_slice(&size.to_le_bytes());
        record[9] = name.len() as u8;
        record[DIR_RECORD_HEADER..].copy_from_slice(name);
        self.len += n;
        self.buf[1] += 1;
        true
    }

    /// Entries written so far.
    pub fn count(&self) -> u8 {
        self.buf[1]
    }

    /// The reply's length in bytes.
    pub fn finish(self) -> usize {
        self.len
    }
}

/// One entry of a `ReadDir` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirRecord<'a> {
    pub is_dir: bool,
    pub size: u64,
    pub name: &'a [u8],
}

/// The records of a `ReadDir` reply, in order. Iteration stops early at a truncated
/// record rather than reading past the frame.
#[derive(Debug, Clone)]
pub struct DirRecords<'a> {
    rest: &'a [u8],
    remaining: u8,
}

impl<'a> Iterator for DirRecords<'a> {
    type Item = DirRecord<'a>;

    fn next(&mut self) -> Option<DirRecord<'a>> {
        if self.remaining == 0 {
            return None;
        }
        let header = self.rest.get(..DIR_RECORD_HEADER)?;
        let name_len = header[9] as usize;
        let name = self
            .rest
            .get(DIR_RECORD_HEADER..DIR_RECORD_HEADER + name_len)?;
        let record = DirRecord {
            is_dir: header[0] & DIR_RECORD_DIRECTORY != 0,
            size: u64::from_le_bytes(header[1..9].try_into().ok()?),
            name,
        };
        self.rest = &self.rest[DIR_RECORD_HEADER + name_len..];
        self.remaining -= 1;
        Some(record)
    }
}

/// Parse a `ReadDir` reply: its entry count and records on [`VFS_OK`], else the error
/// status. A count of zero means the directory has no entries from the requested start.
pub fn parse_dir_reply(resp: &[u8]) -> Result<(u8, DirRecords<'_>), VfsError> {
    match resp.first() {
        None => Err(VfsError::Empty),
        Some(&VFS_OK) => {
            let count = *resp.get(1).ok_or(VfsError::Malformed)?;
            Ok((
                count,
                DirRecords {
                    rest: &resp[2..],
                    remaining: count,
                },
            ))
        }
        Some(&status) => Err(VfsError::Status(status)),
    }
}

/// Why a reply could not be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsError {
    /// The reply frame was empty (no status byte).
    Empty,
    /// The server returned a non-OK status (e.g. [`VFS_NOT_FOUND`]).
    Status(u8),
    /// The status was OK but the payload was the wrong length.
    Malformed,
}

/// Parse an `Open` reply: `(handle, size)` on [`VFS_OK`], else the error status.
pub fn parse_open_reply(resp: &[u8]) -> Result<(u32, u64), VfsError> {
    match resp.first() {
        None => Err(VfsError::Empty),
        Some(&VFS_OK) => {
            let b = resp.get(..OPEN_REPLY_LEN).ok_or(VfsError::Malformed)?;
            let handle = u32::from_le_bytes(b[1..5].try_into().map_err(|_| VfsError::Malformed)?);
            let size = u64::from_le_bytes(b[5..13].try_into().map_err(|_| VfsError::Malformed)?);
            Ok((handle, size))
        }
        Some(&status) => Err(VfsError::Status(status)),
    }
}

/// Interpret a `Read` reply `[status][data…]`: the data slice on [`VFS_OK`], else
/// the error status. Mirrors `drv_blk::read_payload`.
pub fn read_payload(resp: &[u8]) -> Result<&[u8], VfsError> {
    match resp.first() {
        None => Err(VfsError::Empty),
        Some(&VFS_OK) => Ok(&resp[1..]),
        Some(&status) => Err(VfsError::Status(status)),
    }
}

/// Parse the committed byte count from a `Write` reply.
pub fn parse_write_reply(resp: &[u8]) -> Result<u32, VfsError> {
    match resp.first() {
        None => Err(VfsError::Empty),
        Some(&VFS_OK) => {
            let b = resp.get(..WRITE_REPLY_LEN).ok_or(VfsError::Malformed)?;
            Ok(u32::from_le_bytes(
                b[1..5].try_into().map_err(|_| VfsError::Malformed)?,
            ))
        }
        Some(&status) => Err(VfsError::Status(status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_request_round_trips() {
        let req = Request::open(b"/EFI/BOOT/BOOTAA64.EFI");
        let mut buf = [0u8; 64];
        let n = req.encode_into(&mut buf).expect("encode");
        assert_eq!(Request::decode(&buf[..n]), Some(req));
    }

    #[test]
    fn read_request_round_trips() {
        let req = Request::read(0x0102_0304, 0x1122_3344_5566_7788, 256);
        let mut buf = [0u8; 64];
        let n = req.encode_into(&mut buf).expect("encode");
        assert_eq!(n, READ_REQUEST_LEN);
        assert_eq!(Request::decode(&buf[..n]), Some(req));
    }

    #[test]
    fn mutation_requests_round_trip() {
        let mut buf = [0u8; WRITE_HEADER_LEN + MAX_WRITE_DATA];
        let open = Request::open_options(b"/NOTES.TXT", OPEN_READ | OPEN_WRITE | OPEN_CREATE);
        let n = open.encode_into(&mut buf).unwrap();
        assert_eq!(Request::decode(&buf[..n]), Some(open));

        let write = Request::write(7, 0x1122, b"cloud");
        let n = write.encode_into(&mut buf).unwrap();
        assert_eq!(Request::decode(&buf[..n]), Some(write));

        let set_len = Request::set_len(7, 0x3344);
        let n = set_len.encode_into(&mut buf).unwrap();
        assert_eq!(Request::decode(&buf[..n]), Some(set_len));

        let flush = Request::flush(7);
        let n = flush.encode_into(&mut buf).unwrap();
        assert_eq!(Request::decode(&buf[..n]), Some(flush));
    }

    #[test]
    fn open_wire_layout_is_le() {
        let path = b"/HELLO.TXT";
        let mut buf = [0u8; 32];
        let n = Request::open(path).encode_into(&mut buf).unwrap();
        assert_eq!(buf[0], OP_OPEN);
        assert_eq!(&buf[1..3], &(path.len() as u16).to_le_bytes());
        assert_eq!(&buf[3..n], path);
    }

    #[test]
    fn read_wire_layout_is_le() {
        let mut buf = [0u8; 32];
        Request::read(0x0a0b_0c0d, 0x0102, 0x0304)
            .encode_into(&mut buf)
            .unwrap();
        assert_eq!(buf[0], OP_READ);
        assert_eq!(&buf[1..5], &0x0a0b_0c0du32.to_le_bytes());
        assert_eq!(&buf[5..13], &0x0102u64.to_le_bytes());
        assert_eq!(&buf[13..17], &0x0304u32.to_le_bytes());
    }

    #[test]
    fn decode_rejects_short_and_unknown_frames() {
        assert_eq!(Request::decode(&[]), None); // no op byte
        assert_eq!(Request::decode(&[0xFF]), None); // unknown op
        assert_eq!(Request::decode(&[OP_READ; READ_REQUEST_LEN - 1]), None); // short read
                                                                             // Open claiming a 4-byte path but only 1 byte present.
        assert_eq!(Request::decode(&[OP_OPEN, 4, 0, b'/']), None);
    }

    #[test]
    fn encode_into_rejects_small_buffer() {
        let mut tiny = [0u8; 2];
        assert_eq!(Request::open(b"/x").encode_into(&mut tiny), None);
        assert_eq!(Request::read(1, 0, 0).encode_into(&mut tiny), None);
        assert_eq!(Request::write(1, 0, b"x").encode_into(&mut tiny), None);
    }

    #[test]
    fn encode_rejects_unknown_open_options_and_oversized_writes() {
        let mut buf = [0u8; WRITE_HEADER_LEN + MAX_WRITE_DATA + 1];
        assert_eq!(
            Request::open_options(b"/x", OPEN_KNOWN | 0x80).encode_into(&mut buf),
            None
        );
        let data = [0u8; MAX_WRITE_DATA + 1];
        assert_eq!(Request::write(1, 0, &data).encode_into(&mut buf), None);
    }

    #[test]
    fn open_path_too_long_is_rejected() {
        let long = [b'a'; MAX_PATH + 1];
        let mut buf = [0u8; MAX_PATH + 8];
        assert_eq!(Request::open(&long).encode_into(&mut buf), None);
    }

    #[test]
    fn open_reply_round_trips() {
        let reply = encode_open_ok(7, 6);
        assert_eq!(reply.len(), OPEN_REPLY_LEN);
        assert_eq!(parse_open_reply(&reply), Ok((7, 6)));
    }

    #[test]
    fn parse_open_reply_surfaces_errors() {
        assert_eq!(parse_open_reply(&[]), Err(VfsError::Empty));
        assert_eq!(
            parse_open_reply(&encode_status(VFS_NOT_FOUND)),
            Err(VfsError::Status(VFS_NOT_FOUND))
        );
        // OK status but a truncated payload.
        assert_eq!(parse_open_reply(&[VFS_OK, 1, 2]), Err(VfsError::Malformed));
    }

    #[test]
    fn read_payload_ok_and_errors() {
        assert_eq!(read_payload(&[VFS_OK, b'e', b's', b'p']), Ok(&b"esp"[..]));
        assert_eq!(read_payload(&[]), Err(VfsError::Empty));
        assert_eq!(
            read_payload(&encode_status(VFS_IS_DIR)),
            Err(VfsError::Status(VFS_IS_DIR))
        );
    }

    #[test]
    fn write_reply_round_trips_and_surfaces_errors() {
        assert_eq!(parse_write_reply(&encode_write_ok(17)), Ok(17));
        assert_eq!(parse_write_reply(&[]), Err(VfsError::Empty));
        assert_eq!(
            parse_write_reply(&encode_status(VFS_NO_SPACE)),
            Err(VfsError::Status(VFS_NO_SPACE))
        );
        assert_eq!(parse_write_reply(&[VFS_OK, 1]), Err(VfsError::Malformed));
    }

    #[test]
    fn read_dir_request_round_trips() {
        let req = Request::ReadDir {
            path: b"/EFI/BOOT",
            start: 14,
        };
        let mut buf = [0u8; 64];
        let n = req.encode_into(&mut buf).expect("encode");
        assert_eq!(n, 7 + 9);
        assert_eq!(Request::decode(&buf[..n]), Some(req));
        let root = Request::ReadDir {
            path: b"/",
            start: 0,
        };
        let n = root.encode_into(&mut buf).expect("encode");
        assert_eq!(Request::decode(&buf[..n]), Some(root));
        assert_eq!(Request::decode(&buf[..6]), None);
    }

    #[test]
    fn a_dir_reply_carries_what_fits_and_reads_back() {
        let mut reply = [0u8; 2 + 2 * (DIR_RECORD_HEADER + 8) + 5];
        let mut writer = DirReplyWriter::new(&mut reply).unwrap();
        assert!(writer.push(true, 0, b"EFI"));
        assert!(writer.push(false, 0x36d, b"LOREM_JP.TXT"));
        // A third record does not fit: nothing is written and the count holds.
        assert!(!writer.push(false, 6, b"HELLO.TXT"));
        assert_eq!(writer.count(), 2);
        let len = writer.finish();
        assert_eq!(len, 2 + (DIR_RECORD_HEADER + 3) + (DIR_RECORD_HEADER + 12));

        let (count, mut records) = parse_dir_reply(&reply[..len]).unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            records.next(),
            Some(DirRecord {
                is_dir: true,
                size: 0,
                name: b"EFI"
            })
        );
        assert_eq!(
            records.next(),
            Some(DirRecord {
                is_dir: false,
                size: 0x36d,
                name: b"LOREM_JP.TXT"
            })
        );
        assert_eq!(records.next(), None);
    }

    #[test]
    fn an_empty_or_failed_dir_reply_is_recognised() {
        let (count, mut records) = parse_dir_reply(&[VFS_OK, 0]).unwrap();
        assert_eq!(count, 0);
        assert_eq!(records.next(), None);
        assert_eq!(
            parse_dir_reply(&[VFS_NOT_DIR]).err(),
            Some(VfsError::Status(VFS_NOT_DIR))
        );
        assert_eq!(parse_dir_reply(&[VFS_OK]).err(), Some(VfsError::Malformed));
        // A count that claims more than the frame holds stops at the frame's end.
        let (_, records) =
            parse_dir_reply(&[VFS_OK, 3, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, b'A']).unwrap();
        assert_eq!(records.count(), 1);
    }
}
