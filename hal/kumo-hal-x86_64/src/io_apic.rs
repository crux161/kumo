//j448
//j449
//j450
//j451
//j452

//! Read-only inspection of the bootstrap I/O APIC, plus applying the masked first-light timer
//! redirection entry (j451: write the two dwords, read both back; the entry stays masked and the
//! PIC heartbeat is untouched — no interrupt is delivered) and, once the PIC's IRQ0 is masked,
//! unmasking that entry so the timer is delivered through the I/O APIC to vector 0x31 (j452).

use crate::AcpiLegacyIrqRoute;
use core::sync::atomic::{AtomicU64, Ordering};

const IOREGSEL_OFFSET: usize = 0x00;
const IOWIN_OFFSET: usize = 0x10;
const IOAPIC_ID_REGISTER: u32 = 0x00;
const IOAPIC_VERSION_REGISTER: u32 = 0x01;
const IOAPIC_REDIRECTION_BASE: u32 = 0x10;
const IOAPIC_LAST_INDIRECT_REGISTER: u32 = 0xff;
const IOAPIC_MINIMUM_VECTOR: u8 = 0x10;
const IOAPIC_MAXIMUM_VECTOR: u8 = 0xfe;
const IOAPIC_POLARITY_ACTIVE_LOW: u32 = 1 << 13;
const IOAPIC_TRIGGER_LEVEL: u32 = 1 << 15;
const IOAPIC_MASKED: u32 = 1 << 16;

pub(crate) const TIMER_VECTOR: u8 = 0x31;
const BOOT_DESTINATION: u8 = 0;

static TIMER_INTERRUPTS: AtomicU64 = AtomicU64::new(0);

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApicTimerPlan {
    pub gsi: u32,
    pub low_register: u8,
    pub high_register: u8,
    pub low_dword: u32,
    pub high_dword: u32,
    pub entry: IoApicRedirectionEntry,
}

impl IoApicTimerPlan {
    /// The indirect writes that apply this plan, **destination (high) before vector (low)**. The
    /// entry stays masked throughout because `low_dword` already carries the mask bit.
    pub const fn write_ops(self) -> [(u8, u32); 2] {
        [
            (self.high_register, self.high_dword),
            (self.low_register, self.low_dword),
        ]
    }
}

/// The result of applying [`IoApicTimerPlan`]: the plan plus what the two dwords read back as.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApicTimerApplied {
    pub plan: IoApicTimerPlan,
    pub low_readback: u32,
    pub high_readback: u32,
}

impl IoApicTimerApplied {
    /// Both dwords read back exactly as planned (a masked, idle entry has no read-only status bits
    /// set, so an exact compare is the honest check).
    pub const fn matches_plan(self) -> bool {
        self.low_readback == self.plan.low_dword && self.high_readback == self.plan.high_dword
    }

    /// The mask bit survived the write — this slice must never unmask the route.
    pub const fn stays_masked(self) -> bool {
        self.low_readback & IOAPIC_MASKED != 0
    }

    /// Decode what the controller actually holds now, from the read-back dwords.
    pub fn readback_entry(self) -> IoApicRedirectionEntry {
        decode_redirection_entry(
            self.plan.entry.input_pin,
            self.low_readback,
            self.high_readback,
        )
    }

    /// The route read back **unmasked** with the timer vector intact — i.e. it is now live and the
    /// controller will deliver the timer to [`TIMER_VECTOR`].
    pub fn is_live_timer(self) -> bool {
        !self.stays_masked() && self.readback_entry().vector == TIMER_VECTOR
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

fn encode_masked_fixed_physical(
    vector: u8,
    destination: u8,
    active_low: bool,
    level_triggered: bool,
) -> Option<(u32, u32)> {
    if !(IOAPIC_MINIMUM_VECTOR..=IOAPIC_MAXIMUM_VECTOR).contains(&vector) {
        return None;
    }
    let mut low = u32::from(vector) | IOAPIC_MASKED;
    if active_low {
        low |= IOAPIC_POLARITY_ACTIVE_LOW;
    }
    if level_triggered {
        low |= IOAPIC_TRIGGER_LEVEL;
    }
    Some((low, u32::from(destination) << 24))
}

/// Build, but do not apply, the masked first-light route for the legacy timer.
pub fn plan_boot_io_apic_timer(route: AcpiLegacyIrqRoute) -> Option<IoApicTimerPlan> {
    let input_pin = route
        .global_system_interrupt
        .checked_sub(route.candidate_io_apic_gsi_base)?;
    let low_register = IOAPIC_REDIRECTION_BASE.checked_add(input_pin.checked_mul(2)?)?;
    let high_register = low_register.checked_add(1)?;
    if high_register > IOAPIC_LAST_INDIRECT_REGISTER {
        return None;
    }
    let input_pin = u8::try_from(input_pin).ok()?;
    let (low_dword, high_dword) = encode_masked_fixed_physical(
        TIMER_VECTOR,
        BOOT_DESTINATION,
        route.active_low,
        route.level_triggered,
    )?;
    Some(IoApicTimerPlan {
        gsi: route.global_system_interrupt,
        low_register: low_register as u8,
        high_register: high_register as u8,
        low_dword,
        high_dword,
        entry: decode_redirection_entry(input_pin, low_dword, high_dword),
    })
}

/// Apply the masked first-light timer route: write the destination (high) dword then the vector
/// (low) dword, then read both back. The entry stays **masked** (the mask bit is part of
/// `low_dword`) so no interrupt is delivered, and only IOREGSEL/IOWIN are touched — the PIC and
/// its heartbeat are left exactly as they were. Returns the plan plus the read-back dwords.
pub fn apply_boot_io_apic_timer(route: AcpiLegacyIrqRoute) -> Option<IoApicTimerApplied> {
    let plan = plan_boot_io_apic_timer(route)?;
    #[cfg(target_os = "none")]
    {
        let address = route.candidate_io_apic_address;
        let end = address.checked_add(IOWIN_OFFSET as u32 + 4)?;
        if address < BOOT_IOAPIC_WINDOW
            || end > BOOT_IOAPIC_WINDOW.checked_add(BOOT_IOAPIC_WINDOW_LEN)?
        {
            return None;
        }

        // Firmware-described address inside the bootstrap's identity-mapped 2 MiB window. The entry
        // is programmed masked (high/destination first, then low/vector), so this delivers nothing;
        // it only stages the route for a later unmask + source-transition slice. — CORVUS
        let base = address as usize;
        for (register, value) in plan.write_ops() {
            unsafe { write_indirect(base, u32::from(register), value) };
        }
        let low_readback = unsafe { read_indirect(base, u32::from(plan.low_register)) };
        let high_readback = unsafe { read_indirect(base, u32::from(plan.high_register)) };
        Some(IoApicTimerApplied {
            plan,
            low_readback,
            high_readback,
        })
    }
    #[cfg(not(target_os = "none"))]
    {
        // No MMIO off metal; host tests construct `IoApicTimerApplied` directly.
        None
    }
}

/// Unmask the already-written timer redirection entry so the controller delivers the timer to
/// [`TIMER_VECTOR`]. Rewrites only the low dword with the mask bit cleared (the high/destination
/// dword was set by [`apply_boot_io_apic_timer`]), then reads both back.
///
/// After this the route is **live** — the caller MUST have masked the PIC's IRQ0 first, or the
/// timer would be delivered on both the legacy and I/O APIC paths at once.
pub fn unmask_boot_io_apic_timer(route: AcpiLegacyIrqRoute) -> Option<IoApicTimerApplied> {
    let plan = plan_boot_io_apic_timer(route)?;
    #[cfg(target_os = "none")]
    {
        let address = route.candidate_io_apic_address;
        let end = address.checked_add(IOWIN_OFFSET as u32 + 4)?;
        if address < BOOT_IOAPIC_WINDOW
            || end > BOOT_IOAPIC_WINDOW.checked_add(BOOT_IOAPIC_WINDOW_LEN)?
        {
            return None;
        }
        let base = address as usize;
        let unmasked_low = plan.low_dword & !IOAPIC_MASKED;
        unsafe { write_indirect(base, u32::from(plan.low_register), unmasked_low) };
        let low_readback = unsafe { read_indirect(base, u32::from(plan.low_register)) };
        let high_readback = unsafe { read_indirect(base, u32::from(plan.high_register)) };
        Some(IoApicTimerApplied {
            plan,
            low_readback,
            high_readback,
        })
    }
    #[cfg(not(target_os = "none"))]
    {
        None
    }
}

/// Wait (bounded) for the timer vector's delivery count to reach `start + needed`. `max_wakes`
/// caps the `hlt` loop so a route that never delivers cannot hang forever — the still-running
/// local-APIC timer provides the wake beat. Returns the deliveries observed.
#[cfg(target_os = "none")]
pub(crate) fn wait_for_timer_interrupts(start: u64, needed: u64, max_wakes: u64) -> u64 {
    let mut wakes = 0u64;
    loop {
        let seen = timer_interrupt_count().saturating_sub(start);
        if seen >= needed || wakes >= max_wakes {
            return seen;
        }
        wakes += 1;
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
}

#[cfg(any(target_os = "none", test))]
fn dispatch_timer(vector: u8, acknowledge: impl FnOnce()) -> bool {
    if vector != TIMER_VECTOR {
        return false;
    }
    TIMER_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    acknowledge();
    true
}

#[cfg(target_os = "none")]
pub(crate) fn handle(vector: u8) -> bool {
    dispatch_timer(vector, crate::local_apic::acknowledge_external)
}

pub(crate) fn timer_interrupt_count() -> u64 {
    TIMER_INTERRUPTS.load(Ordering::Relaxed)
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

#[cfg(target_os = "none")]
unsafe fn write_indirect(base: usize, register: u32, value: u32) {
    unsafe {
        core::ptr::write_volatile((base + IOREGSEL_OFFSET) as *mut u32, register);
        core::ptr::write_volatile((base + IOWIN_OFFSET) as *mut u32, value);
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

    #[test]
    fn timer_plan_uses_a_free_installed_vector_and_stays_masked() {
        assert!((TIMER_VECTOR as usize) < crate::idt::IDT_VECTORS);
        assert!(
            TIMER_VECTOR
                >= crate::legacy_irq::INTERRUPT_VECTOR_BASE
                    + crate::legacy_irq::LEGACY_INTERRUPT_VECTORS
        );
        assert_ne!(TIMER_VECTOR, crate::local_apic::TIMER_VECTOR);
        assert_ne!(TIMER_VECTOR, crate::local_apic::SPURIOUS_VECTOR);

        let plan = plan_boot_io_apic_timer(route(0, 2)).unwrap();
        assert_eq!((plan.low_register, plan.high_register), (0x14, 0x15));
        assert_eq!((plan.low_dword, plan.high_dword), (0x0001_0031, 0));
        assert_eq!(plan.entry.input_pin, 2);
        assert_eq!(plan.entry.vector, TIMER_VECTOR);
        assert_eq!(plan.entry.delivery_mode_name(), "fixed");
        assert!(!plan.entry.logical_destination);
        assert_eq!(plan.entry.destination, 0);
        assert!(!plan.entry.active_low);
        assert!(!plan.entry.level_triggered);
        assert!(plan.entry.masked);
    }

    #[test]
    fn timer_plan_carries_acpi_polarity_and_trigger() {
        let mut routed = route(24, 26);
        routed.active_low = true;
        routed.level_triggered = true;
        let plan = plan_boot_io_apic_timer(routed).unwrap();
        assert_eq!(plan.low_dword, 0x0001_a031);
        assert!(plan.entry.active_low);
        assert!(plan.entry.level_triggered);
    }

    #[test]
    fn write_ops_program_destination_before_vector_and_stay_masked() {
        let plan = plan_boot_io_apic_timer(route(0, 2)).unwrap();
        // Destination (high, 0x15) first, then vector (low, 0x14).
        assert_eq!(plan.write_ops(), [(0x15, 0x0000_0000), (0x14, 0x0001_0031)]);
        // The low write carries the mask bit, so applying it delivers nothing.
        assert_ne!(plan.write_ops()[1].1 & IOAPIC_MASKED, 0);
    }

    #[test]
    fn applied_accepts_an_exact_masked_readback() {
        let plan = plan_boot_io_apic_timer(route(0, 2)).unwrap();
        let applied = IoApicTimerApplied {
            plan,
            low_readback: plan.low_dword,
            high_readback: plan.high_dword,
        };
        assert!(applied.matches_plan());
        assert!(applied.stays_masked());
        let entry = applied.readback_entry();
        assert_eq!(entry.vector, TIMER_VECTOR);
        assert_eq!(entry.input_pin, 2);
        assert!(entry.masked);
        assert_eq!(entry.delivery_mode_name(), "fixed");
    }

    #[test]
    fn applied_rejects_a_readback_that_differs_from_the_plan() {
        let plan = plan_boot_io_apic_timer(route(0, 2)).unwrap();
        // A single flipped vector bit must fail the exact compare.
        let applied = IoApicTimerApplied {
            plan,
            low_readback: plan.low_dword ^ 1,
            high_readback: plan.high_dword,
        };
        assert!(!applied.matches_plan());
        assert!(applied.stays_masked());
    }

    #[test]
    fn applied_detects_an_unmasked_readback() {
        let plan = plan_boot_io_apic_timer(route(0, 2)).unwrap();
        let applied = IoApicTimerApplied {
            plan,
            low_readback: plan.low_dword & !IOAPIC_MASKED,
            high_readback: plan.high_dword,
        };
        assert!(!applied.stays_masked());
    }

    #[test]
    fn live_timer_requires_unmasked_and_the_timer_vector() {
        let plan = plan_boot_io_apic_timer(route(0, 2)).unwrap();
        // Unmasked with the timer vector -> live.
        let live = IoApicTimerApplied {
            plan,
            low_readback: plan.low_dword & !IOAPIC_MASKED,
            high_readback: plan.high_dword,
        };
        assert!(live.is_live_timer());
        assert_eq!(live.readback_entry().vector, TIMER_VECTOR);
        // Still masked -> not live.
        let masked = IoApicTimerApplied {
            plan,
            low_readback: plan.low_dword,
            high_readback: plan.high_dword,
        };
        assert!(!masked.is_live_timer());
        // Unmasked but wrong vector -> not live.
        let wrong_vector = IoApicTimerApplied {
            plan,
            low_readback: (plan.low_dword & !IOAPIC_MASKED & !0xff) | 0x40,
            high_readback: plan.high_dword,
        };
        assert!(!wrong_vector.is_live_timer());
    }

    #[test]
    fn timer_dispatch_counts_only_its_vector_and_acknowledges_once() {
        let before = timer_interrupt_count();
        let mut acknowledgements = 0;

        assert!(!dispatch_timer(TIMER_VECTOR + 1, || acknowledgements += 1));
        assert_eq!(timer_interrupt_count(), before);
        assert_eq!(acknowledgements, 0);

        assert!(dispatch_timer(TIMER_VECTOR, || acknowledgements += 1));
        assert_eq!(timer_interrupt_count().wrapping_sub(before), 1);
        assert_eq!(acknowledgements, 1);
    }
}
