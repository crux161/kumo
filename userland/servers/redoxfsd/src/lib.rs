#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j928
//j929
//J944_r10_001
//! `redoxfsd`, the RedoxFS-backed `kumo-vfs` handler core (SH8-b, PLAN/008 S5/S6).
//!
//! It is the mount-scoped `fatfs` server's protocol with RedoxFS underneath. One process
//! serves one volume to many sessions. Each session has its own open table, and advisory
//! locks are shared across sessions. The volume's bytes are authoritative; everything in
//! this crate is reconnectable soft state that a restart drops (DESIGN/002).
//!
//! SH8-c (j929): **every acknowledgement is a committed transaction.** j928 acknowledged writes
//! before committing them (coalescing up to 64 KiB); a crash lost acknowledged bytes, which
//! DESIGN/002's hard rule forbids. Now `Write` commits before it replies, and `WriteVmo` moves
//! up to 4 MiB in one committed transaction for bulk writers. A server killed at any moment
//! leaves only what it had acknowledged, and its clients retry the rest (DESIGN/002 §2).
//!
//! What stays is soft state over committed bytes:
//! - [`CachedDisk`] is a write-through block cache. It never delays a write, so the volume's
//!   commit order (data, flush, header, flush) is exactly the one `kumo-redoxfs` proves.
//! - Sequential reads are served from a [`READ_AHEAD_MAX`] buffer that every mutation drops.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use kumo_redoxfs::redoxfs::{Disk, FileSystem, Node, Transaction, TreeData, TreePtr, BLOCK_SIZE};
use kumo_vfs::{
    encode_open_ok, encode_read_vmo_ok, encode_stat_ok, encode_status, encode_write_ok,
    DirReplyWriter, Request, LOCK_EX, LOCK_SH, LOCK_UN, MAX_PATH, MAX_WRITE_DATA, OPEN_CREATE,
    OPEN_READ, OPEN_WRITE, READ_VMO_MAX, STAT_KIND_DIRECTORY, STAT_KIND_FILE, VFS_ACCESS_DENIED,
    VFS_BAD_HANDLE, VFS_BAD_REQUEST, VFS_EXISTS, VFS_IO_ERROR, VFS_IS_DIR, VFS_NOT_DIR,
    VFS_NOT_EMPTY, VFS_NOT_FOUND, VFS_NOT_SUPPORTED, VFS_NO_SPACE, VFS_OK, VFS_WOULD_BLOCK,
    WRITE_HEADER_LEN,
};
use syscall::error::{
    Error, Result, EACCES, EEXIST, EINVAL, EISDIR, ENAMETOOLONG, ENOENT, ENOSPC, ENOTDIR,
    ENOTEMPTY, EPERM, EROFS,
};

pub use kumo_redoxfs;

/// Sessions one mount admits, as many as fatfs's mount-scoped server (a Linux process tree
/// holds one session per process).
pub const MAX_SESSIONS: usize = 16;
/// Open handles per session.
pub const MAX_OPEN_FILES: usize = 32;
/// Advisory locks tracked across all sessions.
pub const MAX_LOCKS: usize = 16;
/// Session access byte: reads only. The launcher's grant is enforced here because the server
/// itself holds a read-write block channel.
pub const SESSION_READ_ONLY: u8 = 0;
/// Session access byte: reads and mutations.
pub const SESSION_READ_WRITE: u8 = 1;
/// The largest request frame `kumo-vfs` sends.
pub const REQUEST_BUF_BYTES: usize = WRITE_HEADER_LEN + MAX_WRITE_DATA;
/// The serve loop's reply buffer, sized like fatfs's so a directory page always holds one
/// record with the longest name.
pub const REPLY_BUF_BYTES: usize = 1024;
/// Inline read payload cap (fatfs parity), plus the status byte.
pub const READ_REPLY_BYTES: usize = 1 + 256;
/// One read-ahead fill.
pub const READ_AHEAD_MAX: usize = 64 * 1024;
/// Blocks [`CachedDisk`] holds by default (4 MiB).
pub const CACHE_BLOCKS: usize = 1024;
/// The mode a created file gets; the VFS open carries none.
pub const DEFAULT_FILE_MODE: u16 = 0o644;
/// The mode a created directory gets.
pub const DEFAULT_DIR_MODE: u16 = 0o755;

const _: () = assert!(REPLY_BUF_BYTES >= 2 + 10 + u8::MAX as usize);

// --- Block cache ---

/// A direct-mapped, write-through cache of RedoxFS blocks over another [`Disk`].
/// A write reaches the inner disk before the cache sees it, and `sync` passes straight
/// through, so caching changes no ordering the inner disk observes.
pub struct CachedDisk<D> {
    inner: D,
    tags: Vec<u64>,
    data: Vec<u8>,
    pub hits: u64,
    pub misses: u64,
}

const NO_TAG: u64 = u64::MAX;
const BLOCK: usize = BLOCK_SIZE as usize;

impl<D: Disk> CachedDisk<D> {
    /// Cache `blocks` blocks (rounded up to one) over `inner`.
    pub fn new(inner: D, blocks: usize) -> Self {
        let blocks = blocks.max(1);
        Self {
            inner,
            tags: vec![NO_TAG; blocks],
            data: vec![0; blocks * BLOCK],
            hits: 0,
            misses: 0,
        }
    }

    pub fn inner(&self) -> &D {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut D {
        &mut self.inner
    }

    pub fn into_inner(self) -> D {
        self.inner
    }

    fn slot(&self, block: u64) -> usize {
        (block % self.tags.len() as u64) as usize
    }

    fn fill(&mut self, block: u64, bytes: &[u8]) {
        let slot = self.slot(block);
        self.tags[slot] = block;
        self.data[slot * BLOCK..(slot + 1) * BLOCK].copy_from_slice(bytes);
    }

    fn forget(&mut self, block: u64) {
        let slot = self.slot(block);
        if self.tags[slot] == block {
            self.tags[slot] = NO_TAG;
        }
    }
}

impl<D: Disk> Disk for CachedDisk<D> {
    unsafe fn read_at(&mut self, block: u64, buffer: &mut [u8]) -> Result<usize> {
        if !buffer.len().is_multiple_of(BLOCK) {
            // Never seen from RedoxFS; serve it uncached rather than guess.
            return unsafe { self.inner.read_at(block, buffer) };
        }
        let count = (buffer.len() / BLOCK) as u64;
        let all_hit = (0..count).all(|i| {
            let b = block.wrapping_add(i);
            self.tags[self.slot(b)] == b
        });
        if all_hit {
            for (i, chunk) in buffer.chunks_exact_mut(BLOCK).enumerate() {
                let slot = self.slot(block + i as u64);
                chunk.copy_from_slice(&self.data[slot * BLOCK..(slot + 1) * BLOCK]);
            }
            self.hits += count;
            return Ok(buffer.len());
        }
        self.misses += count;
        let n = unsafe { self.inner.read_at(block, buffer)? };
        if n == buffer.len() {
            for (i, chunk) in buffer.chunks_exact(BLOCK).enumerate() {
                self.fill(block + i as u64, chunk);
            }
        }
        Ok(n)
    }

    unsafe fn write_at(&mut self, block: u64, buffer: &[u8]) -> Result<usize> {
        let blocks = buffer.len().div_ceil(BLOCK) as u64;
        let result = unsafe { self.inner.write_at(block, buffer) };
        match result {
            Ok(n) if n == buffer.len() && buffer.len().is_multiple_of(BLOCK) => {
                for (i, chunk) in buffer.chunks_exact(BLOCK).enumerate() {
                    self.fill(block + i as u64, chunk);
                }
            }
            _ => {
                // A failed or partial write leaves the device's bytes unknown.
                for i in 0..blocks {
                    self.forget(block.wrapping_add(i));
                }
            }
        }
        result
    }

    fn size(&mut self) -> Result<u64> {
        self.inner.size()
    }

    fn sync(&mut self) -> Result<()> {
        self.inner.sync()
    }
}

// --- Bulk reads ---

/// The server side of [`Request::ReadVmo`]: access to the VMO a request carried (the same
/// contract as fatfs's). The library never closes `vmo`; the serve loop does.
pub trait BulkWindow {
    /// Map `len` bytes of `vmo` from `vmo_offset` writable, let `fill` write into them and
    /// unmap. `None` when the window could not be mapped.
    fn fill(
        &mut self,
        vmo: u32,
        vmo_offset: u64,
        len: usize,
        fill: &mut dyn FnMut(&mut [u8]) -> usize,
    ) -> Option<usize>;
}

/// No VMO access: `ReadVmo` answers [`VFS_BAD_REQUEST`].
pub struct NoWindow;

impl BulkWindow for NoWindow {
    fn fill(
        &mut self,
        _vmo: u32,
        _vmo_offset: u64,
        _len: usize,
        _fill: &mut dyn FnMut(&mut [u8]) -> usize,
    ) -> Option<usize> {
        None
    }
}

// --- Soft state ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenFile {
    /// The node's tree id: stable across renames, unlike a FAT directory slot.
    node: u32,
    readable: bool,
    writable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenTable {
    slots: [Option<OpenFile>; MAX_OPEN_FILES],
}

impl OpenTable {
    const fn new() -> Self {
        Self {
            slots: [None; MAX_OPEN_FILES],
        }
    }

    fn index(handle: u32) -> Option<usize> {
        usize::try_from(handle)
            .ok()?
            .checked_sub(1)
            .filter(|i| *i < MAX_OPEN_FILES)
    }

    fn has_room(&self) -> bool {
        self.slots.iter().any(Option::is_none)
    }

    fn insert(&mut self, file: OpenFile) -> Option<u32> {
        let index = self.slots.iter().position(Option::is_none)?;
        self.slots[index] = Some(file);
        Some(index as u32 + 1)
    }

    fn get(&self, handle: u32) -> Option<OpenFile> {
        self.slots[Self::index(handle)?]
    }

    fn remove(&mut self, handle: u32) -> Option<OpenFile> {
        self.slots[Self::index(handle)?].take()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LockEntry {
    node: u32,
    session: usize,
    mode: u8,
}

/// File bytes `[offset, offset + len)` of `node`, read ahead.
#[derive(Clone, Copy)]
struct Ahead {
    node: u32,
    offset: u64,
    len: usize,
}

/// A wall clock for node timestamps: `(seconds, nanoseconds)`.
pub type Clock = fn() -> (u64, u32);

/// The mount-scoped server: one RedoxFS volume and the soft state of every session on it.
pub struct RedoxServer<D: Disk> {
    pub fs: FileSystem<D>,
    sessions: [OpenTable; MAX_SESSIONS],
    access: [u8; MAX_SESSIONS],
    locks: [Option<LockEntry>; MAX_LOCKS],
    ahead: Option<Ahead>,
    ahead_buf: Vec<u8>,
    clock: Clock,
    /// Transactions committed for writes (`Write` and `WriteVmo`), for tests and diagnostics.
    pub write_commits: u64,
}

impl<D: Disk> RedoxServer<D> {
    pub fn new(fs: FileSystem<D>, clock: Clock) -> Self {
        Self {
            fs,
            sessions: [OpenTable::new(); MAX_SESSIONS],
            access: [SESSION_READ_ONLY; MAX_SESSIONS],
            locks: [None; MAX_LOCKS],
            ahead: None,
            ahead_buf: Vec::new(),
            clock,
            write_commits: 0,
        }
    }

    /// Admit `session` with the launcher-granted access byte.
    pub fn set_access(&mut self, session: usize, access: u8) {
        if let Some(slot) = self.access.get_mut(session) {
            *slot = if access == SESSION_READ_WRITE {
                SESSION_READ_WRITE
            } else {
                SESSION_READ_ONLY
            };
        }
    }

    /// A session's channel closed: close every handle it held (an unlinked node it held open
    /// is released now) and drop its locks. Everything it was told is written is committed.
    pub fn close_session(&mut self, session: usize) {
        if session >= MAX_SESSIONS {
            return;
        }
        let table = core::mem::replace(&mut self.sessions[session], OpenTable::new());
        for file in table.slots.iter().flatten() {
            let _ = self
                .fs
                .tx(|tx| tx.on_close_node(TreePtr::<Node>::new(file.node)));
        }
        for lock in self.locks.iter_mut() {
            if lock.is_some_and(|l| l.session == session) {
                *lock = None;
            }
        }
        self.access[session] = SESSION_READ_ONLY;
        self.ahead = None;
    }

    /// Handle one request frame from `session` and write the reply. `vmo` is the handle the
    /// message carried (0 for none); the caller closes it afterwards.
    pub fn dispatch<W: BulkWindow>(
        &mut self,
        session: usize,
        request: &[u8],
        vmo: u32,
        window: &mut W,
        reply: &mut [u8],
    ) -> usize {
        if session >= MAX_SESSIONS {
            return write_status(reply, VFS_BAD_REQUEST);
        }
        let Some(request) = Request::decode(request) else {
            return write_status(reply, VFS_BAD_REQUEST);
        };
        let writes = request_writes(&request);
        if writes && self.access[session] == SESSION_READ_ONLY {
            return write_status(reply, VFS_ACCESS_DENIED);
        }
        if writes {
            self.ahead = None;
        }
        match request {
            Request::Open { path } => self.open(session, path, OPEN_READ, reply),
            Request::OpenOptions { path, options } => self.open(session, path, options, reply),
            Request::Read {
                handle,
                offset,
                len,
            } => self.read(session, handle, offset, len, reply),
            Request::ReadVmo {
                handle,
                offset,
                len,
                vmo_offset,
            } => self.read_vmo(session, handle, offset, len, vmo, vmo_offset, window, reply),
            Request::Write {
                handle,
                offset,
                data,
            } => self.write(session, handle, offset, data, reply),
            Request::WriteVmo {
                handle,
                offset,
                len,
                vmo_offset,
            } => self.write_vmo(session, handle, offset, len, vmo, vmo_offset, window, reply),
            Request::SetLen { handle, len } => self.set_len(session, handle, len, reply),
            Request::Flush { handle } => self.flush(session, handle, reply),
            Request::ReadDir { path, start } => self.read_dir(path, start, reply),
            Request::Mkdir { path } => self.mkdir(path, reply),
            Request::Unlink { path } => self.remove(path, Node::MODE_FILE, reply),
            Request::Rmdir { path } => self.remove(path, Node::MODE_DIR, reply),
            Request::Rename { from, to } => self.rename(from, to, reply),
            Request::Stat { path } => self.stat(path, reply),
            Request::Chmod { path, mode } => self.chmod(path, mode, reply),
            Request::Lock { handle, operation } => self.lock(session, handle, operation, reply),
            Request::Close { handle } => self.close(session, handle, reply),
            // Hard links wait for a client that needs them (SH8-b cut); the client has a
            // documented fallback.
            Request::Link { .. } => write_status(reply, VFS_NOT_SUPPORTED),
        }
    }

    fn open(&mut self, session: usize, path: &[u8], options: u8, reply: &mut [u8]) -> usize {
        if !self.sessions[session].has_room() {
            return write_status(reply, VFS_BAD_HANDLE);
        }
        let (secs, nsec) = (self.clock)();
        let found = self.fs.tx(|tx| {
            let (parent, name) = parent_and_name(tx, path)?;
            let node = match tx.find_node(parent.ptr(), name) {
                Ok(node) => node,
                Err(error) if error.errno == ENOENT && options & OPEN_CREATE != 0 => tx
                    .create_node(
                        parent.ptr(),
                        name,
                        Node::MODE_FILE | DEFAULT_FILE_MODE,
                        secs,
                        nsec,
                    )?,
                Err(error) => return Err(error),
            };
            if node.data().is_dir() {
                return Err(Error::new(EISDIR));
            }
            Ok((node.id(), node.data().size()))
        });
        let (node, size) = match found {
            Ok(found) => found,
            // The root itself has no parent and is a directory.
            Err(error) if error.errno == EISDIR => return write_status(reply, VFS_IS_DIR),
            Err(error) => return write_status(reply, status_of(error)),
        };
        if let Err(error) = self.fs.tx(|tx| tx.on_open_node(TreePtr::new(node))) {
            return write_status(reply, status_of(error));
        }
        let file = OpenFile {
            node,
            readable: options & OPEN_READ != 0,
            writable: options & OPEN_WRITE != 0,
        };
        match self.sessions[session].insert(file) {
            // J944_r10_001: the RedoxFS node id is the file's VFS id (POSIX st_ino).
            Some(handle) => write_frame(reply, &encode_open_ok(handle, size, u64::from(node))),
            None => write_status(reply, VFS_BAD_HANDLE),
        }
    }

    fn read(
        &mut self,
        session: usize,
        handle: u32,
        offset: u64,
        len: u32,
        reply: &mut [u8],
    ) -> usize {
        if reply.is_empty() {
            return 0;
        }
        let Some(file) = self.sessions[session].get(handle) else {
            return write_status(reply, VFS_BAD_HANDLE);
        };
        if !file.readable {
            return write_status(reply, VFS_ACCESS_DENIED);
        }
        let cap = (len as usize).min(reply.len().min(READ_REPLY_BYTES) - 1);
        if cap == 0 {
            return write_status(reply, VFS_OK);
        }
        // A hit holds the whole request, or everything up to end of file (a fill shorter than
        // READ_AHEAD_MAX stopped there). A short read mid-file would read as end of file to
        // some clients, so a partial hit refills from `offset` instead.
        let hit = self.ahead.is_some_and(|a| {
            a.node == file.node
                && offset >= a.offset
                && (offset - a.offset + cap as u64 <= a.len as u64 || a.len < READ_AHEAD_MAX)
        });
        if !hit {
            self.ahead_buf.resize(READ_AHEAD_MAX, 0);
            let buf = &mut self.ahead_buf;
            match self
                .fs
                .tx(|tx| tx.read_node(TreePtr::new(file.node), offset, buf, 0, 0))
            {
                Ok(n) => {
                    self.ahead = Some(Ahead {
                        node: file.node,
                        offset,
                        len: n,
                    });
                }
                Err(error) => {
                    self.ahead = None;
                    return write_status(reply, status_of(error));
                }
            }
        }
        let ahead = self.ahead.expect("filled above");
        // A short fill ends at end of file, so a later offset past it reads nothing.
        let start = ((offset - ahead.offset) as usize).min(ahead.len);
        let n = cap.min(ahead.len - start);
        reply[0] = VFS_OK;
        reply[1..1 + n].copy_from_slice(&self.ahead_buf[start..start + n]);
        1 + n
    }

    #[allow(clippy::too_many_arguments)]
    fn read_vmo<W: BulkWindow>(
        &mut self,
        session: usize,
        handle: u32,
        offset: u64,
        len: u32,
        vmo: u32,
        vmo_offset: u64,
        window: &mut W,
        reply: &mut [u8],
    ) -> usize {
        if vmo == 0 || len > READ_VMO_MAX {
            return write_status(reply, VFS_BAD_REQUEST);
        }
        let Some(file) = self.sessions[session].get(handle) else {
            return write_status(reply, VFS_BAD_HANDLE);
        };
        if !file.readable {
            return write_status(reply, VFS_ACCESS_DENIED);
        }
        let size = match self
            .fs
            .tx(|tx| tx.read_tree(TreePtr::<Node>::new(file.node)))
        {
            Ok(node) => node.data().size(),
            Err(error) => return write_status(reply, status_of(error)),
        };
        let want = size.saturating_sub(offset).min(len as u64) as usize;
        if want == 0 {
            return write_frame(reply, &encode_read_vmo_ok(0));
        }
        let fs = &mut self.fs;
        let mut failed = None;
        let landed = window.fill(vmo, vmo_offset, want, &mut |out| match fs
            .tx(|tx| tx.read_node(TreePtr::new(file.node), offset, out, 0, 0))
        {
            Ok(n) => n,
            Err(error) => {
                failed = Some(status_of(error));
                0
            }
        });
        match (landed, failed) {
            (_, Some(status)) => write_status(reply, status),
            (Some(count), None) => write_frame(reply, &encode_read_vmo_ok(count as u32)),
            (None, None) => write_status(reply, VFS_BAD_REQUEST),
        }
    }

    /// Commit `data` at `offset` before replying: the acknowledgement IS the commit.
    fn write(
        &mut self,
        session: usize,
        handle: u32,
        offset: u64,
        data: &[u8],
        reply: &mut [u8],
    ) -> usize {
        let node = match self.writable(session, handle) {
            Ok(node) => node,
            Err(status) => return write_status(reply, status),
        };
        match self.commit_write(node, offset, data) {
            Ok(()) => write_frame(reply, &encode_write_ok(data.len() as u32)),
            Err(status) => write_status(reply, status),
        }
    }

    /// `WriteVmo`: up to [`READ_VMO_MAX`] bytes of the carried VMO in ONE committed transaction.
    #[allow(clippy::too_many_arguments)]
    fn write_vmo<W: BulkWindow>(
        &mut self,
        session: usize,
        handle: u32,
        offset: u64,
        len: u32,
        vmo: u32,
        vmo_offset: u64,
        window: &mut W,
        reply: &mut [u8],
    ) -> usize {
        if vmo == 0 || len > READ_VMO_MAX {
            return write_status(reply, VFS_BAD_REQUEST);
        }
        let node = match self.writable(session, handle) {
            Ok(node) => node,
            Err(status) => return write_status(reply, status),
        };
        if len == 0 {
            return write_frame(reply, &encode_write_ok(0));
        }
        let mut result = Err(VFS_BAD_REQUEST);
        let mapped = window.fill(vmo, vmo_offset, len as usize, &mut |bytes| {
            result = self.commit_write(node, offset, bytes);
            0
        });
        match (mapped, result) {
            (Some(_), Ok(())) => write_frame(reply, &encode_write_ok(len)),
            (Some(_), Err(status)) => write_status(reply, status),
            (None, _) => write_status(reply, VFS_BAD_REQUEST),
        }
    }

    /// The node an open, writable `handle` names.
    fn writable(&self, session: usize, handle: u32) -> core::result::Result<u32, u8> {
        let file = self.sessions[session].get(handle).ok_or(VFS_BAD_HANDLE)?;
        if !file.writable {
            return Err(VFS_ACCESS_DENIED);
        }
        Ok(file.node)
    }

    /// One transaction: write `data` at `offset` of `node`, committed (data, flush, header,
    /// flush) before this returns.
    fn commit_write(
        &mut self,
        node: u32,
        offset: u64,
        data: &[u8],
    ) -> core::result::Result<(), u8> {
        if offset.checked_add(data.len() as u64).is_none() {
            return Err(VFS_BAD_REQUEST);
        }
        self.ahead = None;
        let (secs, nsec) = (self.clock)();
        self.write_commits += 1;
        match self
            .fs
            .tx(|tx| tx.write_node(TreePtr::new(node), offset, data, secs, nsec))
        {
            Ok(n) if n == data.len() => Ok(()),
            Ok(_) => Err(VFS_IO_ERROR),
            Err(error) => Err(status_of(error)),
        }
    }

    fn set_len(&mut self, session: usize, handle: u32, len: u64, reply: &mut [u8]) -> usize {
        let node = match self.writable(session, handle) {
            Ok(node) => node,
            Err(status) => return write_status(reply, status),
        };
        let (secs, nsec) = (self.clock)();
        match self
            .fs
            .tx(|tx| tx.truncate_node(TreePtr::new(node), len, secs, nsec))
        {
            Ok(()) => write_status(reply, VFS_OK),
            Err(error) => write_status(reply, status_of(error)),
        }
    }

    /// Every acknowledged mutation already ended in a device flush, so a flush has nothing
    /// left to make durable.
    fn flush(&mut self, session: usize, handle: u32, reply: &mut [u8]) -> usize {
        match self.sessions[session].get(handle) {
            Some(_) => write_status(reply, VFS_OK),
            None => write_status(reply, VFS_BAD_HANDLE),
        }
    }

    fn read_dir(&mut self, path: &[u8], start: u32, reply: &mut [u8]) -> usize {
        let mut children = Vec::new();
        let listed = self.fs.tx(|tx| {
            let dir = walk(tx, path)?;
            if !dir.data().is_dir() {
                return Err(Error::new(ENOTDIR));
            }
            let mut entries = Vec::new();
            tx.child_nodes(dir.ptr(), &mut entries)?;
            for entry in entries.iter().skip(start as usize) {
                let Some(name) = entry.name() else { continue };
                let node = tx.read_tree(entry.node_ptr())?;
                children.push((node.data().is_dir(), node.data().size(), Vec::from(name)));
            }
            Ok(())
        });
        if let Err(error) = listed {
            return write_status(reply, status_of(error));
        }
        let Some(mut listing) = DirReplyWriter::new(reply) else {
            return 0;
        };
        let prefix = path.len() + usize::from(!path.ends_with(b"/"));
        for (is_dir, size, name) in &children {
            if prefix + name.len() > MAX_PATH {
                // A name the client could never open is not listed; it still counts toward
                // `start` above, so paging stays consistent.
                continue;
            }
            if !listing.push(*is_dir, *size, name) {
                break;
            }
        }
        listing.finish()
    }

    fn mkdir(&mut self, path: &[u8], reply: &mut [u8]) -> usize {
        let (secs, nsec) = (self.clock)();
        let made = self.fs.tx(|tx| {
            let (parent, name) = parent_and_name(tx, path)?;
            tx.create_node(
                parent.ptr(),
                name,
                Node::MODE_DIR | DEFAULT_DIR_MODE,
                secs,
                nsec,
            )
            .map(|_| ())
        });
        match made {
            Ok(()) => write_status(reply, VFS_OK),
            Err(error) if error.errno == EISDIR => write_status(reply, VFS_EXISTS),
            Err(error) => write_status(reply, status_of(error)),
        }
    }

    fn remove(&mut self, path: &[u8], mode: u16, reply: &mut [u8]) -> usize {
        let removed = self.fs.tx(|tx| {
            let (parent, name) = parent_and_name(tx, path)?;
            tx.remove_node(parent.ptr(), name, mode).map(|_| ())
        });
        match removed {
            Ok(()) => write_status(reply, VFS_OK),
            // The root: neither a file to unlink nor a directory anyone may remove.
            Err(error) if error.errno == EISDIR && mode == Node::MODE_DIR => {
                write_status(reply, VFS_ACCESS_DENIED)
            }
            Err(error) => write_status(reply, status_of(error)),
        }
    }

    fn rename(&mut self, from: &[u8], to: &[u8], reply: &mut [u8]) -> usize {
        let (Some(from_parts), Some(to_parts)) = (components(from), components(to)) else {
            return write_status(reply, VFS_BAD_REQUEST);
        };
        // A directory cannot move under itself; normalized paths make this a prefix test.
        if to_parts.len() > from_parts.len() && to_parts[..from_parts.len()] == from_parts[..] {
            return write_status(reply, VFS_BAD_REQUEST);
        }
        let renamed = self.fs.tx(|tx| {
            let (from_parent, from_name) = parent_and_name(tx, from)?;
            let (to_parent, to_name) = parent_and_name(tx, to)?;
            let source = tx.find_node(from_parent.ptr(), from_name)?;
            match tx.find_node(to_parent.ptr(), to_name) {
                Ok(target) if target.id() == source.id() => return Ok(()),
                // A directory renames only to a free name (fatfs parity, Cargo's atomic
                // target directory); a file never replaces a directory.
                Ok(_) if source.data().is_dir() => return Err(Error::new(EEXIST)),
                Ok(target) if target.data().is_dir() => return Err(Error::new(EISDIR)),
                Ok(_) => {}
                Err(error) if error.errno == ENOENT => {}
                Err(error) => return Err(error),
            }
            tx.rename_node(from_parent.ptr(), from_name, to_parent.ptr(), to_name)
        });
        match renamed {
            Ok(()) => write_status(reply, VFS_OK),
            Err(error) => write_status(reply, status_of(error)),
        }
    }

    fn stat(&mut self, path: &[u8], reply: &mut [u8]) -> usize {
        match self.fs.tx(|tx| walk(tx, path)) {
            Ok(node) => {
                let kind = if node.data().is_dir() {
                    STAT_KIND_DIRECTORY
                } else {
                    STAT_KIND_FILE
                };
                let mode = node.data().mode() & Node::MODE_PERM;
                write_frame(
                    reply,
                    &encode_stat_ok(kind, mode, node.data().size(), u64::from(node.id())),
                )
            }
            Err(error) => write_status(reply, status_of(error)),
        }
    }

    /// Permission bits only; the type bits belong to the node.
    fn chmod(&mut self, path: &[u8], mode: u16, reply: &mut [u8]) -> usize {
        let changed = self.fs.tx(|tx| {
            let mut node = walk(tx, path)?;
            let kept = node.data().mode() & Node::MODE_TYPE;
            node.data_mut().set_mode(kept | (mode & Node::MODE_PERM));
            tx.sync_tree(node)
        });
        match changed {
            Ok(()) => write_status(reply, VFS_OK),
            Err(error) => write_status(reply, status_of(error)),
        }
    }

    fn lock(&mut self, session: usize, handle: u32, operation: u8, reply: &mut [u8]) -> usize {
        let Some(file) = self.sessions[session].get(handle) else {
            return write_status(reply, VFS_BAD_HANDLE);
        };
        let mine =
            |l: &Option<LockEntry>| l.is_some_and(|l| l.node == file.node && l.session == session);
        match operation {
            LOCK_UN => {
                for lock in self.locks.iter_mut().filter(|l| mine(l)) {
                    *lock = None;
                }
                write_status(reply, VFS_OK)
            }
            LOCK_SH | LOCK_EX => {
                let exclusive = operation == LOCK_EX;
                let conflict = self.locks.iter().flatten().any(|l| {
                    l.node == file.node && l.session != session && (exclusive || l.mode == LOCK_EX)
                });
                if conflict {
                    return write_status(reply, VFS_WOULD_BLOCK);
                }
                let slot = match self.locks.iter().position(mine) {
                    Some(slot) => slot,
                    None => match self.locks.iter().position(Option::is_none) {
                        Some(slot) => slot,
                        None => return write_status(reply, VFS_NO_SPACE),
                    },
                };
                self.locks[slot] = Some(LockEntry {
                    node: file.node,
                    session,
                    mode: operation,
                });
                write_status(reply, VFS_OK)
            }
            _ => write_status(reply, VFS_BAD_REQUEST),
        }
    }

    /// Release a handle. Its session's lock on the node goes with it (fatfs semantics), and an
    /// unlinked node nobody holds any more is freed.
    fn close(&mut self, session: usize, handle: u32, reply: &mut [u8]) -> usize {
        let Some(file) = self.sessions[session].remove(handle) else {
            return write_status(reply, VFS_BAD_HANDLE);
        };
        for lock in self.locks.iter_mut() {
            if lock.is_some_and(|l| l.node == file.node && l.session == session) {
                *lock = None;
            }
        }
        match self.fs.tx(|tx| tx.on_close_node(TreePtr::new(file.node))) {
            Ok(()) => write_status(reply, VFS_OK),
            Err(error) => write_status(reply, status_of(error)),
        }
    }
}

/// Does this request change the volume? Read-only sessions are refused exactly these.
fn request_writes(request: &Request<'_>) -> bool {
    matches!(
        request,
        Request::OpenOptions { options, .. } if options & (OPEN_WRITE | OPEN_CREATE) != 0
    ) || matches!(
        request,
        Request::Write { .. }
            | Request::WriteVmo { .. }
            | Request::SetLen { .. }
            | Request::Mkdir { .. }
            | Request::Unlink { .. }
            | Request::Rmdir { .. }
            | Request::Rename { .. }
            | Request::Chmod { .. }
    )
}

// --- Paths ---

/// The normalized components of `path`: empty and `.` components dropped, `..` steps back
/// (and stops at the root). `None` when a component is not UTF-8, which RedoxFS names must be.
pub fn components(path: &[u8]) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    for raw in path.split(|b| *b == b'/') {
        match raw {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            name => parts.push(core::str::from_utf8(name).ok()?),
        }
    }
    Some(parts)
}

/// The node `path` names.
fn walk<D: Disk>(tx: &mut Transaction<'_, D>, path: &[u8]) -> Result<TreeData<Node>> {
    let parts = components(path).ok_or(Error::new(EINVAL))?;
    walk_parts(tx, &parts)
}

fn walk_parts<D: Disk>(tx: &mut Transaction<'_, D>, parts: &[&str]) -> Result<TreeData<Node>> {
    let mut node = tx.read_tree(TreePtr::<Node>::root())?;
    for part in parts {
        if !node.data().is_dir() {
            return Err(Error::new(ENOTDIR));
        }
        node = tx.find_node(node.ptr(), part)?;
    }
    Ok(node)
}

/// The directory that holds `path`'s last component, and that name. The root has no parent:
/// it answers `EISDIR`, which each caller turns into its own status.
fn parent_and_name<'p, D: Disk>(
    tx: &mut Transaction<'_, D>,
    path: &'p [u8],
) -> Result<(TreeData<Node>, &'p str)> {
    let parts = components(path).ok_or(Error::new(EINVAL))?;
    let Some((name, parent)) = parts.split_last() else {
        return Err(Error::new(EISDIR));
    };
    let parent = walk_parts(tx, parent)?;
    if !parent.data().is_dir() {
        return Err(Error::new(ENOTDIR));
    }
    Ok((parent, name))
}

// --- Replies ---

/// The VFS status for a RedoxFS error.
pub fn status_of(error: Error) -> u8 {
    match error.errno {
        ENOENT => VFS_NOT_FOUND,
        EEXIST => VFS_EXISTS,
        ENOTDIR => VFS_NOT_DIR,
        EISDIR => VFS_IS_DIR,
        ENOTEMPTY => VFS_NOT_EMPTY,
        ENOSPC => VFS_NO_SPACE,
        EROFS | EACCES | EPERM => VFS_ACCESS_DENIED,
        EINVAL | ENAMETOOLONG => VFS_BAD_REQUEST,
        _ => VFS_IO_ERROR,
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
extern crate std;
#[cfg(test)]
mod tests;
