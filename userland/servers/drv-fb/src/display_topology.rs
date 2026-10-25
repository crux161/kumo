//j598
//j601
//j611

//! D0b live adapter: map the supervisor's private live-DTB copy read-only, decode the RK3588
//! display topology, and compare the tree's VP→HDMI routing with D0a's measured scanout.
//!
//! No register writes, no power or clock change. Recovery class: soft-state; the supervisor
//! sends a fresh copy on every boot and a missing or rejected copy only skips the diagnostic.
//! — TIMBERDOODLE

use core::fmt::{self, Write};
use drv_fb::dtb_topology::{self, Routing, Transfer};
use drv_fb::probe_va::{DTB_D0B, VOP_D0B};
use drv_fb::rk3588::{self, Power, ReadRegisters, Snapshot};
use kumo_abi::{BootInfo, Handle, VmarFlags};
use kumo_rt::{handle_close, resource_mint_mmio, syscall_handle, vmar_map};

const PAGE_SIZE: u64 = 4096;

struct Log {
    bytes: [u8; 512],
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
        bytes: [0; 512],
        len: 0,
    };
    if line.write_fmt(args).is_ok() {
        let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line.bytes[..line.len]);
    }
}

/// Print a DT power-domain index, or `-` when the node declares none.
struct Index(u32);

impl fmt::Display for Index {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == dtb_topology::NO_INDEX {
            formatter.write_str("-")
        } else {
            write!(formatter, "{}", self.0)
        }
    }
}

struct Registers;

impl ReadRegisters for Registers {
    fn vop(&mut self, offset: u32) -> u32 {
        assert!(u64::from(offset) + 4 <= rk3588::VOP_LEN && offset % 4 == 0);
        unsafe { core::ptr::read_volatile((VOP_D0B + u64::from(offset)) as *const u32) }
    }
}

fn map_dtb(vmo: Handle, len: u64) -> bool {
    if vmo.0 == 0 || len == 0 {
        return false;
    }
    let Some(mapped) = len
        .checked_add(PAGE_SIZE - 1)
        .map(|value| value & !(PAGE_SIZE - 1))
    else {
        return false;
    };
    vmar_map(Handle(0), vmo, 0, DTB_D0B, mapped, VmarFlags::READ.0) == 0
}

/// Mint and map the duplicate VOP Resource read-only, exactly as D0a does for its own probe.
fn map_vop(resource: Handle) -> bool {
    if resource.0 == 0 {
        return false;
    }
    let raw = resource_mint_mmio(resource, rk3588::VOP_BASE, rk3588::VOP_LEN);
    let Some(vmo) = syscall_handle(raw) else {
        log(format_args!(
            "display: D0b VOP mint failed (errno {})\n",
            raw as u32 as i32
        ));
        return false;
    };
    let mapped = vmar_map(
        Handle(0),
        vmo,
        0,
        VOP_D0B,
        rk3588::VOP_LEN,
        VmarFlags::READ.0,
    ) == 0;
    let _ = handle_close(vmo);
    mapped
}

/// Entry point for the D0b record. `vop` is a private duplicate of D0a's VOP Resource so this
/// comparison can re-read the scanout after `display_probe::report` closes the original. Both
/// handles are consumed here. — TIMBERDOODLE
/// Returns the live DTB's VOP interrupt (GIC INTID) when the blob was received intact and
/// decoded, or 0; D1b arms its interrupt only when this equals the granted INTID (J611).
pub fn report(
    boot: &BootInfo,
    power: Option<Power>,
    vop: Handle,
    transfer: Option<Transfer>,
    dtb: Handle,
) -> u32 {
    let mut vop_irq = 0;
    if rk3588::eligible(
        boot.board_id(),
        boot.has_framebuffer() && boot.framebuffer.is_plausible(),
    ) {
        vop_irq = probe(boot, power, vop, transfer, dtb);
    }
    if vop.0 != 0 {
        let _ = handle_close(vop);
    }
    if dtb.0 != 0 {
        let _ = handle_close(dtb);
    }
    vop_irq
}

fn probe(
    boot: &BootInfo,
    power: Option<Power>,
    vop: Handle,
    transfer: Option<Transfer>,
    dtb: Handle,
) -> u32 {
    let Some(transfer) = transfer else {
        log(format_args!(
            "display: D0b no live-DTB record; topology skipped\n"
        ));
        return 0;
    };
    if transfer.total == 0 {
        log(format_args!(
            "display: D0b live-DTB record empty; topology skipped\n"
        ));
        return 0;
    }
    if !map_dtb(dtb, u64::from(transfer.total)) {
        log(format_args!(
            "display: D0b live-DTB mapping failed; topology skipped\n"
        ));
        return 0;
    }
    // SAFETY: `map_dtb` mapped exactly this extent read-only at DTB_D0B; the slice borrows the
    // process VA for the rest of this function, which owns both the mapping and the decoder.
    let bytes =
        unsafe { core::slice::from_raw_parts(DTB_D0B as *const u8, transfer.total as usize) };
    let checksum = dtb_topology::checksum(bytes);
    if checksum != transfer.checksum {
        log(format_args!(
            "display: D0b live-DTB checksum got={:#018x} sent={:#018x}; topology skipped\n",
            checksum, transfer.checksum
        ));
        return 0;
    }
    let Some(topology) = dtb_topology::decode(bytes) else {
        log(format_args!(
            "display: D0b live DTB rejected: magic, extent or structure invalid\n"
        ));
        return 0;
    };
    log(format_args!(
        "display: D0b live-DTB topology: {} bytes checksum={:#018x}\n",
        transfer.total, checksum
    ));
    log_vop(&topology);
    log_iommu(&topology);
    for index in 0..dtb_topology::HDMI_COUNT {
        log_hdmi(&topology, index);
    }
    for index in 0..dtb_topology::PHY_COUNT {
        log_phy(&topology, index);
    }
    log(format_args!(
        "display: D0b DT route: VP0->{} VP1->{} VP2->{} VP3->{} mutual={:#04x}\n",
        route_name(topology.route[0]),
        route_name(topology.route[1]),
        route_name(topology.route[2]),
        route_name(topology.route[3]),
        topology.route_mutual
    ));
    log_checks(&topology);
    report_routing(boot, power, vop, &topology);
    topology.vop_irq
}

fn log_vop(topology: &dtb_topology::Topology) {
    let device = topology.vop;
    log(format_args!(
        "display: D0b vop reg={:#x}+{:#x},{:#x}+{:#x} irq={} clocks={} pd={} enabled={}\n",
        device.base,
        device.len,
        device.base1,
        device.len1,
        topology.vop_irq,
        device.clocks,
        Index(device.power_index),
        device.enabled as u8
    ));
}

fn log_iommu(topology: &dtb_topology::Topology) {
    let device = topology.iommu;
    log(format_args!(
        "display: D0b iommu reg={:#x}+{:#x},{:#x}+{:#x} clocks={} pd={} enabled={} vop_link={}\n",
        device.base,
        device.len,
        device.base1,
        device.len1,
        device.clocks,
        Index(device.power_index),
        device.enabled as u8,
        topology.vop_iommu_link as u8
    ));
}

fn log_hdmi(topology: &dtb_topology::Topology, index: usize) {
    let device = topology.hdmi[index];
    log(format_args!(
        "display: D0b hdmi{} reg={:#x}+{:#x} clocks={} pd={} enabled={} phy_link={}\n",
        index,
        device.base,
        device.len,
        device.clocks,
        Index(device.power_index),
        device.enabled as u8,
        topology.hdmi_phy_link[index] as u8
    ));
}

fn log_phy(topology: &dtb_topology::Topology, index: usize) {
    let device = topology.phy[index];
    log(format_args!(
        "display: D0b phy{} reg={:#x}+{:#x} grf={:#x} enabled={}\n",
        index, device.base, device.len, topology.phy_grf[index], device.enabled as u8
    ));
}

fn route_name(entry: u8) -> &'static str {
    match entry {
        1 => "HDMI0",
        2 => "HDMI1",
        _ => "-",
    }
}

fn log_checks(topology: &dtb_topology::Topology) {
    let mut failed = 0usize;
    for check in dtb_topology::checks(topology) {
        if !check.ok {
            failed += 1;
            log(format_args!("display: D0b FACT MISMATCH {}\n", check.label));
        }
    }
    log(format_args!(
        "display: D0b DTB facts {}/{} match\n",
        dtb_topology::CHECK_COUNT - failed,
        dtb_topology::CHECK_COUNT
    ));
}

fn report_routing(
    boot: &BootInfo,
    power: Option<Power>,
    vop: Handle,
    topology: &dtb_topology::Topology,
) {
    let measured = measure(boot, power, vop);
    match dtb_topology::routing(topology, measured) {
        Routing::NoMeasurement => log(format_args!(
            "display: D0b measured route unavailable; DT routing recorded only\n"
        )),
        Routing::Agree { hdmi, vop_port } => log(format_args!(
            "display: D0b DT and measured routing agree: HDMI{} <- VP{}\n",
            hdmi, vop_port
        )),
        Routing::Disagree {
            hdmi,
            dt_vop_port,
            measured_vop_port,
        } => {
            log(format_args!(
                "display: D0b measured route: HDMI{} <- VP{} (GOP {}x{})\n",
                hdmi, measured_vop_port, boot.framebuffer.width, boot.framebuffer.height
            ));
            match dt_vop_port {
                Some(dt) => log(format_args!(
                    "display: D0b DISAGREEMENT: DT routes HDMI{} <- VP{}, firmware scans out VP{}; recorded, not corrected\n",
                    hdmi, dt, measured_vop_port
                )),
                None => log(format_args!(
                    "display: D0b DISAGREEMENT: DT routes no VP to HDMI{}, firmware scans out VP{}; recorded, not corrected\n",
                    hdmi, measured_vop_port
                )),
            }
        }
    }
}

/// Re-read the VOP only after the same PMU guard D0a uses: a down or idle domain means no VOP
/// reads, including reads of its MMU registers. — TIMBERDOODLE
fn measure(boot: &BootInfo, power: Option<Power>, vop: Handle) -> Option<(usize, usize)> {
    let power = power?;
    if !power.vop_accessible() || vop.0 == 0 || !map_vop(vop) {
        return None;
    }
    Snapshot::read(power, &mut Registers).and_then(|snapshot| {
        snapshot.matching_hdmi(boot.framebuffer.width, boot.framebuffer.height)
    })
}
