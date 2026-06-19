#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//! `fatfs` — the FAT-backed vfs handler core.
//!
//! FAT is not KUMO's root filesystem (`DESIGN/010`); it remains the ESP /
//! removable-media filesystem. This crate is the host-testable server logic that
//! maps `kumo-vfs` Open/Read requests onto `kumo-fatfs`. It owns no device and
//! has no runtime dependency: a resident binary will later wrap this handler with
//! a `Channel` transport and a `SectorReader` over `drv-blk`.
//!
//! The durable truth is still the mounted FAT volume. The per-server open table is
//! reconnectable soft state, so a supervised restart can rebuild it by reopening
//! paths rather than recovering private RAM.

#[cfg(test)]
extern crate alloc;

use kumo_fatfs::{DirEntry, FatVolume, SectorReader};
use kumo_vfs::{
    encode_open_ok, encode_status, Request, MAX_PATH, VFS_BAD_HANDLE, VFS_BAD_REQUEST, VFS_IS_DIR,
    VFS_NOT_FOUND, VFS_OK,
};

/// Maximum number of open file handles in this first fixed table.
pub const MAX_OPEN_FILES: usize = 8;

/// Largest `Open` request frame accepted by `kumo-vfs`.
pub const REQUEST_BUF_BYTES: usize = 3 + MAX_PATH;

/// Inline read payload cap for this first userspace loop, plus one status byte.
pub const REPLY_BUF_BYTES: usize = 1 + 256;

/// Per-server open file table. Handles are small positive integers; `0` is
/// deliberately invalid so protocol bugs are easy to spot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenTable {
    slots: [Option<DirEntry>; MAX_OPEN_FILES],
}

impl OpenTable {
    pub const fn new() -> OpenTable {
        OpenTable {
            slots: [None; MAX_OPEN_FILES],
        }
    }

    pub fn insert(&mut self, entry: DirEntry) -> Option<u32> {
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(entry);
                return Some((index + 1) as u32);
            }
        }
        None
    }

    pub fn get(&self, handle: u32) -> Option<DirEntry> {
        let index = usize::try_from(handle).ok()?.checked_sub(1)?;
        self.slots.get(index).copied().flatten()
    }
}

impl Default for OpenTable {
    fn default() -> Self {
        Self::new()
    }
}

/// The request/reply transport the server runs over. The real implementation
/// will wrap a KUMO `Channel`; tests use an in-memory fake so the serve loop is
/// host-testable before the resident binary exists.
pub trait Transport {
    /// Receive one request frame into `buf`, returning its length, or `None` when
    /// the peer has closed and the serve loop should exit.
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;

    /// Send one reply frame.
    fn send(&mut self, frame: &[u8]);
}

/// FAT vfs server state. The open table is connection-local soft state; file
/// bytes and metadata remain disk-backed through [`FatVolume`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FatServer {
    open_files: OpenTable,
}

impl FatServer {
    pub const fn new() -> FatServer {
        FatServer {
            open_files: OpenTable::new(),
        }
    }

    /// Handle exactly one transport request/reply exchange. Returns `false` when
    /// the transport closes before yielding a request.
    pub fn serve_once<R: SectorReader, T: Transport>(
        &mut self,
        volume: &FatVolume,
        reader: &mut R,
        transport: &mut T,
        request_buf: &mut [u8],
        reply_buf: &mut [u8],
    ) -> bool {
        let Some(n) = transport.recv(request_buf) else {
            return false;
        };
        let reply_len = dispatch(
            volume,
            reader,
            &mut self.open_files,
            &request_buf[..n],
            reply_buf,
        );
        if reply_len != 0 {
            transport.send(&reply_buf[..reply_len]);
        }
        true
    }

    /// Run request/reply exchanges until the transport closes.
    pub fn serve<R: SectorReader, T: Transport>(
        &mut self,
        volume: &FatVolume,
        reader: &mut R,
        transport: &mut T,
    ) {
        let mut request_buf = [0u8; REQUEST_BUF_BYTES];
        let mut reply_buf = [0u8; REPLY_BUF_BYTES];
        while self.serve_once(volume, reader, transport, &mut request_buf, &mut reply_buf) {}
    }
}

impl Default for FatServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle one vfs request frame, writing the reply into `reply` and returning
/// the number of reply bytes. Malformed frames and too-small reply buffers return
/// a one-byte [`VFS_BAD_REQUEST`] status when possible.
pub fn dispatch<R: SectorReader>(
    volume: &FatVolume,
    reader: &mut R,
    open_files: &mut OpenTable,
    request: &[u8],
    reply: &mut [u8],
) -> usize {
    match Request::decode(request) {
        Some(Request::Open { path }) => open(volume, reader, open_files, path, reply),
        Some(Request::Read {
            handle,
            offset,
            len,
        }) => read(volume, reader, open_files, handle, offset, len, reply),
        None => write_status(reply, VFS_BAD_REQUEST),
    }
}

fn open<R: SectorReader>(
    volume: &FatVolume,
    reader: &mut R,
    open_files: &mut OpenTable,
    path: &[u8],
    reply: &mut [u8],
) -> usize {
    let Some(entry) = volume.resolve_path(reader, path) else {
        return write_status(reply, VFS_NOT_FOUND);
    };
    if entry.is_dir() {
        return write_status(reply, VFS_IS_DIR);
    }
    let Some(handle) = open_files.insert(entry) else {
        return write_status(reply, VFS_BAD_HANDLE);
    };
    write_frame(reply, &encode_open_ok(handle, entry.size as u64))
}

fn read<R: SectorReader>(
    volume: &FatVolume,
    reader: &mut R,
    open_files: &OpenTable,
    handle: u32,
    offset: u64,
    len: u32,
    reply: &mut [u8],
) -> usize {
    if reply.is_empty() {
        return 0;
    }
    let Some(entry) = open_files.get(handle) else {
        return write_status(reply, VFS_BAD_HANDLE);
    };

    reply[0] = VFS_OK;
    let cap = (len as usize).min(reply.len() - 1);
    let n = volume.read_file_at(reader, &entry, offset, &mut reply[1..1 + cap]);
    1 + n
}

fn write_status(reply: &mut [u8], status: u8) -> usize {
    write_frame(reply, &encode_status(status))
}

fn write_frame(reply: &mut [u8], frame: &[u8]) -> usize {
    let n = reply.len().min(frame.len());
    reply[..n].copy_from_slice(&frame[..n]);
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;
    use kumo_fatfs::{attr, FatError, SECTOR_SIZE};
    use kumo_vfs::{parse_open_reply, read_payload, VFS_BAD_REQUEST};

    const SEC: u16 = 512;
    const RESERVED: u16 = 32;
    const NUM_FATS: u8 = 2;
    const SPC: u8 = 1;
    const SPF: u32 = 32;
    const TOTAL: u32 = 4096;
    const ROOT_CLUSTER: u32 = 2;

    fn boot_sector() -> [u8; SECTOR_SIZE] {
        let mut s = [0u8; SECTOR_SIZE];
        s[0..3].copy_from_slice(&[0xEB, 0xFE, 0x90]);
        s[3..11].copy_from_slice(b"MSDOS5.0");
        s[0x0B..0x0D].copy_from_slice(&SEC.to_le_bytes());
        s[0x0D] = SPC;
        s[0x0E..0x10].copy_from_slice(&RESERVED.to_le_bytes());
        s[0x10] = NUM_FATS;
        s[0x15] = 0xF8;
        s[0x20..0x24].copy_from_slice(&TOTAL.to_le_bytes());
        s[0x24..0x28].copy_from_slice(&SPF.to_le_bytes());
        s[0x2C..0x30].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
        s[0x52..0x5A].copy_from_slice(b"FAT32   ");
        s[0x1FE..0x200].copy_from_slice(&[0x55, 0xAA]);
        s
    }

    fn dir_entry(name: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name);
        e[11] = attr;
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    struct FixtureDisk;

    impl SectorReader for FixtureDisk {
        fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
            *buf = [0u8; SECTOR_SIZE];
            match lba {
                0 => *buf = boot_sector(),
                32 => {
                    let entries = [
                        0x0FFF_FFF8u32,
                        0x0FFF_FFFF,
                        0x0FFF_FFFF,
                        0x0FFF_FFFF,
                        0x0FFF_FFFF,
                    ];
                    for (i, e) in entries.iter().enumerate() {
                        buf[i * 4..i * 4 + 4].copy_from_slice(&e.to_le_bytes());
                    }
                }
                96 => {
                    buf[0..32].copy_from_slice(&dir_entry(b"HELLO   TXT", attr::ARCHIVE, 3, 6));
                    buf[32..64].copy_from_slice(&dir_entry(b"EFI        ", attr::DIRECTORY, 4, 0));
                }
                97 => buf[..6].copy_from_slice(b"hello!"),
                98 => {
                    buf[0..32].copy_from_slice(&dir_entry(b"BOOT       ", attr::DIRECTORY, 5, 0));
                }
                99 => {}
                _ => {}
            }
            true
        }
    }

    fn mounted() -> FatVolume {
        FatVolume::mount(&mut FixtureDisk).expect("mount fixture")
    }

    fn encode_request(req: Request<'_>) -> Vec<u8> {
        let mut buf = [0u8; REQUEST_BUF_BYTES];
        let n = req.encode_into(&mut buf).expect("encode request");
        buf[..n].to_vec()
    }

    struct MockTransport {
        incoming: VecDeque<Vec<u8>>,
        sent: Vec<Vec<u8>>,
    }

    impl Transport for MockTransport {
        fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
            let frame = self.incoming.pop_front()?;
            buf[..frame.len()].copy_from_slice(&frame);
            Some(frame.len())
        }

        fn send(&mut self, frame: &[u8]) {
            self.sent.push(frame.to_vec());
        }
    }

    #[test]
    fn open_file_returns_handle_and_size() {
        let volume = mounted();
        let mut table = OpenTable::new();
        let mut req = [0u8; 64];
        let req_len = Request::open(b"/HELLO.TXT").encode_into(&mut req).unwrap();
        let mut reply = [0u8; 32];

        let n = dispatch(
            &volume,
            &mut FixtureDisk,
            &mut table,
            &req[..req_len],
            &mut reply,
        );
        assert_eq!(parse_open_reply(&reply[..n]), Ok((1, 6)));
    }

    #[test]
    fn open_missing_and_directory_report_status() {
        let volume = mounted();
        let mut table = OpenTable::new();
        let mut req = [0u8; 64];
        let mut reply = [0u8; 32];

        let req_len = Request::open(b"/NOPE.TXT").encode_into(&mut req).unwrap();
        let n = dispatch(
            &volume,
            &mut FixtureDisk,
            &mut table,
            &req[..req_len],
            &mut reply,
        );
        assert_eq!(reply[..n], [VFS_NOT_FOUND]);

        let req_len = Request::open(b"/EFI").encode_into(&mut req).unwrap();
        let n = dispatch(
            &volume,
            &mut FixtureDisk,
            &mut table,
            &req[..req_len],
            &mut reply,
        );
        assert_eq!(reply[..n], [VFS_IS_DIR]);
    }

    #[test]
    fn read_uses_open_handle_offset_and_length() {
        let volume = mounted();
        let mut table = OpenTable::new();
        let mut req = [0u8; 64];
        let open_len = Request::open(b"/HELLO.TXT").encode_into(&mut req).unwrap();
        let mut reply = [0u8; 32];
        dispatch(
            &volume,
            &mut FixtureDisk,
            &mut table,
            &req[..open_len],
            &mut reply,
        );

        let read_len = Request::read(1, 1, 4).encode_into(&mut req).unwrap();
        let n = dispatch(
            &volume,
            &mut FixtureDisk,
            &mut table,
            &req[..read_len],
            &mut reply,
        );
        assert_eq!(read_payload(&reply[..n]), Ok(&b"ello"[..]));
    }

    #[test]
    fn read_bad_handle_and_bad_request_report_status() {
        let volume = mounted();
        let mut table = OpenTable::new();
        let mut req = [0u8; 64];
        let mut reply = [0u8; 32];

        let read_len = Request::read(9, 0, 4).encode_into(&mut req).unwrap();
        let n = dispatch(
            &volume,
            &mut FixtureDisk,
            &mut table,
            &req[..read_len],
            &mut reply,
        );
        assert_eq!(reply[..n], [VFS_BAD_HANDLE]);

        let n = dispatch(&volume, &mut FixtureDisk, &mut table, &[0xFF], &mut reply);
        assert_eq!(reply[..n], [VFS_BAD_REQUEST]);
    }

    #[test]
    fn read_reply_is_bounded_by_reply_buffer() {
        let volume = mounted();
        let mut table = OpenTable::new();
        let mut req = [0u8; 64];
        let open_len = Request::open(b"/HELLO.TXT").encode_into(&mut req).unwrap();
        let mut open_reply = [0u8; 32];
        dispatch(
            &volume,
            &mut FixtureDisk,
            &mut table,
            &req[..open_len],
            &mut open_reply,
        );

        let read_len = Request::read(1, 0, 6).encode_into(&mut req).unwrap();
        let mut tiny_reply = [0u8; 4];
        let n = dispatch(
            &volume,
            &mut FixtureDisk,
            &mut table,
            &req[..read_len],
            &mut tiny_reply,
        );
        assert_eq!(n, 4);
        assert_eq!(read_payload(&tiny_reply[..n]), Ok(&b"hel"[..]));
    }

    #[test]
    fn serve_once_replies_to_open_over_transport() {
        let volume = mounted();
        let mut server = FatServer::new();
        let mut transport = MockTransport {
            incoming: VecDeque::from([encode_request(Request::open(b"/HELLO.TXT"))]),
            sent: Vec::new(),
        };
        let mut request_buf = [0u8; REQUEST_BUF_BYTES];
        let mut reply_buf = [0u8; REPLY_BUF_BYTES];

        assert!(server.serve_once(
            &volume,
            &mut FixtureDisk,
            &mut transport,
            &mut request_buf,
            &mut reply_buf,
        ));

        assert_eq!(transport.sent.len(), 1);
        assert_eq!(parse_open_reply(&transport.sent[0]), Ok((1, 6)));
    }

    #[test]
    fn serve_preserves_open_table_across_requests() {
        let volume = mounted();
        let mut server = FatServer::new();
        let mut transport = MockTransport {
            incoming: VecDeque::from([
                encode_request(Request::open(b"/HELLO.TXT")),
                encode_request(Request::read(1, 1, 4)),
            ]),
            sent: Vec::new(),
        };

        server.serve(&volume, &mut FixtureDisk, &mut transport);

        assert_eq!(transport.sent.len(), 2);
        assert_eq!(parse_open_reply(&transport.sent[0]), Ok((1, 6)));
        assert_eq!(read_payload(&transport.sent[1]), Ok(&b"ello"[..]));
    }

    #[test]
    fn serve_stops_when_transport_closes() {
        let volume = mounted();
        let mut server = FatServer::new();
        let mut transport = MockTransport {
            incoming: VecDeque::new(),
            sent: Vec::new(),
        };
        let mut request_buf = [0u8; REQUEST_BUF_BYTES];
        let mut reply_buf = [0u8; REPLY_BUF_BYTES];

        assert!(!server.serve_once(
            &volume,
            &mut FixtureDisk,
            &mut transport,
            &mut request_buf,
            &mut reply_buf,
        ));
        assert!(transport.sent.is_empty());
    }

    #[test]
    fn mount_fixture_is_valid_fat32() {
        assert!(matches!(FatVolume::mount(&mut FixtureDisk), Ok(_)));
        struct DeadDisk;
        impl SectorReader for DeadDisk {
            fn read_sector(&mut self, _lba: u32, _buf: &mut [u8; SECTOR_SIZE]) -> bool {
                false
            }
        }
        assert!(matches!(
            FatVolume::mount(&mut DeadDisk),
            Err(FatError::ReadFailed)
        ));
    }
}
