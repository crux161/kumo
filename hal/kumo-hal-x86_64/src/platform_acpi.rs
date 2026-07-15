//j444
//j445
//j446
//j447

//! Legacy IA-PC ACPI discovery windows.

#[cfg(target_os = "none")]
use niji_loader::acpi::find_rsdp;
#[cfg(any(target_os = "none", test))]
use niji_loader::acpi::madt::{InterruptPolarity, InterruptTriggerMode, Madt, MadtEntry};
#[cfg(target_os = "none")]
use niji_loader::acpi::{RootTable, SdtHeader, SDT_HEADER_LEN};
use niji_loader::acpi::{RootTableKind, Rsdp, RsdpLocation};

#[cfg(target_os = "none")]
const EBDA_SEGMENT_POINTER: usize = 0x040e;
#[cfg(target_os = "none")]
const EBDA_SCAN_LEN: usize = 1024;
#[cfg(target_os = "none")]
const EBDA_MIN: u64 = 0x0400;
#[cfg(target_os = "none")]
const CONVENTIONAL_MEMORY_END: u64 = 0x0a_0000;
#[cfg(target_os = "none")]
const BIOS_ROM_START: u64 = 0x0e_0000;
#[cfg(target_os = "none")]
const BIOS_ROM_LEN: usize = 0x02_0000;
#[cfg(any(target_os = "none", test))]
const KERNEL_PHYS_MAP_END: u64 = crate::paging::USER_BASE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcpiRootReport {
    pub rsdp_address: u64,
    pub revision: u8,
    pub root_address: u64,
    pub uses_xsdt: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcpiMadtReport {
    pub address: u64,
    pub local_interrupt_controller_address: u32,
    pub pcat_compatible: bool,
    pub io_apic_count: usize,
    pub first_io_apic_address: Option<u32>,
    pub first_io_apic_gsi_base: Option<u32>,
    pub source_override_count: usize,
    pub legacy_timer_route: Option<AcpiLegacyIrqRoute>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcpiLegacyIrqRoute {
    pub isa_irq: u8,
    pub global_system_interrupt: u32,
    pub active_low: bool,
    pub level_triggered: bool,
    pub overridden: bool,
    pub candidate_io_apic_id: u8,
    pub candidate_io_apic_address: u32,
    pub candidate_io_apic_gsi_base: u32,
}

impl From<RsdpLocation> for AcpiRootReport {
    fn from(location: RsdpLocation) -> Self {
        let root = location.rsdp.root_address();
        Self {
            rsdp_address: location.physical_address,
            revision: location.rsdp.revision,
            root_address: root.physical_address,
            uses_xsdt: root.kind == RootTableKind::Xsdt,
        }
    }
}

/// Validate an RSDP supplied by a structured boot protocol such as Multiboot2.
pub fn inspect_acpi_rsdp(bytes: &[u8], physical_address: u64) -> Option<AcpiRootReport> {
    Some(
        RsdpLocation {
            physical_address,
            rsdp: Rsdp::parse(bytes).ok()?,
        }
        .into(),
    )
}

/// Find the RSDP in the ACPI-defined EBDA/BIOS windows of an IA-PC boot.
pub fn discover_acpi_root() -> Option<AcpiRootReport> {
    #[cfg(target_os = "none")]
    {
        // The Multiboot trampoline identity-maps the low GiB, so both firmware windows are readable
        // here. Keep those mappings and the unsafe slice construction inside the x86 HAL. — KESTREL
        unsafe {
            let segment = core::ptr::read_volatile(EBDA_SEGMENT_POINTER as *const u16);
            let ebda = u64::from(segment) << 4;
            if ebda >= EBDA_MIN
                && ebda
                    .checked_add(EBDA_SCAN_LEN as u64)
                    .is_some_and(|end| end <= CONVENTIONAL_MEMORY_END)
            {
                let bytes = core::slice::from_raw_parts(ebda as *const u8, EBDA_SCAN_LEN);
                if let Some(location) = find_rsdp(bytes, ebda) {
                    return Some(location.into());
                }
            }

            let bytes = core::slice::from_raw_parts(BIOS_ROM_START as *const u8, BIOS_ROM_LEN);
            find_rsdp(bytes, BIOS_ROM_START).map(Into::into)
        }
    }
    #[cfg(not(target_os = "none"))]
    {
        None
    }
}

/// Validate the selected root table and decode its first checksum-valid MADT. `mapped_top`
/// is the exclusive end of the permanent physical map installed before ACPI discovery.
pub fn discover_acpi_madt(root: AcpiRootReport, mapped_top: u64) -> Option<AcpiMadtReport> {
    #[cfg(target_os = "none")]
    unsafe {
        let kind = if root.uses_xsdt {
            RootTableKind::Xsdt
        } else {
            RootTableKind::Rsdt
        };
        let root_bytes = mapped_sdt(root.root_address, mapped_top)?;
        let root_table = RootTable::parse(root_bytes, kind).ok()?;
        for address in root_table.entries() {
            let Some(bytes) = mapped_sdt(address, mapped_top) else {
                continue;
            };
            if !bytes.starts_with(b"APIC") {
                continue;
            }
            if let Some(report) = summarize_madt(address, bytes) {
                return Some(report);
            }
        }
        None
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (root, mapped_top);
        None
    }
}

#[cfg(any(target_os = "none", test))]
fn summarize_madt(address: u64, bytes: &[u8]) -> Option<AcpiMadtReport> {
    let madt = Madt::parse(bytes).ok()?;
    let mut io_apic_count = 0usize;
    let mut first_io_apic_address = None;
    let mut first_io_apic_gsi_base = None;
    let mut source_override_count = 0usize;
    for entry in madt.entries() {
        match entry {
            MadtEntry::IoApic(io_apic) => {
                io_apic_count += 1;
                if first_io_apic_address.is_none() {
                    first_io_apic_address = Some(io_apic.address);
                    first_io_apic_gsi_base = Some(io_apic.global_system_interrupt_base);
                }
            }
            MadtEntry::InterruptSourceOverride(_) => source_override_count += 1,
            MadtEntry::Other { .. } => {}
        }
    }
    let legacy_timer_route = madt.legacy_irq_route(0).ok().and_then(|route| {
        let mut candidate = None;
        for entry in madt.entries() {
            let MadtEntry::IoApic(io_apic) = entry else {
                continue;
            };
            if io_apic.global_system_interrupt_base <= route.global_system_interrupt
                && candidate.is_none_or(|current: niji_loader::acpi::madt::IoApic| {
                    io_apic.global_system_interrupt_base > current.global_system_interrupt_base
                })
            {
                candidate = Some(io_apic);
            }
        }
        let candidate = candidate?;
        Some(AcpiLegacyIrqRoute {
            isa_irq: route.isa_irq,
            global_system_interrupt: route.global_system_interrupt,
            active_low: route.polarity == InterruptPolarity::ActiveLow,
            level_triggered: route.trigger_mode == InterruptTriggerMode::Level,
            overridden: route.overridden,
            candidate_io_apic_id: candidate.id,
            candidate_io_apic_address: candidate.address,
            candidate_io_apic_gsi_base: candidate.global_system_interrupt_base,
        })
    });
    Some(AcpiMadtReport {
        address,
        local_interrupt_controller_address: madt.local_interrupt_controller_address(),
        pcat_compatible: madt.pcat_compatible(),
        io_apic_count,
        first_io_apic_address,
        first_io_apic_gsi_base,
        source_override_count,
        legacy_timer_route,
    })
}

#[cfg(target_os = "none")]
unsafe fn mapped_sdt(address: u64, mapped_top: u64) -> Option<&'static [u8]> {
    if !sdt_range_is_mapped(address, SDT_HEADER_LEN as u64, mapped_top) {
        return None;
    }
    let address = usize::try_from(address).ok()?;
    // Firmware owns the root-table pointers. Validate their fixed header before extending the
    // slice to its declared length inside the permanent kernel physical map. — KESTREL
    let header_bytes = unsafe { core::slice::from_raw_parts(address as *const u8, SDT_HEADER_LEN) };
    let header = SdtHeader::parse(header_bytes).ok()?;
    if !sdt_range_is_mapped(address as u64, header.length as u64, mapped_top) {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(address as *const u8, header.length) })
}

#[cfg(any(target_os = "none", test))]
fn sdt_range_is_mapped(address: u64, len: u64, mapped_top: u64) -> bool {
    address != 0
        && len != 0
        && mapped_top <= KERNEL_PHYS_MAP_END
        && address
            .checked_add(len)
            .is_some_and(|end| end <= mapped_top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_preserves_selected_root_kind() {
        let report = AcpiRootReport::from(RsdpLocation {
            physical_address: 0xe0010,
            rsdp: niji_loader::acpi::Rsdp {
                revision: 2,
                length: 36,
                rsdt_address: 0x1234_0000,
                xsdt_address: Some(0x1234_5678_0000),
            },
        });
        assert_eq!(report.rsdp_address, 0xe0010);
        assert_eq!(report.root_address, 0x1234_5678_0000);
        assert!(report.uses_xsdt);
    }

    #[test]
    fn permanent_physmap_admits_acpi_above_bootstrap_gib() {
        const FOUR_GIB: u64 = 4 << 30;
        assert!(sdt_range_is_mapped(0x43fe_2328, 128, FOUR_GIB));
        assert!(!sdt_range_is_mapped(0x43fe_2328, 128, 1 << 30));
        assert!(sdt_range_is_mapped(FOUR_GIB - 128, 128, FOUR_GIB));
        assert!(!sdt_range_is_mapped(0, 128, FOUR_GIB));
        assert!(!sdt_range_is_mapped(FOUR_GIB - 127, 128, FOUR_GIB));
        assert!(!sdt_range_is_mapped(0x1000, 128, KERNEL_PHYS_MAP_END + 1));
    }

    #[test]
    fn madt_summary_uses_the_existing_record_decoder() {
        const LEN: usize = niji_loader::acpi::madt::MADT_HEADER_LEN + 34;
        let mut bytes = [0u8; LEN];
        bytes[..4].copy_from_slice(b"APIC");
        bytes[4..8].copy_from_slice(&(LEN as u32).to_le_bytes());
        bytes[8] = 5;
        bytes[36..40].copy_from_slice(&0xfee0_0000u32.to_le_bytes());
        bytes[40..44].copy_from_slice(&1u32.to_le_bytes());
        bytes[44..56].copy_from_slice(&[1, 12, 7, 0, 0x00, 0x00, 0xc0, 0xfe, 0, 0, 0, 0]);
        bytes[56..68].copy_from_slice(&[1, 12, 8, 0, 0x00, 0x10, 0xc0, 0xfe, 24, 0, 0, 0]);
        bytes[68..].copy_from_slice(&[2, 10, 0, 0, 26, 0, 0, 0, 0, 0]);
        let sum = bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
        bytes[9] = 0u8.wrapping_sub(sum);

        assert_eq!(
            summarize_madt(0x7fe2_1000, &bytes),
            Some(AcpiMadtReport {
                address: 0x7fe2_1000,
                local_interrupt_controller_address: 0xfee0_0000,
                pcat_compatible: true,
                io_apic_count: 2,
                first_io_apic_address: Some(0xfec0_0000),
                first_io_apic_gsi_base: Some(0),
                source_override_count: 1,
                legacy_timer_route: Some(AcpiLegacyIrqRoute {
                    isa_irq: 0,
                    global_system_interrupt: 26,
                    active_low: false,
                    level_triggered: false,
                    overridden: true,
                    candidate_io_apic_id: 8,
                    candidate_io_apic_address: 0xfec0_1000,
                    candidate_io_apic_gsi_base: 24,
                }),
            })
        );
    }
}
