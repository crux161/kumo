//j918
//j919
//j921
//j924
//j925
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

use core::sync::atomic::{AtomicU32, Ordering};

use crate::{errno, threads};

/// What one vfs `Read` answers at most (the fatfs reply ceiling).
const READ_CHUNK: u64 = 256;
/// SH7-d3a: a read larger than one inline reply goes through this much private VMO at a time:
/// fatfs writes the file bytes into it (`ReadVmo`), and the shim copies them to the program.
const BOUNCE_BYTES: u64 = 1024 * 1024;
const PATH_MAX: usize = kumo_vfs::MAX_PATH;
const MAX_FDS: usize = 64;
const PIPE_CHUNK: usize = 4096;
const DEV_NULL: &[u8] = b"/dev/null";

const O_ACCMODE: u64 = 3;
const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_RDWR: u64 = 2;
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
const S_IFSOCK: u32 = 0o140000;
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
        /// j925: a Unix socketpair end, readable and writable both (one channel per end).
        duplex: bool,
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
    /// j924 (SH7-f): `/dev/null`, which `std::process::Command` opens for a child's stdin.
    /// Reads end at once; writes are taken whole and dropped.
    Null,
}

#[derive(Clone, Copy)]
struct OpenFile {
    refs: usize,
    kind: Kind,
    offset: u64,
    path: [u8; PATH_MAX],
    path_len: usize,
    flags: u64,
    carry: ReadTail<PIPE_CHUNK>,
    /// SH7-e1: a file written through this descriptor, flushed when it closes.
    dirty: bool,
}

impl OpenFile {
    const FREE: Self = Self {
        refs: 0,
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

#[derive(Clone, Copy)]
struct Descriptor {
    file: usize,
    cloexec: bool,
}

/// j924 (SH7-f): one Linux process's descriptor table and working directory. The program's own
/// is slot 0. A `vfork` child gets a copy in its own slot (processes.rs): `dup3` and `close`
/// in the child must not touch the parent's table, while open files stay shared, as on Linux.
#[derive(Clone, Copy)]
pub struct Bindings {
    fds: [Option<Descriptor>; MAX_FDS],
    cwd: ([u8; PATH_MAX], usize),
}

impl Bindings {
    pub const EMPTY: Self = Self {
        fds: [None; MAX_FDS],
        cwd: ([0; PATH_MAX], 0),
    };
}

/// The program's table and one per vfork child that has not yet exec'd or exited.
pub const MAX_BINDINGS: usize = 1 + crate::processes::MAX_VFORK;
static mut BINDINGS: [Bindings; MAX_BINDINGS] = [Bindings::EMPTY; MAX_BINDINGS];
/// The table the call being served uses: the caller's slot, set by `persona_dispatch` once it
/// holds the shim lock and restored whenever a wait takes the lock back (threads::unlocked).
static ACTIVE: AtomicU32 = AtomicU32::new(0);

pub fn set_active(slot: usize) {
    ACTIVE.store(slot as u32, Ordering::Relaxed);
}

pub fn active() -> usize {
    ACTIVE.load(Ordering::Relaxed) as usize
}

fn bindings() -> &'static mut Bindings {
    // SAFETY: as `session()`: the shim lock guards every table.
    unsafe { &mut (*core::ptr::addr_of_mut!(BINDINGS))[active()] }
}

/// Give vfork slot `slot` a copy of the active table: every open file gains a reference.
pub fn copy_bindings_to(slot: usize) {
    let copy = *bindings();
    for fd in copy.fds.iter().flatten() {
        session().files[fd.file].refs += 1;
    }
    unsafe { (*core::ptr::addr_of_mut!(BINDINGS))[slot] = copy };
}

/// Close every descriptor of vfork slot `slot` (its child exec'd, exited or never started).
pub fn release_bindings(slot: usize) {
    let table = unsafe { &mut (*core::ptr::addr_of_mut!(BINDINGS))[slot] };
    for entry in &mut table.fds {
        if let Some(fd) = entry.take() {
            release_file(fd.file);
        }
    }
    table.cwd.1 = 0;
}

struct Session {
    fs: Handle,
    /// j924: the mount-scoped server's session-control channel, when the launch gave one: each
    /// process this one starts opens its own session through it.
    fs_mount: Handle,
    stdin: Handle,
    stdout: Handle,
    files: [OpenFile; MAX_FDS],
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
    fs_mount: Handle(0),
    stdin: Handle(0),
    stdout: Handle(0),
    files: [OpenFile::FREE; MAX_FDS],
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
pub fn init(fs: Handle, fs_mount: Handle, stdin: Handle, stdout: Handle) {
    let s = session();
    s.fs = fs;
    s.fs_mount = fs_mount;
    s.stdin = stdin;
    s.stdout = stdout;
    new_fd(Kind::Stdin, &[], O_RDONLY).expect("stdin slot");
    new_fd(Kind::Stdout, &[], O_WRONLY).expect("stdout slot");
    new_fd(Kind::Stdout, &[], O_WRONLY).expect("stderr slot");
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
            let cwd = &bindings().cwd;
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

fn descriptor(fd: i64) -> Result<Descriptor, i64> {
    bindings()
        .fds
        .get(fd as usize)
        .copied()
        .flatten()
        .ok_or(-errno::EBADF)
}

fn fd_entry(fd: i64) -> Result<&'static mut OpenFile, i64> {
    let index = descriptor(fd)?.file;
    Ok(&mut session().files[index])
}

fn free_fd(min: usize) -> Result<usize, i64> {
    (min..MAX_FDS)
        .find(|&i| bindings().fds[i].is_none())
        .ok_or(-errno::EMFILE)
}

fn new_fd(kind: Kind, path: &[u8], flags: u64) -> Result<usize, i64> {
    let fd = free_fd(0)?;
    let s = session();
    let file = s
        .files
        .iter()
        .position(|entry| entry.refs == 0)
        .ok_or(-errno::EMFILE)?;
    let entry = &mut s.files[file];
    entry.refs = 1;
    entry.kind = kind;
    entry.flags = flags & !O_CLOEXEC;
    entry.path[..path.len()].copy_from_slice(path);
    entry.path_len = path.len();
    bindings().fds[fd] = Some(Descriptor {
        file,
        cloexec: flags & O_CLOEXEC != 0,
    });
    Ok(fd)
}

pub fn dup(fd: i64, min: u64, cloexec: bool) -> i64 {
    let src = match descriptor(fd) {
        Ok(src) => src,
        Err(e) => return e,
    };
    if min >= MAX_FDS as u64 {
        return -errno::EINVAL;
    }
    match free_fd(min as usize) {
        Ok(target) => duplicate_to(src, target, cloexec),
        Err(e) => e,
    }
}

pub fn dup3(fd: i64, target: i64, flags: u64) -> i64 {
    if flags & !O_CLOEXEC != 0 || fd == target {
        return -errno::EINVAL;
    }
    if !(0..MAX_FDS as i64).contains(&target) {
        return -errno::EBADF;
    }
    match descriptor(fd) {
        Ok(src) => duplicate_to(src, target as usize, flags & O_CLOEXEC != 0),
        Err(e) => e,
    }
}

fn duplicate_to(src: Descriptor, target: usize, cloexec: bool) -> i64 {
    // The shim lock covers replacement. Hold the source across closing an existing alias.
    session().files[src.file].refs += 1;
    if let Some(old) = bindings().fds[target].replace(Descriptor {
        file: src.file,
        cloexec,
    }) {
        release_file(old.file);
    }
    target as i64
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
    let inode = session().next_pipe;
    session().next_pipe += 1;
    let end = |channel: u64, readable: bool| Kind::Pipe {
        channel: channel as u32,
        readable,
        inode,
        duplex: false,
    };
    let reader = match new_fd(end(a, true), &[], flags) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = handle_close(Handle(a as u32));
            let _ = handle_close(Handle(b as u32));
            return e;
        }
    };
    let writer = match new_fd(end(b, false), &[], flags | O_WRONLY) {
        Ok(fd) => fd,
        Err(e) => {
            close(reader as i64);
            let _ = handle_close(Handle(b as u32));
            return e;
        }
    };
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
    if path == DEV_NULL {
        return match new_fd(Kind::Null, path, flags) {
            Ok(fd) => fd as i64,
            Err(e) => e,
        };
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
    match new_fd(kind, path, flags) {
        Ok(fd) => fd as i64,
        Err(e) => {
            if handle != u32::MAX {
                close_vfs(handle);
            }
            e
        }
    }
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
    let Some(open) = bindings().fds.get_mut(fd as usize).and_then(Option::take) else {
        return -errno::EBADF;
    };
    release_file(open.file);
    0
}

fn release_file(index: usize) {
    let entry = &mut session().files[index];
    entry.refs -= 1;
    if entry.refs != 0 {
        return;
    }
    let kind = entry.kind;
    let dirty = entry.dirty;
    *entry = OpenFile::FREE;
    if let Kind::File { handle, .. } = kind {
        if dirty {
            let _ = vfs_status(&Request::Flush { handle });
        }
        close_vfs(handle);
    } else if let Kind::Pipe { channel, .. } = kind {
        let _ = handle_close(Handle(channel));
    }
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
        // A Linux read keeps the open file alive if another thread closes or replaces its fd.
        let index = descriptor(fd).unwrap().file;
        session().files[index].refs += 1;
        let result = match fill_pipe(index, channel, block) {
            Ok(true) => {
                let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
                session().files[index].carry.take(out) as i64
            }
            Ok(false) => 0,
            Err(e) => e,
        };
        release_file(index);
        return result;
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
        Kind::Null => 0,
        _ => -errno::EBADF,
    }
}

/// Make the pipe read end at `index` hold bytes: `Ok(true)` when its tail has some, `Ok(false)`
/// at end of stream (the writer closed). With nothing queued, a non-blocking read is EAGAIN and
/// a blocking one parks with the shim lock released so a sibling may write or close its end.
fn fill_pipe(index: usize, channel: u32, block: bool) -> Result<bool, i64> {
    let busy = &PIPE_READER[index];
    loop {
        let entry = &mut session().files[index];
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
                let entry = &mut session().files[index];
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
        if !session().files[index].carry.received(n) {
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
    const F_GETLK: u64 = 5;
    const F_SETLK: u64 = 6;
    const F_SETLKW: u64 = 7;
    const F_OFD_GETLK: u64 = 36;
    const F_OFD_SETLK: u64 = 37;
    const F_OFD_SETLKW: u64 = 38;
    const F_UNLCK: i16 = 2;
    let d = match descriptor(fd) {
        Ok(d) => d,
        Err(e) => return e,
    };
    match cmd {
        0 | 1030 => dup(fd, arg, cmd == 1030), // F_DUPFD, F_DUPFD_CLOEXEC
        F_GETFD => i64::from(d.cloexec),
        F_SETFD => {
            bindings().fds[fd as usize] = Some(Descriptor {
                cloexec: arg & 1 != 0,
                ..d
            });
            0
        }
        // j925 (SH7-g): record locks (SQLite's, in Cargo's cache tracker) are granted, as
        // `flock` is: nobody else holds the image. F_GETLK reports no conflict (F_UNLCK).
        F_GETLK | F_OFD_GETLK => {
            if arg == 0 {
                return -errno::EFAULT;
            }
            // SAFETY: the program's `struct flock`, whose first field is the short l_type.
            unsafe { core::ptr::write_unaligned(arg as *mut i16, F_UNLCK) };
            0
        }
        F_SETLK | F_SETLKW | F_OFD_SETLK | F_OFD_SETLKW => 0,
        F_GETFL => (session().files[d.file].flags & !(O_CREAT | O_EXCL | O_TRUNC)) as i64,
        F_SETFL => {
            let flags = &mut session().files[d.file].flags;
            let mutable = O_NONBLOCK | O_APPEND;
            *flags = (*flags & !mutable) | (arg & mutable);
            0
        }
        _ => -errno::EINVAL,
    }
}

/// j924 (SH7-f): `ioctl` for the descriptor requests Rust's std makes. `FIONBIO` sets or clears
/// O_NONBLOCK (std's `read2` makes a child's stdout and stderr pipes non-blocking before it polls
/// them; rustc's linker call unwraps that); `FIOCLEX`/`FIONCLEX` set close-on-exec. Terminal
/// requests stay ENOTTY: no descriptor here is a terminal.
pub fn ioctl(fd: i64, request: u64, arg: u64) -> i64 {
    const FIONBIO: u64 = 0x5421;
    const FIONCLEX: u64 = 0x5450;
    const FIOCLEX: u64 = 0x5451;
    let d = match descriptor(fd) {
        Ok(d) => d,
        Err(e) => return e,
    };
    match request {
        FIONBIO => {
            if arg == 0 {
                return -errno::EFAULT;
            }
            // SAFETY: the program's int.
            let on = unsafe { core::ptr::read_unaligned(arg as *const i32) } != 0;
            let flags = &mut session().files[d.file].flags;
            *flags = if on {
                *flags | O_NONBLOCK
            } else {
                *flags & !O_NONBLOCK
            };
            0
        }
        FIOCLEX | FIONCLEX => {
            bindings().fds[fd as usize] = Some(Descriptor {
                cloexec: request == FIOCLEX,
                ..d
            });
            0
        }
        _ => -errno::ENOTTY,
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
        Kind::Pipe {
            inode,
            duplex: true,
            ..
        } => put_stat(buf, 3, inode, S_IFSOCK | 0o600, 0),
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
/// is POLLOUT; a file, a directory or a pipe reader with a held tail is POLLIN.
///
/// j924 (SH7-f): `ppoll` waits. `Command::output()` (rustc's linker call) polls the child's
/// stdout and stderr pipes together until both reach end of stream. A pipe reader takes what
/// its channel holds into its tail without blocking: bytes are POLLIN, a closed writer is
/// POLLHUP. Nothing ready and time left: sleep on a kernel timer with the shim lock released
/// and look again (a sibling or another process may write meanwhile). Never a spin.
pub fn ppoll(fds: u64, count: u64, timeout: u64) -> i64 {
    if count > MAX_FDS as u64 * 4 {
        return -errno::EINVAL;
    }
    let deadline = if timeout == 0 {
        None
    } else {
        let (sec, nsec) = unsafe {
            (
                core::ptr::read_unaligned(timeout as *const u64),
                core::ptr::read_unaligned((timeout + 8) as *const u64),
            )
        };
        Some(
            kumo_rt::clock_get()
                .saturating_add(sec.saturating_mul(1_000_000_000))
                .saturating_add(nsec),
        )
    };
    loop {
        let ready = poll_once(fds, count);
        if ready != 0 {
            return ready;
        }
        if deadline.is_some_and(|at| kumo_rt::clock_get() >= at) {
            return 0;
        }
        threads::unlocked(|| kumo_rt::sleep_ns(PPOLL_NAP_NS));
    }
}

const PPOLL_NAP_NS: u64 = 5_000_000;
const POLLHUP: u16 = 0x10;

/// One look at every descriptor `ppoll` names; the count of those with events.
fn poll_once(fds: u64, count: u64) -> i64 {
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
            match descriptor(fd as i64) {
                Err(_) => POLLNVAL,
                Ok(descriptor) => {
                    let index = descriptor.file;
                    let entry = &session().files[index];
                    let now = match entry.kind {
                        Kind::Stdout
                        | Kind::Pipe {
                            readable: false, ..
                        } => POLLOUT,
                        Kind::File { .. } | Kind::Dir { .. } | Kind::Null => POLLIN | POLLOUT,
                        Kind::Pipe { readable: true, .. } if !entry.carry.is_empty() => POLLIN,
                        Kind::Pipe {
                            readable: true,
                            channel,
                            ..
                        } if PIPE_READER[index].load(Ordering::Acquire) == 0 => {
                            match fill_pipe(index, channel, false) {
                                Ok(true) => POLLIN,
                                Ok(false) => POLLHUP,
                                Err(_) => 0,
                            }
                        }
                        _ => 0,
                    };
                    // j925: a socket end can always be written to as well.
                    let now = if matches!(entry.kind, Kind::Pipe { duplex: true, .. }) {
                        now | POLLOUT
                    } else {
                        now
                    };
                    // POLLHUP is reported whether asked for or not, as on Linux.
                    now & (events | POLLHUP)
                }
            }
        };
        unsafe { core::ptr::write_unaligned((at + 6) as *mut u16, revents) };
        if revents != 0 {
            ready += 1;
        }
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
            readable,
            duplex,
            ..
        }) if !readable || duplex => {
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
        Ok(Kind::Null) => return bytes.len() as i64,
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
    let entry = fd_entry(fd).expect("open file under shim lock");
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
        let entry = fd_entry(fd).expect("open file under shim lock");
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
                    entry.dirty = false;
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
    let cwd = &bindings().cwd;
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
            let cwd = &mut bindings().cwd;
            cwd.0[..path.len()].copy_from_slice(path);
            cwd.1 = path.len();
            0
        }
        Some(Ok(_)) => -errno::ENOTDIR,
        Some(Err(error)) => status_errno(error),
        None => -errno::ENOENT,
    }
}

// ---- j924 (SH7-f): descriptors across execve into a new process -----------------------------

/// What a file description becomes in the process an `execve` starts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Carried {
    Stdin,
    Stdout,
    Null,
    /// A pipe end: its channel arrives as the next `'p'` handle, in plan order.
    Pipe {
        readable: bool,
        inode: u64,
        duplex: bool,
    },
    /// A file or directory, opened again by path on the new process's own session.
    Path {
        directory: bool,
    },
}

#[derive(Clone, Copy)]
struct CarriedFile {
    what: Carried,
    flags: u64,
    offset: u64,
    path: [u8; PATH_MAX],
    path_len: usize,
}

/// The descriptors an `execve` keeps (every one without close-on-exec) and the descriptions
/// behind them. The parent fills it with the shim lock held, right before the snapshot that
/// copies it into the child; the child reads its copy before anything else.
struct SpawnPlan {
    /// Each kept descriptor's plan entry and its close-on-exec flag.
    fds: [Option<(u8, bool)>; MAX_FDS],
    files: [CarriedFile; MAX_FDS],
    count: usize,
    cwd: ([u8; PATH_MAX], usize),
}

static mut SPAWN_PLAN: SpawnPlan = SpawnPlan {
    fds: [None; MAX_FDS],
    files: [CarriedFile {
        what: Carried::Null,
        flags: 0,
        offset: 0,
        path: [0; PATH_MAX],
        path_len: 0,
    }; MAX_FDS],
    count: 0,
    cwd: ([0; PATH_MAX], 0),
};

fn spawn_plan() -> &'static mut SpawnPlan {
    // SAFETY: the parent writes it under the shim lock; the child reads its private copy
    // before it has a second thread.
    unsafe { &mut *core::ptr::addr_of_mut!(SPAWN_PLAN) }
}

/// The capabilities a new process gets with its plan, as `(tag, handle)` pairs in send order.
pub struct SpawnHandles {
    pub list: [(u8, Handle); MAX_FDS + 4],
    pub len: usize,
}

impl SpawnHandles {
    pub const fn new() -> Self {
        Self {
            list: [(0, Handle(0)); MAX_FDS + 4],
            len: 0,
        }
    }

    fn push(&mut self, tag: u8, handle: Handle) {
        self.list[self.len] = (tag, handle);
        self.len += 1;
    }

    /// Close every handle not sent (a failed spawn).
    pub fn close_all(&mut self) {
        for &(_, handle) in &self.list[..self.len] {
            let _ = handle_close(handle);
        }
        self.len = 0;
    }
}

fn transferable(handle: Handle) -> Option<Handle> {
    let rights =
        Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::TRANSFER.0 | Rights::DUPLICATE.0);
    let raw = handle_duplicate(handle, rights);
    (raw != 0 && raw != u64::MAX).then_some(Handle(raw as u32))
}

/// Describe the active table's inheritable descriptors in the spawn plan and collect the
/// handles the new process needs: a fresh session on the same volume (`'f'`), the mount's
/// control channel (`'m'`), the console ends its descriptors use (`'i'`, `'o'`) and one
/// channel per carried pipe end (`'p'`). Shim lock held. ENOSYS without a mount to open
/// sessions on (a `/media` launch); the caller logs it.
pub fn plan_spawn(out: &mut SpawnHandles) -> Result<(), i64> {
    plan(out, false)
}

/// j925: [`plan_spawn`] for `fork`: every descriptor goes, close-on-exec ones with their flag.
pub fn plan_fork(out: &mut SpawnHandles) -> Result<(), i64> {
    plan(out, true)
}

fn plan(out: &mut SpawnHandles, fork: bool) -> Result<(), i64> {
    let s = session();
    if s.fs_mount.0 == 0 {
        return Err(-errno::ENOSYS);
    }
    let session_handle = open_session().ok_or(-errno::EAGAIN)?;
    out.push(b'f', session_handle);
    let Some(control) = transferable(s.fs_mount) else {
        out.close_all();
        return Err(-errno::EAGAIN);
    };
    out.push(b'm', control);
    let table = *bindings();
    let p = spawn_plan();
    p.fds = [None; MAX_FDS];
    p.count = 0;
    p.cwd = table.cwd;
    // Which plan entry each open-file index became, so aliases stay one description.
    let mut planned = [u8::MAX; MAX_FDS];
    let (mut sent_stdin, mut sent_stdout) = (false, false);
    for (fd, entry) in table.fds.iter().enumerate() {
        let Some(descriptor) = entry else {
            continue;
        };
        if descriptor.cloexec && !fork {
            continue;
        }
        if planned[descriptor.file] == u8::MAX {
            let open = &s.files[descriptor.file];
            let what = match open.kind {
                Kind::Free => continue,
                Kind::Stdin => {
                    if !sent_stdin && s.stdin.0 != 0 {
                        match transferable(s.stdin) {
                            Some(handle) => out.push(b'i', handle),
                            None => {
                                out.close_all();
                                return Err(-errno::EAGAIN);
                            }
                        }
                        sent_stdin = true;
                    }
                    Carried::Stdin
                }
                Kind::Stdout => {
                    if !sent_stdout && s.stdout.0 != 0 {
                        match transferable(s.stdout) {
                            Some(handle) => out.push(b'o', handle),
                            None => {
                                out.close_all();
                                return Err(-errno::EAGAIN);
                            }
                        }
                        sent_stdout = true;
                    }
                    Carried::Stdout
                }
                Kind::Null => Carried::Null,
                Kind::Pipe {
                    channel,
                    readable,
                    inode,
                    duplex,
                } => {
                    match transferable(Handle(channel)) {
                        Some(handle) => out.push(b'p', handle),
                        None => {
                            out.close_all();
                            return Err(-errno::EAGAIN);
                        }
                    }
                    Carried::Pipe {
                        readable,
                        inode,
                        duplex,
                    }
                }
                Kind::File { .. } => Carried::Path { directory: false },
                Kind::Dir { .. } => Carried::Path { directory: true },
            };
            let at = p.count;
            p.files[at] = CarriedFile {
                what,
                flags: open.flags,
                offset: open.offset,
                path: open.path,
                path_len: open.path_len,
            };
            planned[descriptor.file] = at as u8;
            p.count += 1;
        }
        p.fds[fd] = Some((planned[descriptor.file], descriptor.cloexec && fork));
    }
    Ok(())
}

/// A new client session on the mount the launch granted: the server end goes over the control
/// channel with read-write access (the image grant is read-write, cut W1).
fn open_session() -> Option<Handle> {
    let (client, server) = kumo_rt::channel_create_pair();
    if client == u64::MAX || server == u64::MAX {
        return None;
    }
    let grant = [kumo_vfs::SESSION_GRANT, kumo_vfs::SESSION_ACCESS_READ_WRITE];
    if channel_write_with_handle(
        session().fs_mount,
        grant.as_ptr(),
        grant.len(),
        Handle(server as u32),
    ) != 0
    {
        let _ = handle_close(Handle(server as u32));
        let _ = handle_close(Handle(client as u32));
        return None;
    }
    Some(Handle(client as u32))
}

/// In the new process, before the program: forget the parent's tables (their handle numbers
/// mean nothing here) and rebuild descriptors from the plan with the handles that came with it.
/// `pipes` are the `'p'` handles in the order they arrived. A file that cannot be opened again
/// leaves its descriptors closed.
pub fn adopt_spawn(fs: Handle, fs_mount: Handle, stdin: Handle, stdout: Handle, pipes: &[Handle]) {
    let s = session();
    s.fs = fs;
    s.fs_mount = fs_mount;
    s.stdin = stdin;
    s.stdout = stdout;
    s.files = [OpenFile::FREE; MAX_FDS];
    s.stdin_carry_at = 0;
    s.stdin_carry_len = 0;
    s.bounce = Handle(0);
    s.bounce_at = 0;
    for busy in PIPE_READER.iter().chain(core::iter::once(&STDIN_READER)) {
        busy.store(0, Ordering::Relaxed);
    }
    unsafe { *core::ptr::addr_of_mut!(BINDINGS) = [Bindings::EMPTY; MAX_BINDINGS] };
    set_active(0);
    let p = spawn_plan();
    bindings().cwd = p.cwd;
    let mut pipe_at = 0usize;
    // Plan entry -> open-file index here, or None when it could not be carried.
    let mut placed = [None; MAX_FDS];
    for (index, carried) in p.files[..p.count].iter().enumerate() {
        let path = &carried.path[..carried.path_len];
        let kind = match carried.what {
            Carried::Stdin => Kind::Stdin,
            Carried::Stdout => Kind::Stdout,
            Carried::Null => Kind::Null,
            Carried::Pipe {
                readable,
                inode,
                duplex,
            } => {
                let Some(&channel) = pipes.get(pipe_at) else {
                    continue;
                };
                pipe_at += 1;
                Kind::Pipe {
                    channel: channel.0,
                    readable,
                    inode,
                    duplex,
                }
            }
            Carried::Path { directory: true } => Kind::Dir { next: 0 },
            Carried::Path { directory: false } => {
                let mut options = 0;
                let access = carried.flags & O_ACCMODE;
                if access != O_WRONLY {
                    options |= OPEN_READ;
                }
                if access != O_RDONLY {
                    options |= OPEN_WRITE;
                }
                let mut reply = [0u8; 64];
                match vfs_call(&Request::OpenOptions { path, options }, &mut reply)
                    .map(parse_open_reply)
                {
                    Some(Ok((handle, size))) => Kind::File { handle, size },
                    _ => continue,
                }
            }
        };
        let Some(slot) = s.files.iter().position(|entry| entry.refs == 0) else {
            break;
        };
        let entry = &mut s.files[slot];
        *entry = OpenFile::FREE;
        entry.kind = kind;
        entry.flags = carried.flags;
        entry.offset = carried.offset;
        entry.path[..path.len()].copy_from_slice(path);
        entry.path_len = path.len();
        // Its descriptors' count now, so the next description takes another slot.
        entry.refs = p
            .fds
            .iter()
            .filter(|at| at.is_some_and(|(file, _)| file == index as u8))
            .count();
        placed[index] = Some(slot);
    }
    for (fd, planned) in p.fds.iter().enumerate() {
        let Some((at, cloexec)) = *planned else {
            continue;
        };
        let Some(slot) = placed[at as usize] else {
            continue;
        };
        bindings().fds[fd] = Some(Descriptor {
            file: slot,
            cloexec,
        });
    }
}

/// j925: the first program's cwd, from the launcher's `PWD` (an absolute path; `/` otherwise).
pub fn set_start_cwd(dir: &[u8]) {
    if dir.first() == Some(&b'/') && dir.len() < PATH_MAX {
        let cwd = &mut bindings().cwd;
        cwd.0[..dir.len()].copy_from_slice(dir);
        cwd.1 = dir.len();
    }
}

/// j924: a program-supplied path, resolved against the active table's cwd (execve's path).
pub fn resolve_user(path_ptr: u64, out: &mut [u8; PATH_MAX]) -> Result<usize, i64> {
    user_resolved(AT_FDCWD, path_ptr, out)
}

/// j925: `execve` in this process closes every close-on-exec descriptor. Lock held.
pub fn close_on_exec() {
    for fd in 0..MAX_FDS {
        if bindings().fds[fd].is_some_and(|d| d.cloexec) {
            let _ = close(fd as i64);
        }
    }
}

/// j925: `execve` in this process unmapped the bounce VMO's mapping: forget it.
pub fn forget_bounce() {
    let s = session();
    if s.bounce.0 != 0 {
        let _ = handle_close(s.bounce);
    }
    s.bounce = Handle(0);
    s.bounce_at = 0;
}

/// j925 (SH7-g): `socketpair(AF_UNIX, SOCK_STREAM | SOCK_SEQPACKET | SOCK_DGRAM, 0, sv)`: one
/// channel pair, each end a descriptor that reads and writes (Rust's fork path reports an
/// `execve` failure through one; Cargo's job server uses pipes). Message boundaries are the
/// channel's, which is what SEQPACKET and DGRAM promise and STREAM does not mind.
pub fn socketpair(domain: u64, kind: u64, protocol: u64, pair: u64) -> i64 {
    const AF_UNIX: u64 = 1;
    const SOCK_NONBLOCK: u64 = O_NONBLOCK;
    const SOCK_CLOEXEC: u64 = O_CLOEXEC;
    if domain != AF_UNIX {
        return -errno::EAFNOSUPPORT;
    }
    if protocol != 0 || !matches!(kind & 0xf, 1 | 2 | 5) {
        return -errno::EPROTONOSUPPORT;
    }
    if kind & !(0xf | SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return -errno::EINVAL;
    }
    if pair == 0 {
        return -errno::EFAULT;
    }
    let flags = O_RDWR | (kind & (SOCK_NONBLOCK | SOCK_CLOEXEC));
    let (a, b) = channel_create_pair();
    if a == u64::MAX || b == u64::MAX {
        for raw in [a, b] {
            if raw != u64::MAX {
                let _ = handle_close(Handle(raw as u32));
            }
        }
        return -errno::ENOMEM;
    }
    let inode = session().next_pipe;
    session().next_pipe += 1;
    let end = |channel: u64| Kind::Pipe {
        channel: channel as u32,
        readable: true,
        inode,
        duplex: true,
    };
    let first = match new_fd(end(a), &[], flags) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = handle_close(Handle(a as u32));
            let _ = handle_close(Handle(b as u32));
            return e;
        }
    };
    let second = match new_fd(end(b), &[], flags) {
        Ok(fd) => fd,
        Err(e) => {
            close(first as i64);
            let _ = handle_close(Handle(b as u32));
            return e;
        }
    };
    unsafe {
        (pair as *mut i32).write_unaligned(first as i32);
        ((pair + 4) as *mut i32).write_unaligned(second as i32);
    }
    0
}

/// j925: `recvfrom(fd, buf, len, flags, addr, addrlen)` on a socket end: a read (MSG_DONTWAIT
/// does not wait). A socketpair peer has no address to report.
pub fn recvfrom(fd: i64, buf: u64, len: u64, flags: u64, addr_len: u64) -> i64 {
    const MSG_DONTWAIT: u64 = 0x40;
    if addr_len != 0 {
        unsafe { (addr_len as *mut u32).write_unaligned(0) };
    }
    read_from(fd, buf, len, flags & MSG_DONTWAIT == 0)
}

/// j925: `flock(fd, operation)`: granted. One persona process tree uses the image at a time,
/// and Cargo's package-cache and build-directory locks have nobody to wait for.
pub fn flock(fd: i64) -> i64 {
    match descriptor(fd) {
        Ok(_) => 0,
        Err(e) => e,
    }
}
