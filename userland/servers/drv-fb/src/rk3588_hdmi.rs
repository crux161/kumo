//j600

//! D0d: read-only identification of the RK3588 HDMI0 transmitter and its combo PHY.
//!
//! Recovery class: soft-state; nothing is programmed. [`ReadRegisters`] exposes three
//! readers and no write method, no DDC/EDID transaction exists here, and every word is
//! sampled under the PMU evidence carried by D0a's bootstrap record. Register sources:
//! RK3588 TRM Part 2 ch. 24 (HDMI TX controller) and ch. 27 (HDMI TX/eDP combo PHY),
//! Part 1 ch. 6 (HDPTXPHY_GRF) and ch. 7 (PMU). — TIMBERDOODLE

use crate::rk3588::Power;

/// HDMI0 TX controller aperture. Every identity and status word used here is below 0x1000.
pub const HDMI_BASE: u64 = 0xfde8_0000;
pub const HDMI_LEN: u64 = 0x1000;
/// HDMI0 combo PHY (HDPTXPHY0); the highest lane register used here is 0x1878.
pub const PHY_BASE: u64 = 0xfed6_0000;
pub const PHY_LEN: u64 = 0x2000;
/// HDPTXPHY0_GRF: the Orange Pi DTB declares `rockchip,hdptxphy-grf` as 0x100 bytes.
pub const PHY_GRF_BASE: u64 = 0xfd5e_0000;
pub const PHY_GRF_LEN: u64 = 0x100;
/// VMAR mappings are page-granular, so the GRF is mapped over one page.
pub const PHY_GRF_MAP_LEN: u64 = 0x1000;

/// The PHY's reference is the 24 MHz board crystal when the selector says XO.
pub const REF_HZ: u64 = 24_000_000;
/// VESA DMT 1600x900@60 reduced blanking: the mode D0a measured on VP2.
pub const D0A_PIXEL_HZ: u64 = 108_000_000;

/// The display power domain and its bus-interface state that make HDMI0 reads safe.
///
/// PMU_PWR_GATE_STS0 bit 19 is PD_VO1 (HDMITX0/1 and eDP0/1); PMU_BIU_IDLE_ACK_STS0 and
/// PMU_BIU_IDLE_STS0 bit 16 are BIU_VO1. The DTB's `power-domains = <&power 0x1a>` is the
/// binding index 26, not a PMU bit. VO1's idle *request enable* lives in
/// PMU_BIU_IDLE_SFTCON1 bit 0, outside D0a's fixed 16-byte record; Sora observes it
/// read-only before granting the apertures. — TIMBERDOODLE
pub fn hdmi_accessible(power: Power) -> bool {
    const PD_VO1: u32 = 1 << 19;
    const BIU_VO1_ACK_STS: u32 = 1 << 16;
    power.gate_status & PD_VO1 == 0
        && power.idle_ack & BIU_VO1_ACK_STS == 0
        && power.idle_status & BIU_VO1_ACK_STS == 0
}

/// All three tagged bootstrap records must carry a resource handle before any read.
pub fn records_present(handles: [u32; 3]) -> bool {
    handles.iter().all(|&handle| handle != 0)
}

/// An implementation can observe registers, but cannot program them through this
/// interface: there is deliberately no write method.
pub trait ReadRegisters {
    fn hdmi(&mut self, offset: u32) -> u32;
    fn phy(&mut self, offset: u32) -> u32;
    fn grf(&mut self, offset: u32) -> u32;
}

/// Which serial link the TX and PHY program words agree on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Link {
    Tmds,
    Frl,
    /// The two blocks contradict each other or program an incomplete FRL/TMDS state.
    Unknown,
}

/// PHY protocol mux (0x0800[2]); the TRM HDMI table programs 1 for HDMI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    Hdmi,
    DisplayPort,
}

/// The output this probe can name from the PHY/TX words alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Output {
    Hdmi0Tmds,
    Hdmi0Frl,
    NotHdmi,
    Unknown,
}

/// Fixed-size video-monitor capture in the terms the TX measures it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timing {
    pub h_sync: u16,
    pub h_front: u16,
    pub h_blank: u16,
    pub h_back: u16,
    pub h_total: u16,
    pub h_active: u16,
    pub v_sync: u16,
    pub v_front: u16,
    pub v_blank: u16,
    pub v_back: u16,
    pub v_total: u16,
    pub v_active: u16,
}

/// D0a's measured VP2 timing, restated in the TX monitor's porch terms:
/// start/sync/end 1800/80/176/1776 gives Hback 96 and Hfront 24; V gives Vback 96
/// and Vfront 1. This is exactly VESA DMT 1600x900@60 reduced blanking.
pub const D0A_TIMING: Timing = Timing {
    h_sync: 80,
    h_front: 24,
    h_blank: 200,
    h_back: 96,
    h_total: 1800,
    h_active: 1600,
    v_sync: 3,
    v_front: 1,
    v_blank: 100,
    v_back: 96,
    v_total: 1000,
    v_active: 900,
};

/// HDMI TX controller words (TRM Part 2 ch. 24).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tx {
    pub core_id: u32,
    pub version: u32,
    pub video_status: u32,
    pub frame_composer: u32,
    pub monitor: [u32; 6],
    pub scrambler: u32,
    pub link_config: u32,
}

impl Tx {
    /// OPMODE_FRL is bit 0 and OPMODE_DVI is bit 4 of LINK_CONFIG0 (0x0968).
    pub fn link_config_frl(&self) -> bool {
        self.link_config & (1 << 0) != 0
    }

    pub fn dvi(&self) -> bool {
        self.link_config & (1 << 4) != 0
    }

    /// TMDS_SCRAMB_ENABLE is bit 0 of SCRAMB_CONFIG0 (0x0960).
    pub fn scrambled(&self) -> bool {
        self.scrambler & 1 != 0
    }

    /// Detected polarities are bits 18/17 of VIDEO_INTERFACE_STATUS0 (0x0814).
    pub fn hsync_positive(&self) -> bool {
        self.video_status & (1 << 18) != 0
    }

    pub fn vsync_positive(&self) -> bool {
        self.video_status & (1 << 17) != 0
    }

    pub fn hdmi_video_format(&self) -> u32 {
        (self.video_status >> 12) & 0b111
    }

    pub fn hdmi_color_depth(&self) -> u32 {
        (self.video_status >> 8) & 0xf
    }

    pub fn ipi_color_depth(&self) -> u32 {
        (self.video_status >> 4) & 0xf
    }

    pub fn ipi_format(&self) -> u32 {
        self.video_status & 0xf
    }

    /// VIDEO_MEAS_OVR_EN is bit 3 of FRAME_COMPOSER_CONFIG0 (0x0840).
    pub fn measure_override(&self) -> bool {
        self.frame_composer & (1 << 3) != 0
    }

    /// Decode the six VIDEO_MONITOR_STATUS words (0x0884..0x0898). A zeroed or
    /// internally inconsistent capture is rejected rather than reported as a mode.
    pub fn timing(&self) -> Option<Timing> {
        let m = self.monitor;
        let timing = Timing {
            h_sync: (m[0] >> 16) as u16,
            h_front: m[0] as u16,
            h_blank: (m[1] >> 16) as u16,
            h_back: m[1] as u16,
            h_total: (m[2] >> 16) as u16,
            h_active: m[2] as u16,
            v_sync: (m[3] >> 16) as u16,
            v_front: m[3] as u16,
            v_blank: (m[4] >> 16) as u16,
            v_back: m[4] as u16,
            v_total: (m[5] >> 16) as u16,
            v_active: m[5] as u16,
        };
        let h_porches =
            u32::from(timing.h_sync) + u32::from(timing.h_back) + u32::from(timing.h_front);
        let v_porches =
            u32::from(timing.v_sync) + u32::from(timing.v_back) + u32::from(timing.v_front);
        if timing.h_active == 0
            || timing.v_active == 0
            || h_porches != u32::from(timing.h_blank)
            || v_porches != u32::from(timing.v_blank)
            || u32::from(timing.h_active) + u32::from(timing.h_blank) != u32::from(timing.h_total)
            || u32::from(timing.v_active) + u32::from(timing.v_blank) != u32::from(timing.v_total)
        {
            return None;
        }
        Some(timing)
    }
}

/// HDPTX combo PHY words (TRM Part 2 ch. 27).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Phy {
    pub mdiv_rbr: u32,
    pub sdiv: u32,
    pub sdm_den_rbr: u32,
    pub sdm_sign: u32,
    pub sdm_num_rbr: u32,
    pub sdc_n_rbr: u32,
    pub sdc_num_rbr: u32,
    pub sdc_den_rbr: u32,
    pub pcg: u32,
    pub frl_mode: u32,
    pub lane_mux: u32,
    pub dig_clk_sel: u32,
    pub ropll: u32,
    pub status: u32,
    pub ref_sel: u32,
    pub protocol: u32,
    pub lane_en: u32,
    pub tmds_clk_lane: u32,
}

/// HDPTXPHY0_GRF words (TRM Part 1 ch. 6.23).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Grf {
    pub con0: u32,
    pub status: u32,
}

/// One fixed-size capture of the HDMI0 identity words.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub tx: Tx,
    pub phy: Phy,
    pub grf: Grf,
}

impl Snapshot {
    /// Sample the fixed identity/status word set. Refuses before the first read when
    /// the PMU does not prove PD_VO1 is up and BIU_VO1 is not idle.
    pub fn read(power: Power, io: &mut impl ReadRegisters) -> Option<Self> {
        if !hdmi_accessible(power) {
            return None;
        }
        Some(Self {
            tx: Tx {
                core_id: io.hdmi(0x0000),
                version: io.hdmi(0x0004),
                video_status: io.hdmi(0x0814),
                frame_composer: io.hdmi(0x0840),
                monitor: [
                    io.hdmi(0x0884),
                    io.hdmi(0x0888),
                    io.hdmi(0x088c),
                    io.hdmi(0x0890),
                    io.hdmi(0x0894),
                    io.hdmi(0x0898),
                ],
                scrambler: io.hdmi(0x0960),
                link_config: io.hdmi(0x0968),
            },
            phy: Phy {
                mdiv_rbr: io.phy(0x0144),
                sdiv: io.phy(0x0168),
                sdm_den_rbr: io.phy(0x0180),
                sdm_sign: io.phy(0x0190),
                sdm_num_rbr: io.phy(0x0194),
                sdc_n_rbr: io.phy(0x01a4),
                sdc_num_rbr: io.phy(0x01b0),
                sdc_den_rbr: io.phy(0x01c0),
                pcg: io.phy(0x0218),
                frl_mode: io.phy(0x021c),
                lane_mux: io.phy(0x0254),
                dig_clk_sel: io.phy(0x025c),
                ropll: io.phy(0x0264),
                status: io.phy(0x026c),
                ref_sel: io.phy(0x0280),
                protocol: io.phy(0x0800),
                lane_en: io.phy(0x081c),
                tmds_clk_lane: io.phy(0x1878),
            },
            grf: Grf {
                con0: io.grf(0x0000),
                status: io.grf(0x0080),
            },
        })
    }

    pub fn protocol(&self) -> Protocol {
        if self.phy.protocol & (1 << 2) != 0 {
            Protocol::Hdmi
        } else {
            Protocol::DisplayPort
        }
    }

    /// 0x0800[1] is 1 for HDMI TMDS and 0 for FRL (TRM table 27-5); 0x021C[3] enables
    /// FRL mode. LINK_CONFIG0[0] is the TX's own FRL bit. All three must agree.
    pub fn link(&self) -> Link {
        let tx_frl = self.tx.link_config_frl();
        let phy_tmds = self.phy.protocol & (1 << 1) != 0;
        let phy_frl_mode = self.frl_mode_enabled();
        match (tx_frl, phy_tmds, phy_frl_mode) {
            (false, true, false) => Link::Tmds,
            (true, false, true) => Link::Frl,
            _ => Link::Unknown,
        }
    }

    /// Name the output only when the PHY protocol and PLL operating mode are the HDMI0
    /// TMDS ones; otherwise say NotHdmi or Unknown rather than guessing.
    pub fn output(&self) -> Output {
        if self.protocol() == Protocol::DisplayPort {
            return Output::NotHdmi;
        }
        if !self.ropll_alone() || !self.ropll_reference_selected() {
            return Output::Unknown;
        }
        match self.link() {
            Link::Tmds => Output::Hdmi0Tmds,
            Link::Frl => Output::Hdmi0Frl,
            Link::Unknown => Output::Unknown,
        }
    }

    /// GRF CON0 bit 7 enables the PLL; bits 11:10 select its reference.
    pub fn pll_enabled(&self) -> bool {
        self.grf.con0 & (1 << 7) != 0
    }

    /// GRF CON0 bit 0 is 1 when the eDP controller or GRF, not the HDMI TX, drives aux.
    pub fn grf_mode_sel(&self) -> bool {
        self.grf.con0 & 1 != 0
    }

    pub fn reference_selector(&self) -> u32 {
        (self.grf.con0 >> 10) & 0b11
    }

    /// GRF STATUS0: 3 PLL lock, 2 PHY clock ready, 1 PHY lanes ready, 0 sideband ready.
    pub fn pll_locked(&self) -> bool {
        self.grf.status & (1 << 3) != 0
    }

    pub fn phy_clk_ready(&self) -> bool {
        self.grf.status & (1 << 2) != 0
    }

    pub fn phy_ready(&self) -> bool {
        self.grf.status & (1 << 1) != 0
    }

    pub fn sb_ready(&self) -> bool {
        self.grf.status & 1 != 0
    }

    /// The PHY's own status register (0x026C): 3 PHY ready, 2 PHY clock ready.
    pub fn phy_status_ready(&self) -> bool {
        self.phy.status & (1 << 3) != 0
    }

    pub fn phy_status_clk_ready(&self) -> bool {
        self.phy.status & (1 << 2) != 0
    }

    /// 0x0264[2]: 1 selects ROPLL-alone operation, the TRM's HDMI TMDS mode.
    pub fn ropll_alone(&self) -> bool {
        (self.phy.ropll >> 2) & 1 != 0
    }

    /// 0x025C[1]: 1 selects the ROPLL reference mux output (ROPLL-alone mode).
    pub fn ropll_reference_selected(&self) -> bool {
        (self.phy.dig_clk_sel >> 1) & 1 != 0
    }

    /// 0x021C[3]: HDMI 2.1 FRL mode enable.
    pub fn frl_mode_enabled(&self) -> bool {
        (self.phy.frl_mode >> 3) & 1 != 0
    }

    /// 0x0254[5:2]: TX lane LC/RO clock select; 0 means every lane uses the RO clock.
    pub fn lane_mux_dp(&self) -> u32 {
        (self.phy.lane_mux >> 2) & 0xf
    }

    /// 0x0254[1:0]: DP PLL register-set selector; 0 is the RBR set decoded here.
    pub fn dp_link_bw(&self) -> u32 {
        self.phy.lane_mux & 0b11
    }

    /// 0x081C[3:0]: the TRM's HDMI setting enables all four lanes.
    pub fn lanes_enabled(&self) -> bool {
        self.phy.lane_en & 0xf == 0xf
    }

    /// 0x1878[3]: lane 3 carries the TMDS clock in the TRM's 40-bit HDMI setting.
    pub fn tmds_clock_lane(&self) -> bool {
        (self.phy.tmds_clk_lane >> 3) & 1 != 0
    }

    /// 0x0280[4:2] selects the PCS reference: 0 XO, 1 IO, 2 pixel clock in, 3 Sys PLL.
    pub fn pcs_reference(&self) -> u32 {
        (self.phy.ref_sel >> 2) & 0b11
    }

    pub fn pcg_enabled(&self) -> bool {
        self.phy.pcg & 1 != 0
    }

    /// 0x0218[3:1]: 0 8-bit, 1 10-bit, 2 12-bit, 4 16-bit.
    pub fn pcg_depth(&self) -> u32 {
        (self.phy.pcg >> 1) & 0b111
    }

    pub fn pcg_postdiv(&self) -> u32 {
        (self.phy.pcg >> 4) & 0xf
    }

    pub fn sdc_divider(&self) -> Option<u32> {
        sdc_divider(self.phy.sdc_n_rbr)
    }

    /// The ROPLL reference frequency, only when the PLL is enabled and the reference
    /// selector says the 24 MHz crystal. Any other source makes the rate unknown.
    pub fn reference_hz(&self) -> Option<u64> {
        if !self.pll_enabled() {
            return None;
        }
        (self.reference_selector() == 0).then_some(REF_HZ)
    }

    /// The serializer bit clock from the RBR register set:
    ///
    /// `serial = reference * (mdiv -/+ numerator/denominator) / sdiv`
    ///
    /// The formula is anchored by the TRM's own DP settings (ch. 27 table 27-9):
    /// RBR 135 / 4 = 810 MHz exactly, HBR (113 - 13/39) / 2 = 1352 MHz and
    /// HBR2 (113 - 13/39) / 1 = 2704 MHz. It is reported only when the words show
    /// ROPLL-alone TMDS mode and the RBR PLL register set; every other case is Unknown.
    pub fn serial_hz(&self) -> Option<u64> {
        if self.output() != Output::Hdmi0Tmds {
            return None;
        }
        if !self.ropll_alone() || !self.ropll_reference_selected() {
            return None;
        }
        if self.dp_link_bw() != 0 {
            return None;
        }
        let reference = self.reference_hz()?;
        let mdiv = u64::from(self.phy.mdiv_rbr & 0xff);
        let denominator = u64::from(self.phy.sdm_den_rbr & 0xff);
        let numerator = u64::from(self.phy.sdm_num_rbr & 0xff);
        let negative = self.phy.sdm_sign & (1 << 3) != 0;
        let divider = u64::from(sdiv_divider(self.phy.sdiv >> 4)?);
        if mdiv == 0 || denominator == 0 {
            return None;
        }
        let integer = mdiv.checked_mul(denominator)?;
        let scaled = if negative {
            integer.checked_sub(numerator)?
        } else {
            integer.checked_add(numerator)?
        };
        if scaled == 0 {
            return None;
        }
        let hz = reference.checked_mul(scaled)? / denominator.checked_mul(divider)?;
        (hz != 0).then_some(hz)
    }

    /// The TMDS pixel clock for the 8-bit ratio the TRM's HDMI setting programs:
    /// `pixel = serial / 10`. Other color depths are reported as unknown in D0d.
    pub fn pixel_hz(&self) -> Option<u64> {
        if !self.pcg_enabled() || self.pcg_depth() != 0 {
            return None;
        }
        let serial = self.serial_hz()?;
        Some(serial / 10)
    }

    /// Refresh rate in millhertz from the derived pixel clock and the TX's own totals.
    pub fn refresh_millihz(&self) -> Option<u64> {
        let pixel = self.pixel_hz()?;
        let timing = self.tx.timing()?;
        let frame = u64::from(timing.h_total) * u64::from(timing.v_total);
        if frame == 0 {
            return None;
        }
        pixel.checked_mul(1000).map(|value| value / frame)
    }

    /// `Some(true)` only when the TX measures exactly D0a's VP2 timing.
    pub fn timing_matches_d0a(&self) -> Option<bool> {
        self.tx.timing().map(|timing| timing == D0A_TIMING)
    }

    /// `Some(true)` when the derived pixel clock is within 1% of D0a's 108 MHz mode.
    pub fn pixel_matches_d0a(&self) -> Option<bool> {
        let pixel = self.pixel_hz()?;
        Some(pixel.abs_diff(D0A_PIXEL_HZ) * 100 <= D0A_PIXEL_HZ)
    }
}

/// sdiv encoding: 0../1, 1../2, 2../3, 3../4, 4../5, 5../6, 6 invalid, 7../8.
pub fn sdiv_divider(code: u32) -> Option<u32> {
    match code & 0b111 {
        0 => Some(1),
        1 => Some(2),
        2 => Some(3),
        3 => Some(4),
        4 => Some(5),
        5 => Some(6),
        6 => None,
        _ => Some(8),
    }
}

/// sdc_n encoding: 0../3, 1../4, 2../5, 3../6, 4../7, 5../8.
pub fn sdc_divider(code: u32) -> Option<u32> {
    match code & 0b111 {
        0 => Some(3),
        1 => Some(4),
        2 => Some(5),
        3 => Some(6),
        4 => Some(7),
        5 => Some(8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    struct Fake {
        reads: Vec<(u8, u32)>,
    }

    impl ReadRegisters for Fake {
        fn hdmi(&mut self, offset: u32) -> u32 {
            self.reads.push((0, offset));
            offset
        }

        fn phy(&mut self, offset: u32) -> u32 {
            self.reads.push((1, offset));
            offset
        }

        fn grf(&mut self, offset: u32) -> u32 {
            self.reads.push((2, offset));
            offset
        }
    }

    fn powered() -> Power {
        Power {
            gate_status: 0,
            idle_request: 0,
            idle_ack: 0,
            idle_status: 0,
        }
    }

    /// The live 1600x900@60 configuration the TRM's HDMI table describes: HDMI TMDS,
    /// ROPLL alone, 24 MHz XO, RBR set, mdiv 225 and sdiv /5 (1080 MHz serial).
    fn fixture() -> Snapshot {
        Snapshot {
            tx: Tx {
                core_id: 0x4851_5458,
                version: 0x3031_3030,
                video_status: 0,
                frame_composer: 0,
                monitor: [
                    (80 << 16) | 24,
                    (200 << 16) | 96,
                    (1800 << 16) | 1600,
                    (3 << 16) | 1,
                    (100 << 16) | 96,
                    (1000 << 16) | 900,
                ],
                scrambler: 0,
                link_config: 0,
            },
            phy: Phy {
                mdiv_rbr: 225,
                sdiv: 4 << 4,
                sdm_den_rbr: 78,
                sdm_sign: 0,
                sdm_num_rbr: 0,
                sdc_n_rbr: 2,
                sdc_num_rbr: 3,
                sdc_den_rbr: 8,
                pcg: 0x11,
                frl_mode: 0,
                lane_mux: 0,
                dig_clk_sel: 1 << 1,
                ropll: 1 << 2,
                status: (1 << 3) | (1 << 2),
                ref_sel: 0,
                protocol: (1 << 2) | (1 << 1),
                lane_en: 0xf,
                tmds_clk_lane: 1 << 3,
            },
            grf: Grf {
                con0: 1 << 7,
                status: (1 << 3) | (1 << 2) | (1 << 1),
            },
        }
    }

    #[test]
    fn power_or_a_busy_vo1_prevents_every_hdmi_read() {
        for power in [
            Power {
                gate_status: 1 << 19,
                ..powered()
            },
            Power {
                idle_ack: 1 << 16,
                ..powered()
            },
            Power {
                idle_status: 1 << 16,
                ..powered()
            },
        ] {
            let mut io = Fake { reads: Vec::new() };
            assert!(Snapshot::read(power, &mut io).is_none());
            assert!(io.reads.is_empty());
        }
        // D0a's idle_request word is PMU_BIU_IDLE_SFTCON0, whose bit 0 is the GPU
        // request enable; VO1's request enable is SFTCON1 bit 0 and is observed by
        // Sora, so an SFTCON0 bit must not suppress a healthy display probe.
        let mut io = Fake { reads: Vec::new() };
        assert!(Snapshot::read(
            Power {
                idle_request: 1,
                ..powered()
            },
            &mut io
        )
        .is_some());
    }

    #[test]
    fn snapshot_reads_only_the_granted_identity_words_in_bounds() {
        let mut io = Fake { reads: Vec::new() };
        assert!(Snapshot::read(powered(), &mut io).is_some());
        let expected = [
            (0, 0x0000),
            (0, 0x0004),
            (0, 0x0814),
            (0, 0x0840),
            (0, 0x0884),
            (0, 0x0888),
            (0, 0x088c),
            (0, 0x0890),
            (0, 0x0894),
            (0, 0x0898),
            (0, 0x0960),
            (0, 0x0968),
            (1, 0x0144),
            (1, 0x0168),
            (1, 0x0180),
            (1, 0x0190),
            (1, 0x0194),
            (1, 0x01a4),
            (1, 0x01b0),
            (1, 0x01c0),
            (1, 0x0218),
            (1, 0x021c),
            (1, 0x0254),
            (1, 0x025c),
            (1, 0x0264),
            (1, 0x026c),
            (1, 0x0280),
            (1, 0x0800),
            (1, 0x081c),
            (1, 0x1878),
            (2, 0x0000),
            (2, 0x0080),
        ];
        assert_eq!(io.reads, expected);
        assert!(io.reads.iter().all(|&(aperture, offset)| {
            offset % 4 == 0
                && match aperture {
                    0 => u64::from(offset) < HDMI_LEN,
                    1 => u64::from(offset) < PHY_LEN,
                    _ => u64::from(offset) < PHY_GRF_LEN,
                }
        }));
        assert_eq!(io.reads.len(), 32);
    }

    #[test]
    fn dmt_1600x900_tmds_decodes_to_the_expected_identity_and_clock() {
        let s = fixture();
        assert_eq!(s.protocol(), Protocol::Hdmi);
        assert_eq!(s.link(), Link::Tmds);
        assert_eq!(s.output(), Output::Hdmi0Tmds);
        assert_eq!(s.reference_hz(), Some(REF_HZ));
        assert_eq!(s.serial_hz(), Some(1_080_000_000));
        assert_eq!(s.pixel_hz(), Some(108_000_000));
        assert_eq!(s.tx.timing(), Some(D0A_TIMING));
        assert_eq!(s.timing_matches_d0a(), Some(true));
        assert_eq!(s.pixel_matches_d0a(), Some(true));
        assert_eq!(s.refresh_millihz(), Some(60_000));
        assert!(!s.tx.scrambled());
        assert!(!s.tx.dvi());
        assert!(!s.tx.measure_override());
        assert!(s.pll_locked() && s.phy_ready() && s.phy_clk_ready());
        assert!(!s.sb_ready());
        assert!(s.lanes_enabled() && s.tmds_clock_lane());
        assert!(s.ropll_alone() && s.ropll_reference_selected());
        assert!(!s.frl_mode_enabled());
        assert_eq!(s.lane_mux_dp(), 0);
        assert_eq!(s.pcg_depth(), 0);
        assert_eq!(s.pcg_postdiv(), 1);
        assert_eq!(s.sdc_divider(), Some(5));
    }

    #[test]
    fn the_ropll_formula_reproduces_the_trm_dp_rate_anchors() {
        let mut rbr = fixture();
        rbr.phy.mdiv_rbr = 0x87;
        rbr.phy.sdiv = 3 << 4;
        rbr.phy.sdm_den_rbr = 0x21;
        rbr.phy.sdm_sign = 0;
        rbr.phy.sdm_num_rbr = 0;
        assert_eq!(rbr.serial_hz(), Some(810_000_000));

        let mut hbr = fixture();
        hbr.phy.mdiv_rbr = 0x71;
        hbr.phy.sdiv = 1 << 4;
        hbr.phy.sdm_den_rbr = 0x27;
        hbr.phy.sdm_num_rbr = 0x0d;
        hbr.phy.sdm_sign = 1 << 3;
        assert_eq!(hbr.serial_hz(), Some(1_352_000_000));

        let mut hbr2 = fixture();
        hbr2.phy.mdiv_rbr = 0x71;
        hbr2.phy.sdiv = 0;
        hbr2.phy.sdm_den_rbr = 0x27;
        hbr2.phy.sdm_num_rbr = 0x0d;
        hbr2.phy.sdm_sign = 1 << 3;
        assert_eq!(hbr2.serial_hz(), Some(2_704_000_000));
    }

    #[test]
    fn contradictory_or_undecodable_words_report_unknown_instead_of_guessing() {
        let mut s = fixture();
        s.tx.link_config |= 1;
        assert_eq!(s.link(), Link::Unknown);
        assert_eq!(s.output(), Output::Unknown);
        assert_eq!(s.serial_hz(), None);
        assert_eq!(s.pixel_hz(), None);

        let mut s = fixture();
        s.phy.protocol = 0;
        assert_eq!(s.output(), Output::NotHdmi);
        assert_eq!(s.serial_hz(), None);

        let mut s = fixture();
        s.phy.ropll = 0;
        assert_eq!(s.output(), Output::Unknown);
        assert_eq!(s.serial_hz(), None);

        let mut s = fixture();
        s.phy.dig_clk_sel = 0;
        assert_eq!(s.serial_hz(), None);

        let mut s = fixture();
        s.grf.con0 = (1 << 7) | (2 << 10);
        assert_eq!(s.reference_hz(), None);
        assert_eq!(s.serial_hz(), None);

        let mut s = fixture();
        s.phy.lane_mux = 2;
        assert_eq!(s.dp_link_bw(), 2);
        assert_eq!(s.serial_hz(), None);

        let mut s = fixture();
        s.grf.con0 = 0;
        assert_eq!(s.serial_hz(), None);

        let mut s = fixture();
        s.phy.sdm_den_rbr = 0;
        assert_eq!(s.serial_hz(), None);

        let mut s = fixture();
        s.phy.sdiv = 6 << 4;
        assert_eq!(s.serial_hz(), None);

        let mut s = fixture();
        s.phy.pcg = 0x10;
        assert_eq!(s.pixel_hz(), None);

        let mut s = fixture();
        s.phy.pcg = 0x13;
        assert_eq!(s.pixel_hz(), None);
    }

    #[test]
    fn malformed_or_disagreeing_timing_is_rejected_or_flagged() {
        let mut s = fixture();
        s.tx.monitor[0] = (80 << 16) | 25;
        assert_eq!(s.tx.timing(), None);
        assert_eq!(s.timing_matches_d0a(), None);
        assert_eq!(s.refresh_millihz(), None);

        let mut s = fixture();
        s.tx.monitor = [0; 6];
        assert_eq!(s.tx.timing(), None);

        let mut s = fixture();
        s.tx.monitor = [
            (96 << 16) | 16,
            (160 << 16) | 48,
            (800 << 16) | 640,
            (2 << 16) | 10,
            (45 << 16) | 33,
            (525 << 16) | 480,
        ];
        let timing = s.tx.timing().unwrap();
        assert_eq!(
            timing,
            Timing {
                h_sync: 96,
                h_front: 16,
                h_blank: 160,
                h_back: 48,
                h_total: 800,
                h_active: 640,
                v_sync: 2,
                v_front: 10,
                v_blank: 45,
                v_back: 33,
                v_total: 525,
                v_active: 480,
            }
        );
        assert_eq!(s.timing_matches_d0a(), Some(false));
    }

    #[test]
    fn absent_bootstrap_records_are_rejected() {
        assert!(records_present([1, 2, 3]));
        assert!(!records_present([0, 2, 3]));
        assert!(!records_present([1, 0, 3]));
        assert!(!records_present([1, 2, 0]));
        assert!(!records_present([0, 0, 0]));
    }

    #[test]
    fn transmitter_status_bits_decode_individually() {
        let mut s = fixture();
        assert!(!s.tx.dvi());
        assert!(!s.tx.scrambled());
        assert!(!s.tx.hsync_positive());
        assert!(!s.tx.vsync_positive());
        s.tx.link_config = 1 << 4;
        assert!(s.tx.dvi());
        s.tx.scrambler = 1;
        assert!(s.tx.scrambled());
        s.tx.video_status = (1 << 18) | (1 << 17) | (3 << 12) | (12 << 8) | (10 << 4) | 5;
        assert!(s.tx.hsync_positive());
        assert!(s.tx.vsync_positive());
        assert_eq!(s.tx.hdmi_video_format(), 3);
        assert_eq!(s.tx.hdmi_color_depth(), 12);
        assert_eq!(s.tx.ipi_color_depth(), 10);
        assert_eq!(s.tx.ipi_format(), 5);
        s.tx.frame_composer = 1 << 3;
        assert!(s.tx.measure_override());
    }
}
