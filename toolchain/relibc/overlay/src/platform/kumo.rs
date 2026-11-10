//j535
//j557
//j759
//j856
//j859
// Generated from relibc's Pal traits at the revision pinned by KUMO. I-a1 made
// startup-channel intake, fd 1/2 write, process exit, and retained anonymous maps live;
// I-a2 lays KUMO's capabilities out as the SysV startup view relibc's own start path
// expects. — KESTREL / TIMBERDOODLE

#![allow(deprecated)]

use core::{
    num::NonZeroU64,
    sync::atomic::{AtomicU32, Ordering},
};

use super::{
    Pal, PalEpoll, PalPtrace, PalSignal, PalSocket,
    auxv_defs::{AT_NULL, AT_PAGESZ, AT_PHDR, AT_PHENT, AT_PHNUM},
    types::*,
};
use crate::{
    c_str::CStr,
    error::{Errno, Result},
    header::{
        bits_sigset_t::sigset_t,
        signal::{sigaction, sigevent, siginfo_t, sigval, stack_t},
        sys_epoll::epoll_event,
        sys_resource::{rlimit, rusage},
        sys_select::timeval,
        sys_socket::{msghdr, sockaddr, socklen_t},
        sys_stat::stat,
        sys_statvfs::statvfs,
        sys_time::{itimerval, timezone},
        sys_utsname::utsname,
        termios::{ECHO, ICANON, VMIN, termios},
        time::{itimerspec, timespec},
    },
    iter::NulTerminated,
    ld_so::tcb::OsSpecific,
    out::Out,
    pthread,
    start::Stack,
};

pub struct Sys;

const SYSCALL_HANDLE_CLOSE: u64 = 0;
const SYSCALL_CHANNEL_WRITE: u64 = 4;
const SYSCALL_CHANNEL_READ: u64 = 5;
const SYSCALL_VMO_CREATE: u64 = 12;
const SYSCALL_VMO_READ: u64 = 13;
const SYSCALL_VMAR_MAP: u64 = 15;
const SYSCALL_PROCESS_EXIT: u64 = 21;
const SYSCALL_CLOCK_GET: u64 = 26;
const SYSCALL_FUTEX_WAIT: u64 = 28;
const SYSCALL_FUTEX_WAKE: u64 = 29;

/// KUMO statuses this backend distinguishes. The whole error space is negative, which is
/// what lets `FutexWake` return a woken count in the same register.
const KUMO_OK: i32 = 0;
const KUMO_SHOULD_WAIT: i32 = -5;
const KUMO_NOT_SUPPORTED: i32 = -8;

const STARTUP_TAG_STDIN: u8 = b'i';
const STARTUP_TAG_STDOUT: u8 = b'o';
const STARTUP_TAG_STDERR: u8 = b'e';
const STARTUP_TAG_ARGV: u8 = b'a';
const STARTUP_TAG_FS: u8 = b'f';
const STARTUP_TAG_TTY: u8 = b't';
/// Mirror of `kumo-rt`'s `STARTUP_TAG_NET` (userland/kumo-rt/src/sys.rs). Only `net`
/// launches receive the client end of a `kns1` session channel; absent, sockets do not
/// exist in this process (DESIGN/026 §4). — TIMBERDOODLE
const STARTUP_TAG_NET: u8 = b'n';
/// SH2: mirror of `kumo-rt`'s `STARTUP_TAG_ENV`; the env VMO uses the argv wire
/// format (`[count u32][K=V\0 ...]`) and only an explicit launcher grant sends one.
const STARTUP_TAG_ENV: u8 = b'v';

// The `kumo-vfs` wire protocol. relibc is built from its own vendored source tree by a
// separate cargo invocation, so it cannot depend on the `kumo-vfs` crate; these constants
// are the second copy of that protocol and must move with it. The acceptance transcript
// is what proves the two ends still agree. — TIMBERDOODLE
const VFS_OP_OPEN: u8 = 0x00;
const VFS_OP_READ: u8 = 0x01;
const VFS_OP_OPEN_OPTIONS: u8 = 0x02;
const VFS_OP_WRITE: u8 = 0x03;
const VFS_OP_SET_LEN: u8 = 0x04;
const VFS_OP_FLUSH: u8 = 0x05;
const VFS_OPEN_READ: u8 = 1 << 0;
const VFS_OPEN_WRITE: u8 = 1 << 1;
const VFS_OPEN_CREATE: u8 = 1 << 2;
const VFS_OK: u8 = 0x00;
const VFS_NOT_FOUND: u8 = 0x01;
const VFS_BAD_HANDLE: u8 = 0x02;
const VFS_IS_DIR: u8 = 0x03;
const VFS_NO_SPACE: u8 = 0x05;
const VFS_ACCESS_DENIED: u8 = 0x06;
const VFS_MAX_PATH: usize = 255;
const VFS_READ_REQUEST_LEN: usize = 17;
const VFS_OPEN_REPLY_LEN: usize = 13;
const VFS_WRITE_HEADER_LEN: usize = 15;
const VFS_WRITE_REPLY_LEN: usize = 5;
/// The server's inline read payload ceiling, so one reply always fits one buffer.
const VFS_READ_MAX: usize = 256;

// The small `ttyd` control protocol. Stream bytes stay on the separately granted
// stdin/stdout channels; only terminal-owned state crosses this channel. relibc is built
// separately from the workspace, so these values intentionally mirror the `ttyd` crate.
// — KESTREL
const TTY_OP_GET_MODE: u8 = 0x04;
const TTY_OP_SET_MODE: u8 = 0x05;
const TTY_OK: u8 = 0x00;
const TTY_EVENT_MODE: u8 = 0x02;
const TTY_OP_GET_WINDOW_SIZE: u8 = 0x06;
const TTY_EVENT_WINDOW_SIZE: u8 = 0x03;
const TTY_MODE_CANONICAL: u8 = 1 << 0;
const TTY_MODE_ECHO: u8 = 1 << 1;
const TTY_REPLY_HEADER_BYTES: usize = 6;

// The `kns1`/`knr1` session protocol (DESIGN/026 §4, §6). Like the vfs constants above,
// this is a deliberate second copy: relibc builds from its own vendored tree by a separate
// cargo invocation and cannot depend on `kumo-netstack`. The NET13a acceptance transcript
// is what proves the two ends still agree. — TIMBERDOODLE
const KNS1_MAGIC: [u8; 4] = *b"KNS1";
const KNR1_MAGIC: [u8; 4] = *b"KNR1";
const KNS1_VERSION: u16 = 1;
const KNS1_HEADER_LEN: usize = 32;
const KNS1_ADDR_LEN: usize = 8;
const KNS1_FAMILY_IPV4: u16 = 2;
const KNS1_WAIT_READABLE: u8 = 1;
const KNS1_WAIT_WRITABLE: u8 = 2;
const KNS1_WAIT_CONNECTED: u8 = 4;
const KNS1_UDP_PAYLOAD_MAX: usize = 1472;
const KNS1_STREAM_PAYLOAD_MAX: usize = 2048;
const KNS1_MESSAGE_MAX: usize = KNS1_HEADER_LEN + KNS1_STREAM_PAYLOAD_MAX;
const KNS1_OP_HELLO: u16 = 1;
const KNS1_OP_SOCKET: u16 = 2;
const KNS1_OP_BIND: u16 = 3;
const KNS1_OP_CONNECT: u16 = 4;
const KNS1_OP_LISTEN: u16 = 5;
const KNS1_OP_ACCEPT: u16 = 6;
const KNS1_OP_SEND: u16 = 7;
const KNS1_OP_SEND_TO: u16 = 8;
const KNS1_OP_RECV: u16 = 9;
const KNS1_OP_RECV_FROM: u16 = 10;
const KNS1_OP_SHUTDOWN: u16 = 11;
const KNS1_OP_CLOSE: u16 = 12;
const KNS1_OP_WAIT: u16 = 13;
const KNS1_OP_GET_NAME: u16 = 14;
const KNS1_OP_RESOLVE: u16 = 15;
const KNS1_SOCKET_UDP: u64 = 1;
const KNS1_SOCKET_TCP: u64 = 2;
const KNS1_LISTEN_BACKLOG_MAX: u64 = 4;
/// IPPROTO_UDP / IPPROTO_TCP: the only protocols `socket()` accepts.
const KNS1_IPPROTO_UDP: c_int = 17;
const KNS1_IPPROTO_TCP: c_int = 6;
/// `sizeof(struct sockaddr_in)`: family, port, address, and eight zero bytes.
const SOCKADDR_IN_LEN: usize = 16;

/// Wire-compatible view of relibc's C `struct winsize`. Its upstream Rust fields are
/// intentionally private, while the PAL must fill the caller-owned C object.
#[repr(C)]
struct KumoWinsize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

const VMAR_READ: u64 = 1 << 0;
const VMAR_WRITE: u64 = 1 << 1;
const PAGE_SIZE: usize = 4096;

/// Bytes of packed argv KUMO can deliver. `VmoRead` copies at most 256 bytes per call and
/// the launcher packs into a 256-byte buffer, so one read is always the whole vector.
const ARGV_BYTES: usize = 256;
/// The env block rides one page-bounded VMO; 256 bytes matches VMO_TRANSFER_MAX, so
/// a single VmoRead moves the whole grant (the ABI caps one call at that many bytes).
const ENV_BYTES: usize = 256;
/// Arguments the startup image has room for, matching the launcher's own argv ceiling.
const ARGV_MAX: usize = 16;
const ENV_MAX: usize = 16;
/// `argc`, `argv` and its terminator, an empty `envp`, then five auxiliary-vector pairs.
const STARTUP_WORDS: usize = 1 + (ARGV_MAX + 1) + 1 + (ENV_MAX + 1) + 10;
/// Exit status for a startup precondition that failed before stdio existed. Distinct so a
/// silent abort can never be mistaken for a run that reached `main`.
const STARTUP_FAILURE_STATUS: c_int = 70;

static STDIN_HANDLE: AtomicU32 = AtomicU32::new(0);
static STDOUT_HANDLE: AtomicU32 = AtomicU32::new(0);
static STDERR_HANDLE: AtomicU32 = AtomicU32::new(0);
/// The client end of a filesystem server's `kumo-vfs` channel, or `0` when the launcher
/// granted none — in which case this process cannot name a file at all.
static FS_HANDLE: AtomicU32 = AtomicU32::new(0);
/// The explicitly granted client end of this process's `ttyd` control channel.
static TTY_HANDLE: AtomicU32 = AtomicU32::new(0);
/// The explicitly granted client end of this process's `kns1` netstack session, or `0`
/// when the launcher granted none — in which case no socket can exist at all.
static NET_HANDLE: AtomicU32 = AtomicU32::new(0);
/// The session generation the first HELLO answer established; zero before it.
static NET_GENERATION: AtomicU32 = AtomicU32::new(0);
/// Request ids start at one; zero is never a legal id (DESIGN/026 §4).
static NET_NEXT_REQUEST: AtomicU32 = AtomicU32::new(1);

/// What a POSIX `int fd` denotes in this process.
///
/// `DESIGN/008 §1.2`: there is no kernel fd. This is the one per-process table, and every
/// descriptor in it is a capability the launcher granted — an input or output channel, or a
/// file the filesystem server opened on our behalf. Nothing here can be forged from an integer.
#[derive(Clone, Copy)]
enum FdSlot {
    Closed,
    /// A granted channel this descriptor reads from. Whether a tty, a pipe, or a one-shot
    /// message sits on the far end is the launcher's business, not this program's.
    Input(u32),
    /// A granted channel this descriptor writes to.
    Output(u32),
    /// A file open on the filesystem server, plus this side's read cursor.
    File {
        handle: u32,
        offset: u64,
        size: u64,
    },
    /// A socket in this process's netstack session; the id names it inside KNS1.
    Socket {
        id: u32,
        /// KNS1 socket kind: 1 = UDP, 2 = TCP.
        kind: u8,
        /// Bit 0: O_NONBLOCK (fcntl F_SETFL); survives dup2, as POSIX requires.
        flags: u8,
        /// SO_RCVTIMEO / SO_SNDTIMEO in milliseconds; 0 = no timeout (block forever).
        rcv_timeo_ms: u16,
        snd_timeo_ms: u16,
    },
}

/// Descriptors this backend can hold. POSIX programs expect 0/1/2 to be stdio, so the table
/// is indexed directly by the descriptor number.
const FD_SLOTS: usize = 16;

/// Written by the startup thread before `main`, then only by the calling thread's own
/// `open`/`read`/`close`. A second thread sharing descriptors needs the kernel to own the
/// thread pointer first (J507), so this is not yet a lock's worth of contention.
static mut FD_TABLE: [FdSlot; FD_SLOTS] = [FdSlot::Closed; FD_SLOTS];

/// One `kns1` request/reply exchange buffer. Single-threaded until the kernel owns the
/// thread pointer (J507), so one buffer serves every call; the reply lands over the
/// request after the kernel has consumed it.
static mut NET_MESSAGE: [u8; KNS1_MESSAGE_MAX] = [0; KNS1_MESSAGE_MAX];

/// The packed argv page as delivered, kept for the lifetime of the process because the
/// `argv` pointers relibc publishes borrow the strings in place.
static mut ARGV_IMAGE: [u8; ARGV_BYTES] = [0; ARGV_BYTES];
static mut ENV_IMAGE: [u8; ENV_BYTES] = [0; ENV_BYTES];

/// The SysV initial-process image relibc reads through [`Stack`]. Both statics are written
/// exactly once, by the startup thread, before any other thread can exist.
static mut STARTUP_IMAGE: [usize; STARTUP_WORDS] = [0; STARTUP_WORDS];

fn syscall4(number: u64, x0: u64, x1: u64, x2: u64, x3: u64) -> u64 {
    let result;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") number,
            in("x0") x0,
            in("x1") x1,
            in("x2") x2,
            in("x3") x3,
            lateout("x0") result,
            clobber_abi("C"),
            options(nostack),
        );
    }
    result
}

fn channel_read_with_handle(channel: u32, tag: &mut u8) -> (u64, u64) {
    let count;
    let handle;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_CHANNEL_READ,
            in("x0") u64::from(channel),
            in("x1") core::ptr::from_mut(tag) as u64,
            in("x2") 1_u64,
            lateout("x0") count,
            lateout("x1") handle,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (count, handle)
}

fn vmar_map_anywhere(vmo: u32, len: usize) -> (u64, u64) {
    let status;
    let address;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_VMAR_MAP,
            in("x0") 0_u64,
            in("x1") u64::from(vmo),
            in("x2") 0_u64,
            in("x3") 0_u64,
            in("x4") len as u64,
            in("x5") VMAR_READ | VMAR_WRITE,
            lateout("x0") status,
            lateout("x1") address,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (status, address)
}

fn vmo_read(vmo: u32, offset: u64, dest: *mut u8, len: usize) -> u64 {
    syscall4(
        SYSCALL_VMO_READ,
        u64::from(vmo),
        offset,
        dest as u64,
        len as u64,
    )
}

fn channel_write_bytes(channel: u32, bytes: &[u8]) -> u64 {
    syscall4(
        SYSCALL_CHANNEL_WRITE,
        u64::from(channel),
        bytes.as_ptr() as u64,
        bytes.len() as u64,
        0,
    )
}

fn channel_read_bytes(channel: u32, buf: &mut [u8]) -> u64 {
    syscall4(
        SYSCALL_CHANNEL_READ,
        u64::from(channel),
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
    )
}

/// One `kumo-vfs` request/reply exchange. The read blocks until the server answers, which
/// is the whole reason the launcher spawns the server before the client.
fn vfs_call(request: &[u8], reply: &mut [u8]) -> Result<usize> {
    use crate::header::errno::{EBADF, EIO};

    let channel = FS_HANDLE.load(Ordering::Relaxed);
    if channel == 0 {
        return Err(Errno(EBADF));
    }
    if channel_write_bytes(channel, request) != 0 {
        return Err(Errno(EIO));
    }
    let n = channel_read_bytes(channel, reply);
    if n == 0 || n == u64::MAX {
        return Err(Errno(EIO));
    }
    Ok(n as usize)
}

fn tty_call(request: &[u8], reply: &mut [u8]) -> Result<usize> {
    use crate::header::errno::{EIO, ENOTTY};

    let channel = TTY_HANDLE.load(Ordering::Relaxed);
    if channel == 0 {
        return Err(Errno(ENOTTY));
    }
    if channel_write_bytes(channel, request) != 0 {
        return Err(Errno(EIO));
    }
    let n = channel_read_bytes(channel, reply);
    if n == 0 || n == u64::MAX {
        return Err(Errno(EIO));
    }
    Ok(n as usize)
}

/// Resolve an `open` path for the vfs wire, writing a relative one into `buf`.
///
/// A process's only directory is the root of the filesystem capability it was granted, so a
/// relative path names an entry under that root: `new.txt` and `./new.txt` are `/new.txt`.
/// Refusing relative paths with `EINVAL` made every relative `fopen` fail in a way no POSIX
/// program reads as "absent": kilo's `kilo new.txt` died on "Opening file: Invalid argument"
/// instead of starting an empty buffer. `.` alone is the root itself, which is not a file.
/// — TIMBERDOODLE
fn vfs_path<'a>(path: &'a [u8], buf: &'a mut [u8; VFS_MAX_PATH]) -> Result<&'a [u8]> {
    use crate::header::errno::{EISDIR, ENAMETOOLONG};

    if path.first() == Some(&b'/') {
        return if path.len() > VFS_MAX_PATH {
            Err(Errno(ENAMETOOLONG))
        } else {
            Ok(path)
        };
    }
    let mut rest = path;
    while let Some(tail) = rest.strip_prefix(b"./") {
        rest = tail;
    }
    if rest.is_empty() || rest == b"." {
        return Err(Errno(EISDIR));
    }
    let len = rest.len() + 1;
    if len > VFS_MAX_PATH {
        return Err(Errno(ENAMETOOLONG));
    }
    buf[0] = b'/';
    buf[1..len].copy_from_slice(rest);
    Ok(&buf[..len])
}

/// Map a vfs status byte onto the closest honest errno.
fn vfs_errno(status: u8) -> Errno {
    use crate::header::errno::{EACCES, EBADF, EIO, EISDIR, ENOENT, ENOSPC};

    match status {
        VFS_NOT_FOUND => Errno(ENOENT),
        VFS_BAD_HANDLE => Errno(EBADF),
        VFS_IS_DIR => Errno(EISDIR),
        VFS_NO_SPACE => Errno(ENOSPC),
        VFS_ACCESS_DENIED => Errno(EACCES),
        _ => Errno(EIO),
    }
}

fn fd_slot(fildes: c_int) -> Option<&'static mut FdSlot> {
    let index = usize::try_from(fildes).ok()?;
    if index >= FD_SLOTS {
        return None;
    }
    // One table, one writer; see `FD_TABLE`.
    unsafe { (&raw mut FD_TABLE).cast::<FdSlot>().add(index).as_mut() }
}

/// Claim the lowest free descriptor at or above 3, as POSIX requires of `open`.
fn fd_alloc() -> Option<c_int> {
    for index in 3..FD_SLOTS {
        if let Some(FdSlot::Closed) = fd_slot(index as c_int) {
            return Some(index as c_int);
        }
    }
    None
}

/// Claim the lowest free descriptor at or above `min` (POSIX F_DUPFD semantics).
fn fd_alloc_from(min: c_int) -> Option<c_int> {
    let start = usize::try_from(min.max(3)).ok()?;
    for index in start..FD_SLOTS {
        if let Some(FdSlot::Closed) = fd_slot(index as c_int) {
            return Some(index as c_int);
        }
    }
    None
}

fn close_handle(handle: u32) {
    if handle != 0 {
        let _ = syscall4(SYSCALL_HANDLE_CLOSE, u64::from(handle), 0, 0, 0);
    }
}

fn install_startup_handle(slot: &AtomicU32, handle: u32) {
    let old = slot.swap(handle, Ordering::Relaxed);
    if old != 0 && old != handle {
        close_handle(old);
    }
}


fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

/// Map a KNS1 status to the errno a POSIX caller expects (DESIGN/026 §6 to errno.h).
fn net_status_errno(status: u16) -> Errno {
    use crate::header::errno::{
        EADDRINUSE, EADDRNOTAVAIL, EAFNOSUPPORT, EAGAIN, EBADF, ECONNREFUSED, ECONNRESET,
        EINPROGRESS, EINVAL, EIO, EISCONN, EMFILE, EMSGSIZE, ENOENT, ENOSYS, ENETUNREACH,
        ENOTCONN, EPIPE, ETIMEDOUT,
    };
    Errno(match status {
        1 | 2 | 3 => EINVAL,   // BadVersion / BadOp / BadLength
        4 => EBADF,            // BadSocket
        5 => ECONNRESET,       // StaleSession
        6 => EMFILE,           // QuotaExceeded
        7 => EADDRINUSE,
        8 => EADDRNOTAVAIL,
        9 => ECONNREFUSED,
        10 => ECONNRESET,
        11 => ENOTCONN,
        12 => EISCONN,
        13 => EAGAIN,
        14 => EINPROGRESS,
        15 => ETIMEDOUT,
        16 => EMSGSIZE,
        17 => EAFNOSUPPORT,    // Unsupported: a capability this stack lacks
        18 => EINVAL,
        19 => EPIPE,           // ShutDown
        20 => ENETUNREACH,
        21 => ECONNRESET,      // ProtocolViolation
        22 => ENOENT,          // NotFound
        23 => ENOSYS,          // NotConfigured
        _ => EIO,
    })
}

fn net_status_ok(status: u16) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(net_status_errno(status))
    }
}

/// One `kns1` request/reply exchange. The session is strictly request/reply — one
/// outstanding message at a time — so the blocking read answers this write. Returns
/// (status, generation, socket, result, payload length); the payload stays in NET_MESSAGE.
fn net_exchange(
    handle: u32,
    op: u16,
    socket: u32,
    generation: u32,
    flags: u16,
    arg: u64,
    payload: &[u8],
) -> Result<(u16, u32, u32, u64, usize)> {
    use crate::header::errno::{ECONNRESET, EINVAL, EIO};

    let total = KNS1_HEADER_LEN
        .checked_add(payload.len())
        .ok_or(Errno(EIO))?;
    if total > KNS1_MESSAGE_MAX || payload.len() > u16::MAX as usize {
        return Err(Errno(EINVAL));
    }
    let request_id = loop {
        let id = NET_NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            break id;
        }
    };
    // SAFETY: one table, one writer; see `NET_MESSAGE`. The request is fully copied and
    // written before the reply is read over it.
    let message = unsafe { (&raw mut NET_MESSAGE).as_mut().unwrap() };
    message[0..4].copy_from_slice(&KNS1_MAGIC);
    message[4..6].copy_from_slice(&KNS1_VERSION.to_le_bytes());
    message[6..8].copy_from_slice(&op.to_le_bytes());
    message[8..12].copy_from_slice(&request_id.to_le_bytes());
    message[12..16].copy_from_slice(&socket.to_le_bytes());
    message[16..20].copy_from_slice(&generation.to_le_bytes());
    message[20..22].copy_from_slice(&flags.to_le_bytes());
    message[22..24].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    message[24..32].copy_from_slice(&arg.to_le_bytes());
    message[KNS1_HEADER_LEN..total].copy_from_slice(payload);
    if channel_write_bytes(handle, &message[..total]) != 0 {
        return Err(Errno(EIO));
    }
    let n = channel_read_bytes(handle, message);
    if n == u64::MAX || (n as usize) < KNS1_HEADER_LEN {
        // The kernel answers a closed channel with none: the session provider is gone.
        return Err(Errno(ECONNRESET));
    }
    let n = n as usize;
    if message[0..4] != KNR1_MAGIC
        || le16(message, 4) != KNS1_VERSION
        || le16(message, 22) as usize != n - KNS1_HEADER_LEN
        || le32(message, 8) != request_id
    {
        return Err(Errno(EIO));
    }
    Ok((
        le16(message, 20),
        le32(message, 16),
        le32(message, 12),
        le64(message, 24),
        n - KNS1_HEADER_LEN,
    ))
}

/// Establish the session generation once: HELLO travels with generation 0, and its answer
/// carries the generation every later request must repeat.
fn net_hello(handle: u32) -> Result<()> {
    use crate::header::errno::EIO;

    let (status, generation, _socket, _result, _len) =
        net_exchange(handle, KNS1_OP_HELLO, 0, 0, 0, 0, &[])?;
    net_status_ok(status)?;
    if generation == 0 {
        return Err(Errno(EIO));
    }
    NET_GENERATION.store(generation, Ordering::Relaxed);
    Ok(())
}

fn net_session() -> Result<u32> {
    use crate::header::errno::EACCES;

    let handle = NET_HANDLE.load(Ordering::Relaxed);
    if handle == 0 {
        // Network access is a launcher decision (DESIGN/026 §4): without the session
        // capability a socket cannot exist, and pretending otherwise would be a stub.
        return Err(Errno(EACCES));
    }
    Ok(handle)
}

fn net_call(
    op: u16,
    socket: u32,
    arg: u64,
    flags: u16,
    payload: &[u8],
) -> Result<(u16, u32, u64, usize)> {
    use crate::header::errno::ECONNRESET;

    let handle = net_session()?;
    let mut generation = NET_GENERATION.load(Ordering::Relaxed);
    if generation == 0 {
        net_hello(handle)?;
        generation = NET_GENERATION.load(Ordering::Relaxed);
    }
    let (status, reply_generation, reply_socket, result, len) =
        net_exchange(handle, op, socket, generation, flags, arg, payload)?;
    if reply_generation != generation {
        // A different generation is a different session; nothing ties this reply to us.
        return Err(Errno(ECONNRESET));
    }
    Ok((status, reply_socket, result, len))
}


const FD_SOCK_NONBLOCK: u8 = 1;
const SOCKET_KIND_UDP: u8 = 1;
const SOCKET_KIND_TCP: u8 = 2;

fn socket_info(fildes: c_int) -> Result<(u32, u8, u8, u16, u16)> {
    use crate::header::errno::{EBADF, ENOTSOCK};

    match fd_slot(fildes).map(|slot| *slot) {
        Some(FdSlot::Socket {
            id,
            kind,
            flags,
            rcv_timeo_ms,
            snd_timeo_ms,
        }) => Ok((id, kind, flags, rcv_timeo_ms, snd_timeo_ms)),
        Some(FdSlot::Closed) | None => Err(Errno(EBADF)),
        Some(_) => Err(Errno(ENOTSOCK)),
    }
}

/// How many descriptors other than `skip` name the same session socket.
fn socket_alias_count(id: u32, skip: c_int) -> usize {
    let mut count = 0;
    for index in 0..FD_SLOTS {
        if index as c_int == skip {
            continue;
        }
        if let Some(FdSlot::Socket { id: other, .. }) = fd_slot(index as c_int) {
            if *other == id {
                count += 1;
            }
        }
    }
    count
}

/// Wait for `interest` on a socket, honoring a POSIX timeout budget: 0 blocks until
/// ready; otherwise expiry answers EAGAIN. The bounded windows keep every single KNS1
/// wait under the stack's own 60 s cap.
fn wait_interest(id: u32, interest: u8, timeout_ms: u16) -> Result<()> {
    use crate::header::errno::EAGAIN;

    const WINDOW_MS: u32 = 2000;
    let mut remaining = if timeout_ms == 0 {
        None
    } else {
        Some(u32::from(timeout_ms))
    };
    loop {
        let window = remaining.map_or(WINDOW_MS, |r| r.clamp(1, WINDOW_MS));
        let (status, _s, _r, _len) = net_call(
            KNS1_OP_WAIT,
            id,
            u64::from(interest) | (u64::from(window) << 32),
            0,
            &[],
        )?;
        match status {
            0 => return Ok(()),
            15 => {
                if let Some(r) = remaining.as_mut() {
                    *r = r.saturating_sub(window);
                    if *r == 0 {
                        return Err(Errno(EAGAIN));
                    }
                }
            }
            other => return Err(net_status_errno(other)),
        }
    }
}

/// Stream send: one KNS1 SEND per call, so partial sends surface exactly as POSIX
/// defines them. A full buffer blocks through WAIT_WRITABLE unless nonblocking.
fn stream_send(info: (u32, u8, u8, u16, u16), data: &[u8]) -> Result<usize> {
    use crate::header::errno::EAGAIN;

    let (id, _kind, flags, _rcv, snd_timeo) = info;
    if data.is_empty() {
        return Ok(0);
    }
    let chunk = &data[..data.len().min(KNS1_STREAM_PAYLOAD_MAX)];
    loop {
        let (status, _s, result, _len) = net_call(KNS1_OP_SEND, id, 0, 0, chunk)?;
        match status {
            0 => return Ok(result as usize),
            13 => {
                if flags & FD_SOCK_NONBLOCK != 0 {
                    return Err(Errno(EAGAIN));
                }
                wait_interest(id, KNS1_WAIT_WRITABLE, snd_timeo)?;
            }
            other => return Err(net_status_errno(other)),
        }
    }
}

/// Stream receive: one KNS1 RECV per call; a zero-length answer is the orderly EOF.
fn stream_recv(
    info: (u32, u8, u8, u16, u16),
    nonblock: bool,
    out: *mut u8,
    len: usize,
    address: *mut sockaddr,
    address_len: *mut socklen_t,
) -> Result<usize> {
    use crate::header::errno::EAGAIN;

    let (id, _kind, _flags, rcv_timeo, _snd) = info;
    if len == 0 {
        return Ok(0);
    }
    let max = len.min(KNS1_STREAM_PAYLOAD_MAX);
    loop {
        let (status, _s, _result, payload_len) = net_call(KNS1_OP_RECV, id, max as u64, 0, &[])?;
        match status {
            0 => {
                let message = unsafe { (&raw const NET_MESSAGE).as_ref().unwrap() };
                let data = &message[KNS1_HEADER_LEN..KNS1_HEADER_LEN + payload_len];
                let take = data.len().min(max);
                if take > 0 {
                    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), out, take) };
                }
                if !address.is_null() && !address_len.is_null() {
                    fill_sockaddr_from_getname(id, true, address, address_len)?;
                }
                return Ok(take);
            }
            13 => {
                if nonblock {
                    return Err(Errno(EAGAIN));
                }
                wait_interest(id, KNS1_WAIT_READABLE, rcv_timeo)?;
            }
            other => return Err(net_status_errno(other)),
        }
    }
}

/// Fill a C sockaddr from KNS1 GETNAME (payload: local addr then peer addr).
fn fill_sockaddr_from_getname(
    id: u32,
    peer: bool,
    address: *mut sockaddr,
    address_len: *mut socklen_t,
) -> Result<()> {
    use crate::header::errno::EIO;

    let (status, _s, _r, payload_len) = net_call(KNS1_OP_GET_NAME, id, 0, 0, &[])?;
    net_status_ok(status)?;
    if payload_len < 2 * KNS1_ADDR_LEN {
        return Err(Errno(EIO));
    }
    let message = unsafe { (&raw const NET_MESSAGE).as_ref().unwrap() };
    let base = KNS1_HEADER_LEN + if peer { KNS1_ADDR_LEN } else { 0 };
    let filled = sockaddr_in_fill(
        [
            message[base + 4],
            message[base + 5],
            message[base + 6],
            message[base + 7],
        ],
        le16(message, base + 2),
    );
    let cap = unsafe { (*address_len as usize).min(SOCKADDR_IN_LEN) };
    unsafe {
        core::ptr::copy_nonoverlapping(filled.as_ptr(), address.cast::<u8>(), cap);
        *address_len = SOCKADDR_IN_LEN as socklen_t;
    }
    Ok(())
}

fn socket_id(fildes: c_int) -> Result<u32> {
    use crate::header::errno::{EBADF, ENOTSOCK};

    match fd_slot(fildes).map(|slot| *slot) {
        Some(FdSlot::Socket { id, .. }) => Ok(id),
        Some(FdSlot::Closed) | None => Err(Errno(EBADF)),
        Some(_) => Err(Errno(ENOTSOCK)),
    }
}

/// Parse the C `sockaddr_in` KUMO accepts: IPv4 only, `sizeof(sockaddr_in)` bytes.
///
/// SAFETY: the caller passes the pointer POSIX defined together with its length.
unsafe fn sockaddr_in_parse(
    address: *const sockaddr,
    address_len: socklen_t,
) -> Result<([u8; 4], u16)> {
    use crate::header::errno::{EAFNOSUPPORT, EINVAL};
    use crate::header::sys_socket::constants::AF_INET;

    if address.is_null() {
        return Err(Errno(EINVAL));
    }
    if (address_len as usize) < SOCKADDR_IN_LEN {
        return Err(Errno(EINVAL));
    }
    let bytes = unsafe { core::slice::from_raw_parts(address.cast::<u8>(), SOCKADDR_IN_LEN) };
    if c_int::from(u16::from_ne_bytes([bytes[0], bytes[1]])) != AF_INET {
        return Err(Errno(EAFNOSUPPORT));
    }
    Ok((
        [bytes[4], bytes[5], bytes[6], bytes[7]],
        u16::from_be_bytes([bytes[2], bytes[3]]),
    ))
}

fn sockaddr_in_fill(ip: [u8; 4], port: u16) -> [u8; SOCKADDR_IN_LEN] {
    use crate::header::sys_socket::constants::AF_INET;

    let mut out = [0u8; SOCKADDR_IN_LEN];
    out[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    out[2..4].copy_from_slice(&port.to_be_bytes());
    out[4..8].copy_from_slice(&ip);
    out
}

/// A startup precondition failed before stdio existed. Report through the granted output
/// channel — the only authority held this early — and exit with a distinct status, so a
/// program that never reached `main` can never be read as one that ran. — TIMBERDOODLE
fn startup_fail(message: &[u8]) -> ! {
    let handle = STDERR_HANDLE.load(Ordering::Relaxed);
    if handle != 0 {
        let _ = syscall4(
            SYSCALL_CHANNEL_WRITE,
            u64::from(handle),
            message.as_ptr() as u64,
            message.len() as u64,
            0,
        );
    }
    <Sys as Pal>::exit(STARTUP_FAILURE_STATUS)
}

/// Drain KUMO's finite bootstrap capability set, keeping only what this slice can honour.
///
/// Unknown capabilities are closed rather than becoming ambient authority. Returns the
/// granted argv VMO, or `0` when the launcher granted none. — KESTREL / TIMBERDOODLE
fn take_bootstrap_capabilities(bootstrap: u32) -> (u32, u32) {
    let mut argv_vmo = 0_u32;
    let mut env_vmo = 0_u32;
    let mut tag = 0_u8;
    loop {
        let (count, raw_handle) = channel_read_with_handle(bootstrap, &mut tag);
        if count == 0 || count == u64::MAX {
            break;
        }
        let handle = raw_handle as u32;
        if handle == 0 {
            continue;
        }
        match tag {
            STARTUP_TAG_STDIN => install_startup_handle(&STDIN_HANDLE, handle),
            STARTUP_TAG_STDOUT => install_startup_handle(&STDOUT_HANDLE, handle),
            STARTUP_TAG_STDERR => install_startup_handle(&STDERR_HANDLE, handle),
            STARTUP_TAG_ARGV => {
                close_handle(core::mem::replace(&mut argv_vmo, handle));
            }
            STARTUP_TAG_FS => install_startup_handle(&FS_HANDLE, handle),
            STARTUP_TAG_TTY => install_startup_handle(&TTY_HANDLE, handle),
            STARTUP_TAG_NET => install_startup_handle(&NET_HANDLE, handle),
            STARTUP_TAG_ENV => {
                close_handle(core::mem::replace(&mut env_vmo, handle));
            }
            _ => close_handle(handle),
        }
    }
    close_handle(bootstrap);

    // Today's launcher grants one output channel. Treat it as both POSIX output
    // streams until distinct stderr authority is supplied. — KESTREL
    let stdout = STDOUT_HANDLE.load(Ordering::Relaxed);
    let stderr = STDERR_HANDLE.load(Ordering::Relaxed);
    if stderr == 0 {
        STDERR_HANDLE.store(stdout, Ordering::Relaxed);
    } else if stdout == 0 {
        STDOUT_HANDLE.store(stderr, Ordering::Relaxed);
    }

    // Seat the granted channels as the descriptors C expects them at. A stream that was not
    // granted leaves its descriptor closed rather than aliased onto one that was: a program
    // reading fd 0 it never received must fail, not quietly read somewhere else.
    let stdin = STDIN_HANDLE.load(Ordering::Relaxed);
    let stdout = STDOUT_HANDLE.load(Ordering::Relaxed);
    let stderr = STDERR_HANDLE.load(Ordering::Relaxed);
    if stdin != 0 {
        if let Some(slot) = fd_slot(0) {
            *slot = FdSlot::Input(stdin);
        }
    }
    for (fildes, handle) in [(1, stdout), (2, stderr)] {
        if handle == 0 {
            continue;
        }
        if let Some(slot) = fd_slot(fildes) {
            *slot = FdSlot::Output(handle);
        }
    }
    (argv_vmo, env_vmo)
}

/// SH2: turn a granted env VMO into relibc's environment. The strings are copied out
/// and leaked for the process lifetime (as `start.rs`'s `copy_string_array` does), the
/// pointer array lands in `OUR_ENVIRON`, and `environ` names it, so relibc's getenv,
/// setenv and unsetenv keep owning one array. No grant installs the empty environment.
/// — TIMBERDOODLE
unsafe fn install_environ(env_vmo: u32) {
    use alloc::ffi::CString;
    use alloc::vec::Vec;

    let mut pointers: Vec<*mut c_char> = Vec::new();
    if env_vmo != 0 {
        let mut buffer = [0u8; ENV_BYTES];
        if vmo_read(env_vmo, 0, buffer.as_mut_ptr(), ENV_BYTES) == 0 {
            let count = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
            let mut pos = 4usize;
            let mut taken = 0usize;
            while taken < count.min(ENV_MAX) && pos < ENV_BYTES {
                let start = pos;
                while pos < ENV_BYTES && buffer[pos] != 0 {
                    pos += 1;
                }
                if pos == ENV_BYTES {
                    break;
                }
                if let Ok(text) = CString::new(&buffer[start..pos]) {
                    pointers.push(text.into_raw());
                    taken += 1;
                }
                pos += 1;
            }
        }
    }
    pointers.push(core::ptr::null_mut());
    unsafe {
        crate::platform::OUR_ENVIRON.unsafe_set(pointers);
        crate::platform::environ = crate::platform::OUR_ENVIRON.unsafe_mut().as_mut_ptr();
    }
}

/// Lay KUMO's granted capabilities out as the SysV initial-process image.
///
/// KUMO packs argv as `[argc: u32 LE][arg0]\0[arg1]\0 …` into one read-only VMO, so the
/// copied page can back the `argv` pointers directly. `envp` is deliberately a lone
/// terminator: KUMO grants no environment capability yet, and inventing one here would be
/// ambient authority wearing a POSIX name. `AT_PHDR`/`AT_PHENT`/`AT_PHNUM` are read from
/// this image's own mapped ELF header, which is how relibc's static TLS setup finds
/// `PT_TLS` without the kernel loader growing a POSIX opinion. — TIMBERDOODLE
unsafe fn build_startup_image(argv_vmo: u32, env_vmo: u32) -> &'static Stack {
    unsafe extern "C" {
        /// Defined by the linker when the ELF header is covered by a `PT_LOAD` segment.
        static __ehdr_start: [u8; 64];
    }

    let image = (&raw mut STARTUP_IMAGE).cast::<usize>();
    let argv_bytes = (&raw mut ARGV_IMAGE).cast::<u8>();
    let env_bytes = (&raw mut ENV_IMAGE).cast::<u8>();

    let mut argc = 0_usize;
    if argv_vmo != 0 && vmo_read(argv_vmo, 0, argv_bytes, ARGV_BYTES) == 0 {
        let header = unsafe { core::ptr::read_unaligned(argv_bytes.cast::<[u8; 4]>()) };
        let packed = u32::from_le_bytes(header) as usize;
        let mut pos = header.len();
        while argc < packed.min(ARGV_MAX) && pos < ARGV_BYTES {
            let start = pos;
            while pos < ARGV_BYTES && unsafe { *argv_bytes.add(pos) } != 0 {
                pos += 1;
            }
            if pos == ARGV_BYTES {
                // A truncated tail is dropped rather than published unterminated.
                break;
            }
            unsafe { image.add(1 + argc).write(argv_bytes.add(start) as usize) };
            argc += 1;
            pos += 1;
        }
    }

    // SH2: the env block, same walk as argv; an ungranted program gets an empty envp,
    // which is exactly "no environment", not an ambient one.
    let mut envc = 0_usize;
    if env_vmo != 0 && vmo_read(env_vmo, 0, env_bytes, ENV_BYTES) == 0 {
        let header = unsafe { core::ptr::read_unaligned(env_bytes.cast::<[u8; 4]>()) };
        let packed = u32::from_le_bytes(header) as usize;
        let mut pos = header.len();
        while envc < packed.min(ENV_MAX) && pos < ENV_BYTES {
            let start = pos;
            while pos < ENV_BYTES && unsafe { *env_bytes.add(pos) } != 0 {
                pos += 1;
            }
            if pos == ENV_BYTES {
                break;
            }
            unsafe { image.add(2 + argc + envc).write(env_bytes.add(start) as usize) };
            envc += 1;
            pos += 1;
        }
    }

    unsafe {
        image.write(argc);
        image.add(1 + argc).write(0);
        image.add(2 + argc + envc).write(0);
    }

    let ehdr = (&raw const __ehdr_start).cast::<u8>();
    if unsafe { core::ptr::read_unaligned(ehdr.cast::<[u8; 4]>()) } != *b"\x7fELF" {
        startup_fail(b"relibc: no mapped ELF header for KUMO startup\n");
    }
    let phoff = unsafe { core::ptr::read_unaligned(ehdr.add(32).cast::<u64>()) } as usize;
    let phentsize = unsafe { core::ptr::read_unaligned(ehdr.add(54).cast::<u16>()) } as usize;
    let phnum = unsafe { core::ptr::read_unaligned(ehdr.add(56).cast::<u16>()) } as usize;

    let auxv = 3 + argc + envc;
    let vector = [
        AT_PHDR,
        ehdr as usize + phoff,
        AT_PHENT,
        phentsize,
        AT_PHNUM,
        phnum,
        AT_PAGESZ,
        PAGE_SIZE,
        AT_NULL,
        0,
    ];
    for (index, entry) in vector.into_iter().enumerate() {
        unsafe { image.add(auxv + index).write(entry) };
    }

    unsafe { &*image.cast::<Stack>() }
}

/// KUMO's C startup entry. `_start` hands over the finite bootstrap channel and the
/// address of `main`; everything after the SysV image is built — TLS activation, stdio,
/// the init arrays, `main`, and exit — is upstream relibc's own start path. — TIMBERDOODLE
/// The Rust `std` entry's startup intake: install the fd handles from the bootstrap
/// stream exactly as the C start path does, and hand the argv VMO back to the caller,
/// which owns its parse and close. std owns the rest of its runtime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __kumo_startup(bootstrap: u64) -> u32 {
    let (argv_vmo, env_vmo) = take_bootstrap_capabilities(bootstrap as u32);
    unsafe { install_environ(env_vmo) };
    close_handle(env_vmo);
    // The std entry bypasses relibc_start_v1, which is what normally initializes stdio
    // and the main thread's Pthread record. Without this, exit's flush_io_streams walks
    // a null FILE and faults, and pthread_self has no record (J856). Mirror start.rs's
    // order: stdio first, then pthread; both are idempotent per-process.
    unsafe {
        crate::header::stdio::stdin = crate::header::stdio::default_stdin().get();
        crate::header::stdio::stdout = crate::header::stdio::default_stdout().get();
        crate::header::stdio::stderr = crate::header::stdio::default_stderr().get();
        crate::pthread::init();
    }
    argv_vmo
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __relibc_kumo_start(
    bootstrap: u64,
    main: unsafe extern "C" fn(
        argc: isize,
        argv: *mut *mut c_char,
        envp: *mut *mut c_char,
    ) -> c_int,
) -> ! {
    // KUMO does not yet define `tpidr_el0` for a fresh EL0 thread, and firmware leaves a
    // stale value in it. relibc reads a non-zero thread pointer as "TLS is already
    // active", skips its static setup, and then dereferences that stale value. Claiming
    // the register here is the crt0's job — it is this thread's own state, and a later
    // kernel slice that owns the thread pointer makes this a no-op rather than a
    // conflict. — TIMBERDOODLE
    unsafe {
        core::arch::asm!(
            "msr tpidr_el0, xzr",
            options(nomem, nostack, preserves_flags)
        )
    };

    let (argv_vmo, env_vmo) = take_bootstrap_capabilities(bootstrap as u32);
    let stack = unsafe { build_startup_image(argv_vmo, env_vmo) };
    close_handle(argv_vmo);
    close_handle(env_vmo);
    unsafe { crate::start::relibc_start_v1(stack, main) }
}

impl Sys {
    /// NET15: resolve a dotted name through the NET12 resolver over the session channel.
    ///
    /// Polls under the launcher's own netprobe cadence: InProgress and NetUnreachable
    /// are retried while a temporary unbound UDP socket parks the caller in the
    /// netstack's Wait for 200 ms each step — never a spin, because a spinning child
    /// outranks Sora and can starve the very service wave that wires the NIC.
    pub fn resolve_host(host: &str) -> Result<[u8; 4], Errno> {
        use crate::header::errno::{ECONNREFUSED, EINVAL, EIO, ENOENT, ETIMEDOUT};

        let name = host.as_bytes();
        if name.is_empty() || name.len() > 255 {
            return Err(Errno(EINVAL));
        }
        let (status, _s, pause, _len) = net_call(KNS1_OP_SOCKET, 0, KNS1_SOCKET_UDP, 0, &[])?;
        if status != 0 {
            return Err(Errno(EIO));
        }
        // The reply's result word is u64; socket ids ride the u32 fields.
        let pause = pause as u32;
        let mut attempts = 0u32;
        let mut outcome = Err(Errno(ETIMEDOUT));
        loop {
            let (status, _s, result, _len) = net_call(KNS1_OP_RESOLVE, 0, 0, 0, name)?;
            match status {
                0 => {
                    outcome = Ok((result as u32).to_be_bytes());
                    break;
                }
                14 | 20 => {
                    attempts += 1;
                    if attempts > 1500 {
                        break;
                    }
                    let _ = wait_interest(pause, KNS1_WAIT_READABLE, 200);
                }
                22 => {
                    outcome = Err(Errno(ENOENT));
                    break;
                }
                23 => {
                    outcome = Err(Errno(ECONNREFUSED));
                    break;
                }
                _ => {
                    outcome = Err(Errno(EIO));
                    break;
                }
            }
        }
        let _ = net_call(KNS1_OP_CLOSE, pause, 0, 0, &[]);
        outcome
    }

    pub unsafe fn ioctl(fd: c_int, request: c_ulong, out: *mut c_void) -> Result<c_int> {
        use crate::header::{
            errno::{EBADF, EFAULT, EIO, ENOTTY},
            sys_ioctl::{TCGETS, TCSETS, TCSETSF, TCSETSW, TIOCGWINSZ},
        };

        // FIONBIO: std's set_nonblocking asks through ioctl, not fcntl.
        if request == crate::header::sys_ioctl::constants::FIONBIO {
            if out.is_null() {
                return Err(Errno(EFAULT));
            }
            let value = unsafe { core::ptr::read_unaligned(out.cast::<c_int>()) };
            let slot = fd_slot(fd).ok_or(Errno(EBADF))?;
            return match slot {
                FdSlot::Socket { flags, .. } => {
                    *flags = if value != 0 { FD_SOCK_NONBLOCK } else { 0 };
                    Ok(0)
                }
                FdSlot::Closed => Err(Errno(EBADF)),
                _ => Err(Errno(ENOTTY)),
            };
        }

        match fd_slot(fd).map(|slot| *slot) {
            None | Some(FdSlot::Closed) => return Err(Errno(EBADF)),
            Some(FdSlot::File { .. }) | Some(FdSlot::Socket { .. }) => {
                return Err(Errno(ENOTTY));
            }
            Some(FdSlot::Input(_)) | Some(FdSlot::Output(_)) => {}
        }
        if out.is_null() {
            return Err(Errno(EFAULT));
        }

        match request {
            TCGETS => {
                let mut reply = [0u8; TTY_REPLY_HEADER_BYTES + 1];
                let n = tty_call(&[TTY_OP_GET_MODE], &mut reply)?;
                if n != reply.len()
                    || reply[0] != TTY_OK
                    || reply[1] != TTY_EVENT_MODE
                    || u16::from_le_bytes([reply[2], reply[3]]) != 0
                    || u16::from_le_bytes([reply[4], reply[5]]) != 1
                {
                    return Err(Errno(EIO));
                }

                let mode = reply[TTY_REPLY_HEADER_BYTES];
                let mut value = termios::default();
                if mode & TTY_MODE_CANONICAL != 0 {
                    value.c_lflag |= ICANON as u32;
                }
                if mode & TTY_MODE_ECHO != 0 {
                    value.c_lflag |= ECHO as u32;
                }
                value.c_cc[VMIN] = 1;
                unsafe { out.cast::<termios>().write(value) };
                Ok(0)
            }
            TCSETS | TCSETSW | TCSETSF => {
                let value = unsafe { &*out.cast::<termios>() };
                let mode = (if value.c_lflag & ICANON as u32 != 0 {
                    TTY_MODE_CANONICAL
                } else {
                    0
                }) | (if value.c_lflag & ECHO as u32 != 0 {
                    TTY_MODE_ECHO
                } else {
                    0
                });
                let action = (request - TCSETS) as u8;
                let mut reply = [0u8; TTY_REPLY_HEADER_BYTES];
                let n = tty_call(&[TTY_OP_SET_MODE, mode, action], &mut reply)?;
                if n != reply.len() || reply[0] != TTY_OK {
                    return Err(Errno(EIO));
                }
                Ok(0)
            }
            TIOCGWINSZ => {
                let mut reply = [0u8; TTY_REPLY_HEADER_BYTES + 4];
                let n = tty_call(&[TTY_OP_GET_WINDOW_SIZE], &mut reply)?;
                if n != reply.len()
                    || reply[0] != TTY_OK
                    || reply[1] != TTY_EVENT_WINDOW_SIZE
                    || u16::from_le_bytes([reply[2], reply[3]]) != 0
                    || u16::from_le_bytes([reply[4], reply[5]]) != 4
                {
                    return Err(Errno(EIO));
                }
                let rows = u16::from_le_bytes([
                    reply[TTY_REPLY_HEADER_BYTES],
                    reply[TTY_REPLY_HEADER_BYTES + 1],
                ]);
                let columns = u16::from_le_bytes([
                    reply[TTY_REPLY_HEADER_BYTES + 2],
                    reply[TTY_REPLY_HEADER_BYTES + 3],
                ]);
                if rows == 0 || columns == 0 {
                    return Err(Errno(EIO));
                }
                unsafe {
                    out.cast::<KumoWinsize>().write(KumoWinsize {
                        rows,
                        columns,
                        x_pixels: 0,
                        y_pixels: 0,
                    })
                };
                Ok(0)
            }
            _ => Err(unsupported()),
        }
    }
}

/// Whether `clk_id` is one of the clocks `ClockGet` answers: time since boot, never adjusted.
fn monotonic_clock(clk_id: clockid_t) -> Result<()> {
    use crate::header::{
        errno::EINVAL,
        time::{CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW},
    };
    match clk_id {
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => Ok(()),
        _ => Err(Errno(EINVAL)),
    }
}

const fn unsupported() -> Errno {
    Errno(crate::header::errno::ENOSYS)
}

impl Pal for Sys {
    fn faccessat(fd: c_int, path: CStr, amode: c_int, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn brk(addr: *mut c_void) -> Result<*mut c_void> {
        Err(unsupported())
    }

    fn chdir(path: CStr) -> Result<()> {
        Err(unsupported())
    }

    //j759
    /// The kernel's `ClockGet` counts nanoseconds since boot, which is exactly what the monotonic
    /// clocks are. KUMO keeps no wall clock, so `CLOCK_REALTIME` and the CPU-time clocks are
    /// refused rather than invented. Gallium's BO cache ages its BOs by `CLOCK_MONOTONIC`;
    /// against the old stub it aged them by uninitialized stack and evicted a live BO (GL1-b).
    /// — TIMBERDOODLE
    fn clock_getres(clk_id: clockid_t, tp: Option<Out<timespec>>) -> Result<()> {
        monotonic_clock(clk_id)?;
        if let Some(mut tp) = tp {
            tp.write(timespec {
                tv_sec: 0,
                tv_nsec: 1,
            });
        }
        Ok(())
    }

    fn clock_gettime(clk_id: clockid_t, mut tp: Out<timespec>) -> Result<()> {
        monotonic_clock(clk_id)?;
        let nanos = syscall4(SYSCALL_CLOCK_GET, 0, 0, 0, 0);
        tp.write(timespec {
            tv_sec: (nanos / 1_000_000_000) as time_t,
            tv_nsec: (nanos % 1_000_000_000) as c_long,
        });
        Ok(())
    }

    unsafe fn clock_settime(clk_id: clockid_t, tp: *const timespec) -> Result<()> {
        Err(unsupported())
    }

    /// Release a descriptor.
    ///
    /// `kumo-vfs` has no close op yet: the server's open table is freed when its channel
    /// does, and the launcher gives each program its own server instance, so a leaked
    /// server-side entry cannot outlive the program that made it. Closing an output
    /// descriptor does not close the underlying channel, which other descriptors alias.
    fn close(fildes: c_int) -> Result<()> {
        use crate::header::errno::EBADF;

        let slot = fd_slot(fildes).ok_or(Errno(EBADF))?;
        match core::mem::replace(slot, FdSlot::Closed) {
            FdSlot::Closed => Err(Errno(EBADF)),
            FdSlot::Socket { id, .. } => {
                // Release the session's socket when this was its last descriptor; a
                // dup2 alias keeps it alive, and a dead session cannot fail the close.
                if socket_alias_count(id, fildes) == 0 {
                    let _ = net_call(KNS1_OP_CLOSE, id, 0, 0, &[]);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// POSIX dup2: the target is replaced by an alias of the source; a socket alias
    /// keeps the session entry alive until the last descriptor naming it closes.
    fn dup2(fildes: c_int, fildes2: c_int) -> Result<c_int> {
        use crate::header::errno::EBADF;

        let source = *fd_slot(fildes).ok_or(Errno(EBADF))?;
        if matches!(source, FdSlot::Closed) {
            return Err(Errno(EBADF));
        }
        if fildes2 < 0 || fildes2 as usize >= FD_SLOTS {
            return Err(Errno(EBADF));
        }
        if fildes == fildes2 {
            return Ok(fildes2);
        }
        let target = *fd_slot(fildes2).ok_or(Errno(EBADF))?;
        if let FdSlot::Socket { id, .. } = target {
            *fd_slot(fildes2).ok_or(Errno(EBADF))? = FdSlot::Closed;
            if socket_alias_count(id, fildes2) == 0 {
                let _ = net_call(KNS1_OP_CLOSE, id, 0, 0, &[]);
            }
        }
        *fd_slot(fildes2).ok_or(Errno(EBADF))? = source;
        Ok(fildes2)
    }

    unsafe fn execve(path: CStr, argv: *const *mut c_char, envp: *const *mut c_char) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn fexecve(
        fildes: c_int,
        argv: *const *mut c_char,
        envp: *const *mut c_char,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn exit(status: c_int) -> ! {
        let _ = syscall4(SYSCALL_PROCESS_EXIT, status as u64, 0, 0, 0);
        loop {
            core::hint::spin_loop();
        }
    }

    unsafe fn exit_thread(_stack_base: *mut (), _stack_size: usize) -> ! {
        panic!("relibc KUMO stub called: exit_thread")
    }

    fn fchdir(fildes: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fchmodat(dirfd: c_int, path: Option<CStr>, mode: mode_t, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fchownat(fildes: c_int, path: CStr, owner: uid_t, group: gid_t, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fdatasync(fildes: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn flock(fd: c_int, operation: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fstatat(fildes: c_int, path: Option<CStr>, buf: Out<stat>, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fstatvfs(fildes: c_int, buf: Out<statvfs>) -> Result<()> {
        Err(unsupported())
    }

    /// F_GETFL / F_SETFL with O_NONBLOCK: the only descriptor flag this backend owns.
    fn fcntl(fildes: c_int, cmd: c_int, arg: c_ulonglong) -> Result<c_int> {
        use crate::header::bits_open_flags::O_NONBLOCK;
        use crate::header::errno::{EBADF, EINVAL, EMFILE};
        use crate::header::fcntl::{F_DUPFD, F_DUPFD_CLOEXEC, F_GETFL, F_SETFL};

        let slot = fd_slot(fildes).ok_or(Errno(EBADF))?;
        match cmd {
            F_GETFL => match *slot {
                FdSlot::Closed => Err(Errno(EBADF)),
                FdSlot::Socket { flags, .. } => Ok(if flags & FD_SOCK_NONBLOCK != 0 {
                    O_NONBLOCK
                } else {
                    0
                }),
                _ => Ok(0),
            },
            F_SETFL => match slot {
                FdSlot::Socket { flags, .. } => {
                    *flags = if (arg as c_int) & O_NONBLOCK != 0 {
                        FD_SOCK_NONBLOCK
                    } else {
                        0
                    };
                    Ok(0)
                }
                FdSlot::Closed => Err(Errno(EBADF)),
                _ => Err(Errno(EINVAL)),
            },
            // dup() is the trait default `fcntl(F_DUPFD, 0)`; std's try_clone lands here.
            F_DUPFD | F_DUPFD_CLOEXEC => {
                if matches!(*slot, FdSlot::Closed) {
                    return Err(Errno(EBADF));
                }
                let source = *slot;
                let Some(new) = fd_alloc_from(arg as c_int) else {
                    return Err(Errno(EMFILE));
                };
                *fd_slot(new).ok_or(Errno(EBADF))? = source;
                Ok(new)
            }
            _ => Err(Errno(EINVAL)),
        }
    }

    unsafe fn fork() -> Result<pid_t> {
        Err(unsupported())
    }

    fn fpath(fildes: c_int, out: &mut [u8]) -> Result<usize> {
        Err(unsupported())
    }

    fn fsync(fildes: c_int) -> Result<()> {
        use crate::header::errno::{EBADF, EIO};

        let Some(FdSlot::File { handle, .. }) = fd_slot(fildes).map(|slot| *slot) else {
            return Err(Errno(EBADF));
        };
        let mut request = [0u8; 5];
        request[0] = VFS_OP_FLUSH;
        request[1..5].copy_from_slice(&handle.to_le_bytes());
        let mut reply = [0u8; 1];
        let n = vfs_call(&request, &mut reply)?;
        if n != 1 || reply[0] != VFS_OK {
            return Err(if n == 0 {
                Errno(EIO)
            } else {
                vfs_errno(reply[0])
            });
        }
        Ok(())
    }

    fn ftruncate(fildes: c_int, length: off_t) -> Result<()> {
        use crate::header::errno::{EBADF, EINVAL, EIO};

        let length = u64::try_from(length).map_err(|_| Errno(EINVAL))?;
        let Some(FdSlot::File { handle, .. }) = fd_slot(fildes).map(|slot| *slot) else {
            return Err(Errno(EBADF));
        };
        let mut request = [0u8; 13];
        request[0] = VFS_OP_SET_LEN;
        request[1..5].copy_from_slice(&handle.to_le_bytes());
        request[5..13].copy_from_slice(&length.to_le_bytes());
        let mut reply = [0u8; 1];
        let n = vfs_call(&request, &mut reply)?;
        if n != 1 || reply[0] != VFS_OK {
            return Err(if n == 0 {
                Errno(EIO)
            } else {
                vfs_errno(reply[0])
            });
        }
        if let Some(FdSlot::File { size, .. }) = fd_slot(fildes) {
            *size = length;
        }
        Ok(())
    }

    /// KUMO's `FutexWait` compares the word and parks only on a match, reporting
    /// `ShouldWait` when the value had already moved on. It carries no deadline, so a
    /// timed wait is refused rather than silently waited forever. — TIMBERDOODLE
    unsafe fn futex_wait(addr: *mut u32, val: u32, deadline: Option<&timespec>) -> Result<()> {
        use crate::header::errno::{EAGAIN, EINVAL};

        if deadline.is_some() {
            return Err(unsupported());
        }
        let status = syscall4(SYSCALL_FUTEX_WAIT, addr as u64, u64::from(val), 0, 0) as i32;
        match status {
            KUMO_OK => Ok(()),
            KUMO_SHOULD_WAIT => Err(Errno(EAGAIN)),
            KUMO_NOT_SUPPORTED => Err(unsupported()),
            _ => Err(Errno(EINVAL)),
        }
    }

    /// `FutexWake` answers with the number of threads released; waking none is the normal
    /// uncontended case, not a failure. Negative KUMO statuses are the error space, and a
    /// woken count can never reach them. — TIMBERDOODLE
    unsafe fn futex_wake(addr: *mut u32, num: u32) -> Result<u32> {
        use crate::header::errno::EINVAL;

        let woken = syscall4(SYSCALL_FUTEX_WAKE, addr as u64, u64::from(num), 0, 0);
        match woken as i32 {
            KUMO_NOT_SUPPORTED => Err(unsupported()),
            status if status < 0 => Err(Errno(EINVAL)),
            _ => Ok(woken as u32),
        }
    }

    unsafe fn utimensat(
        dirfd: c_int,
        path: CStr,
        times: *const timespec,
        flag: c_int,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn getcwd(buf: Out<[u8]>) -> Result<()> {
        Err(unsupported())
    }

    fn getdents(fd: c_int, buf: &mut [u8], opaque_offset: u64) -> Result<usize> {
        Err(unsupported())
    }

    fn dir_seek(fd: c_int, opaque_offset: u64) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn dent_reclen_offset(this_dent: &[u8], offset: usize) -> Option<(u16, u64)> {
        None
    }

    fn getegid() -> gid_t {
        0
    }

    fn geteuid() -> uid_t {
        0
    }

    fn getgid() -> gid_t {
        0
    }

    fn getgroups(list: Out<[gid_t]>) -> Result<c_int> {
        Err(unsupported())
    }

    fn getpagesize() -> usize {
        PAGE_SIZE
    }

    fn getpgid(pid: pid_t) -> Result<pid_t> {
        Err(unsupported())
    }

    fn getpid() -> pid_t {
        0
    }

    fn getppid() -> pid_t {
        0
    }

    fn getpriority(which: c_int, who: id_t) -> Result<c_int> {
        Err(unsupported())
    }

    /// KUMO has no entropy object yet (recorded deviation): bytes are mixed from the
    /// monotonic clock and an internal counter, and that is not cryptographic. HashMap
    /// seeding is its consumer today; security-grade randomness is a later concern.
    fn getrandom(buf: &mut [u8], flags: c_uint) -> Result<usize> {
        use crate::header::errno::EINVAL;

        const GRND_NONBLOCK: c_uint = 1;
        const GRND_INSECURE: c_uint = 4;
        if flags & !(GRND_NONBLOCK | GRND_INSECURE) != 0 {
            return Err(Errno(EINVAL));
        }
        static STATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let mut state = STATE.load(Ordering::Relaxed);
        if state == 0 {
            let nanos = syscall4(SYSCALL_CLOCK_GET, 0, 0, 0, 0);
            state = nanos
                ^ (&raw const STATE as usize as u64).rotate_left(17)
                ^ 0x9e37_79b9_7f4a_7c15;
            if state == 0 {
                state = 0x9e37_79b9_7f4a_7c15;
            }
        }
        for byte in buf.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        STATE.store(state, Ordering::Relaxed);
        Ok(buf.len())
    }

    fn getresgid(
        rgid: Option<Out<gid_t>>,
        egid: Option<Out<gid_t>>,
        sgid: Option<Out<gid_t>>,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn getresuid(
        ruid: Option<Out<uid_t>>,
        euid: Option<Out<uid_t>>,
        suid: Option<Out<uid_t>>,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn getrlimit(resource: c_int, rlim: Out<rlimit>) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn setrlimit(resource: c_int, rlim: *const rlimit) -> Result<()> {
        Err(unsupported())
    }

    fn getrusage(who: c_int, r_usage: Out<rusage>) -> Result<()> {
        Err(unsupported())
    }

    fn getsid(pid: pid_t) -> Result<pid_t> {
        Err(unsupported())
    }

    fn gettid() -> pid_t {
        0
    }

    fn gettimeofday(tp: Out<timeval>, tzp: Option<Out<timezone>>) -> Result<()> {
        Err(unsupported())
    }

    fn getuid() -> uid_t {
        0
    }

    fn linkat(fd1: c_int, oldpath: CStr, fd2: c_int, newpath: CStr, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn lseek(fildes: c_int, offset: off_t, whence: c_int) -> Result<off_t> {
        Err(unsupported())
    }

    fn mkdirat(fildes: c_int, path: CStr, mode: mode_t) -> Result<()> {
        Err(unsupported())
    }

    fn mkfifoat(dir_fd: c_int, path: CStr, mode: mode_t) -> Result<()> {
        Err(unsupported())
    }

    fn mknodat(fildes: c_int, path: CStr, mode: mode_t, dev: dev_t) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn mlock(addr: *const c_void, len: usize) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn mlockall(flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fildes: c_int,
        off: off_t,
    ) -> Result<*mut c_void> {
        use crate::header::{
            errno::{EINVAL, ENOMEM},
            sys_mman::{MAP_ANON, MAP_PRIVATE, PROT_READ, PROT_WRITE},
        };

        let valid = addr.is_null()
            && len != 0
            && fildes == -1
            && off == 0
            && flags == MAP_ANON | MAP_PRIVATE
            && prot & !(PROT_READ | PROT_WRITE) == 0;
        if !valid {
            return Err(Errno(EINVAL));
        }
        let Some(len) = len
            .checked_add(PAGE_SIZE - 1)
            .map(|len| len & !(PAGE_SIZE - 1))
        else {
            return Err(Errno(ENOMEM));
        };
        let raw_vmo = syscall4(SYSCALL_VMO_CREATE, len as u64, 0, 0, 0);
        if raw_vmo == 0 || raw_vmo == u64::MAX {
            return Err(Errno(ENOMEM));
        }
        let vmo = raw_vmo as u32;
        let (status, address) = vmar_map_anywhere(vmo, len);
        close_handle(vmo);
        if status != 0 || address == 0 {
            return Err(Errno(ENOMEM));
        }
        Ok(address as *mut c_void)
    }

    unsafe fn mremap(
        addr: *mut c_void,
        len: usize,
        new_len: usize,
        flags: c_int,
        args: *mut c_void,
    ) -> Result<*mut c_void> {
        Err(unsupported())
    }

    unsafe fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn msync(addr: *mut c_void, len: usize, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn munlock(addr: *const c_void, len: usize) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn madvise(addr: *mut c_void, len: usize, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn munlockall() -> Result<()> {
        Err(unsupported())
    }

    unsafe fn munmap(addr: *mut c_void, len: usize) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> Result<()> {
        Err(unsupported())
    }

    /// Open a file through the granted filesystem server.
    ///
    /// Only `AT_FDCWD` is accepted, and a relative path resolves against the root of the
    /// granted filesystem (see `vfs_path`) until a working-directory capability exists. Access
    /// and create intent cross the vfs wire; no ambient write authority is inferred from an
    /// integer fd.
    fn openat(dirfd: c_int, path: CStr, oflag: c_int, mode: mode_t) -> Result<c_int> {
        use crate::header::{
            errno::{EINVAL, EMFILE, ENAMETOOLONG},
            fcntl::{AT_FDCWD, O_ACCMODE, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY},
        };

        if dirfd != AT_FDCWD {
            return Err(Errno(EINVAL));
        }
        let access = match oflag & O_ACCMODE {
            O_RDONLY => VFS_OPEN_READ,
            O_WRONLY => VFS_OPEN_WRITE,
            O_RDWR => VFS_OPEN_READ | VFS_OPEN_WRITE,
            _ => return Err(Errno(EINVAL)),
        };
        if oflag & O_TRUNC != 0 && access & VFS_OPEN_WRITE == 0 {
            return Err(Errno(EINVAL));
        }
        let options = access
            | if oflag & O_CREAT != 0 {
                VFS_OPEN_CREATE
            } else {
                0
            };
        let path = path.to_bytes();
        if path.is_empty() {
            return Err(Errno(ENAMETOOLONG));
        }
        let mut resolved = [0u8; VFS_MAX_PATH];
        let path = vfs_path(path, &mut resolved)?;
        let fildes = fd_alloc().ok_or(Errno(EMFILE))?;

        let mut request = [0u8; 4 + VFS_MAX_PATH];
        request[0] = VFS_OP_OPEN_OPTIONS;
        request[1] = options;
        request[2..4].copy_from_slice(&(path.len() as u16).to_le_bytes());
        request[4..4 + path.len()].copy_from_slice(path);
        let mut reply = [0u8; VFS_OPEN_REPLY_LEN];
        let n = vfs_call(&request[..4 + path.len()], &mut reply)?;

        if reply[0] != VFS_OK {
            return Err(vfs_errno(reply[0]));
        }
        if n < VFS_OPEN_REPLY_LEN {
            return Err(vfs_errno(u8::MAX));
        }
        let handle = u32::from_le_bytes(reply[1..5].try_into().unwrap());
        let size = u64::from_le_bytes(reply[5..13].try_into().unwrap());
        if let Some(slot) = fd_slot(fildes) {
            *slot = FdSlot::File {
                handle,
                offset: 0,
                size,
            };
        }
        if oflag & O_TRUNC != 0 {
            if let Err(error) = Self::ftruncate(fildes, 0) {
                if let Some(slot) = fd_slot(fildes) {
                    *slot = FdSlot::Closed;
                }
                return Err(error);
            }
        }
        Ok(fildes)
    }

    fn pipe2(fildes: Out<[c_int; 2]>, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn posix_fallocate(fd: c_int, offset: u64, length: NonZeroU64) -> Result<()> {
        Err(unsupported())
    }

    fn posix_getdents(fildes: c_int, buf: &mut [u8]) -> Result<usize> {
        Err(unsupported())
    }

    unsafe fn rlct_clone(
        stack: *mut usize,
        os_specific: &mut OsSpecific,
    ) -> Result<pthread::OsTid, Errno> {
        Err(unsupported())
    }

    unsafe fn rlct_kill(os_tid: pthread::OsTid, signal: usize) -> Result<()> {
        Err(unsupported())
    }

    fn current_os_tid() -> pthread::OsTid {
        pthread::OsTid::default()
    }

    /// Read from an input channel or an open file. Short reads are normal on both — the
    /// channel delivers one message at a time and the file server answers at most
    /// [`VFS_READ_MAX`] bytes per exchange — so callers loop, which is what stdio does.
    /// End of input is a zero-length read, not an error.
    fn read(fildes: c_int, buf: &mut [u8]) -> Result<usize> {
        use crate::header::errno::{EBADF, ENOTCONN};

        if buf.is_empty() {
            return Ok(0);
        }
        let slot = fd_slot(fildes).ok_or(Errno(EBADF))?;
        let (handle, offset, size) = match *slot {
            // A channel read parks until a message arrives, and answers 0 once the writer
            // is gone — which is exactly POSIX end-of-file, so it needs no translation.
            FdSlot::Input(handle) => {
                let n = channel_read_bytes(handle, buf);
                return if n == u64::MAX {
                    Err(Errno(EBADF))
                } else {
                    Ok((n as usize).min(buf.len()))
                };
            }
            FdSlot::File {
                handle,
                offset,
                size,
            } => (handle, offset, size),
            FdSlot::Socket { kind, .. } => {
                if kind != SOCKET_KIND_TCP {
                    return Err(Errno(ENOTCONN));
                }
                let info = socket_info(fildes)?;
                return stream_recv(
                    info,
                    info.2 & FD_SOCK_NONBLOCK != 0,
                    buf.as_mut_ptr(),
                    buf.len(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                );
            }
            _ => return Err(Errno(EBADF)),
        };
        if offset >= size {
            return Ok(0);
        }
        let want = buf
            .len()
            .min(VFS_READ_MAX)
            .min((size - offset).try_into().unwrap_or(usize::MAX));

        let mut request = [0u8; VFS_READ_REQUEST_LEN];
        request[0] = VFS_OP_READ;
        request[1..5].copy_from_slice(&handle.to_le_bytes());
        request[5..13].copy_from_slice(&offset.to_le_bytes());
        request[13..17].copy_from_slice(&(want as u32).to_le_bytes());
        let mut reply = [0u8; 1 + VFS_READ_MAX];
        let n = vfs_call(&request, &mut reply)?;

        if reply[0] != VFS_OK {
            return Err(vfs_errno(reply[0]));
        }
        let got = (n - 1).min(want);
        buf[..got].copy_from_slice(&reply[1..1 + got]);
        // Re-fetch the slot: the borrow above ended with the request, and this keeps the
        // cursor update in one obvious place.
        if let Some(FdSlot::File { offset, .. }) = fd_slot(fildes) {
            *offset += got as u64;
        }
        Ok(got)
    }

    fn pread(fildes: c_int, buf: &mut [u8], offset: off_t) -> Result<usize> {
        Err(unsupported())
    }

    fn readlinkat(dirfd: c_int, pathname: CStr, out: &mut [u8]) -> Result<usize> {
        Err(unsupported())
    }

    fn renameat2(
        old_dir: c_int,
        old_path: CStr,
        new_dir: c_int,
        new_path: CStr,
        flags: c_uint,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn sched_yield() -> Result<()> {
        Err(unsupported())
    }

    unsafe fn setgroups(size: size_t, list: *const gid_t) -> Result<()> {
        Err(unsupported())
    }

    fn setpgid(pid: pid_t, pgid: pid_t) -> Result<()> {
        Err(unsupported())
    }

    fn setpriority(which: c_int, who: id_t, prio: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn setresgid(rgid: gid_t, egid: gid_t, sgid: gid_t) -> Result<()> {
        Err(unsupported())
    }

    fn setresuid(ruid: uid_t, euid: uid_t, suid: uid_t) -> Result<()> {
        Err(unsupported())
    }

    fn setsid() -> Result<c_int> {
        Err(unsupported())
    }

    unsafe fn spawn(
        program: CStr,
        fac: Option<&crate::header::spawn::posix_spawn_file_actions_t>,
        fat: Option<&crate::header::spawn::posix_spawnattr_t>,
        argv: NulTerminated<*mut c_char>,
        envp: Option<NulTerminated<*mut c_char>>,
    ) -> Result<pid_t> {
        Err(unsupported())
    }

    fn symlinkat(path1: CStr, fd: c_int, path2: CStr) -> Result<()> {
        Err(unsupported())
    }

    fn sync() -> Result<()> {
        Err(unsupported())
    }

    fn timer_create(clock_id: clockid_t, evp: &sigevent, timerid: Out<timer_t>) -> Result<()> {
        Err(unsupported())
    }

    fn timer_delete(timerid: timer_t) -> Result<()> {
        Err(unsupported())
    }

    fn timer_gettime(timerid: timer_t, value: Out<itimerspec>) -> Result<()> {
        Err(unsupported())
    }

    fn timer_settime(
        timerid: timer_t,
        flags: c_int,
        value: &itimerspec,
        ovalue: Option<Out<itimerspec>>,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn umask(mask: mode_t) -> mode_t {
        0
    }

    fn uname(utsname: Out<utsname>) -> Result<()> {
        Err(unsupported())
    }

    fn unlinkat(fd: c_int, path: CStr, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn waitpid(pid: pid_t, stat_loc: Option<Out<c_int>>, options: c_int) -> Result<pid_t> {
        Err(unsupported())
    }

    /// Write through the channel or file capability this descriptor names. File writes are
    /// split into bounded inline vfs frames; one POSIX call still reports the accumulated
    /// result, and the per-process cursor advances only by acknowledged bytes.
    fn write(fildes: c_int, buf: &[u8]) -> Result<usize> {
        use crate::header::errno::{EBADF, EIO, ENOTCONN};

        if buf.is_empty() {
            return Ok(0);
        }
        match fd_slot(fildes).map(|slot| *slot) {
            Some(FdSlot::Output(handle)) if handle != 0 => {
                // A full-screen repaint can exceed ChannelWrite's 4096-byte
                // bound. A successful message does not acknowledge the whole
                // POSIX buffer; report only committed progress. — KESTREL
                let mut written = 0;
                for chunk in buf.chunks(4096) {
                    if channel_write_bytes(handle, chunk) != 0 {
                        return if written == 0 { Err(Errno(EIO)) } else { Ok(written) };
                    }
                    written += chunk.len();
                }
                Ok(written)
            }
            Some(FdSlot::File {
                handle,
                mut offset,
                mut size,
            }) => {
                let mut total = 0usize;
                while total < buf.len() {
                    let take = (buf.len() - total).min(VFS_READ_MAX);
                    let mut request = [0u8; VFS_WRITE_HEADER_LEN + VFS_READ_MAX];
                    request[0] = VFS_OP_WRITE;
                    request[1..5].copy_from_slice(&handle.to_le_bytes());
                    request[5..13].copy_from_slice(&offset.to_le_bytes());
                    request[13..15].copy_from_slice(&(take as u16).to_le_bytes());
                    request[VFS_WRITE_HEADER_LEN..VFS_WRITE_HEADER_LEN + take]
                        .copy_from_slice(&buf[total..total + take]);
                    let mut reply = [0u8; VFS_WRITE_REPLY_LEN];
                    let result = vfs_call(
                        &request[..VFS_WRITE_HEADER_LEN + take],
                        &mut reply,
                    );
                    let n = match result {
                        Ok(n) => n,
                        Err(error) if total == 0 => return Err(error),
                        Err(_) => break,
                    };
                    if n != VFS_WRITE_REPLY_LEN || reply[0] != VFS_OK {
                        if total == 0 {
                            return Err(if n == 0 {
                                Errno(EIO)
                            } else {
                                vfs_errno(reply[0])
                            });
                        }
                        break;
                    }
                    let written =
                        u32::from_le_bytes(reply[1..5].try_into().unwrap()) as usize;
                    if written == 0 || written > take {
                        if total == 0 {
                            return Err(Errno(EIO));
                        }
                        break;
                    }
                    total += written;
                    offset += written as u64;
                    size = size.max(offset);
                    if written < take {
                        break;
                    }
                }
                if let Some(slot) = fd_slot(fildes) {
                    *slot = FdSlot::File {
                        handle,
                        offset,
                        size,
                    };
                }
                Ok(total)
            }
            Some(FdSlot::Socket { kind, .. }) if kind == SOCKET_KIND_TCP => {
                let info = socket_info(fildes)?;
                stream_send(info, buf)
            }
            Some(FdSlot::Socket { .. }) => Err(Errno(ENOTCONN)),
            _ => Err(Errno(EBADF)),
        }
    }

    fn pwrite(fildes: c_int, buf: &[u8], offset: off_t) -> Result<usize> {
        Err(unsupported())
    }

    fn verify() -> bool {
        true
    }
}

impl PalEpoll for Sys {
    fn epoll_create1(flags: c_int) -> Result<c_int> {
        Err(unsupported())
    }

    unsafe fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn epoll_pwait(
        epfd: c_int,
        events: *mut epoll_event,
        maxevents: c_int,
        timeout: c_int,
        sigmask: *const sigset_t,
    ) -> Result<usize> {
        Err(unsupported())
    }
}

impl PalPtrace for Sys {
    unsafe fn ptrace(
        request: c_int,
        pid: pid_t,
        addr: *mut c_void,
        data: *mut c_void,
    ) -> Result<c_int> {
        Err(unsupported())
    }
}

impl PalSignal for Sys {
    fn getitimer(which: c_int, out: &mut itimerval) -> Result<()> {
        Err(unsupported())
    }

    fn kill(pid: pid_t, sig: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn sigqueue(pid: pid_t, sig: c_int, val: sigval) -> Result<()> {
        Err(unsupported())
    }

    fn killpg(pgrp: pid_t, sig: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn raise(sig: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn setitimer(which: c_int, new: &itimerval, old: Option<&mut itimerval>) -> Result<()> {
        Err(unsupported())
    }

    /// KUMO delivers no signals, so every disposition is vacuously honored: the query
    /// reports SIG_DFL and the set succeeds. std's runtime asserts on `signal(SIGPIPE)`
    /// during init, and an honest "no signals exist" answer is success, not an error.
    fn sigaction(sig: c_int, act: Option<&sigaction>, oact: Option<&mut sigaction>) -> Result<()> {
        let _ = (sig, act);
        if let Some(old) = oact {
            *old = unsafe { core::mem::zeroed() };
            // The zeroed struct already carries SIG_DFL at the handler position.
            old.sa_handler = None;
        }
        Ok(())
    }

    unsafe fn sigaltstack(ss: Option<&stack_t>, old_ss: Option<&mut stack_t>) -> Result<()> {
        Err(unsupported())
    }

    fn sigpending(set: &mut sigset_t) -> Result<()> {
        Err(unsupported())
    }

    fn sigprocmask(how: c_int, set: Option<&sigset_t>, oset: Option<&mut sigset_t>) -> Result<()> {
        Err(unsupported())
    }

    fn sigsuspend(mask: &sigset_t) -> Errno {
        unsupported()
    }

    fn sigtimedwait(
        set: &sigset_t,
        sig: Option<&mut siginfo_t>,
        tp: Option<&timespec>,
    ) -> Result<c_int> {
        Err(unsupported())
    }
}

impl PalSocket for Sys {
    unsafe fn accept(
        socket: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<c_int> {
        use crate::header::errno::{EAGAIN, EMFILE};

        let info = socket_info(socket)?;
        let (id, _kind, flags, _rcv, _snd) = info;
        loop {
            let (status, new_id, _result, payload_len) = net_call(KNS1_OP_ACCEPT, id, 0, 0, &[])?;
            match status {
                0 => {
                    let Some(fildes) = fd_alloc() else {
                        let _ = net_call(KNS1_OP_CLOSE, new_id, 0, 0, &[]);
                        return Err(Errno(EMFILE));
                    };
                    if let Some(slot) = fd_slot(fildes) {
                        *slot = FdSlot::Socket {
                            id: new_id,
                            kind: SOCKET_KIND_TCP,
                            flags: 0,
                            rcv_timeo_ms: 0,
                            snd_timeo_ms: 0,
                        };
                    }
                    if !address.is_null() && !address_len.is_null() && payload_len >= KNS1_ADDR_LEN {
                        let message = unsafe { (&raw const NET_MESSAGE).as_ref().unwrap() };
                        let base = KNS1_HEADER_LEN;
                        let filled = sockaddr_in_fill(
                            [
                                message[base + 4],
                                message[base + 5],
                                message[base + 6],
                                message[base + 7],
                            ],
                            le16(message, base + 2),
                        );
                        let cap = unsafe { (*address_len as usize).min(SOCKADDR_IN_LEN) };
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                filled.as_ptr(),
                                address.cast::<u8>(),
                                cap,
                            );
                            *address_len = SOCKADDR_IN_LEN as socklen_t;
                        }
                    }
                    return Ok(fildes);
                }
                13 => {
                    if flags & FD_SOCK_NONBLOCK != 0 {
                        return Err(Errno(EAGAIN));
                    }
                    wait_interest(id, KNS1_WAIT_CONNECTED, 0)?;
                }
                other => return Err(net_status_errno(other)),
            }
        }
    }

    unsafe fn bind(socket: c_int, address: *const sockaddr, address_len: socklen_t) -> Result<()> {
        let (id, _kind, _flags, _rcv, _snd) = socket_info(socket)?;
        let (ip, port) = unsafe { sockaddr_in_parse(address, address_len) }?;
        let mut payload = [0u8; KNS1_ADDR_LEN];
        payload[0..2].copy_from_slice(&KNS1_FAMILY_IPV4.to_le_bytes());
        payload[2..4].copy_from_slice(&port.to_le_bytes());
        payload[4..8].copy_from_slice(&ip);
        let (status, _reply_socket, _result, _len) = net_call(KNS1_OP_BIND, id, 0, 0, &payload)?;
        net_status_ok(status)
    }

    /// Stream connect. The session answers InProgress while the handshake runs;
    /// nonblocking callers get EINPROGRESS and may poll through getsockopt(SO_ERROR),
    /// blocking callers wait CONNECTED in one bounded 60 s window.
    unsafe fn connect(
        socket: c_int,
        address: *const sockaddr,
        address_len: socklen_t,
    ) -> Result<c_int> {
        use crate::header::errno::{EINPROGRESS, EOPNOTSUPP, ETIMEDOUT};

        let (id, kind, flags, _rcv, _snd) = socket_info(socket)?;
        if kind != SOCKET_KIND_TCP {
            return Err(Errno(EOPNOTSUPP));
        }
        let (ip, port) = unsafe { sockaddr_in_parse(address, address_len) }?;
        let mut payload = [0u8; KNS1_ADDR_LEN];
        payload[0..2].copy_from_slice(&KNS1_FAMILY_IPV4.to_le_bytes());
        payload[2..4].copy_from_slice(&port.to_le_bytes());
        payload[4..8].copy_from_slice(&ip);
        let (status, _reply_socket, _result, _len) = net_call(KNS1_OP_CONNECT, id, 0, 0, &payload)?;
        match status {
            0 => Ok(0),
            14 => {
                if flags & FD_SOCK_NONBLOCK != 0 {
                    return Err(Errno(EINPROGRESS));
                }
                let (wait_status, _s, _r, _l) = net_call(
                    KNS1_OP_WAIT,
                    id,
                    u64::from(KNS1_WAIT_CONNECTED) | (60_000u64 << 32),
                    0,
                    &[],
                )?;
                match wait_status {
                    0 => Ok(0),
                    15 => Err(Errno(ETIMEDOUT)),
                    other => Err(net_status_errno(other)),
                }
            }
            other => Err(net_status_errno(other)),
        }
    }

    unsafe fn getpeername(
        socket: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<()> {
        use crate::header::errno::ENOTCONN;

        let (id, _kind, _flags, _rcv, _snd) = socket_info(socket)?;
        let (status, _s, _r, payload_len) = net_call(KNS1_OP_GET_NAME, id, 0, 0, &[])?;
        net_status_ok(status)?;
        if payload_len < 2 * KNS1_ADDR_LEN {
            return Err(Errno(crate::header::errno::EIO));
        }
        let message = unsafe { (&raw const NET_MESSAGE).as_ref().unwrap() };
        let base = KNS1_HEADER_LEN + KNS1_ADDR_LEN;
        if le16(message, base + 2) == 0 {
            return Err(Errno(ENOTCONN));
        }
        if address.is_null() || address_len.is_null() {
            return Ok(());
        }
        let filled = sockaddr_in_fill(
            [
                message[base + 4],
                message[base + 5],
                message[base + 6],
                message[base + 7],
            ],
            le16(message, base + 2),
        );
        let cap = unsafe { (*address_len as usize).min(SOCKADDR_IN_LEN) };
        unsafe {
            core::ptr::copy_nonoverlapping(filled.as_ptr(), address.cast::<u8>(), cap);
            *address_len = SOCKADDR_IN_LEN as socklen_t;
        }
        Ok(())
    }

    unsafe fn getsockname(
        socket: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<()> {
        let (id, _kind, _flags, _rcv, _snd) = socket_info(socket)?;
        let (status, _s, _r, payload_len) = net_call(KNS1_OP_GET_NAME, id, 0, 0, &[])?;
        net_status_ok(status)?;
        if payload_len < 2 * KNS1_ADDR_LEN {
            return Err(Errno(crate::header::errno::EIO));
        }
        if address.is_null() || address_len.is_null() {
            return Ok(());
        }
        let message = unsafe { (&raw const NET_MESSAGE).as_ref().unwrap() };
        let base = KNS1_HEADER_LEN;
        let filled = sockaddr_in_fill(
            [
                message[base + 4],
                message[base + 5],
                message[base + 6],
                message[base + 7],
            ],
            le16(message, base + 2),
        );
        let cap = unsafe { (*address_len as usize).min(SOCKADDR_IN_LEN) };
        unsafe {
            core::ptr::copy_nonoverlapping(filled.as_ptr(), address.cast::<u8>(), cap);
            *address_len = SOCKADDR_IN_LEN as socklen_t;
        }
        Ok(())
    }

    /// SO_RCVTIMEO / SO_SNDTIMEO are real (stored and honored) and SO_ERROR reports the
    /// pending connect error; anything else refuses by name (ENOPROTOOPT).
    unsafe fn getsockopt(
        socket: c_int,
        level: c_int,
        option_name: c_int,
        option_value: *mut c_void,
        option_len: *mut socklen_t,
    ) -> Result<()> {
        use crate::header::errno::{EINPROGRESS, EINVAL, ENOPROTOOPT};
        use crate::header::sys_socket::constants::{
            SO_ERROR, SO_RCVTIMEO, SO_SNDTIMEO, SOL_SOCKET,
        };

        let (id, _kind, _flags, rcv_timeo, snd_timeo) = socket_info(socket)?;
        if option_value.is_null() || option_len.is_null() {
            return Err(Errno(EINVAL));
        }
        if level == SOL_SOCKET && (option_name == SO_RCVTIMEO || option_name == SO_SNDTIMEO) {
            let ms = if option_name == SO_RCVTIMEO {
                rcv_timeo
            } else {
                snd_timeo
            };
            let value = timeval {
                tv_sec: time_t::from(ms / 1000),
                // u32 arithmetic: u16 microsecond math wraps (300 * 1000 > 65535).
                tv_usec: (u32::from(ms % 1000) * 1000) as suseconds_t,
            };
            let cap = unsafe { (*option_len as usize).min(core::mem::size_of::<timeval>()) };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (&raw const value).cast::<u8>(),
                    option_value.cast::<u8>(),
                    cap,
                );
                *option_len = core::mem::size_of::<timeval>() as socklen_t;
            }
            return Ok(());
        }
        if level == SOL_SOCKET && option_name == SO_ERROR {
            // Zero when established, EINPROGRESS while the handshake runs, and the
            // session's own refusal otherwise: never a fake zero.
            let (status, _s, result, _l) = net_call(
                KNS1_OP_WAIT,
                id,
                u64::from(KNS1_WAIT_CONNECTED),
                0,
                &[],
            )?;
            let value: c_int = match status {
                0 if result & u64::from(KNS1_WAIT_CONNECTED) != 0 => 0,
                0 => EINPROGRESS,
                9 => crate::header::errno::ECONNREFUSED,
                other => net_status_errno(other).0,
            };
            let cap = unsafe { (*option_len as usize).min(core::mem::size_of::<c_int>()) };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (&raw const value).cast::<u8>(),
                    option_value.cast::<u8>(),
                    cap,
                );
                *option_len = core::mem::size_of::<c_int>() as socklen_t;
            }
            return Ok(());
        }
        Err(Errno(ENOPROTOOPT))
    }

    fn listen(socket: c_int, backlog: c_int) -> Result<()> {
        use crate::header::errno::EOPNOTSUPP;

        let (id, kind, _flags, _rcv, _snd) = socket_info(socket)?;
        if kind != SOCKET_KIND_TCP {
            return Err(Errno(EOPNOTSUPP));
        }
        let backlog = backlog.clamp(1, KNS1_LISTEN_BACKLOG_MAX as c_int) as u64;
        let (status, _reply_socket, _result, _len) =
            net_call(KNS1_OP_LISTEN, id, backlog, 0, &[])?;
        net_status_ok(status)
    }

    unsafe fn recvfrom(
        socket: c_int,
        buf: *mut c_void,
        len: size_t,
        flags: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<usize> {
        use crate::header::errno::{EAGAIN, EFAULT, EIO};
        use crate::header::sys_socket::constants::MSG_DONTWAIT;

        let info = socket_info(socket)?;
        if len != 0 && buf.is_null() {
            return Err(Errno(EFAULT));
        }
        if len == 0 {
            return Ok(0);
        }
        if info.1 == SOCKET_KIND_TCP {
            return stream_recv(
                info,
                flags & MSG_DONTWAIT != 0 || info.2 & FD_SOCK_NONBLOCK != 0,
                buf.cast::<u8>(),
                len,
                address,
                address_len,
            );
        }
        let id = info.0;
        let max = len.min(KNS1_UDP_PAYLOAD_MAX);
        let nonblock = flags & MSG_DONTWAIT != 0 || info.2 & FD_SOCK_NONBLOCK != 0;
        loop {
            let (status, _reply_socket, _result, payload_len) =
                net_call(KNS1_OP_RECV_FROM, id, max as u64, 0, &[])?;
            if status == 13 {
                if nonblock {
                    return Err(Errno(EAGAIN));
                }
                // The descriptor's SO_RCVTIMEO bounds the wait (0 blocks; expiry is
                // EAGAIN) — the same rule the stream path follows.
                wait_interest(id, KNS1_WAIT_READABLE, info.3)?;
                continue;
            }
            net_status_ok(status)?;
            if payload_len < KNS1_ADDR_LEN {
                return Err(Errno(EIO));
            }
            let message = unsafe { (&raw const NET_MESSAGE).as_ref().unwrap() };
            let payload = &message[KNS1_HEADER_LEN..KNS1_HEADER_LEN + payload_len];
            let data = &payload[KNS1_ADDR_LEN..];
            let take = data.len().min(max);
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), buf.cast::<u8>(), take) };
            if !address.is_null() && !address_len.is_null() {
                let filled = sockaddr_in_fill(
                    [payload[4], payload[5], payload[6], payload[7]],
                    le16(payload, 2),
                );
                let cap = unsafe { (*address_len as usize).min(SOCKADDR_IN_LEN) };
                unsafe {
                    core::ptr::copy_nonoverlapping(filled.as_ptr(), address.cast::<u8>(), cap);
                    *address_len = SOCKADDR_IN_LEN as socklen_t;
                }
            }
            return Ok(take);
        }
    }

    unsafe fn recvmsg(socket: c_int, msg: *mut msghdr, flags: c_int) -> Result<usize> {
        let _ = (socket, msg, flags);
        Err(unsupported())
    }

    unsafe fn sendmsg(socket: c_int, msg: *const msghdr, flags: c_int) -> Result<usize> {
        let _ = (socket, msg, flags);
        Err(unsupported())
    }

    unsafe fn sendto(
        socket: c_int,
        buf: *const c_void,
        len: size_t,
        flags: c_int,
        dest_addr: *const sockaddr,
        dest_len: socklen_t,
    ) -> Result<usize> {
        use crate::header::errno::{EDESTADDRREQ, EISCONN, EMSGSIZE, EFAULT};

        let _ = flags;
        let info = socket_info(socket)?;
        if len != 0 && buf.is_null() {
            return Err(Errno(EFAULT));
        }
        if info.1 == SOCKET_KIND_TCP {
            if !dest_addr.is_null() && dest_len != 0 {
                return Err(Errno(EISCONN));
            }
            let data = if len == 0 {
                &[][..]
            } else {
                unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), len) }
            };
            return stream_send(info, data);
        }
        if len > KNS1_UDP_PAYLOAD_MAX {
            return Err(Errno(EMSGSIZE));
        }
        if dest_addr.is_null() || dest_len == 0 {
            return Err(Errno(EDESTADDRREQ));
        }
        let (ip, port) = unsafe { sockaddr_in_parse(dest_addr, dest_len) }?;
        let data = if len == 0 {
            &[][..]
        } else {
            unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), len) }
        };
        let mut payload = [0u8; KNS1_ADDR_LEN + KNS1_UDP_PAYLOAD_MAX];
        payload[0..2].copy_from_slice(&KNS1_FAMILY_IPV4.to_le_bytes());
        payload[2..4].copy_from_slice(&port.to_le_bytes());
        payload[4..8].copy_from_slice(&ip);
        payload[KNS1_ADDR_LEN..KNS1_ADDR_LEN + data.len()].copy_from_slice(data);
        let (status, _reply_socket, _result, _payload_len) =
            net_call(KNS1_OP_SEND_TO, info.0, 0, 0, &payload[..KNS1_ADDR_LEN + data.len()])?;
        net_status_ok(status)?;
        Ok(data.len())
    }

    unsafe fn setsockopt(
        socket: c_int,
        level: c_int,
        option_name: c_int,
        option_value: *const c_void,
        option_len: socklen_t,
    ) -> Result<()> {
        use crate::header::errno::{EINVAL, ENOPROTOOPT};
        use crate::header::sys_socket::constants::{
            SO_RCVTIMEO, SO_REUSEADDR, SO_SNDTIMEO, SOL_SOCKET,
        };

        let _ = socket_info(socket)?;
        if level == SOL_SOCKET && option_name == SO_REUSEADDR {
            // The stack releases the port when the socket closes, so binding a recently
            // closed port already succeeds: the reuse flag's guarantee is vacuous here.
            return Ok(());
        }
        if level == SOL_SOCKET && (option_name == SO_RCVTIMEO || option_name == SO_SNDTIMEO) {
            if option_value.is_null() || (option_len as usize) < core::mem::size_of::<timeval>() {
                return Err(Errno(EINVAL));
            }
            let value = unsafe { core::ptr::read_unaligned(option_value.cast::<timeval>()) };
            let secs = u32::try_from(value.tv_sec.max(0)).unwrap_or(u32::MAX);
            let usecs = u32::try_from(value.tv_usec.max(0)).unwrap_or(0);
            let ms = secs
                .saturating_mul(1000)
                .saturating_add(usecs / 1000)
                .min(u32::from(u16::MAX)) as u16;
            let slot = fd_slot(socket).ok_or(Errno(EINVAL))?;
            match slot {
                FdSlot::Socket {
                    rcv_timeo_ms,
                    snd_timeo_ms,
                    ..
                } => {
                    if option_name == SO_RCVTIMEO {
                        *rcv_timeo_ms = ms;
                    } else {
                        *snd_timeo_ms = ms;
                    }
                    Ok(())
                }
                _ => Err(Errno(EINVAL)),
            }
        } else {
            Err(Errno(ENOPROTOOPT))
        }
    }

    fn shutdown(socket: c_int, how: c_int) -> Result<()> {
        use crate::header::errno::{EINVAL, ENOTCONN};

        let (id, kind, _flags, _rcv, _snd) = socket_info(socket)?;
        if kind != SOCKET_KIND_TCP {
            return Err(Errno(ENOTCONN));
        }
        // POSIX how: 0 = SHUT_RD, 1 = SHUT_WR, 2 = SHUT_RDWR; KNS1 bits: 1 read, 2 write.
        let bits = match how {
            0 => 1u64,
            1 => 2u64,
            2 => 3u64,
            _ => return Err(Errno(EINVAL)),
        };
        let (status, _reply_socket, _result, _len) =
            net_call(KNS1_OP_SHUTDOWN, id, bits, 0, &[])?;
        net_status_ok(status)
    }

    unsafe fn socket(domain: c_int, kind: c_int, protocol: c_int) -> Result<c_int> {
        use crate::header::errno::{EAFNOSUPPORT, EMFILE, EPROTONOSUPPORT};
        use crate::header::sys_socket::constants::{
            AF_INET, SOCK_CLOEXEC, SOCK_DGRAM, SOCK_NONBLOCK, SOCK_STREAM,
        };

        if domain != AF_INET {
            return Err(Errno(EAFNOSUPPORT));
        }
        let nonblock = kind & SOCK_NONBLOCK != 0;
        let base = kind & !(SOCK_NONBLOCK | SOCK_CLOEXEC);
        let (kns1_kind, fd_kind, proto) = if base == SOCK_DGRAM {
            (KNS1_SOCKET_UDP, SOCKET_KIND_UDP, KNS1_IPPROTO_UDP)
        } else if base == SOCK_STREAM {
            (KNS1_SOCKET_TCP, SOCKET_KIND_TCP, KNS1_IPPROTO_TCP)
        } else {
            return Err(Errno(EPROTONOSUPPORT));
        };
        if protocol != 0 && protocol != proto {
            return Err(Errno(EPROTONOSUPPORT));
        }
        let (status, id, _result, _len) = net_call(KNS1_OP_SOCKET, 0, kns1_kind, 0, &[])?;
        net_status_ok(status)?;
        let Some(fildes) = fd_alloc() else {
            let _ = net_call(KNS1_OP_CLOSE, id, 0, 0, &[]);
            return Err(Errno(EMFILE));
        };
        if let Some(slot) = fd_slot(fildes) {
            *slot = FdSlot::Socket {
                id,
                kind: fd_kind,
                flags: if nonblock { FD_SOCK_NONBLOCK } else { 0 },
                rcv_timeo_ms: 0,
                snd_timeo_ms: 0,
            };
        }
        Ok(fildes)
    }

    fn socketpair(domain: c_int, kind: c_int, protocol: c_int, sv: &mut [c_int; 2]) -> Result<()> {
        let _ = (domain, kind, protocol, sv);
        Err(unsupported())
    }
}
