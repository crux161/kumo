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

use kumo_abi::sys::Syscall;

use crate::mm::{Vmar, PAGE_SIZE};
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
        let ptr = r[0] as *const u8;
        let len = (r[1] as usize).min(256);
        // EL1 may read the EL0 window (AP=EL0-RW is also EL1-RW, identity-mapped).
        let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
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
    } else {
        r[0] = u64::MAX; // ENOSYS
    }
}

/// Run the EL0 syscall smoke and report what userspace did.
pub fn run() -> UserReport {
    let mut engine = SyscallEngine::new();
    let job = Job::root(engine.objects_mut());
    let vmar = Vmar::new(0xffff_0000_0000_0000, PAGE_SIZE * 256).expect("user vmar");
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
    let report = kumo_hal::active::run_el0_smoke();

    // SAFETY: initialized above; single-threaded read after EL0 has exited.
    let demo = unsafe { &*demo_ptr() };
    UserReport {
        entered: report.entered,
        syscalls: report.syscalls,
        wrote: demo.wrote,
        chan: demo.chan,
        exit_code: report.exit_code,
    }
}
