#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod bootstrap;
pub mod ipc;
pub mod ipcdemo;
pub mod kdemo;
pub mod mm;
pub mod object;
pub mod sched;
pub mod shell;
pub mod syscall;
pub mod task;

use kumo_abi::{BootInfo, ABI_VERSION};
use niji_loader::{validate_boot_info, HandoffError, HandoffSummary};

pub const STAGE_A_BANNER: &str = "KUMO Ziwei Stage-A core only; halting";

#[macro_export]
macro_rules! klog {
    ($($arg:tt)*) => {
        $crate::bootstrap::console::write_fmt(core::format_args!($($arg)*))
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelInitReport {
    pub abi_version: u32,
    pub arch: &'static str,
    pub mem_region_count: u64,
    pub total_bytes: u64,
    pub usable_bytes: u64,
    pub has_initrd: bool,
    pub has_framebuffer: bool,
}

pub fn inspect_boot(boot: &BootInfo) -> Result<KernelInitReport, HandoffError> {
    let summary = validate_boot_info(boot)?;
    Ok(inspect_handoff(&summary))
}

pub fn inspect_handoff(summary: &HandoffSummary) -> KernelInitReport {
    KernelInitReport {
        abi_version: ABI_VERSION,
        arch: kumo_hal::active::arch_name(),
        mem_region_count: summary.mem_region_count,
        total_bytes: summary.total_bytes,
        usable_bytes: summary.usable_bytes,
        has_initrd: !summary.initrd.is_empty(),
        has_framebuffer: summary.has_framebuffer,
    }
}

pub fn stage_a_banner() -> &'static str {
    STAGE_A_BANNER
}

pub fn stage_a_console_banner() {
    bootstrap::console::write_str(STAGE_A_BANNER);
    bootstrap::console::write_str("\n");
}

pub fn stage_a(boot: &BootInfo) -> ! {
    let report = match inspect_boot(boot) {
        Ok(report) => report,
        Err(err) => tower_halt_ascii("nijigumo->ziwei handoff invalid", Some(err)),
    };

    // Install our own exception vectors first thing, so any fault from here on is
    // reported (and frozen) instead of resetting the machine through stale firmware
    // vectors. Interrupts stay masked (see `_start`) until we own the GIC.
    kumo_hal::active::install_exception_vectors();

    // Route Stage-A output to the framebuffer when the board handed one over (e.g.
    // the X13s, which has no UART here); otherwise the PL011 fallback is used.
    if boot.has_framebuffer() {
        let fb = boot.framebuffer;
        kumo_hal::active::set_framebuffer(fb.phys, fb.len, fb.width, fb.height, fb.stride);
    }

    klog!(
        "[NIJIGUMO] HANDOFF COMPLETE abi=v{} arch={}\n",
        report.abi_version,
        report.arch
    );
    klog!("CPU MODE: Executive (EL1/Ring0)\n");

    // M1: bring up memory. The bump heap is already online; account the frames and
    // prove the allocator yields real addresses (Guidance 002 §5: AETHER is real now).
    let mm = unsafe { mm::init(boot) };
    klog!(
        "AETHER: {} MiB usable / {} MiB total, {} frames  OK\n",
        report.usable_bytes >> 20,
        report.total_bytes >> 20,
        mm.usable_frames
    );
    if mm.sample_count > 0 {
        klog!("AETHER: first free frames");
        let mut i = 0;
        while i < mm.sample_count {
            klog!(" {:#x}", mm.sample_frames[i]);
            i += 1;
        }
        klog!("\n");
    }

    // Prove the heap works on real silicon: build a small Vec and reduce it.
    let mut squares: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
    let mut n = 1u32;
    while n <= 8 {
        squares.push(n * n);
        n += 1;
    }
    let sum: u32 = squares.iter().copied().sum();
    klog!(
        "HEAP: bump {} KiB online; vec self-test sum={}  OK\n",
        mm.heap_bytes >> 10,
        sum
    );

    // Take ownership of the MMU: build KUMO's own identity page tables and switch off
    // the firmware's. Everything below now runs under kernel-owned paging — the
    // foundation for per-process address spaces and EL0/userspace.
    match unsafe { mm::enable_paging(boot) } {
        Some(p) => klog!(
            "PAGING: kernel page tables active; {} MiB identity-mapped, {} tables  OK\n",
            p.mapped_bytes >> 20,
            p.tables
        ),
        None => klog!("PAGING: skipped (no usable memory map)\n"),
    }

    match kumo_hal::active::init_timer_interrupts(boot.platform.dtb, 20) {
        Ok(timer) => {
            let start = kumo_hal::active::timer_irq_count();
            let seen = kumo_hal::active::wait_for_timer_irqs(start, 3, 1_000_000_000);
            if seen >= 3 {
                klog!(
                    "M2: GIC/timer IRQ {} @ {} Hz; heartbeat {} ticks  OK\n",
                    timer.irq,
                    timer.counter_hz,
                    seen
                );
            } else {
                klog!(
                    "M2: GIC/timer IRQ {} armed, but heartbeat timed out after {} ticks\n",
                    timer.irq,
                    seen
                );
                kumo_hal::active::halt();
            }
        }
        Err(err) => {
            klog!("M2: GIC/timer unavailable: {:?}\n", err);
            kumo_hal::active::halt();
        }
    }

    // M3 (opening): run more than one thread of control. A couple of kernel threads
    // cooperatively yield through the HAL context switch — the first real use of the
    // task substrate on the boot path. Preemptive, timer-driven scheduling is next.
    let m3 = kdemo::run();
    klog!(
        "M3: cooperative ctx-switch OK; {} kthreads, {} switches, {} work units\n",
        m3.threads,
        m3.switches,
        m3.work
    );

    // M3: the scheduler substrate (DESIGN/003). Discipline A — the O(1) strict-priority
    // class — replaces the old flat round-robin. A deterministic self-test proves the
    // two-level bitmap selects in strict-priority/FIFO order, the idle floor always has
    // a thread, and a more-urgent thread preempts a running one.
    let sc = sched::smoke();
    if sc.ordered && sc.idle_floor && sc.preemptions == 2 {
        klog!(
            "SCHED: O(1) strict-priority (Discipline A); {}-level bitmap, {} picks ordered, idle floor, {} preempts  OK\n",
            sc.levels,
            sc.picks,
            sc.preemptions
        );
    } else {
        klog!(
            "SCHED: self-test FAILED; ordered={} idle_floor={} preempts={}\n",
            sc.ordered,
            sc.idle_floor,
            sc.preemptions
        );
        kumo_hal::active::halt();
    }

    let preempt = kdemo::run_preemption();
    klog!(
        "SCHED: timer preemption OK; {} kthreads, {} IRQ ticks, {} body switches, work {}/{}\n",
        preempt.threads,
        preempt.ticks,
        preempt.switches,
        preempt.work[0],
        preempt.work[1]
    );

    match ipc::smoke() {
        Ok(ipc) => klog!(
            "P4: IPC channel/call/port OK; {} channel pair, {} port, {} call, {} bytes, {} handles\n",
            ipc.channels,
            ipc.ports,
            ipc.calls,
            ipc.bytes,
            ipc.handle_count
        ),
        Err(err) => {
            klog!("P4: IPC smoke failed: {:?}\n", err);
            kumo_hal::active::halt();
        }
    }

    // P4: blocking IPC between two running kernel threads. The consumer parks on an
    // empty channel and the producer wakes it — real scheduler-integrated block/wake,
    // driven by the cooperative context switch.
    let blk = ipcdemo::run();
    if blk.delivered {
        klog!(
            "P4: blocking IPC OK; consumer parked {}x, producer woke it {}x, delivered {} bytes\n",
            blk.consumer_blocks,
            blk.wakes,
            blk.received
        );
    } else {
        klog!(
            "P4: blocking IPC FAILED; parked {}x woke {}x received {} bytes\n",
            blk.consumer_blocks,
            blk.wakes,
            blk.received
        );
    }

    if report.has_framebuffer {
        // Framebuffer console (e.g. the X13s): no kernel keyboard yet, so idle here.
        // The screen keeps the boot report; a pre-handoff pause lives in the loader.
        stage_a_console_banner();
        kumo_hal::active::halt()
    } else {
        // Serial console (QEMU PL011): an interactive command shell — KUMO's first
        // interactive surface, and the ancestor of the userspace shell.
        let env = shell::ShellEnv {
            arch: report.arch,
            abi_version: report.abi_version,
            usable_frames: mm.usable_frames,
            usable_bytes: report.usable_bytes,
            total_bytes: report.total_bytes,
            heap_kib: mm.heap_bytes >> 10,
            uptime_ns: 0,
            preempt_ticks: 0,
            preempt_switches: 0,
        };
        serial_shell(env)
    }
}

/// Stage-A serial command shell over the PL011 console: line-edit input, dispatch
/// each line to the (host-tested) `shell::run_command`, print its output, reprompt.
fn serial_shell(mut env: shell::ShellEnv) -> ! {
    use alloc::string::String;

    const MAX_LINE: usize = 256;

    kdemo::install_preemption_probe();

    klog!("\nKUMO Ziwei Stage-A serial shell. Type 'help'.\n");
    klog!("{}", shell::PROMPT);

    let mut line = String::new();
    loop {
        match kumo_hal::active::console_read_byte() {
            Some(b'\r') | Some(b'\n') => {
                bootstrap::console::write(b"\r\n");
                env.uptime_ns = kumo_hal::active::monotonic_nanos();
                let preempt = kdemo::preempt_stats();
                env.preempt_ticks = preempt.ticks;
                env.preempt_switches = preempt.switches;
                let tasks = kdemo::tasks();
                let mut out = bootstrap::console::Writer;
                shell::run_command(&line, &env, &tasks, &mut out);
                line.clear();
                klog!("{}", shell::PROMPT);
            }
            Some(0x08) | Some(0x7f) => {
                if line.pop().is_some() {
                    bootstrap::console::write(b"\x08 \x08");
                }
            }
            Some(byte @ 0x20..=0x7e) => {
                if line.len() < MAX_LINE {
                    line.push(byte as char);
                    bootstrap::console::write(&[byte]);
                }
            }
            Some(_) => {}
            None => kumo_hal::active::spin_once(),
        }
    }
}

#[no_mangle]
pub extern "C" fn kmain(boot: *const BootInfo) -> ! {
    if boot.is_null() {
        tower_halt_ascii("nijigumo->ziwei handoff pointer is null", None);
    }

    let boot = unsafe { &*boot };
    stage_a(boot)
}

pub fn expected_abi_version() -> u32 {
    ABI_VERSION
}

fn tower_halt_ascii(reason: &str, error: Option<HandoffError>) -> ! {
    klog!("TOWER: ");
    klog!("{}", reason);
    if let Some(error) = error {
        klog!(": {:?}", error);
    }
    klog!("\nHALT\n");
    kumo_hal::active::halt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_abi::{MemRegion, MemRegionKind, Range, RawSlice};

    #[test]
    #[cfg(feature = "arch_aarch64")]
    fn reports_arm64_when_selected() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&TEST_REGIONS);
        boot.kernel_phys = Range::new(0x80000, 0x20000);
        boot.kernel_virt = Range::new(0xffff_0000_0008_0000, 0x20000);
        boot.initrd = Range::new(0x90000, 0x4000);
        let report = inspect_boot(&boot).unwrap();
        assert_eq!(report.arch, "aarch64");
        assert_eq!(report.mem_region_count, 2);
        assert_eq!(report.usable_bytes, 0x5000);
        assert_eq!(report.total_bytes, 0x6000);
        assert!(report.has_initrd);
    }

    #[test]
    #[cfg(feature = "arch_x86_64")]
    fn reports_x86_64_when_selected() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&TEST_REGIONS);
        boot.kernel_phys = Range::new(0x80000, 0x20000);
        boot.kernel_virt = Range::new(0xffff_8000_0008_0000, 0x20000);
        let report = inspect_boot(&boot).unwrap();
        assert_eq!(report.arch, "x86_64");
        assert_eq!(report.mem_region_count, 2);
        assert_eq!(report.usable_bytes, 0x5000);
    }

    static TEST_REGIONS: [MemRegion; 2] = [
        MemRegion {
            range: Range {
                start: 0x1000,
                len: 0x5000,
            },
            kind: MemRegionKind::Usable,
            _reserved: 0,
        },
        MemRegion {
            range: Range {
                start: 0x6000,
                len: 0x1000,
            },
            kind: MemRegionKind::Reserved,
            _reserved: 0,
        },
    ];
}
