//j606

//! R0 (DESIGN/020 Amendment D0): read-only observation of the shared display state that the
//! live HDMI0 mode depends on but no D0 probe read. RESEARCH/014 lists it: VOP_GRF and VO1_GRF
//! (subsystem-shared), SYS_GRF and PHY0's PMU1CRU reset lines (SoC-shared), and two HDMI0 TX
//! configuration words.
//!
//! Recovery class: soft-state; nothing is programmed. [`ReadShared`] has five readers and no
//! write method. The CRU and TX words come through D0c's and D0d's existing read-only windows;
//! the three GRF pages come from read-only grants (J605 made those real). VO1_GRF is read only
//! when PD_VO1 is up, its bus is not idle and its four bus clocks are ungated, because
//! `pclk_vo1grf` (CRU `GATE_CON59` bit 12) clocks it. Register sources: RK3588 TRM Part 1
//! ch. 2 (CRU), ch. 6 (GRF); Part 2 ch. 24 (HDMI TX). — TIMBERDOODLE

use crate::rk3588::Power;
use crate::rk3588_hdmi::hdmi_accessible;

/// VOP_GRF: the DTB declares 0x2000; every word read here is in the first page.
pub const VOP_GRF_BASE: u64 = 0xfd5a_4000;
pub const VOP_GRF_LEN: u64 = 0x1000;
/// VO1_GRF: the DTB declares 0x4000; every word read here is in the first page.
pub const VO1_GRF_BASE: u64 = 0xfd5a_8000;
pub const VO1_GRF_LEN: u64 = 0x1000;
/// SYS_GRF: the DTB declares exactly one page.
pub const SYS_GRF_BASE: u64 = 0xfd58_c000;
pub const SYS_GRF_LEN: u64 = 0x1000;

/// Offsets from D0c's CRU window base (`0xfd7c0000`).
pub const CRU_GATE_CON59: u32 = 0x08ec;
/// PMU1CRU (`0xfd7f0000`) `SOFTRST_CON03`, inside D0c's `0x31000`-byte span.
pub const PMU1CRU_SOFTRST_CON03: u32 = 0x3_0a0c;
pub const VOP_GRF_VOP_CON2: u32 = 0x0008;
pub const VO1_GRF_VO1_CON0: u32 = 0x0000;
pub const VO1_GRF_VO1_CON3: u32 = 0x000c;
pub const VO1_GRF_VO1_CON9: u32 = 0x0024;
pub const SYS_GRF_SOC_CON2: u32 = 0x0308;
pub const SYS_GRF_SOC_CON7: u32 = 0x031c;
pub const TX_TIMER_BASE_CONFIG0: u32 = 0x0080;
pub const TX_HDCP2LOGIC_CONFIG0: u32 = 0x08e0;

/// `GATE_CON59` bits that clock VO1_GRF's bus: `hclk_vo1_root` (2), `pclk_vo1_root` (4),
/// `hclk_vo1` (9) and `pclk_vo1grf` (12). A set gate bit disables its clock.
pub const VO1_BUS_GATES: u32 = (1 << 2) | (1 << 4) | (1 << 9) | (1 << 12);
/// `PMU1CRU_SOFTRST_CON03` bits 11/12/13: PHY0 `init`, `cmn`, `lane`. A set bit asserts reset.
pub const PHY0_RESETS: u32 = (1 << 11) | (1 << 12) | (1 << 13);
/// `TIMER_BASE_CONFIG0`'s reset value, which the reference driver also writes.
pub const TIMER_BASE_RESET: u32 = 428_571_429;

/// An implementation can observe registers, but cannot program them through this interface.
pub trait ReadShared {
    fn cru(&mut self, offset: u32) -> u32;
    fn tx(&mut self, offset: u32) -> u32;
    fn vop_grf(&mut self, offset: u32) -> u32;
    fn vo1_grf(&mut self, offset: u32) -> u32;
    fn sys_grf(&mut self, offset: u32) -> u32;
}

/// Which windows the live adapter actually mapped.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Sources {
    pub cru: bool,
    pub tx: bool,
    pub vop_grf: bool,
    pub vo1_grf: bool,
    pub sys_grf: bool,
}

/// Why VO1_GRF was not read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vo1Skip {
    NoGrant,
    PowerOrBus,
    NoClockEvidence,
    BusClockGated,
}

/// One fixed-size capture; `None` means not observed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    pub gate_con59: Option<u32>,
    pub pmu1_softrst_con03: Option<u32>,
    pub vop_con2: Option<u32>,
    pub vo1_con0: Option<u32>,
    pub vo1_con3: Option<u32>,
    pub vo1_con9: Option<u32>,
    pub soc_con2: Option<u32>,
    pub soc_con7: Option<u32>,
    pub timer_base: Option<u32>,
    pub hdcp2logic: Option<u32>,
    pub vo1_skip: Option<Vo1Skip>,
}

fn bit(word: Option<u32>, index: u32) -> Option<bool> {
    word.map(|word| word & (1 << index) != 0)
}

impl Snapshot {
    /// Sample the fixed word set. Every read is gated on the window being mapped and on the
    /// PMU evidence for its domain; VO1_GRF additionally needs its bus clocks proven open.
    pub fn read(power: Power, sources: Sources, io: &mut impl ReadShared) -> Self {
        let mut snapshot = Self::default();
        if sources.cru {
            snapshot.gate_con59 = Some(io.cru(CRU_GATE_CON59));
            snapshot.pmu1_softrst_con03 = Some(io.cru(PMU1CRU_SOFTRST_CON03));
        }
        if sources.vop_grf && power.vop_accessible() {
            snapshot.vop_con2 = Some(io.vop_grf(VOP_GRF_VOP_CON2));
        }
        snapshot.vo1_skip = if !sources.vo1_grf {
            Some(Vo1Skip::NoGrant)
        } else if !hdmi_accessible(power) {
            Some(Vo1Skip::PowerOrBus)
        } else {
            match snapshot.gate_con59 {
                None => Some(Vo1Skip::NoClockEvidence),
                Some(gates) if gates & VO1_BUS_GATES != 0 => Some(Vo1Skip::BusClockGated),
                Some(_) => None,
            }
        };
        if snapshot.vo1_skip.is_none() {
            snapshot.vo1_con0 = Some(io.vo1_grf(VO1_GRF_VO1_CON0));
            snapshot.vo1_con3 = Some(io.vo1_grf(VO1_GRF_VO1_CON3));
            snapshot.vo1_con9 = Some(io.vo1_grf(VO1_GRF_VO1_CON9));
        }
        if sources.sys_grf {
            snapshot.soc_con2 = Some(io.sys_grf(SYS_GRF_SOC_CON2));
            snapshot.soc_con7 = Some(io.sys_grf(SYS_GRF_SOC_CON7));
        }
        if sources.tx && hdmi_accessible(power) {
            snapshot.timer_base = Some(io.tx(TX_TIMER_BASE_CONFIG0));
            snapshot.hdcp2logic = Some(io.tx(TX_HDCP2LOGIC_CONFIG0));
        }
        snapshot
    }

    /// `VOP_GRF_VOP_CON2`: 0 edp0_mode, 1 hdmitx0_mode, 2 hdmitx0_compress_mode,
    /// 3 edp1_mode, 4 hdmitx1_mode, 5 hdmitx1_compress_mode.
    pub fn vop_edp0(&self) -> Option<bool> {
        bit(self.vop_con2, 0)
    }

    pub fn vop_hdmitx0(&self) -> Option<bool> {
        bit(self.vop_con2, 1)
    }

    pub fn vop_hdmitx0_compress(&self) -> Option<bool> {
        bit(self.vop_con2, 2)
    }

    pub fn vop_edp1(&self) -> Option<bool> {
        bit(self.vop_con2, 3)
    }

    pub fn vop_hdmitx1(&self) -> Option<bool> {
        bit(self.vop_con2, 4)
    }

    /// `VO1_GRF_VO1_CON0` bits 6/5: HDMITX0 hsync/vsync polarity (0 normal, 1 inverted).
    pub fn hsync_inverted(&self) -> Option<bool> {
        bit(self.vo1_con0, 6)
    }

    pub fn vsync_inverted(&self) -> Option<bool> {
        bit(self.vo1_con0, 5)
    }

    /// `VO1_GRF_VO1_CON3` bit 11: the HDMITX0 interface enable.
    pub fn vo1_hdmitx0(&self) -> Option<bool> {
        bit(self.vo1_con3, 11)
    }

    /// `VO1_GRF_VO1_CON3` [7:4]: 5 is 8 bits per component, 6 is 10; others reserved.
    pub fn color_depth(&self) -> Option<u32> {
        self.vo1_con3.map(|word| (word >> 4) & 0xf)
    }

    /// `VO1_GRF_VO1_CON3` [3:0]: 0 RGB, 2 YCbCr 4:4:4, 3 YCbCr 4:2:0, 0xb compressed.
    pub fn ipi_format(&self) -> Option<u32> {
        self.vo1_con3.map(|word| word & 0xf)
    }

    /// `VO1_GRF_VO1_CON3` bits 10/9: 1 means the DDC SDA/SCL input comes from the pin
    /// (0 ties it to 1). Bit 8: 1 masks CEC. Bits 13/14: I2S5/SPDIF3 routed to HDMITX0.
    pub fn ddc_sda_from_pin(&self) -> Option<bool> {
        bit(self.vo1_con3, 10)
    }

    pub fn ddc_scl_from_pin(&self) -> Option<bool> {
        bit(self.vo1_con3, 9)
    }

    pub fn cec_masked(&self) -> Option<bool> {
        bit(self.vo1_con3, 8)
    }

    pub fn i2s_routed(&self) -> Option<bool> {
        bit(self.vo1_con3, 13)
    }

    /// `VO1_GRF_VO1_CON9` bit 10: the HDCP I2C grant comes from software (bit 11) rather
    /// than from the TX.
    pub fn grant_from_software(&self) -> Option<bool> {
        bit(self.vo1_con9, 10)
    }

    pub fn grant_software_value(&self) -> Option<bool> {
        bit(self.vo1_con9, 11)
    }

    /// `SYS_GRF_SOC_CON2` bit 13: 1 disables the HDMITX0 HPD interrupt.
    pub fn hpd_interrupt_masked(&self) -> Option<bool> {
        bit(self.soc_con2, 13)
    }

    /// `SYS_GRF_SOC_CON7` bit 12: 1 connects HDMITX0 HPD to its pin (0 ties it low).
    pub fn hpd_from_pin(&self) -> Option<bool> {
        bit(self.soc_con7, 12)
    }

    /// PHY0 `init`/`cmn`/`lane` reset lines; `Some(true)` means asserted.
    pub fn phy0_reset_asserted(&self, line: u32) -> Option<bool> {
        bit(self.pmu1_softrst_con03, 11 + line)
    }

    pub fn vo1_bus_clocks_open(&self) -> Option<bool> {
        self.gate_con59.map(|gates| gates & VO1_BUS_GATES == 0)
    }

    /// `HDCP2LOGIC_CONFIG0` bit 0: 1 bypasses the external HDCP2 module.
    pub fn hdcp2_bypass(&self) -> Option<bool> {
        bit(self.hdcp2logic, 0)
    }

    /// The state the reference HDMI0 enable leaves set, compared with what was observed.
    /// R1 and R2 start from these; a mismatch or an unobserved word is reported, never
    /// assumed away.
    pub fn expectations(&self) -> [Expectation; EXPECTATIONS] {
        let phy0_released = self.pmu1_softrst_con03.map(|word| word & PHY0_RESETS == 0);
        [
            Expectation::new("VOP_GRF hdmitx0_mode=1", self.vop_hdmitx0(), true),
            Expectation::new("VOP_GRF edp0_mode=0", self.vop_edp0(), false),
            Expectation::new("VO1_GRF hdmitx0_mode=1", self.vo1_hdmitx0(), true),
            Expectation::new("PHY0 init/cmn/lane released", phy0_released, true),
            Expectation::new("VO1 bus clocks open", self.vo1_bus_clocks_open(), true),
        ]
    }
}

pub const EXPECTATIONS: usize = 5;

/// One expected bit and what was observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Expectation {
    pub name: &'static str,
    pub observed: Option<bool>,
    pub expected: bool,
}

impl Expectation {
    const fn new(name: &'static str, observed: Option<bool>, expected: bool) -> Self {
        Self {
            name,
            observed,
            expected,
        }
    }

    pub fn verdict(&self) -> Verdict {
        match self.observed {
            None => Verdict::Unknown,
            Some(value) if value == self.expected => Verdict::Match,
            Some(_) => Verdict::Disagreement,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Match,
    Disagreement,
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    /// Records every read and answers from fixed words; unknown offsets read zero.
    struct Fake {
        reads: Vec<(u8, u32)>,
        gate_con59: u32,
        softrst: u32,
        vop_con2: u32,
        vo1: [u32; 3],
        sys: [u32; 2],
        tx: [u32; 2],
    }

    impl Fake {
        fn healthy() -> Self {
            Self {
                reads: Vec::new(),
                gate_con59: 0,
                softrst: 0,
                vop_con2: 1 << 1,
                vo1: [0, (1 << 11) | (5 << 4), 0],
                sys: [0, 1 << 12],
                tx: [TIMER_BASE_RESET, 1],
            }
        }
    }

    impl ReadShared for Fake {
        fn cru(&mut self, offset: u32) -> u32 {
            self.reads.push((0, offset));
            match offset {
                CRU_GATE_CON59 => self.gate_con59,
                PMU1CRU_SOFTRST_CON03 => self.softrst,
                _ => 0,
            }
        }

        fn tx(&mut self, offset: u32) -> u32 {
            self.reads.push((1, offset));
            match offset {
                TX_TIMER_BASE_CONFIG0 => self.tx[0],
                TX_HDCP2LOGIC_CONFIG0 => self.tx[1],
                _ => 0,
            }
        }

        fn vop_grf(&mut self, offset: u32) -> u32 {
            self.reads.push((2, offset));
            self.vop_con2
        }

        fn vo1_grf(&mut self, offset: u32) -> u32 {
            self.reads.push((3, offset));
            match offset {
                VO1_GRF_VO1_CON0 => self.vo1[0],
                VO1_GRF_VO1_CON3 => self.vo1[1],
                VO1_GRF_VO1_CON9 => self.vo1[2],
                _ => 0,
            }
        }

        fn sys_grf(&mut self, offset: u32) -> u32 {
            self.reads.push((4, offset));
            match offset {
                SYS_GRF_SOC_CON2 => self.sys[0],
                SYS_GRF_SOC_CON7 => self.sys[1],
                _ => 0,
            }
        }
    }

    /// The PMU words D0a measured on metal: every domain up, BIU idle bits 0-11 only.
    fn metal_power() -> Power {
        Power {
            gate_status: 0x0000_fff9,
            idle_request: 0,
            idle_ack: 0x0000_0fff,
            idle_status: 0x0000_0fff,
        }
    }

    const ALL: Sources = Sources {
        cru: true,
        tx: true,
        vop_grf: true,
        vo1_grf: true,
        sys_grf: true,
    };

    #[test]
    fn a_healthy_path_reads_exactly_the_fixed_words_in_order() {
        let mut io = Fake::healthy();
        let snapshot = Snapshot::read(metal_power(), ALL, &mut io);
        assert_eq!(
            io.reads,
            [
                (0, CRU_GATE_CON59),
                (0, PMU1CRU_SOFTRST_CON03),
                (2, VOP_GRF_VOP_CON2),
                (3, VO1_GRF_VO1_CON0),
                (3, VO1_GRF_VO1_CON3),
                (3, VO1_GRF_VO1_CON9),
                (4, SYS_GRF_SOC_CON2),
                (4, SYS_GRF_SOC_CON7),
                (1, TX_TIMER_BASE_CONFIG0),
                (1, TX_HDCP2LOGIC_CONFIG0),
            ]
        );
        assert_eq!(snapshot.vo1_skip, None);
        for &(_, offset) in &io.reads {
            assert_eq!(offset % 4, 0);
        }
        // Every GRF offset lies inside its one-page grant; the CRU offset inside D0c's span.
        for offset in [VOP_GRF_VOP_CON2] {
            assert!(u64::from(offset) + 4 <= VOP_GRF_LEN);
        }
        for offset in [VO1_GRF_VO1_CON0, VO1_GRF_VO1_CON3, VO1_GRF_VO1_CON9] {
            assert!(u64::from(offset) + 4 <= VO1_GRF_LEN);
        }
        for offset in [SYS_GRF_SOC_CON2, SYS_GRF_SOC_CON7] {
            assert!(u64::from(offset) + 4 <= SYS_GRF_LEN);
        }
        assert!(u64::from(PMU1CRU_SOFTRST_CON03) + 4 <= crate::rk3588_clock::CRU_LEN);
        assert!(u64::from(TX_HDCP2LOGIC_CONFIG0) + 4 <= crate::rk3588_hdmi::HDMI_LEN);
        assert!(snapshot
            .expectations()
            .iter()
            .all(|expectation| expectation.verdict() == Verdict::Match));
    }

    #[test]
    fn vo1_grf_is_never_read_without_power_bus_and_clock_evidence() {
        let cases = [
            (
                Sources {
                    vo1_grf: false,
                    ..ALL
                },
                metal_power(),
                0,
                Vo1Skip::NoGrant,
            ),
            (
                ALL,
                Power {
                    gate_status: metal_power().gate_status | (1 << 19),
                    ..metal_power()
                },
                0,
                Vo1Skip::PowerOrBus,
            ),
            (
                ALL,
                Power {
                    idle_ack: metal_power().idle_ack | (1 << 16),
                    ..metal_power()
                },
                0,
                Vo1Skip::PowerOrBus,
            ),
            (
                Sources { cru: false, ..ALL },
                metal_power(),
                0,
                Vo1Skip::NoClockEvidence,
            ),
            (ALL, metal_power(), 1 << 12, Vo1Skip::BusClockGated),
            (ALL, metal_power(), 1 << 9, Vo1Skip::BusClockGated),
        ];
        for (sources, power, gates, reason) in cases {
            let mut io = Fake {
                gate_con59: gates,
                ..Fake::healthy()
            };
            let snapshot = Snapshot::read(power, sources, &mut io);
            assert_eq!(snapshot.vo1_skip, Some(reason));
            assert!(io.reads.iter().all(|&(aperture, _)| aperture != 3));
            assert_eq!(snapshot.vo1_con3, None);
        }
    }

    #[test]
    fn vop_grf_and_tx_reads_follow_their_power_domains() {
        let mut io = Fake::healthy();
        let vop_down = Power {
            gate_status: metal_power().gate_status | (1 << 17),
            ..metal_power()
        };
        let snapshot = Snapshot::read(vop_down, ALL, &mut io);
        assert_eq!(snapshot.vop_con2, None);
        assert!(io.reads.iter().all(|&(aperture, _)| aperture != 2));

        let mut io = Fake::healthy();
        let vo1_down = Power {
            gate_status: metal_power().gate_status | (1 << 19),
            ..metal_power()
        };
        let snapshot = Snapshot::read(vo1_down, ALL, &mut io);
        assert_eq!(snapshot.timer_base, None);
        assert!(io.reads.iter().all(|&(aperture, _)| aperture != 1));

        let mut io = Fake::healthy();
        let snapshot = Snapshot::read(metal_power(), Sources::default(), &mut io);
        assert!(io.reads.is_empty());
        assert_eq!(snapshot.vo1_skip, Some(Vo1Skip::NoGrant));
    }

    #[test]
    fn fields_decode_individually() {
        let mut io = Fake {
            vop_con2: 0b11_1101,
            vo1: [
                (1 << 6) | (1 << 5),
                (1 << 13) | (1 << 11) | (1 << 10) | (1 << 9) | (1 << 8) | (6 << 4) | 0x3,
                (1 << 11) | (1 << 10),
            ],
            sys: [1 << 13, 1 << 12],
            tx: [TIMER_BASE_RESET, 0],
            ..Fake::healthy()
        };
        let s = Snapshot::read(metal_power(), ALL, &mut io);
        assert_eq!(s.vop_edp0(), Some(true));
        assert_eq!(s.vop_hdmitx0(), Some(false));
        assert_eq!(s.vop_hdmitx0_compress(), Some(true));
        assert_eq!(s.vop_edp1(), Some(true));
        assert_eq!(s.vop_hdmitx1(), Some(true));
        assert_eq!(s.hsync_inverted(), Some(true));
        assert_eq!(s.vsync_inverted(), Some(true));
        assert_eq!(s.vo1_hdmitx0(), Some(true));
        assert_eq!(s.color_depth(), Some(6));
        assert_eq!(s.ipi_format(), Some(3));
        assert_eq!(s.ddc_sda_from_pin(), Some(true));
        assert_eq!(s.ddc_scl_from_pin(), Some(true));
        assert_eq!(s.cec_masked(), Some(true));
        assert_eq!(s.i2s_routed(), Some(true));
        assert_eq!(s.grant_from_software(), Some(true));
        assert_eq!(s.grant_software_value(), Some(true));
        assert_eq!(s.hpd_interrupt_masked(), Some(true));
        assert_eq!(s.hpd_from_pin(), Some(true));
        assert_eq!(s.hdcp2_bypass(), Some(false));
        assert_eq!(s.timer_base, Some(TIMER_BASE_RESET));
    }

    #[test]
    fn mismatches_and_gaps_are_reported_not_assumed() {
        // eDP0 enabled, HDMITX0 off in VOP_GRF, PHY0 lane reset asserted.
        let mut io = Fake {
            vop_con2: 1,
            softrst: 1 << 13,
            ..Fake::healthy()
        };
        let s = Snapshot::read(metal_power(), ALL, &mut io);
        let verdicts: Vec<Verdict> = s.expectations().iter().map(Expectation::verdict).collect();
        assert_eq!(
            verdicts,
            [
                Verdict::Disagreement,
                Verdict::Disagreement,
                Verdict::Match,
                Verdict::Disagreement,
                Verdict::Match,
            ]
        );
        assert_eq!(s.phy0_reset_asserted(2), Some(true));
        assert_eq!(s.phy0_reset_asserted(0), Some(false));

        // Nothing granted: every expectation is unknown, none is a match.
        let mut io = Fake::healthy();
        let s = Snapshot::read(metal_power(), Sources::default(), &mut io);
        assert!(s
            .expectations()
            .iter()
            .all(|expectation| expectation.verdict() == Verdict::Unknown));
    }
}
