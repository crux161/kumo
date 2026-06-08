#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod bootstrap;
pub mod mm;
pub mod object;
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

    stage_a_console_banner();
    kumo_hal::active::halt()
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
