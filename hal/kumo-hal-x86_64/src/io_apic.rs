//j447

//! Read-only inspection of the bootstrap I/O APIC.

use crate::AcpiLegacyIrqRoute;

const IOREGSEL_OFFSET: usize = 0x00;
const IOWIN_OFFSET: usize = 0x10;
const IOAPIC_ID_REGISTER: u32 = 0x00;
const IOAPIC_VERSION_REGISTER: u32 = 0x01;

/// The one non-RAM window mapped by the Multiboot bootstrap page tables.
const BOOT_IOAPIC_WINDOW: u32 = 0xfec0_0000;
const BOOT_IOAPIC_WINDOW_LEN: u32 = 0x20_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApicReport {
    pub madt_id: u8,
    pub hardware_id: u8,
    pub version: u8,
    pub redirection_entries: u16,
    pub gsi_base: u32,
    pub gsi_end: u32,
    pub routed_gsi: u32,
}

impl IoApicReport {
    pub const fn id_matches_madt(self) -> bool {
        self.madt_id == self.hardware_id
    }

    pub const fn contains_routed_gsi(self) -> bool {
        self.routed_gsi >= self.gsi_base && self.routed_gsi <= self.gsi_end
    }
}

fn decode_registers(
    route: AcpiLegacyIrqRoute,
    id_register: u32,
    version_register: u32,
) -> Option<IoApicReport> {
    let maximum_redirection_entry = (version_register >> 16) & 0xff;
    Some(IoApicReport {
        madt_id: route.candidate_io_apic_id,
        hardware_id: ((id_register >> 24) & 0x0f) as u8,
        version: (version_register & 0xff) as u8,
        redirection_entries: (maximum_redirection_entry + 1) as u16,
        gsi_base: route.candidate_io_apic_gsi_base,
        gsi_end: route
            .candidate_io_apic_gsi_base
            .checked_add(maximum_redirection_entry)?,
        routed_gsi: route.global_system_interrupt,
    })
}

/// Read the ID and version registers without modifying any redirection entry.
pub fn inspect_boot_io_apic(route: AcpiLegacyIrqRoute) -> Option<IoApicReport> {
    #[cfg(target_os = "none")]
    {
        let address = route.candidate_io_apic_address;
        let end = address.checked_add(IOWIN_OFFSET as u32 + 4)?;
        if address < BOOT_IOAPIC_WINDOW
            || end > BOOT_IOAPIC_WINDOW.checked_add(BOOT_IOAPIC_WINDOW_LEN)?
        {
            return None;
        }

        // The address is firmware-described, and the x86 bootstrap owns the exact identity-mapped
        // 2 MiB window containing it. Only IOREGSEL is written; IOWIN is read. — KESTREL
        let id = unsafe { read_indirect(address as usize, IOAPIC_ID_REGISTER) };
        let version = unsafe { read_indirect(address as usize, IOAPIC_VERSION_REGISTER) };
        decode_registers(route, id, version)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = route;
        None
    }
}

#[cfg(target_os = "none")]
unsafe fn read_indirect(base: usize, register: u32) -> u32 {
    unsafe {
        core::ptr::write_volatile((base + IOREGSEL_OFFSET) as *mut u32, register);
        core::ptr::read_volatile((base + IOWIN_OFFSET) as *const u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(gsi_base: u32, routed_gsi: u32) -> AcpiLegacyIrqRoute {
        AcpiLegacyIrqRoute {
            isa_irq: 0,
            global_system_interrupt: routed_gsi,
            active_low: false,
            level_triggered: false,
            overridden: true,
            candidate_io_apic_id: 3,
            candidate_io_apic_address: BOOT_IOAPIC_WINDOW,
            candidate_io_apic_gsi_base: gsi_base,
        }
    }

    #[test]
    fn version_register_defines_the_inclusive_gsi_range() {
        let report = decode_registers(route(24, 26), 3 << 24, 0x0017_0020).unwrap();
        assert_eq!(report.hardware_id, 3);
        assert_eq!(report.version, 0x20);
        assert_eq!(report.redirection_entries, 24);
        assert_eq!((report.gsi_base, report.gsi_end), (24, 47));
        assert!(report.id_matches_madt());
        assert!(report.contains_routed_gsi());
    }

    #[test]
    fn report_exposes_id_mismatch_and_route_outside_range() {
        let report = decode_registers(route(24, 48), 2 << 24, 0x0017_0011).unwrap();
        assert!(!report.id_matches_madt());
        assert!(!report.contains_routed_gsi());
    }

    #[test]
    fn overflowing_gsi_range_is_rejected() {
        assert_eq!(
            decode_registers(route(u32::MAX, u32::MAX), 0, 0x0001_0011),
            None
        );
    }
}
