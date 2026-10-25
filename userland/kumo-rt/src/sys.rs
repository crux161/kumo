//j556
//j559
//j572
//j601
//j609

use kumo_abi::{Handle, Rights, Status, Syscall};

/// Execute a raw syscall: `x8` = number, `x0`-`x3` = args, returns `x0`.
///
/// No `nomem`: several syscalls pass buffer pointers the **kernel writes through**
/// (`ChannelRead`, `VmoRead`) or reads from (`DebugWrite`, `ChannelWrite`). The compiler
/// must treat the `svc` as a full memory barrier, or it is entitled to assume a
/// just-zeroed buffer is still zero after the kernel filled it (and, in release builds,
/// to fold the initrd entry walk into "all paths empty").
#[cfg(target_arch = "aarch64")]
fn syscall(num: Syscall, x0: u64, x1: u64, x2: u64, x3: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") num as u64,
            in("x0") x0,
            in("x1") x1,
            in("x2") x2,
            in("x3") x3,
            lateout("x0") ret,
            // The kernel's syscall path uses NEON (memcpy/memset in VmoWrite, frame
            // zeroing, etc.) and does NOT save/restore the caller's FP/SIMD state across
            // the trap. Without this the compiler may keep a live v-register (e.g. a
            // vectorized buffer copy) across the `svc` and read back garbage. `clobber_abi`
            // marks the C caller-saved set (incl. v0-v7/v16-v31) as clobbered.
            clobber_abi("C"),
            options(nostack),
        );
    }
    ret
}

/// AMD64 KUMO syscall ABI: `rax` = number, arguments in the SysV integer registers,
/// and the primary result returned in `rax`. `int 0x80` enters the ring-3 IDT gate.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn syscall(num: Syscall, x0: u64, x1: u64, x2: u64, x3: u64) -> u64 {
    let mut ret = num as u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") ret,
            in("rdi") x0,
            in("rsi") x1,
            in("rdx") x2,
            in("rcx") x3,
            clobber_abi("C"),
            options(nostack),
        );
    }
    ret
}

#[cfg(not(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_os = "none")
)))]
fn syscall(_num: Syscall, _x0: u64, _x1: u64, _x2: u64, _x3: u64) -> u64 {
    0
}

/// 6-argument variant: `x8` = number, `x0`-`x5` = args, returns `x0`.
#[cfg(target_arch = "aarch64")]
fn syscall6(num: Syscall, x0: u64, x1: u64, x2: u64, x3: u64, x4: u64, x5: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") num as u64,
            in("x0") x0,
            in("x1") x1,
            in("x2") x2,
            in("x3") x3,
            in("x4") x4,
            in("x5") x5,
            lateout("x0") ret,
            // See `syscall`: the kernel clobbers FP/SIMD across the trap.
            clobber_abi("C"),
            options(nostack),
        );
    }
    ret
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[allow(dead_code)]
fn syscall6(num: Syscall, x0: u64, x1: u64, x2: u64, x3: u64, x4: u64, x5: u64) -> u64 {
    let mut ret = num as u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") ret,
            in("rdi") x0,
            in("rsi") x1,
            in("rdx") x2,
            in("rcx") x3,
            in("r8") x4,
            in("r9") x5,
            clobber_abi("C"),
            options(nostack),
        );
    }
    ret
}

#[cfg(not(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_os = "none")
)))]
fn syscall6(_num: Syscall, _x0: u64, _x1: u64, _x2: u64, _x3: u64, _x4: u64, _x5: u64) -> u64 {
    0
}

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_os = "none")
))]
pub fn debug_write(ptr: *const u8, len: usize) -> u64 {
    syscall(Syscall::DebugWrite, ptr as u64, len as u64, 0, 0)
}

#[cfg(not(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_os = "none")
)))]
pub fn debug_write(_ptr: *const u8, _len: usize) -> u64 {
    0
}

#[cfg(target_arch = "aarch64")]
pub fn channel_create() -> u64 {
    syscall(Syscall::ChannelCreate, 0, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn channel_create() -> u64 {
    u64::MAX
}

/// Create a channel pair, returning both handles. `pair.0` = first, `pair.1` = second.
#[cfg(target_arch = "aarch64")]
pub fn channel_create_pair() -> (u64, u64) {
    let h0: u64;
    let h1: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") Syscall::ChannelCreate as u64,
            lateout("x0") h0,
            lateout("x1") h1,
            // See `syscall`: the kernel clobbers FP/SIMD across the trap.
            clobber_abi("C"),
            options(nostack),
        );
    }
    (h0, h1)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn channel_create_pair() -> (u64, u64) {
    (u64::MAX, u64::MAX)
}

/// Close one process-local handle. A successful close invalidates the handle
/// immediately; closing it again returns `BadHandle`.
#[cfg(any(
    all(target_arch = "aarch64", target_os = "none"),
    all(target_arch = "x86_64", target_os = "none")
))]
pub fn handle_close(handle: Handle) -> Status {
    syscall(Syscall::HandleClose, handle.0 as u64, 0, 0, 0) as Status
}

#[cfg(not(any(
    all(target_arch = "aarch64", target_os = "none"),
    all(target_arch = "x86_64", target_os = "none")
)))]
pub fn handle_close(_handle: Handle) -> Status {
    kumo_abi::Errno::NotSupported.status()
}

/// Turn a handle-returning syscall's raw `u64` result into a handle. Failure comes back as
/// a negative `Errno` widened to `u64` (`AccessDenied` is `0xffff_fffc`), never as
/// `u64::MAX`, so every value above `i32::MAX` is a status, not a handle; handle tables are
/// small and never number that high. `0` is `INVALID_HANDLE`. J601: the D0 probes compared
/// only against `0`/`u64::MAX`, so a refused mint reached `VmarMap` as a bogus handle and the
/// log said only "mapping failed". — TIMBERDOODLE
pub const fn syscall_handle(raw: u64) -> Option<Handle> {
    if raw != 0 && raw <= i32::MAX as u64 {
        Some(Handle(raw as u32))
    } else {
        None
    }
}

/// Duplicate a handle with a subset of its original rights.
/// The new handle shares the same kernel object.
#[cfg(target_arch = "aarch64")]
pub fn handle_duplicate(handle: Handle, rights: Rights) -> u64 {
    syscall(
        Syscall::HandleDuplicate,
        handle.0 as u64,
        rights.0 as u64,
        0,
        0,
    )
}

#[cfg(not(target_arch = "aarch64"))]
pub fn handle_duplicate(_handle: Handle, _rights: Rights) -> u64 {
    0
}

/// Write a message with an optional handle transfer. Pass `handle` = Handle(0) for
/// no transfer; any non-zero handle is removed from the sender's table and arrives
/// at the receiver.
#[cfg(target_arch = "aarch64")]
pub fn channel_write_with_handle(
    channel: Handle,
    ptr: *const u8,
    len: usize,
    handle: Handle,
) -> Status {
    syscall(
        Syscall::ChannelWrite,
        channel.0 as u64,
        ptr as u64,
        len as u64,
        handle.0 as u64,
    ) as Status
}

#[cfg(not(target_arch = "aarch64"))]
pub fn channel_write_with_handle(
    _channel: Handle,
    _ptr: *const u8,
    _len: usize,
    _handle: Handle,
) -> Status {
    kumo_abi::Errno::NotSupported.status()
}

/// Sleep until woken, but only if `*addr` still equals `expected`.
///
/// Returns `Ok` when a [`futex_wake`] released this thread, and `ShouldWait` when the word had
/// already changed and no sleep happened. Callers must treat a return as a *hint* and re-check
/// their own condition — a futex remembers nothing, so the loop is the contract:
///
/// ```ignore
/// while state.load(Acquire) != WANTED {
///     futex_wait(state.as_ptr(), OBSERVED);
/// }
/// ```
#[cfg(target_arch = "aarch64")]
pub fn futex_wait(addr: *const u32, expected: u32) -> Status {
    syscall(Syscall::FutexWait, addr as u64, u64::from(expected), 0, 0) as Status
}

#[cfg(not(target_arch = "aarch64"))]
pub fn futex_wait(_addr: *const u32, _expected: u32) -> Status {
    kumo_abi::Errno::NotSupported.status()
}

/// Wake up to `count` threads parked on `addr`. Returns how many were woken — waking none is
/// normal, not an error: it means nobody had arrived yet, and that waiter will find the updated
/// word instead of sleeping.
#[cfg(target_arch = "aarch64")]
pub fn futex_wake(addr: *const u32, count: u32) -> u64 {
    syscall(Syscall::FutexWake, addr as u64, u64::from(count), 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn futex_wake(_addr: *const u32, _count: u32) -> u64 {
    0
}

#[cfg(target_arch = "aarch64")]
pub fn channel_read(channel: Handle, buf: *mut u8, cap: usize) -> u64 {
    syscall(
        Syscall::ChannelRead,
        channel.0 as u64,
        buf as u64,
        cap as u64,
        0,
    )
}

#[cfg(not(target_arch = "aarch64"))]
pub fn channel_read(_channel: Handle, _buf: *mut u8, _cap: usize) -> u64 {
    0
}

/// Read from a channel, returning both the byte count and an attached handle.
/// When the peer called `channel_write_with_handle`, the handle travels with
/// the message and is returned here in the second element of the tuple.
#[cfg(target_arch = "aarch64")]
pub fn channel_read_with_handle(channel: Handle, buf: *mut u8, cap: usize) -> (usize, u64) {
    let ret: u64;
    let handle: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") Syscall::ChannelRead as u64,
            in("x0") channel.0 as u64,
            in("x1") buf as u64,
            in("x2") cap as u64,
            lateout("x0") ret,
            lateout("x1") handle,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (ret as usize, handle)
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn channel_read_with_handle(channel: Handle, buf: *mut u8, cap: usize) -> (usize, u64) {
    let mut ret = Syscall::ChannelRead as u64;
    let mut handle = cap as u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") ret,
            in("rdi") channel.0 as u64,
            in("rsi") buf as u64,
            inlateout("rdx") handle,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (ret as usize, handle)
}

#[cfg(not(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_os = "none")
)))]
pub fn channel_read_with_handle(_channel: Handle, _buf: *mut u8, _cap: usize) -> (usize, u64) {
    (0, 0)
}

pub const STARTUP_TAG_STDIN: u8 = b'i';
pub const STARTUP_TAG_STDOUT: u8 = b'o';
pub const STARTUP_TAG_STDERR: u8 = b'e';
pub const STARTUP_TAG_ARGV: u8 = b'a';
pub const STARTUP_TAG_CAP0: u8 = b'c';
/// j508: a client channel to a filesystem server speaking `kumo-vfs`. A program that
/// receives one can name files; a program that does not cannot, whatever it asks for.
pub const STARTUP_TAG_FS: u8 = b'f';
/// A client channel to the process's terminal line-discipline service. This is
/// separate from stdin/stdout data channels so control RPC replies cannot be
/// mistaken for stream bytes.
pub const STARTUP_TAG_TTY: u8 = b't';

/// Explicit capabilities supplied to a newly started program. Each present field came
/// from one transferred handle on the bootstrap channel; absent fields grant no authority.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Startup {
    pub stdin: Option<Handle>,
    pub stdout: Option<Handle>,
    pub stderr: Option<Handle>,
    pub argv: Option<Handle>,
    pub cap0: Option<Handle>,
    pub fs: Option<Handle>,
    pub tty: Option<Handle>,
}

impl Startup {
    /// Install one tagged startup handle. The last value for a known tag wins; the
    /// returned handle is no longer part of the startup set and must be closed.
    fn install(&mut self, tag: u8, handle: Handle) -> Option<Handle> {
        let slot = match tag {
            STARTUP_TAG_STDIN => &mut self.stdin,
            STARTUP_TAG_STDOUT => &mut self.stdout,
            STARTUP_TAG_STDERR => &mut self.stderr,
            STARTUP_TAG_ARGV => &mut self.argv,
            STARTUP_TAG_CAP0 => &mut self.cap0,
            STARTUP_TAG_FS => &mut self.fs,
            STARTUP_TAG_TTY => &mut self.tty,
            _ => return Some(handle),
        };
        slot.replace(handle)
    }
}

/// Drain a finite startup message and return its explicit capabilities. The sender
/// must close its endpoint after queueing the set; empty + peer-closed then returns
/// zero, so this loop terminates instead of parking. Unknown/duplicate handles are
/// closed rather than becoming ambient authority.
pub fn startup(bootstrap: Handle) -> Startup {
    let mut startup = Startup::default();
    let mut tag = [0u8; 1];
    loop {
        let (n, raw_handle) = channel_read_with_handle(bootstrap, tag.as_mut_ptr(), tag.len());
        if n == 0 {
            break;
        }
        if raw_handle == 0 {
            continue;
        }
        if let Some(unused) = startup.install(tag[0], Handle(raw_handle as u32)) {
            let _ = handle_close(unused);
        }
    }
    let _ = handle_close(bootstrap);
    startup
}

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_os = "none")
))]
pub fn channel_write(channel: Handle, ptr: *const u8, len: usize) -> Status {
    syscall(
        Syscall::ChannelWrite,
        channel.0 as u64,
        ptr as u64,
        len as u64,
        0,
    ) as Status
}

#[cfg(not(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_os = "none")
)))]
pub fn channel_write(_channel: Handle, _ptr: *const u8, _len: usize) -> Status {
    kumo_abi::Errno::NotSupported.status()
}

#[cfg(target_arch = "aarch64")]
pub fn vmo_write(vmo: Handle, offset: u64, buf: *const u8, len: usize) -> u64 {
    syscall(
        Syscall::VmoWrite,
        vmo.0 as u64,
        offset,
        buf as u64,
        len as u64,
    )
}

#[cfg(not(target_arch = "aarch64"))]
pub fn vmo_write(_vmo: Handle, _offset: u64, _buf: *const u8, _len: usize) -> u64 {
    0
}

#[cfg(target_arch = "aarch64")]
pub fn vmo_create(size: u64) -> u64 {
    syscall(Syscall::VmoCreate, size, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn vmo_create(_size: u64) -> u64 {
    u64::MAX
}

/// Allocate a physically-contiguous, zeroed DMA VMO of `size` bytes (page-rounded) and
/// return `(handle, phys_base)`. The handle maps like an ordinary VMO — pass
/// `VmarFlags::UNCACHED` for a Normal-NC (coherency-free) DMA mapping — while `phys_base` is
/// the device-visible address to program into a DMA engine. On failure `handle == u64::MAX`
/// and `phys_base == 0`. Mirrors `channel_read_with_handle`'s two-register return.
#[cfg(target_arch = "aarch64")]
pub fn vmo_create_contiguous(size: u64) -> (u64, u64) {
    let handle: u64;
    let phys: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") Syscall::VmoCreateContiguous as u64,
            in("x0") size,
            lateout("x0") handle,
            lateout("x1") phys,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (handle, phys)
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
pub fn vmo_create_contiguous(size: u64) -> (u64, u64) {
    let mut handle = Syscall::VmoCreateContiguous as u64;
    let mut phys: u64 = 0;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") handle,
            in("rdi") size,
            inlateout("rdx") phys,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (handle, phys)
}

#[cfg(not(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_os = "none")
)))]
pub fn vmo_create_contiguous(_size: u64) -> (u64, u64) {
    (u64::MAX, 0)
}

#[cfg(target_arch = "aarch64")]
pub fn vmo_read(vmo: Handle, offset: u64, buf: *mut u8, len: usize) -> u64 {
    syscall(
        Syscall::VmoRead,
        vmo.0 as u64,
        offset,
        buf as u64,
        len as u64,
    )
}

#[cfg(not(target_arch = "aarch64"))]
pub fn vmo_read(_vmo: Handle, _offset: u64, _buf: *mut u8, _len: usize) -> u64 {
    0
}

#[cfg(target_arch = "aarch64")]
pub fn process_create(vmar_base: u64, vmar_size: u64) -> u64 {
    syscall(Syscall::ProcessCreate, vmar_base, vmar_size, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn process_create(_vmar_base: u64, _vmar_size: u64) -> u64 {
    u64::MAX
}

/// Map a VMO into a process's address space (syscall `VmarMap`).
/// `flags` are [`kumo_abi::VmarFlags`] bits OR'd together.
#[cfg(target_arch = "aarch64")]
pub fn vmar_map(
    process: Handle,
    vmo: Handle,
    vmo_offset: u64,
    virt: u64,
    len: u64,
    flags: u64,
) -> u64 {
    syscall6(
        Syscall::VmarMap,
        process.0 as u64,
        vmo.0 as u64,
        vmo_offset,
        virt,
        len,
        flags,
    )
}

/// Map `vmo` into this process at an address the **kernel** chooses. Returns `(status, addr)`.
///
/// Userland cannot pick a base safely: a child's VMAR is 512 MiB from 0 already carrying its image,
/// its stacks and any device mappings, while Sora's is 2 GiB from a different base — no constant is
/// correct in both. The kernel knows every existing mapping, so it places this and reports where.
/// Address 0 is the "you choose" sentinel precisely because the first page is never handed out.
#[cfg(target_arch = "aarch64")]
pub fn vmar_map_anywhere(vmo: Handle, len: u64, flags: u64) -> (u64, u64) {
    let status: u64;
    let addr: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") Syscall::VmarMap as u64,
            in("x0") 0u64,           // process: self
            in("x1") vmo.0 as u64,
            in("x2") 0u64,           // vmo_offset
            in("x3") 0u64,           // virt: 0 asks the kernel to place it
            in("x4") len,
            in("x5") flags,
            lateout("x0") status,
            lateout("x1") addr,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (status, addr)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn vmar_map_anywhere(_vmo: Handle, _len: u64, _flags: u64) -> (u64, u64) {
    (u64::MAX, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn vmar_map(
    _process: Handle,
    _vmo: Handle,
    _vmo_offset: u64,
    _virt: u64,
    _len: u64,
    _flags: u64,
) -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn thread_create(process: Handle) -> u64 {
    syscall(Syscall::ThreadCreate, process.0 as u64, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn thread_create(_process: Handle) -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn thread_start(thread: Handle, entry: u64, sp: u64, arg: u64) -> u64 {
    syscall(Syscall::ThreadStart, thread.0 as u64, entry, sp, arg)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn thread_start(_thread: Handle, _entry: u64, _sp: u64, _arg: u64) -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn address_space_create(process: Handle, stack_virt: u64, stack_size: u64) -> u64 {
    syscall(
        Syscall::AddressSpaceCreate,
        process.0 as u64,
        stack_virt,
        stack_size,
        0,
    )
}

#[cfg(not(target_arch = "aarch64"))]
pub fn address_space_create(_process: Handle, _stack_virt: u64, _stack_size: u64) -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn process_run(process: Handle, entry: u64, sp: u64, arg: u64, arg2: u64, flags: u64) -> u64 {
    syscall6(
        Syscall::ProcessRun,
        process.0 as u64,
        entry,
        sp,
        arg,
        arg2,
        flags,
    )
}

#[cfg(not(target_arch = "aarch64"))]
pub fn process_run(
    _process: Handle,
    _entry: u64,
    _sp: u64,
    _arg: u64,
    _arg2: u64,
    _flags: u64,
) -> u64 {
    u64::MAX
}

/// Keep a diagnostic in the kernel log at `level` (`kumo_abi::sys::log_level`), J559. The console
/// shows it only while its level allows; `dmesg` always can. `debug_write` stays the call for
/// terminal output. Returns the bytes kept (at most 256 a call), or `u64::MAX`.
pub fn log_write(level: u8, bytes: &[u8]) -> u64 {
    let len = bytes.len().min(256);
    syscall(
        Syscall::LogWrite,
        u64::from(level),
        bytes.as_ptr() as u64,
        len as u64,
        0,
    )
}

/// Read the kernel log from byte `offset` into `buf`, holding `resource` with `MANAGE` (J559).
/// Returns the bytes copied (`u64::MAX` when refused) and the offset to read from next.
#[cfg(target_arch = "aarch64")]
pub fn log_read(resource: Handle, offset: u64, buf: &mut [u8]) -> (u64, u64) {
    let copied: u64;
    let next: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") Syscall::LogRead as u64,
            inlateout("x0") resource.0 as u64 => copied,
            inlateout("x1") offset => next,
            in("x2") buf.as_mut_ptr() as u64,
            in("x3") buf.len() as u64,
            clobber_abi("C"),
            options(nostack),
        );
    }
    (copied, next)
}

/// Without a second return register here, the next offset is assumed to follow on directly; a
/// ring that wrapped past `offset` is read again from its oldest line instead of skipped to.
#[cfg(not(target_arch = "aarch64"))]
pub fn log_read(resource: Handle, offset: u64, buf: &mut [u8]) -> (u64, u64) {
    let copied = syscall(
        Syscall::LogRead,
        resource.0 as u64,
        offset,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
    );
    let next = if copied == u64::MAX {
        offset
    } else {
        offset + copied
    };
    (copied, next)
}

/// Set the console log level (0-7), or with `kumo_abi::sys::LOG_LEVEL_QUERY` only report it.
/// Returns the previous level, or `u64::MAX` when refused. Needs `resource` with `MANAGE`.
pub fn log_level(resource: Handle, level: u64) -> u64 {
    syscall(Syscall::LogLevel, resource.0 as u64, level, 0, 0)
}

/// End a process this caller manages (J556). Needs `MANAGE` on `process`, which the handle
/// `process_create` returned carries. Returns 0 on success.
#[cfg(target_arch = "aarch64")]
pub fn process_kill(process: Handle) -> u64 {
    syscall(Syscall::ProcessKill, process.0 as u64, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn process_kill(_process: Handle) -> u64 {
    u64::MAX
}

/// P10-g: block until the async child process exits.
#[cfg(target_arch = "aarch64")]
pub fn process_wait() -> u64 {
    syscall(Syscall::ProcessWait, 0, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn process_wait() -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn interrupt_create(resource: Handle, irq: u32) -> u64 {
    syscall(
        Syscall::InterruptCreate,
        resource.0 as u64,
        irq as u64,
        0,
        0,
    )
}

#[cfg(not(target_arch = "aarch64"))]
pub fn interrupt_create(_resource: Handle, _irq: u32) -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn port_wait(port: Handle) -> u64 {
    syscall(Syscall::PortWait, port.0 as u64, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn port_wait(_port: Handle) -> u64 {
    0
}

/// Read the kernel monotonic clock in nanoseconds.
#[cfg(target_arch = "aarch64")]
pub fn clock_get() -> u64 {
    syscall(Syscall::ClockGet, 0, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn clock_get() -> u64 {
    0
}

/// Sleep for `delay_ns` of monotonic time, returning false if the wait could not be set up.
///
/// A one-shot timer bound to a private port, which is the only way a process blocks on time here.
/// `drv-i2c-hid` has hand-rolled this since its bring-up; hoisting it means a caller who wants to
/// wait until a deadline does not have to know that timers are delivered through ports.
#[cfg(target_arch = "aarch64")]
pub fn sleep_ns(delay_ns: u64) -> bool {
    let port_raw = port_create();
    if port_raw == u64::MAX {
        return false;
    }
    let port = Handle(port_raw as u32);
    let timer_raw = timer_create(delay_ns);
    if timer_raw == u64::MAX {
        let _ = handle_close(port);
        return false;
    }
    let timer = Handle(timer_raw as u32);
    let ok = port_bind(port, timer) == 0 && port_wait(port) != 0;
    let _ = handle_close(timer);
    let _ = handle_close(port);
    ok
}

#[cfg(not(target_arch = "aarch64"))]
pub fn sleep_ns(_delay_ns: u64) -> bool {
    false
}

/// Create a one-shot timer that signals after `delay_ns` of monotonic time.
/// The returned handle is waitable and can be bound to a `Port`.
#[cfg(target_arch = "aarch64")]
pub fn timer_create(delay_ns: u64) -> u64 {
    syscall(Syscall::TimerCreate, delay_ns, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn timer_create(_delay_ns: u64) -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn interrupt_wait(interrupt: Handle) -> u64 {
    syscall(Syscall::InterruptWait, interrupt.0 as u64, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn interrupt_wait(_interrupt: Handle) -> u64 {
    0
}

#[cfg(target_arch = "aarch64")]
pub fn interrupt_complete(interrupt: Handle) -> Status {
    syscall(Syscall::InterruptComplete, interrupt.0 as u64, 0, 0, 0) as Status
}

#[cfg(not(target_arch = "aarch64"))]
pub fn interrupt_complete(_interrupt: Handle) -> Status {
    0
}

#[cfg(target_arch = "aarch64")]
pub fn resource_mint_mmio(resource: Handle, phys_base: u64, len: u64) -> u64 {
    syscall(
        Syscall::ResourceMintMmio,
        resource.0 as u64,
        phys_base,
        len,
        0,
    )
}

#[cfg(not(target_arch = "aarch64"))]
pub fn resource_mint_mmio(_resource: Handle, _phys_base: u64, _len: u64) -> u64 {
    u64::MAX
}

/// Raise the device's power domain at its safe boot clock. Requires a Resource with WRITE
/// covering the aperture; the kernel selects the power unit from the live DTB.
#[cfg(all(
    target_os = "none",
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
pub fn device_power_up(resource: Handle, aperture_base: u64) -> Status {
    syscall(
        Syscall::DevicePowerUp,
        resource.0 as u64,
        aperture_base,
        0,
        0,
    ) as Status
}

#[cfg(not(all(
    target_os = "none",
    any(target_arch = "aarch64", target_arch = "x86_64")
)))]
pub fn device_power_up(_resource: Handle, _aperture_base: u64) -> Status {
    kumo_abi::Errno::NotSupported.status()
}

/// Change one DTB-approved reset line; the kernel writes only that line's masked bit.
#[cfg(all(
    target_os = "none",
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
pub fn device_reset_set(resource: Handle, aperture_base: u64, line: u32, asserted: bool) -> Status {
    syscall(
        Syscall::DeviceResetSet,
        resource.0 as u64,
        aperture_base,
        u64::from(line),
        u64::from(asserted),
    ) as Status
}

#[cfg(not(all(
    target_os = "none",
    any(target_arch = "aarch64", target_arch = "x86_64")
)))]
pub fn device_reset_set(
    _resource: Handle,
    _aperture_base: u64,
    _line: u32,
    _asserted: bool,
) -> Status {
    kumo_abi::Errno::NotSupported.status()
}

/// Read the device's power/clock evidence. Requires Resource READ; inspect `record` only on
/// success. The typed buffer supplies the ABI's size and alignment.
#[cfg(all(
    target_os = "none",
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
pub fn device_power_status(
    resource: Handle,
    aperture_base: u64,
    record: &mut kumo_abi::DevicePowerStatus,
) -> Status {
    syscall(
        Syscall::DevicePowerStatus,
        resource.0 as u64,
        aperture_base,
        record as *mut kumo_abi::DevicePowerStatus as u64,
        core::mem::size_of::<kumo_abi::DevicePowerStatus>() as u64,
    ) as Status
}

#[cfg(not(all(
    target_os = "none",
    any(target_arch = "aarch64", target_arch = "x86_64")
)))]
pub fn device_power_status(
    _resource: Handle,
    _aperture_base: u64,
    _record: &mut kumo_abi::DevicePowerStatus,
) -> Status {
    kumo_abi::Errno::NotSupported.status()
}

/// Claim the boot framebuffer for a userspace console driver. The kernel accepts
/// this only when `resource` grants the requested physical range and that range is
/// the framebuffer currently owned by the Stage-A console.
#[cfg(target_arch = "aarch64")]
pub fn framebuffer_claim(resource: Handle, phys_base: u64, len: u64) -> Status {
    syscall(
        Syscall::FramebufferClaim,
        resource.0 as u64,
        phys_base,
        len,
        0,
    ) as Status
}

#[cfg(not(target_arch = "aarch64"))]
pub fn framebuffer_claim(_resource: Handle, _phys_base: u64, _len: u64) -> Status {
    kumo_abi::Errno::NotSupported.status()
}

/// Carve a child Resource bounded to MMIO `[phys_base, phys_base + len)` and the IRQ
/// window `[irq_base, irq_base + irq_count)`. The IRQ window is packed into one argument
/// as `(irq_base << 32) | irq_count` so the call fits the four-register syscall ABI.
#[cfg(target_arch = "aarch64")]
pub fn resource_create_child(
    parent: Handle,
    phys_base: u64,
    len: u64,
    irq_base: u32,
    irq_count: u32,
) -> u64 {
    let irq_window = ((irq_base as u64) << 32) | (irq_count as u64);
    syscall(
        Syscall::ResourceCreateChild,
        parent.0 as u64,
        phys_base,
        len,
        irq_window,
    )
}

#[cfg(not(target_arch = "aarch64"))]
pub fn resource_create_child(
    _parent: Handle,
    _phys_base: u64,
    _len: u64,
    _irq_base: u32,
    _irq_count: u32,
) -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn port_create() -> u64 {
    syscall(Syscall::PortCreate, 0, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn port_create() -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn handle_koid(handle: Handle) -> u64 {
    syscall(Syscall::HandleKoid, handle.0 as u64, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn handle_koid(_handle: Handle) -> u64 {
    0
}

#[cfg(target_arch = "aarch64")]
pub fn port_bind(port: Handle, object: Handle) -> u64 {
    syscall(Syscall::PortBind, port.0 as u64, object.0 as u64, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn port_bind(_port: Handle, _object: Handle) -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn port_unbind(port: Handle, object: Handle) -> u64 {
    syscall(Syscall::PortUnbind, port.0 as u64, object.0 as u64, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn port_unbind(_port: Handle, _object: Handle) -> u64 {
    u64::MAX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syscall_handles_reject_widened_errno_statuses() {
        assert_eq!(syscall_handle(1), Some(Handle(1)));
        assert_eq!(syscall_handle(0), None);
        assert_eq!(
            syscall_handle(i32::MAX as u64),
            Some(Handle(i32::MAX as u32))
        );
        assert_eq!(syscall_handle(0x8000_0000), None);
        assert_eq!(syscall_handle(0xffff_fffe), None); // BadHandle = -2
        assert_eq!(syscall_handle(0xffff_ffff), None); // InvalidArgs = -1
        assert_eq!(syscall_handle(u64::MAX), None);
    }

    #[test]
    fn startup_tags_install_explicit_handles_and_reject_unknown_tags() {
        let mut startup = Startup::default();
        assert_eq!(startup.install(STARTUP_TAG_STDOUT, Handle(7)), None);
        assert_eq!(startup.install(STARTUP_TAG_CAP0, Handle(9)), None);
        assert_eq!(startup.install(STARTUP_TAG_TTY, Handle(13)), None);
        assert_eq!(startup.stdout, Some(Handle(7)));
        assert_eq!(startup.cap0, Some(Handle(9)));
        assert_eq!(startup.tty, Some(Handle(13)));
        assert_eq!(startup.install(b'?', Handle(11)), Some(Handle(11)));
    }

    #[test]
    fn duplicate_startup_tag_replaces_and_returns_old_handle() {
        let mut startup = Startup::default();
        assert_eq!(startup.install(STARTUP_TAG_STDOUT, Handle(3)), None);
        assert_eq!(
            startup.install(STARTUP_TAG_STDOUT, Handle(4)),
            Some(Handle(3))
        );
        assert_eq!(startup.stdout, Some(Handle(4)));
    }
}

#[cfg(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_os = "none")
))]
pub fn process_exit(code: u64) -> ! {
    syscall(Syscall::ProcessExit, code, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(not(any(
    target_arch = "aarch64",
    all(target_arch = "x86_64", target_os = "none")
)))]
pub fn process_exit(_code: u64) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
