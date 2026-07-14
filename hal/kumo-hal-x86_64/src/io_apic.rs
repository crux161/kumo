//j447
//j448

//! Read-only inspection of the bootstrap I/O APIC.

use crate::AcpiLegacyIrqRoute;

const IOREGSEL_OFFSET: usize = 0x00;
const IOWIN_OFFSET: usize = 0x10;
const IOAPIC_ID_REGISTER: u32 = 0x00;
const IOAPIC_VERSION_REGISTER: u32 = 0x01;
const IOAPIC_REDIRECTION_BASE: u32 = 0x10;
const IOAPIC_LAST_INDIRECT_REGISTER: u32 = 0xff;

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
    pub routed_entry: Option<IoApicRedirectionEntry>,
}

impl IoApicReport {
    pub const fn id_matches_madt(self) -> bool {
        self.madt_id == self.hardware_id
    }

    pub const fn contains_routed_gsi(self) -> bool {
        self.routed_gsi >= self.gsi_base && self.routed_gsi <= self.gsi_end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApicRedirectionEntry {
    pub input_pin: u8,
    pub vector: u8,
    pub delivery_mode: u8,
    pub logical_destination: bool,
    pub delivery_pending: bool,
    pub active_low: bool,
    pub remote_irr: bool,
    pub level_triggered: bool,
    pub masked: bool,
    pub destination: u8,
}

impl IoApicRedirectionEntry {
    pub const fn delivery_mode_name(self) -> &'static str {
        match self.delivery_mode {
            0 => "fixed",
            1 => "lowest",
            2 => "SMI",
            4 => "NMI",
            5 => "INIT",
            7 => "ExtINT",
            _ => "reserved",
        }
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
        routed_entry: None,
    })
}

fn redirection_registers(report: IoApicReport) -> Option<(u8, u32, u32)> {
    let input_pin = report.routed_gsi.checked_sub(report.gsi_base)?;
    if input_pin >= u32::from(report.redirection_entries) {
        return None;
    }
    let low = IOAPIC_REDIRECTION_BASE.checked_add(input_pin.checked_mul(2)?)?;
    let high = low.checked_add(1)?;
    if high > IOAPIC_LAST_INDIRECT_REGISTER {
        return None;
    }
    Some((input_pin as u8, low, high))
}

fn decode_redirection_entry(input_pin: u8, low: u32, high: u32) -> IoApicRedirectionEntry {
    IoApicRedirectionEntry {
        input_pin,
        vector: low as u8,
        delivery_mode: ((low >> 8) & 0x07) as u8,
        logical_destination: low & (1 << 11) != 0,
        delivery_pending: low & (1 << 12) != 0,
        active_low: low & (1 << 13) != 0,
        remote_irr: low & (1 << 14) != 0,
        level_triggered: low & (1 << 15) != 0,
        masked: low & (1 << 16) != 0,
        destination: (high >> 24) as u8,
    }
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
        let mut report = decode_registers(route, id, version)?;
        if let Some((input_pin, low_register, high_register)) = redirection_registers(report) {
            let low = unsafe { read_indirect(address as usize, low_register) };
            let high = unsafe { read_indirect(address as usize, high_register) };
            report.routed_entry = Some(decode_redirection_entry(input_pin, low, high));
        }
        Some(report)
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

    #[test]
    fn redirection_entry_decodes_both_indirect_dwords() {
        let low = 0x0001_0000 | 0x0000_8000 | 0x0000_2000 | 0x0000_1000 | 0x0000_0100 | 0x45;
        let entry = decode_redirection_entry(2, low, 0x7a00_0000);
        assert_eq!(entry.input_pin, 2);
        assert_eq!(entry.vector, 0x45);
        assert_eq!(entry.delivery_mode, 1);
        assert_eq!(entry.delivery_mode_name(), "lowest");
        assert!(entry.delivery_pending);
        assert!(entry.active_low);
        assert!(entry.level_triggered);
        assert!(entry.masked);
        assert!(!entry.logical_destination);
        assert!(!entry.remote_irr);
        assert_eq!(entry.destination, 0x7a);
    }

    #[test]
    fn redirection_register_indices_are_bounded_to_the_selector() {
        let mut report = decode_registers(route(0, 119), 0, 0x0077_0020).unwrap();
        assert_eq!(redirection_registers(report), Some((119, 0xfe, 0xff)));

        report.routed_gsi = 120;
        report.redirection_entries = 121;
        assert_eq!(redirection_registers(report), None);
    }
}
