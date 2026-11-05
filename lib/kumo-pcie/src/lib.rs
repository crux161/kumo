#![no_std]
#![deny(unsafe_code)]
//j792
//j793
//j794
//j798

//! RK3588 DesignWare PCIe: board discovery and the read-only firmware-state probe (PLAN/019 NV0).
//!
//! The M.2 slot of the Orange Pi 5 Plus hangs off **PCIe3_4L**, the dual-mode DWC core at APB
//! `0xfe150000` (RESEARCH/018 §2). This crate finds that controller in the board DTB and reads
//! what firmware left behind, in a fixed ladder: PMU power and bus-idle state, then CRU clock and
//! reset controls, then the PHY GRF, the APB client, and the DBI. Each rung reads its block only
//! when the preceding documented power, clock and reset preconditions hold. A DBI read
//! while the PHY PLL is off can stall the AXI bus (TRM §11.6.1.1); the ladder exists so that
//! cannot happen. Nothing here writes a register, and nothing reads through the config, MEM or
//! I/O windows: the endpoint and NVMe registers are NV1/NV2.
//!
//! Register access goes through [`RegisterRead`], so host tests can prove that a failed rung
//! never reaches a later block. — TIMBERDOODLE

use core::fmt;

use kumo_fdt::{be32, status_enabled, string_list_contains, string_value, Fdt, Node};

pub mod endpoint;
pub mod nvme;

/// APB client window of PCIe3_4L (TRM Table 11-1), which selects the M.2 controller.
pub const PCIE30X4_APB: u64 = 0xfe15_0000;
/// The DBI window the DTB names, above 4 GiB.
pub const PCIE30X4_DBI: u64 = 0xa_4000_0000;
/// TRM Table 11-1's second DBI window for PCIe3_4L, inside the 32-bit device map.
pub const PCIE30X4_DBI_ALIAS: u64 = 0xf500_0000;
/// Both DBI windows are 4 MiB.
pub const PCIE30X4_DBI_LEN: u64 = 0x40_0000;

/// PMU (TRM Part 1 ch. 7). Power-gate status bits read 1 while a domain is down.
pub const PMU_PWR_GATE_STS0: u64 = 0xfd8d_8180;
pub const PMU_BIU_IDLE_SFTCON1: u64 = 0xfd8d_8110;
pub const PMU_BIU_IDLE_ACK_STS0: u64 = 0xfd8d_8118;
pub const PMU_BIU_IDLE_STS0: u64 = 0xfd8d_8120;
const PD_PHP_DOWN: u32 = 1 << 21;
const PD_PCIE_DOWN: u32 = 1 << 23;
/// `idle_req_php_sftena` in SFTCON1; `idle_ack_php` / `idle_php` in ACK_STS0 / STS0.
const BIU_PHP_REQUEST: u32 = 1 << 5;
const BIU_PHP: u32 = 1 << 21;

/// CRU and PHPTOPCRU (TRM Part 1 ch. 2). A set gate bit disables a clock; a set reset bit holds
/// the logic in reset.
pub const CRU_GATE_CON32: u64 = 0xfd7c_0880;
pub const CRU_GATE_CON33: u64 = 0xfd7c_0884;
pub const CRU_GATE_CON34: u64 = 0xfd7c_0888;
pub const CRU_GATE_CON39: u64 = 0xfd7c_089c;
pub const CRU_SOFTRST_CON32: u64 = 0xfd7c_0a80;
pub const CRU_SOFTRST_CON33: u64 = 0xfd7c_0a84;
pub const CRU_SOFTRST_CON34: u64 = 0xfd7c_0a88;
pub const PHPTOPCRU_GATE_CON00: u64 = 0xfd7c_8800;
pub const PHPTOPCRU_SOFTRST_CON00: u64 = 0xfd7c_8a00;
const ACLK_PCIE_4L_DBI: u32 = 1 << 13; // GATE_CON32
const ACLK_PCIE_BIU: u32 = 1 << 12; // GATE_CON32
const ACLK_PHP_ROOT: u32 = 1 << 7; // GATE_CON32
const PCLK_PHP_BIU: u32 = 1 << 5; // GATE_CON32
const PCLK_PHP_ROOT: u32 = 1 << 0; // GATE_CON32
const ACLK_PCIE_4L_MSTR: u32 = 1 << 2; // GATE_CON33
const ACLK_PCIE_4L_SLV: u32 = 1 << 7; // GATE_CON33
const PCLK_PCIE_4L: u32 = 1 << 12; // GATE_CON33
const CLK_PCIE_AUX0: u32 = 1 << 1; // GATE_CON34
const CLK_PCIE_4L_PIPE: u32 = 1 << 0; // GATE_CON39
const PCLK_PCIE3_PHY: u32 = 1 << 8; // PHPTOPCRU_GATE_CON00
const RESETN_PCIE_4L_POWER_UP: u32 = 1 << 13; // SOFTRST_CON32, resets to 1
const ARESETN_PCIE_BIU: u32 = 1 << 12; // SOFTRST_CON32
const ARESETN_PHP_BIU: u32 = 1 << 9; // SOFTRST_CON32
const PRESETN_PHP_BIU: u32 = 1 << 5; // SOFTRST_CON32
const PRESETN_PCIE_4L: u32 = 1 << 12; // SOFTRST_CON33
const RESETN_PCIE3_PHY: u32 = 1 << 10; // PHPTOPCRU_SOFTRST_CON00
const PRESETN_PCIE3_PHY: u32 = 1 << 8; // PHPTOPCRU_SOFTRST_CON00

/// PCIe3PHY_GRF (TRM Part 1 §6.19).
pub const PCIE3PHY_GRF: u64 = 0xfd5b_8000;
const PHY_CMN_CON0: u64 = 0x000;
const PHY0_STATUS1: u64 = 0x904;
const PHY1_STATUS1: u64 = 0xa04;
const PHY_CLAMP_RELEASED: u32 = 1 << 8;
const PHY_SRAM_INIT_DONE: u32 = 1 << 0;
const PHY_MPLLA_STATE: u32 = 1 << 3;
const PHY_MPLLB_STATE: u32 = 1 << 6;
const PHY_POWER_ENABLED: u32 = (1 << 1) | (1 << 2);

/// APB client registers (TRM §11.4.2).
const CLIENT_GENERAL_CON: u64 = 0x000;
const CLIENT_INTR_STATUS_MISC: u64 = 0x010;
const CLIENT_INTR_MASK_LEGACY: u64 = 0x01c;
const CLIENT_INTR_MASK_MISC: u64 = 0x024;
const CLIENT_POWER_CON: u64 = 0x02c;
const CLIENT_HOT_RESET_CTRL: u64 = 0x180;
const CLIENT_AXI_SLV_ATU_BYPASS: u64 = 0x204;
const CLIENT_AXI_SLV_MISC_INFO: u64 = 0x218;
const CLIENT_LTSSM_STATUS: u64 = 0x300;
const CLIENT_VER: u64 = 0x800;

/// DBI offsets: the root port's Type 1 header, PCIe capability and port logic (TRM §11.4.3).
const DBI_ID: u64 = 0x000;
const DBI_COMMAND: u64 = 0x004;
const DBI_CLASS: u64 = 0x008;
const DBI_BUS: u64 = 0x018;
const DBI_MEM: u64 = 0x020;
const DBI_PREF: u64 = 0x024;
const DBI_PREF_BASE_UPPER: u64 = 0x028;
const DBI_PREF_LIMIT_UPPER: u64 = 0x02c;
const DBI_LINK_CAP: u64 = 0x07c;
const DBI_LINK_CONTROL_STATUS: u64 = 0x080;
const DBI_PORT_LINK_CTRL: u64 = 0x710;
const DBI_PL_DEBUG0: u64 = 0x728;
const DBI_PL_DEBUG1: u64 = 0x72c;
const DBI_GEN2_CTRL: u64 = 0x80c;
const DBI_MISC_CONTROL_1: u64 = 0x8bc;
const DBI_AMBA_ERROR_RESPONSE: u64 = 0x8d0;
const DBI_VERSION_NUMBER: u64 = 0x8f8;
const DBI_VERSION_TYPE: u64 = 0x8fc;
/// `VERSION_NUMBER`'s reset value: ASCII "560*", DWC core 5.60a.
pub const DWC_VERSION_560A: u32 = 0x3536_302a;
/// The unrolled iATU: region `i` at `+0x200 * i`, outbound at `+0`, inbound at `+0x100`.
const DBI_ATU: u64 = 0x30_0000;
/// PCIe3_4L implements 16 outbound and 16 inbound regions (TRM §11.1).
pub const ATU_REGIONS: usize = 16;
const ATU_REGION_ENABLE: u32 = 1 << 31;

const GIC_SPI: u32 = 0;
const DBI_PROBE_LENGTH: u64 = DBI_ATU + 0x200 * ATU_REGIONS as u64;

/// Minimum read-only CPU windows for the firmware observer. Controller windows
/// still require the kernel's live admission guard before mapping. — TIMBERDOODLE
pub const STATUS_WINDOWS: [(u64, u64); 7] = [
    (PMU_PWR_GATE_STS0 & !4095, 4096),
    (CRU_GATE_CON32 & !4095, 4096),
    (PHPTOPCRU_GATE_CON00 & !4095, 4096),
    (PCIE3PHY_GRF, 4096),
    (PCIE30X4_APB, 4096),
    (PCIE30X4_DBI_ALIAS, 4096),
    (PCIE30X4_DBI_ALIAS + DBI_ATU, 0x2000),
];

/// Register reads for the probe. The target reads physical MMIO; tests supply a fake.
pub trait RegisterRead {
    fn read32(&mut self, phys: u64) -> u32;
}

/// One GPIO line from the DTB: the bank's register base and the pin within it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpioLine {
    pub bank: u64,
    pub pin: u32,
    pub active_low: bool,
}

/// The five wired interrupts, as GIC SPI numbers in DT order (INTID = SPI + 32).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Spis {
    pub sys: u32,
    pub pmc: u32,
    pub msg: u32,
    pub legacy: u32,
    pub err: u32,
}

/// The enabled PCIe3_4L node, decoded from the board DTB.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Topology {
    pub dbi: u64,
    pub dbi_len: u64,
    pub apb: u64,
    pub apb_len: u64,
    pub config: u64,
    pub config_len: u64,
    pub lanes: u32,
    pub max_link_speed: u32,
    pub spis: Spis,
    pub power_domain: u32,
    pub perst: Option<GpioLine>,
    pub supply: Option<GpioLine>,
    pub bus_range: [u8; 2],
    /// Parent/child address windows, in I/O, MEM32, MEM64 order.
    pub windows: [Option<PciWindow>; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciWindow {
    pub cpu: u64,
    pub pci: u64,
    pub bytes: u64,
}

impl Topology {
    /// The DBI window the probe may read through. The kernel physmap ends at the top of RAM
    /// (4 GiB on the measured board), so a DTB window above 4 GiB is read through the TRM's
    /// 32-bit alias. Only this controller's two documented windows are accepted.
    pub const fn dbi_window(&self) -> Option<u64> {
        if self.dbi_len < DBI_PROBE_LENGTH || self.dbi_len > PCIE30X4_DBI_LEN {
            None
        } else if self.dbi == PCIE30X4_DBI || self.dbi == PCIE30X4_DBI_ALIAS {
            Some(PCIE30X4_DBI_ALIAS)
        } else {
            None
        }
    }

    // A DT node cannot redirect the RK3588-specific reader to another device, even when
    // its APB address still names PCIe3_4L. — TIMBERDOODLE
    fn valid_for_probe(&self) -> bool {
        self.apb == PCIE30X4_APB
            && (CLIENT_VER + 4..=0x1_0000).contains(&self.apb_len)
            && self.dbi_window().is_some()
            && self.config & 3 == 0
            && self.config_len != 0
            && self.config.checked_add(self.config_len).is_some()
            && (1..=4).contains(&self.lanes)
            && (1..=3).contains(&self.max_link_speed)
            && self.power_domain == 34
    }
}

/// Find the enabled `rockchip,rk3588-pcie` controller whose APB client is [`PCIE30X4_APB`].
/// `None` for any other board, a disabled node, or a node whose windows do not decode.
pub fn discover_rk3588_pcie30x4(dtb: &[u8]) -> Option<Topology> {
    let fdt = Fdt::new(dtb)?;
    let root = fdt.root()?;
    if cells(root, b"#address-cells")? != 2 || cells(root, b"#size-cells")? != 2 {
        return None;
    }
    let node = root.children().find(|node| {
        node.property(b"compatible")
            .is_some_and(|list| string_list_contains(list, b"rockchip,rk3588-pcie"))
            && status_enabled(*node)
            && named_reg(*node, b"apb").is_some_and(|(base, _)| base == PCIE30X4_APB)
    })?;
    if cells(node, b"#address-cells")? != 3 || cells(node, b"#size-cells")? != 2 {
        return None;
    }
    let (dbi, dbi_len) = named_reg(node, b"dbi")?;
    let (apb, apb_len) = named_reg(node, b"apb")?;
    let (config, config_len) = named_reg(node, b"config")?;
    let lanes = be32(node.property(b"num-lanes")?, 0)?;
    let max_link_speed = be32(node.property(b"max-link-speed")?, 0)?;
    let domain = node.property(b"power-domains")?;
    let provider = fdt.find_by_phandle(be32(domain, 0)?)?;
    if domain.len() != 8
        || cells(provider, b"#power-domain-cells")? != 1
        || !status_enabled(provider)
        || !string_list_contains(
            provider.property(b"compatible")?,
            b"rockchip,rk3588-power-controller",
        )
    {
        return None;
    }
    let power_domain = be32(domain, 4)?;
    let buses = node.property(b"bus-range")?;
    if buses.len() != 8 {
        return None;
    }
    let bus_range = [
        u8::try_from(be32(buses, 0)?).ok()?,
        u8::try_from(be32(buses, 4)?).ok()?,
    ];
    if bus_range[0] >= bus_range[1] {
        return None;
    }
    let ranges = node.property(b"ranges")?;
    if ranges.len() % 28 != 0 {
        return None;
    }
    let mut windows = [None; 3];
    for at in (0..ranges.len()).step_by(28) {
        let flags = be32(ranges, at)?;
        let space = ((flags >> 24) & 3) as usize;
        if space == 0 || flags & 0x00ff_ffff != 0 || windows[space - 1].is_some() {
            return None;
        }
        let pair = |offset| {
            Some(
                (u64::from(be32(ranges, at + offset)?) << 32)
                    | u64::from(be32(ranges, at + offset + 4)?),
            )
        };
        let window = PciWindow {
            pci: pair(4)?,
            cpu: pair(12)?,
            bytes: pair(20)?,
        };
        if window.bytes == 0
            || window.cpu.checked_add(window.bytes).is_none()
            || window.pci.checked_add(window.bytes).is_none()
        {
            return None;
        }
        windows[space - 1] = Some(window);
    }
    let spis = Spis {
        sys: named_spi(&fdt, root, node, b"sys")?,
        pmc: named_spi(&fdt, root, node, b"pmc")?,
        msg: named_spi(&fdt, root, node, b"msg")?,
        legacy: named_spi(&fdt, root, node, b"legacy")?,
        err: named_spi(&fdt, root, node, b"err")?,
    };
    let perst = node
        .property(b"reset-gpios")
        .and_then(|spec| gpio_line(&fdt, spec, None));
    let supply = node
        .property(b"vpcie3v3-supply")
        .and_then(|value| be32(value, 0))
        .and_then(|phandle| fdt.find_by_phandle(phandle))
        .and_then(|regulator| {
            let active_high = regulator.property(b"enable-active-high").is_some();
            gpio_line(&fdt, regulator.property(b"gpios")?, Some(!active_high))
        });
    let topology = Topology {
        dbi,
        dbi_len,
        apb,
        apb_len,
        config,
        config_len,
        lanes,
        max_link_speed,
        spis,
        power_domain,
        perst,
        supply,
        bus_range,
        windows,
    };
    topology.valid_for_probe().then_some(topology)
}

fn cells(node: Node<'_>, name: &[u8]) -> Option<u32> {
    be32(node.property(name)?, 0)
}

/// The `reg` entry whose `reg-names` entry is `name`, with two address and two size cells.
fn named_reg(node: Node<'_>, name: &[u8]) -> Option<(u64, u64)> {
    let index = string_index(node.property(b"reg-names")?, name)?;
    let reg = node.property(b"reg")?;
    let at = index.checked_mul(16)?;
    let word = |offset: usize| -> Option<u64> {
        let high = u64::from(be32(reg, at.checked_add(offset)?)?);
        let low = u64::from(be32(reg, at.checked_add(offset)?.checked_add(4)?)?);
        Some((high << 32) | low)
    };
    let (base, len) = (word(0)?, word(8)?);
    (len != 0 && base & 3 == 0 && base.checked_add(len).is_some()).then_some((base, len))
}

/// The GIC SPI number of the interrupt named `name`, resolving the interrupt parent's cell count.
fn named_spi(fdt: &Fdt<'_>, root: Node<'_>, node: Node<'_>, name: &[u8]) -> Option<u32> {
    let index = string_index(node.property(b"interrupt-names")?, name)?;
    let parent = node
        .property(b"interrupt-parent")
        .or_else(|| root.property(b"interrupt-parent"))
        .and_then(|value| be32(value, 0))
        .and_then(|phandle| fdt.find_by_phandle(phandle))?;
    let width = cells(parent, b"#interrupt-cells")? as usize;
    if !matches!(width, 3 | 4) || !status_enabled(parent) {
        return None;
    }
    let spec = node.property(b"interrupts")?;
    let at = index.checked_mul(width)?.checked_mul(4)?;
    if be32(spec, at)? != GIC_SPI
        || be32(spec, at.checked_add(8)?)? != 4
        || (width == 4 && be32(spec, at.checked_add(12)?)? != 0)
    {
        return None;
    }
    let spi = be32(spec, at.checked_add(4)?)?;
    (spi < 988).then_some(spi)
}

/// `<&gpioN pin flags>`. A fixed regulator states its polarity with `enable-active-high`, so
/// `polarity` overrides the GPIO flag when given.
fn gpio_line(fdt: &Fdt<'_>, spec: &[u8], polarity: Option<bool>) -> Option<GpioLine> {
    let bank = fdt.find_by_phandle(be32(spec, 0)?)?;
    bank.property(b"gpio-controller")?;
    if spec.len() != 12 || cells(bank, b"#gpio-cells")? != 2 || !status_enabled(bank) {
        return None;
    }
    let reg = bank.property(b"reg")?;
    if reg.len() != 16 {
        return None;
    }
    let base = (u64::from(be32(reg, 0)?) << 32) | u64::from(be32(reg, 4)?);
    let pin = be32(spec, 4)?;
    if pin >= 32 {
        return None;
    }
    Some(GpioLine {
        bank: base,
        pin,
        active_low: polarity.unwrap_or(be32(spec, 8)? & 1 != 0),
    })
}

fn string_index(list: &[u8], needle: &[u8]) -> Option<usize> {
    string_value(list)
        .split(|byte| *byte == 0)
        .position(|entry| entry == needle)
}

/// PMU words, in the order the report prints them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Power {
    pub pwr_gate_sts0: u32,
    pub biu_idle_sftcon1: u32,
    pub biu_idle_ack_sts0: u32,
    pub biu_idle_sts0: u32,
}

impl Power {
    pub const fn php_up(&self) -> bool {
        self.pwr_gate_sts0 & PD_PHP_DOWN == 0
    }

    pub const fn pcie_up(&self) -> bool {
        self.pwr_gate_sts0 & PD_PCIE_DOWN == 0
    }

    pub const fn biu_requested(&self) -> bool {
        self.biu_idle_sftcon1 & BIU_PHP_REQUEST != 0
    }

    pub const fn biu_acknowledged(&self) -> bool {
        self.biu_idle_ack_sts0 & BIU_PHP != 0
    }

    pub const fn biu_idle(&self) -> bool {
        self.biu_idle_sts0 & BIU_PHP != 0
    }
}

/// CRU and PHPTOPCRU control words.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Clock {
    pub gate32: u32,
    pub gate33: u32,
    pub gate34: u32,
    pub gate39: u32,
    pub softrst32: u32,
    pub softrst33: u32,
    pub softrst34: u32,
    pub php_gate0: u32,
    pub php_softrst0: u32,
}

impl Clock {
    pub const fn client_clocked(&self) -> bool {
        self.gate33 & PCLK_PCIE_4L == 0 && self.gate32 & (PCLK_PHP_ROOT | PCLK_PHP_BIU) == 0
    }

    pub const fn client_released(&self) -> bool {
        self.softrst33 & PRESETN_PCIE_4L == 0 && self.softrst32 & PRESETN_PHP_BIU == 0
    }

    pub const fn phy_grf_clocked(&self) -> bool {
        self.php_gate0 & PCLK_PCIE3_PHY == 0
    }

    pub const fn phy_released(&self) -> bool {
        self.php_softrst0 & (RESETN_PCIE3_PHY | PRESETN_PCIE3_PHY) == 0
    }

    pub const fn dbi_clocked(&self) -> bool {
        self.gate32 & (ACLK_PCIE_4L_DBI | ACLK_PHP_ROOT | ACLK_PCIE_BIU) == 0
    }

    pub const fn pipe_clocked(&self) -> bool {
        self.gate39 & CLK_PCIE_4L_PIPE == 0
    }

    pub const fn core_released(&self) -> bool {
        self.softrst32 & (RESETN_PCIE_4L_POWER_UP | ARESETN_PCIE_BIU | ARESETN_PHP_BIU) == 0
    }

    pub const fn axi_clocked(&self) -> bool {
        self.gate33 & (ACLK_PCIE_4L_MSTR | ACLK_PCIE_4L_SLV) == 0
    }

    pub const fn aux_clocked(&self) -> bool {
        self.gate34 & CLK_PCIE_AUX0 == 0
    }
}

/// PCIe3PHY_GRF words.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Phy {
    pub cmn_con0: u32,
    pub phy0_status1: u32,
    pub phy1_status1: u32,
}

impl Phy {
    pub const fn mode(&self) -> u32 {
        self.cmn_con0 & 0b111
    }

    pub const fn clamp_released(&self) -> bool {
        self.cmn_con0 & PHY_CLAMP_RELEASED != 0
    }

    pub const fn sram_ready(&self) -> (bool, bool) {
        (
            self.phy0_status1 & PHY_SRAM_INIT_DONE != 0,
            self.phy1_status1 & PHY_SRAM_INIT_DONE != 0,
        )
    }

    pub const fn pll_on(&self) -> (bool, bool) {
        (
            self.phy0_status1 & PHY_MPLLA_STATE != 0,
            self.phy1_status1 & PHY_MPLLA_STATE != 0,
        )
    }

    pub const fn mpllb_on(&self) -> (bool, bool) {
        (
            self.phy0_status1 & PHY_MPLLB_STATE != 0,
            self.phy1_status1 & PHY_MPLLB_STATE != 0,
        )
    }

    /// Both PHYs report initialized SRAM, powered PCS/PMA and an active MPLL. An all-ones read is not
    /// evidence of a working PHY. — TIMBERDOODLE
    pub const fn running(&self) -> bool {
        let (sram0, sram1) = self.sram_ready();
        let (pll0, pll1) = self.pll_on();
        let (pllb0, pllb1) = self.mpllb_on();
        self.phy0_status1 != u32::MAX
            && self.phy1_status1 != u32::MAX
            && self.phy0_status1 & PHY_POWER_ENABLED == PHY_POWER_ENABLED
            && self.phy1_status1 & PHY_POWER_ENABLED == PHY_POWER_ENABLED
            && sram0
            && sram1
            && (pll0 || pllb0)
            && (pll1 || pllb1)
    }

    fn mode_name(&self) -> &'static str {
        match self.mode() {
            4..=7 => "x4",
            0 => "2x2",
            1 => "1+1/x2",
            2 => "x2/1+1",
            _ => "1+1/1+1",
        }
    }
}

/// APB client words.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Client {
    pub general_con: u32,
    pub intr_status_misc: u32,
    pub intr_mask_legacy: u32,
    pub intr_mask_misc: u32,
    pub power_con: u32,
    pub hot_reset_ctrl: u32,
    pub axi_slv_atu_bypass: u32,
    pub axi_slv_misc_info: u32,
    pub ltssm_status: u32,
    pub version: u32,
}

impl Client {
    pub const fn ltssm_state(&self) -> u32 {
        self.ltssm_status & 0x3f
    }

    pub const fn smlh_link_up(&self) -> bool {
        self.ltssm_status & (1 << 16) != 0
    }

    pub const fn rdlh_link_up(&self) -> bool {
        self.ltssm_status & (1 << 17) != 0
    }

    /// P1's `link_up`: the PHY and the data link layer both report up.
    pub const fn link_up(&self) -> bool {
        self.smlh_link_up() && self.rdlh_link_up()
    }

    pub const fn rate(&self) -> u32 {
        (self.ltssm_status >> 20) & 0b111
    }

    pub const fn device_type(&self) -> u32 {
        (self.general_con >> 4) & 0xf
    }

    pub const fn ltssm_enabled(&self) -> bool {
        self.general_con & (1 << 2) != 0
    }
}

/// The name of an LTSSM state code (TRM §11.7.1).
pub const fn ltssm_name(state: u32) -> &'static str {
    match state {
        0x00 => "DETECT_QUIET",
        0x01 => "DETECT_ACT",
        0x02 => "POLL_ACTIVE",
        0x03 => "POLL_COMPLIANCE",
        0x04 => "POLL_CONFIG",
        0x05 => "PRE_DETECT_QUIET",
        0x06 => "DETECT_WAIT",
        0x07 => "CFG_LINKWD_START",
        0x08 => "CFG_LINKWD_ACEPT",
        0x09 => "CFG_LANENUM_WAIT",
        0x0a => "CFG_LANENUM_ACEPT",
        0x0b => "CFG_COMPLETE",
        0x0c => "CFG_IDLE",
        0x0d => "RCVRY_LOCK",
        0x0e => "RCVRY_SPEED",
        0x0f => "RCVRY_RCVRCFG",
        0x10 => "RCVRY_IDLE",
        0x11 => "L0",
        0x12 => "L0S",
        0x13 => "L123_SEND_EIDLE",
        0x14 => "L1_IDLE",
        0x15 => "L2_IDLE",
        0x16 => "L2_WAKE",
        0x17 => "DISABLED_ENTRY",
        0x18 => "DISABLED_IDLE",
        0x19 => "DISABLED",
        0x1a => "LPBK_ENTRY",
        0x1b => "LPBK_ACTIVE",
        0x1c => "LPBK_EXIT",
        0x1d => "LPBK_EXIT_TIMEOUT",
        0x1e => "HOT_RESET_ENTRY",
        0x1f => "HOT_RESET",
        0x20 => "RCVRY_EQ0",
        0x21 => "RCVRY_EQ1",
        0x22 => "RCVRY_EQ2",
        0x23 => "RCVRY_EQ3",
        _ => "UNKNOWN",
    }
}

/// A link rate code as GT/s text: LTSSM_STATUS[22:20] counts from 0, the PCIe Link Status and
/// Capabilities speed fields from 1.
const fn rate_name(generation: u32) -> &'static str {
    match generation {
        1 => "2.5",
        2 => "5.0",
        3 => "8.0",
        4 => "16.0",
        5 => "32.0",
        _ => "?",
    }
}

/// One iATU region's words (RESEARCH/018 §3).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AtuRegion {
    pub ctrl1: u32,
    pub ctrl2: u32,
    pub lower_base: u32,
    pub upper_base: u32,
    pub limit: u32,
    pub lower_target: u32,
    pub upper_target: u32,
    pub upper_limit: u32,
}

impl AtuRegion {
    pub const fn enabled(&self) -> bool {
        self.ctrl2 & ATU_REGION_ENABLE != 0
    }

    pub const fn tlp_type(&self) -> u32 {
        self.ctrl1 & 0x1f
    }

    pub const fn base(&self) -> u64 {
        ((self.upper_base as u64) << 32) | self.lower_base as u64
    }

    pub const fn limit(&self) -> u64 {
        // Without INCREASE_REGION_SIZE, LIMIT supplies only the low half and the upper
        // address is inherited from BASE (TRM §11.4.3.25). — TIMBERDOODLE
        let upper = if self.ctrl1 & (1 << 13) != 0 {
            self.upper_limit
        } else {
            self.upper_base
        };
        ((upper as u64) << 32) | self.limit as u64
    }

    pub const fn target(&self) -> u64 {
        ((self.upper_target as u64) << 32) | self.lower_target as u64
    }

    fn type_name(&self) -> &'static str {
        match self.tlp_type() {
            0 => "mem",
            2 => "io",
            4 => "cfg0",
            5 => "cfg1",
            0x10..=0x17 => "msg",
            _ => "other",
        }
    }

    /// First BDF in a configuration region. CFG_SHIFT_MODE takes bits [27:12] from
    /// the untranslated CPU address, not TARGET (TRM §11.4.3.25). — TIMBERDOODLE
    pub const fn config_bdf(&self) -> Option<(u8, u8, u8)> {
        if !matches!(self.tlp_type(), 4 | 5) {
            return None;
        }
        let bdf = if self.ctrl2 & (1 << 28) != 0 {
            self.lower_base >> 12
        } else {
            self.lower_target >> 16
        };
        Some(((bdf >> 8) as u8, ((bdf >> 3) & 0x1f) as u8, (bdf & 7) as u8))
    }
}

/// The root port's DBI words.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Dbi {
    pub window: u64,
    pub version: u32,
    pub version_type: u32,
    pub id: u32,
    pub command: u32,
    pub class: u32,
    pub bus: u32,
    pub mem: u32,
    pub pref: u32,
    pub pref_base_upper: u32,
    pub pref_limit_upper: u32,
    pub link_cap: u32,
    pub link_control_status: u32,
    pub port_link_ctrl: u32,
    pub pl_debug0: u32,
    pub pl_debug1: u32,
    pub gen2_ctrl: u32,
    pub misc_control_1: u32,
    pub amba_error_response: u32,
    pub outbound: [AtuRegion; ATU_REGIONS],
    pub inbound: [AtuRegion; ATU_REGIONS],
}

impl Dbi {
    pub const fn link_cap(&self) -> (u32, u32) {
        (self.link_cap & 0xf, (self.link_cap >> 4) & 0x3f)
    }

    /// Current speed and width from Link Status, the upper half of DBI `0x80`.
    pub const fn link_status(&self) -> (u32, u32) {
        let status = self.link_control_status >> 16;
        (status & 0xf, (status >> 4) & 0x3f)
    }

    pub const fn data_link_active(&self) -> bool {
        self.link_control_status & (1 << 29) != 0
    }

    pub const fn training(&self) -> bool {
        self.link_control_status & (1 << 27) != 0
    }

    /// Both non-posted-error and absent-vendor-ID responses return all-ones instead of
    /// an AXI error. CRS completions have their own independent field. — TIMBERDOODLE
    pub const fn failed_reads_return_ones(&self) -> bool {
        self.amba_error_response & 0b101 == 0
    }

    pub const fn crs_response(&self) -> &'static str {
        match (self.amba_error_response >> 3) & 3 {
            0 => "okay",
            1 => "ones",
            2 => "vendor-retry",
            _ => "axi-error",
        }
    }

    pub fn enabled_regions(&self) -> (usize, usize) {
        (
            self.outbound.iter().filter(|r| r.enabled()).count(),
            self.inbound.iter().filter(|r| r.enabled()).count(),
        )
    }

    /// Non-prefetchable memory window: Memory Base/Limit hold address bits [31:20].
    pub const fn memory_window(&self) -> (u64, u64) {
        let base = ((self.mem & 0xfff0) as u64) << 16;
        let limit = (((self.mem >> 16) & 0xfff0) as u64) << 16 | 0xf_ffff;
        (base, limit)
    }

    pub const fn prefetch_window(&self) -> (u64, u64) {
        let base = (((self.pref & 0xfff0) as u64) << 16) | ((self.pref_base_upper as u64) << 32);
        let limit = ((((self.pref >> 16) & 0xfff0) as u64) << 16)
            | 0xf_ffff
            | ((self.pref_limit_upper as u64) << 32);
        (base, limit)
    }
}

/// The first precondition a rung found unmet; every later block is left unread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stop {
    InvalidTopology,
    PhpDown,
    PcieDown,
    BiuIdle,
    ClientClockGated,
    ClientInReset,
    PhyGrfClockGated,
    PhyInReset,
    DbiClockGated,
    PipeClockGated,
    CoreInReset,
    PhyNotRunning,
    PhyClamped,
    DbiAboveMap,
    DbiUnverified,
}

impl Stop {
    pub const fn reason(self) -> &'static str {
        match self {
            Self::InvalidTopology => "unsupported or truncated controller windows/topology",
            Self::PhpDown => "PD_PHP down",
            Self::PcieDown => "PD_PCIE down",
            Self::BiuIdle => "BIU_PHP idle requested/acked/idle",
            Self::ClientClockGated => "pclk_pcie_4l/php root/biu gated",
            Self::ClientInReset => "presetn_pcie_4l/php_biu asserted",
            Self::PhyGrfClockGated => "pclk_pcie3_phy gated",
            Self::PhyInReset => "pcie3 phy reset asserted",
            Self::DbiClockGated => "aclk_pcie_4l_dbi/php root/pcie biu gated",
            Self::PipeClockGated => "clk_pcie_4l_pipe gated",
            Self::CoreInReset => "power-up/biu reset asserted",
            Self::PhyNotRunning => "phy sram/mplla not ready",
            Self::PhyClamped => "phy outputs clamped",
            Self::DbiAboveMap => "dbi above physmap, no alias",
            Self::DbiUnverified => "dbi version mismatch",
        }
    }
}

/// Everything NV0 reads, and where it stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Report {
    pub topology: Topology,
    pub power: Power,
    pub clock: Clock,
    pub phy: Option<Phy>,
    pub client: Option<Client>,
    pub dbi: Option<Dbi>,
    pub stop: Option<Stop>,
}

/// Run the guard ladder of RESEARCH/018 §9 against `topology`. Reads only; each block is read
/// only after every rung before it holds.
pub fn probe(topology: Topology, io: &mut impl RegisterRead) -> Report {
    if !topology.valid_for_probe() {
        return Report {
            topology,
            power: Power::default(),
            clock: Clock::default(),
            phy: None,
            client: None,
            dbi: None,
            stop: Some(Stop::InvalidTopology),
        };
    }
    let power = Power {
        pwr_gate_sts0: io.read32(PMU_PWR_GATE_STS0),
        biu_idle_sftcon1: io.read32(PMU_BIU_IDLE_SFTCON1),
        biu_idle_ack_sts0: io.read32(PMU_BIU_IDLE_ACK_STS0),
        biu_idle_sts0: io.read32(PMU_BIU_IDLE_STS0),
    };
    let clock = Clock {
        gate32: io.read32(CRU_GATE_CON32),
        gate33: io.read32(CRU_GATE_CON33),
        gate34: io.read32(CRU_GATE_CON34),
        gate39: io.read32(CRU_GATE_CON39),
        softrst32: io.read32(CRU_SOFTRST_CON32),
        softrst33: io.read32(CRU_SOFTRST_CON33),
        softrst34: io.read32(CRU_SOFTRST_CON34),
        php_gate0: io.read32(PHPTOPCRU_GATE_CON00),
        php_softrst0: io.read32(PHPTOPCRU_SOFTRST_CON00),
    };
    let mut report = Report {
        topology,
        power,
        clock,
        phy: None,
        client: None,
        dbi: None,
        stop: None,
    };
    report.stop = ladder(&mut report, io).err();
    report
}

fn ladder<I: RegisterRead>(report: &mut Report, io: &mut I) -> Result<(), Stop> {
    let (power, clock) = (report.power, report.clock);
    require(power.php_up(), Stop::PhpDown)?;
    require(power.pcie_up(), Stop::PcieDown)?;
    require(
        !(power.biu_requested() || power.biu_acknowledged() || power.biu_idle()),
        Stop::BiuIdle,
    )?;

    require(clock.phy_grf_clocked(), Stop::PhyGrfClockGated)?;
    require(clock.phy_released(), Stop::PhyInReset)?;
    let phy = Phy {
        cmn_con0: io.read32(PCIE3PHY_GRF + PHY_CMN_CON0),
        phy0_status1: io.read32(PCIE3PHY_GRF + PHY0_STATUS1),
        phy1_status1: io.read32(PCIE3PHY_GRF + PHY1_STATUS1),
    };
    report.phy = Some(phy);

    require(clock.client_clocked(), Stop::ClientClockGated)?;
    require(clock.client_released(), Stop::ClientInReset)?;
    let apb = report.topology.apb;
    report.client = Some(Client {
        general_con: io.read32(apb + CLIENT_GENERAL_CON),
        intr_status_misc: io.read32(apb + CLIENT_INTR_STATUS_MISC),
        intr_mask_legacy: io.read32(apb + CLIENT_INTR_MASK_LEGACY),
        intr_mask_misc: io.read32(apb + CLIENT_INTR_MASK_MISC),
        power_con: io.read32(apb + CLIENT_POWER_CON),
        hot_reset_ctrl: io.read32(apb + CLIENT_HOT_RESET_CTRL),
        axi_slv_atu_bypass: io.read32(apb + CLIENT_AXI_SLV_ATU_BYPASS),
        axi_slv_misc_info: io.read32(apb + CLIENT_AXI_SLV_MISC_INFO),
        ltssm_status: io.read32(apb + CLIENT_LTSSM_STATUS),
        version: io.read32(apb + CLIENT_VER),
    });

    require(clock.dbi_clocked(), Stop::DbiClockGated)?;
    require(clock.pipe_clocked(), Stop::PipeClockGated)?;
    require(clock.core_released(), Stop::CoreInReset)?;
    require(phy.running(), Stop::PhyNotRunning)?;
    // The fitted-drive observation uses MPLLB while the Gen1/no-drive state used MPLLA.
    // Admit B-only operation only with a live L0 link; preserve the established A path.
    // Both SRAM and power checks still apply to both PHYs. — TIMBERDOODLE
    require(
        phy.pll_on() == (true, true)
            || report
                .client
                .is_some_and(|c| c.link_up() && c.ltssm_state() == 0x11),
        Stop::PhyNotRunning,
    )?;
    require(phy.clamp_released(), Stop::PhyClamped)?;
    let window = report.topology.dbi_window().ok_or(Stop::DbiAboveMap)?;
    let version = io.read32(window + DBI_VERSION_NUMBER);
    require(version == DWC_VERSION_560A, Stop::DbiUnverified)?;

    let read = |io: &mut I, offset: u64| io.read32(window + offset);
    let mut dbi = Dbi {
        window,
        version,
        version_type: read(io, DBI_VERSION_TYPE),
        id: read(io, DBI_ID),
        command: read(io, DBI_COMMAND),
        class: read(io, DBI_CLASS),
        bus: read(io, DBI_BUS),
        mem: read(io, DBI_MEM),
        pref: read(io, DBI_PREF),
        pref_base_upper: read(io, DBI_PREF_BASE_UPPER),
        pref_limit_upper: read(io, DBI_PREF_LIMIT_UPPER),
        link_cap: read(io, DBI_LINK_CAP),
        link_control_status: read(io, DBI_LINK_CONTROL_STATUS),
        port_link_ctrl: read(io, DBI_PORT_LINK_CTRL),
        pl_debug0: read(io, DBI_PL_DEBUG0),
        pl_debug1: read(io, DBI_PL_DEBUG1),
        gen2_ctrl: read(io, DBI_GEN2_CTRL),
        misc_control_1: read(io, DBI_MISC_CONTROL_1),
        amba_error_response: read(io, DBI_AMBA_ERROR_RESPONSE),
        outbound: [AtuRegion::default(); ATU_REGIONS],
        inbound: [AtuRegion::default(); ATU_REGIONS],
    };
    for index in 0..ATU_REGIONS {
        let base = DBI_ATU + 0x200 * index as u64;
        dbi.outbound[index] = read_region(io, window + base);
        dbi.inbound[index] = read_region(io, window + base + 0x100);
    }
    report.dbi = Some(dbi);
    Ok(())
}

/// A region's control word first; the address words only when the region is enabled.
fn read_region(io: &mut impl RegisterRead, base: u64) -> AtuRegion {
    let ctrl2 = io.read32(base + 0x04);
    if ctrl2 & ATU_REGION_ENABLE == 0 {
        return AtuRegion {
            ctrl2,
            ..AtuRegion::default()
        };
    }
    AtuRegion {
        ctrl1: io.read32(base),
        ctrl2,
        lower_base: io.read32(base + 0x08),
        upper_base: io.read32(base + 0x0c),
        limit: io.read32(base + 0x10),
        lower_target: io.read32(base + 0x14),
        upper_target: io.read32(base + 0x18),
        upper_limit: io.read32(base + 0x20),
    }
}

fn require(holds: bool, stop: Stop) -> Result<(), Stop> {
    if holds {
        Ok(())
    } else {
        Err(stop)
    }
}

const fn up(flag: bool) -> &'static str {
    if flag {
        "up"
    } else {
        "down"
    }
}

const fn bit(flag: bool) -> u8 {
    flag as u8
}

/// The kernel boot report: one line per block, each ending `OK` when read or `--` when a rung
/// left it unread, then one line per enabled iATU region and the ladder's verdict.
impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.stop == Some(Stop::InvalidTopology) {
            return writeln!(
                f,
                "PCIE30X4 MMIO     Check     stopped: {}; no registers read   --",
                Stop::InvalidTopology.reason()
            );
        }
        let t = &self.topology;
        write!(
            f,
            "PCIE30X4 TOPOLOGY Check     apb={:#x} dbi={:#x} cfg={:#x}+{:#x} x{} gen{} \
             spi={}/{}/{}/{}/{} pd={}",
            t.apb,
            t.dbi,
            t.config,
            t.config_len,
            t.lanes,
            t.max_link_speed,
            t.spis.sys,
            t.spis.pmc,
            t.spis.msg,
            t.spis.legacy,
            t.spis.err,
            t.power_domain,
        )?;
        for (label, line) in [("perst", t.perst), ("3v3", t.supply)] {
            match line {
                Some(line) => write!(
                    f,
                    " {}={:#x}:{}{}",
                    label,
                    line.bank,
                    line.pin,
                    if line.active_low { "-low" } else { "" }
                )?,
                None => write!(f, " {}=none", label)?,
            }
        }
        writeln!(f, "   OK")?;

        let p = &self.power;
        writeln!(
            f,
            "PCIE30X4 POWER    Check     pwr={:#x} php={} pcie={} biu req/ack/idle={}/{}/{} \
             status-only   OK",
            p.pwr_gate_sts0,
            up(p.php_up()),
            up(p.pcie_up()),
            bit(p.biu_requested()),
            bit(p.biu_acknowledged()),
            bit(p.biu_idle()),
        )?;

        let c = &self.clock;
        writeln!(
            f,
            "PCIE30X4 CLOCK    Check     gate={:#x}/{:#x}/{:#x}/{:#x} rst={:#x}/{:#x}/{:#x} \
             phptop={:#x}/{:#x} apb={} dbi={} axi={} aux={} pipe={} core-rst={} phy-rst={} \
             control-only   OK",
            c.gate32,
            c.gate33,
            c.gate34,
            c.gate39,
            c.softrst32,
            c.softrst33,
            c.softrst34,
            c.php_gate0,
            c.php_softrst0,
            bit(c.client_clocked() && c.client_released()),
            bit(c.dbi_clocked()),
            bit(c.axi_clocked()),
            bit(c.aux_clocked()),
            bit(c.pipe_clocked()),
            if c.core_released() { "clear" } else { "held" },
            if c.phy_released() { "clear" } else { "held" },
        )?;

        match self.phy {
            Some(phy) => {
                let (sram0, sram1) = phy.sram_ready();
                let (pll0, pll1) = phy.pll_on();
                let (pllb0, pllb1) = phy.mpllb_on();
                writeln!(
                    f,
                    "PCIE30X4 PHY      Check     cmn={:#x} mode={} clamp={} phy0={:#x} \
                     phy1={:#x} sram={}/{} mplla={}/{} mpllb={}/{}   OK",
                    phy.cmn_con0,
                    phy.mode_name(),
                    if phy.clamp_released() { "off" } else { "on" },
                    phy.phy0_status1,
                    phy.phy1_status1,
                    bit(sram0),
                    bit(sram1),
                    bit(pll0),
                    bit(pll1),
                    bit(pllb0),
                    bit(pllb1),
                )?
            }
            None => writeln!(f, "PCIE30X4 PHY      Check     not read   --")?,
        }

        match self.client {
            Some(client) => writeln!(
                f,
                "PCIE30X4 CLIENT   Check     ver={:#x} con={:#x} type={} ltssm-en={} \
                 ltssm={:#x} {} smlh={} rdlh={} rate={}GT/s misc={:#x} mask={:#x}/{:#x} \
                 pwr={:#x} hot={:#x}   OK",
                client.version,
                client.general_con,
                if client.device_type() == 4 {
                    "rc"
                } else {
                    "not-rc"
                },
                bit(client.ltssm_enabled()),
                client.ltssm_state(),
                ltssm_name(client.ltssm_state()),
                bit(client.smlh_link_up()),
                bit(client.rdlh_link_up()),
                rate_name(client.rate() + 1),
                client.intr_status_misc,
                client.intr_mask_legacy,
                client.intr_mask_misc,
                client.power_con,
                client.hot_reset_ctrl,
            )?,
            None => writeln!(f, "PCIE30X4 CLIENT   Check     not read   --")?,
        }
        if let Some(client) = self.client {
            writeln!(f, "PCIE30X4 AXI      Check     atu-bypass={:#x} read-misc-info={:#x}; observed only   OK",
                client.axi_slv_atu_bypass, client.axi_slv_misc_info)?;
        }

        match self.dbi {
            Some(dbi) => {
                let (cap_speed, cap_width) = dbi.link_cap();
                let (speed, width) = dbi.link_status();
                let (memory_base, memory_limit) = dbi.memory_window();
                let (pref_base, pref_limit) = dbi.prefetch_window();
                let (out, inb) = dbi.enabled_regions();
                writeln!(
                    f,
                    "PCIE30X4 ROOT     Check     via={:#x} ver={:#x}/{:#x} id={:04x}:{:04x} \
                     class={:06x} cmd={:#x} bus={}/{}/{} mem={:#x}-{:#x} pref={:#x}-{:#x}   OK",
                    dbi.window,
                    dbi.version,
                    dbi.version_type,
                    dbi.id & 0xffff,
                    dbi.id >> 16,
                    dbi.class >> 8,
                    dbi.command & 0xffff,
                    dbi.bus & 0xff,
                    (dbi.bus >> 8) & 0xff,
                    (dbi.bus >> 16) & 0xff,
                    memory_base,
                    memory_limit,
                    pref_base,
                    pref_limit,
                )?;
                writeln!(
                    f,
                    "PCIE30X4 LINK     Check     cap={}GT/s x{} sta={}GT/s x{} dll={} train={} \
                     amba={:#x} cfg-fail={} crs={} gen2={:#x} misc1={:#x} plc={:#x} dbg={:#x}/{:#x} \
                     atu out/in={}/{}   OK",
                    rate_name(cap_speed),
                    cap_width,
                    rate_name(speed),
                    width,
                    bit(dbi.data_link_active()),
                    bit(dbi.training()),
                    dbi.amba_error_response,
                    if dbi.failed_reads_return_ones() {
                        "ones"
                    } else {
                        "axi-error"
                    },
                    dbi.crs_response(),
                    dbi.gen2_ctrl,
                    dbi.misc_control_1,
                    dbi.port_link_ctrl,
                    dbi.pl_debug0,
                    dbi.pl_debug1,
                    out,
                    inb,
                )?;
                for (direction, regions) in [("OUT", &dbi.outbound), ("IN ", &dbi.inbound)] {
                    for (index, region) in regions.iter().enumerate() {
                        if region.enabled() {
                            write_region(f, direction, index, region)?;
                        }
                    }
                }
            }
            None => writeln!(f, "PCIE30X4 ROOT     Check     not read   --")?,
        }

        match self.stop {
            None => writeln!(
                f,
                "PCIE30X4 MMIO     Check     every rung held; nothing written   OK"
            ),
            Some(stop) => writeln!(
                f,
                "PCIE30X4 MMIO     Check     stopped: {}; later blocks unread   --",
                stop.reason()
            ),
        }
    }
}

fn write_region(
    f: &mut fmt::Formatter<'_>,
    direction: &str,
    index: usize,
    region: &AtuRegion,
) -> fmt::Result {
    write!(
        f,
        "PCIE30X4 ATU {}{:<2} Check     {} {:#x}-{:#x} -> {:#x}",
        direction,
        index,
        region.type_name(),
        region.base(),
        region.limit(),
        region.target(),
    )?;
    if let Some((bus, device, function)) = region.config_bdf() {
        write!(
            f,
            " ({}{:02x}:{:02x}.{})",
            if region.ctrl2 & (1 << 28) != 0 {
                "ECAM start "
            } else {
                ""
            },
            bus,
            device,
            function
        )?;
    }
    writeln!(f, " ctrl={:#x}/{:#x}   OK", region.ctrl1, region.ctrl2)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::format;
    use std::string::String;
    use std::vec::Vec;

    const DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    const X13S: &[u8] = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");

    /// A register file: listed addresses read their value, everything else reads 0. Every read
    /// is recorded so a test can prove a gated block was never touched.
    struct Fake {
        values: Vec<(u64, u32)>,
        reads: Vec<u64>,
    }

    impl Fake {
        /// The state the j482-era metal readout implies, plus a trained Gen3 x4 link and one CFG0
        /// and one MEM outbound region.
        fn healthy() -> Self {
            let dbi = PCIE30X4_DBI_ALIAS;
            let atu = dbi + DBI_ATU;
            Self {
                values: Vec::from([
                    (PMU_PWR_GATE_STS0, 0xfff9),
                    (PCIE3PHY_GRF + PHY_CMN_CON0, 0x3104),
                    (PCIE3PHY_GRF + PHY0_STATUS1, 0xf),
                    (PCIE3PHY_GRF + PHY1_STATUS1, 0xf),
                    (PCIE30X4_APB + CLIENT_GENERAL_CON, 0x44),
                    (PCIE30X4_APB + CLIENT_LTSSM_STATUS, 0x23_0011),
                    (PCIE30X4_APB + CLIENT_VER, 0x5_0610),
                    (dbi + DBI_VERSION_NUMBER, DWC_VERSION_560A),
                    (dbi + DBI_VERSION_TYPE, 0x6c70_3035),
                    (dbi + DBI_ID, 0x3588_1d87),
                    (dbi + DBI_COMMAND, 0x0010_0007),
                    (dbi + DBI_CLASS, 0x0604_0000),
                    (dbi + DBI_BUS, 0x0001_0100),
                    (dbi + DBI_MEM, 0xf020_f020),
                    (dbi + DBI_LINK_CAP, 0x0047_3c43),
                    (dbi + DBI_LINK_CONTROL_STATUS, 0x2043_0040),
                    (atu + 0x00, 0x4),
                    (atu + 0x04, 0x8000_0000),
                    (atu + 0x08, 0xf000_0000),
                    (atu + 0x10, 0xf00f_ffff),
                    (atu + 0x14, 0x0100_0000),
                    (atu + 0x200, 0x0),
                    (atu + 0x204, 0x8000_0000),
                    (atu + 0x208, 0xf020_0000),
                    (atu + 0x210, 0xf0ff_ffff),
                    (atu + 0x214, 0xf020_0000),
                ]),
                reads: Vec::new(),
            }
        }

        fn set(&mut self, addr: u64, value: u32) {
            self.values.retain(|(a, _)| *a != addr);
            self.values.push((addr, value));
        }

        fn read_any_in(&self, base: u64, len: u64) -> bool {
            self.reads.iter().any(|a| (base..base + len).contains(a))
        }
    }

    impl RegisterRead for Fake {
        fn read32(&mut self, phys: u64) -> u32 {
            self.reads.push(phys);
            self.values
                .iter()
                .find(|(a, _)| *a == phys)
                .map_or(0, |(_, v)| *v)
        }
    }

    fn topology() -> Topology {
        discover_rk3588_pcie30x4(DTB).expect("shipped DTB holds PCIe3_4L")
    }

    /// A copy of the DTB with `value` written over the start of `pcie@fe150000`'s property
    /// `name`, `skip` bytes in.
    fn patched(name: &[u8], skip: usize, value: &[u8]) -> Vec<u8> {
        let fdt = Fdt::new(DTB).unwrap();
        let node = fdt
            .nodes()
            .find(|node| node.name() == b"pcie@fe150000")
            .unwrap();
        let slice = node.property(name).unwrap();
        let at = slice.as_ptr() as usize - DTB.as_ptr() as usize + skip;
        assert!(skip + value.len() <= slice.len());
        let mut bytes = DTB.to_vec();
        bytes[at..at + value.len()].copy_from_slice(value);
        bytes
    }

    #[test]
    fn discovers_the_m2_controller_from_the_shipped_dtb() {
        let t = topology();
        assert_eq!((t.dbi, t.dbi_len), (PCIE30X4_DBI, PCIE30X4_DBI_LEN));
        assert_eq!((t.apb, t.apb_len), (0xfe15_0000, 0x1_0000));
        assert_eq!((t.config, t.config_len), (0xf000_0000, 0x10_0000));
        assert_eq!(t.bus_range, [0, 15]);
        assert_eq!(
            t.windows[2],
            Some(PciWindow {
                cpu: 0x9_0000_0000,
                pci: 0x9_0000_0000,
                bytes: 0x4000_0000
            })
        );
        assert_eq!((t.lanes, t.max_link_speed), (4, 3));
        assert_eq!(
            t.spis,
            Spis {
                sys: 263,
                pmc: 262,
                msg: 261,
                legacy: 260,
                err: 259
            }
        );
        assert_eq!(t.power_domain, 34);
        // PERST# is GPIO4_B6 and the slot's 3.3 V enable GPIO2_B6, both active high.
        assert_eq!(
            t.perst,
            Some(GpioLine {
                bank: 0xfec5_0000,
                pin: 14,
                active_low: false
            })
        );
        assert_eq!(
            t.supply,
            Some(GpioLine {
                bank: 0xfec3_0000,
                pin: 14,
                active_low: false
            })
        );
        assert_eq!(t.dbi_window(), Some(PCIE30X4_DBI_ALIAS));
    }

    #[test]
    fn other_boards_and_bad_nodes_find_nothing() {
        assert!(discover_rk3588_pcie30x4(X13S).is_none());
        assert!(discover_rk3588_pcie30x4(&DTB[..DTB.len() / 2]).is_none());
        assert!(discover_rk3588_pcie30x4(&[]).is_none());
        assert!(discover_rk3588_pcie30x4(&patched(b"status", 0, b"nope")).is_none());
        // Moving 4L's APB client onto the disabled 2L controller's address finds nothing: the
        // second `reg` entry's low address word starts 20 bytes in.
        let moved = patched(b"reg", 20, &0xfe16_0000u32.to_be_bytes());
        assert!(discover_rk3588_pcie30x4(&moved).is_none());
        // An interrupt specifier that is not an SPI is refused rather than misread.
        let ppi = patched(b"interrupts", 0, &1u32.to_be_bytes());
        assert!(discover_rk3588_pcie30x4(&ppi).is_none());
    }

    #[test]
    fn only_the_documented_dbi_windows_with_sufficient_length_are_accepted() {
        let mut t = topology();
        t.dbi = 0xb_0000_0000;
        assert_eq!(t.dbi_window(), None);
        t.dbi = 0xf540_0000;
        assert_eq!(t.dbi_window(), None);
        t.dbi = PCIE30X4_DBI_ALIAS;
        assert_eq!(t.dbi_window(), Some(PCIE30X4_DBI_ALIAS));
        t.dbi_len = DBI_PROBE_LENGTH - 4;
        assert_eq!(t.dbi_window(), None);
    }

    #[test]
    fn malformed_windows_and_binding_metadata_never_enable_a_probe() {
        for (property, offset, value) in [
            (&b"reg"[..], 0, 0u32),          // unrelated 32-bit DBI address
            (b"reg", 12, 0x1000),            // DBI too short to cover the iATU
            (b"reg", 28, 0x800),             // APB too short to cover CLIENT_VER
            (b"power-domains", 0, u32::MAX), // unresolved provider
            (b"power-domains", 4, 33),       // another domain
            (b"num-lanes", 0, 0),
            (b"max-link-speed", 0, 0),
            (b"interrupts", 8, 1),  // edge instead of level-high
            (b"interrupts", 12, 1), // nonzero PPI partition cell on an SPI
            (b"#address-cells", 0, 2),
            (b"bus-range", 4, 0x100),
            (b"ranges", 0, 0), // no PCI address space
        ] {
            assert!(
                discover_rk3588_pcie30x4(&patched(property, offset, &value.to_be_bytes()))
                    .is_none(),
                "{} at {offset}",
                core::str::from_utf8(property).unwrap()
            );
        }
        let mut bad = [topology(); 3];
        bad[0].dbi = 0x4000_0000;
        bad[1].apb = 0xfe16_0000;
        bad[2].dbi_len = 0x1000;
        for topology in bad {
            // The public pure probe also refuses a manually altered topology before MMIO.
            let mut fake = Fake::healthy();
            let report = probe(topology, &mut fake);
            assert_eq!(report.stop, Some(Stop::InvalidTopology));
            assert!(fake.reads.is_empty());
            assert!(format!("{report}").contains("no registers read"));
        }
    }

    #[test]
    fn high_atu_limits_inherit_the_base_unless_the_region_size_is_increased() {
        let mut region = AtuRegion {
            ctrl2: ATU_REGION_ENABLE,
            upper_base: 9,
            lower_base: 0,
            limit: 0x3fff_ffff,
            upper_limit: 0,
            ..AtuRegion::default()
        };
        assert_eq!(
            (region.base(), region.limit()),
            (0x9_0000_0000, 0x9_3fff_ffff)
        );
        region.ctrl1 |= 1 << 13;
        region.upper_limit = 10;
        assert_eq!(region.limit(), 0xa_3fff_ffff);
    }

    #[test]
    fn firmware_shift_mode_uses_the_cpu_address_for_config_bdf() {
        // Actual no-drive firmware region from NV0's metal boot. — TIMBERDOODLE
        let mut region = AtuRegion {
            ctrl1: 4,
            ctrl2: 0x9000_0000,
            upper_base: 9,
            lower_base: 0x0010_0000,
            limit: 0x0010_ffff,
            lower_target: 0x0010_0000,
            ..AtuRegion::default()
        };
        assert_eq!(region.config_bdf(), Some((1, 0, 0)));
        assert_eq!(region.limit(), 0x9_0010_ffff);
        region.lower_target = 0x021d_0000;
        assert_eq!(region.config_bdf(), Some((1, 0, 0)));
        region.lower_base = 0x0011_d000;
        assert_eq!(region.config_bdf(), Some((1, 3, 5)));
        region.ctrl2 &= !(1 << 28);
        assert_eq!(region.config_bdf(), Some((2, 3, 5)));
        region.ctrl1 = 0;
        assert_eq!(region.config_bdf(), None);
    }

    #[test]
    fn a_healthy_controller_reads_every_block_and_decodes_the_link() {
        let mut fake = Fake::healthy();
        let report = probe(topology(), &mut fake);
        assert_eq!(report.stop, None);
        let client = report.client.unwrap();
        assert!(client.link_up());
        assert_eq!(ltssm_name(client.ltssm_state()), "L0");
        assert_eq!(client.device_type(), 4);
        let dbi = report.dbi.unwrap();
        assert_eq!(dbi.link_status(), (3, 4));
        assert_eq!(dbi.link_cap(), (3, 4));
        assert!(dbi.data_link_active());
        assert!(dbi.failed_reads_return_ones());
        assert_eq!(dbi.enabled_regions(), (2, 0));
        assert_eq!(dbi.memory_window(), (0xf020_0000, 0xf02f_ffff));
        assert_eq!(dbi.outbound[0].type_name(), "cfg0");
        assert_eq!(dbi.outbound[1].target(), 0xf020_0000);
        // Disabled regions cost one read each: no address words were read for region 2.
        let atu2 = PCIE30X4_DBI_ALIAS + DBI_ATU + 0x400;
        assert!(fake.reads.contains(&(atu2 + 4)));
        assert!(!fake.reads.contains(&(atu2 + 8)));
        // The DBI is read only through the 32-bit alias, never the DTB window above 4 GiB.
        assert!(!fake.read_any_in(PCIE30X4_DBI, PCIE30X4_DBI_LEN));
        // Nothing reads the config, MEM or I/O windows.
        assert!(!fake.read_any_in(0xf000_0000, 0x100_0000));
        assert!(!fake.read_any_in(0x9_0000_0000, 0x4000_0000));
    }

    #[test]
    fn each_failed_rung_leaves_every_later_block_unread() {
        type Case = (u64, u32, Stop, bool, bool);
        // (register, value, expected stop, PHY GRF read?, APB read?)
        let cases: &[Case] = &[
            (
                PMU_PWR_GATE_STS0,
                0xfff9 | PD_PHP_DOWN,
                Stop::PhpDown,
                false,
                false,
            ),
            (
                PMU_PWR_GATE_STS0,
                0xfff9 | PD_PCIE_DOWN,
                Stop::PcieDown,
                false,
                false,
            ),
            (
                PMU_BIU_IDLE_SFTCON1,
                BIU_PHP_REQUEST,
                Stop::BiuIdle,
                false,
                false,
            ),
            (PMU_BIU_IDLE_STS0, BIU_PHP, Stop::BiuIdle, false, false),
            (PMU_BIU_IDLE_ACK_STS0, BIU_PHP, Stop::BiuIdle, false, false),
            (
                PHPTOPCRU_GATE_CON00,
                PCLK_PCIE3_PHY,
                Stop::PhyGrfClockGated,
                false,
                false,
            ),
            (
                PHPTOPCRU_SOFTRST_CON00,
                RESETN_PCIE3_PHY,
                Stop::PhyInReset,
                false,
                false,
            ),
            (
                PHPTOPCRU_SOFTRST_CON00,
                PRESETN_PCIE3_PHY,
                Stop::PhyInReset,
                false,
                false,
            ),
            (
                CRU_GATE_CON33,
                PCLK_PCIE_4L,
                Stop::ClientClockGated,
                true,
                false,
            ),
            (
                CRU_SOFTRST_CON33,
                PRESETN_PCIE_4L,
                Stop::ClientInReset,
                true,
                false,
            ),
            (
                CRU_GATE_CON32,
                PCLK_PHP_ROOT,
                Stop::ClientClockGated,
                true,
                false,
            ),
            (
                CRU_GATE_CON32,
                PCLK_PHP_BIU,
                Stop::ClientClockGated,
                true,
                false,
            ),
            (
                CRU_SOFTRST_CON32,
                PRESETN_PHP_BIU,
                Stop::ClientInReset,
                true,
                false,
            ),
            (
                CRU_GATE_CON32,
                ACLK_PCIE_4L_DBI,
                Stop::DbiClockGated,
                true,
                true,
            ),
            (
                CRU_GATE_CON32,
                ACLK_PHP_ROOT,
                Stop::DbiClockGated,
                true,
                true,
            ),
            (
                CRU_GATE_CON32,
                ACLK_PCIE_BIU,
                Stop::DbiClockGated,
                true,
                true,
            ),
            (
                CRU_GATE_CON39,
                CLK_PCIE_4L_PIPE,
                Stop::PipeClockGated,
                true,
                true,
            ),
            (
                CRU_SOFTRST_CON32,
                ARESETN_PCIE_BIU,
                Stop::CoreInReset,
                true,
                true,
            ),
            (
                CRU_SOFTRST_CON32,
                ARESETN_PHP_BIU,
                Stop::CoreInReset,
                true,
                true,
            ),
            // The reset value: firmware never released the controller.
            (
                CRU_SOFTRST_CON32,
                RESETN_PCIE_4L_POWER_UP,
                Stop::CoreInReset,
                true,
                true,
            ),
            (
                PCIE3PHY_GRF + PHY1_STATUS1,
                0x7,
                Stop::PhyNotRunning,
                true,
                true,
            ),
            (
                PCIE3PHY_GRF + PHY0_STATUS1,
                0x7,
                Stop::PhyNotRunning,
                true,
                true,
            ),
            (
                PCIE3PHY_GRF + PHY0_STATUS1,
                0xe,
                Stop::PhyNotRunning,
                true,
                true,
            ),
            (
                PCIE3PHY_GRF + PHY1_STATUS1,
                0xe,
                Stop::PhyNotRunning,
                true,
                true,
            ),
            (
                PCIE3PHY_GRF + PHY0_STATUS1,
                u32::MAX,
                Stop::PhyNotRunning,
                true,
                true,
            ),
            (
                PCIE3PHY_GRF + PHY1_STATUS1,
                u32::MAX,
                Stop::PhyNotRunning,
                true,
                true,
            ),
            (
                PCIE3PHY_GRF + PHY_CMN_CON0,
                0x3004,
                Stop::PhyClamped,
                true,
                true,
            ),
        ];
        for &(register, value, stop, phy_read, apb_read) in cases {
            let mut fake = Fake::healthy();
            fake.set(register, value);
            let report = probe(topology(), &mut fake);
            assert_eq!(report.stop, Some(stop), "{stop:?}");
            assert_eq!(
                fake.read_any_in(PCIE3PHY_GRF, 0x1_0000),
                phy_read,
                "{stop:?}"
            );
            assert_eq!(
                fake.read_any_in(PCIE30X4_APB, 0x1_0000),
                apb_read,
                "{stop:?}"
            );
            assert!(
                !fake.read_any_in(PCIE30X4_DBI_ALIAS, PCIE30X4_DBI_LEN),
                "{stop:?}"
            );
            assert!(report.dbi.is_none());
            let text = format!("{report}");
            assert!(text.contains(stop.reason()), "{text}");
        }
    }

    #[test]
    fn a_wrong_dbi_version_stops_after_one_read() {
        let mut fake = Fake::healthy();
        fake.set(PCIE30X4_DBI_ALIAS + DBI_VERSION_NUMBER, 0xffff_ffff);
        let report = probe(topology(), &mut fake);
        assert_eq!(report.stop, Some(Stop::DbiUnverified));
        let dbi_reads: Vec<_> = fake
            .reads
            .iter()
            .filter(|a| (PCIE30X4_DBI_ALIAS..PCIE30X4_DBI_ALIAS + PCIE30X4_DBI_LEN).contains(a))
            .collect();
        assert_eq!(dbi_reads, [&(PCIE30X4_DBI_ALIAS + DBI_VERSION_NUMBER)]);
    }

    #[test]
    fn the_fitted_drive_mpllb_state_needs_sram_power_and_a_live_l0_link() {
        let mut fake = Fake::healthy();
        for status in [0x47, 0x4f] {
            fake.set(PCIE3PHY_GRF + PHY0_STATUS1, status);
            fake.set(PCIE3PHY_GRF + PHY1_STATUS1, status);
            assert_eq!(probe(topology(), &mut fake).stop, None);
        }
        for status in [0x46, 0x45, 0x43, 0x7, u32::MAX] {
            fake.set(PCIE3PHY_GRF + PHY0_STATUS1, status);
            fake.set(PCIE3PHY_GRF + PHY1_STATUS1, 0x47);
            assert_eq!(probe(topology(), &mut fake).stop, Some(Stop::PhyNotRunning));
        }
        fake.set(PCIE3PHY_GRF + PHY0_STATUS1, 0x47);
        fake.set(PCIE30X4_APB + CLIENT_LTSSM_STATUS, 0);
        fake.reads.clear();
        assert_eq!(probe(topology(), &mut fake).stop, Some(Stop::PhyNotRunning));
        assert!(!fake.read_any_in(PCIE30X4_DBI_ALIAS, PCIE30X4_DBI_LEN));
    }

    #[test]
    fn a_firmware_set_amba_error_response_is_reported() {
        let mut fake = Fake::healthy();
        fake.set(PCIE30X4_DBI_ALIAS + DBI_AMBA_ERROR_RESPONSE, 0x9c01);
        let report = probe(topology(), &mut fake);
        assert!(!report.dbi.unwrap().failed_reads_return_ones());
        assert!(format!("{report}").contains("cfg-fail=axi-error"));
        for (bits, response) in [
            (0, "okay"),
            (1, "ones"),
            (2, "vendor-retry"),
            (3, "axi-error"),
        ] {
            fake.set(PCIE30X4_DBI_ALIAS + DBI_AMBA_ERROR_RESPONSE, bits << 3);
            let report = probe(topology(), &mut fake);
            assert_eq!(report.dbi.unwrap().crs_response(), response);
            assert!(format!("{report}").contains(&format!("crs={response}")));
        }
    }

    #[test]
    fn the_report_names_every_block_in_short_lines() {
        let text: String = format!("{}", probe(topology(), &mut Fake::healthy()));
        for needle in [
            "PCIE30X4 TOPOLOGY",
            "perst=0xfec50000:14",
            "3v3=0xfec30000:14",
            "php=up pcie=up",
            "mode=x4 clamp=off",
            "sram=1/1 mplla=1/1",
            "ltssm=0x11 L0 smlh=1 rdlh=1 rate=8.0GT/s",
            "id=1d87:3588",
            "class=060400",
            "bus=0/1/1",
            "cap=8.0GT/s x4 sta=8.0GT/s x4 dll=1",
            "cfg-fail=ones",
            "PCIE30X4 ATU OUT0  Check     cfg0 0xf0000000-0xf00fffff -> 0x1000000 (01:00.0)",
            "PCIE30X4 ATU OUT1  Check     mem 0xf0200000-0xf0ffffff -> 0xf0200000",
            "every rung held; nothing written",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
        // kmsg keeps 240 bytes per line.
        assert!(text.lines().all(|line| line.len() < 240), "{text}");
    }

    #[test]
    fn every_ltssm_code_in_the_trm_has_a_name() {
        for code in 0..=0x23 {
            assert_ne!(ltssm_name(code), "UNKNOWN", "{code:#x}");
        }
        assert_eq!(ltssm_name(0x24), "UNKNOWN");
    }
}
