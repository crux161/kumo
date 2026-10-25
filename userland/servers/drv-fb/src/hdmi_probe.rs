//j600

//! D0d live adapter: read-only mappings of the HDMI0 TX, combo PHY and PHY GRF.
//! No device write, no DDC/EDID transaction and no clock/PMU mutation exists here.
//! — TIMBERDOODLE

use core::fmt::{self, Write};
use drv_fb::rk3588::{self, Power};
use drv_fb::rk3588_hdmi::{self, Link, Output, Protocol, ReadRegisters, Snapshot};
use kumo_abi::{BootInfo, Handle, VmarFlags};
use kumo_rt::{handle_close, resource_mint_mmio, vmar_map};

const HDMI_VA: u64 = 0x1502_0000;
const PHY_VA: u64 = 0x1503_0000;
const PHY_GRF_VA: u64 = 0x1504_0000;

struct Log {
    bytes: [u8; 384],
    len: usize,
}

impl Write for Log {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
        let dst = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        dst.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn log(args: fmt::Arguments<'_>) {
    let mut line = Log {
        bytes: [0; 384],
        len: 0,
    };
    if line.write_fmt(args).is_ok() {
        let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line.bytes[..line.len]);
    }
}

struct Registers;

impl ReadRegisters for Registers {
    fn hdmi(&mut self, offset: u32) -> u32 {
        assert!(
            u64::from(offset) + 4 <= rk3588_hdmi::HDMI_LEN && offset % 4 == 0,
            "HDMI0 TX read outside the granted aperture"
        );
        unsafe { core::ptr::read_volatile((HDMI_VA + u64::from(offset)) as *const u32) }
    }

    fn phy(&mut self, offset: u32) -> u32 {
        assert!(
            u64::from(offset) + 4 <= rk3588_hdmi::PHY_LEN && offset % 4 == 0,
            "PHY0 read outside the granted aperture"
        );
        unsafe { core::ptr::read_volatile((PHY_VA + u64::from(offset)) as *const u32) }
    }

    fn grf(&mut self, offset: u32) -> u32 {
        assert!(
            u64::from(offset) + 4 <= rk3588_hdmi::PHY_GRF_LEN && offset % 4 == 0,
            "PHY GRF read outside the granted aperture"
        );
        unsafe { core::ptr::read_volatile((PHY_GRF_VA + u64::from(offset)) as *const u32) }
    }
}

fn map(resource: Handle, base: u64, len: u64, map_len: u64, va: u64) -> bool {
    if resource.0 == 0 {
        return false;
    }
    let raw = resource_mint_mmio(resource, base, len);
    if raw == 0 || raw == u64::MAX {
        return false;
    }
    let vmo = Handle(raw as u32);
    // Device memory is the default for an MMIO VMO; READ gives an EL0 read-only mapping.
    // The process never retains a writable mapping of these registers. — TIMBERDOODLE
    let ok = vmar_map(Handle(0), vmo, 0, va, map_len, VmarFlags::READ.0) == 0;
    let _ = handle_close(vmo);
    ok
}

/// Observe the firmware-programmed HDMI0 identity after D0a's scanout report. The three
/// handles are closed whether or not the probe runs, mirroring D0a's record hygiene.
pub fn report(boot: &BootInfo, power: Option<Power>, hdmi: Handle, phy: Handle, grf: Handle) {
    if rk3588::eligible(
        boot.board_id(),
        boot.has_framebuffer() && boot.framebuffer.is_plausible(),
    ) {
        match power {
            Some(power) => probe(power, hdmi, phy, grf),
            None => log(format_args!(
                "display: no PMU observation; D0d probe skipped\n"
            )),
        }
    }
    for handle in [hdmi, phy, grf] {
        if handle.0 != 0 {
            let _ = handle_close(handle);
        }
    }
}

fn probe(power: Power, hdmi: Handle, phy: Handle, grf: Handle) {
    log(format_args!(
        "display: D0d read-only HDMI0 TX/PHY identity probe\n"
    ));
    log(format_args!(
        "display: PMU gate={:#010x} PD_VO1={} VO1 ack/status={:#010x}/{:#010x}\n",
        power.gate_status,
        if power.gate_status & (1 << 19) == 0 {
            "up"
        } else {
            "down"
        },
        power.idle_ack,
        power.idle_status
    ));
    if !rk3588_hdmi::hdmi_accessible(power) {
        log(format_args!(
            "display: HDMI0/VO1 down or bus idle; zero HDMI/PHY reads\n"
        ));
        return;
    }
    if !rk3588_hdmi::records_present([hdmi.0, phy.0, grf.0]) {
        log(format_args!(
            "display: HDMI0/PHY0/PHY-GRF records absent; D0d probe skipped\n"
        ));
        return;
    }
    if !map(
        hdmi,
        rk3588_hdmi::HDMI_BASE,
        rk3588_hdmi::HDMI_LEN,
        rk3588_hdmi::HDMI_LEN,
        HDMI_VA,
    ) {
        log(format_args!("display: HDMI0 TX mapping failed\n"));
        return;
    }
    if !map(
        phy,
        rk3588_hdmi::PHY_BASE,
        rk3588_hdmi::PHY_LEN,
        rk3588_hdmi::PHY_LEN,
        PHY_VA,
    ) {
        log(format_args!("display: PHY0 mapping failed\n"));
        return;
    }
    if !map(
        grf,
        rk3588_hdmi::PHY_GRF_BASE,
        rk3588_hdmi::PHY_GRF_LEN,
        rk3588_hdmi::PHY_GRF_MAP_LEN,
        PHY_GRF_VA,
    ) {
        log(format_args!("display: PHY GRF mapping failed\n"));
        return;
    }
    let Some(snapshot) = Snapshot::read(power, &mut Registers) else {
        log(format_args!("display: D0d PMU guard refused; zero reads\n"));
        return;
    };

    let tx = snapshot.tx;
    log(format_args!(
        "display: HDMI0 TX id={:#010x} ver={:#010x} link={:#010x} scrambler={:#010x}\n",
        tx.core_id, tx.version, tx.link_config, tx.scrambler
    ));
    log(format_args!(
        "display: HDMI0 {} dvi={} scramble={} hsync={} vsync={} depth={} ipi_depth={} format={}\n",
        link_name(snapshot.link()),
        tx.dvi(),
        tx.scrambled(),
        polarity(tx.hsync_positive()),
        polarity(tx.vsync_positive()),
        tx.hdmi_color_depth(),
        tx.ipi_color_depth(),
        tx.hdmi_video_format()
    ));
    log(format_args!(
        "display: HDMI0 video_status={:#010x} frame_composer={:#010x} meas_override={}\n",
        tx.video_status,
        tx.frame_composer,
        tx.measure_override()
    ));
    match tx.timing() {
        Some(timing) => {
            log(format_args!(
                "display: HDMI0 VMON {}x{} H total/sync/active/back/front/blank={}/{}/{}/{}/{}/{} V={}/{}/{}/{}/{}/{}\n",
                timing.h_active,
                timing.v_active,
                timing.h_total,
                timing.h_sync,
                timing.h_active,
                timing.h_back,
                timing.h_front,
                timing.h_blank,
                timing.v_total,
                timing.v_sync,
                timing.v_active,
                timing.v_back,
                timing.v_front,
                timing.v_blank
            ));
            if timing == rk3588_hdmi::D0A_TIMING {
                log(format_args!(
                    "display: D0d timing matches D0a VP2 measurement\n"
                ));
            } else {
                log(format_args!(
                    "display: D0d DISAGREEMENT: TX timing differs from D0a VP2 1600x900 H 1800/80/1600/96/24/200 V 1000/3/900/96/1/100\n"
                ));
            }
        }
        None => {
            let m = tx.monitor;
            log(format_args!(
                "display: HDMI0 timing unreadable/inconsistent: raw {:#010x}/{:#010x}/{:#010x}/{:#010x}/{:#010x}/{:#010x}\n",
                m[0], m[1], m[2], m[3], m[4], m[5]
            ));
        }
    }

    log(format_args!(
        "display: PHY0 protocol={} lane_mux={:#06x} dp_lanes={:#x} dp_bw={} lanes={:#x} clk_lane3={} mode_sel={}\n",
        protocol_name(snapshot.protocol()),
        snapshot.phy.lane_mux,
        snapshot.lane_mux_dp(),
        snapshot.dp_link_bw(),
        snapshot.phy.lane_en,
        snapshot.tmds_clock_lane(),
        if snapshot.grf_mode_sel() {
            "edp/grf"
        } else {
            "hdmi"
        }
    ));
    log(format_args!(
        "display: PHY0 GRF con0={:#010x} status={:#010x} pll_en={} lock={} phy_rdy={} clk_rdy={} sb_rdy={} ref_sel={}\n",
        snapshot.grf.con0,
        snapshot.grf.status,
        snapshot.pll_enabled(),
        snapshot.pll_locked(),
        snapshot.phy_ready(),
        snapshot.phy_clk_ready(),
        snapshot.sb_ready(),
        snapshot.reference_selector()
    ));
    log(format_args!(
        "display: PHY0 own status={:#010x} ready={} clk_ready={} pcs_ref={} frl_mode={} ropll_alone={} ropll_ref={}\n",
        snapshot.phy.status,
        snapshot.phy_status_ready(),
        snapshot.phy_status_clk_ready(),
        snapshot.pcs_reference(),
        snapshot.frl_mode_enabled(),
        snapshot.ropll_alone(),
        snapshot.ropll_reference_selected()
    ));
    log(format_args!(
        "display: PHY0 PLL mdiv={} sign={} num={} den={} sdiv_code={} pcg={:#010x} postdiv={} depth={} en={} sdc_n={}\n",
        snapshot.phy.mdiv_rbr & 0xff,
        if snapshot.phy.sdm_sign & (1 << 3) != 0 {
            "-"
        } else {
            "+"
        },
        snapshot.phy.sdm_num_rbr & 0xff,
        snapshot.phy.sdm_den_rbr & 0xff,
        snapshot.phy.sdiv >> 4,
        snapshot.phy.pcg,
        snapshot.pcg_postdiv(),
        snapshot.pcg_depth(),
        snapshot.pcg_enabled(),
        snapshot.phy.sdc_n_rbr & 0b111
    ));

    match (snapshot.serial_hz(), snapshot.pixel_hz()) {
        (Some(serial), Some(pixel)) => {
            let mdiv = snapshot.phy.mdiv_rbr & 0xff;
            let numerator = snapshot.phy.sdm_num_rbr & 0xff;
            let denominator = snapshot.phy.sdm_den_rbr & 0xff;
            let sign = if snapshot.phy.sdm_sign & (1 << 3) != 0 {
                " - "
            } else {
                " + "
            };
            let divider = rk3588_hdmi::sdiv_divider(snapshot.phy.sdiv >> 4).unwrap_or(0);
            log(format_args!(
                "display: PHY0 serial={} Hz = 24000000 * ({}{}{}/{}) / {}; 8-bit TMDS pixel = serial/10\n",
                serial, mdiv, sign, numerator, denominator, divider
            ));
            log(format_args!(
                "display: D0d pixel clock {} Hz (derived), refresh={}.{:03} Hz from VMON totals\n",
                pixel,
                snapshot.refresh_millihz().unwrap_or(0) / 1000,
                snapshot.refresh_millihz().unwrap_or(0) % 1000
            ));
            match snapshot.pixel_matches_d0a() {
                Some(true) => log(format_args!(
                    "display: D0d pixel clock matches D0a 1600x900@60 (108000000 Hz)\n"
                )),
                Some(false) => log(format_args!(
                    "display: D0d DISAGREEMENT: derived pixel {} Hz differs from D0a-implied 108000000 Hz\n",
                    pixel
                )),
                None => {}
            }
        }
        (Some(serial), None) => log(format_args!(
            "display: D0d pixel clock UNKNOWN: serial={} Hz but pcg en/depth is not 8-bit (pcg={:#010x})\n",
            serial, snapshot.phy.pcg
        )),
        (None, _) => log(format_args!(
            "display: D0d pixel clock UNKNOWN: PHY/TX words are not a TMDS ROPLL-alone configuration\n"
        )),
    }

    log(format_args!(
        "display: D0d TMDS clock ratio=UNKNOWN (SCDC sink register; DDC reads are out of D0d scope)\n"
    ));
    log(format_args!(
        "display: D0d output {}\n",
        output_name(snapshot.output())
    ));
}

fn link_name(link: Link) -> &'static str {
    match link {
        Link::Tmds => "TMDS",
        Link::Frl => "FRL",
        Link::Unknown => "UNKNOWN",
    }
}

fn protocol_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Hdmi => "HDMI",
        Protocol::DisplayPort => "DP",
    }
}

fn output_name(output: Output) -> &'static str {
    match output {
        Output::Hdmi0Tmds => "HDMI0 TMDS",
        Output::Hdmi0Frl => "HDMI0 FRL",
        Output::NotHdmi => "not HDMI (PHY protocol_sel = DP)",
        Output::Unknown => "UNKNOWN (PHY/TX mode words disagree or PLL is not ROPLL-alone)",
    }
}

fn polarity(positive: bool) -> &'static str {
    if positive {
        "+"
    } else {
        "-"
    }
}
