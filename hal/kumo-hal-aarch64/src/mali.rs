//j570
//j571

//! RK3588 Mali-G610 integration-status discovery: the observation half of PLAN/013 G1.
//!
//! RK3588 integration facts come from the vendored RK3588 TRM Part 1: chapter 1 (address map and
//! interrupts), chapter 2 (CRU) and chapter 7 (PMU). The GPU itself is described in RESEARCH/009.
//! This slice selects the enabled `rockchip,rk3588-mali` node from the board DTB and reads only
//! always-on PMU status and CRU clock/reset controls. It never dereferences the GPU aperture: while
//! `VD_GPU` is down a read there may hang the bus. Nothing is written, so it cannot change what
//! firmware left behind.

use core::fmt;

use crate::smmuv3::RK3588_PMU_PWR_GATE_STS0;

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;
const MAX_FDT_DEPTH: usize = 32;
const GIC_SPI: u32 = 0;

/// RK3588 TRM Part 1 chapter 1: the GPU's register window as the board DTB maps it (the address
/// map reserves 16 MiB; the DT node claims 2 MiB, which covers every register RESEARCH/009 lists).
pub const RK3588_MALI_BASE: u64 = 0xfb00_0000;
pub const RK3588_MALI_LENGTH: u64 = 0x20_0000;

/// GIC SPIs of the `job`, `mmu` and `gpu` lines, in DT order: INTIDs 124, 125 and 126.
pub const RK3588_MALI_SPIS: [u32; 3] = [92, 93, 94];

/// RK3588 PMU registers (operational base `0xFD8D_0000`, TRM chapter 7) that describe `VD_GPU`.
///
/// `PMU_PWR_GATE_SFTCON0` bit 0 is the software power-down request (reset 1: software holds the
/// domain down) and `PMU_PWR_GATE_STS0` bit 0 its state (1 = down). The three `PMU_BIU_IDLE_*`
/// registers carry `BIU_GPU`'s bus-idle request, acknowledge and state in bit 0. `PMU_BISR_STS4`
/// bit 1 reports "power up and memory repair complete" for `VD_GPU`. — TIMBERDOODLE
pub const RK3588_PMU_BIU_IDLE_SFTCON0: u64 = 0xfd8d_810c;
pub const RK3588_PMU_BIU_IDLE_ACK_STS0: u64 = 0xfd8d_8118;
pub const RK3588_PMU_BIU_IDLE_STS0: u64 = 0xfd8d_8120;
pub const RK3588_PMU_PWR_GATE_SFTCON0: u64 = 0xfd8d_814c;
pub const RK3588_PMU_BISR_STS4: u64 = 0xfd8d_8290;
const VD_GPU: u32 = 1 << 0;
const VD_GPU_BISR: u32 = 1 << 1;

/// RK3588 CRU registers (base `0xFD7C_0000`, TRM chapter 2) for the GPU's clocks and resets.
///
/// `CLKSEL_CON158`..`161` hold the GPU source mux, its PLL parent and dividers, the stacks and BIU
/// dividers, and the PVTPLL reference. `GATE_CON66/67` are software clock gates (high disables the
/// clock); `SOFTRST_CON66/67` are software reset requests (high asserts reset). A read reports only
/// these controls: it cannot prove a clock is toggling or that every reset source is inactive.
/// — TIMBERDOODLE
pub const RK3588_CRU_CLKSEL_CON158: u64 = 0xfd7c_0578;
pub const RK3588_CRU_CLKSEL_CON159: u64 = 0xfd7c_057c;
pub const RK3588_CRU_CLKSEL_CON160: u64 = 0xfd7c_0580;
pub const RK3588_CRU_CLKSEL_CON161: u64 = 0xfd7c_0584;
pub const RK3588_CRU_GATE_CON66: u64 = 0xfd7c_0908;
pub const RK3588_CRU_GATE_CON67: u64 = 0xfd7c_090c;
pub const RK3588_CRU_SOFTRST_CON66: u64 = 0xfd7c_0b08;
pub const RK3588_CRU_SOFTRST_CON67: u64 = 0xfd7c_0b0c;

/// The PMU and CRU words the target wrapper reads, in report order.
pub const RK3588_MALI_POWER_REGISTERS: [u64; 6] = [
    RK3588_PMU_PWR_GATE_SFTCON0,
    RK3588_PMU_PWR_GATE_STS0,
    RK3588_PMU_BIU_IDLE_SFTCON0,
    RK3588_PMU_BIU_IDLE_ACK_STS0,
    RK3588_PMU_BIU_IDLE_STS0,
    RK3588_PMU_BISR_STS4,
];
pub const RK3588_MALI_CLOCK_REGISTERS: [u64; 8] = [
    RK3588_CRU_CLKSEL_CON158,
    RK3588_CRU_CLKSEL_CON159,
    RK3588_CRU_CLKSEL_CON160,
    RK3588_CRU_CLKSEL_CON161,
    RK3588_CRU_GATE_CON66,
    RK3588_CRU_GATE_CON67,
    RK3588_CRU_SOFTRST_CON66,
    RK3588_CRU_SOFTRST_CON67,
];

const CLK_GPU_SRC_SEL_PVTPLL: u32 = 1 << 14;
const CLK_GPU_PVTPLL_SEL_OSC: u32 = 1 << 2;
/// `GATE_CON66`: `clk_gpu_src_t` (1), `clk_gpu_src` (3), `clk_gpu` (4), `clk_gpu_coregroup` (6),
/// `clk_gpu_stacks` (7). `clk_gpu_src_t` is on the path decision 4 fixed (the PLL path, PVTPLL
/// off), and on the reset path too (SPLL through `clk_gpu_src_t`), so its gate counts (J571).
const GPU_CORE_CLOCK_GATES: u32 = (1 << 1) | (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7);
/// `GATE_CON66`: `aclk_s/m0/m1/m2/m3_gpu_biu` (8-12), `pclk_gpu_root` (13), `pclk_gpu_biu` (14).
const GPU_BIU_CLOCK_GATES: u32 = 0x7f << 8;
/// `SOFTRST_CON66`: `resetn_gpu` (4), `sysresetn_gpu` (5).
const GPU_CORE_RESETS: u32 = (1 << 4) | (1 << 5);
/// `SOFTRST_CON66`: `aresetn_s/m0/m1/m2/m3_gpu_biu` (8-12), `presetn_gpu_biu` (14).
const GPU_BIU_RESETS: u32 = (0x1f << 8) | (1 << 14);

/// The enabled RK3588 Mali-G610 selected from the board DTB.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaliG610Topology {
    pub base: u64,
    pub length: u64,
    /// GIC SPI numbers of the `job`, `mmu` and `gpu` interrupts, in DT order.
    pub spis: [u32; 3],
}

/// Read-only PMU status for `VD_GPU`, sampled without touching the GPU aperture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaliG610PowerStatus {
    pub pwr_gate_sftcon0: u32,
    pub pwr_gate_sts0: u32,
    pub biu_idle_sftcon0: u32,
    pub biu_idle_ack_sts0: u32,
    pub biu_idle_sts0: u32,
    pub bisr_sts4: u32,
}

impl MaliG610PowerStatus {
    pub const fn from_words(words: [u32; 6]) -> Self {
        Self {
            pwr_gate_sftcon0: words[0],
            pwr_gate_sts0: words[1],
            biu_idle_sftcon0: words[2],
            biu_idle_ack_sts0: words[3],
            biu_idle_sts0: words[4],
            bisr_sts4: words[5],
        }
    }

    pub const fn domain_powered(self) -> bool {
        self.pwr_gate_sts0 & VD_GPU == 0
    }

    pub const fn software_power_down_requested(self) -> bool {
        self.pwr_gate_sftcon0 & VD_GPU != 0
    }

    pub const fn biu_idle_requested(self) -> bool {
        self.biu_idle_sftcon0 & VD_GPU != 0
    }

    pub const fn biu_idle_acknowledged(self) -> bool {
        self.biu_idle_ack_sts0 & VD_GPU != 0
    }

    /// `BIU_GPU` is in its idle state: the bus interface is quiesced and passes no traffic.
    pub const fn biu_idle(self) -> bool {
        self.biu_idle_sts0 & VD_GPU != 0
    }

    pub const fn memory_repair_complete(self) -> bool {
        self.bisr_sts4 & VD_GPU_BISR != 0
    }
}

/// Where `clk_gpu_src` comes from, decoded from `CLKSEL_CON158` and `CLKSEL_CON161`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaliG610ClockSource {
    /// `clk_gpu_src_t`: a PLL picked by `parent`, divided by `divider`.
    Pll { parent: &'static str, divider: u32 },
    /// `clk_gpu_pvtpll`, referenced from `clk_gpu_src_t` or from the 24 MHz oscillator.
    Pvtpll { reference_is_osc: bool },
}

impl fmt::Display for MaliG610ClockSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Pll { parent, divider } => write!(formatter, "{}/{}", parent, divider),
            Self::Pvtpll {
                reference_is_osc: true,
            } => formatter.write_str("pvtpll(osc)"),
            Self::Pvtpll {
                reference_is_osc: false,
            } => formatter.write_str("pvtpll(src_t)"),
        }
    }
}

/// Snapshot produced by read-only observation of the RK3588 CRU controls for the GPU; the
/// underlying control fields remain read/write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaliG610ClockStatus {
    pub clksel_con158: u32,
    pub clksel_con159: u32,
    pub clksel_con160: u32,
    pub clksel_con161: u32,
    pub gate_con66: u32,
    pub gate_con67: u32,
    pub softrst_con66: u32,
    pub softrst_con67: u32,
}

impl MaliG610ClockStatus {
    pub const fn from_words(words: [u32; 8]) -> Self {
        Self {
            clksel_con158: words[0],
            clksel_con159: words[1],
            clksel_con160: words[2],
            clksel_con161: words[3],
            gate_con66: words[4],
            gate_con67: words[5],
            softrst_con66: words[6],
            softrst_con67: words[7],
        }
    }

    pub const fn source(self) -> MaliG610ClockSource {
        if self.clksel_con158 & CLK_GPU_SRC_SEL_PVTPLL != 0 {
            MaliG610ClockSource::Pvtpll {
                reference_is_osc: self.clksel_con161 & CLK_GPU_PVTPLL_SEL_OSC != 0,
            }
        } else {
            MaliG610ClockSource::Pll {
                parent: match (self.clksel_con158 >> 5) & 0b111 {
                    0 => "gpll",
                    1 => "cpll",
                    2 => "aupll",
                    3 => "npll",
                    4 => "spll",
                    _ => "reserved",
                },
                divider: (self.clksel_con158 & 0x1f) + 1,
            }
        }
    }

    pub const fn stacks_divider(self) -> u32 {
        (self.clksel_con159 & 0x1f) + 1
    }

    pub const fn core_clocks_open(self) -> bool {
        self.gate_con66 & GPU_CORE_CLOCK_GATES == 0
    }

    pub const fn biu_clocks_open(self) -> bool {
        self.gate_con66 & GPU_BIU_CLOCK_GATES == 0
    }

    pub const fn core_resets_clear(self) -> bool {
        self.softrst_con66 & GPU_CORE_RESETS == 0
    }

    pub const fn biu_resets_clear(self) -> bool {
        self.softrst_con66 & GPU_BIU_RESETS == 0
    }
}

/// Result of the target wrapper's PMU/CRU observation of the Mali-G610.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaliG610StatusReport {
    pub topology: MaliG610Topology,
    pub power: MaliG610PowerStatus,
    pub clock: MaliG610ClockStatus,
}

impl MaliG610StatusReport {
    /// The first documented precondition for touching the GPU aperture that does not hold, or
    /// `None` when all of them hold. Even then the aperture stays unproven: these are status and
    /// control reads, not proof of a live clock or of APB admission.
    pub const fn first_unmet_aperture_precondition(&self) -> Option<&'static str> {
        if !self.power.domain_powered() {
            Some("pd-down")
        } else if !self.power.memory_repair_complete() {
            Some("memory-repair-pending")
        } else if self.power.biu_idle_requested() {
            Some("biu-idle-requested")
        } else if self.power.biu_idle() {
            Some("biu-idle")
        } else if !self.clock.core_clocks_open() {
            Some("core-clock-gated")
        } else if !self.clock.biu_clocks_open() {
            Some("biu-clock-gated")
        } else if !self.clock.core_resets_clear() {
            Some("core-reset-asserted")
        } else if !self.clock.biu_resets_clear() {
            Some("biu-reset-asserted")
        } else {
            None
        }
    }
}

const fn open_or_closed(open: bool) -> &'static str {
    if open {
        "open"
    } else {
        "closed"
    }
}

const fn clear_or_asserted(clear: bool) -> &'static str {
    if clear {
        "clear"
    } else {
        "asserted"
    }
}

impl fmt::Display for MaliG610StatusReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let power = self.power;
        let clock = self.clock;
        writeln!(
            formatter,
            "MALI G610 POWER   Check     {:#x} spi={}/{}/{} pd={} sft-down={} biu-req={} \
             biu-ack={} biu-idle={} repair={} pwr={:#x} status-only   OK",
            self.topology.base,
            self.topology.spis[0],
            self.topology.spis[1],
            self.topology.spis[2],
            if power.domain_powered() { "up" } else { "down" },
            power.software_power_down_requested() as u8,
            power.biu_idle_requested() as u8,
            power.biu_idle_acknowledged() as u8,
            power.biu_idle() as u8,
            if power.memory_repair_complete() {
                "done"
            } else {
                "pending"
            },
            power.pwr_gate_sts0,
        )?;
        writeln!(
            formatter,
            "MALI G610 CLOCK   Check     sel={:#x}/{:#x}/{:#x}/{:#x} src={} stacks=/{} \
             gate={:#x}/{:#x} core={} biu={} rst={:#x}/{:#x} core-rst={} biu-rst={} \
             control-only   OK",
            clock.clksel_con158,
            clock.clksel_con159,
            clock.clksel_con160,
            clock.clksel_con161,
            clock.source(),
            clock.stacks_divider(),
            clock.gate_con66,
            clock.gate_con67,
            open_or_closed(clock.core_clocks_open()),
            open_or_closed(clock.biu_clocks_open()),
            clock.softrst_con66,
            clock.softrst_con67,
            clear_or_asserted(clock.core_resets_clear()),
            clear_or_asserted(clock.biu_resets_clear()),
        )?;
        match self.first_unmet_aperture_precondition() {
            Some(reason) => writeln!(
                formatter,
                "MALI G610 MMIO    Check     {:#x} untouched: {} preconditions-unmet   --",
                self.topology.base, reason
            ),
            None => writeln!(
                formatter,
                "MALI G610 MMIO    Check     {:#x} untouched: preconditions met, access unproven   OK",
                self.topology.base
            ),
        }
    }
}

/// Select only the enabled `rockchip,rk3588-mali` node of an RK3588 device tree, at the TRM's
/// address and with the three expected SPIs.
pub fn discover_rk3588_mali_g610(dtb: &[u8]) -> Option<MaliG610Topology> {
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
                if node.compatible_mali && node.status_enabled && node.reg_len >= 4 {
                    let base = cells64(node.reg[0], node.reg[1]);
                    let length = cells64(node.reg[2], node.reg[3]);
                    if base == RK3588_MALI_BASE
                        && length == RK3588_MALI_LENGTH
                        && node.spis() == Some(RK3588_MALI_SPIS)
                    {
                        candidate = Some(MaliG610Topology {
                            base,
                            length,
                            spis: RK3588_MALI_SPIS,
                        });
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
                        node.compatible_mali = string_list_contains(value, b"rockchip,rk3588-mali");
                    }
                    b"reg" => {
                        let count = (value.len() / 4).min(node.reg.len());
                        for (index, slot) in node.reg.iter_mut().take(count).enumerate() {
                            *slot = be32(value, index * 4)?;
                        }
                        node.reg_len = count as u8;
                    }
                    b"interrupts" => {
                        let count = (value.len() / 4).min(node.interrupts.len());
                        for (index, slot) in node.interrupts.iter_mut().take(count).enumerate() {
                            *slot = be32(value, index * 4)?;
                        }
                        node.interrupt_cells = count as u8;
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

#[derive(Clone, Copy)]
struct FdtNode {
    compatible_mali: bool,
    reg: [u32; 4],
    reg_len: u8,
    interrupts: [u32; 12],
    interrupt_cells: u8,
    status_enabled: bool,
}

impl FdtNode {
    const fn empty() -> Self {
        Self {
            compatible_mali: false,
            reg: [0; 4],
            reg_len: 0,
            interrupts: [0; 12],
            interrupt_cells: 0,
            status_enabled: true,
        }
    }

    /// The SPI numbers of exactly three GIC interrupt specifiers, three or four cells each (the
    /// RK3588 GIC-600 uses four: type, number, flags, partition).
    fn spis(&self) -> Option<[u32; 3]> {
        let cells = match self.interrupt_cells {
            9 => 3,
            12 => 4,
            _ => return None,
        };
        let mut spis = [0; 3];
        for (index, spi) in spis.iter_mut().enumerate() {
            let specifier = &self.interrupts[index * cells..];
            if specifier[0] != GIC_SPI {
                return None;
            }
            *spi = specifier[1];
        }
        Some(spis)
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

    const OPI5_DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    const X13S_DTB: &[u8] = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");

    /// `PMU_PWR_GATE_STS0` as CRUX's j482 Orange Pi 5 Plus capture logged it (J483): VD_GPU down.
    const J482_PWR_GATE_STS0: u32 = 0xfff9;

    /// TRM reset values: software holds VD_GPU down, nothing repaired, BIU not idle-requested.
    const RESET_POWER: MaliG610PowerStatus =
        MaliG610PowerStatus::from_words([0x1, 0x1, 0, 0, 0, 0]);
    /// TRM reset values: `clk_gpu_src_t` from SPLL undivided, PVTPLL referenced from the oscillator,
    /// only `clk_testout_gpu` gated, no resets asserted.
    const RESET_CLOCK: MaliG610ClockStatus =
        MaliG610ClockStatus::from_words([0x1f80, 0, 0, 0x4, 0x4, 0, 0, 0]);
    const RESET_CLOCK_PVTPLL_OSC: MaliG610ClockStatus = MaliG610ClockStatus {
        clksel_con158: CLK_GPU_SRC_SEL_PVTPLL,
        ..RESET_CLOCK
    };

    const RUNNING_POWER: MaliG610PowerStatus =
        MaliG610PowerStatus::from_words([0, 0, 0, 0, 0, VD_GPU_BISR]);

    fn topology() -> MaliG610Topology {
        MaliG610Topology {
            base: RK3588_MALI_BASE,
            length: RK3588_MALI_LENGTH,
            spis: RK3588_MALI_SPIS,
        }
    }

    #[test]
    fn discovers_only_the_enabled_rk3588_mali() {
        assert_eq!(discover_rk3588_mali_g610(OPI5_DTB), Some(topology()));
        assert_eq!(discover_rk3588_mali_g610(X13S_DTB), None);
        assert_eq!(discover_rk3588_mali_g610(b"not a dtb"), None);
        assert_eq!(discover_rk3588_mali_g610(&OPI5_DTB[..32]), None);
    }

    #[test]
    fn interrupt_specifiers_decode_with_three_or_four_cells() {
        let mut node = FdtNode::empty();
        node.interrupts[..12].copy_from_slice(&[0, 92, 4, 0, 0, 93, 4, 0, 0, 94, 4, 0]);
        node.interrupt_cells = 12;
        assert_eq!(node.spis(), Some(RK3588_MALI_SPIS));
        node.interrupts[..9].copy_from_slice(&[0, 92, 4, 0, 93, 4, 0, 94, 4]);
        node.interrupt_cells = 9;
        assert_eq!(node.spis(), Some(RK3588_MALI_SPIS));
        node.interrupts[0] = 1; // a PPI is not the GPU
        assert_eq!(node.spis(), None);
        node.interrupt_cells = 8;
        assert_eq!(node.spis(), None);
    }

    #[test]
    fn power_status_decodes_vd_gpu_bits_only() {
        let j482 = MaliG610PowerStatus {
            pwr_gate_sts0: J482_PWR_GATE_STS0,
            ..RESET_POWER
        };
        assert!(!j482.domain_powered());
        assert!(j482.software_power_down_requested());
        assert!(!j482.memory_repair_complete());

        assert!(RUNNING_POWER.domain_powered());
        assert!(!RUNNING_POWER.software_power_down_requested());
        assert!(RUNNING_POWER.memory_repair_complete());
        // Other domains' bits must not leak into VD_GPU's.
        let neighbours = MaliG610PowerStatus::from_words([
            !VD_GPU,
            !VD_GPU,
            !VD_GPU,
            !VD_GPU,
            !VD_GPU,
            VD_GPU_BISR,
        ]);
        assert!(neighbours.domain_powered());
        assert!(!neighbours.software_power_down_requested());
        assert!(!neighbours.biu_idle_requested());
        assert!(!neighbours.biu_idle_acknowledged());
        assert!(!neighbours.biu_idle());
    }

    #[test]
    fn clock_status_decodes_source_dividers_gates_and_resets() {
        assert_eq!(
            RESET_CLOCK.source(),
            MaliG610ClockSource::Pll {
                parent: "spll",
                divider: 1
            }
        );
        assert_eq!(RESET_CLOCK.stacks_divider(), 1);
        assert!(
            RESET_CLOCK.core_clocks_open(),
            "clk_testout_gpu is not on the GPU's path"
        );
        assert!(RESET_CLOCK.biu_clocks_open());
        assert!(RESET_CLOCK.core_resets_clear());
        assert!(RESET_CLOCK.biu_resets_clear());

        let gpll_div4 = MaliG610ClockStatus {
            clksel_con158: 0x1f00 | 3,
            clksel_con159: 1,
            ..RESET_CLOCK
        };
        assert_eq!(format!("{}", gpll_div4.source()), "gpll/4");
        assert_eq!(gpll_div4.stacks_divider(), 2);

        let pvtpll = MaliG610ClockStatus {
            clksel_con158: CLK_GPU_SRC_SEL_PVTPLL,
            clksel_con161: 0,
            ..RESET_CLOCK
        };
        assert_eq!(format!("{}", pvtpll.source()), "pvtpll(src_t)");
        assert_eq!(
            format!("{}", RESET_CLOCK_PVTPLL_OSC.source()),
            "pvtpll(osc)"
        );

        for gate in [1, 3, 4, 6, 7] {
            let gated = MaliG610ClockStatus {
                gate_con66: 1 << gate,
                ..RESET_CLOCK
            };
            assert!(!gated.core_clocks_open(), "gate bit {gate}");
            assert!(gated.biu_clocks_open());
        }
        for gate in 8..=14 {
            let gated = MaliG610ClockStatus {
                gate_con66: 1 << gate,
                ..RESET_CLOCK
            };
            assert!(gated.core_clocks_open());
            assert!(!gated.biu_clocks_open(), "gate bit {gate}");
        }
        for reset in [4, 5] {
            let held = MaliG610ClockStatus {
                softrst_con66: 1 << reset,
                ..RESET_CLOCK
            };
            assert!(!held.core_resets_clear(), "reset bit {reset}");
        }
        for reset in [8, 9, 10, 11, 12, 14] {
            let held = MaliG610ClockStatus {
                softrst_con66: 1 << reset,
                ..RESET_CLOCK
            };
            assert!(held.core_resets_clear());
            assert!(!held.biu_resets_clear(), "reset bit {reset}");
        }
    }

    #[test]
    fn aperture_preconditions_report_the_first_unmet_one_in_order() {
        let report = |power, clock| MaliG610StatusReport {
            topology: topology(),
            power,
            clock,
        };
        assert_eq!(
            report(RESET_POWER, RESET_CLOCK).first_unmet_aperture_precondition(),
            Some("pd-down")
        );
        let no_repair = MaliG610PowerStatus {
            bisr_sts4: 0,
            ..RUNNING_POWER
        };
        assert_eq!(
            report(no_repair, RESET_CLOCK).first_unmet_aperture_precondition(),
            Some("memory-repair-pending")
        );
        let idle_req = MaliG610PowerStatus {
            biu_idle_sftcon0: VD_GPU,
            ..RUNNING_POWER
        };
        assert_eq!(
            report(idle_req, RESET_CLOCK).first_unmet_aperture_precondition(),
            Some("biu-idle-requested")
        );
        let idle = MaliG610PowerStatus {
            biu_idle_sts0: VD_GPU,
            ..RUNNING_POWER
        };
        assert_eq!(
            report(idle, RESET_CLOCK).first_unmet_aperture_precondition(),
            Some("biu-idle")
        );
        let core_gated = MaliG610ClockStatus {
            gate_con66: 1 << 4,
            ..RESET_CLOCK
        };
        assert_eq!(
            report(RUNNING_POWER, core_gated).first_unmet_aperture_precondition(),
            Some("core-clock-gated")
        );
        let biu_gated = MaliG610ClockStatus {
            gate_con66: 1 << 8,
            ..RESET_CLOCK
        };
        assert_eq!(
            report(RUNNING_POWER, biu_gated).first_unmet_aperture_precondition(),
            Some("biu-clock-gated")
        );
        let core_reset = MaliG610ClockStatus {
            softrst_con66: 1 << 4,
            ..RESET_CLOCK
        };
        assert_eq!(
            report(RUNNING_POWER, core_reset).first_unmet_aperture_precondition(),
            Some("core-reset-asserted")
        );
        let biu_reset = MaliG610ClockStatus {
            softrst_con66: 1 << 14,
            ..RESET_CLOCK
        };
        assert_eq!(
            report(RUNNING_POWER, biu_reset).first_unmet_aperture_precondition(),
            Some("biu-reset-asserted")
        );
        assert_eq!(
            report(RUNNING_POWER, RESET_CLOCK).first_unmet_aperture_precondition(),
            None
        );
    }

    #[test]
    fn status_report_renders_power_clock_and_the_aperture_verdict() {
        let down = format!(
            "{}",
            MaliG610StatusReport {
                topology: topology(),
                power: MaliG610PowerStatus {
                    pwr_gate_sts0: J482_PWR_GATE_STS0,
                    ..RESET_POWER
                },
                clock: RESET_CLOCK,
            }
        );
        assert!(down.starts_with(
            "MALI G610 POWER   Check     0xfb000000 spi=92/93/94 pd=down sft-down=1 biu-req=0 \
             biu-ack=0 biu-idle=0 repair=pending pwr=0xfff9 status-only   OK\n"
        ));
        assert!(down.contains(
            "MALI G610 CLOCK   Check     sel=0x1f80/0x0/0x0/0x4 src=spll/1 stacks=/1 \
             gate=0x4/0x0 core=open biu=open rst=0x0/0x0 core-rst=clear biu-rst=clear \
             control-only   OK\n"
        ));
        assert!(down.ends_with(
            "MALI G610 MMIO    Check     0xfb000000 untouched: pd-down preconditions-unmet   --\n"
        ));
        assert_eq!(down.lines().count(), 3);

        let up = format!(
            "{}",
            MaliG610StatusReport {
                topology: topology(),
                power: RUNNING_POWER,
                clock: RESET_CLOCK,
            }
        );
        assert!(up.contains("pd=up sft-down=0"));
        assert!(up.contains("repair=done"));
        assert!(up.ends_with(
            "MALI G610 MMIO    Check     0xfb000000 untouched: preconditions met, access unproven   OK\n"
        ));
    }

    #[test]
    fn register_lists_match_the_report_fields() {
        assert_eq!(RK3588_MALI_POWER_REGISTERS[1], 0xfd8d_8180);
        assert_eq!(RK3588_MALI_CLOCK_REGISTERS[0], 0xfd7c_0300 + 158 * 4);
        assert_eq!(RK3588_MALI_CLOCK_REGISTERS[4], 0xfd7c_0800 + 66 * 4);
        assert_eq!(RK3588_MALI_CLOCK_REGISTERS[6], 0xfd7c_0a00 + 66 * 4);
        for address in RK3588_MALI_POWER_REGISTERS
            .iter()
            .chain(RK3588_MALI_CLOCK_REGISTERS.iter())
        {
            assert!(
                *address < RK3588_MALI_BASE || *address >= RK3588_MALI_BASE + RK3588_MALI_LENGTH,
                "{address:#x} lies in the GPU aperture"
            );
            assert_eq!(address & 3, 0);
        }
    }
}
