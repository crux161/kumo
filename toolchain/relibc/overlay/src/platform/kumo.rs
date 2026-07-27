//j505
//j506
//j507
//j512
//j513
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

// The `kumo-vfs` wire protocol. relibc is built from its own vendored source tree by a
// separate cargo invocation, so it cannot depend on the `kumo-vfs` crate; these constants
// are the second copy of that protocol and must move with it. The acceptance transcript
// is what proves the two ends still agree. — TIMBERDOODLE
const VFS_OP_OPEN: u8 = 0x00;
const VFS_OP_READ: u8 = 0x01;
const VFS_OK: u8 = 0x00;
const VFS_NOT_FOUND: u8 = 0x01;
const VFS_IS_DIR: u8 = 0x03;
const VFS_MAX_PATH: usize = 255;
const VFS_READ_REQUEST_LEN: usize = 17;
const VFS_OPEN_REPLY_LEN: usize = 13;
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
/// Arguments the startup image has room for, matching the launcher's own argv ceiling.
const ARGV_MAX: usize = 16;
/// `argc`, `argv` and its terminator, an empty `envp`, then five auxiliary-vector pairs.
const STARTUP_WORDS: usize = 1 + (ARGV_MAX + 1) + 1 + 10;
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
}

/// Descriptors this backend can hold. POSIX programs expect 0/1/2 to be stdio, so the table
/// is indexed directly by the descriptor number.
const FD_SLOTS: usize = 16;

/// Written by the startup thread before `main`, then only by the calling thread's own
/// `open`/`read`/`close`. A second thread sharing descriptors needs the kernel to own the
/// thread pointer first (J507), so this is not yet a lock's worth of contention.
static mut FD_TABLE: [FdSlot; FD_SLOTS] = [FdSlot::Closed; FD_SLOTS];

/// The packed argv page as delivered, kept for the lifetime of the process because the
/// `argv` pointers relibc publishes borrow the strings in place.
static mut ARGV_IMAGE: [u8; ARGV_BYTES] = [0; ARGV_BYTES];

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

/// Map a vfs status byte onto the closest honest errno.
fn vfs_errno(status: u8) -> Errno {
    use crate::header::errno::{EIO, EISDIR, ENOENT};

    match status {
        VFS_NOT_FOUND => Errno(ENOENT),
        VFS_IS_DIR => Errno(EISDIR),
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
fn take_bootstrap_capabilities(bootstrap: u32) -> u32 {
    let mut argv_vmo = 0_u32;
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
    argv_vmo
}

/// Lay KUMO's granted capabilities out as the SysV initial-process image.
///
/// KUMO packs argv as `[argc: u32 LE][arg0]\0[arg1]\0 …` into one read-only VMO, so the
/// copied page can back the `argv` pointers directly. `envp` is deliberately a lone
/// terminator: KUMO grants no environment capability yet, and inventing one here would be
/// ambient authority wearing a POSIX name. `AT_PHDR`/`AT_PHENT`/`AT_PHNUM` are read from
/// this image's own mapped ELF header, which is how relibc's static TLS setup finds
/// `PT_TLS` without the kernel loader growing a POSIX opinion. — TIMBERDOODLE
unsafe fn build_startup_image(argv_vmo: u32) -> &'static Stack {
    unsafe extern "C" {
        /// Defined by the linker when the ELF header is covered by a `PT_LOAD` segment.
        static __ehdr_start: [u8; 64];
    }

    let image = (&raw mut STARTUP_IMAGE).cast::<usize>();
    let argv_bytes = (&raw mut ARGV_IMAGE).cast::<u8>();

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

    unsafe {
        image.write(argc);
        image.add(1 + argc).write(0);
        image.add(2 + argc).write(0);
    }

    let ehdr = (&raw const __ehdr_start).cast::<u8>();
    if unsafe { core::ptr::read_unaligned(ehdr.cast::<[u8; 4]>()) } != *b"\x7fELF" {
        startup_fail(b"relibc: no mapped ELF header for KUMO startup\n");
    }
    let phoff = unsafe { core::ptr::read_unaligned(ehdr.add(32).cast::<u64>()) } as usize;
    let phentsize = unsafe { core::ptr::read_unaligned(ehdr.add(54).cast::<u16>()) } as usize;
    let phnum = unsafe { core::ptr::read_unaligned(ehdr.add(56).cast::<u16>()) } as usize;

    let auxv = 3 + argc;
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

    let argv_vmo = take_bootstrap_capabilities(bootstrap as u32);
    let stack = unsafe { build_startup_image(argv_vmo) };
    close_handle(argv_vmo);
    unsafe { crate::start::relibc_start_v1(stack, main) }
}

impl Sys {
    pub unsafe fn ioctl(fd: c_int, request: c_ulong, out: *mut c_void) -> Result<c_int> {
        use crate::header::{
            errno::{EBADF, EFAULT, EIO, ENOTTY},
            sys_ioctl::{TCGETS, TCSETS, TCSETSF, TCSETSW, TIOCGWINSZ},
        };

        match fd_slot(fd).map(|slot| *slot) {
            None | Some(FdSlot::Closed) => return Err(Errno(EBADF)),
            Some(FdSlot::File { .. }) => return Err(Errno(ENOTTY)),
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

    fn clock_getres(clk_id: clockid_t, tp: Option<Out<timespec>>) -> Result<()> {
        Err(unsupported())
    }

    fn clock_gettime(clk_id: clockid_t, tp: Out<timespec>) -> Result<()> {
        Err(unsupported())
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
        if matches!(slot, FdSlot::Closed) {
            return Err(Errno(EBADF));
        }
        *slot = FdSlot::Closed;
        Ok(())
    }

    fn dup2(fildes: c_int, fildes2: c_int) -> Result<c_int> {
        Err(unsupported())
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

    fn fcntl(fildes: c_int, cmd: c_int, arg: c_ulonglong) -> Result<c_int> {
        Err(unsupported())
    }

    unsafe fn fork() -> Result<pid_t> {
        Err(unsupported())
    }

    fn fpath(fildes: c_int, out: &mut [u8]) -> Result<usize> {
        Err(unsupported())
    }

    fn fsync(fildes: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn ftruncate(fildes: c_int, length: off_t) -> Result<()> {
        Err(unsupported())
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

    fn getrandom(buf: &mut [u8], flags: c_uint) -> Result<usize> {
        Err(unsupported())
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
    /// The backing volume is read-only, so a write mode is refused with `EROFS` rather
    /// than accepted and then failed at the first `write`. Relative paths have nowhere to
    /// resolve against until a working-directory capability exists, so only `AT_FDCWD`
    /// with an absolute path is accepted. — TIMBERDOODLE
    fn openat(dirfd: c_int, path: CStr, oflag: c_int, mode: mode_t) -> Result<c_int> {
        use crate::header::{
            errno::{EINVAL, EMFILE, ENAMETOOLONG, EROFS},
            fcntl::{AT_FDCWD, O_ACCMODE, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY},
        };

        if dirfd != AT_FDCWD {
            return Err(Errno(EINVAL));
        }
        if oflag & O_ACCMODE != O_RDONLY || oflag & (O_CREAT | O_TRUNC | O_WRONLY) != 0 {
            return Err(Errno(EROFS));
        }
        let path = path.to_bytes();
        if path.is_empty() || path.len() > VFS_MAX_PATH {
            return Err(Errno(ENAMETOOLONG));
        }
        if path[0] != b'/' {
            return Err(Errno(EINVAL));
        }
        let fildes = fd_alloc().ok_or(Errno(EMFILE))?;

        let mut request = [0u8; 3 + VFS_MAX_PATH];
        request[0] = VFS_OP_OPEN;
        request[1..3].copy_from_slice(&(path.len() as u16).to_le_bytes());
        request[3..3 + path.len()].copy_from_slice(path);
        let mut reply = [0u8; VFS_OPEN_REPLY_LEN];
        let n = vfs_call(&request[..3 + path.len()], &mut reply)?;

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
        use crate::header::errno::EBADF;

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

    /// Write through the channel this descriptor names. Since J508 that lookup is the one
    /// FD table rather than a pair of special cases, so a file descriptor is refused here
    /// for the same reason an unopened one is: the table says it is not an output.
    fn write(fildes: c_int, buf: &[u8]) -> Result<usize> {
        use crate::header::errno::{EBADF, EIO};

        if buf.is_empty() {
            return Ok(0);
        }
        let Some(FdSlot::Output(handle)) = fd_slot(fildes).map(|slot| *slot) else {
            return Err(Errno(EBADF));
        };
        if handle == 0 {
            return Err(Errno(EBADF));
        }
        if channel_write_bytes(handle, buf) == 0 {
            Ok(buf.len())
        } else {
            Err(Errno(EIO))
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

    fn sigaction(sig: c_int, act: Option<&sigaction>, oact: Option<&mut sigaction>) -> Result<()> {
        Err(unsupported())
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
        Err(unsupported())
    }

    unsafe fn bind(socket: c_int, address: *const sockaddr, address_len: socklen_t) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn connect(
        socket: c_int,
        address: *const sockaddr,
        address_len: socklen_t,
    ) -> Result<c_int> {
        Err(unsupported())
    }

    unsafe fn getpeername(
        socket: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn getsockname(
        socket: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn getsockopt(
        socket: c_int,
        level: c_int,
        option_name: c_int,
        option_value: *mut c_void,
        option_len: *mut socklen_t,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn listen(socket: c_int, backlog: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn recvfrom(
        socket: c_int,
        buf: *mut c_void,
        len: size_t,
        flags: c_int,
        address: *mut sockaddr,
        address_len: *mut socklen_t,
    ) -> Result<usize> {
        Err(unsupported())
    }

    unsafe fn recvmsg(socket: c_int, msg: *mut msghdr, flags: c_int) -> Result<usize> {
        Err(unsupported())
    }

    unsafe fn sendmsg(socket: c_int, msg: *const msghdr, flags: c_int) -> Result<usize> {
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
        Err(unsupported())
    }

    unsafe fn setsockopt(
        socket: c_int,
        level: c_int,
        option_name: c_int,
        option_value: *const c_void,
        option_len: socklen_t,
    ) -> Result<()> {
        Err(unsupported())
    }

    fn shutdown(socket: c_int, how: c_int) -> Result<()> {
        Err(unsupported())
    }

    unsafe fn socket(domain: c_int, kind: c_int, protocol: c_int) -> Result<c_int> {
        Err(unsupported())
    }

    fn socketpair(domain: c_int, kind: c_int, protocol: c_int, sv: &mut [c_int; 2]) -> Result<()> {
        Err(unsupported())
    }
}
