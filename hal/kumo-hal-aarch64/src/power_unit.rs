//j571

//! Kernel-run device power-up, named by the device's register aperture (DESIGN/020).
//!
//! A [`PowerUnit`] is one device's slice of the SoC's power and clock controllers: the exact PMU
//! and CRU bits that power it, clock it, take its bus out of idle and release its resets, plus the
//! safe rate its clock is set to before power-up. Userspace names a unit by its aperture and
//! proves the right with a Resource; it never passes a register, a bit or a rate. A unit exists
//! only when the board DTB holds its device, enabled, at that aperture (DESIGN/017: inert when
//! unknown).
//!
//! Every register access goes through [`PowerIo`], so the sequence is host-tested against a fake
//! PMU/CRU. The RK3588 facts are those of RESEARCH/009 §2.1–2.4 and J570's metal readout.

use crate::mali::{
    discover_rk3588_mali_g610, RK3588_CRU_CLKSEL_CON158, RK3588_CRU_GATE_CON66,
    RK3588_CRU_SOFTRST_CON66, RK3588_MALI_BASE, RK3588_MALI_CLOCK_REGISTERS, RK3588_MALI_LENGTH,
    RK3588_MALI_POWER_REGISTERS, RK3588_PMU_BISR_STS4, RK3588_PMU_BIU_IDLE_ACK_STS0,
    RK3588_PMU_BIU_IDLE_SFTCON0, RK3588_PMU_BIU_IDLE_STS0, RK3588_PMU_PWR_GATE_SFTCON0,
};
use crate::smmuv3::RK3588_PMU_PWR_GATE_STS0;

/// RK3588 CRU GPLL configuration (TRM Part 1 chapter 2): `CON0` M [9:0] and bypass bit 15;
/// `CON1` P [5:0], S [8:6] and power-down (`resetb`) bit 13; `CON2` the signed fraction K.
pub const RK3588_CRU_GPLL_CON0: u64 = 0xfd7c_01c0;
pub const RK3588_CRU_GPLL_CON1: u64 = 0xfd7c_01c4;
pub const RK3588_CRU_GPLL_CON2: u64 = 0xfd7c_01c8;
/// `CRU_MODE_CON00` [3:2] picks what `clk_gpll_mux` passes on: 0 the 24 MHz oscillator,
/// 1 the PLL, 2 deep-slow.
pub const RK3588_CRU_MODE_CON00: u64 = 0xfd7c_0280;

const OSC_HZ: u64 = 24_000_000;
const PLL_BYPASS: u32 = 1 << 15;
const PLL_POWER_DOWN: u32 = 1 << 13;
const PLL_MODE_NORMAL: u32 = 1;
/// Backstop for a clock that never advances: no wait polls more than this, whatever the time says.
const MAX_POLLS: u32 = 1_000_000;

/// Register access for the power sequence. The target implementation reads and writes physical
/// MMIO through the kernel physmap; host tests supply a fake PMU/CRU.
pub trait PowerIo {
    fn read(&mut self, phys: u64) -> u32;
    fn write(&mut self, phys: u64, value: u32);
    fn now_ns(&mut self) -> u64;
}

/// A PLL whose rate must be confirmed before the unit's clock is divided from it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PllCheck {
    pub con0: u64,
    pub con1: u64,
    pub con2: u64,
    pub mode: u64,
    pub mode_shift: u32,
    pub expected_hz: u64,
}

/// One device's power and clock controls (DESIGN/020 §2). Every bit field is a mask within its
/// register; the PMU and CRU control registers use Rockchip's write-mask convention (bits [31:16]
/// enable writing the matching bits [15:0]).
#[derive(Clone, Copy)]
pub struct PowerUnit {
    pub name: &'static str,
    pub aperture_base: u64,
    pub aperture_len: u64,
    /// True when the DTB holds this unit's device, enabled, at the aperture.
    pub present: fn(&[u8]) -> bool,
    /// Software power-down request; the same bit in `pwr_sts` reads 1 while down.
    pub pwr_sftcon: u64,
    pub pwr_sts: u64,
    pub domain: u32,
    /// Reads `repair` set once the domain is up and its memory repaired.
    pub repair_sts: u64,
    pub repair: u32,
    /// Bus-interface idle request, acknowledge and state.
    pub idle_sftcon: u64,
    pub idle_ack: u64,
    pub idle_sts: u64,
    pub idle: u32,
    pub pll: PllCheck,
    /// The clock select register and the words that set the safe boot rate, written in order.
    pub clksel: u64,
    pub clock_writes: [u32; 3],
    /// `clksel & clock_mask == clock_value` once the boot rate is set.
    pub clock_mask: u32,
    pub clock_value: u32,
    /// Software clock gates: a set bit disables the clock.
    pub gate: u64,
    pub core_gates: u32,
    pub bus_gates: u32,
    /// Software resets: a set bit holds the logic in reset.
    pub softrst: u64,
    pub core_resets: u32,
    pub bus_resets: u32,
    /// The words [`readout`] reports, in the kernel boot report's order.
    pub status_pmu: [u64; 6],
    pub status_cru: [u64; 8],
    /// Upper bound for each wait.
    pub wait_ns: u64,
}

fn rk3588_mali_present(dtb: &[u8]) -> bool {
    discover_rk3588_mali_g610(dtb).is_some()
}

/// The RK3588 Mali-G610 (`VD_GPU`), from RESEARCH/009 §2. Its clock goes to GPLL/6 = 198 MHz with
/// PVTPLL off before power-up (PLAN/013 decision 4): divider 5 first, then the GPLL parent, then
/// the PVTPLL bit cleared, so the rate only falls from the 702 MHz reset leaves it at.
/// — TIMBERDOODLE
pub const RK3588_GPU_POWER_UNIT: PowerUnit = PowerUnit {
    name: "MALI G610",
    aperture_base: RK3588_MALI_BASE,
    aperture_len: RK3588_MALI_LENGTH,
    present: rk3588_mali_present,
    pwr_sftcon: RK3588_PMU_PWR_GATE_SFTCON0,
    pwr_sts: RK3588_PMU_PWR_GATE_STS0,
    domain: 1 << 0,
    repair_sts: RK3588_PMU_BISR_STS4,
    repair: 1 << 1,
    idle_sftcon: RK3588_PMU_BIU_IDLE_SFTCON0,
    idle_ack: RK3588_PMU_BIU_IDLE_ACK_STS0,
    idle_sts: RK3588_PMU_BIU_IDLE_STS0,
    idle: 1 << 0,
    pll: PllCheck {
        con0: RK3588_CRU_GPLL_CON0,
        con1: RK3588_CRU_GPLL_CON1,
        con2: RK3588_CRU_GPLL_CON2,
        mode: RK3588_CRU_MODE_CON00,
        mode_shift: 2,
        expected_hz: 1_188_000_000,
    },
    clksel: RK3588_CRU_CLKSEL_CON158,
    clock_writes: [0x001f_0005, 0x00e0_0000, 0x4000_0000],
    clock_mask: 0x40ff,
    clock_value: 0x0005,
    gate: RK3588_CRU_GATE_CON66,
    core_gates: (1 << 1) | (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7),
    bus_gates: 0x7f << 8,
    softrst: RK3588_CRU_SOFTRST_CON66,
    core_resets: (1 << 4) | (1 << 5),
    bus_resets: (0x1f << 8) | (1 << 14),
    status_pmu: RK3588_MALI_POWER_REGISTERS,
    status_cru: RK3588_MALI_CLOCK_REGISTERS,
    wait_ns: 10_000_000,
};

/// Every power unit the kernel knows. A unit is live only on a board whose DTB holds it.
pub static POWER_UNITS: [PowerUnit; 1] = [RK3588_GPU_POWER_UNIT];

/// What the kernel learns about a live unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevicePowerUnitInfo {
    pub name: &'static str,
    pub aperture_base: u64,
    pub aperture_len: u64,
}

/// A wait in the power-up sequence that can time out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerStep {
    /// The domain to report powered and memory-repaired.
    Repair,
    /// The bus interface to drop its idle acknowledge.
    IdleAck,
    /// The bus interface to leave idle.
    Idle,
}

impl PowerStep {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Repair => "repair",
            Self::IdleAck => "idle-ack",
            Self::Idle => "idle",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DevicePowerUpOutcome {
    /// Every aperture precondition already held; nothing was written.
    AlreadyUp,
    /// The sequence ran; nanoseconds spent waiting for repair and for the bus to leave idle.
    Raised { repair_ns: u64, idle_ns: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DevicePowerError {
    /// No live unit has this aperture.
    NoUnit,
    /// The parent PLL is not at the rate the boot divider assumes; nothing was written.
    PllMismatch { hz: u64 },
    /// A wait expired; the software power-down request was written back.
    Timeout { step: PowerStep },
}

/// [`readout`]'s decoded state plus the raw words.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevicePowerReadout {
    pub powered: bool,
    pub repair_done: bool,
    pub bus_idle: bool,
    pub bus_idle_requested: bool,
    pub clock_at_boot_rate: bool,
    pub aperture_ready: bool,
    pub pmu: [u32; 6],
    pub cru: [u32; 8],
}

impl PowerUnit {
    pub const fn info(&self) -> DevicePowerUnitInfo {
        DevicePowerUnitInfo {
            name: self.name,
            aperture_base: self.aperture_base,
            aperture_len: self.aperture_len,
        }
    }

    /// True when `[phys_base, phys_base + len)` overlaps the aperture.
    pub const fn overlaps(&self, phys_base: u64, len: u64) -> bool {
        let Some(end) = phys_base.checked_add(len) else {
            return true;
        };
        len != 0 && phys_base < self.aperture_base + self.aperture_len && self.aperture_base < end
    }

    /// Every documented precondition for touching the aperture holds (the same list, in effect,
    /// as G1a's `MALI G610 MMIO` line).
    pub fn ready(&self, io: &mut impl PowerIo) -> bool {
        io.read(self.pwr_sts) & self.domain == 0
            && io.read(self.repair_sts) & self.repair != 0
            && io.read(self.idle_sftcon) & self.idle == 0
            && io.read(self.idle_sts) & self.idle == 0
            && io.read(self.gate) & (self.core_gates | self.bus_gates) == 0
            && io.read(self.softrst) & (self.core_resets | self.bus_resets) == 0
    }
}

/// The live unit whose aperture starts at `aperture_base`, if the DTB holds its device.
pub fn live_unit(dtb: &[u8], aperture_base: u64) -> Option<&'static PowerUnit> {
    POWER_UNITS
        .iter()
        .find(|unit| unit.aperture_base == aperture_base && (unit.present)(dtb))
}

/// The output rate of the PLL `check` describes, as the mux after it passes it on: 0 when the PLL
/// is powered down or misconfigured, the oscillator when bypassed or in slow mode.
pub fn pll_output_hz(check: &PllCheck, io: &mut impl PowerIo) -> u64 {
    let mode = (io.read(check.mode) >> check.mode_shift) & 0b11;
    if mode != PLL_MODE_NORMAL {
        return if mode == 0 { OSC_HZ } else { 0 };
    }
    let con0 = io.read(check.con0);
    let con1 = io.read(check.con1);
    if con0 & PLL_BYPASS != 0 {
        return OSC_HZ;
    }
    if con1 & PLL_POWER_DOWN != 0 {
        return 0;
    }
    let m = (con0 & 0x3ff) as i128;
    let p = (con1 & 0x3f) as i128;
    let s = (con1 >> 6) & 0b111;
    let k = io.read(check.con2) as u16 as i16 as i128;
    if p == 0 {
        return 0;
    }
    let hz = (OSC_HZ as i128 * (m * 65_536 + k)) / ((p << s) * 65_536);
    if hz < 0 {
        0
    } else {
        hz as u64
    }
}

/// Poll `done` until it holds or `wait_ns` passes; the time it took, or `None` on a timeout.
fn wait_until<I: PowerIo>(
    wait_ns: u64,
    io: &mut I,
    mut done: impl FnMut(&mut I) -> bool,
) -> Option<u64> {
    let start = io.now_ns();
    for _ in 0..MAX_POLLS {
        if done(io) {
            return Some(io.now_ns().saturating_sub(start));
        }
        if io.now_ns().saturating_sub(start) >= wait_ns {
            return None;
        }
    }
    None
}

/// Run `unit`'s power-up sequence (DESIGN/020 §4). On a timeout the software power-down request
/// is written back, so the domain is not left half-raised.
pub fn power_up(
    unit: &PowerUnit,
    io: &mut impl PowerIo,
) -> Result<DevicePowerUpOutcome, DevicePowerError> {
    if unit.ready(io) {
        return Ok(DevicePowerUpOutcome::AlreadyUp);
    }
    let hz = pll_output_hz(&unit.pll, io);
    if hz != unit.pll.expected_hz {
        return Err(DevicePowerError::PllMismatch { hz });
    }

    for word in unit.clock_writes {
        io.write(unit.clksel, word);
    }
    let gated = io.read(unit.gate) & (unit.core_gates | unit.bus_gates);
    if gated != 0 {
        io.write(unit.gate, gated << 16);
    }

    io.write(unit.pwr_sftcon, unit.domain << 16);
    let Some(repair_ns) = wait_until(unit.wait_ns, io, |io| {
        io.read(unit.repair_sts) & unit.repair != 0
    }) else {
        return Err(roll_back(unit, io, PowerStep::Repair));
    };

    io.write(unit.idle_sftcon, unit.idle << 16);
    let Some(ack_ns) = wait_until(unit.wait_ns, io, |io| {
        io.read(unit.idle_ack) & unit.idle == 0
    }) else {
        return Err(roll_back(unit, io, PowerStep::IdleAck));
    };
    let Some(idle_ns) = wait_until(unit.wait_ns, io, |io| {
        io.read(unit.idle_sts) & unit.idle == 0
    }) else {
        return Err(roll_back(unit, io, PowerStep::Idle));
    };

    let held = io.read(unit.softrst) & (unit.core_resets | unit.bus_resets);
    if held != 0 {
        io.write(unit.softrst, held << 16);
    }
    Ok(DevicePowerUpOutcome::Raised {
        repair_ns,
        idle_ns: ack_ns + idle_ns,
    })
}

fn roll_back(unit: &PowerUnit, io: &mut impl PowerIo, step: PowerStep) -> DevicePowerError {
    io.write(unit.pwr_sftcon, (unit.domain << 16) | unit.domain);
    DevicePowerError::Timeout { step }
}

/// Read `unit`'s status words and decode them.
pub fn readout(unit: &PowerUnit, io: &mut impl PowerIo) -> DevicePowerReadout {
    let pmu = unit.status_pmu.map(|phys| io.read(phys));
    let cru = unit.status_cru.map(|phys| io.read(phys));
    DevicePowerReadout {
        powered: io.read(unit.pwr_sts) & unit.domain == 0,
        repair_done: io.read(unit.repair_sts) & unit.repair != 0,
        bus_idle: io.read(unit.idle_sts) & unit.idle != 0,
        bus_idle_requested: io.read(unit.idle_sftcon) & unit.idle != 0,
        clock_at_boot_rate: io.read(unit.clksel) & unit.clock_mask == unit.clock_value,
        aperture_ready: unit.ready(io),
        pmu,
        cru,
    }
}

/// The mint guard (DESIGN/020 §5): true when `[phys_base, phys_base + len)` overlaps a live unit's
/// aperture while that unit is not ready. The DTB is consulted only for an overlapping range.
pub fn aperture_blocked(dtb: &[u8], phys_base: u64, len: u64, io: &mut impl PowerIo) -> bool {
    POWER_UNITS
        .iter()
        .any(|unit| unit.overlaps(phys_base, len) && (unit.present)(dtb) && !unit.ready(io))
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    const OPI5_DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    const X13S_DTB: &[u8] = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");
    const UNIT: PowerUnit = RK3588_GPU_POWER_UNIT;

    /// A behavioural RK3588 PMU/CRU for the GPU: write-masked control registers, a domain that
    /// powers up a few polls after its software request clears, and a bus that leaves idle a few
    /// polls after its request clears. Time advances 1 µs per read.
    struct FakeSoc {
        regs: BTreeMap<u64, u32>,
        writes: Vec<(u64, u32)>,
        now: u64,
        /// Polls until the domain reports power and repair; `None` never does.
        power_delay: Option<u32>,
        /// Polls until the bus drops its idle acknowledge and state; `None` never does.
        idle_delay: Option<u32>,
        power_countdown: Option<u32>,
        idle_countdown: Option<u32>,
    }

    const MASKED: [u64; 6] = [
        RK3588_PMU_PWR_GATE_SFTCON0,
        RK3588_PMU_BIU_IDLE_SFTCON0,
        RK3588_CRU_CLKSEL_CON158,
        RK3588_CRU_GATE_CON66,
        RK3588_CRU_SOFTRST_CON66,
        RK3588_CRU_MODE_CON00,
    ];

    impl FakeSoc {
        /// J570's Orange Pi readout: VD_GPU down and held down, bus idle and acknowledged with no
        /// request, every CRU word at reset; GPLL at Linux's 1188 MHz configuration.
        fn j570() -> Self {
            let mut regs = BTreeMap::new();
            regs.insert(RK3588_PMU_PWR_GATE_SFTCON0, 0x1);
            regs.insert(RK3588_PMU_PWR_GATE_STS0, 0xfff9);
            regs.insert(RK3588_PMU_BIU_IDLE_SFTCON0, 0);
            regs.insert(RK3588_PMU_BIU_IDLE_ACK_STS0, 0x1);
            regs.insert(RK3588_PMU_BIU_IDLE_STS0, 0x1);
            regs.insert(RK3588_PMU_BISR_STS4, 0);
            regs.insert(RK3588_CRU_CLKSEL_CON158, 0x1f80);
            regs.insert(RK3588_CRU_GATE_CON66, 0x4);
            regs.insert(RK3588_CRU_SOFTRST_CON66, 0);
            regs.insert(RK3588_CRU_GPLL_CON0, 198);
            regs.insert(RK3588_CRU_GPLL_CON1, (1 << 6) | 2);
            regs.insert(RK3588_CRU_GPLL_CON2, 0);
            regs.insert(RK3588_CRU_MODE_CON00, PLL_MODE_NORMAL << 2);
            Self {
                regs,
                writes: Vec::new(),
                now: 0,
                power_delay: Some(3),
                idle_delay: Some(2),
                power_countdown: None,
                idle_countdown: None,
            }
        }

        fn get(&self, phys: u64) -> u32 {
            self.regs.get(&phys).copied().unwrap_or(0)
        }

        fn step(&mut self) {
            if let Some(count) = self.power_countdown {
                if count == 0 {
                    self.power_countdown = None;
                    let sts = self.get(RK3588_PMU_PWR_GATE_STS0) & !1;
                    self.regs.insert(RK3588_PMU_PWR_GATE_STS0, sts);
                    let bisr = self.get(RK3588_PMU_BISR_STS4) | (1 << 1);
                    self.regs.insert(RK3588_PMU_BISR_STS4, bisr);
                } else {
                    self.power_countdown = Some(count - 1);
                }
            }
            if let Some(count) = self.idle_countdown {
                if count == 0 {
                    self.idle_countdown = None;
                    let ack = self.get(RK3588_PMU_BIU_IDLE_ACK_STS0) & !1;
                    self.regs.insert(RK3588_PMU_BIU_IDLE_ACK_STS0, ack);
                    let idle = self.get(RK3588_PMU_BIU_IDLE_STS0) & !1;
                    self.regs.insert(RK3588_PMU_BIU_IDLE_STS0, idle);
                } else {
                    self.idle_countdown = Some(count - 1);
                }
            }
        }
    }

    impl PowerIo for FakeSoc {
        fn read(&mut self, phys: u64) -> u32 {
            self.now += 1_000;
            self.step();
            self.get(phys)
        }

        fn write(&mut self, phys: u64, value: u32) {
            self.writes.push((phys, value));
            let value = if MASKED.contains(&phys) {
                let enable = value >> 16;
                (self.get(phys) & !enable) | (value & enable & 0xffff)
            } else {
                value
            };
            self.regs.insert(phys, value);
            if phys == RK3588_PMU_PWR_GATE_SFTCON0 && value & 1 == 0 {
                self.power_countdown = self.power_delay;
            }
            if phys == RK3588_PMU_BIU_IDLE_SFTCON0
                && value & 1 == 0
                && self.get(RK3588_PMU_PWR_GATE_STS0) & 1 == 0
            {
                self.idle_countdown = self.idle_delay;
            }
        }

        fn now_ns(&mut self) -> u64 {
            self.now
        }
    }

    #[test]
    fn the_gpu_unit_matches_research_009_and_g1a() {
        assert_eq!(UNIT.aperture_base, 0xfb00_0000);
        assert_eq!(UNIT.aperture_len, 0x20_0000);
        assert_eq!(UNIT.pwr_sftcon, 0xfd8d_814c);
        assert_eq!(UNIT.pwr_sts, 0xfd8d_8180);
        assert_eq!(UNIT.repair_sts, 0xfd8d_8290);
        assert_eq!((UNIT.domain, UNIT.repair, UNIT.idle), (1, 2, 1));
        assert_eq!(UNIT.clksel, 0xfd7c_0578);
        assert_eq!(UNIT.clock_writes, [0x001f_0005, 0x00e0_0000, 0x4000_0000]);
        assert_eq!((UNIT.pll.con0, UNIT.pll.mode), (0xfd7c_01c0, 0xfd7c_0280));
        assert_eq!(UNIT.status_pmu, RK3588_MALI_POWER_REGISTERS);
        assert_eq!(UNIT.status_cru, RK3588_MALI_CLOCK_REGISTERS);
        // The boot words decode to GPLL/6 with PVTPLL off.
        let mut con158 = 0x1f80u32;
        for word in UNIT.clock_writes {
            let enable = word >> 16;
            con158 = (con158 & !enable) | (word & enable & 0xffff);
        }
        assert_eq!(con158 & UNIT.clock_mask, UNIT.clock_value);
        assert_eq!((con158 >> 5) & 0b111, 0, "parent GPLL");
        assert_eq!((con158 & 0x1f) + 1, 6, "divide by 6");
        assert_eq!(1_188_000_000 / 6, 198_000_000);
    }

    #[test]
    fn units_are_live_only_where_the_dtb_holds_the_device() {
        assert_eq!(
            live_unit(OPI5_DTB, RK3588_MALI_BASE).map(|unit| unit.info()),
            Some(DevicePowerUnitInfo {
                name: "MALI G610",
                aperture_base: 0xfb00_0000,
                aperture_len: 0x20_0000,
            })
        );
        assert!(live_unit(OPI5_DTB, RK3588_MALI_BASE + 0x1000).is_none());
        assert!(live_unit(X13S_DTB, RK3588_MALI_BASE).is_none());
        assert!(live_unit(b"not a dtb", RK3588_MALI_BASE).is_none());
    }

    #[test]
    fn gpll_is_decoded_from_mode_m_p_s_and_k() {
        let mut soc = FakeSoc::j570();
        assert_eq!(pll_output_hz(&UNIT.pll, &mut soc), 1_188_000_000);
        soc.regs.insert(RK3588_CRU_MODE_CON00, 0);
        assert_eq!(pll_output_hz(&UNIT.pll, &mut soc), 24_000_000, "slow mode");
        soc.regs.insert(RK3588_CRU_MODE_CON00, 2 << 2);
        assert_eq!(pll_output_hz(&UNIT.pll, &mut soc), 0, "deep slow");
        soc.regs.insert(RK3588_CRU_MODE_CON00, PLL_MODE_NORMAL << 2);
        soc.regs.insert(RK3588_CRU_GPLL_CON0, 198 | PLL_BYPASS);
        assert_eq!(pll_output_hz(&UNIT.pll, &mut soc), 24_000_000, "bypass");
        soc.regs.insert(RK3588_CRU_GPLL_CON0, 198);
        soc.regs
            .insert(RK3588_CRU_GPLL_CON1, PLL_POWER_DOWN | (1 << 6) | 2);
        assert_eq!(pll_output_hz(&UNIT.pll, &mut soc), 0, "powered down");
        // Linux's 1500 MHz CPLL entry (P 2, M 250, S 1) and a fractional K.
        soc.regs.insert(RK3588_CRU_GPLL_CON1, (1 << 6) | 2);
        soc.regs.insert(RK3588_CRU_GPLL_CON0, 250);
        assert_eq!(pll_output_hz(&UNIT.pll, &mut soc), 1_500_000_000);
        soc.regs.insert(RK3588_CRU_GPLL_CON0, 198);
        soc.regs.insert(RK3588_CRU_GPLL_CON2, 0x8000);
        assert_eq!(
            pll_output_hz(&UNIT.pll, &mut soc),
            1_185_000_000,
            "K = -32768"
        );
    }

    #[test]
    fn power_up_from_the_j570_state_runs_the_designed_sequence() {
        let mut soc = FakeSoc::j570();
        let outcome = power_up(&UNIT, &mut soc).unwrap();
        assert!(matches!(outcome, DevicePowerUpOutcome::Raised { .. }));
        assert_eq!(
            soc.writes,
            [
                (RK3588_CRU_CLKSEL_CON158, 0x001f_0005),
                (RK3588_CRU_CLKSEL_CON158, 0x00e0_0000),
                (RK3588_CRU_CLKSEL_CON158, 0x4000_0000),
                (RK3588_PMU_PWR_GATE_SFTCON0, 0x0001_0000),
                (RK3588_PMU_BIU_IDLE_SFTCON0, 0x0001_0000),
            ],
            "clock first, then power, then bus; no gate or reset writes when none are set"
        );
        let state = readout(&UNIT, &mut soc);
        assert!(state.powered && state.repair_done && state.aperture_ready);
        assert!(!state.bus_idle && !state.bus_idle_requested);
        assert!(state.clock_at_boot_rate);
        assert_eq!(state.cru[0] & 0x40ff, 0x0005, "CLKSEL_CON158 = GPLL/6");
    }

    #[test]
    fn power_up_is_idempotent() {
        let mut soc = FakeSoc::j570();
        power_up(&UNIT, &mut soc).unwrap();
        let writes = soc.writes.len();
        assert_eq!(
            power_up(&UNIT, &mut soc),
            Ok(DevicePowerUpOutcome::AlreadyUp)
        );
        assert_eq!(soc.writes.len(), writes, "a second call writes nothing");
    }

    #[test]
    fn gated_clocks_and_held_resets_are_released_through_the_write_mask() {
        let mut soc = FakeSoc::j570();
        soc.regs
            .insert(RK3588_CRU_GATE_CON66, 0x4 | (1 << 4) | (1 << 9));
        soc.regs
            .insert(RK3588_CRU_SOFTRST_CON66, (1 << 5) | (1 << 14));
        power_up(&UNIT, &mut soc).unwrap();
        assert!(soc
            .writes
            .contains(&(RK3588_CRU_GATE_CON66, ((1 << 4) | (1 << 9)) << 16)));
        assert!(soc
            .writes
            .contains(&(RK3588_CRU_SOFTRST_CON66, ((1 << 5) | (1 << 14)) << 16)));
        assert_eq!(
            soc.get(RK3588_CRU_GATE_CON66),
            0x4,
            "the test-out gate is left alone"
        );
        assert_eq!(soc.get(RK3588_CRU_SOFTRST_CON66), 0);
    }

    #[test]
    fn a_wrong_gpll_refuses_before_any_write() {
        let mut soc = FakeSoc::j570();
        soc.regs.insert(RK3588_CRU_MODE_CON00, 0);
        assert_eq!(
            power_up(&UNIT, &mut soc),
            Err(DevicePowerError::PllMismatch { hz: 24_000_000 })
        );
        assert!(soc.writes.is_empty());
    }

    #[test]
    fn every_timeout_writes_the_power_down_request_back() {
        for (power_delay, idle_delay, step) in [
            (None, Some(2), PowerStep::Repair),
            (Some(3), None, PowerStep::IdleAck),
        ] {
            let mut soc = FakeSoc::j570();
            soc.power_delay = power_delay;
            soc.idle_delay = idle_delay;
            assert_eq!(
                power_up(&UNIT, &mut soc),
                Err(DevicePowerError::Timeout { step })
            );
            assert_eq!(
                soc.writes.last(),
                Some(&(RK3588_PMU_PWR_GATE_SFTCON0, 0x0001_0001)),
                "{}",
                step.name()
            );
            assert_eq!(soc.get(RK3588_PMU_PWR_GATE_SFTCON0) & 1, 1);
            assert!(soc.now >= UNIT.wait_ns, "the wait ran its full bound");
        }
        // The acknowledge drops but the bus never leaves idle.
        let mut stuck = FakeSoc::j570();
        stuck.idle_delay = None;
        stuck.regs.insert(RK3588_PMU_BIU_IDLE_ACK_STS0, 0);
        assert_eq!(
            power_up(&UNIT, &mut stuck),
            Err(DevicePowerError::Timeout {
                step: PowerStep::Idle
            })
        );
    }

    #[test]
    fn a_clock_that_never_advances_still_ends_the_wait() {
        struct FrozenClock(FakeSoc);
        impl PowerIo for FrozenClock {
            fn read(&mut self, phys: u64) -> u32 {
                self.0.get(phys)
            }
            fn write(&mut self, phys: u64, value: u32) {
                self.0.write(phys, value);
            }
            fn now_ns(&mut self) -> u64 {
                0
            }
        }
        let mut soc = FakeSoc::j570();
        soc.power_delay = None;
        assert_eq!(
            power_up(&UNIT, &mut FrozenClock(soc)),
            Err(DevicePowerError::Timeout {
                step: PowerStep::Repair
            })
        );
    }

    #[test]
    fn readout_reports_the_j570_state() {
        let mut soc = FakeSoc::j570();
        let state = readout(&UNIT, &mut soc);
        assert!(!state.powered && !state.repair_done && !state.aperture_ready);
        assert!(state.bus_idle && !state.bus_idle_requested);
        assert!(!state.clock_at_boot_rate, "reset leaves SPLL/1");
        assert_eq!(state.pmu, [0x1, 0xfff9, 0, 0x1, 0x1, 0]);
        assert_eq!(state.cru, [0x1f80, 0, 0, 0, 0x4, 0, 0, 0]);
    }

    #[test]
    fn the_mint_guard_blocks_only_a_live_aperture_that_is_not_ready() {
        let mut soc = FakeSoc::j570();
        assert!(aperture_blocked(
            OPI5_DTB,
            RK3588_MALI_BASE,
            0x1000,
            &mut soc
        ));
        assert!(aperture_blocked(
            OPI5_DTB,
            RK3588_MALI_BASE + 0x8_0000,
            4,
            &mut soc
        ));
        assert!(aperture_blocked(
            OPI5_DTB,
            RK3588_MALI_BASE - 0x1000,
            0x2000,
            &mut soc
        ));
        assert!(!aperture_blocked(
            OPI5_DTB,
            RK3588_MALI_BASE - 0x1000,
            0x1000,
            &mut soc
        ));
        assert!(!aperture_blocked(
            OPI5_DTB,
            RK3588_MALI_BASE + 0x20_0000,
            0x1000,
            &mut soc
        ));
        assert!(!aperture_blocked(
            X13S_DTB,
            RK3588_MALI_BASE,
            0x1000,
            &mut soc
        ));
        power_up(&UNIT, &mut soc).unwrap();
        assert!(!aperture_blocked(
            OPI5_DTB,
            RK3588_MALI_BASE,
            0x1000,
            &mut soc
        ));
    }

    #[test]
    fn a_non_overlapping_mint_never_reads_the_dtb_or_the_soc() {
        struct Untouchable;
        impl PowerIo for Untouchable {
            fn read(&mut self, phys: u64) -> u32 {
                panic!("read {phys:#x}")
            }
            fn write(&mut self, phys: u64, _value: u32) {
                panic!("write {phys:#x}")
            }
            fn now_ns(&mut self) -> u64 {
                panic!("clock")
            }
        }
        assert!(!aperture_blocked(
            b"not a dtb",
            0xfc00_0000,
            0x8000,
            &mut Untouchable
        ));
        assert!(!aperture_blocked(
            X13S_DTB,
            RK3588_MALI_BASE,
            0x1000,
            &mut Untouchable
        ));
    }

    #[test]
    fn ready_matches_g1a_aperture_preconditions() {
        use crate::mali::{MaliG610ClockStatus, MaliG610PowerStatus, MaliG610StatusReport};
        let topology = discover_rk3588_mali_g610(OPI5_DTB).unwrap();
        let bits = [0u32, 1, 2, 4, 5, 8, 9, 14];
        for (index, phys) in UNIT
            .status_pmu
            .iter()
            .chain(UNIT.status_cru.iter())
            .enumerate()
        {
            for bit in bits {
                let mut soc = FakeSoc::j570();
                power_up(&UNIT, &mut soc).unwrap();
                let flipped = soc.get(*phys) ^ (1 << bit);
                soc.regs.insert(*phys, flipped);
                let state = readout(&UNIT, &mut soc);
                let report = MaliG610StatusReport {
                    topology,
                    power: MaliG610PowerStatus::from_words(state.pmu),
                    clock: MaliG610ClockStatus::from_words(state.cru),
                };
                assert_eq!(
                    state.aperture_ready,
                    report.first_unmet_aperture_precondition().is_none(),
                    "word {index} bit {bit}"
                );
            }
        }
    }
}
