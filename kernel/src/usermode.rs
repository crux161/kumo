//! P5 (opening): EL0 syscalls routed through the real `SyscallEngine`.
//!
//! Builds a demo `Process` + `SyscallEngine`, registers a HAL SVC hook, then drops to
//! EL0 (via the HAL mechanism, journal 042) where a tiny payload issues *real* syscalls
//! against the kernel-object ABI:
//!   * `DebugWrite` — the kernel reads the user-supplied string out of the EL0 window and
//!     prints it (user -> kernel memory + console), proving argument passing,
//!   * `ChannelCreate` — dispatched through `SyscallEngine`, minting genuine handles on
//!     the trap path (the first time the host-tested engine runs from a real syscall),
//!   * `ProcessExit` — trampolines back to the boot flow.
//!
//! Soundness: the demo state lives in a `static` reached only from the SVC hook, which
//! runs single-threaded at EL1 with IRQs masked while EL0 is blocked in the trap — no
//! re-entrancy, no concurrency.

use core::cell::UnsafeCell;

use alloc::vec::Vec;

use kumo_abi::{find_file, sys::Syscall, BootInfo, Handle, InitrdError, SORA_INIT_PATH};
use kumo_hal::active::{UserImage, UserImageError, UserLoadSegment};
use kumo_hal::PageFlags;
use kumo_ipc::Message;

use crate::bootstrap::user::{
    plan_elf_process, UserBootstrapError, USER_IMAGE_BASE, USER_ROOT_BASE, USER_ROOT_SIZE,
    USER_STACK_SIZE, USER_STACK_TOP,
};
use crate::mm::{alloc_zeroed_frame, Vmar};
use crate::syscall::{KernelCall, KernelCallResult, SyscallEngine};
use crate::task::{Job, Process};

struct Demo {
    engine: SyscallEngine,
    process: Process,
    wrote: usize,
    chan: (u32, u32),
}

struct DemoCell(UnsafeCell<Option<Demo>>);
// The EL0 smoke runs single-threaded with interrupts masked; never accessed concurrently.
unsafe impl Sync for DemoCell {}
static DEMO: DemoCell = DemoCell(UnsafeCell::new(None));

fn demo_ptr() -> *mut Demo {
    let opt: *mut Option<Demo> = DEMO.0.get();
    // SAFETY: `run` initializes DEMO before the hook can fire; single-threaded.
    unsafe { (&mut *opt).as_mut().expect("usermode demo not initialized") as *mut Demo }
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsermodeError {
    Initrd(InitrdError),
    MissingSora,
    Bootstrap(UserBootstrapError),
    Image(UserImageError),
    BadSegmentRange,
    ChannelSetup,
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

/// Validate an EL0-supplied pointer range before the kernel dereferences it. With
/// per-process TTBR0 (P5-mmu-b), the user's VA is only mapped while its address space is
/// active — but a malicious EL0 could pass a *kernel-half* VA to trick EL1 into reading
/// through TTBR1. Require `[ptr, ptr+len)` to sit wholly inside the process's user VMAR
/// (the low half); reject anything else. (Real `copy_from_user`/`copy_to_user` with a
/// fault-fixup handler is future work; this is the structural bound check.)
fn user_range_ok(process: &Process, ptr: u64, len: u64) -> bool {
    let vmar = process.root_vmar();
    let base = vmar.base();
    match (base.checked_add(vmar.len()), ptr.checked_add(len)) {
        (Some(vmar_end), Some(end)) => ptr >= base && end <= vmar_end,
        _ => false,
    }
}

/// The kernel's EL0 syscall handler (registered with the HAL). `regs` points at the
/// saved x0..x30: x8 = syscall number, args in x0.., results written back to x0/x1.
extern "C" fn svc_hook(regs: *mut u64) {
    // SAFETY: the HAL hands us a valid pointer to the 31 saved EL0 registers.
    let r = unsafe { core::slice::from_raw_parts_mut(regs, 31) };
    let num = r[8];

    if num == Syscall::ProcessExit as u64 {
        kumo_hal::active::el0_exit(r[0]); // restores the kernel context; never returns
    }

    // SAFETY: initialized in `run`; single-threaded, IRQs masked while EL0 is trapped.
    let demo = unsafe { &mut *demo_ptr() };

    if num == Syscall::DebugWrite as u64 {
        let user_ptr = r[0];
        let len = (r[1] as usize).min(256);
        if !user_range_ok(&demo.process, user_ptr, len as u64) {
            r[0] = u64::MAX; // EFAULT-like: refuse to read outside the user VMAR
            return;
        }
        // EL1 reads the user VA while the process TTBR0 is active (AP EL0-RW/RO is also
        // EL1-readable); the range was just bounds-checked into the user half.
        let bytes = unsafe { core::slice::from_raw_parts(user_ptr as *const u8, len) };
        crate::bootstrap::console::write(bytes);
        demo.wrote += len;
        r[0] = len as u64;
    } else if num == Syscall::ChannelCreate as u64 {
        match demo
            .engine
            .dispatch(&mut demo.process, KernelCall::ChannelCreate)
        {
            KernelCallResult::Handles { first, second } => {
                demo.chan = (first.0, second.0);
                r[0] = first.0 as u64;
                r[1] = second.0 as u64;
            }
            _ => r[0] = u64::MAX,
        }
    } else if num == Syscall::ChannelWrite as u64 {
        // x0 = handle, x1 = bytes ptr (user VA), x2 = len. Status returned in x0.
        let channel = Handle(r[0] as u32);
        let user_ptr = r[1];
        let len = (r[2] as usize).min(256);
        if !user_range_ok(&demo.process, user_ptr, len as u64) {
            r[0] = (-1i32) as u32 as u64;
            return;
        }
        // SAFETY: bounds-checked into the user VMAR, readable by EL1 under the process map.
        let bytes = unsafe { core::slice::from_raw_parts(user_ptr as *const u8, len) };
        let status = match Message::new(1, bytes, &[]) {
            Ok(message) => match demo.engine.dispatch(
                &mut demo.process,
                KernelCall::ChannelWrite { channel, message },
            ) {
                KernelCallResult::Status(s) => s,
                _ => -1,
            },
            Err(_) => -1,
        };
        r[0] = status as u32 as u64;
    } else if num == Syscall::ChannelRead as u64 {
        // x0 = handle, x1 = dst buffer (user VA), x2 = capacity. Bytes read returned in x0.
        let channel = Handle(r[0] as u32);
        let user_buf = r[1];
        let cap = (r[2] as usize).min(256);
        if !user_range_ok(&demo.process, user_buf, cap as u64) {
            r[0] = 0; // refuse to write outside the user VMAR -> 0 bytes
            return;
        }
        match demo
            .engine
            .dispatch(&mut demo.process, KernelCall::ChannelRead { channel })
        {
            KernelCallResult::Message(message) => {
                let bytes = message.bytes();
                let n = bytes.len().min(cap);
                // SAFETY: dst bounds-checked into the user VMAR (an RW page), writable by EL1.
                unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), user_buf as *mut u8, n) };
                r[0] = n as u64;
            }
            _ => r[0] = 0, // nothing pending / error -> 0 bytes
        }
    } else {
        r[0] = u64::MAX; // ENOSYS
    }
}

/// Run the EL0 syscall smoke and report what userspace did. `boot` supplies the frame
/// allocator the per-process page tables are built from.
pub fn run(boot: &BootInfo) -> UserReport {
    let mut engine = SyscallEngine::new();
    let job = Job::root(engine.objects_mut());
    // Match the smoke's actual VAs so the SVC pointer-validation accepts its accesses.
    let vmar = Vmar::new(USER_ROOT_BASE, USER_ROOT_SIZE).expect("user vmar");
    let process = Process::new(engine.objects_mut(), &job, vmar);

    // SAFETY: first writer; nothing else touches DEMO yet.
    unsafe {
        *DEMO.0.get() = Some(Demo {
            engine,
            process,
            wrote: 0,
            chan: (0, 0),
        });
    }

    kumo_hal::active::set_svc_hook(svc_hook);
    // SAFETY: runs before the TTBR0 switch, so frames are reachable via the identity map.
    let mut alloc = || unsafe { alloc_zeroed_frame(boot) };
    let report = kumo_hal::active::run_el0_smoke(
        USER_IMAGE_BASE,
        USER_STACK_TOP,
        USER_STACK_SIZE,
        &mut alloc,
    )
    .unwrap_or_default();

    // SAFETY: initialized above; single-threaded read after EL0 has exited.
    let demo = unsafe { &*demo_ptr() };
    UserReport {
        entered: report.entered,
        syscalls: report.syscalls,
        wrote: demo.wrote,
        chan: demo.chan,
        exit_code: report.exit_code,
        ..Default::default()
    }
}

pub fn run_sora(boot: &BootInfo, initrd: &[u8]) -> Result<UserReport, UsermodeError> {
    let sora = find_file(initrd, SORA_INIT_PATH)?.ok_or(UsermodeError::MissingSora)?;
    let mut engine = SyscallEngine::new();
    let plan = plan_elf_process(engine.objects_mut(), sora.bytes)?;

    let mut load_segments = Vec::new();
    for segment in &plan.load_segments {
        let start =
            usize::try_from(segment.file_offset).map_err(|_| UsermodeError::BadSegmentRange)?;
        let len = usize::try_from(segment.file_size).map_err(|_| UsermodeError::BadSegmentRange)?;
        let end = start
            .checked_add(len)
            .ok_or(UsermodeError::BadSegmentRange)?;
        if end > sora.bytes.len() {
            return Err(UsermodeError::BadSegmentRange);
        }
        load_segments.push(UserLoadSegment {
            source: &sora.bytes[start..end],
            virt_addr: segment.virt_addr,
            mem_size: segment.mem_size,
            writable: segment.flags.contains(PageFlags::WRITE),
            executable: segment.flags.contains(PageFlags::EXECUTE),
        });
    }

    // SAFETY: first writer for this EL0 descent; SVC hook runs synchronously.
    unsafe {
        *DEMO.0.get() = Some(Demo {
            engine,
            process: plan.process,
            wrote: 0,
            chan: (0, 0),
        });
    }

    // Provision the root channel *before* Sora runs. `first` = Sora's end (handed to it as
    // the bootstrap handle in x0), `second` = the kernel's end (we read it after exit).
    let (sora_end, kernel_end) = {
        // SAFETY: DEMO just initialized; single-threaded.
        let demo = unsafe { &mut *demo_ptr() };
        match demo
            .engine
            .dispatch(&mut demo.process, KernelCall::ChannelCreate)
        {
            KernelCallResult::Handles { first, second } => {
                demo.chan = (first.0, second.0);
                (first, second)
            }
            _ => return Err(UsermodeError::ChannelSetup),
        }
    };

    // Seed Sora's inbox with a kernel boot message: writing to our end (`kernel_end`)
    // lands it in Sora's end. Sora `ChannelRead`s it, echoes it, and replies.
    {
        // SAFETY: DEMO initialized; single-threaded.
        let demo = unsafe { &mut *demo_ptr() };
        if let Ok(message) = Message::new(1, b"kernel->sora boot\n", &[]) {
            let _ = demo.engine.dispatch(
                &mut demo.process,
                KernelCall::ChannelWrite {
                    channel: kernel_end,
                    message,
                },
            );
        }
    }

    kumo_hal::active::set_svc_hook(svc_hook);
    // SAFETY: runs before the TTBR0 switch, so frames are reachable via the identity map.
    let mut alloc = || unsafe { alloc_zeroed_frame(boot) };
    let report = kumo_hal::active::run_el0_image(
        UserImage {
            entry: plan.entry,
            stack_top: plan.stack_top,
            stack_size: USER_STACK_SIZE,
            bootstrap: sora_end.0 as u64,
            segments: &load_segments,
        },
        &mut alloc,
    )?;

    // Read what Sora sent down the root channel during its run.
    let mut handshake = [0u8; 32];
    let mut handshake_len = 0;
    {
        // SAFETY: initialized above; single-threaded read after EL0 has exited.
        let demo = unsafe { &mut *demo_ptr() };
        if let KernelCallResult::Message(message) = demo.engine.dispatch(
            &mut demo.process,
            KernelCall::ChannelRead {
                channel: kernel_end,
            },
        ) {
            let bytes = message.bytes();
            handshake_len = bytes.len().min(handshake.len());
            handshake[..handshake_len].copy_from_slice(&bytes[..handshake_len]);
        }
    }

    // SAFETY: initialized above; single-threaded read after EL0 has exited.
    let demo = unsafe { &*demo_ptr() };
    Ok(UserReport {
        entered: report.entered,
        syscalls: report.syscalls,
        wrote: demo.wrote,
        chan: demo.chan,
        exit_code: report.exit_code,
        handshake,
        handshake_len,
    })
}
