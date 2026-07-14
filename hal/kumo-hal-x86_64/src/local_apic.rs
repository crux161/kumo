//j439

#![cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]

//! x2APIC periodic-timer calibration against the legacy PIT reference clock.
//!
//! The AMD64 APM Volume 2, Chapter 16 defines the local-APIC timer as a decrementing 32-bit
//! counter controlled by the divide, local-vector-table, initial-count, and current-count
//! registers. Keep that register choreography pure over [`ApicIo`]; only the metal adapter uses
//! `cpuid`, `rdmsr`, and `wrmsr`.

use core::sync::atomic::{AtomicU64, Ordering};

pub(crate) const TIMER_VECTOR: u8 = 0x30;
pub(crate) const SPURIOUS_VECTOR: u8 = 0x3f;

const APIC_BASE_MSR: u32 = 0x1b;
const APIC_GLOBAL_ENABLE: u64 = 1 << 11;
const APIC_X2_MODE: u64 = 1 << 10;

const TASK_PRIORITY: u32 = 0x808;
const END_OF_INTERRUPT: u32 = 0x80b;
const SPURIOUS_INTERRUPT: u32 = 0x80f;
const TIMER_LVT: u32 = 0x832;
const TIMER_INITIAL_COUNT: u32 = 0x838;
const TIMER_CURRENT_COUNT: u32 = 0x839;
const TIMER_DIVIDE: u32 = 0x83e;

const SOFTWARE_ENABLE: u32 = 1 << 8;
const TIMER_MASKED: u32 = 1 << 16;
const TIMER_PERIODIC: u32 = 1 << 17;
const DIVIDE_BY_16: u32 = 0b011;
const CALIBRATION_START: u32 = u32::MAX;
const CALIBRATION_TICKS: u64 = 2;

static TIMER_INTERRUPTS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TimerSetup {
    pub counter_hz: u64,
    pub period_hz: u64,
    pub initial_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Unsupported,
    Calibration,
}

trait ApicIo {
    fn read(&mut self, register: u32) -> u32;
    fn write(&mut self, register: u32, value: u32);
}

fn prepare<IO: ApicIo>(io: &mut IO) {
    // The architectural order matters: divide and a masked LVT are established before a non-zero
    // initial count starts the timer (AMD64 APM Vol. 2 §16.4.1). — KESTREL 2026-07-14
    io.write(TASK_PRIORITY, 0);
    io.write(
        SPURIOUS_INTERRUPT,
        SOFTWARE_ENABLE | u32::from(SPURIOUS_VECTOR),
    );
    io.write(TIMER_DIVIDE, DIVIDE_BY_16);
    io.write(TIMER_LVT, TIMER_MASKED | u32::from(TIMER_VECTOR));
}

fn calibrated_setup(
    elapsed_counts: u32,
    reference_ticks: u64,
    reference_hz: u64,
    target_hz: u64,
) -> Option<TimerSetup> {
    if elapsed_counts == 0 || reference_ticks == 0 || reference_hz == 0 || target_hz == 0 {
        return None;
    }
    let counter_hz = u64::from(elapsed_counts)
        .checked_mul(reference_hz)?
        .checked_add(reference_ticks / 2)?
        / reference_ticks;
    let initial_count = counter_hz
        .checked_add(target_hz / 2)?
        .checked_div(target_hz)?;
    let initial_count = u32::try_from(initial_count).ok()?;
    if initial_count == 0 {
        return None;
    }
    Some(TimerSetup {
        counter_hz,
        period_hz: target_hz,
        initial_count,
    })
}

fn arm_periodic<IO: ApicIo>(io: &mut IO, setup: TimerSetup) {
    io.write(TIMER_LVT, TIMER_PERIODIC | u32::from(TIMER_VECTOR));
    io.write(TIMER_INITIAL_COUNT, setup.initial_count);
}

fn acknowledge<IO: ApicIo>(io: &mut IO) {
    io.write(END_OF_INTERRUPT, 0);
}

#[cfg(target_os = "none")]
struct HardwareMsrs;

#[cfg(target_os = "none")]
impl ApicIo for HardwareMsrs {
    fn read(&mut self, register: u32) -> u32 {
        unsafe { read_msr(register) as u32 }
    }

    fn write(&mut self, register: u32, value: u32) {
        unsafe { write_msr(register, u64::from(value)) }
    }
}

#[cfg(target_os = "none")]
unsafe fn read_msr(register: u32) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") register,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

#[cfg(target_os = "none")]
unsafe fn write_msr(register: u32, value: u64) {
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") register,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags)
        );
    }
}

#[cfg(target_os = "none")]
fn enable_x2apic() -> Result<(), Error> {
    let features = core::arch::x86_64::__cpuid(1);
    let has_apic = features.edx & (1 << 9) != 0;
    let has_x2apic = features.ecx & (1 << 21) != 0;
    if !has_apic || !has_x2apic {
        return Err(Error::Unsupported);
    }

    let mut base = unsafe { read_msr(APIC_BASE_MSR) };
    if base & APIC_GLOBAL_ENABLE == 0 {
        base |= APIC_GLOBAL_ENABLE;
        unsafe { write_msr(APIC_BASE_MSR, base) };
    }
    if base & APIC_X2_MODE == 0 {
        unsafe { write_msr(APIC_BASE_MSR, base | APIC_X2_MODE) };
    }
    Ok(())
}

#[cfg(target_os = "none")]
pub(crate) fn initialize(reference_hz: u64, target_hz: u64) -> Result<TimerSetup, Error> {
    enable_x2apic()?;

    let mut io = HardwareMsrs;
    prepare(&mut io);

    // Align the measurement to a PIT edge, then measure exactly two complete reference periods.
    // PIT remains the independent clock only for this calibration gate. — KESTREL 2026-07-14
    let align = crate::legacy_irq::count();
    crate::legacy_irq::wait(align, 1);
    io.write(TIMER_INITIAL_COUNT, CALIBRATION_START);
    let reference_start = crate::legacy_irq::count();
    crate::legacy_irq::wait(reference_start, CALIBRATION_TICKS);
    let current = io.read(TIMER_CURRENT_COUNT);
    io.write(TIMER_INITIAL_COUNT, 0);

    let elapsed = CALIBRATION_START.wrapping_sub(current);
    let setup = calibrated_setup(elapsed, CALIBRATION_TICKS, reference_hz, target_hz)
        .ok_or(Error::Calibration)?;
    TIMER_INTERRUPTS.store(0, Ordering::Relaxed);
    arm_periodic(&mut io, setup);
    Ok(setup)
}

#[cfg(target_os = "none")]
pub(crate) fn handle(vector: u8) -> bool {
    if vector == SPURIOUS_VECTOR {
        return true;
    }
    if vector != TIMER_VECTOR {
        return false;
    }
    TIMER_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    acknowledge(&mut HardwareMsrs);
    true
}

pub(crate) fn count() -> u64 {
    TIMER_INTERRUPTS.load(Ordering::Relaxed)
}

#[cfg(target_os = "none")]
pub(crate) fn wait(start: u64, needed: u64) -> u64 {
    loop {
        let seen = count().saturating_sub(start);
        if seen >= needed {
            return seen;
        }
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingIo {
        writes: [(u32, u32); 8],
        len: usize,
    }

    impl ApicIo for RecordingIo {
        fn read(&mut self, _register: u32) -> u32 {
            0
        }

        fn write(&mut self, register: u32, value: u32) {
            self.writes[self.len] = (register, value);
            self.len += 1;
        }
    }

    #[test]
    fn preparation_masks_timer_before_starting_it() {
        let mut io = RecordingIo::default();
        prepare(&mut io);
        assert_eq!(
            &io.writes[..io.len],
            &[
                (TASK_PRIORITY, 0),
                (
                    SPURIOUS_INTERRUPT,
                    SOFTWARE_ENABLE | u32::from(SPURIOUS_VECTOR)
                ),
                (TIMER_DIVIDE, DIVIDE_BY_16),
                (TIMER_LVT, TIMER_MASKED | u32::from(TIMER_VECTOR)),
            ]
        );
    }

    #[test]
    fn pit_measurement_becomes_periodic_initial_count() {
        let setup = calibrated_setup(5_000_000, 2, 20, 20).unwrap();
        assert_eq!(
            setup,
            TimerSetup {
                counter_hz: 50_000_000,
                period_hz: 20,
                initial_count: 2_500_000,
            }
        );

        let mut io = RecordingIo::default();
        arm_periodic(&mut io, setup);
        assert_eq!(
            &io.writes[..io.len],
            &[
                (TIMER_LVT, TIMER_PERIODIC | u32::from(TIMER_VECTOR)),
                (TIMER_INITIAL_COUNT, 2_500_000),
            ]
        );
        assert_eq!(calibrated_setup(0, 2, 20, 20), None);
        assert_eq!(calibrated_setup(1, 0, 20, 20), None);
        assert_eq!(calibrated_setup(1, 2, 0, 20), None);
        assert_eq!(calibrated_setup(1, 2, 20, 0), None);
    }

    #[test]
    fn timer_acknowledgement_is_one_zero_eoi_write() {
        let mut io = RecordingIo::default();
        acknowledge(&mut io);
        assert_eq!(&io.writes[..io.len], &[(END_OF_INTERRUPT, 0)]);
    }
}
