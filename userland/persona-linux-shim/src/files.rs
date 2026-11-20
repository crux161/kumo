//j915
//j916
//j917
//j918
//j919
//! SH7-d2: the Linux file surface over the persona's `kumo-vfs` session.
//!
//! Sora launches the shim with a filesystem grant (`STARTUP_TAG_FS`): a private `fatfs` session
//! over the image or a volume. Every Linux path the program names resolves against that
//! session's root, the cwd starts at `/`, and descriptors 3 and up are the shim's own table of
//! vfs handles (files) and paths (directories). Descriptors 0–2 are the launcher's stdin and
//! stdout channels. SH7-e1 (j918, cut W1): the image session is read-write, so creates, writes,
//! truncation, mkdir/unlink/rmdir/rename and chdir go to the server; a read-only session (a
//! `/media` launch) answers each mutation with the server's refusal as EROFS. — TEAL

use kumo_abi::{Handle, Rights};
use kumo_rt::{
    channel_create_pair, channel_read, channel_try_read_with_handle, channel_write,
    channel_write_with_handle, handle_close, handle_duplicate, log_write, vmar_map_anywhere,
    vmo_create,
};
use kumo_vfs::{
    parse_dir_reply, parse_open_reply, parse_read_vmo_reply, parse_stat_reply, parse_write_reply,
    read_payload, Request, VfsError, MAX_WRITE_DATA, OPEN_CREATE, OPEN_READ, OPEN_WRITE,
    READ_VMO_MAX, STAT_KIND_DIRECTORY, VFS_ACCESS_DENIED, VFS_EXISTS, VFS_IS_DIR, VFS_NOT_DIR,
    VFS_NOT_EMPTY, VFS_NOT_FOUND, VFS_NOT_SUPPORTED, VFS_NO_SPACE, VFS_OK,
};
use persona_linux::stream::ReadTail;

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::{errno, threads};

/// What one vfs `Read` answers at most (the fatfs reply ceiling).
const READ_CHUNK: u64 = 256;
/// SH7-d3a: a read larger than one inline reply goes through this much private VMO at a time:
/// fatfs writes the file bytes into it (`ReadVmo`), and the shim copies them to the program.
const BOUNCE_BYTES: u64 = 1024 * 1024;
const PATH_MAX: usize = kumo_vfs::MAX_PATH;
const MAX_FDS: usize = 64;
const PIPE_CHUNK: usize = 4096;

const O_ACCMODE: u64 = 3;
const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_CREAT: u64 = 0o100;
const O_EXCL: u64 = 0o200;
const O_TRUNC: u64 = 0o1000;
const O_APPEND: u64 = 0o2000;
const AT_REMOVEDIR: u64 = 0x200;
const RENAME_NOREPLACE: u64 = 1;
const O_DIRECTORY: u64 = 0o40000;
const O_NONBLOCK: u64 = 0o4000;
const O_CLOEXEC: u64 = 0o2000000;
const AT_FDCWD: i64 = -100;
const AT_EMPTY_PATH: u64 = 0x1000;
const SEEK_SET: u64 = 0;
const SEEK_CUR: u64 = 1;
const SEEK_END: u64 = 2;
const S_IFCHR: u32 = 0o020000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFIFO: u32 = 0o010000;
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Free,
    Stdin,
    Stdout,
    Pipe {
        channel: u32,
        readable: bool,
        inode: u64,
    },
    /// An open vfs file: its server handle and size.
    File {
        handle: u32,
        size: u64,
    },
    /// A directory: the next listing index (0 and 1 are the synthesized `.` and `..`).
    Dir {
        next: u32,
    },
}

#[derive(Clone, Copy)]
struct Fd {
    kind: Kind,
    offset: u64,
    path: [u8; PATH_MAX],
    path_len: usize,
    flags: u64,
    carry: ReadTail<PIPE_CHUNK>,
    /// SH7-e1: a file written through this descriptor, flushed when it closes.
    dirty: bool,
}

impl Fd {
    const FREE: Fd = Fd {
        kind: Kind::Free,
        offset: 0,
        path: [0; PATH_MAX],
        path_len: 0,
        flags: 0,
        carry: ReadTail::EMPTY,
        dirty: false,
    };

    fn path(&self) -> &[u8] {
        &self.path[..self.path_len]
    }
}

struct Session {
    fs: Handle,
    stdin: Handle,
    stdout: Handle,
    fds: [Fd; MAX_FDS],
    /// Bytes of the last stdin message a short `read` did not take.
    stdin_carry: [u8; 256],
    stdin_carry_at: usize,
    stdin_carry_len: usize,
    /// The bulk-read bounce VMO and where the shim mapped it (0 until the first large read).
    bounce: Handle,
    bounce_at: u64,
    next_pipe: u64,
}

static mut SESSION: Session = Session {
    fs: Handle(0),
    stdin: Handle(0),
    stdout: Handle(0),
    fds: [Fd::FREE; MAX_FDS],
    stdin_carry: [0; 256],
    stdin_carry_at: 0,
    stdin_carry_len: 0,
    bounce: Handle(0),
    bounce_at: 0,
    next_pipe: 1,
};

fn session() -> &'static mut Session {
    // SAFETY: SH7-d3c-2: every caller holds the shim lock (threads::lock, taken by
    // persona_dispatch), and no reference is kept across a wait that releases it. Before the
    // program starts, only the main thread exists.
    unsafe { &mut *core::ptr::addr_of_mut!(SESSION) }
}

/// SH7-d3c-2: a reader parked on a descriptor's channel with the shim lock released (1), so a
/// second reader of the same descriptor waits its turn instead of racing it for the message.
static PIPE_READER: [AtomicU32; MAX_FDS] = [const { AtomicU32::new(0) }; MAX_FDS];
static STDIN_READER: AtomicU32 = AtomicU32::new(0);

/// Park until `channel` has a message, with the shim lock released (a sibling may hold the
/// other end), then take the lock back. `busy` keeps other readers of the same channel out
/// meanwhile. j919 (SH7-e2): the wait also watches the thread's alert channel, so a deliverable
/// signal ends it: `None`, which the caller answers with -ERESTARTSYS.
fn parked_read(busy: &AtomicU32, channel: Handle, out: &mut [u8]) -> Option<u64> {
    busy.store(1, Ordering::Release);
    let me = crate::signals::current();
    let n = threads::unlocked(|| loop {
        if let Some((n, _)) = channel_try_read_with_handle(channel, out.as_mut_ptr(), out.len()) {
            break Some(n as u64);
        }
        match me.map(|rec| crate::signals::wait_or_alert(rec, channel)) {
            Some(Some(true)) => break None,
            Some(Some(false)) => continue,
            // No record or no port: the plain blocking read, uninterruptible.
            _ => break Some(channel_read(channel, out.as_mut_ptr(), out.len())),
        }
    });
    busy.store(0, Ordering::Release);
    let _ = kumo_rt::futex_wake(busy.as_ptr(), u32::MAX);
    n
}

/// Take the launcher's grants. Handle 0 means "not granted".
pub fn init(fs: Handle, stdin: Handle, stdout: Handle) {
    let s = session();
    s.fs = fs;
    s.stdin = stdin;
    s.stdout = stdout;
    s.fds[0].kind = Kind::Stdin;
    s.fds[1].kind = Kind::Stdout;
    s.fds[2].kind = Kind::Stdout;
}

// ---- the vfs session -----------------------------------------------------------------------

/// One request/reply exchange with the filesystem. `None` when there is no grant or it is gone.
fn vfs_call<'r>(request: &Request<'_>, reply: &'r mut [u8]) -> Option<&'r [u8]> {
    let fs = session().fs;
    if fs.0 == 0 {
        return None;
    }
    let mut frame = [0u8; 16 + PATH_MAX * 2];
    let len = request.encode_into(&mut frame)?;
    if channel_write(fs, frame.as_ptr(), len) != 0 {
        return None;
    }
    let n = channel_read(fs, reply.as_mut_ptr(), reply.len());
    if n == 0 || n == u64::MAX {
        return None;
    }
    Some(&reply[..n as usize])
}

fn status_errno(error: VfsError) -> i64 {
    match error {
        VfsError::Status(VFS_NOT_FOUND) => -errno::ENOENT,
        VfsError::Status(VFS_NOT_DIR) => -errno::ENOTDIR,
        VfsError::Status(VFS_IS_DIR) => -errno::EISDIR,
        // SH7-e1: the mutation statuses.
        VfsError::Status(VFS_EXISTS) => -errno::EEXIST,
        VfsError::Status(VFS_NOT_EMPTY) => -errno::ENOTEMPTY,
        VfsError::Status(VFS_NO_SPACE) => -errno::ENOSPC,
        // A read-only session refuses every mutation this way.
        VfsError::Status(VFS_ACCESS_DENIED) => -errno::EROFS,
        VfsError::Status(VFS_NOT_SUPPORTED) => -errno::EPERM,
        _ => -errno::EIO,
    }
}

/// SH7-e1: a one-status-byte vfs reply (mkdir, unlink, rmdir, rename, flush, set-len, chmod).
fn vfs_status(request: &Request<'_>) -> i64 {
    let mut reply = [0u8; 8];
    match vfs_call(request, &mut reply) {
        None => -errno::EIO,
        Some([]) => -errno::EIO,
        Some(&[VFS_OK, ..]) => 0,
        Some(&[status, ..]) => status_errno(VfsError::Status(status)),
    }
}

/// SH7-d3a: read up to `len` bytes of vfs file `handle` from `offset` straight into `vmo` at
/// `vmo_offset`, [`READ_VMO_MAX`] per request. Each request carries a duplicate of `vmo`,
/// which fatfs maps for that request alone and closes. Returns how many bytes landed (short
/// only at end of file), or a negative errno.
pub fn vfs_read_vmo(handle: u32, offset: u64, vmo: Handle, vmo_offset: u64, len: u64) -> i64 {
    let fs = session().fs;
    if fs.0 == 0 {
        return -errno::EIO;
    }
    let mut done = 0u64;
    while done < len {
        let want = (len - done).min(READ_VMO_MAX as u64) as u32;
        let mut frame = [0u8; kumo_vfs::READ_VMO_REQUEST_LEN];
        let Some(n) = (Request::ReadVmo {
            handle,
            offset: offset + done,
            len: want,
            vmo_offset: vmo_offset + done,
        })
        .encode_into(&mut frame) else {
            return -errno::EINVAL;
        };
        // The rights VmoCreate gave. DUPLICATE stays (SH7-d3b): a block-backed fatfs hands a
        // copy to the block driver, which writes the sectors into it directly.
        let raw = handle_duplicate(
            vmo,
            Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::DUPLICATE.0 | Rights::TRANSFER.0),
        );
        if raw == 0 || raw == u64::MAX {
            read_vmo_trouble(b"duplicate", raw);
            return -errno::ENOMEM;
        }
        let carried = Handle(raw as u32);
        let status = channel_write_with_handle(fs, frame.as_ptr(), n, carried);
        if status != 0 {
            read_vmo_trouble(b"send", status as u64);
            let _ = handle_close(carried);
            return -errno::EIO;
        }
        let mut reply = [0u8; kumo_vfs::READ_VMO_REPLY_LEN];
        let got = channel_read(fs, reply.as_mut_ptr(), reply.len());
        if got == 0 || got == u64::MAX {
            read_vmo_trouble(b"reply", got);
            return -errno::EIO;
        }
        let count = match parse_read_vmo_reply(&reply[..got as usize]) {
            Ok(count) => count as u64,
            Err(error) => {
                read_vmo_trouble(b"status", reply[0] as u64);
                return status_errno(error);
            }
        };
        done += count;
        if count < want as u64 {
            break;
        }
    }
    done as i64
}

/// Say which step of a `ReadVmo` failed, with its status, once per step kind is plenty: the
/// caller's errno says the rest.
fn read_vmo_trouble(step: &[u8], value: u64) {
    let mut line = [0u8; 80];
    let head = b"persona-linux: ReadVmo ";
    let mut len = 0;
    for part in [head as &[u8], step, b" failed: 0x"] {
        line[len..len + part.len()].copy_from_slice(part);
        len += part.len();
    }
    for shift in (0..16).rev() {
        line[len] = b"0123456789abcdef"[((value >> (shift * 4)) & 0xf) as usize];
        len += 1;
    }
    line[len] = b'\n';
    let _ = log_write(kumo_abi::sys::log_level::NOTICE, &line[..len + 1]);
}

/// The bounce VMO, created and mapped on first use.
fn bounce() -> Option<(Handle, u64)> {
    let s = session();
    if s.bounce.0 == 0 {
        let raw = vmo_create(BOUNCE_BYTES);
        if raw == 0 || raw == u64::MAX {
            return None;
        }
        let vmo = Handle(raw as u32);
        match vmar_map_anywhere(vmo, BOUNCE_BYTES, 1 | 2) {
            (0, at) if at != 0 => {
                s.bounce = vmo;
                s.bounce_at = at;
            }
            _ => {
                let _ = handle_close(vmo);
                return None;
            }
        }
    }
    Some((s.bounce, s.bounce_at))
}

/// Read up to `out.len()` bytes of vfs file `handle` at `offset`. A small read is inline
/// replies; a large one (SH7-d3a) goes through the bounce VMO a megabyte at a time.
/// Returns how many bytes arrived (short only at end of file), or a negative errno.
pub fn vfs_pread(handle: u32, size: u64, offset: u64, out: &mut [u8]) -> i64 {
    if out.len() as u64 > READ_CHUNK {
        if let Some((vmo, at)) = bounce() {
            let mut done = 0usize;
            while done < out.len() && offset + (done as u64) < size {
                let want = ((out.len() - done) as u64).min(BOUNCE_BYTES);
                let got = vfs_read_vmo(handle, offset + done as u64, vmo, 0, want);
                if got < 0 {
                    return if done > 0 { done as i64 } else { got };
                }
                // SAFETY: the bounce mapping holds BOUNCE_BYTES >= got bytes.
                let landed = unsafe { core::slice::from_raw_parts(at as *const u8, got as usize) };
                out[done..done + got as usize].copy_from_slice(landed);
                done += got as usize;
                if (got as u64) < want {
                    break;
                }
            }
            return done as i64;
        }
    }
    let mut done = 0usize;
    let mut reply = [0u8; 1 + READ_CHUNK as usize];
    while done < out.len() && offset + (done as u64) < size {
        let want = (out.len() - done).min(READ_CHUNK as usize) as u32;
        let Some(raw) = vfs_call(
            &Request::read(handle, offset + done as u64, want),
            &mut reply,
        ) else {
            return -errno::EIO;
        };
        let data = match read_payload(raw) {
            Ok(data) => data,
            Err(error) => return status_errno(error),
        };
        if data.is_empty() {
            break;
        }
        out[done..done + data.len()].copy_from_slice(data);
        done += data.len();
    }
    done as i64
}

// ---- paths ---------------------------------------------------------------------------------

/// Read a NUL-terminated Linux path from the program's memory.
fn user_path(ptr: u64, out: &mut [u8; PATH_MAX]) -> Result<usize, i64> {
    if ptr == 0 {
        return Err(-errno::EFAULT);
    }
    for (i, slot) in out.iter_mut().enumerate() {
        // SAFETY: the program passed this pointer; a bad one faults the program, as a stray
        // pointer into its own memory would on Linux (EFAULT is the noted gap, SH7-d cut).
        let byte = unsafe { core::ptr::read_volatile((ptr + i as u64) as *const u8) };
        if byte == 0 {
            return Ok(i);
        }
        *slot = byte;
    }
    Err(-errno::ENAMETOOLONG)
}

/// Resolve `path` against `dirfd` (or the cwd `/` for `AT_FDCWD`) into an absolute, normalized
/// path: no empty, `.` or `..` components, `/` alone for the root.
fn resolve(dirfd: i64, path: &[u8], out: &mut [u8; PATH_MAX]) -> Result<usize, i64> {
    let mut joined = [0u8; PATH_MAX * 2 + 1];
    let mut len = 0usize;
    let mut push = |bytes: &[u8], len: &mut usize| -> Result<(), i64> {
        if *len + bytes.len() > joined.len() {
            return Err(-errno::ENAMETOOLONG);
        }
        joined[*len..*len + bytes.len()].copy_from_slice(bytes);
        *len += bytes.len();
        Ok(())
    };
    if path.first() != Some(&b'/') {
        if dirfd == AT_FDCWD {
            // SH7-e1: relative to the cwd (`chdir`), `/` until the program changes it.
            let cwd = unsafe { &*core::ptr::addr_of!(CWD) };
            push(&cwd.0[..cwd.1], &mut len)?;
        } else {
            let fd = fd_entry(dirfd)?;
            if !matches!(fd.kind, Kind::Dir { .. }) {
                return Err(-errno::ENOTDIR);
            }
            let base = fd.path;
            let base_len = fd.path_len;
            push(&base[..base_len], &mut len)?;
        }
        push(b"/", &mut len)?;
    }
    push(path, &mut len)?;

    let mut n = 0usize;
    for component in joined[..len].split(|&b| b == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                while n > 0 && out[n - 1] != b'/' {
                    n -= 1;
                }
                n = n.saturating_sub(1);
            }
            name => {
                if n + 1 + name.len() > PATH_MAX {
                    return Err(-errno::ENAMETOOLONG);
                }
                out[n] = b'/';
                out[n + 1..n + 1 + name.len()].copy_from_slice(name);
                n += 1 + name.len();
            }
        }
    }
    if n == 0 {
        out[0] = b'/';
        n = 1;
    }
    Ok(n)
}

// ---- descriptors ---------------------------------------------------------------------------

fn fd_entry(fd: i64) -> Result<&'static mut Fd, i64> {
    let s = session();
    if fd < 0 || fd as usize >= MAX_FDS || s.fds[fd as usize].kind == Kind::Free {
        return Err(-errno::EBADF);
    }
    Ok(&mut s.fds[fd as usize])
}

fn new_fd(kind: Kind, path: &[u8]) -> i64 {
    let s = session();
    let Some(index) = (3..MAX_FDS).find(|&i| s.fds[i].kind == Kind::Free) else {
        return -errno::EMFILE;
    };
    let fd = &mut s.fds[index];
    fd.kind = kind;
    fd.offset = 0;
    fd.path[..path.len()].copy_from_slice(path);
    fd.path_len = path.len();
    index as i64
}

/// The open file behind `fd`, for `mmap`: `(vfs handle, size)`.
pub fn file_of(fd: i64) -> Result<(u32, u64), i64> {
    match fd_entry(fd)?.kind {
        Kind::File { handle, size } => Ok((handle, size)),
        Kind::Dir { .. } | Kind::Pipe { .. } => Err(-errno::ENODEV),
        _ => Err(-errno::EACCES),
    }
}

// ---- the Linux calls -----------------------------------------------------------------------

/// A private channel pair supplies byte-stream data, blocking reads and peer-close EOF.
/// The FD table is behind the shim lock (SH7-d3c-2); a blocking read parks with it released.
/// Channel writes have no bounded-capacity backpressure.
/// — MILVUS
pub fn pipe2(pair: u64, flags: u64) -> i64 {
    if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return -errno::EINVAL;
    }
    if pair == 0 {
        return -errno::EFAULT;
    }
    let s = session();
    let mut slots = (3..MAX_FDS).filter(|&i| s.fds[i].kind == Kind::Free);
    let (Some(reader), Some(writer)) = (slots.next(), slots.next()) else {
        return -errno::EMFILE;
    };
    let (a, b) = channel_create_pair();
    if a == u64::MAX || b == u64::MAX {
        if a != u64::MAX {
            let _ = handle_close(Handle(a as u32));
        }
        if b != u64::MAX {
            let _ = handle_close(Handle(b as u32));
        }
        return -errno::ENOMEM;
    }
    let inode = s.next_pipe;
    s.next_pipe += 1;
    for (fd, channel, readable) in [(reader, a, true), (writer, b, false)] {
        s.fds[fd] = Fd {
            kind: Kind::Pipe {
                channel: channel as u32,
                readable,
                inode,
            },
            flags,
            ..Fd::FREE
        };
    }
    // As with the existing file surface, an invalid non-null user pointer faults the program.
    unsafe {
        (pair as *mut i32).write_unaligned(reader as i32);
        ((pair + 4) as *mut i32).write_unaligned(writer as i32);
    }
    0
}

pub fn openat(dirfd: i64, path_ptr: u64, flags: u64) -> i64 {
    let mut raw = [0u8; PATH_MAX];
    let mut path = [0u8; PATH_MAX];
    let len = match user_path(path_ptr, &mut raw).and_then(|n| resolve(dirfd, &raw[..n], &mut path))
    {
        Ok(len) => len,
        Err(e) => return e,
    };
    let path = &path[..len];
    let access = flags & O_ACCMODE;
    if access == O_ACCMODE {
        return -errno::EINVAL;
    }
    let writes = access != O_RDONLY || flags & (O_CREAT | O_TRUNC) != 0;
    if writes {
        return open_for_write(path, flags);
    }
    let mut reply = [0u8; 64];
    let Some(raw_reply) = vfs_call(&Request::open(path), &mut reply) else {
        return -errno::ENOENT;
    };
    match parse_open_reply(raw_reply) {
        Ok((handle, size)) => {
            if flags & O_DIRECTORY != 0 {
                close_vfs(handle);
                return -errno::ENOTDIR;
            }
            new_open(Kind::File { handle, size }, path, flags, handle)
        }
        Err(VfsError::Status(VFS_IS_DIR)) => new_open(Kind::Dir { next: 0 }, path, flags, u32::MAX),
        Err(error) => status_errno(error),
    }
}

/// A descriptor for a fresh open, recording its Linux flags; the vfs handle is released if the
/// table is full.
fn new_open(kind: Kind, path: &[u8], flags: u64, handle: u32) -> i64 {
    let fd = new_fd(kind, path);
    if fd < 0 {
        if handle != u32::MAX {
            close_vfs(handle);
        }
        return fd;
    }
    let entry = &mut session().fds[fd as usize];
    entry.flags = flags;
    entry.dirty = false;
    fd
}

/// SH7-e1: an open that may create, truncate or write. The existing entry is stat'ed first so
/// O_EXCL, O_DIRECTORY and "a directory opened for writing" answer as Linux does; the server
/// creates a missing file only with O_CREAT. O_TRUNC empties the file through SetLen.
fn open_for_write(path: &[u8], flags: u64) -> i64 {
    let access = flags & O_ACCMODE;
    let mut reply = [0u8; 32];
    let existing = vfs_call(&Request::stat(path), &mut reply).map(parse_stat_reply);
    match existing {
        Some(Ok(stat)) => {
            if flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
                return -errno::EEXIST;
            }
            if stat.kind == STAT_KIND_DIRECTORY {
                return -errno::EISDIR;
            }
        }
        Some(Err(VfsError::Status(VFS_NOT_FOUND))) => {
            if flags & O_CREAT == 0 {
                return -errno::ENOENT;
            }
        }
        Some(Err(error)) => return status_errno(error),
        None => return -errno::EIO,
    }
    if flags & O_DIRECTORY != 0 {
        return -errno::ENOTDIR;
    }
    let mut options = 0;
    if access != O_WRONLY {
        options |= OPEN_READ;
    }
    if access != O_RDONLY || flags & O_TRUNC != 0 {
        options |= OPEN_WRITE;
    }
    if flags & O_CREAT != 0 {
        options |= OPEN_CREATE;
    }
    let mut reply = [0u8; 64];
    let Some(raw_reply) = vfs_call(&Request::OpenOptions { path, options }, &mut reply) else {
        return -errno::EIO;
    };
    let (handle, mut size) = match parse_open_reply(raw_reply) {
        Ok(open) => open,
        Err(error) => return status_errno(error),
    };
    if flags & O_TRUNC != 0 && size != 0 {
        let status = vfs_status(&Request::SetLen { handle, len: 0 });
        if status < 0 {
            close_vfs(handle);
            return status;
        }
        size = 0;
    }
    new_open(Kind::File { handle, size }, path, flags, handle)
}

fn close_vfs(handle: u32) {
    let mut reply = [0u8; 8];
    let _ = vfs_call(&Request::Close { handle }, &mut reply);
}

pub fn close(fd: i64) -> i64 {
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    if let Kind::File { handle, .. } = entry.kind {
        if entry.dirty {
            let _ = vfs_status(&Request::Flush { handle });
        }
        close_vfs(handle);
    } else if let Kind::Pipe { channel, .. } = entry.kind {
        let _ = handle_close(Handle(channel));
    }
    *entry = Fd::FREE;
    0
}

pub fn read(fd: i64, buf: u64, len: u64) -> i64 {
    read_from(fd, buf, len, true)
}

/// `read`, or (`may_block` false) a read that answers EAGAIN instead of waiting on an empty
/// pipe: `readv` blocks for its first bytes only, as on Linux.
fn read_from(fd: i64, buf: u64, len: u64, may_block: bool) -> i64 {
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    if let Kind::Pipe {
        channel, readable, ..
    } = entry.kind
    {
        if !readable {
            return -errno::EBADF;
        }
        if len == 0 {
            return 0;
        }
        if buf == 0 {
            return -errno::EFAULT;
        }
        let block = may_block && entry.flags & O_NONBLOCK == 0;
        return match fill_pipe(fd as usize, channel, block) {
            Ok(true) => {
                let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
                session().fds[fd as usize].carry.take(out) as i64
            }
            Ok(false) => 0,
            Err(e) => e,
        };
    }
    if buf == 0 && len != 0 {
        return -errno::EFAULT;
    }
    // SAFETY: the program's buffer, as it named it.
    let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
    match entry.kind {
        Kind::File { .. } if entry.flags & O_ACCMODE == O_WRONLY => -errno::EBADF,
        Kind::File { handle, size } => {
            let n = vfs_pread(handle, size, entry.offset, out);
            if n > 0 {
                entry.offset += n as u64;
            }
            n
        }
        Kind::Stdin => read_stdin(out),
        Kind::Dir { .. } => -errno::EISDIR,
        _ => -errno::EBADF,
    }
}

/// Make the pipe read end at `index` hold bytes: `Ok(true)` when its tail has some, `Ok(false)`
/// at end of stream (the writer closed). With nothing queued, a non-blocking read is EAGAIN and
/// a blocking one parks with the shim lock released (SH7-d3c-2) — a sibling thread may be the
/// writer. A descriptor closed while its reader was parked reads EBADF.
fn fill_pipe(index: usize, channel: u32, block: bool) -> Result<bool, i64> {
    let busy = &PIPE_READER[index];
    loop {
        let entry = &mut session().fds[index];
        if !matches!(entry.kind, Kind::Pipe { channel: c, readable: true, .. } if c == channel) {
            return Err(-errno::EBADF);
        }
        if !entry.carry.is_empty() {
            return Ok(true);
        }
        if busy.load(Ordering::Acquire) != 0 {
            if !block {
                return Err(-errno::EAGAIN);
            }
            threads::wait_turn(busy);
            continue;
        }
        let storage = entry.carry.storage();
        let n = match channel_try_read_with_handle(
            Handle(channel),
            storage.as_mut_ptr(),
            storage.len(),
        ) {
            Some((n, _)) => n,
            None if !block => return Err(-errno::EAGAIN),
            None => {
                let mut parked = [0u8; PIPE_CHUNK];
                let Some(n) = parked_read(busy, Handle(channel), &mut parked) else {
                    return Err(-crate::signals::ERESTARTSYS);
                };
                let n = n as usize;
                let entry = &mut session().fds[index];
                if !matches!(entry.kind, Kind::Pipe { channel: c, .. } if c == channel) {
                    return Err(-errno::EBADF);
                }
                if n <= PIPE_CHUNK {
                    entry.carry.storage()[..n].copy_from_slice(&parked[..n]);
                }
                n
            }
        };
        if n as u32 as i32 == kumo_abi::Errno::PeerClosed.status() {
            return Ok(false);
        }
        if !session().fds[index].carry.received(n) {
            return Err(-errno::EIO);
        }
        return Ok(true);
    }
}

/// stdin is a channel of messages; a short `read` keeps the rest of a message for the next one.
/// End of input (no grant, or the writer closed) reads 0. Waiting for a line parks with the shim
/// lock released (SH7-d3c-2).
fn read_stdin(out: &mut [u8]) -> i64 {
    loop {
        let s = session();
        if s.stdin_carry_at < s.stdin_carry_len {
            break;
        }
        if s.stdin.0 == 0 {
            return 0;
        }
        if STDIN_READER.load(Ordering::Acquire) != 0 {
            threads::wait_turn(&STDIN_READER);
            continue;
        }
        let stdin = s.stdin;
        let mut parked = [0u8; 256];
        let Some(n) = parked_read(&STDIN_READER, stdin, &mut parked) else {
            return -crate::signals::ERESTARTSYS;
        };
        if n == 0 || n == u64::MAX || n as usize > parked.len() {
            return 0;
        }
        let s = session();
        s.stdin_carry[..n as usize].copy_from_slice(&parked[..n as usize]);
        s.stdin_carry_at = 0;
        s.stdin_carry_len = n as usize;
    }
    let s = session();
    let take = out.len().min(s.stdin_carry_len - s.stdin_carry_at);
    out[..take].copy_from_slice(&s.stdin_carry[s.stdin_carry_at..s.stdin_carry_at + take]);
    s.stdin_carry_at += take;
    take as i64
}

pub fn pread(fd: i64, buf: u64, len: u64, offset: u64) -> i64 {
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    if (offset as i64) < 0 {
        return -errno::EINVAL;
    }
    match entry.kind {
        Kind::File { .. } if entry.flags & O_ACCMODE == O_WRONLY => -errno::EBADF,
        Kind::File { handle, size } => {
            // SAFETY: the program's buffer, as it named it.
            let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
            vfs_pread(handle, size, offset, out)
        }
        Kind::Dir { .. } => -errno::EISDIR,
        _ => -errno::ESPIPE,
    }
}

/// `readv` (musl's stdio reads through it): each vector in turn, stopping at a short read.
pub fn readv(fd: i64, iov: u64, count: u64) -> i64 {
    let mut total = 0i64;
    for index in 0..count.min(1024) {
        let entry = iov + index * 16;
        // SAFETY: the program's iovec array.
        let (base, len) = unsafe {
            (
                core::ptr::read_unaligned(entry as *const u64),
                core::ptr::read_unaligned((entry + 8) as *const u64),
            )
        };
        if len == 0 {
            continue;
        }
        // SH7-d3c-2: only the first bytes may wait; after them an empty pipe ends the call.
        let n = read_from(fd, base, len, total == 0);
        if n < 0 {
            return if total > 0 { total } else { n };
        }
        total += n;
        if (n as u64) < len {
            break;
        }
    }
    total
}

pub fn lseek(fd: i64, offset: i64, whence: u64) -> i64 {
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    match entry.kind {
        Kind::File { size, .. } => {
            let base = match whence {
                SEEK_SET => 0,
                SEEK_CUR => entry.offset as i64,
                SEEK_END => size as i64,
                _ => return -errno::EINVAL,
            };
            let Some(at) = base.checked_add(offset).filter(|&at| at >= 0) else {
                return -errno::EINVAL;
            };
            entry.offset = at as u64;
            at
        }
        Kind::Dir { .. } if whence == SEEK_SET && offset == 0 => {
            entry.kind = Kind::Dir { next: 0 };
            0
        }
        Kind::Dir { .. } => -errno::EINVAL,
        _ => -errno::ESPIPE,
    }
}

pub fn fcntl(fd: i64, cmd: u64, arg: u64) -> i64 {
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    if let Kind::Pipe { readable, .. } = entry.kind {
        return match cmd {
            F_GETFD => i64::from(entry.flags & O_CLOEXEC != 0),
            F_SETFD => {
                entry.flags = (entry.flags & !O_CLOEXEC) | if arg & 1 != 0 { O_CLOEXEC } else { 0 };
                0
            }
            F_GETFL => (entry.flags & O_NONBLOCK) as i64 | i64::from(!readable),
            F_SETFL => {
                entry.flags = (entry.flags & !O_NONBLOCK) | (arg & O_NONBLOCK);
                0
            }
            _ => -errno::EINVAL,
        };
    }
    match cmd {
        // Close-on-exec has no meaning until the persona can exec (SH7-f); accepted and kept 0.
        F_GETFD | F_SETFD | F_SETFL => 0,
        F_GETFL => match entry.kind {
            Kind::Dir { .. } => O_DIRECTORY as i64,
            Kind::Stdout => 1, // O_WRONLY
            _ => 0,            // O_RDONLY
        },
        _ => -errno::EINVAL,
    }
}

/// A stable inode number for a path (FNV-1a), never 0. musl's loader tells libraries apart by
/// `(st_dev, st_ino)`, so each file needs its own.
fn inode(path: &[u8]) -> u64 {
    let hash = path.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    hash | 1
}

/// Fill a Linux aarch64 `struct stat` (asm-generic layout, 128 bytes) at `buf`.
fn put_stat(buf: u64, dev: u64, ino: u64, mode: u32, size: u64) -> i64 {
    if buf == 0 {
        return -errno::EFAULT;
    }
    let mut st = [0u8; 128];
    st[0..8].copy_from_slice(&dev.to_le_bytes());
    st[8..16].copy_from_slice(&ino.to_le_bytes());
    st[16..20].copy_from_slice(&mode.to_le_bytes());
    st[20..24].copy_from_slice(&1u32.to_le_bytes()); // st_nlink
    st[48..56].copy_from_slice(&size.to_le_bytes());
    st[56..60].copy_from_slice(&512u32.to_le_bytes()); // st_blksize
    st[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes()); // st_blocks
                                                                   // SAFETY: the program's stat buffer.
    unsafe { core::ptr::copy_nonoverlapping(st.as_ptr(), buf as *mut u8, st.len()) };
    0
}

pub fn fstat(fd: i64, buf: u64) -> i64 {
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    match entry.kind {
        Kind::File { size, .. } => put_stat(buf, 1, inode(entry.path()), S_IFREG | 0o755, size),
        Kind::Dir { .. } => put_stat(buf, 1, inode(entry.path()), S_IFDIR | 0o755, 0),
        Kind::Pipe { inode, .. } => put_stat(buf, 3, inode, S_IFIFO | 0o600, 0),
        _ => put_stat(buf, 2, fd as u64 + 1, S_IFCHR | 0o620, 0),
    }
}

pub fn newfstatat(dirfd: i64, path_ptr: u64, buf: u64, flags: u64) -> i64 {
    let mut raw = [0u8; PATH_MAX];
    let n = match user_path(path_ptr, &mut raw) {
        Ok(n) => n,
        Err(e) => return e,
    };
    if n == 0 {
        return if flags & AT_EMPTY_PATH != 0 {
            fstat(dirfd, buf)
        } else {
            -errno::ENOENT
        };
    }
    let mut path = [0u8; PATH_MAX];
    let len = match resolve(dirfd, &raw[..n], &mut path) {
        Ok(len) => len,
        Err(e) => return e,
    };
    let path = &path[..len];
    let mut reply = [0u8; 32];
    let Some(raw_reply) = vfs_call(&Request::stat(path), &mut reply) else {
        return -errno::ENOENT;
    };
    match parse_stat_reply(raw_reply) {
        Ok(stat) if stat.kind == STAT_KIND_DIRECTORY => {
            put_stat(buf, 1, inode(path), S_IFDIR | 0o755, 0)
        }
        Ok(stat) => put_stat(buf, 1, inode(path), S_IFREG | 0o755, stat.size),
        Err(error) => status_errno(error),
    }
}

/// SH7-d3d (j917): the exec path `/proc/self/exe` answers (musl resolves the main program's
/// `$ORIGIN` RPATH through it). Set once by `main`, before the program runs.
static mut EXEC_PATH: ([u8; PATH_MAX], usize) = ([0; PATH_MAX], 0);

pub fn set_exec_path(path: &[u8]) {
    let mut absolute = [0u8; PATH_MAX];
    let len = resolve(AT_FDCWD, path, &mut absolute).unwrap_or(0);
    let exec = unsafe { &mut *core::ptr::addr_of_mut!(EXEC_PATH) };
    exec.0[..len].copy_from_slice(&absolute[..len]);
    exec.1 = len;
}

/// `readlinkat`: `/proc/self/exe` is the one link there is. Any other path that exists is not a
/// link (EINVAL, which musl's `realpath` takes as "a real component"); one that does not is
/// ENOENT. The result is not NUL-terminated and truncates to `size`, as on Linux.
pub fn readlinkat(dirfd: i64, path_ptr: u64, buf: u64, size: u64) -> i64 {
    let mut raw = [0u8; PATH_MAX];
    let n = match user_path(path_ptr, &mut raw) {
        Ok(n) => n,
        Err(e) => return e,
    };
    if (size as i64) <= 0 {
        return -errno::EINVAL;
    }
    if &raw[..n] == b"/proc/self/exe" {
        let exec = unsafe { &*core::ptr::addr_of!(EXEC_PATH) };
        let len = exec.1.min(size as usize);
        // SAFETY: the program's buffer of `size` bytes.
        unsafe { core::ptr::copy_nonoverlapping(exec.0.as_ptr(), buf as *mut u8, len) };
        return len as i64;
    }
    if n == 0 {
        return -errno::ENOENT;
    }
    let mut path = [0u8; PATH_MAX];
    let len = match resolve(dirfd, &raw[..n], &mut path) {
        Ok(len) => len,
        Err(e) => return e,
    };
    let mut reply = [0u8; 32];
    let Some(raw_reply) = vfs_call(&Request::stat(&path[..len]), &mut reply) else {
        return -errno::ENOENT;
    };
    match parse_stat_reply(raw_reply) {
        Ok(_) => -errno::EINVAL,
        Err(error) => status_errno(error),
    }
}

const POLLIN: u16 = 0x1;
const POLLOUT: u16 = 0x4;
const POLLNVAL: u16 = 0x20;

/// `ppoll` without waiting (SH7-d3d): what each descriptor is ready for now. Rust's runtime
/// polls fds 0-2 with no events and a zero timeout at startup to learn which are open (it aborts
/// if `poll` is missing). A closed descriptor is POLLNVAL; a write end (stdout, a pipe's writer)
/// is POLLOUT; a file, a directory or a pipe reader with a held tail is POLLIN. A call that
/// would have to wait for something not yet ready is not served yet (-ENOSYS, logged).
pub fn ppoll(fds: u64, count: u64, timeout: u64) -> i64 {
    if count > MAX_FDS as u64 * 4 {
        return -errno::EINVAL;
    }
    let mut ready = 0i64;
    for i in 0..count {
        let at = fds + i * 8;
        // struct pollfd { int fd; short events; short revents; }
        let (fd, events) = unsafe {
            (
                core::ptr::read_unaligned(at as *const i32),
                core::ptr::read_unaligned((at + 4) as *const u16),
            )
        };
        let revents = if fd < 0 {
            0
        } else {
            match fd_entry(fd as i64) {
                Err(_) => POLLNVAL,
                Ok(entry) => {
                    let now = match entry.kind {
                        Kind::Stdout
                        | Kind::Pipe {
                            readable: false, ..
                        } => POLLOUT,
                        Kind::File { .. } | Kind::Dir { .. } => POLLIN | POLLOUT,
                        Kind::Pipe { readable: true, .. } if !entry.carry.is_empty() => POLLIN,
                        _ => 0,
                    };
                    now & events
                }
            }
        };
        unsafe { core::ptr::write_unaligned((at + 6) as *mut u16, revents) };
        if revents != 0 {
            ready += 1;
        }
    }
    let zero_timeout = timeout != 0
        && unsafe { *(timeout as *const u64) == 0 && *((timeout + 8) as *const u64) == 0 };
    if ready == 0 && !zero_timeout {
        log_once(
            &SAID_PPOLL,
            b"persona-linux: a waiting ppoll arrives in a later slice (-ENOSYS)\n",
        );
        return -errno::ENOSYS;
    }
    ready
}

/// `getdents64`: `.` and `..` first (fatfs does not list them), then the server's entries, as
/// `linux_dirent64` records: `d_ino u64, d_off i64, d_reclen u16, d_type u8, d_name[]`.
pub fn getdents64(fd: i64, buf: u64, count: u64) -> i64 {
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    let Kind::Dir { mut next } = entry.kind else {
        return -errno::ENOTDIR;
    };
    let dir_path = entry.path;
    let dir_len = entry.path_len;
    // SAFETY: the program's buffer, as it named it.
    let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, count as usize) };
    let mut used = 0usize;
    // Emit one record; false when it does not fit.
    let mut emit = |name: &[u8], kind: u8, index: u32, used: &mut usize| -> bool {
        let reclen = (19 + name.len() + 1).next_multiple_of(8);
        if *used + reclen > out.len() {
            return false;
        }
        let record = &mut out[*used..*used + reclen];
        record.fill(0);
        let mut full = [0u8; PATH_MAX + 1];
        let full_len = (dir_len + 1 + name.len()).min(full.len());
        full[..dir_len].copy_from_slice(&dir_path[..dir_len]);
        full[dir_len] = b'/';
        let tail = full_len - dir_len - 1;
        full[dir_len + 1..full_len].copy_from_slice(&name[..tail]);
        record[0..8].copy_from_slice(&inode(&full[..full_len]).to_le_bytes());
        record[8..16].copy_from_slice(&(u64::from(index) + 1).to_le_bytes());
        record[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        record[18] = kind;
        record[19..19 + name.len()].copy_from_slice(name);
        *used += reclen;
        true
    };
    while next < 2 {
        let name: &[u8] = if next == 0 { b"." } else { b".." };
        if !emit(name, DT_DIR, next, &mut used) {
            break;
        }
        next += 1;
    }
    let mut done = false;
    while next >= 2 && !done {
        let mut reply = [0u8; 1100];
        let request = Request::ReadDir {
            path: &dir_path[..dir_len],
            start: next - 2,
        };
        let Some(raw_reply) = vfs_call(&request, &mut reply) else {
            break;
        };
        let records = match parse_dir_reply(raw_reply) {
            Ok((0, _)) => break,
            Ok((_, records)) => records,
            Err(error) => return status_errno(error),
        };
        for record in records {
            if !emit(
                record.name,
                if record.is_dir { DT_DIR } else { DT_REG },
                next,
                &mut used,
            ) {
                done = true;
                break;
            }
            next += 1;
        }
    }
    entry.kind = Kind::Dir { next };
    if used == 0 && done {
        return -errno::EINVAL; // the caller's buffer cannot hold the next record
    }
    used as i64
}

// ---- output --------------------------------------------------------------------------------

/// Write program output: the launcher's stdout channel when granted, else the kernel log.
pub fn write_out(fd: i64, bytes: &[u8]) -> i64 {
    match fd_entry(fd).map(|entry| entry.kind) {
        Ok(Kind::Stdout) => {}
        Ok(Kind::Pipe {
            channel,
            readable: false,
            ..
        }) => {
            let mut written = 0;
            for chunk in bytes.chunks(PIPE_CHUNK) {
                let status = channel_write(Handle(channel), chunk.as_ptr(), chunk.len());
                if status != 0 {
                    return if written > 0 {
                        written as i64
                    } else if status == kumo_abi::Errno::PeerClosed.status() {
                        -errno::EPIPE
                    } else {
                        -errno::EIO
                    };
                }
                written += chunk.len();
            }
            return written as i64;
        }
        Ok(Kind::File { .. }) => return write_file(fd, bytes, None),
        Ok(Kind::Dir { .. }) => return -errno::EBADF,
        Ok(_) => return -errno::EBADF,
        Err(e) => return e,
    }
    let stdout = session().stdout;
    for chunk in bytes.chunks(256) {
        if stdout.0 != 0 {
            let _ = channel_write(stdout, chunk.as_ptr(), chunk.len());
        } else {
            let _ = log_write(kumo_abi::sys::log_level::INFO, chunk);
        }
    }
    bytes.len() as i64
}

static SAID_PPOLL: AtomicBool = AtomicBool::new(false);

/// Log `message` the first time `said` is seen unset (j917: one flag per message, atomic now
/// that Linux threads share the shim).
fn log_once(said: &AtomicBool, message: &[u8]) {
    if !said.swap(true, Ordering::Relaxed) {
        let _ = log_write(kumo_abi::sys::log_level::NOTICE, message);
    }
}

// ---- execve --------------------------------------------------------------------------------

/// Open the regular file at absolute `path` for the loader: `(vfs handle, size)`.
pub fn open_path(path: &[u8]) -> Result<(u32, u64), i64> {
    let mut normal = [0u8; PATH_MAX];
    let len = resolve(AT_FDCWD, path, &mut normal)?;
    let mut reply = [0u8; 64];
    let raw = vfs_call(&Request::open(&normal[..len]), &mut reply).ok_or(-errno::ENOENT)?;
    parse_open_reply(raw).map_err(status_errno)
}

/// Release a handle [`open_path`] returned.
pub fn close_handle(handle: u32) {
    close_vfs(handle);
}

// ---- SH7-e1 (j918): the write surface (cut W1) ----------------------------------------------

/// The working directory `resolve` joins relative paths to: absolute and normalized. Guarded by
/// the shim lock like the rest of the session.
static mut CWD: ([u8; PATH_MAX], usize) = ([0; PATH_MAX], 0);

/// Write `bytes` to the file behind `fd`: at `at` (pwrite), else at the descriptor's offset (or
/// the end, with O_APPEND), which then advances. MAX_WRITE_DATA (256) bytes per vfs request; a
/// short count from the server ends the call there, as a short write does on Linux.
fn write_file(fd: i64, bytes: &[u8], at: Option<u64>) -> i64 {
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    let Kind::File { handle, size } = entry.kind else {
        return -errno::EBADF;
    };
    if entry.flags & O_ACCMODE == O_RDONLY {
        return -errno::EBADF;
    }
    let mut offset = match at {
        Some(at) => at,
        None if entry.flags & O_APPEND != 0 => size,
        None => entry.offset,
    };
    if offset
        .checked_add(bytes.len() as u64)
        .is_none_or(|end| end > u32::MAX as u64)
    {
        // FAT files end at 4 GiB.
        return -errno::EFBIG;
    }
    let mut written = 0usize;
    for chunk in bytes.chunks(MAX_WRITE_DATA) {
        let mut reply = [0u8; 16];
        let request = Request::Write {
            handle,
            offset,
            data: chunk,
        };
        let n = match vfs_call(&request, &mut reply).map(parse_write_reply) {
            Some(Ok(n)) => n as usize,
            Some(Err(error)) if written == 0 => return status_errno(error),
            None if written == 0 => return -errno::EIO,
            _ => break,
        };
        written += n;
        offset += n as u64;
        if n < chunk.len() {
            break;
        }
    }
    let entry = &mut session().fds[fd as usize];
    if let Kind::File { size, .. } = &mut entry.kind {
        *size = (*size).max(offset);
    }
    if at.is_none() {
        entry.offset = offset;
    }
    entry.dirty |= written > 0;
    written as i64
}

/// Linux `pwrite64`.
pub fn pwrite(fd: i64, buf: u64, len: u64, offset: u64) -> i64 {
    if (offset as i64) < 0 {
        return -errno::EINVAL;
    }
    if !matches!(fd_entry(fd).map(|e| e.kind), Ok(Kind::File { .. })) {
        return if fd_entry(fd).is_err() {
            -errno::EBADF
        } else {
            -errno::ESPIPE
        };
    }
    if len == 0 {
        return 0;
    }
    // SAFETY: the program's buffer, as it named it.
    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };
    write_file(fd, bytes, Some(offset))
}

/// Linux `ftruncate`: the file's length through SetLen (zero-filled when it grows).
pub fn ftruncate(fd: i64, len: u64) -> i64 {
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    let Kind::File { handle, .. } = entry.kind else {
        return -errno::EINVAL;
    };
    if entry.flags & O_ACCMODE == O_RDONLY {
        return -errno::EINVAL;
    }
    if (len as i64) < 0 || len > u32::MAX as u64 {
        return -errno::EINVAL;
    }
    let status = vfs_status(&Request::SetLen { handle, len });
    if status == 0 {
        let entry = &mut session().fds[fd as usize];
        if let Kind::File { size, .. } = &mut entry.kind {
            *size = len;
        }
        entry.dirty = true;
    }
    status
}

/// Linux `fsync` / `fdatasync`: Flush for a file; nothing to do for the rest.
pub fn fsync(fd: i64) -> i64 {
    match fd_entry(fd) {
        Ok(entry) => match entry.kind {
            Kind::File { handle, .. } => {
                let status = vfs_status(&Request::Flush { handle });
                if status == 0 {
                    session().fds[fd as usize].dirty = false;
                }
                status
            }
            _ => 0,
        },
        Err(e) => e,
    }
}

/// A user path resolved against `dirfd` (or the cwd).
fn user_resolved(dirfd: i64, path_ptr: u64, out: &mut [u8; PATH_MAX]) -> Result<usize, i64> {
    let mut raw = [0u8; PATH_MAX];
    let n = user_path(path_ptr, &mut raw)?;
    if n == 0 {
        return Err(-errno::ENOENT);
    }
    resolve(dirfd, &raw[..n], out)
}

/// Linux `mkdirat` (the mode is not kept: FAT has none).
pub fn mkdirat(dirfd: i64, path_ptr: u64) -> i64 {
    let mut path = [0u8; PATH_MAX];
    match user_resolved(dirfd, path_ptr, &mut path) {
        Ok(len) => vfs_status(&Request::Mkdir { path: &path[..len] }),
        Err(e) => e,
    }
}

/// Linux `unlinkat`: a file, or with AT_REMOVEDIR an empty directory.
pub fn unlinkat(dirfd: i64, path_ptr: u64, flags: u64) -> i64 {
    if flags & !AT_REMOVEDIR != 0 {
        return -errno::EINVAL;
    }
    let mut path = [0u8; PATH_MAX];
    let len = match user_resolved(dirfd, path_ptr, &mut path) {
        Ok(len) => len,
        Err(e) => return e,
    };
    let path = &path[..len];
    if path == b"/" {
        return -errno::EPERM;
    }
    if flags & AT_REMOVEDIR != 0 {
        vfs_status(&Request::Rmdir { path })
    } else {
        // Linux unlinks no directory (EISDIR); the server's unlink is files-only too.
        let mut reply = [0u8; 32];
        if let Some(Ok(stat)) = vfs_call(&Request::stat(path), &mut reply).map(parse_stat_reply) {
            if stat.kind == STAT_KIND_DIRECTORY {
                return -errno::EISDIR;
            }
        }
        vfs_status(&Request::Unlink { path })
    }
}

/// Linux `renameat` / `renameat2` (flags 0 or RENAME_NOREPLACE): the server replaces an existing
/// file atomically.
pub fn renameat(from_dir: i64, from_ptr: u64, to_dir: i64, to_ptr: u64, flags: u64) -> i64 {
    if flags & !RENAME_NOREPLACE != 0 {
        return -errno::EINVAL;
    }
    let mut from = [0u8; PATH_MAX];
    let mut to = [0u8; PATH_MAX];
    let (from_len, to_len) = match (
        user_resolved(from_dir, from_ptr, &mut from),
        user_resolved(to_dir, to_ptr, &mut to),
    ) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    if flags & RENAME_NOREPLACE != 0 {
        let mut reply = [0u8; 32];
        if let Some(Ok(_)) =
            vfs_call(&Request::stat(&to[..to_len]), &mut reply).map(parse_stat_reply)
        {
            return -errno::EEXIST;
        }
    }
    vfs_status(&Request::Rename {
        from: &from[..from_len],
        to: &to[..to_len],
    })
}

/// Linux `fchmod`: the server records a synthetic mode (FAT stores none).
pub fn fchmod(fd: i64, mode: u64) -> i64 {
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    match entry.kind {
        Kind::File { .. } | Kind::Dir { .. } => {
            let mut path = [0u8; PATH_MAX];
            let len = entry.path_len;
            path[..len].copy_from_slice(entry.path());
            chmod_path(&path[..len], mode)
        }
        // Pipes and the standard streams: nothing to keep.
        _ => 0,
    }
}

/// Linux `fchmodat` (flags are not taken: there are no symlinks to not follow).
pub fn fchmodat(dirfd: i64, path_ptr: u64, mode: u64) -> i64 {
    let mut path = [0u8; PATH_MAX];
    match user_resolved(dirfd, path_ptr, &mut path) {
        Ok(len) => chmod_path(&path[..len], mode),
        Err(e) => e,
    }
}

fn chmod_path(path: &[u8], mode: u64) -> i64 {
    match vfs_status(&Request::Chmod {
        path,
        mode: (mode & 0o7777) as u16,
    }) {
        // A server that keeps no modes for this volume: the call still succeeds, as on vfat.
        status if status == -errno::EPERM => 0,
        status => status,
    }
}

/// Linux `faccessat`: whether the path exists; every existing entry is accessible.
pub fn faccessat(dirfd: i64, path_ptr: u64) -> i64 {
    let mut path = [0u8; PATH_MAX];
    let len = match user_resolved(dirfd, path_ptr, &mut path) {
        Ok(len) => len,
        Err(e) => return e,
    };
    let mut reply = [0u8; 32];
    match vfs_call(&Request::stat(&path[..len]), &mut reply).map(parse_stat_reply) {
        Some(Ok(_)) => 0,
        Some(Err(error)) => status_errno(error),
        None => -errno::ENOENT,
    }
}

/// The cwd as an absolute path.
fn cwd() -> &'static [u8] {
    let cwd = unsafe { &*core::ptr::addr_of!(CWD) };
    if cwd.1 == 0 {
        b"/"
    } else {
        &cwd.0[..cwd.1]
    }
}

/// Linux `getcwd`: the path and its NUL; ERANGE when it does not fit.
pub fn getcwd(buf: u64, size: u64) -> i64 {
    let path = cwd();
    if buf == 0 {
        return -errno::EFAULT;
    }
    if (size as usize) < path.len() + 1 {
        return -errno::ERANGE;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(path.as_ptr(), buf as *mut u8, path.len());
        *((buf + path.len() as u64) as *mut u8) = 0;
    }
    (path.len() + 1) as i64
}

/// Linux `chdir`: an existing directory becomes the cwd.
pub fn chdir(path_ptr: u64) -> i64 {
    let mut path = [0u8; PATH_MAX];
    let len = match user_resolved(AT_FDCWD, path_ptr, &mut path) {
        Ok(len) => len,
        Err(e) => return e,
    };
    set_cwd(&path[..len])
}

/// Linux `fchdir`: the directory a descriptor names becomes the cwd.
pub fn fchdir(fd: i64) -> i64 {
    let entry = match fd_entry(fd) {
        Ok(entry) => entry,
        Err(e) => return e,
    };
    if !matches!(entry.kind, Kind::Dir { .. }) {
        return -errno::ENOTDIR;
    }
    let mut path = [0u8; PATH_MAX];
    let len = entry.path_len;
    path[..len].copy_from_slice(entry.path());
    set_cwd(&path[..len])
}

fn set_cwd(path: &[u8]) -> i64 {
    let mut reply = [0u8; 32];
    match vfs_call(&Request::stat(path), &mut reply).map(parse_stat_reply) {
        Some(Ok(stat)) if stat.kind == STAT_KIND_DIRECTORY => {
            let cwd = unsafe { &mut *core::ptr::addr_of_mut!(CWD) };
            cwd.0[..path.len()].copy_from_slice(path);
            cwd.1 = path.len();
            0
        }
        Some(Ok(_)) => -errno::ENOTDIR,
        Some(Err(error)) => status_errno(error),
        None => -errno::ENOENT,
    }
}
