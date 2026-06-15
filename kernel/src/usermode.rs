//! P5-sora: two-party root channel + kernel-held Sora recipe + supervised restart.
//!
//! The kernel creates a root channel: one endpoint goes to Sora as a bootstrap handle
//! (passed in x0), the other is held directly by the kernel via [`IpcRegistry`] —
//! no handle table needed for the kernel side. Messages cross the process boundary:
//! Sora's `ChannelWrite`/`ChannelRead` go through its process handle table; the kernel
//! reads/writes the peer endpoint via direct [`ChannelPair`] access.
//!
//! Sora's ELF image is retained as a [`SoraRecipe`] so the kernel can relaunch it
//! after a crash (`DESIGN/002`). Stage-A runs a bounded restart loop (3 attempts).

use core::cell::UnsafeCell;

use alloc::vec::Vec;

use kumo_abi::{
    find_file, sys::Syscall, BootInfo, Errno, Handle, InitrdError, KoId, Rights, SORA_INIT_PATH,
};
use kumo_hal::active::{UserImage, UserImageError, UserLoadSegment, UserMapping, UserState};
use kumo_hal::PageFlags;
use kumo_ipc::Message;

use crate::bootstrap::user::{
    plan_elf_process, ElfSegment, UserBootstrapError, USER_IMAGE_BASE, USER_ROOT_BASE,
    USER_ROOT_SIZE, USER_STACK_SIZE, USER_STACK_TOP,
};
use crate::ipc::{ChannelEnd, IpcError, KernelMessage};
use crate::mm::{alloc_zeroed_frame, Vmar, Vmo};
use crate::syscall::{KernelCall, KernelCallResult, SyscallEngine};
use crate::task::{Job, Process};

/// Maximum restart attempts before giving up and reporting failure.
const MAX_SORA_ATTEMPTS: u32 = 3;

/// The kernel-held Sora recipe: the ELF image and its parsed layout, retained so the
/// kernel can re-spawn Sora after a crash without re-reading the initrd.
struct SoraRecipe {
    /// Sora's raw ELF bytes (the file image from the initrd).
    _elf_bytes: Vec<u8>,
    /// ELF entry point.
    entry: u64,
    /// Initial stack pointer (SP_EL0).
    stack_top: u64,
    /// Loadable segments (file offset/size, VA, mem size, flags).
    segments: Vec<ElfSegment>,
}

/// Live state for one Sora incarnation. The kernel holds the root channel's peer
/// endpoint directly; Sora holds the other end as a handle in its process table.
pub(crate) struct SoraState {
    pub engine: SyscallEngine,
    pub process: Process,
    /// The root Job — child processes are created under this.
    pub root_job: Job,
    /// Index of the root [`ChannelPair`] in the IPC registry.
    pub root_channel: usize,
    /// The kernel's endpoint for the root channel (Left — Sora gets Right).
    pub kernel_end: ChannelEnd,
    /// Index of the console [`ChannelPair`] in the IPC registry.
    pub console_channel: usize,
    /// The kernel's endpoint for the console channel.
    pub console_kernel_end: ChannelEnd,
    /// Index of the block [`ChannelPair`] (P7-g: kernel-as-client block reads).
    pub block_channel: usize,
    /// The kernel's endpoint for the block channel.
    pub block_kernel_end: ChannelEnd,
    /// Index of the network [`ChannelPair`] (P9-c: loopback server).
    pub net_channel: usize,
    /// The kernel's endpoint for the network channel.
    pub net_kernel_end: ChannelEnd,
    /// Index of the keyboard [`ChannelPair`] (P8-a restoration).
    pub keyboard_channel: usize,
    /// The kernel's endpoint for the keyboard channel.
    pub keyboard_kernel_end: ChannelEnd,
    /// Koids of Sora's *own* ends of the console/block/net channels — the koids Sora binds
    /// to its serve-loop `Port`. A kernel-side write to one of these channels signals the
    /// matching port (`signal_channel_ports`) so Sora's `PortWait` wakes; the engine's
    /// `ChannelWrite` does this for user writes, but the kernel writes pairs directly.
    pub console_koid: KoId,
    pub block_koid: KoId,
    pub net_koid: KoId,
    /// Bytes written by `DebugWrite` syscalls during this run.
    pub wrote: usize,
}

struct SoraCell(UnsafeCell<Option<SoraState>>);
unsafe impl Sync for SoraCell {}
static SORA: SoraCell = SoraCell(UnsafeCell::new(None));

fn sora_ptr() -> *mut SoraState {
    let opt: *mut Option<SoraState> = SORA.0.get();
    unsafe { (&mut *opt).as_mut().expect("sora state not initialized") as *mut SoraState }
}

pub fn sora_ptr_mut() -> *mut SoraState {
    sora_ptr()
}

/// P9-a: signal all interrupt objects bound to `irq`. Called from the timer IRQ
/// handler via `set_interrupt_hook`. Wakes Sora if it's parked on InterruptWait.
extern "C" fn signal_irq(irq: u32) {
    let sora = unsafe { &mut *sora_ptr() };
    sora.engine.signal_interrupt(irq);
    // Wake Sora if parked — InterruptWait uses park_current_user().
    if crate::user_thread::is_started()
        && !crate::user_thread::is_done()
        && crate::user_thread::is_parked()
    {
        crate::user_thread::wake_user();
    }
}

/// P9-e: check the net channel for a handle transferred by Sora. Returns true if
/// a handle was received.
pub fn net_check_transfer() -> bool {
    let sora = unsafe { &mut *sora_ptr() };
    let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.net_channel) else {
        return false;
    };
    match channel.read(sora.net_kernel_end) {
        Ok(message) => !message.handles().is_empty(),
        Err(_) => false,
    }
}

/// P9-c: send `data` on the network channel, wake Sora (loopback server echoes it),
/// and read the reply. Returns bytes copied into `buf`.
pub fn net_loopback(data: &[u8], buf: &mut [u8]) -> usize {
    if !crate::user_thread::is_started()
        || crate::user_thread::is_done()
        || !crate::user_thread::is_parked()
    {
        return 0;
    }
    {
        let sora = unsafe { &mut *sora_ptr() };
        let Ok(message) = Message::new(4, data, &[]) else {
            return 0;
        };
        let Ok(msg) = KernelMessage::from_borrowed(message) else {
            return 0;
        };
        let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.net_channel) else {
            return 0;
        };
        if channel.write(sora.net_kernel_end, msg).is_err() {
            return 0;
        }
    }
    signal_and_wake(unsafe { &*sora_ptr() }.net_koid);
    let sora = unsafe { &mut *sora_ptr() };
    let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.net_channel) else {
        return 0;
    };
    match channel.read(sora.net_kernel_end) {
        Ok(reply) => {
            let bytes = reply.bytes();
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            n
        }
        Err(_) => 0,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UserReport {
    pub entered: bool,
    pub syscalls: u32,
    pub wrote: usize,
    pub chan: (u32, u32),
    pub exit_code: u64,
    /// Bytes the kernel read back from the root channel after the process exited.
    pub handshake: [u8; 32],
    pub handshake_len: usize,
    /// Number of Sora restart attempts (0 = first run succeeded).
    pub attempts: u32,
    /// True if Sora is alive and parked on its console channel (a live server).
    pub serving: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsermodeError {
    Initrd(InitrdError),
    MissingSora,
    Bootstrap(UserBootstrapError),
    Image(UserImageError),
    BadSegmentRange,
    ChannelSetup,
    /// Sora exited non-zero after all restart attempts were exhausted.
    SoraExhausted {
        exit_code: u64,
        attempts: u32,
    },
}

impl From<InitrdError> for UsermodeError {
    fn from(error: InitrdError) -> Self {
        Self::Initrd(error)
    }
}

impl From<UserBootstrapError> for UsermodeError {
    fn from(error: UserBootstrapError) -> Self {
        Self::Bootstrap(error)
    }
}

impl From<UserImageError> for UsermodeError {
    fn from(error: UserImageError) -> Self {
        Self::Image(error)
    }
}

fn user_range_ok(process: &Process, ptr: u64, len: u64) -> bool {
    let vmar = process.root_vmar();
    let base = vmar.base();
    match (base.checked_add(vmar.len()), ptr.checked_add(len)) {
        (Some(vmar_end), Some(end)) => ptr >= base && end <= vmar_end,
        _ => false,
    }
}

extern "C" fn svc_hook(regs: *mut u64) {
    let r = unsafe { core::slice::from_raw_parts_mut(regs, 31) };
    let num = r[8];

    if num == Syscall::ProcessExit as u64 {
        if crate::user_thread::is_started() {
            crate::user_thread::exit_current_user(r[0]);
        }
        kumo_hal::active::el0_exit(r[0]);
    }

    let sora = unsafe { &mut *sora_ptr() };

    if let Some(current_process) = crate::user_thread::current_process_koid() {
        if current_process != sora.process.koid() {
            if num == Syscall::DebugWrite as u64 {
                let user_ptr = r[0];
                let len = (r[1] as usize).min(256);
                let Some(child_process) = sora.engine.process_by_koid(current_process) else {
                    r[0] = u64::MAX;
                    return;
                };
                if !user_range_ok(child_process, user_ptr, len as u64) {
                    r[0] = u64::MAX;
                    return;
                }
                let bytes = unsafe { core::slice::from_raw_parts(user_ptr as *const u8, len) };
                kumo_hal::active::early_console_write(bytes);
                r[0] = len as u64;
            } else {
                r[0] = u64::MAX;
            }
            return;
        }
    }

    if num == Syscall::DebugWrite as u64 {
        let user_ptr = r[0];
        let len = (r[1] as usize).min(256);
        if !user_range_ok(&sora.process, user_ptr, len as u64) {
            r[0] = u64::MAX;
            return;
        }
        let bytes = unsafe { core::slice::from_raw_parts(user_ptr as *const u8, len) };
        // Render on the device directly (never through `bootstrap::console::write`):
        // this is the console *server's own* output path — routing it back through the
        // console channel would recurse into Sora forever (P6-e reentrancy guard).
        kumo_hal::active::early_console_write(bytes);
        sora.wrote += len;
        r[0] = len as u64;
    } else if num == Syscall::VmoWrite as u64 {
        let vmo = Handle(r[0] as u32);
        let offset = r[1];
        let user_buf = r[2];
        let len = (r[3] as usize).min(256);
        if !user_range_ok(&sora.process, user_buf, len as u64) {
            r[0] = u64::MAX;
            return;
        }
        match sora.engine.dispatch(
            &mut sora.process,
            KernelCall::VmoWrite {
                vmo,
                offset,
                src: user_buf as *const u8,
                len,
            },
        ) {
            KernelCallResult::Status(status) => r[0] = status as u32 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::VmoCreate as u64 {
        let size = r[0];
        match sora
            .engine
            .dispatch(&mut sora.process, KernelCall::VmoCreate { size })
        {
            KernelCallResult::Handle(handle) => r[0] = handle.0 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::VmoRead as u64 {
        let vmo = Handle(r[0] as u32);
        let offset = r[1];
        let user_buf = r[2];
        let len = (r[3] as usize).min(256);
        if !user_range_ok(&sora.process, user_buf, len as u64) {
            r[0] = u64::MAX;
            return;
        }
        match sora.engine.dispatch(
            &mut sora.process,
            KernelCall::VmoRead {
                vmo,
                offset,
                dest: user_buf as *mut u8,
                len,
            },
        ) {
            KernelCallResult::Status(status) => r[0] = status as u32 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::HandleKoid as u64 {
        let handle = Handle(r[0] as u32);
        match sora
            .engine
            .dispatch(&mut sora.process, KernelCall::HandleKoid { handle })
        {
            KernelCallResult::Handle(koid_handle) => r[0] = koid_handle.0 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::PortCreate as u64 {
        match sora
            .engine
            .dispatch(&mut sora.process, KernelCall::PortCreate)
        {
            KernelCallResult::Handle(handle) => r[0] = handle.0 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::PortBindChannel as u64 {
        let port = Handle(r[0] as u32);
        let channel = Handle(r[1] as u32);
        match sora.engine.dispatch(
            &mut sora.process,
            KernelCall::PortBindChannel { port, channel },
        ) {
            KernelCallResult::Status(status) => r[0] = status as u32 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::ResourceMintMmio as u64 {
        let resource = Handle(r[0] as u32);
        let phys_base = r[1];
        let len = r[2];
        match sora.engine.dispatch(
            &mut sora.process,
            KernelCall::ResourceMintMmio {
                resource,
                phys_base,
                len,
            },
        ) {
            KernelCallResult::Handle(handle) => r[0] = handle.0 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::InterruptCreate as u64 {
        let irq = r[0] as u32;
        match sora
            .engine
            .dispatch(&mut sora.process, KernelCall::InterruptCreate { irq })
        {
            KernelCallResult::Handle(handle) => r[0] = handle.0 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::PortWait as u64 {
        let port = Handle(r[0] as u32);
        let should_wait = Errno::ShouldWait.status();
        loop {
            match sora
                .engine
                .dispatch(&mut sora.process, KernelCall::PortWait { port })
            {
                KernelCallResult::PortPacket(packet) => {
                    r[0] = packet.source.0 as u64;
                    break;
                }
                KernelCallResult::Status(status)
                    if status == should_wait && crate::user_thread::is_started() =>
                {
                    crate::user_thread::park_current_user();
                }
                _ => {
                    r[0] = 0;
                    break;
                }
            }
        }
    } else if num == Syscall::InterruptWait as u64 {
        let interrupt = Handle(r[0] as u32);
        let should_wait = Errno::ShouldWait.status();
        loop {
            match sora
                .engine
                .dispatch(&mut sora.process, KernelCall::InterruptWait { interrupt })
            {
                KernelCallResult::Handle(count) => {
                    r[0] = count.0 as u64;
                    break;
                }
                KernelCallResult::Status(status)
                    if status == should_wait && crate::user_thread::is_started() =>
                {
                    crate::user_thread::park_current_user();
                }
                _ => {
                    r[0] = 0;
                    break;
                }
            }
        }
    } else if num == Syscall::ProcessRun as u64 {
        let process_handle = Handle(r[0] as u32);
        let entry = r[1];
        let sp = r[2];
        let arg = r[3];
        match sora.engine.dispatch(
            &mut sora.process,
            KernelCall::ProcessRun {
                process_handle,
                entry,
                sp,
                arg,
            },
        ) {
            KernelCallResult::Status(status) => r[0] = status as u32 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::AddressSpaceCreate as u64 {
        let process_handle = Handle(r[0] as u32);
        let stack_virt = r[1];
        let stack_size = r[2];
        match sora.engine.dispatch(
            &mut sora.process,
            KernelCall::AddressSpaceCreate {
                process_handle,
                stack_virt,
                stack_size,
            },
        ) {
            KernelCallResult::Handle(handle) => r[0] = handle.0 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::ThreadCreate as u64 {
        let process_handle = Handle(r[0] as u32);
        match sora.engine.dispatch(
            &mut sora.process,
            KernelCall::ThreadCreate { process_handle },
        ) {
            KernelCallResult::Handle(handle) => r[0] = handle.0 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::ThreadStart as u64 {
        let thread_handle = Handle(r[0] as u32);
        let entry = r[1];
        let sp = r[2];
        let arg = r[3];
        match sora.engine.dispatch(
            &mut sora.process,
            KernelCall::ThreadStart {
                thread_handle,
                entry,
                sp,
                arg,
            },
        ) {
            KernelCallResult::Status(status) => r[0] = status as u32 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::ProcessCreate as u64 {
        let vmar_base = r[0];
        let vmar_size = r[1];
        let result = sora.engine.dispatch(
            &mut sora.process,
            KernelCall::ProcessCreate {
                parent_job: sora.root_job,
                vmar_base,
                vmar_size,
            },
        );
        match result {
            KernelCallResult::Handle(handle) => r[0] = handle.0 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::VmarMap as u64 {
        let process_handle = Handle(r[0] as u32);
        let vmo_handle = Handle(r[1] as u32);
        let vmo_offset = r[2];
        let virt = r[3];
        let len = r[4];
        let flags_raw = r[5];
        let flags = PageFlags(flags_raw);
        match sora.engine.dispatch(
            &mut sora.process,
            KernelCall::VmarMap {
                process_handle,
                vmo_handle,
                vmo_offset,
                virt,
                len,
                flags,
            },
        ) {
            KernelCallResult::Status(status) => r[0] = status as u32 as u64,
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::ChannelCreate as u64 {
        match sora
            .engine
            .dispatch(&mut sora.process, KernelCall::ChannelCreate)
        {
            KernelCallResult::Handles { first, second } => {
                r[0] = first.0 as u64;
                r[1] = second.0 as u64;
            }
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::ChannelWrite as u64 {
        let channel = Handle(r[0] as u32);
        let user_ptr = r[1];
        let len = (r[2] as usize).min(256);
        if !user_range_ok(&sora.process, user_ptr, len as u64) {
            r[0] = (-1i32) as u32 as u64;
            return;
        }
        let bytes = unsafe { core::slice::from_raw_parts(user_ptr as *const u8, len) };
        // P9-e: r[3] is an optional handle to transfer alongside the message.
        let transfer_handle = if r[3] != 0 {
            &[Handle(r[3] as u32)]
        } else {
            &[][..]
        };
        let status = match Message::new(1, bytes, transfer_handle) {
            Ok(message) => match sora.engine.dispatch(
                &mut sora.process,
                KernelCall::ChannelWrite { channel, message },
            ) {
                KernelCallResult::Status(s) => s,
                _ => -1,
            },
            Err(_) => -1,
        };
        r[0] = status as u32 as u64;
    } else if num == Syscall::ChannelRead as u64 {
        let channel = Handle(r[0] as u32);
        let user_buf = r[1];
        let cap = (r[2] as usize).min(256);
        if !user_range_ok(&sora.process, user_buf, cap as u64) {
            r[0] = 0;
            return;
        }
        // Blocking read with multiplex-friendly wakes: an empty inbox parks the user
        // thread **once** (the boot flow resumes); the next kernel-side wake retries
        // this channel exactly once. If it is still empty the syscall returns 0 —
        // "woken, but not for this channel" — so a server waiting on several channels
        // can poll its others and come back. (P7-g: Sora serves console + block.)
        let should_wait = kumo_abi::sys::Errno::ShouldWait.status();
        let mut parked_once = false;
        loop {
            match sora
                .engine
                .dispatch(&mut sora.process, KernelCall::ChannelRead { channel })
            {
                KernelCallResult::Message(message) => {
                    let bytes = message.bytes();
                    let n = bytes.len().min(cap);
                    unsafe {
                        core::ptr::copy_nonoverlapping(bytes.as_ptr(), user_buf as *mut u8, n)
                    };
                    r[0] = n as u64;
                    break;
                }
                KernelCallResult::Status(status)
                    if status == should_wait
                        && !parked_once
                        && crate::user_thread::is_started() =>
                {
                    parked_once = true;
                    crate::user_thread::park_current_user();
                }
                _ => {
                    r[0] = 0; // error, or woken for a different channel
                    break;
                }
            }
        }
    } else {
        r[0] = u64::MAX;
    }
}

/// Fallback (no Sora in the initrd): run the embedded EL0 smoke payload via the old
/// synchronous path. The SVC hook reads the `SORA` static, so install a minimal state
/// for it — there is no prior Sora to preserve on this path.
pub fn run(boot: &BootInfo) -> UserReport {
    let mut engine = SyscallEngine::new();
    let job = Job::root(engine.objects_mut());
    let vmar = Vmar::new(USER_ROOT_BASE, USER_ROOT_SIZE).expect("user vmar");
    let process = Process::new(engine.objects_mut(), &job, vmar);

    // SAFETY: single-threaded boot path; installed before any SVC can fire.
    unsafe {
        *SORA.0.get() = Some(SoraState {
            engine,
            process,
            root_job: job,
            root_channel: 0,
            kernel_end: ChannelEnd::Left,
            console_channel: 0,
            console_kernel_end: ChannelEnd::Left,
            block_channel: 0,
            block_kernel_end: ChannelEnd::Left,
            net_channel: 0,
            net_kernel_end: ChannelEnd::Left,
            keyboard_channel: 0,
            keyboard_kernel_end: ChannelEnd::Left,
            console_koid: KoId(0),
            block_koid: KoId(0),
            net_koid: KoId(0),
            wrote: 0,
        });
    }

    kumo_hal::active::set_svc_hook(svc_hook);
    let mut alloc = || unsafe { alloc_zeroed_frame(boot) };
    let report = kumo_hal::active::run_el0_smoke(
        USER_IMAGE_BASE,
        USER_STACK_TOP,
        USER_STACK_SIZE,
        &mut alloc,
    )
    .unwrap_or_default();

    let sora = unsafe { &*sora_ptr() };
    UserReport {
        entered: report.entered,
        syscalls: report.syscalls,
        wrote: sora.wrote,
        chan: (0, 0),
        exit_code: report.exit_code,
        attempts: 0,
        ..Default::default()
    }
}

/// Build a Sora recipe from the initrd. Called once; the recipe is reused across
/// restart attempts.
fn build_recipe(initrd: &[u8]) -> Result<SoraRecipe, UsermodeError> {
    let sora_file = find_file(initrd, SORA_INIT_PATH)?.ok_or(UsermodeError::MissingSora)?;
    let elf_bytes = sora_file.bytes.to_vec();
    let plan = plan_elf_process(&mut crate::object::ObjectManager::new(), &elf_bytes)?;
    Ok(SoraRecipe {
        _elf_bytes: elf_bytes,
        entry: plan.entry,
        stack_top: plan.stack_top,
        segments: plan.load_segments,
    })
}

/// One attempt: build page tables, create root channel, spawn Sora, wait for exit.
/// Returns the handshake and exit code.
fn attempt_sora(
    boot: &BootInfo,
    recipe: &SoraRecipe,
    initrd_bytes: &[u8],
) -> Result<(UserReport, u64), UsermodeError> {
    let mut engine = SyscallEngine::new();
    engine.set_boot_info(*boot);
    let job = Job::root(engine.objects_mut());
    let vmar = Vmar::new(USER_ROOT_BASE, USER_ROOT_SIZE).map_err(UserBootstrapError::from)?;
    let mut process = Process::new(engine.objects_mut(), &job, vmar);

    // Root channel (bootstrap): kernel gets Left, Sora gets Right.
    let (sora_handle, root_channel, kernel_end) = engine
        .root_channel_create(&mut process)
        .map_err(|_| UsermodeError::ChannelSetup)?;

    // Console channel: kernel sends console output to Sora. Kernel keeps Left,
    // Sora gets Right as a second handle (passed in x2 at entry).
    let (console_handle, console_channel_idx, console_kernel_end) = engine
        .root_channel_create(&mut process)
        .map_err(|_| UsermodeError::ChannelSetup)?;

    // P7-a: hand the initrd to Sora as a VMO. Sora receives the handle in x3.
    let initrd_vmo_handle = engine
        .root_vmo_create(
            &mut process,
            Vmo::from_physical_range(boot.initrd.start, boot.initrd.len as u64)
                .map_err(|_| UsermodeError::ChannelSetup)?,
            // WRITE lets Sora stage child code into the initrd VMO (VmoWrite) before mapping
            // it RX into a child address space — the P10 process-model demo. A scratch
            // anonymous VMO would avoid mutating the initrd, but that path is deferred.
            Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
        )
        .map_err(|_| UsermodeError::ChannelSetup)?;

    // P9-b: root Resource — grants access to all physical MMIO (scaffold; Gouchen
    // will mint per-device Resources later). Sora receives the handle in x5.
    let root_resource_handle = engine
        .root_resource_create(&mut process, 0, u64::MAX)
        .map_err(|_| UsermodeError::ChannelSetup)?;

    // P9-c: network channel — loopback server. Kernel is the first client.
    let (net_handle, net_channel_idx, net_kernel_end) = engine
        .root_channel_create(&mut process)
        .map_err(|_| UsermodeError::ChannelSetup)?;

    // P8-a restoration: keyboard channel — kernel forwards keystrokes to Sora.
    let (kbd_handle, kbd_channel_idx, kbd_kernel_end) = engine
        .root_channel_create(&mut process)
        .map_err(|_| UsermodeError::ChannelSetup)?;

    // P7-g: block channel — the kernel sends block-read requests, Sora serves them from
    // the initrd VMO. Kernel keeps Left; Sora gets Right as a handle (passed in x4).
    let (block_handle, block_channel_idx, block_kernel_end) = engine
        .root_channel_create(&mut process)
        .map_err(|_| UsermodeError::ChannelSetup)?;

    // Seed Sora's root inbox.
    if let Ok(message) = Message::new(1, b"kernel->sora boot\n", &[]) {
        let msg = KernelMessage::from_borrowed(message).map_err(|_| UsermodeError::ChannelSetup)?;
        let channel = engine
            .ipc_mut()
            .channel_pair_mut(root_channel)
            .ok_or(UsermodeError::ChannelSetup)?;
        let _ = channel.write(kernel_end, msg);
    }

    // Build load segments from the recipe.
    let mut load_segments = Vec::new();
    let sora_file = find_file(initrd_bytes, SORA_INIT_PATH)?.ok_or(UsermodeError::MissingSora)?;
    for segment in &recipe.segments {
        let start =
            usize::try_from(segment.file_offset).map_err(|_| UsermodeError::BadSegmentRange)?;
        let len = usize::try_from(segment.file_size).map_err(|_| UsermodeError::BadSegmentRange)?;
        let end = start
            .checked_add(len)
            .ok_or(UsermodeError::BadSegmentRange)?;
        if end > sora_file.bytes.len() {
            return Err(UsermodeError::BadSegmentRange);
        }
        load_segments.push(UserLoadSegment {
            source: &sora_file.bytes[start..end],
            virt_addr: segment.virt_addr,
            mem_size: segment.mem_size,
            writable: segment.flags.contains(PageFlags::WRITE),
            executable: segment.flags.contains(PageFlags::EXECUTE),
        });
    }

    // Build page tables. If the boot handoff has a framebuffer, map it into Sora's
    // address space so it can paint pixels directly (the first driver capability).
    // `fb_slot` is the 2 MiB-aligned mapping slot; the VA handed to Sora carries the
    // framebuffer's offset within its first 2 MiB block, so Sora's writes land at the
    // real scanout base regardless of GOP alignment (the journal-061 paint wart).
    let fb_mapping: UserMapping;
    let fb_va: u64;
    let extra_mappings: &[_];
    if boot.has_framebuffer() {
        const BLOCK_MASK: u64 = (1 << 21) - 1;
        let fb_slot = USER_STACK_TOP + 0x0200_0000; // 32 MiB above user stack (2M-aligned)
        fb_va = fb_slot + (boot.framebuffer.phys & BLOCK_MASK);
        fb_mapping = UserMapping {
            phys_base: boot.framebuffer.phys,
            virt_addr: fb_slot,
            len: boot.framebuffer.len,
            writable: true,
            device: false, // Normal-NC for framebuffer
            executable: false,
        };
        extra_mappings = core::slice::from_ref(&fb_mapping);
    } else {
        fb_va = 0;
        extra_mappings = &[];
    }

    let image = UserImage {
        entry: recipe.entry,
        stack_top: recipe.stack_top,
        stack_size: USER_STACK_SIZE,
        bootstrap: sora_handle.0 as u64,
        segments: &load_segments,
        extra_mappings,
    };
    let mut alloc = || unsafe { alloc_zeroed_frame(boot) };
    let user_ttbr0 =
        kumo_hal::active::build_user_tables(&image, &mut alloc).map_err(UsermodeError::Image)?;

    let kernel_ttbr0 = kumo_hal::active::read_ttbr0();
    let user_state = UserState {
        x: {
            let mut x = [0u64; 31];
            x[0] = sora_handle.0 as u64; // bootstrap handle (root channel)
            x[1] = fb_va; // framebuffer virtual address (0 if none)
            x[2] = console_handle.0 as u64; // console channel handle
            x[3] = initrd_vmo_handle.0 as u64; // initrd VMO handle (P7-a)
            x[4] = block_handle.0 as u64; // block-server channel handle (P7-g)
            x[5] = root_resource_handle.0 as u64; // root Resource handle (P9-b)
            x[6] = net_handle.0 as u64; // network channel handle (P9-c)
            x[7] = kbd_handle.0 as u64; // keyboard channel handle (P8-a)
            x
        },
        elr: recipe.entry,
        spsr: 0,
        sp_el0: recipe.stack_top - 16,
        ttbr0: user_ttbr0,
    };

    // Scheduler harness. Bind Sora's thread to Sora's real process koid so syscall
    // dispatch can distinguish Sora from scheduled child processes.
    crate::user_thread::init(
        engine.objects_mut(),
        process.koid(),
        process.root_vmar(),
        kernel_ttbr0,
    )
    .map_err(|_| UsermodeError::Bootstrap(UserBootstrapError::EmptyImage))?;

    // Koids of Sora's own channel ends — the koids Sora binds to its serve-loop port.
    // Resolved from the handle table before `process` is moved into `SoraState`.
    let console_koid = process.handles().get(console_handle).map(|e| e.koid).unwrap_or(KoId(0));
    let block_koid = process.handles().get(block_handle).map(|e| e.koid).unwrap_or(KoId(0));
    let net_koid = process.handles().get(net_handle).map(|e| e.koid).unwrap_or(KoId(0));

    // Install Sora state for the SVC hook. (The relaunch recipe stays with `run_sora`'s
    // restart loop — the hook never needs it.)
    unsafe {
        *SORA.0.get() = Some(SoraState {
            engine,
            process,
            root_job: job,
            root_channel,
            kernel_end,
            console_channel: console_channel_idx,
            console_kernel_end,
            block_channel: block_channel_idx,
            block_kernel_end,
            net_channel: net_channel_idx,
            net_kernel_end,
            keyboard_channel: kbd_channel_idx,
            keyboard_kernel_end: kbd_kernel_end,
            console_koid,
            block_koid,
            net_koid,
            wrote: 0,
        });
    }

    let boot_ctx = kumo_hal::active::ThreadContext::default();
    unsafe { crate::user_thread::pin_boot_context(&boot_ctx) };

    // P6-c: write several console messages from the kernel to Sora's console channel.
    // Sora's read loop will echo each one via DebugWrite.
    {
        let sora = unsafe { &mut *sora_ptr() };
        let messages: &[&[u8]] = &[
            b"P6: console line 1\n",
            b"P6: console line 2\n",
            b"P6: console line 3\n",
            b"P6: console line 4\n",
        ];
        for msg_bytes in messages {
            if let Ok(message) = Message::new(1, msg_bytes, &[]) {
                if let Ok(msg) = KernelMessage::from_borrowed(message) {
                    if let Some(channel) =
                        sora.engine.ipc_mut().channel_pair_mut(sora.console_channel)
                    {
                        let _ = channel.write(sora.console_kernel_end, msg);
                    }
                }
            }
        }
    }

    kumo_hal::active::set_svc_hook(svc_hook);
    #[cfg(target_os = "none")]
    kumo_hal::active::set_interrupt_hook(signal_irq);
    // Returns when Sora *parks* on the (drained) console channel — Sora stays alive as
    // a server — or exits (the legacy/fault path).
    unsafe { crate::user_thread::spawn_user(user_state, user_ttbr0) };

    // P6-d: prove the park/wake cycle live — with Sora parked, push two more console
    // lines; each write wakes Sora, it echoes and parks again before we continue.
    console_to_sora(b"P6: live console A\n");
    console_to_sora(b"P6: live console B\n");

    // Read Sora's reply from the root channel (kernel side).
    let mut handshake = [0u8; 32];
    let mut handshake_len = 0;
    let serving = !crate::user_thread::is_done() && crate::user_thread::is_parked();
    let exit_code = if crate::user_thread::is_done() {
        crate::user_thread::exit_code()
    } else {
        0
    };
    let wrote;
    {
        let sora = unsafe { &mut *sora_ptr() };
        wrote = sora.wrote;
        if let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.root_channel) {
            match channel.read(sora.kernel_end) {
                Ok(message) => {
                    let bytes = message.bytes();
                    handshake_len = bytes.len().min(handshake.len());
                    handshake[..handshake_len].copy_from_slice(&bytes[..handshake_len]);
                }
                Err(IpcError::ShouldWait) => {} // Sora didn't reply
                Err(_) => {}
            }
        }
    }

    Ok((
        UserReport {
            entered: true,
            syscalls: kumo_hal::active::syscall_count(),
            wrote,
            chan: (sora_handle.0, 0),
            exit_code,
            handshake,
            handshake_len,
            attempts: 0,
            serving,
        },
        exit_code,
    ))
}

/// P6-e: when true, `bootstrap::console::write` offers each fragment to the live Sora
/// server before falling back to the direct device path. Enabled by `stage_a` once the
/// probe proves Sora is serving; cleared forever by the Tower (a panic must never wake
/// or switch threads).
static CONSOLE_ROUTE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn enable_console_route() {
    CONSOLE_ROUTE.store(true, core::sync::atomic::Ordering::Release);
}

pub fn disable_console_route() {
    CONSOLE_ROUTE.store(false, core::sync::atomic::Ordering::Release);
}

/// Largest routed fragment: must fit Sora's 256-byte read buffer with margin.
const ROUTE_CHUNK: usize = 192;

/// Queue a port packet for the Sora-bound channel `channel_koid` (so its serve-loop
/// `PortWait` returns that source), then run Sora until it parks again. The kernel writes
/// to channel pairs directly rather than via the engine's `ChannelWrite`, so nothing has
/// signalled the bound port — this does what `ChannelWrite`'s `signal_channel_ports` would.
fn signal_and_wake(channel_koid: KoId) {
    unsafe { &mut *sora_ptr() }
        .engine
        .signal_channel_ports(channel_koid);
    crate::user_thread::wake_user();
}

/// Deliver one console message to Sora and wake it to drain. Returns false if the
/// channel write failed (the caller falls back to the direct path).
fn deliver_to_sora(bytes: &[u8]) -> bool {
    let sora = unsafe { &mut *sora_ptr() };
    let Ok(message) = Message::new(1, bytes, &[]) else {
        return false;
    };
    let Ok(msg) = KernelMessage::from_borrowed(message) else {
        return false;
    };
    let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.console_channel) else {
        return false;
    };
    if channel.write(sora.console_kernel_end, msg).is_err() {
        return false;
    }
    let koid = sora.console_koid;
    signal_and_wake(koid);
    true
}

/// Send one console message to the live Sora server (the P6-c live-wake demo path).
pub fn console_to_sora(bytes: &[u8]) {
    if !crate::user_thread::is_started() {
        return;
    }
    let _ = deliver_to_sora(bytes);
}

/// P7-g: read `len` bytes at `offset` of the "disk" (the initrd) **through the
/// userspace block server**. The kernel is the client here: it writes a 16-byte
/// request (`offset: u64 LE, len: u64 LE`) to the block channel, wakes Sora — which
/// `VmoRead`s the initrd and writes the data back — and reads the reply once Sora has
/// re-parked. Returns the bytes copied into `buf` (0 = server down / refused).
pub fn block_read_via_sora(offset: u64, len: usize, buf: &mut [u8]) -> usize {
    if !crate::user_thread::is_started()
        || crate::user_thread::is_done()
        || !crate::user_thread::is_parked()
    {
        return 0;
    }

    let mut request = [0u8; 16];
    request[..8].copy_from_slice(&offset.to_le_bytes());
    request[8..].copy_from_slice(&(len as u64).to_le_bytes());

    {
        let sora = unsafe { &mut *sora_ptr() };
        let Ok(message) = Message::new(2, &request, &[]) else {
            return 0;
        };
        let Ok(msg) = KernelMessage::from_borrowed(message) else {
            return 0;
        };
        let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.block_channel) else {
            return 0;
        };
        if channel.write(sora.block_kernel_end, msg).is_err() {
            return 0;
        }
    }

    // Run Sora until it parks again; single-core and synchronous, so by the time this
    // returns the reply (if any) is sitting in our endpoint's inbox.
    signal_and_wake(unsafe { &*sora_ptr() }.block_koid);

    let sora = unsafe { &mut *sora_ptr() };
    let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.block_channel) else {
        return 0;
    };
    match channel.read(sora.block_kernel_end) {
        Ok(reply) => {
            let bytes = reply.bytes();
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            n
        }
        Err(_) => 0,
    }
}

/// P7-k: read a byte range from a named file via Sora's block channel. Sends
/// [0x01][file_off:u64 LE][len:u64 LE][path…] as the request; Sora resolves the path,
/// seeks to `file_off`, and returns up to `len` bytes. Returns bytes copied into `buf`.
pub fn file_read_via_sora_at(path: &[u8], file_off: u64, len: usize, buf: &mut [u8]) -> usize {
    if !crate::user_thread::is_started()
        || crate::user_thread::is_done()
        || !crate::user_thread::is_parked()
    {
        return 0;
    }

    let mut req = [0u8; 32];
    req[0] = 0x01;
    req[1..9].copy_from_slice(&file_off.to_le_bytes());
    req[9..17].copy_from_slice(&(len as u64).to_le_bytes());
    let path_len = path.len().min(req.len() - 17);
    req[17..17 + path_len].copy_from_slice(&path[..path_len]);

    {
        let sora = unsafe { &mut *sora_ptr() };
        let Ok(message) = Message::new(2, &req[..17 + path_len], &[]) else {
            return 0;
        };
        let Ok(msg) = KernelMessage::from_borrowed(message) else {
            return 0;
        };
        let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.block_channel) else {
            return 0;
        };
        if channel.write(sora.block_kernel_end, msg).is_err() {
            return 0;
        }
    }

    signal_and_wake(unsafe { &*sora_ptr() }.block_koid);

    let sora = unsafe { &mut *sora_ptr() };
    let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.block_channel) else {
        return 0;
    };
    match channel.read(sora.block_kernel_end) {
        Ok(reply) => {
            let bytes = reply.bytes();
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            n
        }
        Err(_) => 0,
    }
}

/// P7-j: read a named file from the FAT32 filesystem via Sora's block channel. Sends
/// the path bytes as the request; Sora recognises non-16-byte requests as path-based
/// reads and returns the file contents (up to 512 bytes). Returns bytes copied into `buf`.
pub fn file_read_via_sora(path: &[u8], buf: &mut [u8]) -> usize {
    if !crate::user_thread::is_started()
        || crate::user_thread::is_done()
        || !crate::user_thread::is_parked()
    {
        return 0;
    }

    {
        let sora = unsafe { &mut *sora_ptr() };
        let Ok(message) = Message::new(2, path, &[]) else {
            return 0;
        };
        let Ok(msg) = KernelMessage::from_borrowed(message) else {
            return 0;
        };
        let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.block_channel) else {
            return 0;
        };
        if channel.write(sora.block_kernel_end, msg).is_err() {
            return 0;
        }
    }

    signal_and_wake(unsafe { &*sora_ptr() }.block_koid);

    let sora = unsafe { &mut *sora_ptr() };
    let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.block_channel) else {
        return 0;
    };
    match channel.read(sora.block_kernel_end) {
        Ok(reply) => {
            let bytes = reply.bytes();
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            n
        }
        Err(_) => 0,
    }
}

/// P8-a/b: forward a single keystroke byte to Sora via the console channel. Sora
/// P8-a restoration: forward a keystroke to Sora via the dedicated keyboard channel.
/// Returns true if sent.
pub fn kbd_forward(byte: u8) -> bool {
    if !crate::user_thread::is_started()
        || crate::user_thread::is_done()
        || !crate::user_thread::is_parked()
    {
        return false;
    }
    {
        let sora = unsafe { &mut *sora_ptr() };
        let payload = [byte];
        let Ok(message) = Message::new(5, &payload, &[]) else {
            return false;
        };
        let Ok(msg) = KernelMessage::from_borrowed(message) else {
            return false;
        };
        let Some(channel) = sora
            .engine
            .ipc_mut()
            .channel_pair_mut(sora.keyboard_channel)
        else {
            return false;
        };
        if channel.write(sora.keyboard_kernel_end, msg).is_err() {
            return false;
        }
    }
    crate::user_thread::wake_user();
    true
}

/// P8-b: check the root channel for a completed command line from Sora. If one is
/// waiting, run it through the kernel shell and return the byte count. Returns 0 if
/// no line is available. The caller should emit the prompt after the command output.
pub fn poll_root_command(
    tasks: &[crate::shell::TaskInfo],
    env: &mut crate::shell::ShellEnv,
) -> usize {
    let sora = unsafe { &mut *sora_ptr() };
    let Some(channel) = sora.engine.ipc_mut().channel_pair_mut(sora.root_channel) else {
        return 0;
    };
    let Ok(message) = channel.read(sora.kernel_end) else {
        return 0;
    };
    let bytes = message.bytes();
    if bytes.is_empty() {
        return 0;
    }
    let line = core::str::from_utf8(bytes).unwrap_or("");
    env.uptime_ns = kumo_hal::active::monotonic_nanos();
    let preempt = crate::kdemo::preempt_stats();
    env.preempt_ticks = preempt.ticks;
    env.preempt_switches = preempt.switches;
    let mut out = crate::bootstrap::console::Writer;
    crate::shell::run_command(line, env, tasks, &mut out);
    line.len()
}

/// Offer a console fragment to the live Sora server. Returns true if Sora rendered it
/// (the caller must not also write it directly). Refuses — returning false for the
/// direct fallback — unless routing is enabled AND Sora is alive and **parked**: the
/// parked check is both the liveness test and the reentrancy guard, since any console
/// write issued while Sora itself is running (its own `DebugWrite`, an SVC handler, a
/// fault path) must take the direct path rather than wake the thread we are inside of.
pub fn try_console_route(bytes: &[u8]) -> bool {
    if !CONSOLE_ROUTE.load(core::sync::atomic::Ordering::Acquire) {
        return false;
    }
    if !crate::user_thread::is_started()
        || crate::user_thread::is_done()
        || !crate::user_thread::is_parked()
    {
        return false;
    }
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + ROUTE_CHUNK).min(bytes.len());
        if !deliver_to_sora(&bytes[offset..end]) {
            // Render the undelivered remainder directly ourselves — never hand it back
            // to the caller, which would double-print the chunks already delivered.
            kumo_hal::active::early_console_write(&bytes[offset..]);
            break;
        }
        offset = end;
    }
    true
}

/// Run Sora with supervised restart: if Sora exits non-zero or crashes, re-spawn up to
/// [`MAX_SORA_ATTEMPTS`] times. Returns the last successful report, or an error if all
/// attempts are exhausted.
pub fn run_sora(boot: &BootInfo, initrd: &[u8]) -> Result<UserReport, UsermodeError> {
    let recipe = build_recipe(initrd)?;
    let mut last_report = None;

    for attempt in 0..MAX_SORA_ATTEMPTS {
        match attempt_sora(boot, &recipe, initrd) {
            Ok((report, 0)) => {
                // Clean exit.
                let mut report = report;
                report.attempts = attempt;
                return Ok(report);
            }
            Ok((report, exit_code)) => {
                // Non-zero exit — may retry.
                last_report = Some((report, exit_code));
            }
            Err(err) => {
                // Bootstrap error (e.g. out of frames) — not retryable.
                return Err(err);
            }
        }
    }

    // All attempts exhausted.
    if let Some((_report, exit_code)) = last_report {
        Err(UsermodeError::SoraExhausted {
            exit_code,
            attempts: MAX_SORA_ATTEMPTS,
        })
    } else {
        Err(UsermodeError::ChannelSetup)
    }
}
