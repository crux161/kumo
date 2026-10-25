//j606

//! R0 live adapter: read-only mappings of VOP_GRF, VO1_GRF and SYS_GRF, plus two words each
//! through D0c's CRU window and D0d's TX window when those probes mapped them. No device
//! write and no clock/PMU mutation exists here. — TIMBERDOODLE

use core::fmt::{self, Write};
use drv_fb::probe_va::{CRU_D0C, HDMI_D0D, SYS_GRF_R0, VO1_GRF_R0, VOP_GRF_R0};
use drv_fb::rk3588::{self, Power};
use drv_fb::rk3588_clock::CRU_LEN;
use drv_fb::rk3588_hdmi::HDMI_LEN;
use drv_fb::rk3588_shared::{
    self, ReadShared, Snapshot, Sources, Verdict, Vo1Skip, SYS_GRF_BASE, SYS_GRF_LEN, VO1_GRF_BASE,
    VO1_GRF_LEN, VOP_GRF_BASE, VOP_GRF_LEN,
};
use kumo_abi::{BootInfo, Handle, VmarFlags};
use kumo_rt::{handle_close, resource_mint_mmio, syscall_handle, vmar_map};

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

fn read_at(base: u64, len: u64, offset: u32) -> u32 {
    assert!(
        offset % 4 == 0 && u64::from(offset) + 4 <= len,
        "R0 read outside its window"
    );
    unsafe { core::ptr::read_volatile((base + u64::from(offset)) as *const u32) }
}

struct Registers;

impl ReadShared for Registers {
    fn cru(&mut self, offset: u32) -> u32 {
        read_at(CRU_D0C, CRU_LEN, offset)
    }

    fn tx(&mut self, offset: u32) -> u32 {
        read_at(HDMI_D0D, HDMI_LEN, offset)
    }

    fn vop_grf(&mut self, offset: u32) -> u32 {
        read_at(VOP_GRF_R0, VOP_GRF_LEN, offset)
    }

    fn vo1_grf(&mut self, offset: u32) -> u32 {
        read_at(VO1_GRF_R0, VO1_GRF_LEN, offset)
    }

    fn sys_grf(&mut self, offset: u32) -> u32 {
        read_at(SYS_GRF_R0, SYS_GRF_LEN, offset)
    }
}

/// Mint and map one read-only GRF page. The grant carries no `WRITE` (J605), so the VMO
/// cannot be mapped writable either.
fn map(resource: Handle, base: u64, len: u64, va: u64, label: &str) -> bool {
    if resource.0 == 0 {
        log(format_args!("display: R0 {} grant absent\n", label));
        return false;
    }
    let raw = resource_mint_mmio(resource, base, len);
    let Some(vmo) = syscall_handle(raw) else {
        log(format_args!(
            "display: R0 {} mint failed (errno {})\n",
            label, raw as u32 as i32
        ));
        return false;
    };
    let ok = vmar_map(Handle(0), vmo, 0, va, len, VmarFlags::READ.0) == 0;
    let _ = handle_close(vmo);
    if !ok {
        log(format_args!("display: R0 {} mapping failed\n", label));
    }
    ok
}

fn flag(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "1",
        Some(false) => "0",
        None => "?",
    }
}

/// Observe the shared state after D0a-D0d. `cru` and `tx` say whether D0c's and D0d's
/// windows are mapped; the three GRF handles are closed on every path.
#[allow(clippy::too_many_arguments)]
pub fn report(
    boot: &BootInfo,
    power: Option<Power>,
    cru: bool,
    tx: bool,
    vop_grf: Handle,
    vo1_grf: Handle,
    sys_grf: Handle,
) {
    if rk3588::eligible(
        boot.board_id(),
        boot.has_framebuffer() && boot.framebuffer.is_plausible(),
    ) {
        match power {
            Some(power) => probe(power, cru, tx, vop_grf, vo1_grf, sys_grf),
            None => log(format_args!(
                "display: no PMU observation; R0 probe skipped\n"
            )),
        }
    }
    for handle in [vop_grf, vo1_grf, sys_grf] {
        if handle.0 != 0 {
            let _ = handle_close(handle);
        }
    }
}

fn probe(power: Power, cru: bool, tx: bool, vop_grf: Handle, vo1_grf: Handle, sys_grf: Handle) {
    log(format_args!(
        "display: R0 read-only shared display state (VOP_GRF, VO1_GRF, SYS_GRF, PMU1CRU, TX)\n"
    ));
    let sources = Sources {
        cru,
        tx,
        vop_grf: map(vop_grf, VOP_GRF_BASE, VOP_GRF_LEN, VOP_GRF_R0, "VOP_GRF"),
        vo1_grf: map(vo1_grf, VO1_GRF_BASE, VO1_GRF_LEN, VO1_GRF_R0, "VO1_GRF"),
        sys_grf: map(sys_grf, SYS_GRF_BASE, SYS_GRF_LEN, SYS_GRF_R0, "SYS_GRF"),
    };
    let s = Snapshot::read(power, sources, &mut Registers);

    match (s.gate_con59, s.pmu1_softrst_con03) {
        (Some(gates), Some(resets)) => log(format_args!(
            "display: R0 CRU GATE_CON59={:#010x} vo1_bus_open={} PMU1CRU SOFTRST_CON03={:#010x} phy0 init/cmn/lane asserted={}/{}/{}\n",
            gates,
            flag(s.vo1_bus_clocks_open()),
            resets,
            flag(s.phy0_reset_asserted(0)),
            flag(s.phy0_reset_asserted(1)),
            flag(s.phy0_reset_asserted(2))
        )),
        _ => log(format_args!(
            "display: R0 CRU words not read (D0c window not mapped)\n"
        )),
    }
    match s.vop_con2 {
        Some(word) => log(format_args!(
            "display: R0 VOP_GRF VOP_CON2={:#010x} hdmitx0_mode={} hdmitx0_compress={} edp0_mode={} hdmitx1_mode={} edp1_mode={}\n",
            word,
            flag(s.vop_hdmitx0()),
            flag(s.vop_hdmitx0_compress()),
            flag(s.vop_edp0()),
            flag(s.vop_hdmitx1()),
            flag(s.vop_edp1())
        )),
        None => log(format_args!(
            "display: R0 VOP_GRF not read (no grant, or PD_VOP down/idle)\n"
        )),
    }
    match (s.vo1_con0, s.vo1_con3, s.vo1_con9) {
        (Some(con0), Some(con3), Some(con9)) => {
            log(format_args!(
                "display: R0 VO1_GRF CON0={:#010x} hdmitx0 hsync/vsync inverted={}/{}\n",
                con0,
                flag(s.hsync_inverted()),
                flag(s.vsync_inverted())
            ));
            log(format_args!(
                "display: R0 VO1_GRF CON3={:#010x} hdmitx0_mode={} color_depth={:#x} ipi_format={:#x} ddc sda/scl from pin={}/{} cec_masked={} i2s={}\n",
                con3,
                flag(s.vo1_hdmitx0()),
                s.color_depth().unwrap_or(0),
                s.ipi_format().unwrap_or(0),
                flag(s.ddc_sda_from_pin()),
                flag(s.ddc_scl_from_pin()),
                flag(s.cec_masked()),
                flag(s.i2s_routed())
            ));
            log(format_args!(
                "display: R0 VO1_GRF CON9={:#010x} hdcp_i2c_grant from_sw={} sw_value={}\n",
                con9,
                flag(s.grant_from_software()),
                flag(s.grant_software_value())
            ));
        }
        _ => log(format_args!(
            "display: R0 VO1_GRF not read: {}\n",
            match s.vo1_skip {
                Some(Vo1Skip::NoGrant) => "no grant",
                Some(Vo1Skip::PowerOrBus) => "PD_VO1 down or BIU_VO1 idle",
                Some(Vo1Skip::NoClockEvidence) => "no CRU evidence for its bus clocks",
                Some(Vo1Skip::BusClockGated) => "a VO1 bus clock is gated",
                None => "unknown",
            }
        )),
    }
    match (s.soc_con2, s.soc_con7) {
        (Some(con2), Some(con7)) => log(format_args!(
            "display: R0 SYS_GRF SOC_CON2={:#010x} hdmitx0_hpd_int_masked={} SOC_CON7={:#010x} hdmitx0_hpd_from_pin={}\n",
            con2,
            flag(s.hpd_interrupt_masked()),
            con7,
            flag(s.hpd_from_pin())
        )),
        _ => log(format_args!("display: R0 SYS_GRF not read (no grant)\n")),
    }
    match (s.timer_base, s.hdcp2logic) {
        (Some(timer), Some(hdcp2)) => log(format_args!(
            "display: R0 HDMI0 TIMER_BASE_CONFIG0={} ({}) HDCP2LOGIC_CONFIG0={:#010x} hdcp2_bypass={}\n",
            timer,
            if timer == rk3588_shared::TIMER_BASE_RESET {
                "reset value"
            } else {
                "not the reset value"
            },
            hdcp2,
            flag(s.hdcp2_bypass())
        )),
        _ => log(format_args!(
            "display: R0 TX words not read (D0d window not mapped)\n"
        )),
    }

    let expectations = s.expectations();
    let mut matched = 0;
    for expectation in &expectations {
        match expectation.verdict() {
            Verdict::Match => matched += 1,
            Verdict::Disagreement => log(format_args!(
                "display: R0 DISAGREEMENT: {} (observed {})\n",
                expectation.name,
                flag(expectation.observed)
            )),
            Verdict::Unknown => log(format_args!(
                "display: R0 UNKNOWN: {} (not observed)\n",
                expectation.name
            )),
        }
    }
    log(format_args!(
        "display: R0 reference-enable state {}/{} match\n",
        matched,
        expectations.len()
    ));
}
