#![no_std]

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
//! Scope (grown one provable slice at a time, `GUIDANCE/006 §6`): the minimum to
//! resolve a path and read its bytes — [`Request::Open`] and [`Request::Read`].
//! Larger reads currently return their bytes inline in the reply (bounded by the
//! IPC inline ceiling, J174); `DESIGN/001`'s zero-copy `Vmo` file-data transfer
//! is the throughput follow-up, not this gate.

/// Op code: open a file by absolute path. Reply: `(handle, size)` on success.
pub const OP_OPEN: u8 = 0x00;
/// Op code: read bytes from an open handle. Reply: `[VFS_OK][data…]`.
pub const OP_READ: u8 = 0x01;

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

/// Maximum path length accepted on the wire. ESP paths are short; this keeps an
/// `Open` frame bounded and well within the IPC inline ceiling.
pub const MAX_PATH: usize = 255;

/// Wire length of a [`Request::Read`] frame: `op(1) + handle(4) + offset(8) + len(4)`.
pub const READ_REQUEST_LEN: usize = 17;

/// Wire length of a successful `Open` reply: `status(1) + handle(4) + size(8)`.
pub const OPEN_REPLY_LEN: usize = 13;

/// A filesystem request on the wire, borrowing its path from the frame buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request<'a> {
    /// Open the file at absolute `path` (e.g. `b"/EFI/BOOT/BOOTAA64.EFI"`).
    Open { path: &'a [u8] },
    /// Read up to `len` bytes from `handle` starting at byte `offset`.
    Read { handle: u32, offset: u64, len: u32 },
}

impl<'a> Request<'a> {
    /// An open request for `path`.
    pub const fn open(path: &'a [u8]) -> Request<'a> {
        Request::Open { path }
    }

    /// A read request for `len` bytes of `handle` at `offset`.
    pub const fn read(handle: u32, offset: u64, len: u32) -> Request<'a> {
        Request::Read {
            handle,
            offset,
            len,
        }
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
}
