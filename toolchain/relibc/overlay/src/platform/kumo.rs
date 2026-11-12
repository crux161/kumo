//j869
//j871
//j873
//j875
//j877
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
const SYSCALL_HANDLE_DUPLICATE: u64 = 1;
const SYSCALL_CHANNEL_CREATE: u64 = 3;
const SYSCALL_PORT_CREATE: u64 = 7;
const SYSCALL_PORT_WAIT: u64 = 8;
const SYSCALL_TIMER_CREATE: u64 = 27;
const SYSCALL_PORT_BIND: u64 = 33;
const SYSCALL_PORT_UNBIND: u64 = 37;
const SYSCALL_VMO_WRITE: u64 = 14;
const SYSCALL_PROCESS_CREATE: u64 = 18;
const SYSCALL_ADDRESS_SPACE_CREATE: u64 = 31;
const SYSCALL_PROCESS_RUN: u64 = 32;
const SYSCALL_HANDLE_KOID: u64 = 34;
const SYSCALL_PROCESS_WAIT: u64 = 35;
const SYSCALL_PROCESS_KILL: u64 = 46;
const SYSCALL_VMAR_UNMAP: u64 = 17;
const SYSCALL_VMAR_PROTECT: u64 = 16;
const SYSCALL_THREAD_SPAWN: u64 = 58;
const SYSCALL_THREAD_EXIT: u64 = 59;
const SYSCALL_THREAD_SELF: u64 = 60;
const SYSCALL_FAULT_HANDLER_SET: u64 = 61;

// SH4 spawn constants. The VMAR bits mirror `kumo_abi::VmarFlags`; a child's segments are
// user pages. Rights::ALL for the duplicated standard handles.
const VMAR_EXECUTE: u64 = 1 << 2;
const VMAR_USER: u64 = 1 << 3;
/// Rights for a duplicated standard handle: enough to use the channel end and transfer it
/// into the child, and no more — a duplicate may only narrow its source (the kernel
/// refuses amplification), and Sora's transfers use exactly this subset.
// DUPLICATE | TRANSFER | READ | WRITE. SH6-b: DUPLICATE stays on the copy, or a spawned
// child could never pass its own stdio (or an opted-in filesystem grant) to a child of
// its own: SH4's copies dropped it, which closed a grandchild's fds 1 and 2 and made
// std's startup abort. Nothing widens: duplicating already required DUPLICATE. — TEAL
const RIGHTS_CHANNEL: u64 = 0x1 | 0x2 | 0x4 | 0x8;
const PROCESS_RUN_ASYNC: u64 = 1 << 0;
const PROCESS_RUN_TRANSFER_ARG: u64 = 1 << 1;
/// The child address-space reservation ProcessCreate is given, as Sora gives its children.
const CHILD_VMAR_SIZE: u64 = 0x2000_0000;
/// The child stack arena top, as Sora's loader fixes it.
const CHILD_STACK_TOP: u64 = 0x1001_0000;
const DEFAULT_CHILD_STACK: u64 = 0x4000;
const MAX_CHILD_STACK: u64 = 8 * 1024 * 1024;
const MAX_CHILDREN: usize = 8;
/// Child pids are table slots offset by 100, clear of the standard descriptors.
const CHILD_PID_BASE: c_int = 100;
const PT_LOAD: u32 = 1;
const PT_GNU_STACK: u32 = 0x6474_E551;
/// `ChannelRead` x3: answer `ShouldWait` on an empty inbox instead of parking (SH4; mirrors
/// `kumo_abi::sys::CHANNEL_READ_NONBLOCK`).
const CHANNEL_READ_NONBLOCK: u64 = 1 << 0;
/// Epoll sets this process may hold open at once, and the descriptors each may watch.
/// relibc's `poll` makes one transient set per call; std's piped-output loop watches two.
const EPOLL_SETS: usize = 4;
const EPOLL_ENTRIES: usize = 8;

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
/// SH6-b: the child's end of its signal channel (PROPOSAL/004's event channel). The
/// spawner keeps the other end; asynchronous signals travel down it and this process's
/// caught/ignored masks travel up it.
const STARTUP_TAG_SIGNAL: u8 = b's';
/// SH6-b: the environment entry that asks a spawn to pass this process's filesystem grant.
const KUMO_GRANT_FS: &[u8] = b"KUMO_GRANT=fs";

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
const VFS_OP_READ_DIR: u8 = 0x06;
/// The server's inline reply ceiling bounds one listing page; a smaller client buffer
/// would make ChannelRead truncate the page silently, so reads always take this much.
const VFS_READ_DIR_BUF: usize = 1024;
/// Fixed bytes of one listing record: `flags(1) + size(8) + name_len(1)`.
const VFS_DIR_RECORD_HEADER: usize = 10;
/// Record flag: the entry is itself a directory.
const VFS_DIR_RECORD_DIRECTORY: u8 = 1 << 0;
const VFS_OP_MKDIR: u8 = 0x07;
const VFS_OP_UNLINK: u8 = 0x08;
const VFS_OP_RMDIR: u8 = 0x09;
const VFS_OP_RENAME: u8 = 0x0a;
const VFS_OP_STAT: u8 = 0x0b;
const VFS_OP_LINK: u8 = 0x0c;
const VFS_OP_LOCK: u8 = 0x0d;
const VFS_OP_CHMOD: u8 = 0x0e;
const VFS_OP_CLOSE: u8 = 0x0f;
/// Wire length of a close request: `op(1) + handle(4)`.
const CLOSE_REQUEST_LEN: usize = 5;
const VFS_OPEN_READ: u8 = 1 << 0;
const VFS_OPEN_WRITE: u8 = 1 << 1;
const VFS_OPEN_CREATE: u8 = 1 << 2;
const VFS_OK: u8 = 0x00;
const VFS_NOT_FOUND: u8 = 0x01;
const VFS_BAD_HANDLE: u8 = 0x02;
const VFS_IS_DIR: u8 = 0x03;
const VFS_BAD_REQUEST: u8 = 0x04;
const VFS_NO_SPACE: u8 = 0x05;
const VFS_ACCESS_DENIED: u8 = 0x06;
const VFS_IO_ERROR: u8 = 0x07;
const VFS_NOT_DIR: u8 = 0x08;
const VFS_EXISTS: u8 = 0x09;
const VFS_NOT_EMPTY: u8 = 0x0a;
const VFS_WOULD_BLOCK: u8 = 0x0b;
const VFS_NOT_SUPPORTED: u8 = 0x0c;
const VFS_STAT_REPLY_LEN: usize = 12;
const VFS_STAT_KIND_FILE: u8 = 0;
const VFS_STAT_KIND_DIRECTORY: u8 = 1;
const VFS_LOCK_SH: u8 = 1 << 0;
const VFS_LOCK_EX: u8 = 1 << 1;
const VFS_LOCK_UN: u8 = 1 << 2;
/// Wire length of a lock request: `op(1) + handle(4) + operation(1)`.
const LOCK_REQUEST_LEN: usize = 6;
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
/// SH6-b: this process's end of its signal channel, from the spawner (0 = none).
static SIGNAL_HANDLE: AtomicU32 = AtomicU32::new(0);
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
        /// Bit 0: O_APPEND (SH3-d2d) — writes start at the recorded end. Carried by
        /// dup and try_clone with the rest of the slot.
        flags: u8,
    },
    /// A directory opened with O_DIRECTORY (SH3-d2c): the resolved path this launch's
    /// server lists, plus the entry-index cursor posix_getdents resumes from. getdents
    /// itself is stateless — the caller owns the cookie — so it leaves the cursor.
    Dir {
        path: [u8; VFS_MAX_PATH],
        path_len: u16,
        cursor: u32,
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
    /// An epoll instance (SH4): a kernel port the watched channels are bound to. relibc's
    /// `poll` is built on it, which is what std's piped-stdio reader needs.
    Epoll {
        set: u8,
    },
}

/// One watched descriptor of an epoll set: the channel it names (bound to the set's port
/// when readable interest was registered) and the koid `PortWait` reports for it.
#[derive(Clone, Copy)]
struct EpollEntry {
    fd: c_int,
    events: u32,
    data: u64,
    handle: u32,
    koid: u64,
    bound: bool,
}

#[derive(Clone, Copy)]
struct EpollSet {
    port: u32,
    entries: [Option<EpollEntry>; EPOLL_ENTRIES],
}

static mut EPOLL_TABLE: [Option<EpollSet>; EPOLL_SETS] = [None; EPOLL_SETS];

/// O_NONBLOCK for channel descriptors (SH4), indexed like `FD_TABLE`. Per descriptor, not
/// per open file description: a dup does not share it (recorded deviation).
static mut FD_NONBLOCK: [bool; FD_SLOTS] = [false; FD_SLOTS];

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

/// SH5 `ThreadSpawn`: a sibling thread entering `entry` with `sp`; returns (status, koid).
fn thread_spawn(entry: u64, sp: u64) -> (u64, u64) {
    let status;
    let koid;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_THREAD_SPAWN,
            in("x0") entry,
            in("x1") sp,
            in("x2") 0_u64,
            lateout("x0") status,
            lateout("x1") koid,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (status, koid)
}

/// SH5 `ThreadSelf`: the calling thread's koid (its OS thread id).
fn thread_self_koid() -> u64 {
    syscall4(SYSCALL_THREAD_SELF, 0, 0, 0, 0)
}

// SH5: where a thread made by `rlct_clone` enters EL0. relibc's `pthread::create` pushed,
// from the stack pointer upward: new_thread_shim, start_routine, arg, the new Tcb, the
// synchronization mutex, then three padding words (64 bytes in all, so the stack end is
// reached again and stays 16-byte aligned). The shim takes the next four as its arguments
// and never returns. — TEAL
core::arch::global_asm!(
    ".text",
    ".global __relibc_kumo_thread_entry",
    ".type __relibc_kumo_thread_entry, %function",
    "__relibc_kumo_thread_entry:",
    "  ldp x9, x0, [sp]",
    "  ldp x1, x2, [sp, #16]",
    "  ldr x3, [sp, #32]",
    "  add sp, sp, #64",
    "  br x9",
);

/// The trampoline's address, PC-relative (no extern declaration: the backend's method
/// census counts indented `fn` lines).
fn thread_entry_address() -> u64 {
    let address: u64;
    unsafe {
        core::arch::asm!(
            "adrp {0}, __relibc_kumo_thread_entry",
            "add {0}, {0}, :lo12:__relibc_kumo_thread_entry",
            out(reg) address,
            options(nomem, nostack, preserves_flags),
        );
    }
    address
}

// ---- SH6-a signals: dispositions, the alternate stack, and synchronous faults ----------
//
// Per the SH6 ruling (2026-11-11): a thread's OWN synchronous fault (SIGSEGV/SIGBUS/SIGILL)
// is redirected by the kernel to `__relibc_kumo_signal_entry` on the registered alternate
// stack (`FaultHandlerSet`); asynchronous signals stay PROPOSAL/004 messages (SH6-b). No
// asynchronous delivery exists yet, so nothing is ever pending and the mask only records.
// — TEAL

const SIG_COUNT: usize = 65;
const SIGILL_NO: usize = 4;
const SIGBUS_NO: usize = 7;
const SIGKILL_NO: usize = 9;
const SIGSEGV_NO: usize = 11;
const SIGPIPE_NO: usize = 13;
const SIGSTOP_NO: usize = 19;
const SA_SIGINFO_FLAG: usize = 4;
const SS_ONSTACK_FLAG: c_int = 1;
const SS_DISABLE_FLAG: c_int = 2;
/// Linux aarch64's MINSIGSTKSZ.
const MIN_SIGNAL_STACK: usize = 5120;
/// A process that ended by a signal exits with this bit plus the signal number; `waitpid`
/// reports it as WIFSIGNALED/WTERMSIG. The kernel carries the code untouched.
const SIGNALED_EXIT: u64 = 0x100;
const STATUS_PEER_CLOSED: i32 = -6;

/// Process-wide dispositions: 0 = SIG_DFL, 1 = SIG_IGN, otherwise the handler address.
static SIG_HANDLER: [core::sync::atomic::AtomicUsize; SIG_COUNT] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; SIG_COUNT];
static SIG_FLAGS: [core::sync::atomic::AtomicUsize; SIG_COUNT] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; SIG_COUNT];
static SIG_MASK: [core::sync::atomic::AtomicU64; SIG_COUNT] =
    [const { core::sync::atomic::AtomicU64::new(0) }; SIG_COUNT];

/// This thread's alternate signal stack (`sigaltstack`), or a zero base when none.
#[thread_local]
static ALT_STACK_BASE: core::cell::Cell<usize> = core::cell::Cell::new(0);
#[thread_local]
static ALT_STACK_SIZE: core::cell::Cell<usize> = core::cell::Cell::new(0);
/// This thread's blocked-signal mask (`sigprocmask`), bit `sig - 1`.
#[thread_local]
static SIGNAL_MASK: core::cell::Cell<u64> = core::cell::Cell::new(0);

core::arch::global_asm!(
    ".text",
    ".global __relibc_kumo_signal_entry",
    ".type __relibc_kumo_signal_entry, %function",
    "__relibc_kumo_signal_entry:",
    // The kernel left x0..x4 = (signo, fault address, ESR, fault PC, fault SP) and SP on
    // the alternate stack. Terminate the frame chain for any unwinder, then deliver.
    "  mov x29, #0",
    "  mov x30, #0",
    "  bl __relibc_kumo_signal_deliver",
    "  brk #0x6b",
);

fn signal_entry_address() -> u64 {
    let address: u64;
    unsafe {
        core::arch::asm!(
            "adrp {0}, __relibc_kumo_signal_entry",
            "add {0}, {0}, :lo12:__relibc_kumo_signal_entry",
            out(reg) address,
            options(nomem, nostack, preserves_flags),
        );
    }
    address
}

fn current_stack_pointer() -> usize {
    let sp: usize;
    unsafe {
        core::arch::asm!("mov {0}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    sp
}

fn on_alt_stack() -> bool {
    let base = ALT_STACK_BASE.get();
    let sp = current_stack_pointer();
    base != 0 && sp >= base && sp < base + ALT_STACK_SIZE.get()
}

/// Re-arm the CALLING thread's kernel registration from the current dispositions and its
/// alternate stack: an entry only while a synchronous signal has a real handler.
fn refresh_fault_registration() {
    let caught = [SIGSEGV_NO, SIGBUS_NO, SIGILL_NO]
        .iter()
        .any(|&sig| SIG_HANDLER[sig].load(Ordering::Acquire) > 1);
    let entry = if caught { signal_entry_address() } else { 0 };
    let base = ALT_STACK_BASE.get();
    let (base, size) = if entry != 0 && base != 0 {
        (base as u64, ALT_STACK_SIZE.get() as u64)
    } else {
        (0, 0)
    };
    let _ = syscall4(SYSCALL_FAULT_HANDLER_SET, entry, base, size, 0);
}

/// End the process as killed by `sig` (no stdio flush, as a signal death has none).
fn exit_signaled(sig: usize) -> ! {
    let _ = syscall4(
        SYSCALL_PROCESS_EXIT,
        SIGNALED_EXIT | (sig as u64 & 0x7f),
        0,
        0,
        0,
    );
    loop {
        core::hint::spin_loop();
    }
}

/// Signals whose default action is to do nothing. With no job control on KUMO the stop
/// signals are ignored too (recorded deviation).
fn ignored_by_default(sig: usize) -> bool {
    matches!(sig, 17 | 18 | 19 | 20 | 21 | 22 | 23 | 28)
}

/// Run `sig`'s disposition synchronously on this thread (`raise`, SIGPIPE).
fn run_disposition(sig: usize) {
    match SIG_HANDLER[sig].load(Ordering::Acquire) {
        0 if ignored_by_default(sig) => {}
        0 => exit_signaled(sig),
        1 => {}
        handler => {
            let flags = SIG_FLAGS[sig].load(Ordering::Acquire);
            if flags & SA_RESETHAND_FLAG != 0 {
                SIG_HANDLER[sig].store(0, Ordering::Release);
            }
            if flags & SA_SIGINFO_FLAG != 0 {
                let mut info = [0u64; 16];
                info[0] = sig as u64; // si_signo; si_errno 0; si_code SI_USER (0)
                let handler: extern "C" fn(c_int, *mut c_void, *mut c_void) =
                    unsafe { core::mem::transmute(handler) };
                handler(sig as c_int, info.as_mut_ptr().cast(), core::ptr::null_mut());
            } else {
                let handler: extern "C" fn(c_int) = unsafe { core::mem::transmute(handler) };
                handler(sig as c_int);
            }
        }
    }
}

/// The write side of a closed pipe or channel: SIGPIPE's disposition, then EPIPE.
fn broken_pipe() -> Errno {
    use crate::header::errno::EPIPE;

    run_disposition(SIGPIPE_NO);
    Errno(EPIPE)
}

// ---- SH6-b asynchronous signals (PROPOSAL/004) -------------------------------------
//
// A signal to another process is a message on that process's signal channel, never an
// interruption: the target's relibc starts a hidden thread (only once it catches some
// asynchronous signal) that blocks on the channel and runs the disposition there, on its
// own thread — no reentrancy into the interrupted code. What a target catches or ignores
// travels up the same channel, so a sender can apply the default itself when there is
// nobody to deliver to: `ProcessKill` with `0x100 | signo`. A group (`kill(-pid)`,
// `killpg`) is the spawn tree under `pid`: the handler thread forwards a group signal to
// this process's own children before running the handler, and a default-action group
// death takes the whole tree in one kernel call. — TEAL

const SIGNAL_MSG_MASKS: u8 = b'M';
const SIGNAL_MSG_SIGNAL: u8 = b'S';
const SA_RESETHAND_FLAG: usize = 0x8000_0000;
static SIGNAL_THREAD_STARTED: AtomicU32 = AtomicU32::new(0);
static SIGNAL_REPORTED: [core::sync::atomic::AtomicU64; 2] =
    [const { core::sync::atomic::AtomicU64::new(u64::MAX) }; 2];

/// Signals the hidden thread can deliver: everything but the synchronous faults and the
/// two that cannot be caught.
fn is_async_signal(sig: usize) -> bool {
    sig != 0
        && sig < SIG_COUNT
        && !matches!(sig, SIGSEGV_NO | SIGBUS_NO | SIGILL_NO | SIGKILL_NO | SIGSTOP_NO)
}

/// This process's (caught, ignored) asynchronous-signal masks, bit `sig - 1`.
fn async_masks() -> (u64, u64) {
    let (mut caught, mut ignored) = (0u64, 0u64);
    for sig in 1..SIG_COUNT {
        if !is_async_signal(sig) {
            continue;
        }
        match SIG_HANDLER[sig].load(Ordering::Acquire) {
            0 => {}
            1 => ignored |= 1 << (sig - 1),
            _ => caught |= 1 << (sig - 1),
        }
    }
    (caught, ignored)
}

/// After an asynchronous disposition changed: start the handler thread once something is
/// caught, and report the masks to the spawner when they differ from the last report.
fn refresh_async_registration() {
    let channel = SIGNAL_HANDLE.load(Ordering::Acquire);
    if channel == 0 {
        return;
    }
    let (caught, ignored) = async_masks();
    if caught != 0
        && SIGNAL_THREAD_STARTED
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        let started = unsafe {
            crate::pthread::create(None, signal_thread_main, core::ptr::null_mut())
        };
        match started {
            Ok(thread) => unsafe {
                let _ = crate::header::pthread::pthread_detach(thread);
            },
            Err(_) => SIGNAL_THREAD_STARTED.store(0, Ordering::Release),
        }
    }
    if SIGNAL_REPORTED[0].load(Ordering::Acquire) == caught
        && SIGNAL_REPORTED[1].load(Ordering::Acquire) == ignored
    {
        return;
    }
    let mut message = [0u8; 17];
    message[0] = SIGNAL_MSG_MASKS;
    message[1..9].copy_from_slice(&caught.to_le_bytes());
    message[9..17].copy_from_slice(&ignored.to_le_bytes());
    if channel_write_bytes(channel, &message) == 0 {
        SIGNAL_REPORTED[0].store(caught, Ordering::Release);
        SIGNAL_REPORTED[1].store(ignored, Ordering::Release);
    }
}

/// The hidden handler thread: one signal message at a time, run where it lands.
extern "C" fn signal_thread_main(_arg: *mut c_void) -> *mut c_void {
    let channel = SIGNAL_HANDLE.load(Ordering::Acquire);
    let mut message = [0u8; 16];
    loop {
        let n = channel_read_bytes(channel, &mut message);
        if n == 0 || n == u64::MAX {
            // The spawner is gone: no signal can arrive any more.
            return core::ptr::null_mut();
        }
        if n < 3 || message[0] != SIGNAL_MSG_SIGNAL {
            continue;
        }
        let sig = message[1] as usize;
        if !is_async_signal(sig) {
            continue;
        }
        if message[2] != 0 {
            forward_to_children(sig);
        }
        run_disposition(sig);
    }
}

/// Fold every masks report the child sent since the last look into its slot.
fn child_signal_sync(slot: &mut ChildSlot) {
    if slot.signal == 0 {
        return;
    }
    let mut message = [0u8; 32];
    loop {
        let n = channel_read_bytes_nonblock(slot.signal, &mut message);
        if n == 0 || n == u64::MAX || n as u32 as i32 == KUMO_SHOULD_WAIT {
            return;
        }
        if n as usize >= 17 && message[0] == SIGNAL_MSG_MASKS {
            slot.caught = u64::from_le_bytes(message[1..9].try_into().unwrap());
            slot.ignored = u64::from_le_bytes(message[9..17].try_into().unwrap());
        }
    }
}

/// Deliver `sig` to one live child (and, for a group, its spawn tree). Returns the kernel
/// status of a default-action kill, or 0.
fn signal_child(slot: &mut ChildSlot, sig: usize, group: bool) -> u64 {
    child_signal_sync(slot);
    let bit = 1u64 << (sig - 1);
    if sig != SIGKILL_NO && slot.caught & bit != 0 {
        let message = [SIGNAL_MSG_SIGNAL, sig as u8, u8::from(group)];
        if channel_write_bytes(slot.signal, &message) == 0 {
            return 0;
        }
        // Nobody reads any more: fall through to the default.
    }
    if sig != SIGKILL_NO && (slot.ignored & bit != 0 || ignored_by_default(sig)) {
        // Recorded limit: an ignoring member does not pass a group signal on to the
        // processes it spawned.
        return 0;
    }
    process_kill_signaled(slot.process, sig, group)
}

/// A group signal reached this process: pass it on to every live child's tree first.
fn forward_to_children(sig: usize) {
    for index in 0..MAX_CHILDREN {
        let slot = unsafe {
            (&raw mut CHILD_TABLE)
                .cast::<Option<ChildSlot>>()
                .add(index)
                .as_mut()
        };
        if let Some(Some(slot)) = slot {
            if matches!(slot.state, ChildState::Live) {
                let _ = signal_child(slot, sig, true);
            }
        }
    }
}

/// Where the kernel's fault redirect lands (via `__relibc_kumo_signal_entry`), on the
/// alternate stack when one is registered. Builds `siginfo_t` and a minimal `ucontext_t`
/// here, never in the kernel. `si_addr` is written at offset 16, where Linux and the Rust
/// libc crate read it, and at offset 24, where relibc's own `siginfo` places it. A
/// synchronous fault never resumes: a handler that returns, an ignored or a default
/// disposition all end the process as killed by the signal (recorded deviation; on
/// Linux the re-executed instruction faults again to the same end). — TEAL
#[unsafe(no_mangle)]
unsafe extern "C" fn __relibc_kumo_signal_deliver(
    signo: u64,
    far: u64,
    esr: u64,
    pc: u64,
    sp: u64,
) -> ! {
    let sig = (signo as usize).min(SIG_COUNT - 1);
    let handler = SIG_HANDLER[sig].load(Ordering::Acquire);
    if handler > 1 {
        let code: u64 = match sig {
            SIGSEGV_NO => match esr & 0x3c {
                0x08 | 0x0c => 2, // access flag / permission fault: SEGV_ACCERR
                _ => 1,           // translation and the rest: SEGV_MAPERR
            },
            _ => 1, // BUS_ADRALN, ILL_ILLOPC
        };
        let mut info = [0u64; 16];
        info[0] = signo; // si_signo (low half), si_errno 0 (high half)
        info[1] = code; // si_code (low half)
        info[2] = far; // si_addr: Linux / libc-crate offset
        info[3] = far; // si_addr: relibc offset
        // ucontext: uc_flags, uc_link, uc_stack {ss_sp, ss_flags, ss_size}; the rest zero.
        let mut context = [0u64; 160];
        context[2] = ALT_STACK_BASE.get() as u64;
        context[3] = if ALT_STACK_BASE.get() != 0 {
            SS_ONSTACK_FLAG as u64
        } else {
            SS_DISABLE_FLAG as u64
        };
        context[4] = ALT_STACK_SIZE.get() as u64;
        let _ = (pc, sp);
        if SIG_FLAGS[sig].load(Ordering::Acquire) & SA_SIGINFO_FLAG != 0 {
            let handler: extern "C" fn(c_int, *mut c_void, *mut c_void) =
                unsafe { core::mem::transmute(handler) };
            handler(
                sig as c_int,
                info.as_mut_ptr().cast(),
                context.as_mut_ptr().cast(),
            );
        } else {
            let handler: extern "C" fn(c_int) = unsafe { core::mem::transmute(handler) };
            handler(sig as c_int);
        }
    }
    SIG_HANDLER[sig].store(0, Ordering::Release);
    exit_signaled(sig)
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

/// A successful syscall's handle/status word. Kernel errors arrive zero-extended — a
/// negative i32 in the low word — so `u64::MAX`-only screens let refusals masquerade as
/// handles; every raw handle helper goes through here.
fn syscall_ok(value: u64) -> Option<u32> {
    if value == 0 || (value as u32 as i32) < 0 {
        return None;
    }
    Some(value as u32)
}

fn channel_pair() -> Option<(u32, u32)> {
    let first: u64;
    let second: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_CHANNEL_CREATE,
            lateout("x0") first,
            lateout("x1") second,
            clobber_abi("C"),
            options(nostack),
        );
    }
    Some((syscall_ok(first)?, syscall_ok(second)?))
}

fn vmo_create_raw(len: u64) -> Option<u32> {
    syscall_ok(syscall4(SYSCALL_VMO_CREATE, len, 0, 0, 0))
}

fn vmo_write(vmo: u32, offset: u64, src: *const u8, len: usize) -> u64 {
    syscall4(SYSCALL_VMO_WRITE, u64::from(vmo), offset, src as u64, len as u64)
}

/// One startup tag plus one transferred handle, the wire the child's intake loop reads.
fn channel_write_tag(channel: u32, tag: u8, handle: u32) -> u64 {
    syscall4(
        SYSCALL_CHANNEL_WRITE,
        u64::from(channel),
        (&raw const tag) as u64,
        1,
        u64::from(handle),
    )
}

fn handle_duplicate_raw(handle: u32) -> Option<u32> {
    syscall_ok(syscall4(
        SYSCALL_HANDLE_DUPLICATE,
        u64::from(handle),
        RIGHTS_CHANNEL,
        0,
        0,
    ))
}

fn handle_koid_raw(handle: u32) -> Option<u64> {
    let koid = syscall4(SYSCALL_HANDLE_KOID, u64::from(handle), 0, 0, 0);
    if koid == 0 || (koid as u32 as i32) < 0 {
        return None;
    }
    Some(koid)
}

fn process_create_raw(vmar_base: u64, vmar_size: u64) -> Option<u32> {
    syscall_ok(syscall4(SYSCALL_PROCESS_CREATE, vmar_base, vmar_size, 0, 0))
}

fn process_run_raw(process: u32, entry: u64, sp: u64, arg: u64, arg2: u64, flags: u64) -> u64 {
    let status: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_PROCESS_RUN,
            in("x0") u64::from(process),
            in("x1") entry,
            in("x2") sp,
            in("x3") arg,
            in("x4") arg2,
            in("x5") flags,
            lateout("x0") status,
            clobber_abi("C"),
            options(nostack),
        );
    }
    status
}

fn process_wait_pair() -> (u64, u64, u64) {
    let status: u64;
    let koid: u64;
    let code: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_PROCESS_WAIT,
            lateout("x0") status,
            lateout("x1") koid,
            lateout("x2") code,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (status, koid, code)
}

fn process_kill_raw(process: u32) -> u64 {
    syscall4(SYSCALL_PROCESS_KILL, u64::from(process), 0, 0, 0)
}

/// SH6-b `ProcessKill` with the status the parent sees and, for a group, the whole spawn
/// tree (`PROCESS_KILL_TREE`).
fn process_kill_signaled(process: u32, sig: usize, tree: bool) -> u64 {
    syscall4(
        SYSCALL_PROCESS_KILL,
        u64::from(process),
        SIGNALED_EXIT | sig as u64,
        u64::from(tree),
        0,
    )
}

fn address_space_create(process: u32, stack_top: u64, stack_size: u64) -> u64 {
    syscall4(
        SYSCALL_ADDRESS_SPACE_CREATE,
        u64::from(process),
        stack_top,
        stack_size,
        0,
    )
}

/// Map a child's segment: vmar_map_anywhere's shape, but the child names the address space
/// and the virtual address is explicit.
fn vmar_map_child(
    process: u32,
    vmo: u32,
    vmo_offset: u64,
    virt: u64,
    len: u64,
    flags: u64,
) -> u64 {
    let status: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") SYSCALL_VMAR_MAP,
            in("x0") u64::from(process),
            in("x1") u64::from(vmo),
            in("x2") vmo_offset,
            in("x3") virt,
            in("x4") len,
            in("x5") flags,
            lateout("x0") status,
            clobber_abi("C"),
            options(nostack),
        );
    }
    status
}

/// Build one startup VMO in the argv/env wire format: `[count u32][item\0 ...]`.
fn startup_image(items: &[&[u8]]) -> Option<u32> {
    let mut wire = [0u8; 1024];
    wire[0..4].copy_from_slice(&(items.len() as u32).to_le_bytes());
    let mut at = 4usize;
    for item in items {
        let end = at + item.len() + 1;
        if end > wire.len() {
            return None;
        }
        wire[at..at + item.len()].copy_from_slice(item);
        wire[at + item.len()] = 0;
        at = end;
    }
    let vmo = vmo_create_raw(at as u64)?;
    if vmo_write(vmo, 0, wire.as_ptr(), at) != 0 {
        close_handle(vmo);
        return None;
    }
    Some(vmo)
}

// The loader's page arithmetic lives at module scope: the scaffold gate counts
// four-space `fn` lines as Pal methods, and nested helpers would inflate the trait count.
fn align_up4k(value: u64) -> Option<u64> {
    value.checked_add(0xFFF).map(|value| value & !0xFFF)
}

fn align_down4k(value: u64) -> u64 {
    value & !0xFFF
}

fn page_flags(elf: u32) -> u64 {
    let mut flags = VMAR_USER;
    if elf & 4 != 0 {
        flags |= 1; // PF_R
    }
    if elf & 2 != 0 {
        flags |= 2; // PF_W
    }
    if elf & 1 != 0 {
        flags |= VMAR_EXECUTE; // PF_X
    }
    flags
}

/// The kernel spawn pipeline, ported from Sora's loader (SH4): the ELF is read through
/// THIS process's filesystem grant, staged into a child VMO, mapped into a fresh process,
/// given a stack, and run asynchronously with one transferred bootstrap channel carrying
/// exactly the tagged grants built here. Nothing is ambient: the child receives what was
/// enqueued and no more. — TIMBERDOODLE
fn spawn_elf(
    program: CStr,
    args: &[&[u8]],
    envs: &[&[u8]],
    stdio: &[Option<(u8, u32)>; 3],
) -> Result<pid_t> {
    use crate::header::{
        errno::{E2BIG, EAGAIN, EINVAL, EIO, ENOMEM},
        fcntl::{AT_FDCWD, O_RDONLY},
    };

    #[derive(Clone, Copy)]
    struct Segment {
        file_offset: u64,
        virt_addr: u64,
        file_size: u64,
        mem_size: u64,
        flags: u64,
    }

    let fd = Sys::openat(AT_FDCWD, program, O_RDONLY, 0)?;

    let mut vmo = 0u32;
    let mut process = 0u32;
    let mut child_end = 0u32;
    let mut sender = 0u32;
    let mut signal_end = 0u32;
    let result = (|| -> Result<pid_t> {
        let mut header = [0u8; 64];
        let n = Sys::pread(fd, &mut header, 0)?;
        if n != 64
            || header[0..4] != [0x7F, b'E', b'L', b'F']
            || header[4] != 2
            || header[5] != 1
        {
            return Err(Errno(EINVAL));
        }
        let entry = u64::from_le_bytes(header[0x18..0x20].try_into().unwrap());
        let phoff = u64::from_le_bytes(header[0x20..0x28].try_into().unwrap());
        let phentsize = usize::from(u16::from_le_bytes([header[0x36], header[0x37]]));
        let phnum = u16::from_le_bytes([header[0x38], header[0x39]]);
        if phentsize < 56 || phnum == 0 || phnum > 8 {
            return Err(Errno(EINVAL));
        }

        let empty = Segment {
            file_offset: 0,
            virt_addr: 0,
            file_size: 0,
            mem_size: 0,
            flags: 0,
        };
        let mut segments = [empty; 8];
        let mut segment_count = 0usize;
        let mut vmo_len = 0u64;
        let mut stack_request = 0u64;
        for index in 0..u64::from(phnum) {
            let mut ph = [0u8; 56];
            let off = phoff + index * phentsize as u64;
            if Sys::pread(fd, &mut ph, off as off_t)? != 56 {
                return Err(Errno(EIO));
            }
            let kind = u32::from_le_bytes(ph[0..4].try_into().unwrap());
            let elf_flags = u32::from_le_bytes(ph[4..8].try_into().unwrap());
            let file_offset = u64::from_le_bytes(ph[8..16].try_into().unwrap());
            let virt_addr = u64::from_le_bytes(ph[16..24].try_into().unwrap());
            let file_size = u64::from_le_bytes(ph[32..40].try_into().unwrap());
            let mem_size = u64::from_le_bytes(ph[40..48].try_into().unwrap());
            if kind == PT_GNU_STACK {
                stack_request = mem_size;
            } else if kind == PT_LOAD && mem_size != 0 {
                segments[segment_count] = Segment {
                    file_offset,
                    virt_addr,
                    file_size,
                    mem_size,
                    flags: page_flags(elf_flags),
                };
                segment_count += 1;
                // The VMO must cover the mapping's full memory extent, not just the file
                // bytes: a segment's BSS zero-fill rides the same aligned window, so
                // size from align_down(file_offset) through align_up(page delta +
                // mem_size). Sora's loader sizes identically.
                let vmo_offset = align_down4k(file_offset);
                let delta = virt_addr & 0xFFF;
                let span = delta
                    .checked_add(mem_size)
                    .and_then(align_up4k)
                    .ok_or(Errno(EINVAL))?;
                vmo_len = vmo_len.max(
                    vmo_offset.checked_add(span).ok_or(Errno(EINVAL))?,
                );
            }
        }
        if segment_count == 0 {
            return Err(Errno(EINVAL));
        }

        let len = align_up4k(vmo_len.max(1)).ok_or(Errno(EINVAL))?;
        vmo = vmo_create_raw(len).ok_or(Errno(ENOMEM))?;
        let mut chunk = [0u8; 256];
        for segment in &segments[..segment_count] {
            let mut copied = 0u64;
            while copied < segment.file_size {
                let take = ((segment.file_size - copied) as usize).min(chunk.len());
                let got = Sys::pread(
                    fd,
                    &mut chunk[..take],
                    (segment.file_offset + copied) as off_t,
                )?;
                if got != take
                    || vmo_write(
                        vmo,
                        segment.file_offset + copied,
                        chunk.as_ptr(),
                        take,
                    ) != 0
                {
                    return Err(Errno(EIO));
                }
                copied += take as u64;
            }
        }

        process = process_create_raw(0, CHILD_VMAR_SIZE).ok_or(Errno(ENOMEM))?;
        for segment in &segments[..segment_count] {
            let delta = segment.virt_addr & 0xFFF;
            let virt = align_down4k(segment.virt_addr);
            let map_len = align_up4k(delta + segment.mem_size).ok_or(Errno(EINVAL))?;
            let vmo_offset = align_down4k(segment.file_offset);
            let map_status = vmar_map_child(process, vmo, vmo_offset, virt, map_len, segment.flags);
            if map_status != 0 {
                return Err(Errno(ENOMEM));
            }
        }

        let stack_size = match stack_request {
            0 => DEFAULT_CHILD_STACK,
            size if size <= MAX_CHILD_STACK => size.max(DEFAULT_CHILD_STACK),
            _ => return Err(Errno(EINVAL)),
        };
        // Sora's loader convention: this call reports failure as u64::MAX; any other
        // value (a mapped address) is success.
        let as_value = address_space_create(process, CHILD_STACK_TOP, stack_size);
        if as_value == u64::MAX {
            return Err(Errno(ENOMEM));
        }

        let Some((first, second)) = channel_pair() else {
            return Err(Errno(ENOMEM));
        };
        child_end = first;
        sender = second;
        // SH6-b: the child's signal channel; this process keeps `signal_end`.
        let Some((ours, theirs)) = channel_pair() else {
            return Err(Errno(ENOMEM));
        };
        signal_end = ours;
        if channel_write_tag(sender, STARTUP_TAG_SIGNAL, theirs) != 0 {
            close_handle(theirs);
            return Err(Errno(EIO));
        }
        for entry in stdio.iter().flatten() {
            let (tag, handle) = *entry;
            if channel_write_tag(sender, tag, handle) != 0 {
                close_handle(handle);
                return Err(Errno(EIO));
            }
        }
        // SH6-b: the filesystem grant travels only when this spawn asks for it — the
        // ratified opt-in, so a cargo-shaped tree can spawn again while the default stays
        // SH4's "no grant". `KUMO_GRANT=fs` is consumed here, not passed on. — TEAL
        let mut kept: [&[u8]; 16] = [b""; 16];
        let mut kept_len = 0usize;
        let mut grant_fs = false;
        for env in envs {
            if *env == KUMO_GRANT_FS {
                grant_fs = true;
            } else if kept_len < kept.len() {
                kept[kept_len] = env;
                kept_len += 1;
            }
        }
        let envs = &kept[..kept_len];
        let fs = FS_HANDLE.load(Ordering::Relaxed);
        if grant_fs && fs != 0 {
            let Some(duplicate) = handle_duplicate_raw(fs) else {
                return Err(Errno(ENOMEM));
            };
            if channel_write_tag(sender, STARTUP_TAG_FS, duplicate) != 0 {
                close_handle(duplicate);
                return Err(Errno(EIO));
            }
        }
        let Some(argv_vmo) = startup_image(args) else {
            return Err(Errno(E2BIG));
        };
        if channel_write_tag(sender, STARTUP_TAG_ARGV, argv_vmo) != 0 {
            close_handle(argv_vmo);
            return Err(Errno(EIO));
        }
        if !envs.is_empty() {
            let Some(env_vmo) = startup_image(envs) else {
                return Err(Errno(E2BIG));
            };
            if channel_write_tag(sender, STARTUP_TAG_ENV, env_vmo) != 0 {
                close_handle(env_vmo);
                return Err(Errno(EIO));
            }
        }
        // The child's intake reads until EOF: the sender must close after the queue.
        close_handle(sender);
        sender = 0;

        let koid = handle_koid_raw(process).ok_or(Errno(EINVAL))?;
        let status = process_run_raw(
            process,
            entry,
            CHILD_STACK_TOP - 0x10,
            u64::from(child_end),
            0,
            PROCESS_RUN_ASYNC | PROCESS_RUN_TRANSFER_ARG,
        ) as u32 as i32;
        if status != 0 {
            return Err(Errno(-status));
        }
        // The bootstrap child end was transferred; the VMO mapping kept ours.
        child_end = 0;
        close_handle(vmo);
        vmo = 0;

        let Some(pid) = child_alloc(process, koid, signal_end) else {
            let _ = process_kill_raw(process);
            return Err(Errno(EAGAIN));
        };
        process = 0;
        signal_end = 0;
        Ok(pid)
    })();

    if result.is_err() {
        if sender != 0 {
            close_handle(sender);
        }
        if signal_end != 0 {
            close_handle(signal_end);
        }
        if child_end != 0 {
            close_handle(child_end);
        }
        if process != 0 {
            let _ = process_kill_raw(process);
            close_handle(process);
        }
        if vmo != 0 {
            close_handle(vmo);
        }
        for entry in stdio.iter().flatten() {
            close_handle(entry.1);
        }
    }
    let _ = Sys::close(fd);
    result
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

/// `channel_read_bytes` that answers `KUMO_SHOULD_WAIT` (zero-extended in the count word)
/// on an empty inbox instead of parking.
// SH6-a: the unread rest of a channel message. ChannelRead is a datagram read: a message
// larger than the caller's buffer is consumed whole and its tail dropped. A POSIX pipe
// keeps that tail for the next read (std's read_to_end opens with a 32-byte probe, which
// cut a child's stderr line at 32 bytes), so a short read takes the message into a
// full-size buffer and parks the rest here, keyed by the channel handle (dup aliases
// share it, as they share a file description). — TEAL
const CHANNEL_MESSAGE_MAX: usize = 4096;
const PIPE_STASH_SLOTS: usize = 8;

struct PipeStash {
    handle: u32,
    start: usize,
    end: usize,
    bytes: [u8; CHANNEL_MESSAGE_MAX],
}

static PIPE_STASH_LOCK: AtomicU32 = AtomicU32::new(0);
static mut PIPE_STASH: [PipeStash; PIPE_STASH_SLOTS] = [const {
    PipeStash {
        handle: 0,
        start: 0,
        end: 0,
        bytes: [0; CHANNEL_MESSAGE_MAX],
    }
}; PIPE_STASH_SLOTS];

fn with_pipe_stash<R>(f: impl FnOnce(&mut [PipeStash; PIPE_STASH_SLOTS]) -> R) -> R {
    while PIPE_STASH_LOCK
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    let result = f(unsafe { &mut *core::ptr::addr_of_mut!(PIPE_STASH) });
    PIPE_STASH_LOCK.store(0, Ordering::Release);
    result
}

/// Copy a parked remainder for `handle` into `buf`, if one exists.
fn pipe_stash_take(handle: u32, buf: &mut [u8]) -> Option<usize> {
    with_pipe_stash(|stash| {
        let slot = stash
            .iter_mut()
            .find(|slot| slot.handle == handle && slot.start < slot.end)?;
        let take = buf.len().min(slot.end - slot.start);
        buf[..take].copy_from_slice(&slot.bytes[slot.start..slot.start + take]);
        slot.start += take;
        if slot.start == slot.end {
            slot.handle = 0;
        }
        Some(take)
    })
}

/// Forget `handle`'s remainder (its last descriptor closed).
fn pipe_stash_drop(handle: u32) {
    with_pipe_stash(|stash| {
        for slot in stash.iter_mut().filter(|slot| slot.handle == handle) {
            slot.handle = 0;
            slot.start = 0;
            slot.end = 0;
        }
    });
}

/// One POSIX read from a channel: a parked remainder first, then a fresh message whose
/// tail, if `buf` is short, is parked. Returns the raw ChannelRead status on failure.
fn channel_read_posix(handle: u32, buf: &mut [u8], nonblock: bool) -> u64 {
    if let Some(n) = pipe_stash_take(handle, buf) {
        return n as u64;
    }
    let read = |into: &mut [u8]| {
        if nonblock {
            channel_read_bytes_nonblock(handle, into)
        } else {
            channel_read_bytes(handle, into)
        }
    };
    if buf.len() >= CHANNEL_MESSAGE_MAX {
        return read(buf);
    }
    let mut message = [0u8; CHANNEL_MESSAGE_MAX];
    let n = read(&mut message);
    if n == u64::MAX || n as u32 as i32 == KUMO_SHOULD_WAIT {
        return n;
    }
    let n = (n as usize).min(CHANNEL_MESSAGE_MAX);
    let take = n.min(buf.len());
    buf[..take].copy_from_slice(&message[..take]);
    if n > take {
        let parked = with_pipe_stash(|stash| {
            let Some(slot) = stash.iter_mut().find(|slot| slot.handle == 0) else {
                return false;
            };
            slot.handle = handle;
            slot.bytes[..n - take].copy_from_slice(&message[take..n]);
            slot.start = 0;
            slot.end = n - take;
            true
        });
        // With every slot busy the tail is lost, as before this fix (recorded limit).
        let _ = parked;
    }
    take as u64
}

fn channel_read_bytes_nonblock(channel: u32, buf: &mut [u8]) -> u64 {
    syscall4(
        SYSCALL_CHANNEL_READ,
        u64::from(channel),
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        CHANNEL_READ_NONBLOCK,
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
    use crate::header::errno::{
        EACCES, EAGAIN, EBADF, EEXIST, EINVAL, EIO, EISDIR, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY,
        EOPNOTSUPP,
    };

    match status {
        VFS_NOT_FOUND => Errno(ENOENT),
        VFS_BAD_HANDLE => Errno(EBADF),
        VFS_IS_DIR => Errno(EISDIR),
        VFS_BAD_REQUEST => Errno(EINVAL),
        VFS_NO_SPACE => Errno(ENOSPC),
        VFS_ACCESS_DENIED => Errno(EACCES),
        VFS_IO_ERROR => Errno(EIO),
        VFS_NOT_DIR => Errno(ENOTDIR),
        VFS_EXISTS => Errno(EEXIST),
        VFS_NOT_EMPTY => Errno(ENOTEMPTY),
        VFS_WOULD_BLOCK => Errno(EAGAIN),
        VFS_NOT_SUPPORTED => Errno(EOPNOTSUPP),
        _ => Errno(EIO),
    }
}

/// Resolve an `…at` call's path. `AT_FDCWD` bases on this process's filesystem root —
/// a process's cwd is the capability's root — and a directory descriptor bases on its
/// recorded path, which is what remove_dir_all's unlinkat(dirfd, name, ...) walk needs
/// (SH3-d2d). Absolute names ignore the base, as POSIX says. — TIMBERDOODLE
fn at_path<'a>(dirfd: c_int, path: &'a [u8], buf: &'a mut [u8; VFS_MAX_PATH]) -> Result<&'a [u8]> {
    use crate::header::{
        errno::{EINVAL, EISDIR, ENAMETOOLONG},
        fcntl::AT_FDCWD,
    };

    if dirfd == AT_FDCWD {
        if path.is_empty() {
            return Err(Errno(ENAMETOOLONG));
        }
        return vfs_path(path, buf);
    }
    let Some(FdSlot::Dir {
        path: dir_path,
        path_len,
        ..
    }) = fd_slot(dirfd).map(|slot| *slot)
    else {
        return Err(Errno(EINVAL));
    };
    if path.is_empty() {
        return Err(Errno(ENAMETOOLONG));
    }
    if path[0] == b'/' {
        return vfs_path(path, buf);
    }
    let mut rest = path;
    while let Some(tail) = rest.strip_prefix(b"./") {
        rest = tail;
    }
    if rest.is_empty() || rest == b"." {
        return Err(Errno(EISDIR));
    }
    let dir = &dir_path[..usize::from(path_len)];
    let dir = if dir == b"/" { &dir[..0] } else { dir };
    let len = dir.len() + 1 + rest.len();
    if len > VFS_MAX_PATH {
        return Err(Errno(ENAMETOOLONG));
    }
    buf[..dir.len()].copy_from_slice(dir);
    buf[dir.len()] = b'/';
    buf[dir.len() + 1..len].copy_from_slice(rest);
    Ok(&buf[..len])
}

/// One status-only `kumo-vfs` exchange. The SH3 namespace ops answer exactly one status
/// byte on success or failure alike; `vfs_call` already rejects a dead channel.
fn vfs_status(request: &[u8]) -> Result<()> {
    let mut reply = [0u8; 4];
    vfs_call(request, &mut reply)?;
    if reply[0] != VFS_OK {
        return Err(vfs_errno(reply[0]));
    }
    Ok(())
}

/// One `OP_LOCK` exchange (`[op][handle][operation]`, one status byte) for an open
/// file handle (SH3-d2b). A conflicting holder answers WOULD_BLOCK, which `vfs_errno`
/// maps to EAGAIN — the errno both flock(2) with LOCK_NB and the fcntl lock calls
/// report. — TIMBERDOODLE
fn vfs_lock(fildes: c_int, operation: u8) -> Result<()> {
    use crate::header::errno::EBADF;

    let Some(FdSlot::File { handle, .. }) = fd_slot(fildes).map(|slot| *slot) else {
        return Err(Errno(EBADF));
    };
    let mut request = [0u8; LOCK_REQUEST_LEN];
    request[0] = VFS_OP_LOCK;
    request[1..5].copy_from_slice(&handle.to_le_bytes());
    request[5] = operation;
    vfs_status(&request)
}

/// Fetch one `OP_READ_DIR` page for `path` from entry `start` and lay whole `dirent`
/// records into `buf` (SH3-d2c). Each record carries the musl aarch64 layout std and
/// libc read: `d_ino` at 0, the resume cookie at 8, `d_reclen` at 16, `d_type` at 18
/// and the NUL-terminated name from 19 — the same shape relibc's DIR consumes. Records
/// never straddle the returned length, cookies are entry indexes (record i's cookie is
/// the index of the record after it), and a record that cannot fit is EINVAL for the
/// caller to grow the buffer and retry from the same cookie. — TIMBERDOODLE
fn dir_read_page(path: &[u8], start: u32, buf: &mut [u8]) -> Result<usize> {
    use crate::header::{
        dirent::{DT_DIR, DT_REG},
        errno::{EINVAL, EIO},
    };

    let mut request = [0u8; 7 + VFS_MAX_PATH];
    request[0] = VFS_OP_READ_DIR;
    request[1..5].copy_from_slice(&start.to_le_bytes());
    request[5..7].copy_from_slice(&(path.len() as u16).to_le_bytes());
    request[7..7 + path.len()].copy_from_slice(path);
    let mut reply = [0u8; VFS_READ_DIR_BUF];
    let n = vfs_call(&request[..7 + path.len()], &mut reply)?;
    if reply[0] != VFS_OK {
        return Err(vfs_errno(reply[0]));
    }
    if n < 2 {
        return Err(Errno(EIO));
    }
    let count = reply[1];
    let mut at = 2usize;
    let mut written = 0usize;
    for i in 0..u32::from(count) {
        let header = reply.get(at..at + VFS_DIR_RECORD_HEADER).ok_or(Errno(EIO))?;
        let flags = header[0];
        let name_len = usize::from(header[9]);
        if name_len > 255 {
            return Err(Errno(EIO));
        }
        let name = reply
            .get(at + VFS_DIR_RECORD_HEADER..at + VFS_DIR_RECORD_HEADER + name_len)
            .ok_or(Errno(EIO))?;
        at += VFS_DIR_RECORD_HEADER + name_len;
        let reclen = (19 + name_len + 1 + 7) & !7;
        if written + reclen > buf.len() {
            if written == 0 {
                return Err(Errno(EINVAL));
            }
            break;
        }
        let rec = &mut buf[written..written + reclen];
        rec.fill(0);
        let cookie = u64::from(start) + u64::from(i) + 1;
        rec[0..8].copy_from_slice(&cookie.to_le_bytes()); // d_ino: synthetic
        rec[8..16].copy_from_slice(&cookie.to_le_bytes()); // d_off: resume cookie
        rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        rec[18] = if flags & VFS_DIR_RECORD_DIRECTORY != 0 {
            DT_DIR as u8
        } else {
            DT_REG as u8
        };
        rec[19..19 + name_len].copy_from_slice(name);
        written += reclen;
    }
    Ok(written)
}

/// Encode a one-path SH3 request (`[op][len u16][path]`) and exchange it.
fn vfs_path_status(op: u8, path: &[u8]) -> Result<()> {
    use crate::header::errno::ENAMETOOLONG;

    if path.len() > VFS_MAX_PATH {
        return Err(Errno(ENAMETOOLONG));
    }
    let mut request = [0u8; 3 + VFS_MAX_PATH];
    request[0] = op;
    request[1..3].copy_from_slice(&(path.len() as u16).to_le_bytes());
    request[3..3 + path.len()].copy_from_slice(path);
    vfs_status(&request[..3 + path.len()])
}

/// Encode a two-path SH3 request (`[op][len u16][from][len u16][to]`) and exchange it.
fn vfs_two_path_status(op: u8, from: &[u8], to: &[u8]) -> Result<()> {
    use crate::header::errno::ENAMETOOLONG;

    if from.len() > VFS_MAX_PATH || to.len() > VFS_MAX_PATH {
        return Err(Errno(ENAMETOOLONG));
    }
    let mut request = [0u8; 5 + 2 * VFS_MAX_PATH];
    request[0] = op;
    request[1..3].copy_from_slice(&(from.len() as u16).to_le_bytes());
    request[3..3 + from.len()].copy_from_slice(from);
    let at = 3 + from.len();
    request[at..at + 2].copy_from_slice(&(to.len() as u16).to_le_bytes());
    request[at + 2..at + 2 + to.len()].copy_from_slice(to);
    vfs_status(&request[..at + 2 + to.len()])
}

/// Ask the server to record a synthetic mode (SH3-d). FAT stores no permission bits; the
/// mount-scoped server keeps the value in its mode table and reports it from stat. A
/// per-launch server owns no table and answers `NOT_SUPPORTED`, which callers treat as
/// "keep the synthetic default", not as a failure.
fn vfs_chmod(path: &[u8], mode: mode_t) -> Result<()> {
    use crate::header::errno::ENAMETOOLONG;

    if path.len() > VFS_MAX_PATH {
        return Err(Errno(ENAMETOOLONG));
    }
    let mut request = [0u8; 5 + VFS_MAX_PATH];
    request[0] = VFS_OP_CHMOD;
    request[1..3].copy_from_slice(&((mode & 0o7777) as u16).to_le_bytes());
    request[3..5].copy_from_slice(&(path.len() as u16).to_le_bytes());
    request[5..5 + path.len()].copy_from_slice(path);
    vfs_status(&request[..5 + path.len()])
}

/// Fill a `stat` from the fields FAT can honestly answer. Device, inode, owner, and
/// timestamps stay zero — the wire reply carries none — while kind, mode (synthetic, per
/// SH3-d), and size are real. `stat::default()` seeds the struct so every field is
/// written even when the caller's out-pointer is uninitialized. — TIMBERDOODLE
fn fill_stat(mut buf: Out<'_, stat>, file_type: mode_t, mode: u16, nlink: u64, size: u64) {
    let mut st = stat::default();
    st.st_mode = file_type | mode_t::from(mode & 0o7777);
    st.st_nlink = nlink as _;
    st.st_size = size as _;
    st.st_blksize = 512;
    st.st_blocks = size.div_ceil(512) as _;
    unsafe { *buf.as_mut_ptr() = st };
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
fn fd_nonblock(fildes: c_int) -> bool {
    usize::try_from(fildes)
        .ok()
        .filter(|&index| index < FD_SLOTS)
        .is_some_and(|index| unsafe { *(&raw const FD_NONBLOCK).cast::<bool>().add(index) })
}

fn set_fd_nonblock(fildes: c_int, nonblock: bool) {
    if let Some(index) = usize::try_from(fildes).ok().filter(|&index| index < FD_SLOTS) {
        unsafe { *(&raw mut FD_NONBLOCK).cast::<bool>().add(index) = nonblock };
    }
}

fn epoll_set(set: u8) -> Option<&'static mut EpollSet> {
    let index = usize::from(set);
    if index >= EPOLL_SETS {
        return None;
    }
    unsafe {
        (&raw mut EPOLL_TABLE)
            .cast::<Option<EpollSet>>()
            .add(index)
            .as_mut()?
            .as_mut()
    }
}

fn epoll_set_entry(set: u8) -> Option<&'static mut Option<EpollSet>> {
    let index = usize::from(set);
    if index >= EPOLL_SETS {
        return None;
    }
    unsafe {
        (&raw mut EPOLL_TABLE)
            .cast::<Option<EpollSet>>()
            .add(index)
            .as_mut()
    }
}

/// Release an epoll set: unbind every watched channel, close the port, free the slot.
fn epoll_release(set: u8) {
    let Some(entry) = epoll_set_entry(set) else {
        return;
    };
    if let Some(instance) = entry.take() {
        for watched in instance.entries.iter().flatten() {
            if watched.bound {
                let _ = port_unbind_raw(instance.port, watched.handle);
            }
        }
        close_handle(instance.port);
    }
}

fn port_create_raw() -> Option<u32> {
    syscall_ok(syscall4(SYSCALL_PORT_CREATE, 0, 0, 0, 0))
}

fn port_bind_raw(port: u32, object: u32) -> u64 {
    syscall4(SYSCALL_PORT_BIND, u64::from(port), u64::from(object), 0, 0)
}

fn port_unbind_raw(port: u32, object: u32) -> u64 {
    syscall4(SYSCALL_PORT_UNBIND, u64::from(port), u64::from(object), 0, 0)
}

/// Block until a bound object signals; the answer is that object's koid (0 on failure).
fn port_wait_raw(port: u32) -> u64 {
    syscall4(SYSCALL_PORT_WAIT, u64::from(port), 0, 0, 0)
}

fn timer_create_raw(delay_ns: u64) -> Option<u32> {
    syscall_ok(syscall4(SYSCALL_TIMER_CREATE, delay_ns, 0, 0, 0))
}

/// Park until the process behind `process` terminates (SH4 waitpid): a private port bound
/// to the process object, which the kernel signals `TERMINATED` — replayed at bind time
/// when the child already ended. Any wake releases the caller; it re-reaps and re-checks.
fn wait_process_terminated(process: u32) -> Result<()> {
    use crate::header::errno::EIO;

    let port = port_create_raw().ok_or(Errno(EIO))?;
    let bound = port_bind_raw(port, process) == 0;
    let woke = bound && port_wait_raw(port) != 0;
    if bound {
        let _ = port_unbind_raw(port, process);
    }
    close_handle(port);
    if woke {
        Ok(())
    } else {
        Err(Errno(EIO))
    }
}

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

/// A spawned child (SH4): the retained process handle and the process koid `ProcessWait`
/// reports for it, or the collected exit code once reaped.
#[derive(Clone, Copy)]
enum ChildState {
    Live,
    Exited(u64),
}

#[derive(Clone, Copy)]
struct ChildSlot {
    state: ChildState,
    process: u32,
    koid: u64,
    /// SH6-b: this side of the child's signal channel (0 = none) and the dispositions the
    /// child last reported up it: signals it catches and signals it ignores.
    signal: u32,
    caught: u64,
    ignored: u64,
}

static mut CHILD_TABLE: [Option<ChildSlot>; MAX_CHILDREN] = [None; MAX_CHILDREN];

fn child_slot(pid: c_int) -> Option<&'static mut ChildSlot> {
    let index = usize::try_from(pid.checked_sub(CHILD_PID_BASE)?).ok()?;
    if index >= MAX_CHILDREN {
        return None;
    }
    unsafe {
        (&raw mut CHILD_TABLE)
            .cast::<Option<ChildSlot>>()
            .add(index)
            .as_mut()?
            .as_mut()
    }
}

fn child_by_koid(koid: u64) -> Option<&'static mut ChildSlot> {
    for index in 0..MAX_CHILDREN {
        let slot = unsafe {
            (&raw mut CHILD_TABLE)
                .cast::<Option<ChildSlot>>()
                .add(index)
                .as_mut()?
        };
        if slot.as_ref().is_some_and(|child| child.koid == koid) {
            return slot.as_mut();
        }
    }
    None
}

fn child_alloc(process: u32, koid: u64, signal: u32) -> Option<c_int> {
    for index in 0..MAX_CHILDREN {
        let slot = unsafe {
            (&raw mut CHILD_TABLE)
                .cast::<Option<ChildSlot>>()
                .add(index)
                .as_mut()?
        };
        if slot.is_none() {
            *slot = Some(ChildSlot {
                state: ChildState::Live,
                process,
                koid,
                signal,
                caught: 0,
                ignored: 0,
            });
            return Some(CHILD_PID_BASE + index as c_int);
        }
    }
    None
}

fn child_entry(pid: c_int) -> Option<&'static mut Option<ChildSlot>> {
    let index = usize::try_from(pid.checked_sub(CHILD_PID_BASE)?).ok()?;
    if index >= MAX_CHILDREN {
        return None;
    }
    unsafe {
        (&raw mut CHILD_TABLE)
            .cast::<Option<ChildSlot>>()
            .add(index)
            .as_mut()
    }
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
/// Bit 0 of FdSlot::File::flags: O_APPEND (SH3-d2d).
const FD_FILE_APPEND: u8 = 1 << 0;
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
/// Descriptors other than `skip` that name the same channel handle (SH4): `dup2` copies a
/// channel slot by handle, so the kernel handle is released only with its last alias —
/// otherwise a pipe writer's `close` would never let the reader see PEER_CLOSED.
fn channel_alias_count(handle: u32, skip: c_int) -> usize {
    let mut count = 0;
    for index in 0..FD_SLOTS {
        if index as c_int == skip {
            continue;
        }
        match fd_slot(index as c_int) {
            Some(FdSlot::Input(other)) | Some(FdSlot::Output(other)) if *other == handle => {
                count += 1;
            }
            _ => {}
        }
    }
    count
}

/// True unless `source` itself names `handle` (dup2 onto an alias of the source).
fn handle_matches_no_source(handle: u32, source: FdSlot) -> bool {
    !matches!(source, FdSlot::Input(other) | FdSlot::Output(other) if other == handle)
}

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
            STARTUP_TAG_SIGNAL => install_startup_handle(&SIGNAL_HANDLE, handle),
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
                // A pipe or tty end (SH4): std's piped-output reader flips both pipes.
                FdSlot::Input(_) | FdSlot::Output(_) => {
                    set_fd_nonblock(fd, value != 0);
                    Ok(0)
                }
                FdSlot::Closed => Err(Errno(EBADF)),
                _ => Err(Errno(ENOTTY)),
            };
        }

        match fd_slot(fd).map(|slot| *slot) {
            None | Some(FdSlot::Closed) => return Err(Errno(EBADF)),
            Some(FdSlot::File { .. })
            | Some(FdSlot::Socket { .. })
            | Some(FdSlot::Dir { .. })
            | Some(FdSlot::Epoll { .. }) => {
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
        set_fd_nonblock(fildes, false);
        match core::mem::replace(slot, FdSlot::Closed) {
            FdSlot::Closed => Err(Errno(EBADF)),
            FdSlot::Epoll { set } => {
                epoll_release(set);
                Ok(())
            }
            FdSlot::Input(handle) | FdSlot::Output(handle) => {
                // Release the channel end with its last alias (SH4): a pipe peer must see
                // PEER_CLOSED when this process is done writing or reading.
                if channel_alias_count(handle, fildes) == 0 {
                    pipe_stash_drop(handle);
                    close_handle(handle);
                }
                Ok(())
            }
            FdSlot::Socket { id, .. } => {
                // Release the session's socket when this was its last descriptor; a
                // dup2 alias keeps it alive, and a dead session cannot fail the close.
                if socket_alias_count(id, fildes) == 0 {
                    let _ = net_call(KNS1_OP_CLOSE, id, 0, 0, &[]);
                }
                Ok(())
            }
            FdSlot::File { handle, .. } => {
                // Release the server-side slot (SH3-d2d): a vfs handle is a server-table
                // index with no lifetime of its own, so client close is what frees it.
                // Best-effort — a close never surfaces server failure.
                let mut request = [0u8; CLOSE_REQUEST_LEN];
                request[0] = VFS_OP_CLOSE;
                request[1..5].copy_from_slice(&handle.to_le_bytes());
                let mut reply = [0u8; 1];
                let _ = vfs_call(&request, &mut reply);
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
        match target {
            FdSlot::Socket { id, .. } => {
                *fd_slot(fildes2).ok_or(Errno(EBADF))? = FdSlot::Closed;
                if socket_alias_count(id, fildes2) == 0 {
                    let _ = net_call(KNS1_OP_CLOSE, id, 0, 0, &[]);
                }
            }
            FdSlot::Input(handle) | FdSlot::Output(handle) => {
                // The replaced channel end is released with its last alias (SH4).
                *fd_slot(fildes2).ok_or(Errno(EBADF))? = FdSlot::Closed;
                if channel_alias_count(handle, fildes2) == 0 && handle_matches_no_source(handle, source) {
                    close_handle(handle);
                }
            }
            _ => {}
        }
        set_fd_nonblock(fildes2, fd_nonblock(fildes));
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

    /// SH5: end this thread only. The kernel unmaps the thread's own stack once it can no
    /// longer touch it (relibc passes a zero size for a caller-provided stack), and the
    /// process's last thread ending this way ends the process with status 0. — TEAL
    unsafe fn exit_thread(stack_base: *mut (), stack_size: usize) -> ! {
        let _ = syscall4(
            SYSCALL_THREAD_EXIT,
            stack_base as u64,
            stack_size as u64,
            0,
            0,
        );
        loop {
            core::hint::spin_loop();
        }
    }

    fn fchdir(fildes: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fchmodat(dirfd: c_int, path: Option<CStr>, mode: mode_t, flags: c_int) -> Result<()> {
        use crate::header::{
            errno::{EINVAL, EOPNOTSUPP},
            fcntl::{AT_EMPTY_PATH, AT_SYMLINK_NOFOLLOW},
        };

        if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
            return Err(Errno(EINVAL));
        }
        let _ = mode;
        let Some(path) = path.map(|p| p.to_bytes()).filter(|p| !p.is_empty()) else {
            // fchmod on an open descriptor. FAT records no modes and the synthetic table
            // is path-keyed, so the fd form succeeds as a documented no-op on an open
            // file — std::fs::copy's set_permissions chain depends on that succeeding
            // (JOURNAL/869). A non-file descriptor stays explicit.
            if flags & AT_EMPTY_PATH != 0
                && matches!(fd_slot(dirfd).map(|slot| *slot), Some(FdSlot::File { .. }))
            {
                return Ok(());
            }
            return Err(Errno(EOPNOTSUPP));
        };
        let mut resolved = [0u8; VFS_MAX_PATH];
        let path = at_path(dirfd, path, &mut resolved)?;
        vfs_chmod(path, mode)
    }

    fn fchownat(fildes: c_int, path: CStr, owner: uid_t, group: gid_t, flags: c_int) -> Result<()> {
        Err(unsupported())
    }

    fn fdatasync(fildes: c_int) -> Result<()> {
        Err(unsupported())
    }

    /// Advisory locks over the vfs `OP_LOCK` (SH3-d2b). POSIX operation bits are
    /// LOCK_SH=1, LOCK_EX=2, LOCK_NB=4, LOCK_UN=8; the wire carries exactly one of
    /// SH=1, EX=2, UN=4, so LOCK_UN maps 8 to 4 and LOCK_NB is stripped. The server's
    /// lock is non-blocking by design: a conflicting request answers EAGAIN, which is
    /// also what a LOCK_NB caller expects. The ABI has no sleep primitive to park a
    /// blocking caller, so a contending blocking request deviates the same way —
    /// recorded in JOURNAL/867; LOCK_NB is exact. — TIMBERDOODLE
    fn flock(fd: c_int, operation: c_int) -> Result<()> {
        use crate::header::sys_file::{LOCK_EX, LOCK_NB, LOCK_SH, LOCK_UN};

        let wire = match operation & !LOCK_NB {
            LOCK_SH => VFS_LOCK_SH,
            LOCK_EX => VFS_LOCK_EX,
            LOCK_UN => VFS_LOCK_UN,
            _ => return Err(Errno(crate::header::errno::EINVAL)),
        };
        vfs_lock(fd, wire)
    }

    /// stat/lstat/fstat all land here (the Pal defaults route to this one method). Path
    /// stats run over `OP_STAT`; an fd stat (`AT_EMPTY_PATH`) answers from the descriptor
    /// table, where a file's size is the one recorded at open — a documented admission
    /// until the fd→path table arrives with SH3-d2c. — TIMBERDOODLE
    fn fstatat(fildes: c_int, path: Option<CStr>, buf: Out<stat>, flags: c_int) -> Result<()> {
        use crate::header::{
            errno::{EBADF, EINVAL, EIO, ENOENT},
            fcntl::{AT_EMPTY_PATH, AT_SYMLINK_NOFOLLOW},
        };

        if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
            return Err(Errno(EINVAL));
        }
        let path = path.map(|p| p.to_bytes()).filter(|p| !p.is_empty());
        let Some(path) = path else {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(Errno(ENOENT));
            }
            let slot = fd_slot(fildes).ok_or(Errno(EBADF))?;
            match *slot {
                FdSlot::Closed => return Err(Errno(EBADF)),
                FdSlot::File { size, .. } => {
                    fill_stat(buf, 0o100000, 0o644, 1, size); // S_IFREG
                    return Ok(());
                }
                FdSlot::Input(_) | FdSlot::Output(_) => {
                    // A granted channel reads as a character device, as POSIX tty/pipe
                    // fstat does.
                    fill_stat(buf, 0o020000, 0o666, 1, 0); // S_IFCHR
                    return Ok(());
                }
                FdSlot::Socket { .. } => {
                    fill_stat(buf, 0o140000, 0o666, 1, 0); // S_IFSOCK
                    return Ok(());
                }
                FdSlot::Epoll { .. } => {
                    // An epoll instance is an anonymous inode, as on Linux.
                    fill_stat(buf, 0o020000, 0o600, 1, 0); // S_IFCHR
                    return Ok(());
                }
                FdSlot::Dir { .. } => {
                    // The same synthetic directory shape the path branch reports
                    // (SH3-d2c): fdopendir's S_IFDIR gate and stat's nlink ride on it.
                    fill_stat(buf, 0o040000, 0o755, 2, 0); // S_IFDIR
                    return Ok(());
                }
            }
        };
        let mut resolved = [0u8; VFS_MAX_PATH];
        let path = at_path(fildes, path, &mut resolved)?;
        let mut request = [0u8; 3 + VFS_MAX_PATH];
        request[0] = VFS_OP_STAT;
        request[1..3].copy_from_slice(&(path.len() as u16).to_le_bytes());
        request[3..3 + path.len()].copy_from_slice(path);
        let mut reply = [0u8; VFS_STAT_REPLY_LEN];
        let n = vfs_call(&request[..3 + path.len()], &mut reply)?;
        if reply[0] != VFS_OK {
            return Err(vfs_errno(reply[0]));
        }
        if n < VFS_STAT_REPLY_LEN {
            return Err(Errno(EIO));
        }
        let kind = reply[1];
        let mode = u16::from_le_bytes(reply[2..4].try_into().unwrap());
        let size = u64::from_le_bytes(reply[4..12].try_into().unwrap());
        let (file_type, nlink): (mode_t, u64) = match kind {
            VFS_STAT_KIND_DIRECTORY => (0o040000, 2), // S_IFDIR
            VFS_STAT_KIND_FILE => (0o100000, 1),      // S_IFREG
            _ => (0, 0),
        };
        fill_stat(buf, file_type, mode, nlink, size);
        Ok(())
    }

    fn fstatvfs(fildes: c_int, buf: Out<statvfs>) -> Result<()> {
        Err(unsupported())
    }

    /// F_GETFL / F_SETFL with O_NONBLOCK: the only descriptor flag this backend owns.
    fn fcntl(fildes: c_int, cmd: c_int, arg: c_ulonglong) -> Result<c_int> {
        use crate::header::bits_open_flags::{O_APPEND, O_NONBLOCK};
        use crate::header::errno::{EBADF, EINVAL, EMFILE};
        use crate::header::fcntl::{
            F_DUPFD, F_DUPFD_CLOEXEC, F_GETFD, F_GETFL, F_GETLK, F_RDLCK, F_SETFD, F_SETFL,
            F_SETLK, F_SETLKW, F_UNLCK, F_WRLCK, flock as flock_record,
        };

        let slot = fd_slot(fildes).ok_or(Errno(EBADF))?;
        match cmd {
            F_GETFL => match *slot {
                FdSlot::Closed => Err(Errno(EBADF)),
                FdSlot::Socket { flags, .. } => Ok(if flags & FD_SOCK_NONBLOCK != 0 {
                    O_NONBLOCK
                } else {
                    0
                }),
                FdSlot::File { flags, .. } => Ok(if flags & FD_FILE_APPEND != 0 {
                    O_APPEND
                } else {
                    0
                }),
                FdSlot::Input(_) | FdSlot::Output(_) => Ok(if fd_nonblock(fildes) {
                    O_NONBLOCK
                } else {
                    0
                }),
                _ => Ok(0),
            },
            F_SETFL => match slot {
                FdSlot::Input(_) | FdSlot::Output(_) => {
                    set_fd_nonblock(fildes, (arg as c_int) & O_NONBLOCK != 0);
                    Ok(0)
                }
                FdSlot::Socket { flags, .. } => {
                    *flags = if (arg as c_int) & O_NONBLOCK != 0 {
                        FD_SOCK_NONBLOCK
                    } else {
                        0
                    };
                    Ok(0)
                }
                FdSlot::File { flags, .. } => {
                    *flags = if (arg as c_int) & O_APPEND != 0 {
                        FD_FILE_APPEND
                    } else {
                        0
                    };
                    Ok(0)
                }
                FdSlot::Closed => Err(Errno(EBADF)),
                _ => Err(Errno(EINVAL)),
            },
            // fdopendir sets FD_CLOEXEC; there is no exec on kumo, so the flag is
            // inherently set and both calls are no-ops on any open slot (SH3-d2c).
            F_GETFD | F_SETFD => match *slot {
                FdSlot::Closed => Err(Errno(EBADF)),
                _ => Ok(0),
            },
            // POSIX record locks are whole-file here (SH3-d2b): the wire lock has no
            // range, so l_start/l_len/l_whence are accepted and ignored. The C fcntl
            // passes the caller's struct pointer as c_ulonglong without translation.
            // F_SETLKW shares F_SETLK's one-shot answer for the flock deviation's
            // reason: no sleep primitive exists to park a contending caller.
            F_GETLK | F_SETLK | F_SETLKW => {
                if !matches!(*slot, FdSlot::File { .. }) {
                    return Err(Errno(EBADF));
                }
                if arg == 0 {
                    return Err(Errno(EINVAL));
                }
                let record = unsafe { &mut *(arg as usize as *mut flock_record) };
                if cmd == F_GETLK {
                    // No wire query exists; this backend knows of no conflicting holder
                    // when asked, so the whole-file answer is F_UNLCK. Recorded
                    // approximation (JOURNAL/867).
                    record.l_type = F_UNLCK as _;
                    return Ok(0);
                }
                let operation = match record.l_type as c_int {
                    F_RDLCK => VFS_LOCK_SH,
                    F_WRLCK => VFS_LOCK_EX,
                    F_UNLCK => VFS_LOCK_UN,
                    _ => return Err(Errno(EINVAL)),
                };
                vfs_lock(fildes, operation)?;
                Ok(0)
            }
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

    /// Page OP_READ_DIR from the caller's entry-index cookie (SH3-d2c). Only whole,
    /// 8-byte-aligned dirent records are emitted; a record that does not fit the
    /// caller's buffer is the grow signal (EINVAL, retried with the same cookie), and a
    /// zero-record page is end-of-listing. The server is stateless across pages, so no
    /// per-fd cursor is kept here.
    fn getdents(fd: c_int, buf: &mut [u8], opaque_offset: u64) -> Result<usize> {
        use crate::header::errno::{EBADF, EINVAL};

        let Ok(start) = u32::try_from(opaque_offset) else {
            return Err(Errno(EINVAL));
        };
        let Some(FdSlot::Dir { path, path_len, .. }) = fd_slot(fd).map(|slot| *slot) else {
            return Err(Errno(EBADF));
        };
        dir_read_page(&path[..usize::from(path_len)], start, buf)
    }

    /// The cookie is the entry index itself and the server pages statelessly, so a
    /// seek has nothing to do beyond validating the descriptor (SH3-d2c).
    fn dir_seek(fd: c_int, opaque_offset: u64) -> Result<()> {
        use crate::header::errno::{EBADF, EINVAL};

        if opaque_offset > u64::from(u32::MAX) {
            return Err(Errno(EINVAL));
        }
        match fd_slot(fd).map(|slot| *slot) {
            Some(FdSlot::Dir { .. }) => Ok(()),
            _ => Err(Errno(EBADF)),
        }
    }

    /// Read one record's length and resume cookie from the fixed musl dirent offsets
    /// (d_off at 8, d_reclen at 16). A zero or short record is malformed; returning
    /// None surfaces EIO rather than looping forever (SH3-d2c).
    unsafe fn dent_reclen_offset(this_dent: &[u8], offset: usize) -> Option<(u16, u64)> {
        let _ = offset;
        if this_dent.len() < 19 {
            return None;
        }
        let reclen = u16::from_le_bytes([this_dent[16], this_dent[17]]);
        if reclen < 19 {
            return None;
        }
        let next = u64::from_le_bytes(this_dent[8..16].try_into().unwrap());
        Some((reclen, next))
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
        thread_self_koid() as pid_t
    }

    fn gettimeofday(tp: Out<timeval>, tzp: Option<Out<timezone>>) -> Result<()> {
        Err(unsupported())
    }

    fn getuid() -> uid_t {
        0
    }

    fn linkat(fd1: c_int, oldpath: CStr, fd2: c_int, newpath: CStr, flags: c_int) -> Result<()> {
        use crate::header::errno::EINVAL;

        if flags != 0 {
            return Err(Errno(EINVAL));
        }
        let mut old_resolved = [0u8; VFS_MAX_PATH];
        let oldpath = at_path(fd1, oldpath.to_bytes(), &mut old_resolved)?;
        let mut new_resolved = [0u8; VFS_MAX_PATH];
        let newpath = at_path(fd2, newpath.to_bytes(), &mut new_resolved)?;
        // FAT has no hard links; the server answers NOT_SUPPORTED, which maps to
        // EOPNOTSUPP for the caller's copy fallback.
        vfs_two_path_status(VFS_OP_LINK, oldpath, newpath)
    }

    /// Cursor movement over the descriptor table (SH3-d2d). The wire is positional —
    /// READ and WRITE carry explicit offsets — so a seek is pure client state: SEEK_SET,
    /// SEEK_CUR and SEEK_END over the slot's cursor and its recorded size. Channels and
    /// sockets are not seekable (ESPIPE); directories have no byte cursor (EINVAL).
    /// — TIMBERDOODLE
    fn lseek(fildes: c_int, offset: off_t, whence: c_int) -> Result<off_t> {
        use crate::header::{
            errno::{EBADF, EINVAL, ESPIPE},
            unistd::{SEEK_CUR, SEEK_END, SEEK_SET},
        };

        let Some(slot) = fd_slot(fildes) else {
            return Err(Errno(EBADF));
        };
        let (cursor, size) = match *slot {
            FdSlot::Closed => return Err(Errno(EBADF)),
            FdSlot::Input(_) | FdSlot::Output(_) | FdSlot::Socket { .. } => {
                return Err(Errno(ESPIPE));
            }
            FdSlot::Dir { .. } | FdSlot::Epoll { .. } => return Err(Errno(EINVAL)),
            FdSlot::File { offset, size, .. } => (offset, size),
        };
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => i64::try_from(cursor).map_err(|_| Errno(EINVAL))?,
            SEEK_END => i64::try_from(size).map_err(|_| Errno(EINVAL))?,
            _ => return Err(Errno(EINVAL)),
        };
        let Some(next) = base.checked_add(offset) else {
            return Err(Errno(EINVAL));
        };
        if next < 0 {
            return Err(Errno(EINVAL));
        }
        let next = next as u64;
        if let FdSlot::File { offset, .. } = slot {
            *offset = next;
        }
        Ok(next as off_t)
    }

    fn mkdirat(fildes: c_int, path: CStr, mode: mode_t) -> Result<()> {
        let mut resolved = [0u8; VFS_MAX_PATH];
        let path = at_path(fildes, path.to_bytes(), &mut resolved)?;
        vfs_path_status(VFS_OP_MKDIR, path)?;
        // FAT has no permission bits; record the requested mode in the server's synthetic
        // table when there is one. A per-launch server answers NOT_SUPPORTED and the
        // directory simply keeps the default synthetic mode.
        let _ = vfs_chmod(path, mode);
        Ok(())
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
            sys_mman::{MAP_ANON, MAP_PRIVATE, MAP_STACK, PROT_READ, PROT_WRITE},
        };

        // SH6: MAP_STACK is a placement hint (std's linux alternate signal stack asks for
        // it); every anonymous mapping here is already stack-suitable. — TEAL
        let valid = addr.is_null()
            && len != 0
            && fildes == -1
            && off == 0
            && flags & !MAP_STACK == MAP_ANON | MAP_PRIVATE
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

    /// SH6: PROT_NONE at one end of a mapping made by [`Self::mmap`] — the guard page std's
    /// stack-overflow machinery puts under each alternate signal stack. Other protections
    /// and middle ranges are not supported yet (the kernel answers which). — TEAL
    unsafe fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> Result<()> {
        use crate::header::{errno::EINVAL, sys_mman::PROT_NONE};

        if prot != PROT_NONE {
            return Err(unsupported());
        }
        let Some(len) = len
            .checked_add(PAGE_SIZE - 1)
            .map(|len| len & !(PAGE_SIZE - 1))
        else {
            return Err(Errno(EINVAL));
        };
        if addr.is_null() || len == 0 || addr as usize % PAGE_SIZE != 0 {
            return Err(Errno(EINVAL));
        }
        match syscall4(SYSCALL_VMAR_PROTECT, 0, addr as u64, len as u64, 0) {
            0 => Ok(()),
            status if status as u32 as i32 == -8 => Err(unsupported()),
            _ => Err(Errno(EINVAL)),
        }
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

    /// SH5: release whole mappings made by [`Self::mmap`] (rounded up to the page as `mmap`
    /// rounded it). SH6: every mapping wholly inside the range goes, so the original range
    /// of a mapping with an `mprotect` guard hole releases the rest. A range that would
    /// split a mapping is refused with EINVAL; relibc's allocator never releases part of a
    /// segment on KUMO (`can_release_part`). The frames return to the kernel once nothing
    /// maps them. — TEAL
    unsafe fn munmap(addr: *mut c_void, len: usize) -> Result<()> {
        use crate::header::errno::EINVAL;

        let Some(len) = len
            .checked_add(PAGE_SIZE - 1)
            .map(|len| len & !(PAGE_SIZE - 1))
        else {
            return Err(Errno(EINVAL));
        };
        if addr.is_null() || len == 0 || addr as usize % PAGE_SIZE != 0 {
            return Err(Errno(EINVAL));
        }
        match syscall4(SYSCALL_VMAR_UNMAP, 0, addr as u64, len as u64, 0) {
            0 => Ok(()),
            _ => Err(Errno(EINVAL)),
        }
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
            errno::{EINVAL, EIO, EMFILE, ENAMETOOLONG, ENOTDIR},
            fcntl::{
                O_ACCMODE, O_APPEND, O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY,
            },
        };

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
        let path = at_path(dirfd, path, &mut resolved)?;
        // O_DIRECTORY (SH3-d2c): directories are opened as path records, not server
        // handles. One OP_STAT gates existence and kind, so opendir fails like POSIX
        // (ENOENT / ENOTDIR) instead of at the first listing. The bit carries two
        // numbers: relibc's headers (linux-forked) say 0x1_0000 while the Rust libc
        // crate's musl aarch64 table — what std compiled against, and what its
        // remove_dir_all passes — says 0x4000, so both are honored; 0x4000 is O_DIRECT
        // in relibc's numbering, which this volume cannot honor anyway (SH3-d2d).
        const O_DIRECTORY_COMPAT: c_int = 0x4000;
        if oflag & (O_DIRECTORY | O_DIRECTORY_COMPAT) != 0 {
            if options != VFS_OPEN_READ {
                return Err(Errno(EINVAL));
            }
            let mut stat_request = [0u8; 3 + VFS_MAX_PATH];
            stat_request[0] = VFS_OP_STAT;
            stat_request[1..3].copy_from_slice(&(path.len() as u16).to_le_bytes());
            stat_request[3..3 + path.len()].copy_from_slice(path);
            let mut stat_reply = [0u8; VFS_STAT_REPLY_LEN];
            let n = vfs_call(&stat_request[..3 + path.len()], &mut stat_reply)?;
            if stat_reply[0] != VFS_OK {
                return Err(vfs_errno(stat_reply[0]));
            }
            if n < VFS_STAT_REPLY_LEN {
                return Err(Errno(EIO));
            }
            if stat_reply[1] != VFS_STAT_KIND_DIRECTORY {
                return Err(Errno(ENOTDIR));
            }
            let fildes = fd_alloc().ok_or(Errno(EMFILE))?;
            let mut dir_path = [0u8; VFS_MAX_PATH];
            dir_path[..path.len()].copy_from_slice(path);
            if let Some(slot) = fd_slot(fildes) {
                *slot = FdSlot::Dir {
                    path: dir_path,
                    path_len: path.len() as u16,
                    cursor: 0,
                };
            }
            return Ok(fildes);
        }
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
                flags: if oflag & O_APPEND != 0 {
                    FD_FILE_APPEND
                } else {
                    0
                },
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

    /// A pipe over one channel pair (SH4): the read end in fildes[0], the write end in
    /// fildes[1], the two ends of the same channel.
    fn pipe2(mut fildes: Out<[c_int; 2]>, flags: c_int) -> Result<()> {
        use crate::header::errno::EMFILE;

        let _ = flags;
        let Some((first, second)) = channel_pair() else {
            return Err(Errno(EMFILE));
        };
        // Install each end before allocating the next: `fd_alloc` reports the lowest
        // CLOSED slot, so allocating both first returned the SAME descriptor twice and the
        // second install overwrote the first (SH4: the parent's pipe read landed on its own
        // write slot and answered EBADF).
        let Some(read_fd) = fd_alloc() else {
            close_handle(first);
            close_handle(second);
            return Err(Errno(EMFILE));
        };
        if let Some(slot) = fd_slot(read_fd) {
            *slot = FdSlot::Input(first);
        }
        let Some(write_fd) = fd_alloc() else {
            if let Some(slot) = fd_slot(read_fd) {
                *slot = FdSlot::Closed;
            }
            close_handle(first);
            close_handle(second);
            return Err(Errno(EMFILE));
        };
        if let Some(slot) = fd_slot(write_fd) {
            *slot = FdSlot::Output(second);
        }
        unsafe { *fildes.as_mut_ptr() = [read_fd, write_fd] };
        Ok(())
    }

    fn posix_fallocate(fd: c_int, offset: u64, length: NonZeroU64) -> Result<()> {
        Err(unsupported())
    }

    /// The per-fd form: resume from the slot's cursor and advance it by the records
    /// actually emitted, mirroring the lseek protocol the reference backends use
    /// (SH3-d2c). A record that cannot fit is EINVAL, as for getdents.
    fn posix_getdents(fildes: c_int, buf: &mut [u8]) -> Result<usize> {
        use crate::header::errno::EBADF;

        let Some(FdSlot::Dir {
            path,
            path_len,
            cursor,
        }) = fd_slot(fildes).map(|slot| *slot)
        else {
            return Err(Errno(EBADF));
        };
        let n = dir_read_page(&path[..usize::from(path_len)], cursor, buf)?;
        let mut at = 0usize;
        let mut emitted = 0u32;
        while at < n {
            let reclen = u16::from_le_bytes([buf[at + 16], buf[at + 17]]);
            at += usize::from(reclen);
            emitted += 1;
        }
        if let Some(FdSlot::Dir { cursor, .. }) = fd_slot(fildes) {
            *cursor += emitted;
        }
        Ok(n)
    }

    /// SH5: a sibling thread over `ThreadSpawn`, entering `__relibc_kumo_thread_entry` on
    /// the stack `pthread::create` prepared. The kernel's refusals (resident bound, kernel
    /// memory) are the "insufficient resources" EAGAIN. — TEAL
    unsafe fn rlct_clone(
        stack: *mut usize,
        _os_specific: &mut OsSpecific,
    ) -> Result<pthread::OsTid, Errno> {
        use crate::header::errno::EAGAIN;

        let (status, koid) = thread_spawn(thread_entry_address(), stack as u64);
        if status != 0 || koid == 0 {
            return Err(Errno(EAGAIN));
        }
        Ok(pthread::OsTid {
            thread_id: koid as usize,
        })
    }

    unsafe fn rlct_kill(os_tid: pthread::OsTid, signal: usize) -> Result<()> {
        Err(unsupported())
    }

    fn current_os_tid() -> pthread::OsTid {
        pthread::OsTid {
            thread_id: thread_self_koid() as usize,
        }
    }

    /// Read from an input channel or an open file. Short reads are normal on both — the
    /// channel delivers one message at a time and the file server answers at most
    /// [`VFS_READ_MAX`] bytes per exchange — so callers loop, which is what stdio does.
    /// End of input is a zero-length read, not an error.
    fn read(fildes: c_int, buf: &mut [u8]) -> Result<usize> {
        use crate::header::errno::{EAGAIN, EBADF, ENOTCONN};

        if buf.is_empty() {
            return Ok(0);
        }
        let slot = fd_slot(fildes).ok_or(Errno(EBADF))?;
        let (handle, offset, size) = match *slot {
            // A channel read parks until a message arrives, and answers 0 once the writer
            // is gone — which is exactly POSIX end-of-file, so it needs no translation.
            FdSlot::Input(handle) => {
                let n = channel_read_posix(handle, buf, fd_nonblock(fildes));
                return if n == u64::MAX {
                    Err(Errno(EBADF))
                } else if n as u32 as i32 == KUMO_SHOULD_WAIT {
                    // SH4: O_NONBLOCK and nothing queued yet (the peer is still open).
                    Err(Errno(EAGAIN))
                } else {
                    Ok((n as usize).min(buf.len()))
                };
            }
            FdSlot::File {
                handle,
                offset,
                size,
                ..
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

    /// Positional read: the wire read already carries an offset, so this is read()'s
    /// frame without the cursor update (SH3-d2d). O_APPEND does not affect reads.
    fn pread(fildes: c_int, buf: &mut [u8], offset: off_t) -> Result<usize> {
        use crate::header::errno::{EBADF, EINVAL};

        if offset < 0 {
            return Err(Errno(EINVAL));
        }
        let offset = offset as u64;
        let Some(FdSlot::File { handle, size, .. }) = fd_slot(fildes).map(|slot| *slot) else {
            return Err(Errno(EBADF));
        };
        if buf.is_empty() || offset >= size {
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
        Ok(got)
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
        use crate::header::errno::EINVAL;

        // RENAME_NOREPLACE and RENAME_EXCHANGE have no cheap FAT implementation; refusing
        // unknown flags rather than silently dropping them keeps the atomic-rename
        // promise honest.
        if flags != 0 {
            return Err(Errno(EINVAL));
        }
        let mut old_resolved = [0u8; VFS_MAX_PATH];
        let old_path = at_path(old_dir, old_path.to_bytes(), &mut old_resolved)?;
        let mut new_resolved = [0u8; VFS_MAX_PATH];
        let new_path = at_path(new_dir, new_path.to_bytes(), &mut new_resolved)?;
        vfs_two_path_status(VFS_OP_RENAME, old_path, new_path)
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

    /// posix_spawn over the kernel spawn pipeline (SH4). The child is loaded from THIS
    /// process's filesystem grant and receives exactly the tagged grants built here —
    /// stdio (inherited ends duplicated, or the file-action overrides), argv and env as
    /// VMOs — nothing ambient. File actions support Dup2 and Close on fds 0-2, the shape
    /// std's piped stdio uses; anything else is EOPNOTSUPP. — TIMBERDOODLE
    unsafe fn spawn(
        program: CStr,
        fac: Option<&crate::header::spawn::posix_spawn_file_actions_t>,
        fat: Option<&crate::header::spawn::posix_spawnattr_t>,
        argv: NulTerminated<*mut c_char>,
        envp: Option<NulTerminated<*mut c_char>>,
    ) -> Result<pid_t> {
        use crate::header::{
            errno::{E2BIG, EINVAL, EMFILE, EOPNOTSUPP},
            spawn::Action,
        };

        let _ = fat;
        let tags = [STARTUP_TAG_STDIN, STARTUP_TAG_STDOUT, STARTUP_TAG_STDERR];
        let mut args: [&[u8]; 16] = [b""; 16];
        let mut argn = 0usize;
        for arg in argv {
            if argn == args.len() {
                return Err(Errno(E2BIG));
            }
            args[argn] = unsafe { CStr::from_ptr(*arg) }.to_bytes();
            argn += 1;
        }
        if argn == 0 {
            return Err(Errno(EINVAL));
        }
        let mut envs: [&[u8]; 16] = [b""; 16];
        let mut envn = 0usize;
        if let Some(envp) = envp {
            for env in envp {
                if envn == envs.len() {
                    return Err(Errno(E2BIG));
                }
                envs[envn] = unsafe { CStr::from_ptr(*env) }.to_bytes();
                envn += 1;
            }
        }

        let mut stdio: [Option<(u8, u32)>; 3] = [None; 3];
        for fd in 0..3usize {
            let handle = match fd_slot(fd as c_int).map(|slot| *slot) {
                Some(FdSlot::Input(handle)) | Some(FdSlot::Output(handle)) if handle != 0 => handle,
                _ => continue,
            };
            let Some(duplicate) = handle_duplicate_raw(handle) else {
                continue;
            };
            stdio[fd] = Some((tags[fd], duplicate));
        }
        if let Some(fac) = fac {
            for action in fac {
                match action {
                    Action::Dup2(from, to) if (0..3).contains(&to) => {
                        let handle = match fd_slot(from).map(|slot| *slot) {
                            Some(FdSlot::Input(handle)) | Some(FdSlot::Output(handle))
                                if handle != 0 =>
                            {
                                handle
                            }
                            _ => return Err(Errno(EOPNOTSUPP)),
                        };
                        let Some(duplicate) = handle_duplicate_raw(handle) else {
                            return Err(Errno(EMFILE));
                        };
                        if let Some((_, previous)) = stdio[to as usize].take() {
                            close_handle(previous);
                        }
                        stdio[to as usize] = Some((tags[to as usize], duplicate));
                    }
                    Action::Close(fd) if (0..3).contains(&fd) => {
                        if let Some((_, previous)) = stdio[fd as usize].take() {
                            close_handle(previous);
                        }
                    }
                    _ => return Err(Errno(EOPNOTSUPP)),
                }
            }
        }

        spawn_elf(program, &args[..argn], &envs[..envn], &stdio)
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
        use crate::header::{errno::EINVAL, fcntl::AT_REMOVEDIR};

        if flags & !AT_REMOVEDIR != 0 {
            return Err(Errno(EINVAL));
        }
        let mut resolved = [0u8; VFS_MAX_PATH];
        let path = at_path(fd, path.to_bytes(), &mut resolved)?;
        let op = if flags & AT_REMOVEDIR != 0 {
            VFS_OP_RMDIR
        } else {
            VFS_OP_UNLINK
        };
        vfs_path_status(op, path)
    }

    /// Ownership is the wait (SH4): pids name table slots for processes this process
    /// retained from posix_spawn. Each ProcessWait pump reports one reaped child of this
    /// process (koid and exit code); the POSIX status word is the exited encoding
    /// `(code & 0xFF) << 8` (a killed child carries the kernel's 137), and a reported
    /// child's pid is released. WNOHANG checks without blocking; otherwise the caller parks
    /// on a port bound to the child process until it terminates. Only a specific pid is
    /// supported — `-1`/group waits answer ECHILD (recorded deviation). — TEAL
    fn waitpid(pid: pid_t, stat_loc: Option<Out<c_int>>, options: c_int) -> Result<pid_t> {
        use crate::header::errno::ECHILD;

        const WNOHANG: c_int = 1;
        loop {
            // Drain everything the kernel has reaped for this process, ours or a sibling's.
            loop {
                let (_, koid, code) = process_wait_pair();
                if koid == 0 {
                    break;
                }
                if let Some(target) = child_by_koid(koid) {
                    if matches!(target.state, ChildState::Live) {
                        target.state = ChildState::Exited(code);
                    }
                }
            }
            let Some(entry) = child_entry(pid) else {
                return Err(Errno(ECHILD));
            };
            let Some(slot) = entry.as_ref() else {
                return Err(Errno(ECHILD));
            };
            match slot.state {
                ChildState::Exited(code) => {
                    // SH6-a: a signal death carries SIGNALED_EXIT | signo (WIFSIGNALED).
                    let raw = if code & !0x7f == SIGNALED_EXIT {
                        (code & 0x7f) as c_int
                    } else {
                        ((code as c_int & 0xFF) << 8) as c_int
                    };
                    if let Some(mut out) = stat_loc {
                        unsafe { *out.as_mut_ptr() = raw };
                    }
                    close_handle(slot.process);
                    if slot.signal != 0 {
                        close_handle(slot.signal);
                    }
                    *entry = None;
                    return Ok(pid);
                }
                ChildState::Live if options & WNOHANG != 0 => return Ok(0),
                ChildState::Live => wait_process_terminated(slot.process)?,
            }
        }
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
                    let status = channel_write_bytes(handle, chunk);
                    if status != 0 {
                        // SH6-a: the reader is gone — SIGPIPE's disposition, then EPIPE.
                        if written == 0 && status as u32 as i32 == STATUS_PEER_CLOSED {
                            return Err(broken_pipe());
                        }
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
                flags,
            }) => {
                let mut total = 0usize;
                while total < buf.len() {
                    // O_APPEND (SH3-d2d): every chunk starts at the recorded end, so a
                    // growing file is never overwritten from a stale cursor.
                    if flags & FD_FILE_APPEND != 0 {
                        offset = size;
                    }
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
                        flags,
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

    /// Positional write: write()'s frame loop with an explicit offset, no O_APPEND
    /// (POSIX semantics), no cursor movement; the recorded size still grows so later
    /// bound checks see the extension (SH3-d2d).
    fn pwrite(fildes: c_int, buf: &[u8], offset: off_t) -> Result<usize> {
        use crate::header::errno::{EBADF, EINVAL, EIO};

        if offset < 0 {
            return Err(Errno(EINVAL));
        }
        let mut offset = offset as u64;
        let Some(FdSlot::File { handle, mut size, .. }) = fd_slot(fildes).map(|slot| *slot) else {
            return Err(Errno(EBADF));
        };
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
            let result = vfs_call(&request[..VFS_WRITE_HEADER_LEN + take], &mut reply);
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
            let written = u32::from_le_bytes(reply[1..5].try_into().unwrap()) as usize;
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
        if let Some(FdSlot::File { size: slot_size, .. }) = fd_slot(fildes) {
            *slot_size = size;
        }
        Ok(total)
    }

    fn verify() -> bool {
        true
    }
}

/// Epoll over one kernel port (SH4). Readable interest in a channel descriptor binds the
/// channel to the set's port; the kernel queues READABLE/PEER_CLOSED packets for it (level
/// state replayed at bind time), and `PortWait` names the channel that signalled. Writable
/// interest in a channel is answered at once — a channel write never parks. A timeout is a
/// one-shot timer bound to the same port. Files, directories and sockets refuse (EPERM), as
/// Linux refuses regular files. — TEAL
impl PalEpoll for Sys {
    fn epoll_create1(flags: c_int) -> Result<c_int> {
        use crate::header::errno::EMFILE;

        let _ = flags;
        let Some(set) = (0..EPOLL_SETS as u8).find(|&set| epoll_set(set).is_none()) else {
            return Err(Errno(EMFILE));
        };
        let port = port_create_raw().ok_or(Errno(EMFILE))?;
        let Some(fd) = fd_alloc() else {
            close_handle(port);
            return Err(Errno(EMFILE));
        };
        if let Some(entry) = epoll_set_entry(set) {
            *entry = Some(EpollSet {
                port,
                entries: [None; EPOLL_ENTRIES],
            });
        }
        if let Some(slot) = fd_slot(fd) {
            *slot = FdSlot::Epoll { set };
        }
        Ok(fd)
    }

    unsafe fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> Result<()> {
        use crate::header::{
            errno::{EBADF, EEXIST, EINVAL, ENOENT, ENOSPC, EPERM},
            sys_epoll::{EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, EPOLLIN},
        };

        let set = match fd_slot(epfd).map(|slot| *slot) {
            Some(FdSlot::Epoll { set }) => set,
            Some(FdSlot::Closed) | None => return Err(Errno(EBADF)),
            _ => return Err(Errno(EINVAL)),
        };
        let handle = match fd_slot(fd).map(|slot| *slot) {
            Some(FdSlot::Input(handle)) | Some(FdSlot::Output(handle)) => handle,
            Some(FdSlot::Closed) | None => return Err(Errno(EBADF)),
            _ => return Err(Errno(EPERM)),
        };
        let readable_end = matches!(fd_slot(fd).map(|slot| *slot), Some(FdSlot::Input(_)));
        let instance = epoll_set(set).ok_or(Errno(EBADF))?;
        let port = instance.port;
        let (events, data) = if event.is_null() {
            (0, 0)
        } else {
            let event = unsafe { core::ptr::read_unaligned(event) };
            (event.events, unsafe { event.data.u64 })
        };
        let existing = instance
            .entries
            .iter()
            .position(|entry| entry.is_some_and(|entry| entry.fd == fd));
        match op {
            EPOLL_CTL_ADD => {
                if existing.is_some() {
                    return Err(Errno(EEXIST));
                }
                let Some(index) = instance.entries.iter().position(Option::is_none) else {
                    return Err(Errno(ENOSPC));
                };
                let koid = handle_koid_raw(handle).ok_or(Errno(EBADF))?;
                let bound = readable_end && events & EPOLLIN != 0;
                if bound && port_bind_raw(port, handle) != 0 {
                    return Err(Errno(EBADF));
                }
                instance.entries[index] = Some(EpollEntry {
                    fd,
                    events,
                    data,
                    handle,
                    koid,
                    bound,
                });
                Ok(())
            }
            EPOLL_CTL_DEL => {
                let index = existing.ok_or(Errno(ENOENT))?;
                if let Some(entry) = instance.entries[index].take() {
                    if entry.bound {
                        let _ = port_unbind_raw(port, entry.handle);
                    }
                }
                Ok(())
            }
            EPOLL_CTL_MOD => {
                let index = existing.ok_or(Errno(ENOENT))?;
                let Some(entry) = instance.entries[index].as_mut() else {
                    return Err(Errno(ENOENT));
                };
                let want_bound = readable_end && events & EPOLLIN != 0;
                if want_bound && !entry.bound {
                    if port_bind_raw(port, entry.handle) != 0 {
                        return Err(Errno(EBADF));
                    }
                } else if !want_bound && entry.bound {
                    let _ = port_unbind_raw(port, entry.handle);
                }
                entry.events = events;
                entry.data = data;
                entry.bound = want_bound;
                Ok(())
            }
            _ => Err(Errno(EINVAL)),
        }
    }

    unsafe fn epoll_pwait(
        epfd: c_int,
        events: *mut epoll_event,
        maxevents: c_int,
        timeout: c_int,
        sigmask: *const sigset_t,
    ) -> Result<usize> {
        use crate::header::{
            errno::{EBADF, EFAULT, EINVAL, EIO},
            sys_epoll::{epoll_data, EPOLLIN, EPOLLOUT},
        };

        let _ = sigmask;
        let set = match fd_slot(epfd).map(|slot| *slot) {
            Some(FdSlot::Epoll { set }) => set,
            Some(FdSlot::Closed) | None => return Err(Errno(EBADF)),
            _ => return Err(Errno(EINVAL)),
        };
        if events.is_null() || maxevents <= 0 {
            return Err(Errno(EFAULT));
        }
        let max = maxevents as usize;
        let instance = *epoll_set(set).ok_or(Errno(EBADF))?;
        let mut count = 0usize;
        let emit = |count: &mut usize, ready: u32, data: u64| {
            if *count < max {
                let record = epoll_event {
                    events: ready,
                    data: epoll_data { u64: data },
                };
                unsafe { core::ptr::write_unaligned(events.add(*count), record) };
                *count += 1;
            }
        };

        // Writable interest is satisfied without waiting: a channel write never parks.
        for entry in instance.entries.iter().flatten() {
            if entry.events & EPOLLOUT != 0 && !entry.bound {
                emit(&mut count, EPOLLOUT, entry.data);
            }
        }
        if count > 0 {
            return Ok(count);
        }
        if !instance.entries.iter().flatten().any(|entry| entry.bound) || timeout == 0 {
            return Ok(0);
        }

        let mut timer: Option<(u32, u64)> = None;
        if timeout > 0 {
            let handle = timer_create_raw(timeout as u64 * 1_000_000).ok_or(Errno(EIO))?;
            let koid = handle_koid_raw(handle).unwrap_or(0);
            if port_bind_raw(instance.port, handle) != 0 {
                close_handle(handle);
                return Err(Errno(EIO));
            }
            timer = Some((handle, koid));
        }
        let result = loop {
            let source = port_wait_raw(instance.port);
            if source == 0 {
                break Err(Errno(EIO));
            }
            if timer.is_some_and(|(_, koid)| koid == source) {
                break Ok(0);
            }
            if let Some(entry) = instance
                .entries
                .iter()
                .flatten()
                .find(|entry| entry.bound && entry.koid == source)
            {
                // READABLE or PEER_CLOSED alike: the read will not park (data or EOF).
                emit(&mut count, EPOLLIN, entry.data);
                break Ok(count);
            }
            // A packet for a channel no longer watched: wait again.
        };
        if let Some((handle, _)) = timer {
            let _ = port_unbind_raw(instance.port, handle);
            close_handle(handle);
        }
        result
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

    /// SH6-b: signal a spawned child by pid, or its spawn tree by `-pid` (the KUMO
    /// process group). A caught signal is a message the child's handler thread runs; an
    /// ignored one is dropped; otherwise the default action applies here, as a kill whose
    /// status reads WIFSIGNALED. `pid` 0 is this process's own group: its children's trees,
    /// then itself. Any other pid (no relation to this process) is ESRCH. — TEAL
    fn kill(pid: pid_t, sig: c_int) -> Result<()> {
        use crate::header::errno::{EINVAL, EPERM, ESRCH};

        if sig < 0 || sig as usize >= SIG_COUNT {
            return Err(Errno(EINVAL));
        }
        let sig = sig as usize;
        if pid == -1 {
            return Err(Errno(EPERM));
        }
        if pid == 0 {
            if sig != 0 {
                forward_to_children(sig);
                run_disposition(sig);
            }
            return Ok(());
        }
        let group = pid < 0;
        let Some(slot) = child_slot(pid.checked_abs().ok_or(Errno(ESRCH))?) else {
            return Err(Errno(ESRCH));
        };
        if matches!(slot.state, ChildState::Exited(_)) {
            return Err(Errno(ESRCH));
        }
        if sig == 0 {
            return Ok(());
        }
        let status = signal_child(slot, sig, group) as u32 as i32;
        if status != 0 {
            return Err(Errno(-status));
        }
        Ok(())
    }

    fn sigqueue(pid: pid_t, sig: c_int, val: sigval) -> Result<()> {
        Err(unsupported())
    }

    /// SH6-b: `kill(-pgrp, sig)` — the spawn tree under `pgrp`.
    fn killpg(pgrp: pid_t, sig: c_int) -> Result<()> {
        use crate::header::errno::EINVAL;

        if pgrp <= 0 {
            return Err(Errno(EINVAL));
        }
        Self::kill(-pgrp, sig)
    }

    /// SH6-a: run `sig`'s disposition on this thread now — a handler is called, SIG_IGN
    /// and default-ignored signals return, and a default terminating action ends the
    /// process as killed by `sig` (rustc's `signal(sig, SIG_DFL); raise(sig)`). — TEAL
    fn raise(sig: c_int) -> Result<()> {
        use crate::header::errno::EINVAL;

        if sig == 0 {
            return Ok(());
        }
        if sig < 0 || sig as usize >= SIG_COUNT {
            return Err(Errno(EINVAL));
        }
        run_disposition(sig as usize);
        Ok(())
    }

    fn setitimer(which: c_int, new: &itimerval, old: Option<&mut itimerval>) -> Result<()> {
        Err(unsupported())
    }

    /// SH6-a: a real process-wide disposition table. Installing or removing a handler for
    /// SIGSEGV, SIGBUS or SIGILL re-arms the calling thread's kernel fault registration;
    /// other threads keep the registration they inherited at spawn (recorded deviation:
    /// re-arm per thread). SIGKILL and SIGSTOP cannot be caught or ignored. — TEAL
    fn sigaction(sig: c_int, act: Option<&sigaction>, oact: Option<&mut sigaction>) -> Result<()> {
        use crate::header::errno::EINVAL;

        if sig <= 0 || sig as usize >= SIG_COUNT {
            return Err(Errno(EINVAL));
        }
        let sig = sig as usize;
        if act.is_some() && (sig == SIGKILL_NO || sig == SIGSTOP_NO) {
            return Err(Errno(EINVAL));
        }
        if let Some(old) = oact {
            *old = unsafe { core::mem::zeroed() };
            old.sa_handler =
                unsafe { core::mem::transmute(SIG_HANDLER[sig].load(Ordering::Acquire)) };
            old.sa_flags = SIG_FLAGS[sig].load(Ordering::Acquire) as c_int;
            old.sa_mask = SIG_MASK[sig].load(Ordering::Acquire) as sigset_t;
        }
        if let Some(new) = act {
            let handler = new.sa_handler.map_or(0, |handler| handler as usize);
            SIG_FLAGS[sig].store(new.sa_flags as u32 as usize, Ordering::Release);
            SIG_MASK[sig].store(new.sa_mask as u64, Ordering::Release);
            SIG_HANDLER[sig].store(handler, Ordering::Release);
            if matches!(sig, SIGSEGV_NO | SIGBUS_NO | SIGILL_NO) {
                refresh_fault_registration();
            } else if is_async_signal(sig) {
                refresh_async_registration();
            }
        }
        Ok(())
    }

    /// SH6-a: this thread's alternate signal stack, handed to the kernel with the fault
    /// registration. EPERM while running on it; ENOMEM below MINSIGSTKSZ. — TEAL
    unsafe fn sigaltstack(ss: Option<&stack_t>, old_ss: Option<&mut stack_t>) -> Result<()> {
        use crate::header::errno::{EINVAL, ENOMEM, EPERM};

        let (base, size, on) = (ALT_STACK_BASE.get(), ALT_STACK_SIZE.get(), on_alt_stack());
        if let Some(new) = ss {
            if on {
                return Err(Errno(EPERM));
            }
            if new.ss_flags & !(SS_DISABLE_FLAG | SS_ONSTACK_FLAG) != 0 {
                return Err(Errno(EINVAL));
            }
            if new.ss_flags & SS_DISABLE_FLAG == 0 && new.ss_size < MIN_SIGNAL_STACK {
                return Err(Errno(ENOMEM));
            }
        }
        if let Some(old) = old_ss {
            old.ss_sp = base as *mut c_void;
            old.ss_size = size;
            old.ss_flags = if base == 0 {
                SS_DISABLE_FLAG
            } else if on {
                SS_ONSTACK_FLAG
            } else {
                0
            };
        }
        if let Some(new) = ss {
            if new.ss_flags & SS_DISABLE_FLAG != 0 {
                ALT_STACK_BASE.set(0);
                ALT_STACK_SIZE.set(0);
            } else {
                ALT_STACK_BASE.set(new.ss_sp as usize);
                ALT_STACK_SIZE.set(new.ss_size);
            }
            refresh_fault_registration();
        }
        Ok(())
    }

    /// SH6-a: nothing is ever pending — no asynchronous delivery exists before SH6-b.
    fn sigpending(set: &mut sigset_t) -> Result<()> {
        *set = 0;
        Ok(())
    }

    /// SH6-a: the per-thread mask is recorded and reported; with no asynchronous delivery
    /// yet it gates nothing, and synchronous faults ignore it as Linux does. — TEAL
    fn sigprocmask(how: c_int, set: Option<&sigset_t>, oset: Option<&mut sigset_t>) -> Result<()> {
        use crate::header::errno::EINVAL;

        const SIG_BLOCK: c_int = 0;
        const SIG_UNBLOCK: c_int = 1;
        const SIG_SETMASK: c_int = 2;
        // SIGKILL and SIGSTOP cannot be blocked.
        const UNBLOCKABLE: u64 = (1 << (SIGKILL_NO - 1)) | (1 << (SIGSTOP_NO - 1));
        let old = SIGNAL_MASK.get();
        if let Some(&set) = set {
            let set = set as u64;
            let new = match how {
                SIG_BLOCK => old | set,
                SIG_UNBLOCK => old & !set,
                SIG_SETMASK => set,
                _ => return Err(Errno(EINVAL)),
            };
            SIGNAL_MASK.set(new & !UNBLOCKABLE);
        }
        if let Some(oset) = oset {
            *oset = old as sigset_t;
        }
        Ok(())
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
