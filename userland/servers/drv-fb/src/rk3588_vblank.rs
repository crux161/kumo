//j611
//j614
//j632

//! D1b: VP2's frame-start interrupt, delivered to drv-fb, and a measurement of its rate.
//!
//! Recovery class: soft-state. The VOP raises one interrupt line, GIC SPI 156 (INTID 188), the
//! OR of every enabled VOP source and of the display IOMMU's interrupts. drv-fb may clear only
//! VOP sources (the MMU page stays read-only), so [`preflight`] refuses unless every other
//! VOP enable and both IOMMU interrupt masks read zero and nothing is pending. Then only VP2's
//! frame-start enable is set. Each delivery is acknowledged by writing the auto-clearing
//! `PORT2_INTR_CLR` bit before the kernel unmasks the line (DESIGN/016). [`Meter`] turns the
//! delivery timestamps into a rate and its jitter. Register sources: RK3588 TRM Part 2 ch. 7
//! (`SYS_CTRL_*_INTR_*`, `MMU*_INT_*`). — TIMBERDOODLE

use crate::rk3588_mode::{Block, Hardware, Mode};

/// Interrupt enable registers other than VP2's: system 0/1, video ports 0/1/3, AFBCD 0/1.
pub const OTHER_ENABLES: [u32; 7] = [0x0080, 0x0090, 0x00a0, 0x00b0, 0x00d0, 0x00e0, 0x00f0];
pub const PORT2_INTR_EN: u32 = 0x00c0;
pub const PORT2_INTR_CLR: u32 = 0x00c4;
pub const PORT2_INTR_STATUS: u32 = 0x00c8;
pub const PORT2_INTR_RAW: u32 = 0x00cc;
/// `PORT2_INTR_*` bit 0: frame start (`intr_*_fs`, TRM pp. 875–877). Bit 5 is the
/// separate field-start source; this single progressive-mode probe counts frames. — KESTREL
pub const FS: u32 = 1;
/// Masked-write registers: the upper half enables writes to the lower half.
const fn masked(mask: u32, value: u32) -> u32 {
    (mask << 16) | (value & mask)
}

/// Display IOMMU interrupt registers, as offsets in the VOP aperture (MMU0 `+0x7e00`, MMU1
/// `+0x7f00`): `INT_MASK` `+0x1c` and `INT_STATUS` `+0x20`.
pub const MMU_INT_REGS: [u32; 4] = [0x7e1c, 0x7e20, 0x7f1c, 0x7f20];

/// How many frame starts the measurement takes: two seconds at 60 Hz.
pub const FRAMES: u32 = 120;
/// Drain the scanout diagnostics on the port before measuring; console scrolling can take
/// longer than one frame, coalescing interrupts while the source is masked. — KESTREL
pub const SETTLE_MS: u64 = 1_000;
/// If the frames do not arrive in this time, the measurement stops and reports what it saw.
pub const WATCHDOG_MS: u64 = 3_000;
/// A measured rate within this many parts per million of the mode's rate matches it.
pub const TOLERANCE_PPM: u64 = 5_000;

/// What stopped [`preflight`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Conflict {
    /// Another VOP interrupt source is enabled (offset, value).
    VopEnable(u32, u32),
    /// The display IOMMU could drive the shared line (offset, value).
    Mmu(u32, u32),
    /// VP2's enable was already set (value).
    AlreadyEnabled(u32),
}

/// Refuse unless VP2's frame start would be the only source on the line. `mmu` reads the
/// IOMMU page through a read-only mapping.
pub fn preflight(hw: &mut impl Hardware, mut mmu: impl FnMut(u32) -> u32) -> Result<(), Conflict> {
    for offset in OTHER_ENABLES {
        let value = hw.read(Block::Vop, offset) & 0xffff;
        if value != 0 {
            return Err(Conflict::VopEnable(offset, value));
        }
    }
    let port2 = hw.read(Block::Vop, PORT2_INTR_EN) & 0xffff;
    if port2 != 0 {
        return Err(Conflict::AlreadyEnabled(port2));
    }
    for offset in MMU_INT_REGS {
        let value = mmu(offset);
        if value != 0 {
            return Err(Conflict::Mmu(offset, value));
        }
    }
    Ok(())
}

/// For a caller that just queried an actual LIVE kernel DeviceCtx: kernel-owned
/// MMU sources may share the line, but both banks must name one paging-on root.
/// A fault source is never treated as frame-start completion. — KESTREL
pub fn preflight_owned(
    hw: &mut impl Hardware,
    mut mmu: impl FnMut(u32) -> u32,
) -> Result<(), Conflict> {
    for offset in OTHER_ENABLES {
        let value = hw.read(Block::Vop, offset) & 0xffff;
        if value != 0 {
            return Err(Conflict::VopEnable(offset, value));
        }
    }
    let enabled = hw.read(Block::Vop, PORT2_INTR_EN) & 0xffff;
    if enabled != 0 {
        return Err(Conflict::AlreadyEnabled(enabled));
    }
    let root = mmu(0x7e00);
    if root == 0 || root & 4095 != 0 || mmu(0x7f00) != root {
        return Err(Conflict::Mmu(0x7e00, root));
    }
    for base in [0x7e00, 0x7f00] {
        for (offset, expected, mask) in [
            (4, 1, 7),
            (0x14, 0, u32::MAX),
            (0x1c, 3, u32::MAX),
            (0x20, 0, u32::MAX),
        ] {
            let value = mmu(base + offset);
            if value & mask != expected {
                return Err(Conflict::Mmu(base + offset, value));
            }
        }
    }
    Ok(())
}

/// Clear any latched frame start, then enable VP2's frame-start interrupt alone.
pub fn enable(hw: &mut impl Hardware) {
    hw.write(Block::Vop, PORT2_INTR_CLR, masked(FS, FS));
    hw.barrier();
    hw.write(Block::Vop, PORT2_INTR_EN, masked(FS, FS));
    hw.barrier();
}

/// Read VP2 status and clear only our frame-start source before completing the IRQ.
/// Return every observed source, without clearing sources we do not own. — KESTREL
pub fn acknowledge(hw: &mut impl Hardware) -> u32 {
    let status = hw.read(Block::Vop, PORT2_INTR_STATUS) & 0x3ff;
    if status & FS != 0 {
        hw.write(Block::Vop, PORT2_INTR_CLR, masked(FS, FS));
        hw.barrier();
    }
    status
}

/// Disable VP2's frame-start interrupt and clear anything left latched.
pub fn disable(hw: &mut impl Hardware) {
    hw.write(Block::Vop, PORT2_INTR_EN, masked(FS, 0));
    hw.barrier();
    hw.write(Block::Vop, PORT2_INTR_CLR, masked(FS, FS));
    hw.barrier();
}

/// Frame-start timestamps reduced to a rate and its spread.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Meter {
    pub count: u32,
    pub first_ns: u64,
    pub last_ns: u64,
    pub min_interval_ns: u64,
    pub max_interval_ns: u64,
    /// Deliveries whose status had no frame-start bit.
    pub spurious: u32,
}

impl Meter {
    pub fn record(&mut self, status: u32, now_ns: u64) {
        if status & FS == 0 {
            self.spurious += 1;
            return;
        }
        if self.count == 0 {
            self.first_ns = now_ns;
        } else {
            let interval = now_ns.saturating_sub(self.last_ns);
            if self.count == 1 || interval < self.min_interval_ns {
                self.min_interval_ns = interval;
            }
            if interval > self.max_interval_ns {
                self.max_interval_ns = interval;
            }
        }
        self.last_ns = now_ns;
        self.count += 1;
    }

    pub fn done(&self) -> bool {
        self.count >= FRAMES
    }

    /// Mean userspace delivery throughput, in millihertz. A masked level interrupt can
    /// coalesce frame starts while delivery is delayed; this is not the display cadence.
    pub fn millihertz(&self) -> Option<u64> {
        let intervals = u64::from(self.count.checked_sub(1)?);
        let elapsed = self.last_ns.checked_sub(self.first_ns)?;
        if intervals == 0 || elapsed == 0 {
            return None;
        }
        Some(intervals * 1_000_000_000_000 / elapsed)
    }
}

/// Display cadence estimated independently of the expected mode, with its sample quality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cadence {
    pub millihertz: u64,
    pub stable: usize,
    pub total: usize,
}

impl Cadence {
    /// Require at least three quarters of the intervals to agree with the median within
    /// 0.5%; heavy delivery interference must not pass just because the median matches.
    pub fn credible(&self) -> bool {
        self.total != 0 && self.stable * 4 >= self.total * 3
    }
}

/// Median inter-delivery period, converted to millihertz. Sorts the supplied samples in
/// place. Late/coalesced deliveries produce long gaps followed by short catch-up intervals;
/// their throughput underestimates refresh. The dominant undisturbed interval measures the
/// cadence without guessing how many frames were missed or using the expected mode. — KESTREL
pub fn cadence(intervals: &mut [u64]) -> Option<Cadence> {
    if intervals.is_empty() {
        return None;
    }
    intervals.sort_unstable();
    let median = intervals[intervals.len() / 2];
    if median == 0 {
        return None;
    }
    let stable = intervals
        .iter()
        .filter(|&&interval| {
            u128::from(interval.abs_diff(median)) * 1_000_000
                <= u128::from(median) * u128::from(TOLERANCE_PPM)
        })
        .count();
    Some(Cadence {
        millihertz: 1_000_000_000_000 / median,
        stable,
        total: intervals.len(),
    })
}

/// The frame rate a mode implies, in millihertz.
pub fn mode_millihertz(mode: &Mode) -> u64 {
    let frame = u64::from(mode.h_total()) * u64::from(mode.v_total());
    if frame == 0 {
        return 0;
    }
    mode.pixel_hz * 1_000 / frame
}

/// Whether a measured rate is within [`TOLERANCE_PPM`] of the mode's.
pub fn matches(measured_mhz: u64, mode_mhz: u64) -> bool {
    mode_mhz != 0 && measured_mhz.abs_diff(mode_mhz) * 1_000_000 <= mode_mhz * TOLERANCE_PPM
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rk3588_mode::DMT_1600X900_60_RB;
    extern crate std;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    #[derive(Default)]
    struct Vop {
        regs: BTreeMap<u32, u32>,
        writes: Vec<(u32, u32)>,
        barriers: usize,
    }

    impl Hardware for Vop {
        fn read(&mut self, block: Block, offset: u32) -> u32 {
            assert_eq!(block, Block::Vop);
            *self.regs.get(&offset).unwrap_or(&0)
        }

        fn write(&mut self, block: Block, offset: u32, value: u32) {
            assert_eq!(block, Block::Vop);
            self.writes.push((offset, value));
            if offset == PORT2_INTR_CLR {
                // Auto-clearing: the masked bits drop out of the status.
                let status = self.regs.entry(PORT2_INTR_STATUS).or_insert(0);
                *status &= !(value & (value >> 16));
            } else {
                let mask = value >> 16;
                let old = self.regs.get(&offset).copied().unwrap_or(0);
                self.regs.insert(offset, (old & !mask) | (value & mask));
            }
        }

        fn barrier(&mut self) {
            self.barriers += 1;
        }

        fn sleep_ms(&mut self, _ms: u64) {}
    }

    #[test]
    fn preflight_refuses_any_other_source_on_the_shared_line() {
        let mut vop = Vop::default();
        assert_eq!(preflight(&mut vop, |_| 0), Ok(()));
        for offset in OTHER_ENABLES {
            let mut vop = Vop::default();
            vop.regs.insert(offset, 0x0001);
            assert_eq!(
                preflight(&mut vop, |_| 0),
                Err(Conflict::VopEnable(offset, 1))
            );
        }
        let mut vop = Vop::default();
        vop.regs.insert(PORT2_INTR_EN, FS);
        assert_eq!(
            preflight(&mut vop, |_| 0),
            Err(Conflict::AlreadyEnabled(FS))
        );
        for offset in MMU_INT_REGS {
            let mut vop = Vop::default();
            assert_eq!(
                preflight(&mut vop, |at| if at == offset { 3 } else { 0 }),
                Err(Conflict::Mmu(offset, 3))
            );
        }
        // Write-mask bits in the upper half are not enables.
        let mut vop = Vop::default();
        vop.regs.insert(0x00b0, 0xffff_0000);
        assert_eq!(preflight(&mut vop, |_| 0), Ok(()));
        assert!(vop.writes.is_empty());
    }

    #[test]
    fn owned_preflight_requires_both_roots_paging_masks_and_zero_fault_sources() {
        let mut regs = std::collections::BTreeMap::new();
        for b in [0x7e00, 0x7f00] {
            regs.insert(b, 0x1000);
            regs.insert(b + 4, 0x19);
            regs.insert(b + 0x1c, 3);
        }
        let mut vop = Vop::default();
        assert_eq!(
            preflight_owned(&mut vop, |p| *regs.get(&p).unwrap_or(&0)),
            Ok(())
        );
        assert!(preflight(&mut vop, |p| *regs.get(&p).unwrap_or(&0)).is_err());
        for b in [0x7e00, 0x7f00] {
            for (offset, value) in [
                (0, 0),
                (0, 0x2000),
                (4, 0x18),
                (4, 0x1b),
                (4, 0x1d),
                (0x14, 1),
                (0x14, 2),
                (0x14, 4),
                (0x1c, 0),
                (0x1c, 7),
                (0x20, 1),
            ] {
                let mut fault = regs.clone();
                fault.insert(b + offset, value);
                assert!(preflight_owned(&mut vop, |p| *fault.get(&p).unwrap_or(&0)).is_err());
            }
        }
        assert!(vop.writes.is_empty());
    }

    #[test]
    fn enable_acknowledge_and_disable_touch_only_vp2_frame_start() {
        let mut vop = Vop::default();
        vop.regs.insert(PORT2_INTR_STATUS, FS);
        enable(&mut vop);
        assert_eq!(
            vop.writes,
            [(PORT2_INTR_CLR, 0x0001_0001), (PORT2_INTR_EN, 0x0001_0001)]
        );
        assert_eq!(vop.regs[&PORT2_INTR_STATUS], 0);

        vop.writes.clear();
        vop.regs.insert(PORT2_INTR_STATUS, FS);
        assert_eq!(acknowledge(&mut vop), FS);
        assert_eq!(vop.writes, [(PORT2_INTR_CLR, 0x0001_0001)]);
        assert_eq!(vop.regs[&PORT2_INTR_STATUS], 0);

        // Nothing pending: nothing written.
        vop.writes.clear();
        assert_eq!(acknowledge(&mut vop), 0);
        assert!(vop.writes.is_empty());

        disable(&mut vop);
        assert_eq!(
            vop.writes,
            [(PORT2_INTR_EN, 0x0001_0000), (PORT2_INTR_CLR, 0x0001_0001)]
        );
        assert_eq!(vop.regs[&PORT2_INTR_EN] & FS, 0);
        assert_eq!(vop.barriers, 5);
        for (offset, _) in &vop.writes {
            assert!([PORT2_INTR_EN, PORT2_INTR_CLR].contains(offset));
        }
    }

    #[test]
    fn acknowledge_leaves_unowned_pending_sources_untouched() {
        let mut vop = Vop::default();
        vop.regs.insert(PORT2_INTR_STATUS, FS | (1 << 5));
        assert_eq!(acknowledge(&mut vop), FS | (1 << 5));
        assert_eq!(vop.writes, [(PORT2_INTR_CLR, 0x10001)]);
        assert_eq!(vop.regs[&PORT2_INTR_STATUS], 1 << 5);
        vop.writes.clear();
        assert_eq!(acknowledge(&mut vop), 1 << 5);
        assert!(vop.writes.is_empty());
    }

    #[test]
    fn the_meter_reports_sixty_hertz_and_its_jitter() {
        let mut meter = Meter::default();
        // 120 frame starts 1/60 s apart, with one late and one early delivery.
        let period = 16_666_667u64;
        for frame in 0..FRAMES as u64 {
            let jitter = match frame {
                40 => 300_000,
                41 => 0,
                80 => 0,
                _ => 0,
            };
            meter.record(FS, 1_000_000_000 + frame * period + jitter);
        }
        meter.record(0, 9_999_999_999);
        assert!(meter.done());
        assert_eq!(meter.count, FRAMES);
        assert_eq!(meter.spurious, 1);
        assert_eq!(meter.max_interval_ns, period + 300_000);
        assert_eq!(meter.min_interval_ns, period - 300_000);
        let measured = meter.millihertz().unwrap();
        assert_eq!(mode_millihertz(&DMT_1600X900_60_RB), 60_000);
        assert!(matches(measured, 60_000), "{measured} mHz");
        assert!((59_999..=60_001).contains(&measured));
        // 30 Hz, the rate J600's wrong PLL model implied, does not match.
        assert!(!matches(30_000, 60_000));
        assert!(!matches(60_400, 60_000));
    }

    #[test]
    fn too_few_frames_give_no_rate() {
        let mut meter = Meter::default();
        assert_eq!(meter.millihertz(), None);
        meter.record(FS, 5);
        assert_eq!(meter.millihertz(), None);
        assert!(!meter.done());
        meter.record(FS, 5);
        assert_eq!(meter.millihertz(), None);
    }

    #[test]
    fn field_start_and_other_status_cannot_count_as_a_frame() {
        let mut meter = Meter::default();
        for status in [0, 1 << 5, 1 << 4, (1 << 5) | (1 << 4)] {
            meter.record(status, 1_000_000);
        }
        assert_eq!(meter.count, 0);
        assert_eq!(meter.spurious, 4);
        assert_eq!(meter.millihertz(), None);
        meter.record(FS, 2_000_000);
        assert_eq!(meter.first_ns, 2_000_000);
        assert_eq!(meter.count, 1);
    }

    #[test]
    fn missed_frames_and_the_tolerance_boundary_are_visible() {
        let mut meter = Meter::default();
        for frame in 0..FRAMES as u64 {
            meter.record(FS, 1_000_000_000 + frame * 33_333_333);
        }
        assert!(meter.done());
        assert!(!matches(meter.millihertz().unwrap(), 60_000));
        assert!(matches(59_700, 60_000));
        assert!(matches(60_300, 60_000));
        assert!(!matches(59_699, 60_000));
        assert!(!matches(60_301, 60_000));
        assert!(!matches(0, 0));
    }

    #[test]
    fn cadence_survives_late_and_coalesced_deliveries_without_hiding_throughput() {
        let period = 16_666_667;
        let mut intervals = [period; FRAMES as usize - 1];
        // Metal's characteristic long gap followed by a short catch-up interval. Three
        // actual frame starts coalesced in the first pair, two in the second. — KESTREL
        intervals[8] = 5 * period - 1_863_000;
        intervals[9] = 1_863_000;
        intervals[27] = 4 * period - 1_863_000;
        intervals[28] = 1_863_000;
        let mut meter = Meter::default();
        let mut now = 1_000_000_000;
        meter.record(FS, now);
        for interval in intervals {
            now += interval;
            meter.record(FS, now);
        }
        assert!(meter.done());
        assert!(!matches(meter.millihertz().unwrap(), 60_000));
        let observed = cadence(&mut intervals).unwrap();
        assert!(observed.credible());
        assert_eq!(observed.stable, intervals.len() - 4);
        assert!(matches(observed.millihertz, 60_000));
    }

    #[test]
    fn cadence_rejects_contaminated_samples_and_a_real_thirty_hertz_mode() {
        assert_eq!(cadence(&mut []), None);
        assert_eq!(cadence(&mut [0; 4]), None);
        let mut intervals = [16_666_667; FRAMES as usize - 1];
        intervals[..40].fill(33_333_334);
        let observed = cadence(&mut intervals).unwrap();
        assert!(!observed.credible());
        assert!(matches(observed.millihertz, 60_000));
        intervals.fill(33_333_334);
        let observed = cadence(&mut intervals).unwrap();
        assert!(observed.credible());
        assert!(!matches(observed.millihertz, 60_000));
    }
}
