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

#[cfg(not(target_arch = "aarch64"))]
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

#[cfg(not(target_arch = "aarch64"))]
fn syscall6(_num: Syscall, _x0: u64, _x1: u64, _x2: u64, _x3: u64, _x4: u64, _x5: u64) -> u64 {
    0
}

#[cfg(target_arch = "aarch64")]
pub fn debug_write(ptr: *const u8, len: usize) -> u64 {
    syscall(Syscall::DebugWrite, ptr as u64, len as u64, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
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
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn handle_close(handle: Handle) -> Status {
    syscall(Syscall::HandleClose, handle.0 as u64, 0, 0, 0) as Status
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn handle_close(_handle: Handle) -> Status {
    kumo_abi::Errno::NotSupported.status()
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

#[cfg(not(target_arch = "aarch64"))]
pub fn channel_read_with_handle(_channel: Handle, _buf: *mut u8, _cap: usize) -> (usize, u64) {
    (0, 0)
}

pub const STARTUP_TAG_STDIN: u8 = b'i';
pub const STARTUP_TAG_STDOUT: u8 = b'o';
pub const STARTUP_TAG_STDERR: u8 = b'e';
pub const STARTUP_TAG_ARGV: u8 = b'a';
pub const STARTUP_TAG_CAP0: u8 = b'c';

/// Explicit capabilities supplied to a newly started program. Each present field came
/// from one transferred handle on the bootstrap channel; absent fields grant no authority.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Startup {
    pub stdin: Option<Handle>,
    pub stdout: Option<Handle>,
    pub stderr: Option<Handle>,
    pub argv: Option<Handle>,
    pub cap0: Option<Handle>,
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

#[cfg(target_arch = "aarch64")]
pub fn channel_write(channel: Handle, ptr: *const u8, len: usize) -> Status {
    syscall(
        Syscall::ChannelWrite,
        channel.0 as u64,
        ptr as u64,
        len as u64,
        0,
    ) as Status
}

#[cfg(not(target_arch = "aarch64"))]
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

#[cfg(target_arch = "aarch64")]
pub fn process_create_named(vmar_base: u64, vmar_size: u64, label: &[u8]) -> u64 {
    syscall(
        Syscall::ProcessCreate,
        vmar_base,
        vmar_size,
        label.as_ptr() as u64,
        label.len() as u64,
    )
}

#[cfg(not(target_arch = "aarch64"))]
pub fn process_create_named(_vmar_base: u64, _vmar_size: u64, _label: &[u8]) -> u64 {
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
    fn startup_tags_install_explicit_handles_and_reject_unknown_tags() {
        let mut startup = Startup::default();
        assert_eq!(startup.install(STARTUP_TAG_STDOUT, Handle(7)), None);
        assert_eq!(startup.install(STARTUP_TAG_CAP0, Handle(9)), None);
        assert_eq!(startup.stdout, Some(Handle(7)));
        assert_eq!(startup.cap0, Some(Handle(9)));
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

#[cfg(target_arch = "aarch64")]
pub fn process_exit(code: u64) -> ! {
    syscall(Syscall::ProcessExit, code, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(not(target_arch = "aarch64"))]
pub fn process_exit(_code: u64) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
