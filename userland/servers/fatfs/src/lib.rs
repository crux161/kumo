#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j548
//j549
//j862
//j863
//j865

//! `fatfs` — the FAT-backed vfs handler core.
//!
//! FAT is not KUMO's root filesystem (`DESIGN/010`); it remains the ESP /
//! removable-media filesystem. This crate is the host-testable server logic that
//! maps `kumo-vfs` open/read/create/resize/write/flush requests onto `kumo-fatfs`.
//! It owns no device and has no runtime dependency: the resident binary wraps
//! this handler with a `Channel` transport and a narrowed backing-store VMO.
//!
//! The mounted FAT volume's bytes are authoritative. The per-server open table is
//! reconnectable soft state, so a supervised restart can rebuild it by reopening
//! paths rather than recovering private RAM.

#[cfg(test)]
extern crate alloc;

pub mod block;

use kumo_fatfs::{DirEntry, DirLocation, FatVolume, FatWriteError, SectorReader, SectorWriter};
use kumo_vfs::{
    encode_open_ok, encode_stat_ok, encode_status, encode_write_ok, DirReplyWriter, Request,
    LOCK_EX, LOCK_SH, LOCK_UN, MAX_WRITE_DATA, OPEN_CREATE, OPEN_READ, OPEN_WRITE,
    STAT_KIND_DIRECTORY, STAT_KIND_FILE, VFS_ACCESS_DENIED, VFS_BAD_HANDLE, VFS_BAD_REQUEST,
    VFS_EXISTS, VFS_IO_ERROR, VFS_IS_DIR, VFS_NOT_DIR, VFS_NOT_EMPTY, VFS_NOT_FOUND,
    VFS_NOT_SUPPORTED, VFS_NO_SPACE, VFS_OK, VFS_WOULD_BLOCK, WRITE_HEADER_LEN,
};

/// Maximum number of open file handles in this first fixed table.
pub const MAX_OPEN_FILES: usize = 8;

/// Largest `Open` request frame accepted by `kumo-vfs`.
pub const REQUEST_BUF_BYTES: usize = WRITE_HEADER_LEN + MAX_WRITE_DATA;

/// Inline read payload cap for this first userspace loop, plus one status byte.
pub const READ_REPLY_BYTES: usize = 1 + 256;

/// The serve loop's reply buffer. A read still answers at most [`READ_REPLY_BYTES`]; the rest
/// is for directory pages, which must always hold at least one record with the longest name
/// a record carries (`2 + 10 + 255` bytes). If one did not fit, a page with no records would
/// read as the end of the directory (J548).
/// — KESTREL
pub const REPLY_BUF_BYTES: usize = 1024;
const _: () = assert!(REPLY_BUF_BYTES >= 2 + 10 + u8::MAX as usize);

/// Per-server open file table. Handles are small positive integers; `0` is
/// deliberately invalid so protocol bugs are easy to spot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenTable {
    slots: [Option<OpenFile>; MAX_OPEN_FILES],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenFile {
    entry: DirEntry,
    readable: bool,
    writable: bool,
    /// Advisory lock held through this open handle (SH3): 0, `LOCK_SH`, or `LOCK_EX`.
    lock: u8,
}

impl OpenTable {
    pub const fn new() -> OpenTable {
        OpenTable {
            slots: [None; MAX_OPEN_FILES],
        }
    }

    pub fn insert(&mut self, entry: DirEntry, options: u8) -> Option<u32> {
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(OpenFile {
                    entry,
                    readable: options & OPEN_READ != 0,
                    writable: options & OPEN_WRITE != 0,
                    lock: 0,
                });
                return Some((index + 1) as u32);
            }
        }
        None
    }

    pub fn get(&self, handle: u32) -> Option<OpenFile> {
        let index = usize::try_from(handle).ok()?.checked_sub(1)?;
        self.slots.get(index).copied().flatten()
    }

    pub fn get_mut(&mut self, handle: u32) -> Option<&mut OpenFile> {
        let index = usize::try_from(handle).ok()?.checked_sub(1)?;
        self.slots.get_mut(index)?.as_mut()
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
    pub fn serve_once<R: SectorReader + SectorWriter, T: Transport>(
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
    pub fn serve<R: SectorReader + SectorWriter, T: Transport>(
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

/// Sessions the mount-scoped server admits (SH3-c). Small on purpose: every session
/// reserves an open table, and the first user is a two-process lock/rename proof.
pub const MAX_SESSIONS: usize = 4;
/// Session access byte: reads only (SH3-c2). The launcher's grant stays authoritative:
/// the shared server holds an rw block channel, so a read-only session is enforced here.
pub const SESSION_READ_ONLY: u8 = 0;
/// Session access byte: reads and mutations.
pub const SESSION_READ_WRITE: u8 = 1;
/// Advisory locks the shared registry tracks at once.
pub const MAX_LOCKS: usize = 8;
/// Recorded synthetic permission modes the shared server tracks (SH3-d). FAT stores
/// no modes; this soft state is what stat reports until a restart drops it.
pub const MAX_MODES: usize = 8;

/// One advisory lock held through an open entry (SH3-c). The key is the entry's
/// on-disk location: FAT has no stable inode, so a rename gives the file a new
/// identity and a later open of the new name re-locks under it. Documented, not
/// silent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LockEntry {
    key: DirLocation,
    session: usize,
    mode: u8,
}

/// One recorded synthetic mode (SH3-d), keyed like the locks by the entry's location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModeEntry {
    key: DirLocation,
    mode: u16,
}

/// The synthetic permission modes a shared server records (SH3-d).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeTable {
    entries: [Option<ModeEntry>; MAX_MODES],
}

impl ModeTable {
    pub const fn new() -> Self {
        ModeTable {
            entries: [None; MAX_MODES],
        }
    }

    fn get(&self, key: DirLocation) -> Option<u16> {
        self.entries
            .iter()
            .flatten()
            .find(|entry| entry.key == key)
            .map(|entry| entry.mode)
    }

    fn set(&mut self, key: DirLocation, mode: u16) -> bool {
        let mut free = None;
        for slot in self.entries.iter_mut() {
            match slot {
                Some(entry) if entry.key == key => {
                    entry.mode = mode;
                    return true;
                }
                Some(_) => {}
                None if free.is_none() => free = Some(()),
                None => {}
            }
        }
        if let Some(()) = free {
            if let Some(slot) = self.entries.iter_mut().find(|slot| slot.is_none()) {
                *slot = Some(ModeEntry { key, mode });
                return true;
            }
        }
        false
    }

    fn clear(&mut self, key: DirLocation) {
        for slot in self.entries.iter_mut() {
            if slot.is_some_and(|entry| entry.key == key) {
                *slot = None;
            }
        }
    }
}

impl Default for ModeTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Cross-session advisory locks for the mount-scoped server (SH3-c1). Unlike the
/// per-launch table's slot-local lock, these conflict between sessions, which is
/// what makes flock meaningful across processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockRegistry {
    entries: [Option<LockEntry>; MAX_LOCKS],
}

impl LockRegistry {
    pub const fn new() -> Self {
        LockRegistry {
            entries: [None; MAX_LOCKS],
        }
    }

    fn conflicts(&self, key: DirLocation, session: usize, exclusive: bool) -> bool {
        self.entries.iter().flatten().any(|entry| {
            entry.key == key && entry.session != session && (exclusive || entry.mode == LOCK_EX)
        })
    }

    fn set(&mut self, key: DirLocation, session: usize, mode: u8) -> bool {
        let mut free = None;
        for (index, slot) in self.entries.iter_mut().enumerate() {
            match slot {
                Some(entry) if entry.key == key && entry.session == session => {
                    entry.mode = mode;
                    return true;
                }
                Some(_) => {}
                None if free.is_none() => free = Some(index),
                None => {}
            }
        }
        if let Some(index) = free {
            self.entries[index] = Some(LockEntry { key, session, mode });
            return true;
        }
        false
    }

    fn clear_session(&mut self, session: usize) {
        for slot in self.entries.iter_mut() {
            if slot.is_some_and(|entry| entry.session == session) {
                *slot = None;
            }
        }
    }

    fn clear(&mut self, key: DirLocation, session: usize) {
        for slot in self.entries.iter_mut() {
            if slot.is_some_and(|entry| entry.key == key && entry.session == session) {
                *slot = None;
            }
        }
    }
}

impl Default for LockRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The mount-scoped server state (SH3-c1): one open table per client session and a
/// registry of advisory locks shared across them. The volume bytes are shared by
/// construction, so one session's rename is immediately visible to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SharedServer {
    pub sessions: [OpenTable; MAX_SESSIONS],
    pub locks: LockRegistry,
    /// Per-session access (SH3-c2): the launcher's grant, enforced at the server.
    pub access: [u8; MAX_SESSIONS],
    /// Recorded synthetic modes (SH3-d).
    pub modes: ModeTable,
}

impl SharedServer {
    pub const fn new() -> Self {
        SharedServer {
            sessions: [OpenTable::new(); MAX_SESSIONS],
            locks: LockRegistry::new(),
            access: [SESSION_READ_WRITE; MAX_SESSIONS],
            modes: ModeTable::new(),
        }
    }

    /// Admit `session` with the launcher-granted access byte (SH3-c2).
    pub fn set_access(&mut self, session: usize, access: u8) {
        if let Some(slot) = self.access.get_mut(session) {
            *slot = access;
        }
    }

    /// Drop a closed session's locks and open table (SH3-c2): a dead client must not
    /// hold advisory locks against the survivors.
    pub fn close_session(&mut self, session: usize) {
        if session >= MAX_SESSIONS {
            return;
        }
        self.locks.clear_session(session);
        self.sessions[session] = OpenTable::new();
        self.access[session] = SESSION_READ_WRITE;
    }
}

impl Default for SharedServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Does this request mutate the volume? Read-only sessions are refused exactly these
/// (SH3-c2); locks and reads are not mutations.
fn request_writes(request: &Request<'_>) -> bool {
    matches!(
        request,
        Request::OpenOptions { options, .. }
            if options & (OPEN_WRITE | OPEN_CREATE) != 0
    ) || matches!(
        request,
        Request::Write { .. }
            | Request::SetLen { .. }
            | Request::Mkdir { .. }
            | Request::Unlink { .. }
            | Request::Rmdir { .. }
            | Request::Rename { .. }
            | Request::Chmod { .. }
    )
}

/// Handle one request for `session` of a mount-scoped server (SH3-c1). Open-handle
/// requests index that session's table; metadata requests reuse the same handlers;
/// locks flow through the cross-session registry.
pub fn dispatch_shared<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    server: &mut SharedServer,
    session: usize,
    request: &[u8],
    reply: &mut [u8],
) -> usize {
    if session >= MAX_SESSIONS {
        return write_status(reply, VFS_BAD_REQUEST);
    }
    let decoded = Request::decode(request);
    if server.access[session] == SESSION_READ_ONLY && decoded.as_ref().is_some_and(request_writes) {
        return write_status(reply, VFS_ACCESS_DENIED);
    }
    match decoded {
        Some(Request::Open { path }) => open(
            volume,
            io,
            &mut server.sessions[session],
            path,
            OPEN_READ,
            reply,
        ),
        Some(Request::OpenOptions { path, options }) => open(
            volume,
            io,
            &mut server.sessions[session],
            path,
            options,
            reply,
        ),
        Some(Request::Read {
            handle,
            offset,
            len,
        }) => read(
            volume,
            io,
            &server.sessions[session],
            handle,
            offset,
            len,
            reply,
        ),
        Some(Request::Write {
            handle,
            offset,
            data,
        }) => write(
            volume,
            io,
            &mut server.sessions[session],
            handle,
            offset,
            data,
            reply,
        ),
        Some(Request::SetLen { handle, len }) => set_len(
            volume,
            io,
            &mut server.sessions[session],
            handle,
            len,
            reply,
        ),
        Some(Request::Flush { handle }) => {
            flush(volume, io, &server.sessions[session], handle, reply)
        }
        Some(Request::ReadDir { path, start }) => read_dir(volume, io, path, start, reply),
        Some(Request::Mkdir { path }) => mkdir(volume, io, path, reply),
        Some(Request::Unlink { path }) => unlink(volume, io, path, reply),
        Some(Request::Rmdir { path }) => rmdir(volume, io, path, reply),
        Some(Request::Rename { from, to }) => rename(volume, io, from, to, reply),
        Some(Request::Stat { path }) => stat_with_modes(volume, io, &server.modes, path, reply),
        Some(Request::Lock { handle, operation }) => {
            lock_session(server, session, handle, operation, reply)
        }
        Some(Request::Chmod { path, mode }) => {
            chmod_shared(volume, io, &mut server.modes, path, mode, reply)
        }
        Some(Request::Link { .. }) => write_status(reply, VFS_NOT_SUPPORTED),
        None => write_status(reply, VFS_BAD_REQUEST),
    }
}

/// Advisory lock over the cross-session registry (SH3-c1).
fn lock_session(
    server: &mut SharedServer,
    session: usize,
    handle: u32,
    operation: u8,
    reply: &mut [u8],
) -> usize {
    let Some(file) = server.sessions[session].get(handle) else {
        return write_status(reply, VFS_BAD_HANDLE);
    };
    let Some(location) = file.entry.location else {
        return write_status(reply, VFS_BAD_HANDLE);
    };
    match operation {
        LOCK_UN => {
            server.locks.clear(location, session);
            write_status(reply, VFS_OK)
        }
        LOCK_SH | LOCK_EX => {
            let exclusive = operation == LOCK_EX;
            if server.locks.conflicts(location, session, exclusive) {
                return write_status(reply, VFS_WOULD_BLOCK);
            }
            if !server.locks.set(location, session, operation) {
                return write_status(reply, VFS_NO_SPACE);
            }
            write_status(reply, VFS_OK)
        }
        _ => write_status(reply, VFS_BAD_REQUEST),
    }
}

/// Handle one vfs request frame, writing the reply into `reply` and returning
/// the number of reply bytes. Malformed frames and too-small reply buffers return
/// a one-byte [`VFS_BAD_REQUEST`] status when possible.
pub fn dispatch<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    reader: &mut R,
    open_files: &mut OpenTable,
    request: &[u8],
    reply: &mut [u8],
) -> usize {
    match Request::decode(request) {
        Some(Request::Open { path }) => open(volume, reader, open_files, path, OPEN_READ, reply),
        Some(Request::OpenOptions { path, options }) => {
            open(volume, reader, open_files, path, options, reply)
        }
        Some(Request::Read {
            handle,
            offset,
            len,
        }) => read(volume, reader, open_files, handle, offset, len, reply),
        Some(Request::Write {
            handle,
            offset,
            data,
        }) => write(volume, reader, open_files, handle, offset, data, reply),
        Some(Request::SetLen { handle, len }) => {
            set_len(volume, reader, open_files, handle, len, reply)
        }
        Some(Request::Flush { handle }) => flush(volume, reader, open_files, handle, reply),
        Some(Request::ReadDir { path, start }) => read_dir(volume, reader, path, start, reply),
        Some(Request::Mkdir { path }) => mkdir(volume, reader, path, reply),
        Some(Request::Unlink { path }) => unlink(volume, reader, path, reply),
        Some(Request::Rmdir { path }) => rmdir(volume, reader, path, reply),
        Some(Request::Rename { from, to }) => rename(volume, reader, from, to, reply),
        Some(Request::Stat { path }) => stat(volume, reader, path, reply),
        Some(Request::Lock { handle, operation }) => lock(open_files, handle, operation, reply),
        // The per-launch image server records no modes (SH3-d scope): the mount-scoped
        // server does. Documented, not silent.
        Some(Request::Chmod { .. }) => write_status(reply, VFS_NOT_SUPPORTED),
        // FAT has no hard links; the documented fallback lives in the client.
        Some(Request::Link { .. }) => write_status(reply, VFS_NOT_SUPPORTED),
        None => write_status(reply, VFS_BAD_REQUEST),
    }
}

fn open<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    reader: &mut R,
    open_files: &mut OpenTable,
    path: &[u8],
    options: u8,
    reply: &mut [u8],
) -> usize {
    let entry = match volume.resolve_path(reader, path) {
        Some(entry) => entry,
        None if options & OPEN_CREATE != 0 => match volume.create_file(reader, path) {
            Ok(entry) => {
                if let Err(error) = volume.flush(reader) {
                    return write_status(reply, mutation_status(error));
                }
                entry
            }
            Err(error) => return write_status(reply, mutation_status(error)),
        },
        None => return write_status(reply, VFS_NOT_FOUND),
    };
    if entry.is_dir() {
        return write_status(reply, VFS_IS_DIR);
    }
    let Some(handle) = open_files.insert(entry, options) else {
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
    let Some(file) = open_files.get(handle) else {
        return write_status(reply, VFS_BAD_HANDLE);
    };
    if !file.readable {
        return write_status(reply, VFS_ACCESS_DENIED);
    }

    reply[0] = VFS_OK;
    let cap = (len as usize).min(reply.len().min(READ_REPLY_BYTES) - 1);
    let n = volume.read_file_at(reader, &file.entry, offset, &mut reply[1..1 + cap]);
    1 + n
}

fn write<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    open_files: &mut OpenTable,
    handle: u32,
    offset: u64,
    data: &[u8],
    reply: &mut [u8],
) -> usize {
    let Some(file) = open_files.get_mut(handle) else {
        return write_status(reply, VFS_BAD_HANDLE);
    };
    if !file.writable {
        return write_status(reply, VFS_ACCESS_DENIED);
    }
    match volume.write_file_at(io, &mut file.entry, offset, data) {
        // Upstream Kilo saves with write+close, without fsync. A successful mutation is
        // synchronous here, so "written on disk" cannot precede a failed device flush.
        // — KESTREL
        Ok(written) => match volume.flush(io) {
            Ok(()) => write_frame(reply, &encode_write_ok(written as u32)),
            Err(error) => write_status(reply, mutation_status(error)),
        },
        Err(error) => write_status(reply, mutation_status(error)),
    }
}

fn set_len<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    open_files: &mut OpenTable,
    handle: u32,
    len: u64,
    reply: &mut [u8],
) -> usize {
    let Some(file) = open_files.get_mut(handle) else {
        return write_status(reply, VFS_BAD_HANDLE);
    };
    if !file.writable {
        return write_status(reply, VFS_ACCESS_DENIED);
    }
    match volume
        .set_file_len(io, &mut file.entry, len)
        .and_then(|()| volume.flush(io))
    {
        Ok(()) => write_status(reply, VFS_OK),
        Err(error) => write_status(reply, mutation_status(error)),
    }
}

fn flush<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    open_files: &OpenTable,
    handle: u32,
    reply: &mut [u8],
) -> usize {
    if open_files.get(handle).is_none() {
        return write_status(reply, VFS_BAD_HANDLE);
    }
    match volume.flush(io) {
        Ok(()) => write_status(reply, VFS_OK),
        Err(error) => write_status(reply, mutation_status(error)),
    }
}

/// List the directory at `path` from its `start`-th entry, as many entries as the reply holds
/// (J547). `.` and `..` are left out, as `ls` leaves them out, and do not count toward
/// `start`. Each entry carries its long name when it has one (J548).
fn read_dir<R: SectorReader>(
    volume: &FatVolume,
    reader: &mut R,
    path: &[u8],
    start: u32,
    reply: &mut [u8],
) -> usize {
    let cluster = match directory_cluster(volume, reader, path) {
        Ok(cluster) => cluster,
        Err(status) => return write_status(reply, status),
    };
    let Some(mut listing) = DirReplyWriter::new(reply) else {
        return 0;
    };
    let mut index = 0u32;
    let walked = volume.for_each_named_in_dir(reader, cluster, |entry, name| {
        if entry.name[0] == b'.' {
            return true;
        }
        if index < start {
            index += 1;
            return true;
        }
        // Preserve a usable name when UTF-8 exceeds the wire/path limit. A truncated long
        // name would display successfully but name no file on the volume. — KESTREL
        let short = entry.short_name();
        let prefix_len = path.len() + usize::from(!path.ends_with(b"/"));
        let listed_name = if name.as_bytes().len() <= u8::MAX as usize
            && prefix_len + name.as_bytes().len() <= kumo_vfs::MAX_PATH
        {
            name.as_bytes()
        } else {
            short.as_bytes()
        };
        if !listing.push(entry.is_dir(), entry.size as u64, listed_name) {
            return false;
        }
        index += 1;
        true
    });
    let len = listing.finish();
    if walked {
        len
    } else {
        write_status(reply, VFS_IO_ERROR)
    }
}

/// The first cluster of the directory `path` names. The root is `/` (or an empty path); a
/// `..` entry that points at the root records cluster 0.
fn directory_cluster<R: SectorReader>(
    volume: &FatVolume,
    reader: &mut R,
    path: &[u8],
) -> Result<u32, u8> {
    let root = volume.bpb().root_cluster;
    if path.split(|byte| *byte == b'/').all(<[u8]>::is_empty) {
        return Ok(root);
    }
    match volume.resolve_path(reader, path) {
        Some(entry) if entry.is_dir() => Ok(if entry.first_cluster == 0 {
            root
        } else {
            entry.first_cluster
        }),
        Some(_) => Err(VFS_NOT_DIR),
        None => Err(VFS_NOT_FOUND),
    }
}

/// Create the directory at `path` (SH3). Existence is checked first so the answer is
/// `VFS_EXISTS`; a missing parent or an unrepresentable 8.3 name answers `VFS_BAD_REQUEST`
/// (the protocol has no separate ENOENT-for-parent status).
fn mkdir<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    path: &[u8],
    reply: &mut [u8],
) -> usize {
    if volume.resolve_path(io, path).is_some() {
        return write_status(reply, VFS_EXISTS);
    }
    match volume.create_dir(io, path).and_then(|_| volume.flush(io)) {
        Ok(()) => write_status(reply, VFS_OK),
        Err(error) => write_status(reply, mutation_status(error)),
    }
}

/// Remove the file at `path` (SH3).
fn unlink<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    path: &[u8],
    reply: &mut [u8],
) -> usize {
    match volume.resolve_path(io, path) {
        None => write_status(reply, VFS_NOT_FOUND),
        Some(entry) if entry.is_dir() => write_status(reply, VFS_IS_DIR),
        Some(_) => match volume.remove_file(io, path).and_then(|_| volume.flush(io)) {
            Ok(()) => write_status(reply, VFS_OK),
            Err(error) => write_status(reply, mutation_status(error)),
        },
    }
}

/// Remove the empty directory at `path` (SH3). A populated directory answers
/// `VFS_NOT_EMPTY`, mirroring rmdir's `ENOTEMPTY`.
fn rmdir<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    path: &[u8],
    reply: &mut [u8],
) -> usize {
    match volume.resolve_path(io, path) {
        None => write_status(reply, VFS_NOT_FOUND),
        Some(entry) if !entry.is_dir() => write_status(reply, VFS_NOT_DIR),
        Some(_) => match volume.remove_dir(io, path).and_then(|_| volume.flush(io)) {
            Ok(()) => write_status(reply, VFS_OK),
            Err(FatWriteError::Invalid) => write_status(reply, VFS_NOT_EMPTY),
            Err(error) => write_status(reply, mutation_status(error)),
        },
    }
}

/// Rename the file `from` to `to`, replacing an existing file (SH3). Directory
/// renames answer `VFS_NOT_SUPPORTED`: FAT would also have to rewrite `..`.
fn rename<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    from: &[u8],
    to: &[u8],
    reply: &mut [u8],
) -> usize {
    match volume.resolve_path(io, from) {
        None => write_status(reply, VFS_NOT_FOUND),
        Some(entry) if entry.is_dir() => write_status(reply, VFS_NOT_SUPPORTED),
        Some(_) => match volume.rename(io, from, to).and_then(|_| volume.flush(io)) {
            Ok(()) => write_status(reply, VFS_OK),
            Err(error) => write_status(reply, mutation_status(error)),
        },
    }
}

/// Stat `path` (SH3): the kind and byte size, with the synthetic POSIX mode FAT
/// cannot store (0755 for directories, 0644 for files).
fn stat<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    path: &[u8],
    reply: &mut [u8],
) -> usize {
    match volume.resolve_path(io, path) {
        None => write_status(reply, VFS_NOT_FOUND),
        Some(entry) => {
            let (kind, mode) = if entry.is_dir() {
                (STAT_KIND_DIRECTORY, 0o755)
            } else {
                (STAT_KIND_FILE, 0o644)
            };
            write_frame(reply, &encode_stat_ok(kind, mode, entry.size as u64))
        }
    }
}

/// Stat with the recorded synthetic mode (SH3-d): a chmod override wins over the
/// kind's default.
fn stat_with_modes<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    modes: &ModeTable,
    path: &[u8],
    reply: &mut [u8],
) -> usize {
    match volume.resolve_path(io, path) {
        None => write_status(reply, VFS_NOT_FOUND),
        Some(entry) => {
            let (kind, default_mode) = if entry.is_dir() {
                (STAT_KIND_DIRECTORY, 0o755)
            } else {
                (STAT_KIND_FILE, 0o644)
            };
            let mode = entry
                .location
                .and_then(|location| modes.get(location))
                .unwrap_or(default_mode);
            write_frame(reply, &encode_stat_ok(kind, mode, entry.size as u64))
        }
    }
}

/// Record a synthetic mode for `path` (SH3-d). The permissions bits are kept; the
/// file-type bits belong to stat's kind and are ignored here.
fn chmod_shared<R: SectorReader + SectorWriter>(
    volume: &FatVolume,
    io: &mut R,
    modes: &mut ModeTable,
    path: &[u8],
    mode: u16,
    reply: &mut [u8],
) -> usize {
    match volume.resolve_path(io, path) {
        None => write_status(reply, VFS_NOT_FOUND),
        Some(entry) => {
            let Some(location) = entry.location else {
                return write_status(reply, VFS_NOT_FOUND);
            };
            if modes.set(location, mode & 0o777) {
                write_status(reply, VFS_OK)
            } else {
                write_status(reply, VFS_NO_SPACE)
            }
        }
    }
}

/// Advisory locks over the open table (SH3). A shared lock conflicts with another
/// handle's exclusive lock; an exclusive lock conflicts with any other holder. The
/// holder may upgrade or downgrade its own lock while it is the only holder. Locks
/// are per open handle and, until SH3-c's shared mount server, per process instance.
fn lock(open_files: &mut OpenTable, handle: u32, operation: u8, reply: &mut [u8]) -> usize {
    let Some(index) = usize::try_from(handle)
        .ok()
        .and_then(|raw| raw.checked_sub(1))
        .filter(|index| *index < MAX_OPEN_FILES)
    else {
        return write_status(reply, VFS_BAD_HANDLE);
    };
    if open_files.slots[index].is_none() {
        return write_status(reply, VFS_BAD_HANDLE);
    }
    match operation {
        LOCK_UN => {
            if let Some(file) = open_files.slots[index].as_mut() {
                file.lock = 0;
            }
            write_status(reply, VFS_OK)
        }
        LOCK_SH | LOCK_EX => {
            let exclusive = operation == LOCK_EX;
            let conflict = open_files.slots.iter().enumerate().any(|(other, slot)| {
                other != index
                    && (*slot)
                        .is_some_and(|file| file.lock != 0 && (exclusive || file.lock == LOCK_EX))
            });
            if conflict {
                return write_status(reply, VFS_WOULD_BLOCK);
            }
            if let Some(file) = open_files.slots[index].as_mut() {
                file.lock = operation;
            }
            write_status(reply, VFS_OK)
        }
        _ => write_status(reply, VFS_BAD_REQUEST),
    }
}

const fn mutation_status(error: FatWriteError) -> u8 {
    match error {
        FatWriteError::NoSpace => VFS_NO_SPACE,
        FatWriteError::Invalid => VFS_BAD_REQUEST,
        FatWriteError::Io => VFS_IO_ERROR,
    }
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
    use kumo_vfs::{
        parse_dir_reply, parse_open_reply, parse_write_reply, read_payload, DirRecord, OPEN_CREATE,
        OPEN_READ, OPEN_WRITE, VFS_BAD_REQUEST,
    };

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

    impl SectorWriter for FixtureDisk {
        fn write_sector(&mut self, _lba: u32, _buf: &[u8; SECTOR_SIZE]) -> bool {
            false
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

    struct WritableDisk {
        sectors: [[u8; SECTOR_SIZE]; 128],
    }

    impl WritableDisk {
        fn fixture() -> WritableDisk {
            let mut disk = WritableDisk {
                sectors: [[0u8; SECTOR_SIZE]; 128],
            };
            let mut source = FixtureDisk;
            for lba in 0..disk.sectors.len() {
                source.read_sector(lba as u32, &mut disk.sectors[lba]);
            }
            disk
        }
    }

    impl SectorReader for WritableDisk {
        fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
            let Some(sector) = self.sectors.get(lba as usize) else {
                return false;
            };
            *buf = *sector;
            true
        }
    }

    impl SectorWriter for WritableDisk {
        fn write_sector(&mut self, lba: u32, buf: &[u8; SECTOR_SIZE]) -> bool {
            let Some(sector) = self.sectors.get_mut(lba as usize) else {
                return false;
            };
            *sector = *buf;
            true
        }
    }

    #[test]
    fn mutations_wait_for_flush_and_surface_its_failure() {
        struct FlushDisk(WritableDisk, usize, bool);
        impl SectorReader for FlushDisk {
            fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
                self.0.read_sector(lba, buf)
            }
        }
        impl SectorWriter for FlushDisk {
            fn write_sector(&mut self, lba: u32, buf: &[u8; SECTOR_SIZE]) -> bool {
                self.0.write_sector(lba, buf)
            }
            fn flush(&mut self) -> bool {
                self.1 += 1;
                !self.2
            }
        }
        let mut disk = FlushDisk(WritableDisk::fixture(), 0, false);
        let volume = FatVolume::mount(&mut disk).unwrap();
        let mut table = OpenTable::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let request = encode_request(Request::open_options(
            b"/SAVE.TXT",
            OPEN_READ | OPEN_WRITE | OPEN_CREATE,
        ));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        let (handle, _) = parse_open_reply(&reply[..n]).unwrap();
        assert_eq!(disk.1, 1);
        let request = encode_request(Request::set_len(handle, 6));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        assert_eq!(&reply[..n], &[VFS_OK]);
        assert_eq!(disk.1, 2);
        let request = encode_request(Request::write(handle, 0, b"saved!"));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        assert_eq!(parse_write_reply(&reply[..n]), Ok(6));
        assert_eq!(disk.1, 3);
        disk.2 = true;
        for request in [
            Request::write(handle, 0, b"unsure"),
            Request::set_len(handle, 4),
            Request::flush(handle),
            Request::open_options(b"/FAIL.TXT", OPEN_WRITE | OPEN_CREATE),
        ] {
            let request = encode_request(request);
            let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
            assert_eq!(&reply[..n], &[VFS_IO_ERROR]);
        }
        assert_eq!(disk.1, 7);
    }

    #[test]
    fn create_resize_write_flush_and_reopen_cross_the_vfs_handler() {
        let mut disk = WritableDisk::fixture();
        let volume = FatVolume::mount(&mut disk).unwrap();
        let mut table = OpenTable::new();
        let mut request = [0u8; REQUEST_BUF_BYTES];
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let options = OPEN_READ | OPEN_WRITE | OPEN_CREATE;

        let n = Request::open_options(b"/KILO.TXT", options)
            .encode_into(&mut request)
            .unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut table, &request[..n], &mut reply);
        let (handle, size) = parse_open_reply(&reply[..reply_len]).unwrap();
        assert_eq!(size, 0);

        let n = Request::set_len(handle, 6)
            .encode_into(&mut request)
            .unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut table, &request[..n], &mut reply);
        assert_eq!(&reply[..reply_len], &[VFS_OK]);

        let n = Request::write(handle, 0, b"cloud!")
            .encode_into(&mut request)
            .unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut table, &request[..n], &mut reply);
        assert_eq!(parse_write_reply(&reply[..reply_len]), Ok(6));

        let n = Request::flush(handle).encode_into(&mut request).unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut table, &request[..n], &mut reply);
        assert_eq!(&reply[..reply_len], &[VFS_OK]);

        let mut reopened = OpenTable::new();
        let n = Request::open(b"/KILO.TXT")
            .encode_into(&mut request)
            .unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut reopened, &request[..n], &mut reply);
        let (reopened_handle, reopened_size) = parse_open_reply(&reply[..reply_len]).unwrap();
        assert_eq!(reopened_size, 6);
        let n = Request::read(reopened_handle, 0, 6)
            .encode_into(&mut request)
            .unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut reopened, &request[..n], &mut reply);
        assert_eq!(read_payload(&reply[..reply_len]), Ok(&b"cloud!"[..]));
    }

    #[test]
    fn open_access_options_gate_reads_writes_and_resize() {
        let mut disk = WritableDisk::fixture();
        let volume = FatVolume::mount(&mut disk).unwrap();
        let mut table = OpenTable::new();
        let mut request = [0u8; REQUEST_BUF_BYTES];
        let mut reply = [0u8; REPLY_BUF_BYTES];

        let n = Request::open_options(b"/HELLO.TXT", OPEN_READ)
            .encode_into(&mut request)
            .unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut table, &request[..n], &mut reply);
        let (read_handle, _) = parse_open_reply(&reply[..reply_len]).unwrap();

        let n = Request::write(read_handle, 0, b"no")
            .encode_into(&mut request)
            .unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut table, &request[..n], &mut reply);
        assert_eq!(&reply[..reply_len], &[VFS_ACCESS_DENIED]);

        let n = Request::set_len(read_handle, 1)
            .encode_into(&mut request)
            .unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut table, &request[..n], &mut reply);
        assert_eq!(&reply[..reply_len], &[VFS_ACCESS_DENIED]);

        let n = Request::open_options(b"/HELLO.TXT", OPEN_WRITE)
            .encode_into(&mut request)
            .unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut table, &request[..n], &mut reply);
        let (write_handle, _) = parse_open_reply(&reply[..reply_len]).unwrap();

        let n = Request::read(write_handle, 0, 1)
            .encode_into(&mut request)
            .unwrap();
        let reply_len = dispatch(&volume, &mut disk, &mut table, &request[..n], &mut reply);
        assert_eq!(&reply[..reply_len], &[VFS_ACCESS_DENIED]);
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

    /// List `path` from `start` through `dispatch`, with a reply of `reply_len` bytes.
    fn list(path: &[u8], start: u32, reply_len: usize) -> Result<Vec<(bool, u64, Vec<u8>)>, u8> {
        let volume = mounted();
        let mut table = OpenTable::new();
        let request = encode_request(Request::ReadDir { path, start });
        let mut reply = alloc::vec![0u8; reply_len];
        let n = dispatch(&volume, &mut FixtureDisk, &mut table, &request, &mut reply);
        match parse_dir_reply(&reply[..n]) {
            Ok((count, records)) => {
                let records: Vec<_> = records
                    .map(|DirRecord { is_dir, size, name }| (is_dir, size, name.to_vec()))
                    .collect();
                assert_eq!(records.len(), count as usize);
                Ok(records)
            }
            Err(kumo_vfs::VfsError::Status(status)) => Err(status),
            Err(other) => panic!("malformed reply: {other:?}"),
        }
    }

    #[test]
    fn the_root_lists_its_files_and_directories() {
        assert_eq!(
            list(b"/", 0, REPLY_BUF_BYTES),
            Ok(alloc::vec![
                (false, 6, b"HELLO.TXT".to_vec()),
                (true, 0, b"EFI".to_vec())
            ])
        );
        assert_eq!(
            list(b"", 0, REPLY_BUF_BYTES),
            list(b"/", 0, REPLY_BUF_BYTES)
        );
        assert_eq!(
            list(b"/EFI", 0, REPLY_BUF_BYTES),
            Ok(alloc::vec![(true, 0, b"BOOT".to_vec())])
        );
    }

    #[test]
    fn a_listing_pages_from_its_start_until_empty() {
        // Room for one record: the client asks again from the next entry.
        let one = 2 + 10 + 9;
        assert_eq!(
            list(b"/", 0, one),
            Ok(alloc::vec![(false, 6, b"HELLO.TXT".to_vec())])
        );
        assert_eq!(
            list(b"/", 1, one),
            Ok(alloc::vec![(true, 0, b"EFI".to_vec())])
        );
        assert_eq!(list(b"/", 2, one), Ok(alloc::vec![]));
    }

    #[test]
    fn listing_a_file_or_a_missing_path_reports_why() {
        assert_eq!(list(b"/HELLO.TXT", 0, REPLY_BUF_BYTES), Err(VFS_NOT_DIR));
        assert_eq!(list(b"/NOPE", 0, REPLY_BUF_BYTES), Err(VFS_NOT_FOUND));
    }

    /// The fixture with its root directory holding long-named entries (J548): `Hello there.txt`
    /// on `HELLO.TXT`'s data, the `EFI` directory, and a `._Hello there.txt` AppleDouble twin.
    struct LongNameDisk<'a>(&'a str);

    fn long_name_slots(long: &str, short: &[u8; 11], out: &mut [[u8; 32]]) -> usize {
        let mut units = [0xFFFFu16; 260];
        let mut n = 0;
        for unit in long.encode_utf16() {
            units[n] = unit;
            n += 1;
        }
        if n % 13 != 0 {
            units[n] = 0;
        }
        let slots = n.div_ceil(13);
        for (index, order) in (1..=slots).rev().enumerate() {
            let slot = &mut out[index];
            slot[0] = order as u8 | if order == slots { 0x40 } else { 0 };
            slot[11] = attr::LONG_NAME;
            slot[13] = kumo_fatfs::short_name_checksum(short);
            let offsets = (1..11)
                .step_by(2)
                .chain((14..26).step_by(2))
                .chain((28..32).step_by(2));
            for (unit, at) in units[(order - 1) * 13..order * 13].iter().zip(offsets) {
                slot[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            }
        }
        slots
    }

    impl SectorReader for LongNameDisk<'_> {
        fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
            if lba != 96 {
                return FixtureDisk.read_sector(lba, buf);
            }
            *buf = [0u8; SECTOR_SIZE];
            let mut slots = [[0u8; 32]; 16];
            let mut at = long_name_slots(self.0, b"HELLOT~1TXT", &mut slots);
            slots[at] = dir_entry(b"HELLOT~1TXT", attr::ARCHIVE, 3, 6);
            at += 1;
            slots[at] = dir_entry(b"EFI        ", attr::DIRECTORY, 4, 0);
            at += 1;
            at += long_name_slots("._Hello there.txt", b"_HELLO~1TXT", &mut slots[at..]);
            slots[at] = dir_entry(b"_HELLO~1TXT", attr::ARCHIVE, 3, 6);
            for (raw, slot) in buf.chunks_exact_mut(32).zip(&slots) {
                raw.copy_from_slice(slot);
            }
            true
        }
    }

    impl SectorWriter for LongNameDisk<'_> {
        fn write_sector(&mut self, _lba: u32, _buf: &[u8; SECTOR_SIZE]) -> bool {
            false
        }
    }

    #[test]
    fn listings_carry_long_names_and_opens_take_them() {
        let mut disk = LongNameDisk("Hello there.txt");
        let volume = FatVolume::mount(&mut disk).unwrap();
        let mut table = OpenTable::new();
        let request = encode_request(Request::ReadDir {
            path: b"/",
            start: 0,
        });
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        let (count, records) = parse_dir_reply(&reply[..n]).unwrap();
        assert_eq!(count, 3);
        let names: Vec<Vec<u8>> = records.map(|record| record.name.to_vec()).collect();
        assert_eq!(
            names,
            [
                b"Hello there.txt".to_vec(),
                b"EFI".to_vec(),
                b"._Hello there.txt".to_vec()
            ]
        );

        // The listed name opens the file, without regard to ASCII case.
        let request = encode_request(Request::open(b"/hello THERE.txt"));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        assert_eq!(parse_open_reply(&reply[..n]), Ok((1, 6)));
        let request = encode_request(Request::read(1, 0, 256));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        assert_eq!(read_payload(&reply[..n]), Ok(b"hello!".as_slice()));
    }

    #[test]
    fn an_oversized_name_lists_an_alias_that_opens_the_file() {
        for len in [84, 85, 100] {
            let long = "語".repeat(len);
            let mut disk = LongNameDisk(&long);
            let volume = FatVolume::mount(&mut disk).unwrap();
            let mut table = OpenTable::new();
            let request = encode_request(Request::ReadDir {
                path: b"/",
                start: 0,
            });
            let mut reply = [0u8; REPLY_BUF_BYTES];
            let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
            let (_, mut records) = parse_dir_reply(&reply[..n]).unwrap();
            let name = records.next().unwrap().name;
            assert_eq!(
                name,
                if len < 85 {
                    long.as_bytes()
                } else {
                    b"HELLOT~1.TXT"
                }
            );
            let mut path = alloc::vec![b'/'];
            path.extend_from_slice(name);
            let request = encode_request(Request::open(&path));
            let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
            assert_eq!(parse_open_reply(&reply[..n]), Ok((1, 6)));
        }
    }

    #[test]
    fn long_name_pages_advance_past_hidden_entries_until_empty() {
        let mut disk = LongNameDisk("Hello there.txt");
        let volume = FatVolume::mount(&mut disk).unwrap();
        let mut table = OpenTable::new();
        let mut names = Vec::new();
        for start in 0..=3 {
            let request = encode_request(Request::ReadDir { path: b"/", start });
            let mut reply = [0u8; 30];
            let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
            let (count, records) = parse_dir_reply(&reply[..n]).unwrap();
            assert_eq!(count, u8::from(start < 3));
            names.extend(records.map(|record| record.name.to_vec()));
        }
        assert_eq!(
            names,
            [
                b"Hello there.txt".to_vec(),
                b"EFI".to_vec(),
                b"._Hello there.txt".to_vec()
            ]
        );
    }

    #[test]
    fn reads_stay_capped_at_the_inline_payload() {
        let volume = mounted();
        let mut table = OpenTable::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let mut entry = volume
            .resolve_path(&mut FixtureDisk, b"/HELLO.TXT")
            .unwrap();
        entry.size = SECTOR_SIZE as u32;
        let handle = table.insert(entry, OPEN_READ).unwrap();
        let request = encode_request(Request::read(handle, 0, 4096));
        let n = dispatch(&volume, &mut FixtureDisk, &mut table, &request, &mut reply);
        assert_eq!(n, READ_REPLY_BYTES);
        let payload = read_payload(&reply[..n]).unwrap();
        assert_eq!(&payload[..6], b"hello!");
        assert!(payload[6..].iter().all(|byte| *byte == 0));
    }

    fn writable() -> (FatVolume, WritableDisk) {
        let mut disk = WritableDisk::fixture();
        let volume = FatVolume::mount(&mut disk).expect("mount");
        (volume, disk)
    }

    /// One request/reply exchange through the same dispatcher the server runs.
    fn call(
        volume: &FatVolume,
        disk: &mut WritableDisk,
        table: &mut OpenTable,
        reply: &mut [u8],
        request: Request<'_>,
    ) -> alloc::vec::Vec<u8> {
        let frame = encode_request(request);
        let n = dispatch(volume, disk, table, &frame, reply);
        reply[..n].to_vec()
    }

    #[test]
    fn mkdir_stat_and_exists_through_dispatch() {
        let (volume, mut disk) = writable();
        let mut table = OpenTable::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let request = encode_request(Request::mkdir(b"/WORK"));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        assert_eq!(&reply[..n], &[VFS_OK]);
        let request = encode_request(Request::mkdir(b"/WORK"));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        assert_eq!(&reply[..n], &[VFS_EXISTS]);

        let request = encode_request(Request::stat(b"/WORK"));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        let dir = kumo_vfs::parse_stat_reply(&reply[..n]).unwrap();
        assert_eq!(dir.kind, STAT_KIND_DIRECTORY);
        assert_eq!(dir.mode, 0o755);

        let request = encode_request(Request::stat(b"/HELLO.TXT"));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        let file = kumo_vfs::parse_stat_reply(&reply[..n]).unwrap();
        assert_eq!(file.kind, STAT_KIND_FILE);
        assert_eq!(file.size, 6);
        assert_eq!(file.mode, 0o644);

        let request = encode_request(Request::stat(b"/NOPE.TXT"));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        assert_eq!(&reply[..n], &[VFS_NOT_FOUND]);
    }

    #[test]
    fn unlink_and_rmdir_statuses_through_dispatch() {
        let (volume, mut disk) = writable();
        let mut table = OpenTable::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];

        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::unlink(b"/HELLO.TXT")
            ),
            alloc::vec![VFS_OK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::unlink(b"/HELLO.TXT")
            ),
            alloc::vec![VFS_NOT_FOUND]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::unlink(b"/EFI")
            ),
            alloc::vec![VFS_IS_DIR]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::rmdir(b"/HELLO.TXT")
            ),
            alloc::vec![VFS_NOT_FOUND]
        );
        // rmdir on a regular file answers NOT_DIR (the server fixture has no
        // README.TXT, so create one to exercise the kind check).
        volume.create_file(&mut disk, b"/FILE.TXT").unwrap();
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::rmdir(b"/FILE.TXT")
            ),
            alloc::vec![VFS_NOT_DIR]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::mkdir(b"/WORK")
            ),
            alloc::vec![VFS_OK]
        );
        let mut child = volume.create_file(&mut disk, b"/WORK/A.TXT").unwrap();
        volume.set_file_len(&mut disk, &mut child, 2).unwrap();
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::rmdir(b"/WORK")
            ),
            alloc::vec![VFS_NOT_EMPTY]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::unlink(b"/WORK/A.TXT")
            ),
            alloc::vec![VFS_OK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::rmdir(b"/WORK")
            ),
            alloc::vec![VFS_OK]
        );
    }

    #[test]
    fn rename_through_dispatch_and_refusals() {
        let (volume, mut disk) = writable();
        let mut table = OpenTable::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let mut temp = volume.create_file(&mut disk, b"/SAVE.TMP").unwrap();
        volume.set_file_len(&mut disk, &mut temp, 3).unwrap();
        volume
            .write_file_at(&mut disk, &mut temp, 0, b"new")
            .unwrap();
        let mut old = volume.create_file(&mut disk, b"/SAVE.TXT").unwrap();
        volume.set_file_len(&mut disk, &mut old, 4).unwrap();
        volume
            .write_file_at(&mut disk, &mut old, 0, b"old!")
            .unwrap();

        let request = encode_request(Request::rename(b"/SAVE.TMP", b"/SAVE.TXT"));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        assert_eq!(&reply[..n], &[VFS_OK]);
        let landed = volume.resolve_path(&mut disk, b"/SAVE.TXT").unwrap();
        let mut out = [0u8; 8];
        let got = volume.read_file(&mut disk, &landed, &mut out);
        assert_eq!(&out[..got], b"new");

        let request = encode_request(Request::rename(b"/NOPE.TXT", b"/OTHER.TXT"));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        assert_eq!(&reply[..n], &[VFS_NOT_FOUND]);

        let request = encode_request(Request::mkdir(b"/WORK"));
        let _ = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        let request = encode_request(Request::rename(b"/WORK", b"/OTHER"));
        let n = dispatch(&volume, &mut disk, &mut table, &request, &mut reply);
        assert_eq!(&reply[..n], &[VFS_NOT_SUPPORTED]);
    }

    #[test]
    fn advisory_locks_lock_upgrade_and_release_through_dispatch() {
        let (volume, mut disk) = writable();
        let mut table = OpenTable::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let first = {
            let bytes = call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::open(b"/HELLO.TXT"),
            );
            parse_open_reply(&bytes).unwrap().0
        };
        let second = {
            let bytes = call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::open(b"/HELLO.TXT"),
            );
            parse_open_reply(&bytes).unwrap().0
        };
        assert_ne!(first, second);

        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::lock(first, LOCK_EX)
            ),
            alloc::vec![VFS_OK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::lock(second, LOCK_SH)
            ),
            alloc::vec![VFS_WOULD_BLOCK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::lock(first, LOCK_UN)
            ),
            alloc::vec![VFS_OK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::lock(second, LOCK_SH)
            ),
            alloc::vec![VFS_OK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::lock(first, LOCK_EX)
            ),
            alloc::vec![VFS_WOULD_BLOCK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::lock(first, LOCK_SH)
            ),
            alloc::vec![VFS_OK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::lock(second, LOCK_UN)
            ),
            alloc::vec![VFS_OK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::lock(first, LOCK_EX)
            ),
            alloc::vec![VFS_OK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::lock(first, LOCK_UN)
            ),
            alloc::vec![VFS_OK]
        );
        assert_eq!(
            call(
                &volume,
                &mut disk,
                &mut table,
                &mut reply,
                Request::lock(99, LOCK_SH)
            ),
            alloc::vec![VFS_BAD_HANDLE]
        );
    }

    fn shared_call(
        volume: &FatVolume,
        disk: &mut WritableDisk,
        server: &mut SharedServer,
        session: usize,
        reply: &mut [u8],
        request: Request<'_>,
    ) -> alloc::vec::Vec<u8> {
        let frame = encode_request(request);
        let n = dispatch_shared(volume, disk, server, session, &frame, reply);
        reply[..n].to_vec()
    }

    fn shared_open(
        volume: &FatVolume,
        disk: &mut WritableDisk,
        server: &mut SharedServer,
        session: usize,
        path: &[u8],
    ) -> u32 {
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let bytes = shared_call(
            volume,
            disk,
            server,
            session,
            &mut reply,
            Request::open(path),
        );
        parse_open_reply(&bytes).expect("open").0
    }

    #[test]
    fn shared_sessions_contend_for_an_advisory_lock() {
        let (volume, mut disk) = writable();
        let mut server = SharedServer::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let first = shared_open(&volume, &mut disk, &mut server, 0, b"/HELLO.TXT");
        let second = shared_open(&volume, &mut disk, &mut server, 1, b"/HELLO.TXT");
        let mut lock = |session: usize, handle: u32, operation: u8| {
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                session,
                &mut reply,
                Request::lock(handle, operation),
            )
        };
        assert_eq!(lock(0, first, LOCK_EX), alloc::vec![VFS_OK]);
        assert_eq!(lock(1, second, LOCK_SH), alloc::vec![VFS_WOULD_BLOCK]);
        assert_eq!(lock(0, first, LOCK_UN), alloc::vec![VFS_OK]);
        assert_eq!(lock(1, second, LOCK_SH), alloc::vec![VFS_OK]);
        assert_eq!(lock(0, first, LOCK_EX), alloc::vec![VFS_WOULD_BLOCK]);
        assert_eq!(lock(1, second, LOCK_EX), alloc::vec![VFS_OK]);
        assert_eq!(lock(1, second, LOCK_UN), alloc::vec![VFS_OK]);
        assert_eq!(lock(1, 77, LOCK_SH), alloc::vec![VFS_BAD_HANDLE]);
        let mut out = [0u8; REPLY_BUF_BYTES];
        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                MAX_SESSIONS,
                &mut out,
                Request::stat(b"/HELLO.TXT")
            ),
            alloc::vec![VFS_BAD_REQUEST]
        );
    }

    #[test]
    fn shared_rename_is_visible_to_the_other_session() {
        let (volume, mut disk) = writable();
        let mut server = SharedServer::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let mut temp = volume.create_file(&mut disk, b"/SAVE.TMP").unwrap();
        volume.set_file_len(&mut disk, &mut temp, 3).unwrap();
        volume
            .write_file_at(&mut disk, &mut temp, 0, b"new")
            .unwrap();

        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                0,
                &mut reply,
                Request::rename(b"/SAVE.TMP", b"/SAVE.TXT")
            ),
            alloc::vec![VFS_OK]
        );
        let bytes = shared_call(
            &volume,
            &mut disk,
            &mut server,
            1,
            &mut reply,
            Request::stat(b"/SAVE.TXT"),
        );
        let stat = kumo_vfs::parse_stat_reply(&bytes).expect("stat");
        assert_eq!(stat.kind, STAT_KIND_FILE);
        assert_eq!(stat.size, 3);
        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                1,
                &mut reply,
                Request::stat(b"/SAVE.TMP")
            ),
            alloc::vec![VFS_NOT_FOUND]
        );
    }

    #[test]
    fn read_only_sessions_cannot_mutate_the_volume() {
        let (volume, mut disk) = writable();
        let mut server = SharedServer::new();
        server.set_access(1, SESSION_READ_ONLY);
        let mut reply = [0u8; REPLY_BUF_BYTES];

        // The read-only session reads fine.
        let bytes = shared_call(
            &volume,
            &mut disk,
            &mut server,
            1,
            &mut reply,
            Request::stat(b"/HELLO.TXT"),
        );
        assert_eq!(
            kumo_vfs::parse_stat_reply(&bytes).expect("stat").kind,
            STAT_KIND_FILE
        );
        // Every mutation shape is refused before any handler runs.
        for request in [
            Request::open_options(b"/NEW.TXT", OPEN_READ | OPEN_WRITE | OPEN_CREATE),
            Request::write(1, 0, b"x"),
            Request::set_len(1, 1),
            Request::mkdir(b"/WORK"),
            Request::unlink(b"/HELLO.TXT"),
            Request::rmdir(b"/EFI"),
            Request::rename(b"/HELLO.TXT", b"/OTHER.TXT"),
        ] {
            let reply_bytes = shared_call(&volume, &mut disk, &mut server, 1, &mut reply, request);
            assert_eq!(reply_bytes, alloc::vec![VFS_ACCESS_DENIED], "{:?}", request);
        }
        // The rw session still mutates.
        let reply_bytes = shared_call(
            &volume,
            &mut disk,
            &mut server,
            0,
            &mut reply,
            Request::mkdir(b"/WORK"),
        );
        assert_eq!(reply_bytes, alloc::vec![VFS_OK]);
    }

    #[test]
    fn closing_a_session_releases_its_locks_and_table() {
        let (volume, mut disk) = writable();
        let mut server = SharedServer::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let first = shared_open(&volume, &mut disk, &mut server, 0, b"/HELLO.TXT");
        let second = shared_open(&volume, &mut disk, &mut server, 1, b"/HELLO.TXT");
        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                0,
                &mut reply,
                Request::lock(first, LOCK_EX)
            ),
            alloc::vec![VFS_OK]
        );
        // The durable lock blocks the other session until the holder is closed.
        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                1,
                &mut reply,
                Request::lock(second, LOCK_SH)
            ),
            alloc::vec![VFS_WOULD_BLOCK]
        );
        server.close_session(0);
        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                1,
                &mut reply,
                Request::lock(second, LOCK_SH)
            ),
            alloc::vec![VFS_OK]
        );
        // The closed session's handles are gone.
        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                0,
                &mut reply,
                Request::flush(first)
            ),
            alloc::vec![VFS_BAD_HANDLE]
        );
    }

    #[test]
    fn chmod_records_a_mode_that_stat_reports_across_sessions() {
        let (volume, mut disk) = writable();
        let mut server = SharedServer::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                0,
                &mut reply,
                Request::chmod(b"/HELLO.TXT", 0o640)
            ),
            alloc::vec![VFS_OK]
        );
        let bytes = shared_call(
            &volume,
            &mut disk,
            &mut server,
            1,
            &mut reply,
            Request::stat(b"/HELLO.TXT"),
        );
        let stat = kumo_vfs::parse_stat_reply(&bytes).expect("stat");
        assert_eq!(stat.mode, 0o640);
        assert_eq!(stat.size, 6);
        // A second chmod replaces the recorded value; the file-type bits are ignored.
        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                1,
                &mut reply,
                Request::chmod(b"/HELLO.TXT", 0o100600)
            ),
            alloc::vec![VFS_OK]
        );
        let bytes = shared_call(
            &volume,
            &mut disk,
            &mut server,
            0,
            &mut reply,
            Request::stat(b"/HELLO.TXT"),
        );
        assert_eq!(
            kumo_vfs::parse_stat_reply(&bytes).expect("stat").mode,
            0o600
        );
        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                0,
                &mut reply,
                Request::chmod(b"/NOPE.TXT", 0o600)
            ),
            alloc::vec![VFS_NOT_FOUND]
        );
        // Read-only sessions cannot chmod.
        server.set_access(2, SESSION_READ_ONLY);
        assert_eq!(
            shared_call(
                &volume,
                &mut disk,
                &mut server,
                2,
                &mut reply,
                Request::chmod(b"/HELLO.TXT", 0o777)
            ),
            alloc::vec![VFS_ACCESS_DENIED]
        );
    }
}
