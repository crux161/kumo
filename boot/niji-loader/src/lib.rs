#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod elf;

use kumo_abi::{BootInfo, MemRegion, MemRegionKind, Range, ABI_VERSION};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandoffError {
    AbiVersion { expected: u32, found: u32 },
    MissingMemoryMap,
    MissingKernelImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HandoffSummary {
    pub kernel_phys: Range,
    pub kernel_virt: Range,
    pub initrd: Range,
    pub mem_region_count: u64,
    pub total_bytes: u64,
    pub usable_bytes: u64,
    pub has_framebuffer: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemorySummary {
    pub region_count: u64,
    pub total_bytes: u64,
    pub usable_bytes: u64,
}

pub fn summarize_memory(regions: &[MemRegion]) -> MemorySummary {
    let mut summary = MemorySummary {
        region_count: regions.len() as u64,
        total_bytes: 0,
        usable_bytes: 0,
    };

    for region in regions {
        summary.total_bytes = summary.total_bytes.saturating_add(region.range.len);
        if region.kind == MemRegionKind::Usable {
            summary.usable_bytes = summary.usable_bytes.saturating_add(region.range.len);
        }
    }

    summary
}

/// A snapshot of what Nijigumo has discovered about the machine *before* the
/// kernel image is loaded.
///
/// Unlike [`validate_boot_info`], this does not require `kernel_phys`/`kernel_virt`
/// to be populated: it describes the platform-discovery milestone (memory map +
/// framebuffer + device tree gathered) where the kernel and initrd are still
/// pending. It is what the loader prints as its honest first-light report and what
/// Stage-A can later reuse to derive memory totals (Guidance 002 §4).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlatformDiscovery {
    pub region_count: u64,
    pub total_bytes: u64,
    pub usable_bytes: u64,
    pub has_framebuffer: bool,
    pub fb_width: u32,
    pub fb_height: u32,
    pub has_dtb: bool,
    pub dtb_addr: u64,
    pub has_acpi_rsdp: bool,
    pub has_kernel: bool,
    pub has_initrd: bool,
}

/// Summarize the platform handoff without demanding a loaded kernel image.
///
/// # Safety
///
/// `boot.mem_regions` must either be empty or point at a valid `MemRegion` slice
/// for the duration of the call (the same contract as [`validate_boot_info`]).
pub unsafe fn summarize_platform(boot: &BootInfo) -> PlatformDiscovery {
    let memory = unsafe { summarize_memory(boot.mem_regions.as_slice()) };
    PlatformDiscovery {
        region_count: memory.region_count,
        total_bytes: memory.total_bytes,
        usable_bytes: memory.usable_bytes,
        has_framebuffer: boot.has_framebuffer(),
        fb_width: boot.framebuffer.width,
        fb_height: boot.framebuffer.height,
        has_dtb: boot.platform.dtb != 0,
        dtb_addr: boot.platform.dtb,
        has_acpi_rsdp: boot.platform.acpi_rsdp != 0,
        has_kernel: !boot.kernel_phys.is_empty() && !boot.kernel_virt.is_empty(),
        has_initrd: !boot.initrd.is_empty(),
    }
}

pub fn validate_boot_info(boot: &BootInfo) -> Result<HandoffSummary, HandoffError> {
    if boot.version != ABI_VERSION {
        return Err(HandoffError::AbiVersion {
            expected: ABI_VERSION,
            found: boot.version,
        });
    }

    if boot.mem_regions.is_empty() {
        return Err(HandoffError::MissingMemoryMap);
    }

    if boot.kernel_phys.is_empty() || boot.kernel_virt.is_empty() {
        return Err(HandoffError::MissingKernelImage);
    }

    let memory = unsafe { summarize_memory(boot.mem_regions.as_slice()) };

    Ok(HandoffSummary {
        kernel_phys: boot.kernel_phys,
        kernel_virt: boot.kernel_virt,
        initrd: boot.initrd,
        mem_region_count: memory.region_count,
        total_bytes: memory.total_bytes,
        usable_bytes: memory.usable_bytes,
        has_framebuffer: boot.has_framebuffer(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_abi::{MemRegionKind, RawSlice};

    #[test]
    fn rejects_empty_boot_info() {
        let boot = BootInfo::empty(ABI_VERSION);
        assert_eq!(
            validate_boot_info(&boot),
            Err(HandoffError::MissingMemoryMap)
        );
    }

    #[test]
    fn rejects_null_memory_map_pointer() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_raw_parts(0, 1);
        boot.kernel_phys = Range::new(0x80000, 0x20000);
        boot.kernel_virt = Range::new(0xffff_0000_0008_0000, 0x20000);
        assert_eq!(
            validate_boot_info(&boot),
            Err(HandoffError::MissingMemoryMap)
        );
    }

    #[test]
    fn accepts_minimal_handoff() {
        static REGIONS: [MemRegion; 1] = [MemRegion {
            range: Range {
                start: 0x1000,
                len: 0x4000,
            },
            kind: MemRegionKind::Usable,
            _reserved: 0,
        }];
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&REGIONS);
        boot.kernel_phys = Range::new(0x80000, 0x20000);
        boot.kernel_virt = Range::new(0xffff_0000_0008_0000, 0x20000);
        let summary = validate_boot_info(&boot).unwrap();
        assert_eq!(summary.mem_region_count, 1);
        assert_eq!(summary.total_bytes, 0x4000);
        assert_eq!(summary.usable_bytes, 0x4000);
        assert!(!summary.has_framebuffer);
    }

    #[test]
    fn platform_discovery_reports_devices_without_a_kernel() {
        use kumo_abi::{Framebuffer, FramebufferFormat};

        static REGIONS: [MemRegion; 2] = [
            MemRegion {
                range: Range::new(0x4000_0000, 0x2000_0000),
                kind: MemRegionKind::Usable,
                _reserved: 0,
            },
            MemRegion {
                range: Range::new(0x6000_0000, 0x1000_0000),
                kind: MemRegionKind::Mmio,
                _reserved: 0,
            },
        ];
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&REGIONS);
        boot.platform.dtb = 0x4080_0000;
        boot.framebuffer = Framebuffer {
            phys: 0x9000_0000,
            len: 0x0080_0000,
            width: 1280,
            height: 800,
            stride: 1280,
            format: FramebufferFormat::Bgr,
        };
        boot.flags |= BootInfo::FLAG_FRAMEBUFFER_PRESENT;

        // Kernel image is deliberately still empty: validate_boot_info would
        // reject this, but discovery describes it honestly.
        assert_eq!(
            validate_boot_info(&boot),
            Err(HandoffError::MissingKernelImage)
        );

        let discovery = unsafe { summarize_platform(&boot) };
        assert_eq!(discovery.region_count, 2);
        assert_eq!(discovery.total_bytes, 0x3000_0000);
        assert_eq!(discovery.usable_bytes, 0x2000_0000);
        assert!(discovery.has_framebuffer);
        assert_eq!(discovery.fb_width, 1280);
        assert_eq!(discovery.fb_height, 800);
        assert!(discovery.has_dtb);
        assert_eq!(discovery.dtb_addr, 0x4080_0000);
        assert!(!discovery.has_acpi_rsdp);
        assert!(!discovery.has_kernel);
        assert!(!discovery.has_initrd);
    }

    #[test]
    fn summarizes_usable_memory_only_from_usable_regions() {
        let regions = [
            MemRegion {
                range: Range::new(0, 10),
                kind: MemRegionKind::Reserved,
                _reserved: 0,
            },
            MemRegion {
                range: Range::new(10, 20),
                kind: MemRegionKind::Usable,
                _reserved: 0,
            },
        ];
        let summary = summarize_memory(&regions);
        assert_eq!(summary.region_count, 2);
        assert_eq!(summary.total_bytes, 30);
        assert_eq!(summary.usable_bytes, 20);
    }
}
