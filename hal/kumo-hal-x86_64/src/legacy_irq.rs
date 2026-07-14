//j436
//j452

#![cfg_attr(not(any(target_os = "none", test)), allow(dead_code))]

//! Legacy x86 interrupt-controller and periodic-timer first light.
//!
//! QEMU's Multiboot machine exposes the PC-compatible 8259 PIC pair and 8254 PIT before ACPI/APIC
//! discovery exists. Keep the programming sequence pure over [`PortIo`] so the exact remap, mask,
//! divisor, and EOI contract is host-testable; only the metal initializer performs privileged
//! port I/O.

use core::sync::atomic::{AtomicU64, Ordering};

pub(crate) const INTERRUPT_VECTOR_BASE: u8 = 0x20;
pub(crate) const LEGACY_INTERRUPT_VECTORS: u8 = 16;

const PRIMARY_COMMAND: u16 = 0x20;
const PRIMARY_DATA: u16 = 0x21;
const SECONDARY_COMMAND: u16 = 0xa0;
const SECONDARY_DATA: u16 = 0xa1;
const PIT_CHANNEL0: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;
const IO_WAIT: u16 = 0x80;

const INIT_WITH_ICW4: u8 = 0x11;
const MODE_8086: u8 = 0x01;
const END_OF_INTERRUPT: u8 = 0x20;
const PIT_CHANNEL0_RATE_GENERATOR: u8 = 0x34;
const PIT_INPUT_HZ: u64 = 1_193_182;

/// All primary-PIC lines masked. [`initialize_sequence`] leaves the primary at `0xfe` (only IRQ0
/// live), so writing this masks IRQ0 while the already-masked lines stay masked. — CORVUS
const PRIMARY_ALL_MASKED: u8 = 0xff;

static TIMER_INTERRUPTS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TimerSetup {
    pub input_hz: u64,
    pub actual_hz: u64,
    pub divisor: u16,
}

trait PortIo {
    fn write8(&mut self, port: u16, value: u8);
}

pub(crate) fn timer_setup(period_hz: u64) -> Option<TimerSetup> {
    if period_hz == 0 || period_hz > PIT_INPUT_HZ {
        return None;
    }
    let rounded = PIT_INPUT_HZ.checked_add(period_hz / 2)? / period_hz;
    if !(1..=u16::MAX as u64).contains(&rounded) {
        return None;
    }
    let divisor = rounded as u16;
    Some(TimerSetup {
        input_hz: PIT_INPUT_HZ,
        actual_hz: (PIT_INPUT_HZ + u64::from(divisor) / 2) / u64::from(divisor),
        divisor,
    })
}

fn initialize_sequence<IO: PortIo>(io: &mut IO, setup: TimerSetup) {
    // Remap the cascaded PICs away from CPU exceptions, retain their hardware cascade on IRQ2,
    // then mask every source except the PIT on IRQ0. — KESTREL 2026-07-14
    write_with_wait(io, PRIMARY_COMMAND, INIT_WITH_ICW4);
    write_with_wait(io, SECONDARY_COMMAND, INIT_WITH_ICW4);
    write_with_wait(io, PRIMARY_DATA, INTERRUPT_VECTOR_BASE);
    write_with_wait(
        io,
        SECONDARY_DATA,
        INTERRUPT_VECTOR_BASE + LEGACY_INTERRUPT_VECTORS / 2,
    );
    write_with_wait(io, PRIMARY_DATA, 1 << 2);
    write_with_wait(io, SECONDARY_DATA, 2);
    write_with_wait(io, PRIMARY_DATA, MODE_8086);
    write_with_wait(io, SECONDARY_DATA, MODE_8086);
    io.write8(PRIMARY_DATA, 0xfe);
    io.write8(SECONDARY_DATA, 0xff);

    io.write8(PIT_COMMAND, PIT_CHANNEL0_RATE_GENERATOR);
    io.write8(PIT_CHANNEL0, setup.divisor as u8);
    io.write8(PIT_CHANNEL0, (setup.divisor >> 8) as u8);
}

/// Mask IRQ0 on the primary PIC — take the PIT off the legacy vector 0x20 path so the I/O APIC
/// redirection entry can own the timer without double delivery.
fn mask_timer_source_sequence<IO: PortIo>(io: &mut IO) {
    io.write8(PRIMARY_DATA, PRIMARY_ALL_MASKED);
}

fn acknowledge<IO: PortIo>(io: &mut IO, vector: u8) {
    let secondary_base = INTERRUPT_VECTOR_BASE + LEGACY_INTERRUPT_VECTORS / 2;
    if (secondary_base..INTERRUPT_VECTOR_BASE + LEGACY_INTERRUPT_VECTORS).contains(&vector) {
        io.write8(SECONDARY_COMMAND, END_OF_INTERRUPT);
    }
    if (INTERRUPT_VECTOR_BASE..INTERRUPT_VECTOR_BASE + LEGACY_INTERRUPT_VECTORS).contains(&vector) {
        io.write8(PRIMARY_COMMAND, END_OF_INTERRUPT);
    }
}

fn write_with_wait<IO: PortIo>(io: &mut IO, port: u16, value: u8) {
    io.write8(port, value);
    io.write8(IO_WAIT, 0);
}

#[cfg(target_os = "none")]
struct HardwarePorts;

#[cfg(target_os = "none")]
impl PortIo for HardwarePorts {
    fn write8(&mut self, port: u16, value: u8) {
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") port,
                in("al") value,
                options(nostack, nomem, preserves_flags)
            );
        }
    }
}

#[cfg(target_os = "none")]
pub(crate) fn initialize(setup: TimerSetup) {
    TIMER_INTERRUPTS.store(0, Ordering::Relaxed);
    initialize_sequence(&mut HardwarePorts, setup);
}

#[cfg(target_os = "none")]
pub(crate) fn mask_timer_source() {
    mask_timer_source_sequence(&mut HardwarePorts);
}

#[cfg(target_os = "none")]
pub(crate) fn handle(vector: u8) {
    if vector == INTERRUPT_VECTOR_BASE {
        TIMER_INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    }
    acknowledge(&mut HardwarePorts, vector);
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
        writes: [(u16, u8); 24],
        len: usize,
    }

    impl PortIo for RecordingIo {
        fn write8(&mut self, port: u16, value: u8) {
            self.writes[self.len] = (port, value);
            self.len += 1;
        }
    }

    impl RecordingIo {
        fn recorded(&self) -> &[(u16, u8)] {
            &self.writes[..self.len]
        }
    }

    #[test]
    fn timer_setup_rounds_to_a_representable_divisor() {
        assert_eq!(
            timer_setup(20),
            Some(TimerSetup {
                input_hz: PIT_INPUT_HZ,
                actual_hz: 20,
                divisor: 59_659,
            })
        );
        assert_eq!(timer_setup(0), None);
        assert_eq!(timer_setup(1), None);
        assert_eq!(timer_setup(PIT_INPUT_HZ + 1), None);
    }

    #[test]
    fn initialization_remaps_masks_and_programs_channel_zero() {
        let setup = timer_setup(100).unwrap();
        assert_eq!(setup.divisor, 11_932);
        let mut io = RecordingIo::default();
        initialize_sequence(&mut io, setup);

        assert_eq!(
            io.recorded(),
            &[
                (PRIMARY_COMMAND, INIT_WITH_ICW4),
                (IO_WAIT, 0),
                (SECONDARY_COMMAND, INIT_WITH_ICW4),
                (IO_WAIT, 0),
                (PRIMARY_DATA, 0x20),
                (IO_WAIT, 0),
                (SECONDARY_DATA, 0x28),
                (IO_WAIT, 0),
                (PRIMARY_DATA, 0x04),
                (IO_WAIT, 0),
                (SECONDARY_DATA, 0x02),
                (IO_WAIT, 0),
                (PRIMARY_DATA, MODE_8086),
                (IO_WAIT, 0),
                (SECONDARY_DATA, MODE_8086),
                (IO_WAIT, 0),
                (PRIMARY_DATA, 0xfe),
                (SECONDARY_DATA, 0xff),
                (PIT_COMMAND, PIT_CHANNEL0_RATE_GENERATOR),
                (PIT_CHANNEL0, 0x9c),
                (PIT_CHANNEL0, 0x2e),
            ]
        );
    }

    #[test]
    fn masking_the_timer_source_masks_irq0_on_the_primary_pic() {
        let mut io = RecordingIo::default();
        mask_timer_source_sequence(&mut io);
        // Only IRQ0 was live (0xfe) after init, so masking it fully masks the primary.
        assert_eq!(io.recorded(), &[(PRIMARY_DATA, 0xff)]);
    }

    #[test]
    fn acknowledgement_follows_the_pic_cascade() {
        let mut primary = RecordingIo::default();
        acknowledge(&mut primary, INTERRUPT_VECTOR_BASE);
        assert_eq!(primary.recorded(), &[(PRIMARY_COMMAND, END_OF_INTERRUPT)]);

        let mut secondary = RecordingIo::default();
        acknowledge(
            &mut secondary,
            INTERRUPT_VECTOR_BASE + LEGACY_INTERRUPT_VECTORS / 2,
        );
        assert_eq!(
            secondary.recorded(),
            &[
                (SECONDARY_COMMAND, END_OF_INTERRUPT),
                (PRIMARY_COMMAND, END_OF_INTERRUPT),
            ]
        );
    }
}
