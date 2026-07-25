//j482
//j483

//! ARM SMMUv3 scaffolding and RK3588 MMU600 integration-status discovery.
//!
//! RK3588 integration facts come from the vendored RK3588 TRM Part 1, chapters 1, 2, 7, and 8.
//! This slice reads only documented PMU status and CRU clock-gate/software-reset controls. It does
//! not dereference either MMU600 aperture, enable an SMMU, configure queues, bind streams, or imply
//! that a `DeviceCtx` exists.

use core::fmt;

const SMMU_IDR0: usize = 0x0;
const SMMU_CR0: usize = 0x20;
const SMMU_STRTAB_BASE: usize = 0x80;
const SMMU_STRTAB_BASE_CFG: usize = 0x88;
const SMMU_CMDQ_BASE: usize = 0x90;
const SMMU_CMDQ_PROD: usize = 0x98;
const SMMU_CMDQ_CONS: usize = 0x9C;
const SMMU_EVENTQ_BASE: usize = 0xA0;
const SMMU_EVENTQ_PROD: usize = 0xA8;
const SMMU_EVENTQ_CONS: usize = 0xAC;

const CR0_SMMUEN: u32 = 1 << 0;
const IOMMU_KIND_SMMUV3: u32 = 2;
const PAGE_SIZE: u64 = 4096;
const DMA_RIGHTS_READ: u32 = 1 << 2;
const DMA_RIGHTS_WRITE: u32 = 1 << 3;
const EVTQ_0_ID_MASK: u64 = 0xff;
const EVTQ_0_SSV: u64 = 1 << 11;
const EVTQ_0_SSID_SHIFT: u64 = 12;
const EVTQ_0_SSID_MASK: u64 = 0x000f_ffff;
const EVTQ_0_SID_SHIFT: u64 = 32;
const EVT_ID_TRANSLATION: u8 = 0x10;
const EVT_ID_ADDR_SIZE: u8 = 0x11;
const EVT_ID_ACCESS: u8 = 0x12;
const EVT_ID_PERMISSION: u8 = 0x13;

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;
const MAX_FDT_DEPTH: usize = 32;

/// RK3588 TRM Part 1 chapter 1 register aperture for `MMU600_PCIE`.
pub const RK3588_MMU600_PCIE_BASE: u64 = 0xfc90_0000;
pub const RK3588_MMU600_PCIE_LENGTH: u64 = 0x20_0000;

/// RK3588 PMU operational status registers used to observe the MMU600 integration state.
///
/// Chapter 7 defines `PMU_PWR_GATE_STS0.PD_PHP` at bit 21 and
/// `PMU_SUBMEM_PWR_GATE_STS.PCIEMMU` at bit 7, plus the PCIe-MMU TCU/TBU Q-channel fields in
/// `PMU_QCHANNEL_PWR_STS`. All selected fields are read-only. — KESTREL
pub const RK3588_PMU_PWR_GATE_STS0: u64 = 0xfd8d_8180;
pub const RK3588_PMU_SUBMEM_PWR_GATE_STS: u64 = 0xfd8d_81bc;
pub const RK3588_PMU_QCHANNEL_PWR_STS: u64 = 0xfd8d_81d8;
const RK3588_PD_PHP_DOWN: u32 = 1 << 21;
const RK3588_PCIEMMU_MEMORY_DOWN: u32 = 1 << 7;

/// RK3588 always-on CRU controls observed without changing firmware state.
///
/// Chapter 2 defines `CRU_GATE_CON34` bits 9/7 as the MMU-BIU/PCIe-MMU software gates
/// (high disables the clock), and `CRU_SOFTRST_CON34` bits 9/7 as their software reset
/// requests (high asserts reset). A read reports only these controls: it cannot prove that
/// a clock is toggling, that every reset source is inactive, or that the MMU APB path admits
/// accesses. — KESTREL
pub const RK3588_CRU_GATE_CON34: u64 = 0xfd7c_0888;
pub const RK3588_CRU_SOFTRST_CON34: u64 = 0xfd7c_0a88;
const RK3588_ACLK_MMU_PCIE_DISABLED: u32 = 1 << 7;
const RK3588_ACLK_MMU_BIU_DISABLED: u32 = 1 << 9;
const RK3588_MMU_PCIE_SOFTWARE_RESET: u32 = 1 << 7;
const RK3588_MMU_BIU_SOFTWARE_RESET: u32 = 1 << 9;

/// The enabled RK3588 PCIe SMMU register window selected from the board DTB.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mmu600PcieTopology {
    pub base: u64,
    pub length: u64,
}

/// Read-only RK3588 integration status sampled without touching the MMU600 aperture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mmu600PciePowerStatus {
    pub pwr_gate_sts0: u32,
    pub submem_pwr_gate_sts: u32,
    pub qchannel_pwr_sts: u32,
}

impl Mmu600PciePowerStatus {
    pub const fn php_domain_powered(self) -> bool {
        self.pwr_gate_sts0 & RK3588_PD_PHP_DOWN == 0
    }

    pub const fn pcie_mmu_memory_powered(self) -> bool {
        self.submem_pwr_gate_sts & RK3588_PCIEMMU_MEMORY_DOWN == 0
    }

    /// Whether the two selected power-down observations both read as up. This is not a guarantee
    /// that the MMU's clock/reset/APB path is accessible.
    pub const fn observed_power_up(self) -> bool {
        self.php_domain_powered() && self.pcie_mmu_memory_powered()
    }

    /// TCU:TBU request bits, packed as the TRM presents them (`bit1:bit0`).
    pub const fn pcie_mmu_qchannel_request(self) -> u32 {
        (self.qchannel_pwr_sts >> 23) & 0b11
    }

    /// TCU:TBU active bits.
    pub const fn pcie_mmu_qchannel_active(self) -> u32 {
        (self.qchannel_pwr_sts >> 16) & 0b11
    }

    /// TCU:TBU deny bits.
    pub const fn pcie_mmu_qchannel_deny(self) -> u32 {
        (self.qchannel_pwr_sts >> 9) & 0b11
    }

    /// TCU:TBU accept bits.
    pub const fn pcie_mmu_qchannel_accept(self) -> u32 {
        (self.qchannel_pwr_sts >> 2) & 0b11
    }
}

/// Snapshot produced by read-only observation of the RK3588 CRU controls associated with
/// `MMU600_PCIE`; the underlying control fields remain read/write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mmu600PcieControlStatus {
    pub gate_con34: u32,
    pub softrst_con34: u32,
}

impl Mmu600PcieControlStatus {
    pub const fn pcie_clock_gate_open(self) -> bool {
        self.gate_con34 & RK3588_ACLK_MMU_PCIE_DISABLED == 0
    }

    pub const fn biu_clock_gate_open(self) -> bool {
        self.gate_con34 & RK3588_ACLK_MMU_BIU_DISABLED == 0
    }

    pub const fn pcie_software_reset_clear(self) -> bool {
        self.softrst_con34 & RK3588_MMU_PCIE_SOFTWARE_RESET == 0
    }

    pub const fn biu_software_reset_clear(self) -> bool {
        self.softrst_con34 & RK3588_MMU_BIU_SOFTWARE_RESET == 0
    }
}

/// Result of the target wrapper's PMU/CRU integration-status observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mmu600PcieStatusReport {
    pub topology: Mmu600PcieTopology,
    pub power: Mmu600PciePowerStatus,
    pub control: Mmu600PcieControlStatus,
}

impl fmt::Display for Mmu600PcieStatusReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let observed = if self.power.observed_power_up() {
            "up"
        } else {
            "gated"
        };
        write!(
            formatter,
            "MMU600 PCIE       Check     {:#x} pwr={:#x} mem={:#x} qch={:#x} \
             req={:#x} act={:#x} deny={:#x} accept={:#x} observed={} status-only   OK\n",
            self.topology.base,
            self.power.pwr_gate_sts0,
            self.power.submem_pwr_gate_sts,
            self.power.qchannel_pwr_sts,
            self.power.pcie_mmu_qchannel_request(),
            self.power.pcie_mmu_qchannel_active(),
            self.power.pcie_mmu_qchannel_deny(),
            self.power.pcie_mmu_qchannel_accept(),
            observed
        )?;
        write!(
            formatter,
            "MMU600 CONTROL    Check     gate={:#x} softrst={:#x} pcie-gate={} biu-gate={} \
             pcie-rst-req={} biu-rst-req={} control-only   OK\n",
            self.control.gate_con34,
            self.control.softrst_con34,
            if self.control.pcie_clock_gate_open() {
                "open"
            } else {
                "closed"
            },
            if self.control.biu_clock_gate_open() {
                "open"
            } else {
                "closed"
            },
            if self.control.pcie_software_reset_clear() {
                "clear"
            } else {
                "asserted"
            },
            if self.control.biu_software_reset_clear() {
                "clear"
            } else {
                "asserted"
            },
        )
    }
}

/// Select only the enabled `arm,smmu-v3` PCIe MMU600 from an RK3588 device tree.
///
/// `MMU600_PHP` is deliberately rejected: its tracked node is disabled and it does not serve the
/// USB3OTG_0 controller probed by j481.
pub fn discover_rk3588_mmu600_pcie(dtb: &[u8]) -> Option<Mmu600PcieTopology> {
    discover_rk3588_smmuv3(dtb, RK3588_MMU600_PCIE_BASE, RK3588_MMU600_PCIE_LENGTH)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmmuFaultEvent {
    pub event_id: u8,
    pub stream_id: u32,
    pub substream_id: Option<u32>,
    pub fault_record: u64,
    pub fault_addr: u64,
}

pub fn iommu_init(kind: u32, _phys_base: u64, _len: u64) -> bool {
    if kind != IOMMU_KIND_SMMUV3 {
        return false;
    }
    // TODO: Actually program SMMUv3 global registers here
    true
}

pub fn iommu_create_device_context(
    kind: u32,
    _phys_base: u64,
    _stream_id: u32,
    _pgd_phys: u64,
) -> bool {
    if kind != IOMMU_KIND_SMMUV3 {
        return false;
    }
    // TODO: Program STE and CD
    true
}

pub fn iommu_destroy_device_context(_kind: u32, _phys_base: u64, _stream_id: u32) {
    // TODO: Invalidate STE
}

pub fn iommu_map_device_page(
    kind: u32,
    _phys_base: u64,
    _stream_id: u32,
    pgd_phys: u64,
    iova: u64,
    phys: u64,
    rights: u32,
) -> bool {
    if kind != IOMMU_KIND_SMMUV3 {
        return false;
    }
    device_page_request_is_valid(pgd_phys, iova)
        && phys != 0
        && page_aligned(phys)
        && dma_rights_valid(rights)
}

pub fn iommu_unmap_device_range(
    kind: u32,
    _phys_base: u64,
    _stream_id: u32,
    pgd_phys: u64,
    iova: u64,
    len: u64,
) -> bool {
    if kind != IOMMU_KIND_SMMUV3 {
        return false;
    }
    device_page_request_is_valid(pgd_phys, iova) && len != 0 && page_aligned(len)
}

pub fn decode_smmuv3_fault_event(words: [u64; 4]) -> Option<SmmuFaultEvent> {
    let event_id = (words[0] & EVTQ_0_ID_MASK) as u8;
    if !smmuv3_event_is_device_fault(event_id) {
        return None;
    }
    let stream_id = (words[0] >> EVTQ_0_SID_SHIFT) as u32;
    let substream_id = if words[0] & EVTQ_0_SSV != 0 {
        Some(((words[0] >> EVTQ_0_SSID_SHIFT) & EVTQ_0_SSID_MASK) as u32)
    } else {
        None
    };
    Some(SmmuFaultEvent {
        event_id,
        stream_id,
        substream_id,
        fault_record: words[0],
        fault_addr: words[2],
    })
}

const fn page_aligned(value: u64) -> bool {
    value & (PAGE_SIZE - 1) == 0
}

const fn dma_rights_valid(rights: u32) -> bool {
    let allowed = DMA_RIGHTS_READ | DMA_RIGHTS_WRITE;
    rights != 0 && rights & !allowed == 0
}

const fn device_page_request_is_valid(pgd_phys: u64, iova: u64) -> bool {
    pgd_phys != 0 && page_aligned(pgd_phys) && iova != 0 && page_aligned(iova)
}

const fn smmuv3_event_is_device_fault(event_id: u8) -> bool {
    matches!(
        event_id,
        EVT_ID_TRANSLATION | EVT_ID_ADDR_SIZE | EVT_ID_ACCESS | EVT_ID_PERMISSION
    )
}

#[derive(Clone, Copy)]
struct FdtNode {
    compatible_smmuv3: bool,
    reg: [u32; 4],
    reg_len: u8,
    status_enabled: bool,
}

impl FdtNode {
    const fn empty() -> Self {
        Self {
            compatible_smmuv3: false,
            reg: [0; 4],
            reg_len: 0,
            status_enabled: true,
        }
    }
}

fn discover_rk3588_smmuv3(
    dtb: &[u8],
    expected_base: u64,
    expected_length: u64,
) -> Option<Mmu600PcieTopology> {
    if be32(dtb, 0)? != FDT_MAGIC {
        return None;
    }
    let total = be32(dtb, 4)? as usize;
    let dtb = dtb.get(..total)?;
    let struct_off = be32(dtb, 8)? as usize;
    let strings_off = be32(dtb, 12)? as usize;
    let strings_len = be32(dtb, 32)? as usize;
    let struct_len = be32(dtb, 36)? as usize;
    let structures = dtb.get(struct_off..struct_off.checked_add(struct_len)?)?;
    let strings = dtb.get(strings_off..strings_off.checked_add(strings_len)?)?;

    let mut stack = [FdtNode::empty(); MAX_FDT_DEPTH];
    let mut depth = 0usize;
    let mut cursor = 0usize;
    let mut root_rk3588 = false;
    let mut candidate = None;

    while cursor < structures.len() {
        let token = be32(structures, cursor)?;
        cursor += 4;
        match token {
            FDT_BEGIN_NODE => {
                if depth == MAX_FDT_DEPTH {
                    return None;
                }
                let end = structures
                    .get(cursor..)?
                    .iter()
                    .position(|byte| *byte == 0)?
                    + cursor;
                stack[depth] = FdtNode::empty();
                depth += 1;
                cursor = align4(end.checked_add(1)?)?;
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return None;
                }
                let node = stack[depth - 1];
                if node.compatible_smmuv3 && node.status_enabled && node.reg_len >= 4 {
                    let base = cells64(node.reg[0], node.reg[1]);
                    let length = cells64(node.reg[2], node.reg[3]);
                    if base == expected_base && length == expected_length {
                        candidate = Some(Mmu600PcieTopology { base, length });
                    }
                }
                depth -= 1;
            }
            FDT_PROP => {
                if depth == 0 {
                    return None;
                }
                let len = be32(structures, cursor)? as usize;
                let name_off = be32(structures, cursor + 4)? as usize;
                cursor += 8;
                let value = structures.get(cursor..cursor.checked_add(len)?)?;
                cursor = align4(cursor.checked_add(len)?)?;
                let name = property_name(strings, name_off)?;
                let node = &mut stack[depth - 1];
                match name {
                    b"compatible" => {
                        if depth == 1 && string_list_contains(value, b"rockchip,rk3588") {
                            root_rk3588 = true;
                        }
                        node.compatible_smmuv3 = string_list_contains(value, b"arm,smmu-v3");
                    }
                    b"reg" => {
                        let count = (value.len() / 4).min(node.reg.len());
                        for (index, slot) in node.reg.iter_mut().take(count).enumerate() {
                            *slot = be32(value, index * 4)?;
                        }
                        node.reg_len = count as u8;
                    }
                    b"status" => {
                        node.status_enabled = string_list_contains(value, b"ok")
                            || string_list_contains(value, b"okay");
                    }
                    _ => {}
                }
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => return None,
        }
    }

    if root_rk3588 {
        candidate
    } else {
        None
    }
}

const fn cells64(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

fn property_name(strings: &[u8], offset: usize) -> Option<&[u8]> {
    let end = strings.get(offset..)?.iter().position(|byte| *byte == 0)? + offset;
    strings.get(offset..end)
}

fn string_list_contains(list: &[u8], needle: &[u8]) -> bool {
    list.split(|byte| *byte == 0).any(|entry| entry == needle)
}

fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_be_bytes(bytes.get(offset..end)?.try_into().ok()?))
}

fn align4(value: usize) -> Option<usize> {
    value.checked_add(3).map(|value| value & !3)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::format;

    const IOMMU_KIND_VIRTIO: u32 = 1;
    const OPI5_DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    const X13S_DTB: &[u8] = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");

    #[test]
    fn discovers_only_the_enabled_rk3588_pcie_mmu600() {
        assert_eq!(
            discover_rk3588_mmu600_pcie(OPI5_DTB),
            Some(Mmu600PcieTopology {
                base: RK3588_MMU600_PCIE_BASE,
                length: RK3588_MMU600_PCIE_LENGTH,
            })
        );
        assert_eq!(
            discover_rk3588_smmuv3(OPI5_DTB, 0xfcb0_0000, 0x20_0000),
            None,
            "the tracked PHP MMU is disabled"
        );
        assert_eq!(discover_rk3588_mmu600_pcie(X13S_DTB), None);
        assert_eq!(discover_rk3588_mmu600_pcie(b"not a dtb"), None);
        assert_eq!(discover_rk3588_mmu600_pcie(&OPI5_DTB[..32]), None);
    }

    #[test]
    fn rk3588_power_status_decodes_observed_power_and_qchannels() {
        let powered = Mmu600PciePowerStatus {
            pwr_gate_sts0: 0,
            submem_pwr_gate_sts: 0,
            qchannel_pwr_sts: (0b10 << 23) | (0b01 << 16) | (0b11 << 9) | (0b01 << 2),
        };
        assert!(powered.observed_power_up());
        assert_eq!(powered.pcie_mmu_qchannel_request(), 0b10);
        assert_eq!(powered.pcie_mmu_qchannel_active(), 0b01);
        assert_eq!(powered.pcie_mmu_qchannel_deny(), 0b11);
        assert_eq!(powered.pcie_mmu_qchannel_accept(), 0b01);
        assert!(!Mmu600PciePowerStatus {
            pwr_gate_sts0: RK3588_PD_PHP_DOWN,
            ..powered
        }
        .observed_power_up());
        assert!(!Mmu600PciePowerStatus {
            submem_pwr_gate_sts: RK3588_PCIEMMU_MEMORY_DOWN,
            ..powered
        }
        .observed_power_up());
    }

    #[test]
    fn rk3588_control_status_decodes_only_software_gate_and_reset_bits() {
        let open = Mmu600PcieControlStatus {
            gate_con34: 0,
            softrst_con34: 0,
        };
        assert!(open.pcie_clock_gate_open());
        assert!(open.biu_clock_gate_open());
        assert!(open.pcie_software_reset_clear());
        assert!(open.biu_software_reset_clear());

        let pcie_only = Mmu600PcieControlStatus {
            gate_con34: RK3588_ACLK_MMU_PCIE_DISABLED,
            softrst_con34: RK3588_MMU_PCIE_SOFTWARE_RESET,
        };
        assert!(!pcie_only.pcie_clock_gate_open());
        assert!(pcie_only.biu_clock_gate_open());
        assert!(!pcie_only.pcie_software_reset_clear());
        assert!(pcie_only.biu_software_reset_clear());

        let biu_only = Mmu600PcieControlStatus {
            gate_con34: RK3588_ACLK_MMU_BIU_DISABLED,
            softrst_con34: RK3588_MMU_BIU_SOFTWARE_RESET,
        };
        assert!(biu_only.pcie_clock_gate_open());
        assert!(!biu_only.biu_clock_gate_open());
        assert!(biu_only.pcie_software_reset_clear());
        assert!(!biu_only.biu_software_reset_clear());
    }

    #[test]
    fn status_report_renders_raw_power_qchannel_and_control_evidence() {
        let topology = Mmu600PcieTopology {
            base: RK3588_MMU600_PCIE_BASE,
            length: RK3588_MMU600_PCIE_LENGTH,
        };
        let power = Mmu600PciePowerStatus {
            pwr_gate_sts0: 0,
            submem_pwr_gate_sts: 0,
            qchannel_pwr_sts: 0,
        };
        let control = Mmu600PcieControlStatus {
            gate_con34: 0,
            softrst_con34: 0,
        };
        let line = format!(
            "{}",
            Mmu600PcieStatusReport {
                topology,
                power,
                control,
            }
        );
        assert!(line.contains("pwr=0x0 mem=0x0 qch=0x0"));
        assert!(line.contains("req=0x0 act=0x0 deny=0x0 accept=0x0"));
        assert!(line.contains("observed=up status-only   OK\n"));
        assert!(line.ends_with(
            "gate=0x0 softrst=0x0 pcie-gate=open biu-gate=open \
             pcie-rst-req=clear biu-rst-req=clear control-only   OK\n"
        ));

        let gated = format!(
            "{}",
            Mmu600PcieStatusReport {
                topology,
                power: Mmu600PciePowerStatus {
                    pwr_gate_sts0: RK3588_PD_PHP_DOWN,
                    submem_pwr_gate_sts: RK3588_PCIEMMU_MEMORY_DOWN,
                    qchannel_pwr_sts: 0,
                },
                control: Mmu600PcieControlStatus {
                    gate_con34: RK3588_ACLK_MMU_PCIE_DISABLED | RK3588_ACLK_MMU_BIU_DISABLED,
                    softrst_con34: RK3588_MMU_PCIE_SOFTWARE_RESET | RK3588_MMU_BIU_SOFTWARE_RESET,
                },
            }
        );
        assert!(gated.contains("pwr=0x200000 mem=0x80 qch=0x0"));
        assert!(gated.contains("observed=gated status-only   OK\n"));
        assert!(gated.ends_with(
            "gate=0x280 softrst=0x280 pcie-gate=closed biu-gate=closed \
             pcie-rst-req=asserted biu-rst-req=asserted control-only   OK\n"
        ));
    }

    #[test]
    fn smmuv3_init_accepts_smmuv3_kind() {
        assert!(iommu_init(IOMMU_KIND_SMMUV3, 0x1500_0000, 0x20_0000));
    }

    #[test]
    fn smmuv3_init_rejects_non_smmuv3_kind() {
        assert!(!iommu_init(IOMMU_KIND_VIRTIO, 0x1500_0000, 0x20_0000));
    }

    #[test]
    fn device_context_creation_uses_smmuv3_kind() {
        assert!(iommu_create_device_context(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000
        ));
        assert!(!iommu_create_device_context(
            IOMMU_KIND_VIRTIO,
            0x1500_0000,
            0x42,
            0x8000_0000
        ));
    }

    #[test]
    fn device_page_mapping_validates_smmuv3_request_shape() {
        assert!(iommu_map_device_page(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            0x9000_0000,
            DMA_RIGHTS_READ | DMA_RIGHTS_WRITE,
        ));
        assert!(!iommu_map_device_page(
            IOMMU_KIND_VIRTIO,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            0x9000_0000,
            DMA_RIGHTS_READ,
        ));
        assert!(!iommu_map_device_page(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0,
            0x4000_0000,
            0x9000_0000,
            DMA_RIGHTS_READ,
        ));
        assert!(!iommu_map_device_page(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0123,
            0x9000_0000,
            DMA_RIGHTS_READ,
        ));
        assert!(!iommu_map_device_page(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            0x9000_0123,
            DMA_RIGHTS_READ,
        ));
        assert!(!iommu_map_device_page(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            0x9000_0000,
            0,
        ));
    }

    #[test]
    fn device_range_unmap_validates_smmuv3_request_shape() {
        assert!(iommu_unmap_device_range(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            PAGE_SIZE * 2,
        ));
        assert!(!iommu_unmap_device_range(
            IOMMU_KIND_VIRTIO,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            PAGE_SIZE,
        ));
        assert!(!iommu_unmap_device_range(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0,
            PAGE_SIZE,
        ));
        assert!(!iommu_unmap_device_range(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            0,
        ));
        assert!(!iommu_unmap_device_range(
            IOMMU_KIND_SMMUV3,
            0x1500_0000,
            0x42,
            0x8000_0000,
            0x4000_0000,
            PAGE_SIZE + 1,
        ));
    }

    #[test]
    fn decodes_smmuv3_translation_fault_event() {
        let words = [
            (0x42_u64 << EVTQ_0_SID_SHIFT)
                | (0x12345_u64 << EVTQ_0_SSID_SHIFT)
                | EVTQ_0_SSV
                | EVT_ID_TRANSLATION as u64,
            0,
            0xdead_beef_cafe_f000,
            0,
        ];

        assert_eq!(
            decode_smmuv3_fault_event(words),
            Some(SmmuFaultEvent {
                event_id: EVT_ID_TRANSLATION,
                stream_id: 0x42,
                substream_id: Some(0x12345),
                fault_record: words[0],
                fault_addr: 0xdead_beef_cafe_f000,
            })
        );
    }

    #[test]
    fn decodes_smmuv3_permission_fault_without_substream() {
        let words = [
            (0x84_u64 << EVTQ_0_SID_SHIFT) | EVT_ID_PERMISSION as u64,
            0,
            0x4000_1000,
            0,
        ];

        assert_eq!(
            decode_smmuv3_fault_event(words),
            Some(SmmuFaultEvent {
                event_id: EVT_ID_PERMISSION,
                stream_id: 0x84,
                substream_id: None,
                fault_record: words[0],
                fault_addr: 0x4000_1000,
            })
        );
    }

    #[test]
    fn decodes_only_device_address_fault_events() {
        for event_id in [EVT_ID_ADDR_SIZE, EVT_ID_ACCESS] {
            let words = [
                (0x21_u64 << EVTQ_0_SID_SHIFT) | event_id as u64,
                0,
                0x8000_0000,
                0,
            ];
            assert_eq!(
                decode_smmuv3_fault_event(words).map(|event| event.event_id),
                Some(event_id)
            );
        }

        let config_fault = [(0x21_u64 << EVTQ_0_SID_SHIFT) | 0x02, 0, 0x8000_0000, 0];
        assert_eq!(decode_smmuv3_fault_event(config_fault), None);
    }
}
