//j907

//! A PCI 16550 console mirror: Intel AMT Serial-over-LAN on the NUC.
//!
//! Intel's Management Engine exposes its Serial-over-LAN redirection to the host as the "KT"
//! (keyboard and text) function at PCI 00:16.3: an ordinary 16550-compatible UART (class 07h,
//! subclass 00h, programming interface 02h) behind an I/O BAR, not the legacy COM1 at 0x3F8.
//! Whatever the host writes there reaches a SOL client (MeshCommander's terminal, `amtterm`)
//! over the network, so a machine with no serial header still gives a text console. On a
//! machine without AMT the scan finds nothing and the console stays COM1 + glass. QEMU's
//! `-device pci-serial` presents the same class and BAR shape, which is how the smoke proves it.
//!
//! Discovery uses PCI configuration mechanism #1 (ports 0xCF8/0xCFC) on bus 0 only: the KT
//! function is always a root-complex device, and bus 0 needs no bridge walk.

/// What the scan found and how it was made usable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciSerialReport {
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    /// The UART's I/O port base (BAR0 with the space bit cleared).
    pub io_base: u16,
    /// The PCI command register before and after I/O decode was switched on.
    pub command_before: u16,
    pub command_after: u16,
    /// 00:16.3 from Intel: the ME's KT function, i.e. AMT Serial-over-LAN.
    pub amt_sol: bool,
}

/// Class code, subclass and programming interface of a 16550-compatible serial controller.
const CLASS_SERIAL_16550: u32 = 0x07_00_02;
const INTEL: u16 = 0x8086;
/// The ME's KT (keyboard and text redirection) function on every PCH since ME 6.
const KT_DEVICE: u8 = 0x16;
const KT_FUNCTION: u8 = 3;
const COMMAND_IO_SPACE: u16 = 1 << 0;

/// The CONFIG_ADDRESS word for bus 0 `device`.`function`, dword-aligned `offset`.
pub(crate) const fn config_address(device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((device as u32 & 0x1f) << 11)
        | ((function as u32 & 0x07) << 8)
        | (offset as u32 & 0xfc)
}

/// Whether a function's class register (offset 0x08) and BAR0 describe a usable 16550 in I/O
/// space; returns the port base. A zero base means firmware never assigned it.
pub(crate) fn serial_io_base(class_revision: u32, bar0: u32) -> Option<u16> {
    if class_revision >> 8 != CLASS_SERIAL_16550 || bar0 & 1 == 0 {
        return None;
    }
    let base = bar0 & 0xffff_fffc;
    (base != 0 && base <= 0xfff8).then_some(base as u16)
}

/// The preferred candidate: Intel's KT at 00:16.3 wins over any other PCI 16550 on bus 0.
pub(crate) fn prefer(
    current: Option<PciSerialReport>,
    candidate: PciSerialReport,
) -> Option<PciSerialReport> {
    match current {
        Some(found) if found.amt_sol => Some(found),
        Some(_) if candidate.amt_sol => Some(candidate),
        Some(found) => Some(found),
        None => Some(candidate),
    }
}

#[cfg(target_os = "none")]
mod io {
    pub unsafe fn outl(port: u16, value: u32) {
        unsafe {
            core::arch::asm!("out dx, eax", in("dx") port, in("eax") value,
                options(nostack, nomem, preserves_flags));
        }
    }

    pub unsafe fn inl(port: u16) -> u32 {
        let value: u32;
        unsafe {
            core::arch::asm!("in eax, dx", out("eax") value, in("dx") port,
                options(nostack, nomem, preserves_flags));
        }
        value
    }
}

#[cfg(target_os = "none")]
fn config_read(device: u8, function: u8, offset: u8) -> u32 {
    unsafe {
        io::outl(0xcf8, config_address(device, function, offset));
        io::inl(0xcfc)
    }
}

#[cfg(target_os = "none")]
fn config_write(device: u8, function: u8, offset: u8, value: u32) {
    unsafe {
        io::outl(0xcf8, config_address(device, function, offset));
        io::outl(0xcfc, value);
    }
}

/// Find a PCI 16550 on bus 0 (preferring AMT SOL), switch its I/O decode on, and make it the
/// console mirror. Call once, early, on the boot CPU.
#[cfg(target_os = "none")]
pub fn discover_pci_serial_console() -> Option<PciSerialReport> {
    let mut found: Option<PciSerialReport> = None;
    for device in 0..32u8 {
        let id = config_read(device, 0, 0x00);
        if id & 0xffff == 0xffff {
            continue;
        }
        let multifunction = (config_read(device, 0, 0x0c) >> 16) & 0x80 != 0;
        let functions = if multifunction { 8 } else { 1 };
        for function in 0..functions {
            let id = config_read(device, function, 0x00);
            let vendor_id = (id & 0xffff) as u16;
            if vendor_id == 0xffff {
                continue;
            }
            let class_revision = config_read(device, function, 0x08);
            let bar0 = config_read(device, function, 0x10);
            let Some(io_base) = serial_io_base(class_revision, bar0) else {
                continue;
            };
            let command = (config_read(device, function, 0x04) & 0xffff) as u16;
            let candidate = PciSerialReport {
                device,
                function,
                vendor_id,
                device_id: (id >> 16) as u16,
                io_base,
                command_before: command,
                command_after: command,
                amt_sol: vendor_id == INTEL && device == KT_DEVICE && function == KT_FUNCTION,
            };
            found = prefer(found, candidate);
        }
    }
    let mut report = found?;
    if report.command_before & COMMAND_IO_SPACE == 0 {
        // Only the command half of the dword is written back; status bits are write-1-to-clear,
        // so they go back as zero.
        let value = (report.command_before | COMMAND_IO_SPACE) as u32;
        config_write(report.device, report.function, 0x04, value);
    }
    report.command_after = (config_read(report.device, report.function, 0x04) & 0xffff) as u16;
    if report.command_after & COMMAND_IO_SPACE == 0 {
        return None;
    }
    crate::serial::set_mirror(report.io_base);
    Some(report)
}

/// Host builds have no PCI bus.
#[cfg(not(target_os = "none"))]
pub fn discover_pci_serial_console() -> Option<PciSerialReport> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_addresses_follow_mechanism_one() {
        assert_eq!(config_address(0x16, 3, 0x00), 0x8000_b300);
        assert_eq!(config_address(0x16, 3, 0x10), 0x8000_b310);
        // Offsets are dword-aligned and fields are masked to their widths.
        assert_eq!(config_address(0x16, 3, 0x13), 0x8000_b310);
        assert_eq!(config_address(0x3f, 0xf, 0), config_address(0x1f, 7, 0));
    }

    #[test]
    fn only_an_assigned_io_bar_of_a_16550_qualifies() {
        // Sunrise Point KT: class 07 00 02, BAR0 = I/O 0xf0a0.
        assert_eq!(serial_io_base(0x0700_0221, 0xf0a1), Some(0xf0a0));
        // QEMU pci-serial: class 07 00 02 revision 01.
        assert_eq!(serial_io_base(0x0700_0201, 0xc001), Some(0xc000));
        // Memory BAR, unassigned BAR, other prog-if (8250 = 00, generic = 06), other class.
        assert_eq!(serial_io_base(0x0700_0200, 0xfe00_0000), None);
        assert_eq!(serial_io_base(0x0700_0200, 0x1), None);
        assert_eq!(serial_io_base(0x0700_0000, 0xf0a1), None);
        assert_eq!(serial_io_base(0x0700_0600, 0xf0a1), None);
        assert_eq!(serial_io_base(0x0c03_3000, 0xf0a1), None);
        assert_eq!(serial_io_base(0x0700_0200, 0x0001_0001), None);
    }

    #[test]
    fn amt_sol_wins_over_any_other_pci_uart() {
        let uart = |device, amt_sol| PciSerialReport {
            device,
            function: if amt_sol { 3 } else { 0 },
            vendor_id: if amt_sol { INTEL } else { 0x1b36 },
            device_id: 0x0002,
            io_base: 0xc000,
            command_before: 0,
            command_after: 0,
            amt_sol,
        };
        let other = uart(3, false);
        let sol = uart(KT_DEVICE, true);
        assert_eq!(prefer(None, other), Some(other));
        assert_eq!(prefer(Some(other), sol), Some(sol));
        assert_eq!(prefer(Some(sol), other), Some(sol));
        assert_eq!(prefer(Some(other), uart(5, false)), Some(other));
    }
}
