//j444

//! Legacy IA-PC ACPI discovery windows.

#[cfg(target_os = "none")]
use niji_loader::acpi::find_rsdp;
use niji_loader::acpi::{RootTableKind, RsdpLocation};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcpiRootReport {
    pub rsdp_address: u64,
    pub revision: u8,
    pub root_address: u64,
    pub uses_xsdt: bool,
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
}
