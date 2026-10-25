//j598

//! D0b: decode the live boot device tree's RK3588 display topology as data.
//!
//! The input is whatever blob firmware handed the boot chain, so every read goes through
//! [`kumo_fdt::Fdt`], which validates the header and the whole structure block before this
//! module sees a node. The decoder then resolves the display graph — VOP2, its IOMMU, both
//! HDMI encoders, both HDPTX PHYs and the port/endpoint/`phandle` links between them — into
//! the fixed-size [`Topology`] record.
//!
//! The record is evidence, not authority: nothing here writes a register or powers a domain.
//! [`checks`] compares it with the facts RESEARCH/010 pins for the Orange Pi 5 Plus;
//! [`routing`] compares the tree's VP→HDMI wiring with D0a's measured scanout. D0b logs a
//! disagreement; it does not correct one. — TIMBERDOODLE

use kumo_fdt::{be32, status_enabled, string_list_count, Fdt, Node};

/// Same admission bound as the existing Sora DTB consumers: a display tree larger than this
/// is not a board description we are prepared to trust. — TIMBERDOODLE
pub const MAX_DTB_BYTES: usize = 16 * 1024 * 1024;

pub const COMPAT_VOP: &[u8] = b"rockchip,rk3588-vop";
pub const COMPAT_IOMMU: &[u8] = b"rockchip,rk3588-iommu";
pub const COMPAT_HDMI: &[u8] = b"rockchip,rk3588-dw-hdmi-qp";
pub const COMPAT_HDPTX_PHY: &[u8] = b"rockchip,rk3588-hdptx-phy";

pub const HDMI_COUNT: usize = 2;
pub const PHY_COUNT: usize = 2;
pub const VOP_PORTS: usize = 4;
pub const NO_INDEX: u32 = u32::MAX;

/// One display node as the tree describes it. `regs` is the number of (address, size) regions
/// the decoder recognised; `base`/`len` are the first, `base1`/`len1` the second when present.
/// RK3588's top-level SoC nodes use `#address-cells = 2` and `#size-cells = 2`, so a region is
/// four big-endian cells. The decoder reads exactly that; a tree that changes cell counts will
/// fail the fact checks rather than be silently misread. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Device {
    pub present: bool,
    pub enabled: bool,
    pub regs: u8,
    pub base: u64,
    pub len: u64,
    pub base1: u64,
    pub len1: u64,
    pub clocks: u8,
    pub power_domains: u8,
    pub power_index: u32,
}

impl Device {
    pub const ABSENT: Self = Self {
        present: false,
        enabled: false,
        regs: 0,
        base: 0,
        len: 0,
        base1: 0,
        len1: 0,
        clocks: 0,
        power_domains: 0,
        power_index: NO_INDEX,
    };
}

impl Default for Device {
    fn default() -> Self {
        Self::ABSENT
    }
}

/// The compact fixed-size display-topology record. No allocation, no borrows of the blob.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Topology {
    pub vop: Device,
    /// GIC INTID of the VOP's first interrupt; 0 when absent or not an SPI.
    pub vop_irq: u32,
    pub iommu: Device,
    /// VOP `iommus` resolves to the IOMMU node in this tree.
    pub vop_iommu_link: bool,
    pub hdmi: [Device; HDMI_COUNT],
    /// Each HDMI encoder's `phys` resolves to the corresponding HDPTX PHY node.
    pub hdmi_phy_link: [bool; HDMI_COUNT],
    pub phy: [Device; PHY_COUNT],
    /// Each PHY's `rockchip,grf` resolves to a syscon and yields its first `reg` base.
    pub phy_grf: [u64; PHY_COUNT],
    /// VOP port index → `HDMI index + 1`. Zero means the port has no resolved link.
    pub route: [u8; VOP_PORTS],
    /// Bit `n` set when VOP port `n`'s endpoint and its remote endpoint point at each other.
    pub route_mutual: u8,
}

/// Decode a complete DTB into the topology record. `None` means the blob did not validate as a
/// device tree; a valid tree that simply lacks display nodes decodes to `present = false`
/// devices so the fact checks can report precisely what is missing.
pub fn decode(bytes: &[u8]) -> Option<Topology> {
    let fdt = Fdt::new(bytes)?;

    let vop_node = fdt.find_by_compatible(COMPAT_VOP);
    // Several rk3588 IOMMU blocks share this compatible string; the display one is whichever
    // node VOP's own `iommus` phandle names. Tree order is not evidence. — TIMBERDOODLE
    let iommu_node = vop_node
        .and_then(|vop| vop.property(b"iommus").and_then(|value| be32(value, 0)))
        .and_then(|phandle| fdt.find_by_phandle(phandle))
        .filter(|node| compatible_is(*node, COMPAT_IOMMU))
        .or_else(|| find_display_iommu(&fdt));

    let mut hdmi_nodes = [None; HDMI_COUNT];
    let mut phy_nodes = [None; PHY_COUNT];
    let mut hdmi_seen = 0usize;
    let mut phy_seen = 0usize;
    for node in fdt.nodes() {
        if compatible_is(node, COMPAT_HDMI) {
            if hdmi_seen < HDMI_COUNT {
                hdmi_nodes[hdmi_seen] = Some(node);
            }
            hdmi_seen += 1;
        } else if compatible_is(node, COMPAT_HDPTX_PHY) {
            if phy_seen < PHY_COUNT {
                phy_nodes[phy_seen] = Some(node);
            }
            phy_seen += 1;
        }
    }

    let (route, route_mutual) = routing_edges(&fdt, vop_node, hdmi_nodes);

    Some(Topology {
        vop: device(vop_node),
        vop_irq: vop_node.and_then(first_spi).unwrap_or(0),
        iommu: device(iommu_node),
        vop_iommu_link: link_matches(vop_node, b"iommus", iommu_node),
        hdmi: [device(hdmi_nodes[0]), device(hdmi_nodes[1])],
        hdmi_phy_link: [
            link_matches(hdmi_nodes[0], b"phys", phy_nodes[0]),
            link_matches(hdmi_nodes[1], b"phys", phy_nodes[1]),
        ],
        phy: [device(phy_nodes[0]), device(phy_nodes[1])],
        phy_grf: [phy_grf(&fdt, phy_nodes[0]), phy_grf(&fdt, phy_nodes[1])],
        route,
        route_mutual,
    })
}

/// Fallback when VOP itself is absent: the display IOMMU is the one at 0xfdd97e00. This keeps
/// the record useful on a tree whose VOP node is disabled or missing while the IOMMU remains.
fn find_display_iommu<'a>(fdt: &Fdt<'a>) -> Option<Node<'a>> {
    fdt.nodes().find(|node| {
        compatible_is(*node, COMPAT_IOMMU)
            && reg_regions(node.property(b"reg")).base == E_IOMMU_BASE
    })
}

fn compatible_is(node: Node<'_>, needle: &[u8]) -> bool {
    node.property(b"compatible")
        .is_some_and(|value| kumo_fdt::string_list_contains(value, needle))
}

fn device(node: Option<Node<'_>>) -> Device {
    let Some(node) = node else {
        return Device::ABSENT;
    };
    let regions = reg_regions(node.property(b"reg"));
    Device {
        present: true,
        enabled: status_enabled(node),
        regs: regions.regs,
        base: regions.base,
        len: regions.len,
        base1: regions.base1,
        len1: regions.len1,
        clocks: node
            .property(b"clock-names")
            .map(string_list_count)
            .unwrap_or(0)
            .min(usize::from(u8::MAX)) as u8,
        power_domains: power_domains(node.property(b"power-domains")),
        power_index: power_index(node.property(b"power-domains")).unwrap_or(NO_INDEX),
    }
}

struct Regions {
    regs: u8,
    base: u64,
    len: u64,
    base1: u64,
    len1: u64,
}

fn reg_regions(value: Option<&[u8]>) -> Regions {
    let Some(value) = value else {
        return Regions {
            regs: 0,
            base: 0,
            len: 0,
            base1: 0,
            len1: 0,
        };
    };
    let count = (value.len() / 16).min(2) as u8;
    let cell = |offset: usize| be32(value, offset).map(u64::from).unwrap_or(0);
    let region = |index: usize| -> (u64, u64) {
        let offset = index * 16;
        let base = (cell(offset) << 32) | cell(offset + 4);
        let len = (cell(offset + 8) << 32) | cell(offset + 12);
        (base, len)
    };
    let (base, len) = region(0);
    let (base1, len1) = if count >= 2 { region(1) } else { (0, 0) };
    Regions {
        regs: count,
        base,
        len,
        base1,
        len1,
    }
}

fn power_domains(value: Option<&[u8]>) -> u8 {
    value
        .map(|value| (value.len() / 8).min(usize::from(u8::MAX)) as u8)
        .unwrap_or(0)
}

fn power_index(value: Option<&[u8]>) -> Option<u32> {
    let value = value?;
    if value.len() < 8 {
        return None;
    }
    be32(value, 4)
}

fn node_phandle(node: Node<'_>) -> Option<u32> {
    node.property(b"phandle")
        .or_else(|| node.property(b"linux,phandle"))
        .and_then(|value| be32(value, 0))
}

fn link_matches(source: Option<Node<'_>>, property: &[u8], target: Option<Node<'_>>) -> bool {
    let (Some(source), Some(target)) = (source, target) else {
        return false;
    };
    source.property(property).and_then(|value| be32(value, 0)) == node_phandle(target)
}

fn phy_grf(fdt: &Fdt<'_>, node: Option<Node<'_>>) -> u64 {
    let Some(node) = node else {
        return 0;
    };
    let Some(phandle) = node
        .property(b"rockchip,grf")
        .and_then(|value| be32(value, 0))
    else {
        return 0;
    };
    let Some(grf) = fdt.find_by_phandle(phandle) else {
        return 0;
    };
    grf.property(b"reg")
        .map(|value| reg_regions(Some(value)).base)
        .unwrap_or(0)
}

/// GIC SPI number → INTID. RK3588's GIC declares four interrupt cells; the first two are the
/// type and number, and an SPI's INTID is its number plus 32. A non-SPI first interrupt is not
/// evidence this slice consumes, so it reads as absent. — TIMBERDOODLE
fn first_spi(node: Node<'_>) -> Option<u32> {
    let value = node.property(b"interrupts")?;
    if be32(value, 0)? != 0 {
        return None;
    }
    be32(value, 4)?.checked_add(32)
}

fn routing_edges(
    fdt: &Fdt<'_>,
    vop: Option<Node<'_>>,
    hdmi: [Option<Node<'_>>; HDMI_COUNT],
) -> ([u8; VOP_PORTS], u8) {
    let mut route = [0u8; VOP_PORTS];
    let mut mutual = 0u8;
    let Some(vop) = vop else {
        return (route, mutual);
    };
    let Some(ports) = vop.child(b"ports") else {
        return (route, mutual);
    };
    for port in ports.children() {
        if !port.name().starts_with(b"port") {
            continue;
        }
        let Some(index) = port_index(port) else {
            continue;
        };
        if index >= VOP_PORTS {
            continue;
        }
        let Some(endpoint) = port.child_named(b"endpoint") else {
            continue;
        };
        let Some(remote) = endpoint
            .property(b"remote-endpoint")
            .and_then(|value| be32(value, 0))
        else {
            continue;
        };
        for (hdmi_index, hdmi_node) in hdmi.iter().enumerate() {
            if !endpoint_has_phandle(*hdmi_node, remote) {
                continue;
            }
            route[index] = hdmi_index as u8 + 1;
            if let Some(endpoint_phandle) = node_phandle(endpoint) {
                let back = fdt
                    .find_by_phandle(remote)
                    .and_then(|node| node.property(b"remote-endpoint"))
                    .and_then(|value| be32(value, 0));
                if back == Some(endpoint_phandle) {
                    mutual |= 1u8 << index;
                }
            }
        }
    }
    (route, mutual)
}

fn port_index(port: Node<'_>) -> Option<usize> {
    if let Some(reg) = port.property(b"reg").and_then(|value| be32(value, 0)) {
        return Some(reg as usize);
    }
    unit_address(port.name()).map(|value| value as usize)
}

fn unit_address(name: &[u8]) -> Option<u32> {
    let at = name.iter().position(|byte| *byte == b'@')?;
    let mut value = 0u32;
    let digits = name.get(at + 1..)?;
    if digits.is_empty() {
        return None;
    }
    for digit in digits {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(u32::from(digit - b'0'))?;
    }
    Some(value)
}

fn endpoint_has_phandle(node: Option<Node<'_>>, phandle: u32) -> bool {
    let Some(node) = node else {
        return false;
    };
    let Some(ports) = node.child(b"ports") else {
        return false;
    };
    ports.children().any(|port| {
        port.name().starts_with(b"port")
            && port.children().any(|endpoint| {
                endpoint.name().starts_with(b"endpoint") && node_phandle(endpoint) == Some(phandle)
            })
    })
}

/// Facts RESEARCH/010 pins for the Orange Pi 5 Plus. These are DT-binding indices, not PMU
/// register bits: index 24 names PD_VOP and index 26 names PD_VO1. — TIMBERDOODLE
const E_VOP_BASE: u64 = 0xfdd9_0000;
const E_VOP_LEN: u64 = 0x4200;
const E_VOP_GAMMA_BASE: u64 = 0xfdd9_5000;
const E_VOP_GAMMA_LEN: u64 = 0x1000;
const E_VOP_IRQ: u32 = 188;
const E_VOP_CLOCKS: u8 = 9;
const E_VOP_PD: u32 = 24;
const E_IOMMU_BASE: u64 = 0xfdd9_7e00;
const E_IOMMU_LEN: u64 = 0x100;
const E_IOMMU_BASE1: u64 = 0xfdd9_7f00;
const E_IOMMU_LEN1: u64 = 0x100;
const E_IOMMU_CLOCKS: u8 = 2;
const E_IOMMU_PD: u32 = 24;
const E_HDMI_BASE: [u64; HDMI_COUNT] = [0xfde8_0000, 0xfdea_0000];
const E_HDMI_LEN: u64 = 0x2_0000;
const E_HDMI_CLOCKS: u8 = 6;
const E_HDMI_PD: u32 = 26;
const E_PHY_BASE: [u64; PHY_COUNT] = [0xfed6_0000, 0xfed7_0000];
const E_PHY_LEN: u64 = 0x2000;
const E_PHY_GRF: [u64; PHY_COUNT] = [0xfd5e_0000, 0xfd5e_4000];

pub const CHECK_COUNT: usize = 46;

/// One named comparison between the decoded tree and the pinned board facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Check {
    pub label: &'static str,
    pub ok: bool,
}

/// Compare every fact D0b depends on. Routing is deliberately absent here: a firmware topology
/// that disagrees with the measured scanout is valid data, not a malformed tree. — TIMBERDOODLE
pub fn checks(topology: &Topology) -> [Check; CHECK_COUNT] {
    let vop = topology.vop;
    let iommu = topology.iommu;
    let hdmi0 = topology.hdmi[0];
    let hdmi1 = topology.hdmi[1];
    let phy0 = topology.phy[0];
    let phy1 = topology.phy[1];
    [
        Check {
            label: "vop.present",
            ok: vop.present,
        },
        Check {
            label: "vop.enabled",
            ok: vop.enabled,
        },
        Check {
            label: "vop.reg.base",
            ok: vop.base == E_VOP_BASE,
        },
        Check {
            label: "vop.reg.len",
            ok: vop.len == E_VOP_LEN,
        },
        Check {
            label: "vop.gamma.base",
            ok: vop.base1 == E_VOP_GAMMA_BASE,
        },
        Check {
            label: "vop.gamma.len",
            ok: vop.len1 == E_VOP_GAMMA_LEN,
        },
        Check {
            label: "vop.irq",
            ok: topology.vop_irq == E_VOP_IRQ,
        },
        Check {
            label: "vop.clocks",
            ok: vop.clocks == E_VOP_CLOCKS,
        },
        Check {
            label: "vop.power_index",
            ok: vop.power_index == E_VOP_PD,
        },
        Check {
            label: "iommu.present",
            ok: iommu.present,
        },
        Check {
            label: "iommu.enabled",
            ok: iommu.enabled,
        },
        Check {
            label: "iommu.reg.base",
            ok: iommu.base == E_IOMMU_BASE,
        },
        Check {
            label: "iommu.reg.len",
            ok: iommu.len == E_IOMMU_LEN,
        },
        Check {
            label: "iommu.reg1.base",
            ok: iommu.base1 == E_IOMMU_BASE1,
        },
        Check {
            label: "iommu.reg1.len",
            ok: iommu.len1 == E_IOMMU_LEN1,
        },
        Check {
            label: "iommu.clocks",
            ok: iommu.clocks == E_IOMMU_CLOCKS,
        },
        Check {
            label: "iommu.power_index",
            ok: iommu.power_index == E_IOMMU_PD,
        },
        Check {
            label: "vop.iommus_link",
            ok: topology.vop_iommu_link,
        },
        Check {
            label: "hdmi0.present",
            ok: hdmi0.present,
        },
        Check {
            label: "hdmi0.enabled",
            ok: hdmi0.enabled,
        },
        Check {
            label: "hdmi0.reg.base",
            ok: hdmi0.base == E_HDMI_BASE[0],
        },
        Check {
            label: "hdmi0.reg.len",
            ok: hdmi0.len == E_HDMI_LEN,
        },
        Check {
            label: "hdmi0.clocks",
            ok: hdmi0.clocks == E_HDMI_CLOCKS,
        },
        Check {
            label: "hdmi0.power_index",
            ok: hdmi0.power_index == E_HDMI_PD,
        },
        Check {
            label: "hdmi0.phys_link",
            ok: topology.hdmi_phy_link[0],
        },
        Check {
            label: "hdmi1.present",
            ok: hdmi1.present,
        },
        Check {
            label: "hdmi1.enabled",
            ok: hdmi1.enabled,
        },
        Check {
            label: "hdmi1.reg.base",
            ok: hdmi1.base == E_HDMI_BASE[1],
        },
        Check {
            label: "hdmi1.reg.len",
            ok: hdmi1.len == E_HDMI_LEN,
        },
        Check {
            label: "hdmi1.clocks",
            ok: hdmi1.clocks == E_HDMI_CLOCKS,
        },
        Check {
            label: "hdmi1.power_index",
            ok: hdmi1.power_index == E_HDMI_PD,
        },
        Check {
            label: "hdmi1.phys_link",
            ok: topology.hdmi_phy_link[1],
        },
        Check {
            label: "phy0.present",
            ok: phy0.present,
        },
        Check {
            label: "phy0.enabled",
            ok: phy0.enabled,
        },
        Check {
            label: "phy0.reg.base",
            ok: phy0.base == E_PHY_BASE[0],
        },
        Check {
            label: "phy0.reg.len",
            ok: phy0.len == E_PHY_LEN,
        },
        Check {
            label: "phy0.grf",
            ok: topology.phy_grf[0] == E_PHY_GRF[0],
        },
        Check {
            label: "phy1.present",
            ok: phy1.present,
        },
        Check {
            label: "phy1.enabled",
            ok: phy1.enabled,
        },
        Check {
            label: "phy1.reg.base",
            ok: phy1.base == E_PHY_BASE[1],
        },
        Check {
            label: "phy1.reg.len",
            ok: phy1.len == E_PHY_LEN,
        },
        Check {
            label: "phy1.grf",
            ok: topology.phy_grf[1] == E_PHY_GRF[1],
        },
        Check {
            label: "route.vp0_hdmi0",
            ok: topology.route[0] == 1,
        },
        Check {
            label: "route.vp0_mutual",
            ok: topology.route_mutual & 1 != 0,
        },
        Check {
            label: "route.vp1_hdmi1",
            ok: topology.route[1] == 2,
        },
        Check {
            label: "route.vp1_mutual",
            ok: topology.route_mutual & 2 != 0,
        },
    ]
}

/// The comparison between the tree's wiring and D0a's measured scanout. `measured` is
/// `(HDMI output, VOP port)` from [`drv_fb::rk3588::Snapshot::matching_hdmi`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Routing {
    NoMeasurement,
    Agree {
        hdmi: usize,
        vop_port: usize,
    },
    Disagree {
        hdmi: usize,
        dt_vop_port: Option<usize>,
        measured_vop_port: usize,
    },
}

pub fn routing(topology: &Topology, measured: Option<(usize, usize)>) -> Routing {
    let Some((hdmi, measured_port)) = measured else {
        return Routing::NoMeasurement;
    };
    if hdmi >= HDMI_COUNT || measured_port >= VOP_PORTS {
        return Routing::NoMeasurement;
    }
    let dt_port = (0..VOP_PORTS).find(|port| topology.route[*port] == hdmi as u8 + 1);
    if dt_port == Some(measured_port) {
        Routing::Agree {
            hdmi,
            vop_port: measured_port,
        }
    } else {
        Routing::Disagree {
            hdmi,
            dt_vop_port: dt_port,
            measured_vop_port: measured_port,
        }
    }
}

/// D0b bootstrap record carried under tag `T`: the exact live-DTB extent and a transfer
/// checksum. `total == 0` means the board was ineligible or the blob did not validate; the
/// record is still sent so every board receives the same number of bootstrap messages.
/// — TIMBERDOODLE
pub const TRANSFER_TAG: u8 = b'T';
pub const TRANSFER_LEN: usize = 13;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transfer {
    pub total: u32,
    pub checksum: u64,
}

impl Transfer {
    pub const NONE: Self = Self {
        total: 0,
        checksum: 0,
    };

    pub fn encode(self) -> [u8; TRANSFER_LEN] {
        let mut bytes = [0u8; TRANSFER_LEN];
        bytes[0] = TRANSFER_TAG;
        bytes[1..5].copy_from_slice(&self.total.to_le_bytes());
        bytes[5..13].copy_from_slice(&self.checksum.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != TRANSFER_LEN || bytes[0] != TRANSFER_TAG {
            return None;
        }
        let total = u32::from_le_bytes(bytes[1..5].try_into().ok()?);
        if total != 0 && !(40..=MAX_DTB_BYTES as u32).contains(&total) {
            return None;
        }
        Some(Self {
            total,
            checksum: u64::from_le_bytes(bytes[5..13].try_into().ok()?),
        })
    }
}

/// FNV-1a over the transferred bytes. This detects a short or mangled copy across the
/// supervisor/driver boundary; it is not a security hash and does not authenticate firmware.
/// — TIMBERDOODLE
pub fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    const BOARD: &[u8] = include_bytes!("../../../../rk3588-orangepi-5-plus.dtb");

    #[test]
    fn tracked_board_decodes_to_the_pinned_facts() {
        let topology = decode(BOARD).expect("tracked board tree must decode");
        let failed: Vec<&str> = checks(&topology)
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.label)
            .collect();
        assert!(failed.is_empty(), "failed facts: {failed:?}");
        assert_eq!(topology.vop.base, 0xfdd9_0000);
        assert_eq!(topology.vop.base1, 0xfdd9_5000);
        assert_eq!(topology.vop_irq, 188);
        assert_eq!(topology.vop.power_index, 24);
        assert!(topology.vop_iommu_link);
        assert_eq!(topology.hdmi_phy_link, [true, true]);
        assert_eq!(topology.phy_grf, [0xfd5e_0000, 0xfd5e_4000]);
        assert_eq!(topology.route, [1, 2, 0, 0]);
        assert_eq!(topology.route_mutual, 0b11);
    }

    #[test]
    fn measured_routing_disagreement_is_data_not_a_check_failure() {
        let topology = decode(BOARD).unwrap();
        assert_eq!(
            routing(&topology, Some((0, 2))),
            Routing::Disagree {
                hdmi: 0,
                dt_vop_port: Some(0),
                measured_vop_port: 2,
            }
        );
        assert_eq!(
            routing(&topology, Some((0, 0))),
            Routing::Agree {
                hdmi: 0,
                vop_port: 0,
            }
        );
        assert_eq!(routing(&topology, None), Routing::NoMeasurement);
        assert!(checks(&topology).iter().all(|check| check.ok));
    }

    #[test]
    fn checks_name_the_exact_fact_that_flips() {
        let mut topology = decode(BOARD).unwrap();
        topology.vop.base = 0;
        let failed: Vec<&str> = checks(&topology)
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.label)
            .collect();
        assert_eq!(failed, vec!["vop.reg.base"]);
    }

    #[test]
    fn malformed_or_truncated_blobs_are_rejected() {
        assert!(decode(BOARD).is_some());

        let mut bad = BOARD.to_vec();
        bad[0] = 0;
        assert!(decode(&bad).is_none());

        let mut bad = BOARD.to_vec();
        bad[4..8].copy_from_slice(&0xffff_ffffu32.to_be_bytes());
        assert!(decode(&bad).is_none());

        let mut bad = BOARD.to_vec();
        bad[36..40].copy_from_slice(&0xffff_fffcu32.to_be_bytes());
        assert!(decode(&bad).is_none());

        assert!(decode(&BOARD[..BOARD.len() - 16]).is_none());
        assert!(decode(&[]).is_none());
    }

    #[test]
    fn transfer_record_roundtrips_and_rejects_bad_input() {
        let transfer = Transfer {
            total: 4096,
            checksum: 0xdead_beef_0123_4567,
        };
        assert_eq!(Transfer::decode(&transfer.encode()), Some(transfer));

        let mut bad = transfer.encode();
        bad[0] = b'X';
        assert_eq!(Transfer::decode(&bad), None);
        assert_eq!(Transfer::decode(&transfer.encode()[..12]), None);

        assert_eq!(
            Transfer::decode(&Transfer::NONE.encode()),
            Some(Transfer::NONE)
        );
        let mut short = Transfer::NONE.encode();
        short[1..5].copy_from_slice(&39u32.to_le_bytes());
        assert_eq!(Transfer::decode(&short), None);
    }

    #[test]
    fn checksum_is_fnv1a() {
        assert_eq!(checksum(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(checksum(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(checksum(b"ab"), checksum(b"ba"));
    }

    #[test]
    fn synthetic_graph_resolves_ports_phandles_and_status() {
        let bytes = synthetic();
        let topology = decode(&bytes).expect("synthetic tree must decode");
        assert_eq!(topology.vop.base, 0x1234_0000);
        assert_eq!(topology.vop.clocks, 2);
        assert_eq!(topology.vop.power_index, 7);
        assert_eq!(topology.vop_irq, 188);
        assert!(topology.vop_iommu_link);
        assert!(topology.hdmi_phy_link[0], "hdmi0 links phy0");
        assert_eq!(topology.phy_grf[0], 0xfd5e_0000);
        assert!(!topology.phy[1].enabled);
        // VOP port 2 (declared by reg, not by order) drives HDMI0; HDMI1 has no VOP link.
        assert_eq!(topology.route, [0, 0, 1, 0]);
        assert_eq!(topology.route_mutual, 1 << 2);
        assert_eq!(
            routing(&topology, Some((0, 2))),
            Routing::Agree {
                hdmi: 0,
                vop_port: 2,
            }
        );
    }

    /// Minimal FDT builder for graph-resolution tests. The decoder's structural reader is
    /// exercised separately in `kumo-fdt`; this only has to produce a valid tree.
    struct Tree {
        structs: Vec<u8>,
        strings: Vec<u8>,
    }

    impl Tree {
        fn new() -> Self {
            Self {
                structs: Vec::new(),
                strings: Vec::new(),
            }
        }
        fn token(&mut self, token: u32) {
            self.structs.extend_from_slice(&token.to_be_bytes());
        }
        fn begin(&mut self, name: &str) {
            self.token(kumo_fdt::FDT_BEGIN_NODE);
            self.structs.extend_from_slice(name.as_bytes());
            self.structs.push(0);
            while self.structs.len() % 4 != 0 {
                self.structs.push(0);
            }
        }
        fn end(&mut self) {
            self.token(kumo_fdt::FDT_END_NODE);
        }
        fn prop(&mut self, name: &str, value: &[u8]) {
            let offset = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            self.token(kumo_fdt::FDT_PROP);
            self.structs
                .extend_from_slice(&(value.len() as u32).to_be_bytes());
            self.structs.extend_from_slice(&offset.to_be_bytes());
            self.structs.extend_from_slice(value);
            while self.structs.len() % 4 != 0 {
                self.structs.push(0);
            }
        }
        fn prop_str(&mut self, name: &str, value: &str) {
            let mut bytes = Vec::from(value.as_bytes());
            bytes.push(0);
            self.prop(name, &bytes);
        }
        fn prop_u32(&mut self, name: &str, value: u32) {
            self.prop(name, &value.to_be_bytes());
        }
        fn finish(mut self) -> Vec<u8> {
            self.token(kumo_fdt::FDT_END);
            const HEADER: usize = 40;
            let total = HEADER + self.structs.len() + self.strings.len();
            let mut out = vec![0u8; total];
            out[0..4].copy_from_slice(&kumo_fdt::FDT_MAGIC.to_be_bytes());
            out[4..8].copy_from_slice(&(total as u32).to_be_bytes());
            out[8..12].copy_from_slice(&(HEADER as u32).to_be_bytes());
            out[12..16].copy_from_slice(&((HEADER + self.structs.len()) as u32).to_be_bytes());
            out[16..20].copy_from_slice(&(HEADER as u32).to_be_bytes());
            out[20..24].copy_from_slice(&17u32.to_be_bytes());
            out[24..28].copy_from_slice(&16u32.to_be_bytes());
            out[32..36].copy_from_slice(&(self.strings.len() as u32).to_be_bytes());
            out[36..40].copy_from_slice(&(self.structs.len() as u32).to_be_bytes());
            out[HEADER..HEADER + self.structs.len()].copy_from_slice(&self.structs);
            out[HEADER + self.structs.len()..].copy_from_slice(&self.strings);
            out
        }
    }

    fn reg(base: u32, len: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&base.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(&len.to_be_bytes());
        bytes
    }

    fn synthetic() -> Vec<u8> {
        let mut tree = Tree::new();
        tree.begin("");
        tree.prop_str("compatible", "rockchip,rk3588");

        tree.begin("vop@12340000");
        tree.prop_str("compatible", "rockchip,rk3588-vop");
        tree.prop("reg", &reg(0x1234_0000, 0x1000));
        tree.prop_u32("phandle", 0x10);
        tree.prop(
            "interrupts",
            &[0, 0, 0, 0, 0, 0, 0, 0x9c, 0, 0, 0, 4, 0, 0, 0, 0],
        );
        tree.prop("clock-names", b"\0aclk\0dclk\0");
        tree.prop("power-domains", &[0, 0, 0, 0x30, 0, 0, 0, 7]);
        tree.prop_u32("iommus", 0x20);
        tree.begin("ports");
        tree.begin("port@2");
        tree.prop_u32("reg", 2);
        tree.begin("endpoint@5");
        tree.prop_u32("reg", 5);
        tree.prop_u32("remote-endpoint", 0x41);
        tree.prop_u32("phandle", 0x40);
        tree.end();
        tree.end();
        tree.end();
        tree.end();

        tree.begin("iommu@12350000");
        tree.prop_str("compatible", "rockchip,rk3588-iommu");
        tree.prop("reg", &reg(0x1235_0000, 0x100));
        tree.prop_u32("phandle", 0x20);
        tree.prop("clock-names", b"\0aclk\0");
        tree.prop("power-domains", &[0, 0, 0, 0x30, 0, 0, 0, 7]);
        tree.end();

        tree.begin("hdmi@12360000");
        tree.prop_str("compatible", "rockchip,rk3588-dw-hdmi-qp");
        tree.prop("reg", &reg(0x1236_0000, 0x1000));
        tree.prop("clock-names", b"\0pclk\0ref\0");
        tree.prop("power-domains", &[0, 0, 0, 0x30, 0, 0, 0, 8]);
        tree.prop_u32("phys", 0x60);
        tree.prop_u32("phandle", 0x50);
        tree.begin("ports");
        tree.begin("port@0");
        tree.prop_u32("reg", 0);
        tree.begin("endpoint");
        tree.prop_u32("remote-endpoint", 0x40);
        tree.prop_u32("phandle", 0x41);
        tree.end();
        tree.end();
        tree.end();
        tree.end();

        tree.begin("hdmi@12370000");
        tree.prop_str("compatible", "rockchip,rk3588-dw-hdmi-qp");
        tree.prop("reg", &reg(0x1237_0000, 0x1000));
        tree.end();

        tree.begin("phy@12380000");
        tree.prop_str("compatible", "rockchip,rk3588-hdptx-phy");
        tree.prop("reg", &reg(0x1238_0000, 0x1000));
        tree.prop_u32("phandle", 0x60);
        tree.prop_u32("rockchip,grf", 0x70);
        tree.end();

        tree.begin("phy@12390000");
        tree.prop_str("compatible", "rockchip,rk3588-hdptx-phy");
        tree.prop("reg", &reg(0x1239_0000, 0x1000));
        tree.prop_str("status", "disabled");
        tree.end();

        tree.begin("syscon@fd5e0000");
        tree.prop("reg", &reg(0xfd5e_0000, 0x100));
        tree.prop_u32("phandle", 0x70);
        tree.end();

        tree.end();
        tree.finish()
    }
}
