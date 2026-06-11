use kumo_abi::{Handle, Status, Syscall};

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
pub fn process_run(process: Handle, entry: u64, sp: u64) -> u64 {
    syscall(Syscall::ProcessRun, process.0 as u64, entry, sp, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn process_run(_process: Handle, _entry: u64, _sp: u64) -> u64 {
    u64::MAX
}

#[cfg(target_arch = "aarch64")]
pub fn interrupt_create(irq: u32) -> u64 {
    syscall(Syscall::InterruptCreate, irq as u64, 0, 0, 0)
}

#[cfg(not(target_arch = "aarch64"))]
pub fn interrupt_create(_irq: u32) -> u64 {
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
