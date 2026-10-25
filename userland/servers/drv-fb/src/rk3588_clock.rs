//j599

//! D0c: read-only CRU clock/reset inventory for the active RK3588 display path.
//!
//! Pure decoding over a [`ReadCru`] boundary. The trait has exactly one method and
//! no write counterpart, so nothing in this module can program a clock, gate or
//! reset. The live adapter maps the span in [`Grant`] Device/READ, samples each
//! watched register once, and discards the mapping.
//!
//! DESIGN/020's whole-page concern is *write* authority. A read-only mapping of a
//! wider CRU window grants no reach: the only operation here is a volatile 32-bit
//! load, and the kernel's `ResourceMintMmio` already requires WRITE to mint MMIO
//! at all, so the child Resource below is a read-only *mapping* by construction,
//! not a rights-narrowed Resource (that would need a kernel/ABI change).
//!
//! Sources: RK3588 TRM Part 1 ch02 (CRU register descriptions) and the board DTB's
//! clock/reset names; every offset and field is cited in
//! `RESEARCH/012-rk3588-display-clock-inventory.md`.

/// CRU operational base (TRM Part 1 ch02 §2.1 "The CRU is located at several addresses").
pub const CRU_BASE: u64 = 0xfd7c_0000;
/// Span: CRU (0x00000), PHPTOPCRU/PPLL (0x08000), PMU1CRU up to GATE_CON04.
/// PMU1CRU starts at 0xfd7f0000, i.e. offset 0x30000; GATE_CON04 is +0x810.
pub const CRU_LEN: u64 = 0x3_1000;
/// No IRQ authority rides the D0c grant; the child Resource requests (0, 0).
pub const CLOCK_PROBE_IRQ_BASE: u32 = 0;
pub const CLOCK_PROBE_IRQ_COUNT: u32 = 0;
/// Stream tag of the D0c handle message (the data message is the raw grant record).
pub const CLOCK_PROBE_TAG: u8 = b'K';

/// The one operation this module has on the CRU.
pub trait ReadCru {
    fn cru(&mut self, offset: u32) -> u32;
}

// ---------------------------------------------------------------------------
// Grant record
// ---------------------------------------------------------------------------

pub const GRANT_LEN: usize = 12;
pub const GRANT_VERSION: u8 = 1;
pub const GRANT_FLAG_READ_ONLY: u8 = 1;

/// Sora's description of the one child Resource granted to the clock probe.
/// The record carries only the span and the read-only intent: no IRQ base or
/// count, no rights word, no write flag. [`Grant::decode`] accepts exactly the
/// expected base/length, so a malformed or substituted span is never mapped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Grant {
    pub base: u32,
    pub len: u32,
}

impl Grant {
    pub const fn expected() -> Self {
        Self {
            base: CRU_BASE as u32,
            len: CRU_LEN as u32,
        }
    }

    pub fn encode(self) -> [u8; GRANT_LEN] {
        let base = self.base.to_le_bytes();
        let len = self.len.to_le_bytes();
        [
            GRANT_VERSION,
            GRANT_FLAG_READ_ONLY,
            0,
            0,
            base[0],
            base[1],
            base[2],
            base[3],
            len[0],
            len[1],
            len[2],
            len[3],
        ]
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != GRANT_LEN {
            return None;
        }
        if bytes[0] != GRANT_VERSION || bytes[1] != GRANT_FLAG_READ_ONLY {
            return None;
        }
        if bytes[2] != 0 || bytes[3] != 0 {
            return None;
        }
        let grant = Self {
            base: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            len: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        };
        if grant != Self::expected() {
            return None;
        }
        Some(grant)
    }
}

// ---------------------------------------------------------------------------
// Rates and unknown reasons
// ---------------------------------------------------------------------------

pub const XIN24M: u64 = 24_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rate {
    Hz(u64),
    Unknown(Unknown),
}

impl Rate {
    pub fn hz(self) -> Option<u64> {
        match self {
            Rate::Hz(hz) => Some(hz),
            Rate::Unknown(_) => None,
        }
    }

    pub fn unknown(self) -> u8 {
        match self {
            Rate::Hz(_) => Unknown::None as u8,
            Rate::Unknown(reason) => reason as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Unknown {
    None = 0,
    PllPoweredDown = 1,
    PllMalformed = 2,
    HdmiPhyPixel = 3,
    /// SPLL lives in SBUSCRU (secure); D0c deliberately does not read it.
    SecureSpllNotRead = 4,
    /// The HDMI RX reference source is outside the display TX path.
    HdmiRxRefsrc = 5,
    /// TRM names `aclk_vop_div2_src` only as a mux input; no register configures it.
    AclkVopDiv2Unverified = 6,
    /// Reserved selector encodings, or a selector wider than its parent list.
    ReservedSelector = 7,
    RateOverflow = 8,
}

pub fn unknown_name(reason: u8) -> &'static str {
    match reason {
        x if x == Unknown::None as u8 => "none",
        x if x == Unknown::PllPoweredDown as u8 => "pll-powered-down",
        x if x == Unknown::PllMalformed as u8 => "pll-malformed",
        x if x == Unknown::HdmiPhyPixel as u8 => "hdmi-phy-pixel",
        x if x == Unknown::SecureSpllNotRead as u8 => "secure-spll-not-read",
        x if x == Unknown::HdmiRxRefsrc as u8 => "hdmi-rx-refsrc",
        x if x == Unknown::AclkVopDiv2Unverified as u8 => "aclk-vop-div2-unverified",
        x if x == Unknown::ReservedSelector as u8 => "reserved-selector",
        x if x == Unknown::RateOverflow as u8 => "rate-overflow",
        _ => "unrecognised",
    }
}

// ---------------------------------------------------------------------------
// PLL decoding
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PllWords {
    pub con0: u32,
    pub con1: u32,
    pub con2: u32,
}

/// Nominal PLL rate from CON0..CON2.
///
/// TRM Part 1 ch02, e.g. CRU_GPLL_CON0/1/2:
/// - CON0[15] `bp` = 1 -> bypass, FOUT = FIN (24 MHz);
/// - CON0[9:0] M, 64..=1023;
/// - CON1[13] `resetb` = 1 -> power down (no output);
/// - CON1[8:6] S (0..=6), CON1[5:0] P (1..=63);
/// - CON2[15:0] K, a two's-complement DSM value;
/// - FOUT = FIN * (M + K/65536) / (P * 2^S).
///
/// Integer arithmetic truncates to whole Hz; no rate is invented when the
/// configured fields are outside the documented ranges.
pub fn decode_pll(words: PllWords) -> Rate {
    if words.con1 & (1 << 13) != 0 {
        return Rate::Unknown(Unknown::PllPoweredDown);
    }
    if words.con0 & (1 << 15) != 0 {
        return Rate::Hz(XIN24M);
    }
    let m = words.con0 & 0x3ff;
    let p = words.con1 & 0x3f;
    let s = (words.con1 >> 6) & 0x7;
    let k = (words.con2 & 0xffff) as u16 as i16 as i64;
    if m < 64 || p == 0 || s > 6 {
        return Rate::Unknown(Unknown::PllMalformed);
    }
    let numerator = i128::from(XIN24M) * (i128::from(m) * 65_536 + i128::from(k));
    let denominator = 65_536i128 * i128::from(p) * (1i128 << s);
    if numerator <= 0 {
        return Rate::Unknown(Unknown::PllMalformed);
    }
    match u64::try_from(numerator / denominator) {
        Ok(hz) => Rate::Hz(hz),
        Err(_) => Rate::Unknown(Unknown::RateOverflow),
    }
}

// ---------------------------------------------------------------------------
// Register sample
// ---------------------------------------------------------------------------

pub const O_CLKSEL0: u32 = 0x0300;
pub const O_CLKSEL1: u32 = 0x0304;
pub const O_CLKSEL8: u32 = 0x0320;
pub const O_CLKSEL110: u32 = 0x04b8;
pub const O_CLKSEL112: u32 = 0x04c0;
pub const O_CLKSEL115: u32 = 0x04cc;
pub const O_CLKSEL128: u32 = 0x0500;
pub const O_CLKSEL129: u32 = 0x0504;
pub const O_CLKSEL133: u32 = 0x0514;
pub const O_GATE0: u32 = 0x0800;
pub const O_GATE1: u32 = 0x0804;
pub const O_GATE52: u32 = 0x08d0;
pub const O_GATE53: u32 = 0x08d4;
pub const O_GATE59: u32 = 0x08ec;
pub const O_GATE60: u32 = 0x08f0;
pub const O_GATE61: u32 = 0x08f4;
pub const O_GATE72: u32 = 0x0920;
pub const O_GATE73: u32 = 0x0924;
pub const O_SOFTRST52: u32 = 0x0ad0;
pub const O_SOFTRST53: u32 = 0x0ad4;
pub const O_SOFTRST60: u32 = 0x0af0;
pub const O_SOFTRST61: u32 = 0x0af4;
pub const O_SOFTRST72: u32 = 0x0b20;
pub const O_SOFTRST73: u32 = 0x0b24;
pub const O_V0PLL: u32 = 0x0160;
pub const O_AUPLL: u32 = 0x0180;
pub const O_CPLL: u32 = 0x01a0;
pub const O_GPLL: u32 = 0x01c0;
pub const O_NPLL: u32 = 0x01e0;
pub const O_PPLL: u32 = 0x8200;
pub const O_PMU_CLKSEL14: u32 = 0x3_0338;
pub const O_PMU_GATE04: u32 = 0x3_0810;

/// One snapshot of every register D0c reads. Taking the sample up front means
/// each address is loaded exactly once, so a PLL or mux word cannot be observed
/// in two different states within one inventory.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub clksel0: u32,
    pub clksel1: u32,
    pub clksel8: u32,
    pub clksel110: u32,
    pub clksel112: u32,
    pub clksel115: u32,
    pub clksel128: u32,
    pub clksel129: u32,
    pub clksel133: u32,
    pub gate0: u32,
    pub gate1: u32,
    pub gate52: u32,
    pub gate53: u32,
    pub gate59: u32,
    pub gate60: u32,
    pub gate61: u32,
    pub gate72: u32,
    pub gate73: u32,
    pub softrst52: u32,
    pub softrst53: u32,
    pub softrst60: u32,
    pub softrst61: u32,
    pub softrst72: u32,
    pub softrst73: u32,
    pub v0pll: PllWords,
    pub aupll: PllWords,
    pub cpll: PllWords,
    pub gpll: PllWords,
    pub npll: PllWords,
    pub ppll: PllWords,
    pub pmu_clksel14: u32,
    pub pmu_gate04: u32,
}

impl Sample {
    pub fn read(regs: &mut impl ReadCru) -> Self {
        Self {
            clksel0: regs.cru(O_CLKSEL0),
            clksel1: regs.cru(O_CLKSEL1),
            clksel8: regs.cru(O_CLKSEL8),
            clksel110: regs.cru(O_CLKSEL110),
            clksel112: regs.cru(O_CLKSEL112),
            clksel115: regs.cru(O_CLKSEL115),
            clksel128: regs.cru(O_CLKSEL128),
            clksel129: regs.cru(O_CLKSEL129),
            clksel133: regs.cru(O_CLKSEL133),
            gate0: regs.cru(O_GATE0),
            gate1: regs.cru(O_GATE1),
            gate52: regs.cru(O_GATE52),
            gate53: regs.cru(O_GATE53),
            gate59: regs.cru(O_GATE59),
            gate60: regs.cru(O_GATE60),
            gate61: regs.cru(O_GATE61),
            gate72: regs.cru(O_GATE72),
            gate73: regs.cru(O_GATE73),
            softrst52: regs.cru(O_SOFTRST52),
            softrst53: regs.cru(O_SOFTRST53),
            softrst60: regs.cru(O_SOFTRST60),
            softrst61: regs.cru(O_SOFTRST61),
            softrst72: regs.cru(O_SOFTRST72),
            softrst73: regs.cru(O_SOFTRST73),
            v0pll: PllWords {
                con0: regs.cru(O_V0PLL),
                con1: regs.cru(O_V0PLL + 4),
                con2: regs.cru(O_V0PLL + 8),
            },
            aupll: PllWords {
                con0: regs.cru(O_AUPLL),
                con1: regs.cru(O_AUPLL + 4),
                con2: regs.cru(O_AUPLL + 8),
            },
            cpll: PllWords {
                con0: regs.cru(O_CPLL),
                con1: regs.cru(O_CPLL + 4),
                con2: regs.cru(O_CPLL + 8),
            },
            gpll: PllWords {
                con0: regs.cru(O_GPLL),
                con1: regs.cru(O_GPLL + 4),
                con2: regs.cru(O_GPLL + 8),
            },
            npll: PllWords {
                con0: regs.cru(O_NPLL),
                con1: regs.cru(O_NPLL + 4),
                con2: regs.cru(O_NPLL + 8),
            },
            ppll: PllWords {
                con0: regs.cru(O_PPLL),
                con1: regs.cru(O_PPLL + 4),
                con2: regs.cru(O_PPLL + 8),
            },
            pmu_clksel14: regs.cru(O_PMU_CLKSEL14),
            pmu_gate04: regs.cru(O_PMU_GATE04),
        }
    }
}

// ---------------------------------------------------------------------------
// Inventory records
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Parent {
    None = 0,
    Xin24M = 1,
    Gpll = 2,
    Cpll = 3,
    AuPll = 4,
    V0Pll = 5,
    NPll = 6,
    PPll = 7,
    Spll = 8,
    Matrix50M = 9,
    Matrix100M = 10,
    Matrix150M = 11,
    Matrix200M = 12,
    DclkVp2Src = 13,
    HdmiPhyPixel0 = 14,
    HdmiPhyPixel1 = 15,
    AclkVopRoot = 16,
    AclkVopDiv2 = 17,
    HclkVopRoot = 18,
    PclkVopRoot = 19,
    HclkVo1Root = 20,
    PclkVo1Root = 21,
    PclkTopRoot = 22,
    AclkHdcp1Root = 23,
}

pub fn parent_name(parent: u8) -> &'static str {
    match parent {
        x if x == Parent::None as u8 => "none",
        x if x == Parent::Xin24M as u8 => "xin24m",
        x if x == Parent::Gpll as u8 => "gpll",
        x if x == Parent::Cpll as u8 => "cpll",
        x if x == Parent::AuPll as u8 => "aupll",
        x if x == Parent::V0Pll as u8 => "v0pll",
        x if x == Parent::NPll as u8 => "npll",
        x if x == Parent::PPll as u8 => "ppll",
        x if x == Parent::Spll as u8 => "spll",
        x if x == Parent::Matrix50M as u8 => "clk_50m_src",
        x if x == Parent::Matrix100M as u8 => "clk_100m_src",
        x if x == Parent::Matrix150M as u8 => "clk_150m_src",
        x if x == Parent::Matrix200M as u8 => "clk_200m_src",
        x if x == Parent::DclkVp2Src as u8 => "dclk_vp2_src",
        x if x == Parent::HdmiPhyPixel0 as u8 => "hdmiphy0_pixel",
        x if x == Parent::HdmiPhyPixel1 as u8 => "hdmiphy1_pixel",
        x if x == Parent::AclkVopRoot as u8 => "aclk_vop_root",
        x if x == Parent::AclkVopDiv2 as u8 => "aclk_vop_div2_src",
        x if x == Parent::HclkVopRoot as u8 => "hclk_vop_root",
        x if x == Parent::PclkVopRoot as u8 => "pclk_vop_root",
        x if x == Parent::HclkVo1Root as u8 => "hclk_vo1_root",
        x if x == Parent::PclkVo1Root as u8 => "pclk_vo1_root",
        x if x == Parent::PclkTopRoot as u8 => "pclk_top_root",
        x if x == Parent::AclkHdcp1Root as u8 => "aclk_hdcp1_root",
        _ => "unknown",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ClockId {
    Gpll = 0,
    Cpll = 1,
    V0Pll = 2,
    AuPll = 3,
    NPll = 4,
    PPll = 5,
    Clk50MSrc = 6,
    Clk100MSrc = 7,
    Clk150MSrc = 8,
    Clk200MSrc = 9,
    DclkVp2Src = 10,
    DclkVp2 = 11,
    AclkVopRoot = 12,
    AclkVop = 13,
    HclkVopRoot = 14,
    HclkVop = 15,
    PclkVopRoot = 16,
    HclkVo1Root = 17,
    HclkVo1 = 18,
    PclkVo1Root = 19,
    PclkTopRoot = 20,
    AclkHdcp1Root = 21,
    PclkHdmiTx0 = 22,
    HdmiTx0Ref = 23,
    HdmiTx0Earc = 24,
    HdmiHdp0 = 25,
    PclkHdptx0 = 26,
    HdptxPhy0Ref = 27,
}

pub const CLOCK_COUNT: usize = 28;

pub fn clock_name(id: u8) -> &'static str {
    match id {
        x if x == ClockId::Gpll as u8 => "gpll",
        x if x == ClockId::Cpll as u8 => "cpll",
        x if x == ClockId::V0Pll as u8 => "v0pll",
        x if x == ClockId::AuPll as u8 => "aupll",
        x if x == ClockId::NPll as u8 => "npll",
        x if x == ClockId::PPll as u8 => "ppll",
        x if x == ClockId::Clk50MSrc as u8 => "clk_50m_src",
        x if x == ClockId::Clk100MSrc as u8 => "clk_100m_src",
        x if x == ClockId::Clk150MSrc as u8 => "clk_150m_src",
        x if x == ClockId::Clk200MSrc as u8 => "clk_200m_src",
        x if x == ClockId::DclkVp2Src as u8 => "dclk_vp2_src",
        x if x == ClockId::DclkVp2 as u8 => "dclk_vp2",
        x if x == ClockId::AclkVopRoot as u8 => "aclk_vop_root",
        x if x == ClockId::AclkVop as u8 => "aclk_vop",
        x if x == ClockId::HclkVopRoot as u8 => "hclk_vop_root",
        x if x == ClockId::HclkVop as u8 => "hclk_vop",
        x if x == ClockId::PclkVopRoot as u8 => "pclk_vop_root",
        x if x == ClockId::HclkVo1Root as u8 => "hclk_vo1_root",
        x if x == ClockId::HclkVo1 as u8 => "hclk_vo1",
        x if x == ClockId::PclkVo1Root as u8 => "pclk_vo1_root",
        x if x == ClockId::PclkTopRoot as u8 => "pclk_top_root",
        x if x == ClockId::AclkHdcp1Root as u8 => "aclk_hdcp1_root",
        x if x == ClockId::PclkHdmiTx0 as u8 => "pclk_hdmitx0",
        x if x == ClockId::HdmiTx0Ref as u8 => "clk_hdmitx0_ref",
        x if x == ClockId::HdmiTx0Earc as u8 => "clk_hdmitx0_earc",
        x if x == ClockId::HdmiHdp0 as u8 => "clk_hdmihdp0",
        x if x == ClockId::PclkHdptx0 as u8 => "pclk_hdptx0",
        x if x == ClockId::HdptxPhy0Ref as u8 => "clk_usb2phy_hdptxrxphy_ref",
        _ => "unknown",
    }
}

pub const KIND_PLL: u8 = 0;
pub const KIND_SELECTOR: u8 = 1;

pub const FLAG_GATE_DISABLED: u8 = 1 << 0;
pub const FLAG_RESET_ASSERTED: u8 = 1 << 1;
pub const FLAG_RATE_UNKNOWN: u8 = 1 << 2;
/// PLL K != 0: the rate is fractional, not parent/divider.
pub const FLAG_FRACTIONAL: u8 = 1 << 3;

/// `watched` bits: which of the gate/reset fields this record actually sampled.
/// Absence is distinct from "gate on"/"reset released".
pub const WATCH_GATE: u8 = 1 << 0;
pub const WATCH_RESET: u8 = 1 << 1;

/// Fixed-size record, one per watched clock. `detail` is the raw selector or
/// gate register word that governs this clock; for PLLs it is M | K << 10.
/// `divider` is the configured divide ratio (P * 2^S for a PLL).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClockRecord {
    pub id: u8,
    pub kind: u8,
    pub parent: u8,
    pub flags: u8,
    pub unknown: u8,
    pub watched: u8,
    pub divider: u16,
    pub rate_hz: u64,
    pub detail: u32,
}

impl ClockRecord {
    pub fn gate_disabled(&self) -> bool {
        self.flags & FLAG_GATE_DISABLED != 0
    }

    pub fn reset_asserted(&self) -> bool {
        self.flags & FLAG_RESET_ASSERTED != 0
    }

    pub fn gate_watched(&self) -> bool {
        self.watched & WATCH_GATE != 0
    }

    pub fn reset_watched(&self) -> bool {
        self.watched & WATCH_RESET != 0
    }

    pub fn rate(&self) -> Rate {
        if self.flags & FLAG_RATE_UNKNOWN != 0 {
            Rate::Unknown(unknown_from_u8(self.unknown))
        } else {
            Rate::Hz(self.rate_hz)
        }
    }
}

fn unknown_from_u8(value: u8) -> Unknown {
    match value {
        x if x == Unknown::PllPoweredDown as u8 => Unknown::PllPoweredDown,
        x if x == Unknown::PllMalformed as u8 => Unknown::PllMalformed,
        x if x == Unknown::HdmiPhyPixel as u8 => Unknown::HdmiPhyPixel,
        x if x == Unknown::SecureSpllNotRead as u8 => Unknown::SecureSpllNotRead,
        x if x == Unknown::HdmiRxRefsrc as u8 => Unknown::HdmiRxRefsrc,
        x if x == Unknown::AclkVopDiv2Unverified as u8 => Unknown::AclkVopDiv2Unverified,
        x if x == Unknown::ReservedSelector as u8 => Unknown::ReservedSelector,
        x if x == Unknown::RateOverflow as u8 => Unknown::RateOverflow,
        _ => Unknown::None,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Inventory {
    pub records: [ClockRecord; CLOCK_COUNT],
}

impl Inventory {
    pub fn get(&self, id: ClockId) -> &ClockRecord {
        &self.records[id as usize]
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

fn pll_record(id: ClockId, words: PllWords) -> ClockRecord {
    let rate = decode_pll(words);
    let m = words.con0 & 0x3ff;
    let k = words.con2 & 0xffff;
    let p = words.con1 & 0x3f;
    let s = (words.con1 >> 6) & 0x7;
    let divider = (p * (1 << s)) as u16;
    let mut flags = 0;
    if k != 0 {
        flags |= FLAG_FRACTIONAL;
    }
    if rate.hz().is_none() {
        flags |= FLAG_RATE_UNKNOWN;
    }
    ClockRecord {
        id: id as u8,
        kind: KIND_PLL,
        parent: Parent::Xin24M as u8,
        flags,
        unknown: rate.unknown(),
        watched: 0,
        divider,
        rate_hz: rate.hz().unwrap_or(0),
        detail: m | (k << 10),
    }
}

/// Divide a rate. An unknown parent propagates its reason unchanged.
fn divide(rate: Rate, divider: u32) -> Rate {
    if divider == 0 {
        return Rate::Unknown(Unknown::ReservedSelector);
    }
    match rate {
        Rate::Hz(hz) => Rate::Hz(hz / u64::from(divider)),
        Rate::Unknown(reason) => Rate::Unknown(reason),
    }
}

/// Select from a fixed parent list; a selector index outside the list is unknown.
fn select(parents: &[Rate], index: u32) -> Rate {
    match parents.get(index as usize) {
        Some(rate) => *rate,
        None => Rate::Unknown(Unknown::ReservedSelector),
    }
}

/// Composite mux+divider: `sel_shift`/`sel_width` choose the parent,
/// `div_shift`/`div_width` hold div_con (the divider is div_con + 1).
fn composite(
    word: u32,
    sel_shift: u32,
    sel_width: u32,
    div_shift: u32,
    div_width: u32,
    parents: &[Rate],
) -> Rate {
    let sel = (word >> sel_shift) & ((1 << sel_width) - 1);
    let div = ((word >> div_shift) & ((1 << div_width) - 1)) + 1;
    divide(select(parents, sel), div)
}

/// Clocks whose CRU reset bit D0c samples. Other selector records have no
/// corresponding reset field (or the DTB lists none); their record says so.
fn reset_watched(id: ClockId) -> bool {
    matches!(
        id,
        ClockId::DclkVp2
            | ClockId::AclkVop
            | ClockId::HclkVop
            | ClockId::PclkHdmiTx0
            | ClockId::HdmiTx0Ref
            | ClockId::HdmiHdp0
            | ClockId::PclkHdptx0
    )
}

fn selector_record(
    id: ClockId,
    parent: Parent,
    divider: u32,
    gate_disabled: bool,
    reset_asserted: bool,
    rate: Rate,
    detail: u32,
) -> ClockRecord {
    let mut flags = 0;
    if gate_disabled {
        flags |= FLAG_GATE_DISABLED;
    }
    if reset_asserted {
        flags |= FLAG_RESET_ASSERTED;
    }
    if rate.hz().is_none() {
        flags |= FLAG_RATE_UNKNOWN;
    }
    ClockRecord {
        id: id as u8,
        kind: KIND_SELECTOR,
        parent: parent as u8,
        flags,
        unknown: rate.unknown(),
        watched: WATCH_GATE | if reset_watched(id) { WATCH_RESET } else { 0 },
        divider: divider as u16,
        rate_hz: rate.hz().unwrap_or(0),
        detail,
    }
}

fn bit(word: u32, index: u32) -> bool {
    word & (1 << index) != 0
}

pub fn decode(sample: &Sample) -> Inventory {
    let gpll = decode_pll(sample.gpll);
    let cpll = decode_pll(sample.cpll);
    let v0pll = decode_pll(sample.v0pll);
    let aupll = decode_pll(sample.aupll);
    let npll = decode_pll(sample.npll);
    let ppll = decode_pll(sample.ppll);

    // Matrix (top) clocks: one-bit GPLL/CPLL mux plus a 5-bit divider.
    let clk50m = composite(sample.clksel0, 5, 1, 0, 5, &[gpll, cpll]);
    let clk100m = composite(sample.clksel0, 11, 1, 6, 5, &[gpll, cpll]);
    let clk150m = composite(sample.clksel1, 5, 1, 0, 5, &[gpll, cpll]);
    let clk200m = composite(sample.clksel1, 11, 1, 6, 5, &[gpll, cpll]);

    // Pixel path: dclk_vp2_src (PLL/div) -> dclk_vp2 (src / HDMI PHY pixel).
    let dclk_vp2_src = composite(sample.clksel112, 5, 2, 0, 5, &[gpll, cpll, v0pll, aupll]);
    let dclk_vp2_sel = (sample.clksel112 >> 11) & 0x3;
    let dclk_vp2_parent = match dclk_vp2_sel {
        0 => Parent::DclkVp2Src,
        1 => Parent::HdmiPhyPixel0,
        2 => Parent::HdmiPhyPixel1,
        _ => Parent::None,
    };
    let dclk_vp2 = match dclk_vp2_sel {
        0 => dclk_vp2_src,
        1 | 2 => Rate::Unknown(Unknown::HdmiPhyPixel),
        _ => Rate::Unknown(Unknown::ReservedSelector),
    };

    // VOP bus clocks. aclk_vop_root selects GPLL/CPLL/AUPLL/NPLL/SPLL.
    let aclk_vop_root = composite(
        sample.clksel110,
        5,
        3,
        0,
        5,
        &[
            gpll,
            cpll,
            aupll,
            npll,
            Rate::Unknown(Unknown::SecureSpllNotRead),
        ],
    );
    let (aclk_vop_parent, aclk_vop) = if bit(sample.clksel115, 9) {
        (
            Parent::AclkVopDiv2,
            Rate::Unknown(Unknown::AclkVopDiv2Unverified),
        )
    } else {
        (Parent::AclkVopRoot, aclk_vop_root)
    };
    let hclk_vop_root = composite(
        sample.clksel110,
        10,
        2,
        0,
        0,
        &[clk200m, clk100m, clk50m, Rate::Hz(XIN24M)],
    );
    let pclk_vop_root = composite(
        sample.clksel110,
        12,
        2,
        0,
        0,
        &[
            clk100m,
            clk50m,
            Rate::Hz(XIN24M),
            Rate::Unknown(Unknown::ReservedSelector),
        ],
    );

    // VO1 roots.
    let hclk_vo1_root = composite(
        sample.clksel128,
        13,
        2,
        0,
        0,
        &[clk200m, clk100m, clk50m, Rate::Hz(XIN24M)],
    );
    let pclk_vo1_root = composite(
        sample.clksel129,
        2,
        2,
        0,
        0,
        &[
            clk150m,
            clk100m,
            Rate::Hz(XIN24M),
            Rate::Unknown(Unknown::ReservedSelector),
        ],
    );
    let pclk_top_root = composite(
        sample.clksel8,
        7,
        2,
        0,
        0,
        &[
            clk100m,
            clk50m,
            Rate::Hz(XIN24M),
            Rate::Unknown(Unknown::ReservedSelector),
        ],
    );
    let aclk_hdcp1_root = composite(
        sample.clksel128,
        5,
        2,
        0,
        5,
        &[gpll, cpll, Rate::Unknown(Unknown::HdmiRxRefsrc)],
    );

    // HDMI0 clock set. GATE bits are active-high disable; SOFTRST bits are
    // active-high "reset relative logic" (TRM wording).
    let pclk_hdmitx0 = pclk_vo1_root;
    let hdmi_tx0_ref = aclk_hdcp1_root;
    let hdmi_tx0_earc = composite(sample.clksel133, 6, 1, 1, 5, &[gpll, cpll]);
    let hdmi_hdp0 = Rate::Hz(XIN24M);
    let pclk_hdptx0 = pclk_top_root;
    let hdptx_phy0_ref = composite(sample.pmu_clksel14, 14, 1, 9, 5, &[Rate::Hz(XIN24M), ppll]);

    let mut records = [ClockRecord::default(); CLOCK_COUNT];
    records[ClockId::Gpll as usize] = pll_record(ClockId::Gpll, sample.gpll);
    records[ClockId::Cpll as usize] = pll_record(ClockId::Cpll, sample.cpll);
    records[ClockId::V0Pll as usize] = pll_record(ClockId::V0Pll, sample.v0pll);
    records[ClockId::AuPll as usize] = pll_record(ClockId::AuPll, sample.aupll);
    records[ClockId::NPll as usize] = pll_record(ClockId::NPll, sample.npll);
    records[ClockId::PPll as usize] = pll_record(ClockId::PPll, sample.ppll);

    records[ClockId::Clk50MSrc as usize] = selector_record(
        ClockId::Clk50MSrc,
        if bit(sample.clksel0, 5) {
            Parent::Cpll
        } else {
            Parent::Gpll
        },
        (sample.clksel0 & 0x1f) + 1,
        bit(sample.gate0, 0),
        false,
        clk50m,
        sample.clksel0,
    );
    records[ClockId::Clk100MSrc as usize] = selector_record(
        ClockId::Clk100MSrc,
        if bit(sample.clksel0, 11) {
            Parent::Cpll
        } else {
            Parent::Gpll
        },
        ((sample.clksel0 >> 6) & 0x1f) + 1,
        bit(sample.gate0, 1),
        false,
        clk100m,
        sample.clksel0,
    );
    records[ClockId::Clk150MSrc as usize] = selector_record(
        ClockId::Clk150MSrc,
        if bit(sample.clksel1, 5) {
            Parent::Cpll
        } else {
            Parent::Gpll
        },
        (sample.clksel1 & 0x1f) + 1,
        bit(sample.gate0, 2),
        false,
        clk150m,
        sample.clksel1,
    );
    records[ClockId::Clk200MSrc as usize] = selector_record(
        ClockId::Clk200MSrc,
        if bit(sample.clksel1, 11) {
            Parent::Cpll
        } else {
            Parent::Gpll
        },
        ((sample.clksel1 >> 6) & 0x1f) + 1,
        bit(sample.gate0, 3),
        false,
        clk200m,
        sample.clksel1,
    );

    let dclk_vp2_src_sel = (sample.clksel112 >> 5) & 0x3;
    let dclk_vp2_src_parent = match dclk_vp2_src_sel {
        0 => Parent::Gpll,
        1 => Parent::Cpll,
        2 => Parent::V0Pll,
        _ => Parent::AuPll,
    };
    records[ClockId::DclkVp2Src as usize] = selector_record(
        ClockId::DclkVp2Src,
        dclk_vp2_src_parent,
        (sample.clksel112 & 0x1f) + 1,
        bit(sample.gate52, 12),
        false,
        dclk_vp2_src,
        sample.clksel112,
    );
    records[ClockId::DclkVp2 as usize] = selector_record(
        ClockId::DclkVp2,
        dclk_vp2_parent,
        1,
        bit(sample.gate53, 1),
        bit(sample.softrst53, 1),
        dclk_vp2,
        sample.clksel112,
    );

    records[ClockId::AclkVopRoot as usize] = selector_record(
        ClockId::AclkVopRoot,
        match (sample.clksel110 >> 5) & 0x7 {
            0 => Parent::Gpll,
            1 => Parent::Cpll,
            2 => Parent::AuPll,
            3 => Parent::NPll,
            4 => Parent::Spll,
            _ => Parent::None,
        },
        (sample.clksel110 & 0x1f) + 1,
        bit(sample.gate52, 0),
        false,
        aclk_vop_root,
        sample.clksel110,
    );
    records[ClockId::AclkVop as usize] = selector_record(
        ClockId::AclkVop,
        aclk_vop_parent,
        1,
        bit(sample.gate52, 9),
        bit(sample.softrst52, 9),
        aclk_vop,
        sample.clksel115,
    );
    records[ClockId::HclkVopRoot as usize] = selector_record(
        ClockId::HclkVopRoot,
        match (sample.clksel110 >> 10) & 0x3 {
            0 => Parent::Matrix200M,
            1 => Parent::Matrix100M,
            2 => Parent::Matrix50M,
            _ => Parent::Xin24M,
        },
        1,
        bit(sample.gate52, 2),
        false,
        hclk_vop_root,
        sample.clksel110,
    );
    records[ClockId::HclkVop as usize] = selector_record(
        ClockId::HclkVop,
        Parent::HclkVopRoot,
        1,
        bit(sample.gate52, 8),
        bit(sample.softrst52, 8),
        hclk_vop_root,
        sample.gate52,
    );
    records[ClockId::PclkVopRoot as usize] = selector_record(
        ClockId::PclkVopRoot,
        match (sample.clksel110 >> 12) & 0x3 {
            0 => Parent::Matrix100M,
            1 => Parent::Matrix50M,
            2 => Parent::Xin24M,
            _ => Parent::None,
        },
        1,
        bit(sample.gate52, 3),
        false,
        pclk_vop_root,
        sample.clksel110,
    );

    records[ClockId::HclkVo1Root as usize] = selector_record(
        ClockId::HclkVo1Root,
        match (sample.clksel128 >> 13) & 0x3 {
            0 => Parent::Matrix200M,
            1 => Parent::Matrix100M,
            2 => Parent::Matrix50M,
            _ => Parent::Xin24M,
        },
        1,
        bit(sample.gate59, 2),
        false,
        hclk_vo1_root,
        sample.clksel128,
    );
    records[ClockId::HclkVo1 as usize] = selector_record(
        ClockId::HclkVo1,
        Parent::HclkVo1Root,
        1,
        bit(sample.gate59, 9),
        false,
        hclk_vo1_root,
        sample.gate59,
    );
    records[ClockId::PclkVo1Root as usize] = selector_record(
        ClockId::PclkVo1Root,
        match (sample.clksel129 >> 2) & 0x3 {
            0 => Parent::Matrix150M,
            1 => Parent::Matrix100M,
            2 => Parent::Xin24M,
            _ => Parent::None,
        },
        1,
        bit(sample.gate59, 4),
        false,
        pclk_vo1_root,
        sample.clksel129,
    );
    records[ClockId::PclkTopRoot as usize] = selector_record(
        ClockId::PclkTopRoot,
        match (sample.clksel8 >> 7) & 0x3 {
            0 => Parent::Matrix100M,
            1 => Parent::Matrix50M,
            2 => Parent::Xin24M,
            _ => Parent::None,
        },
        1,
        bit(sample.gate1, 1),
        false,
        pclk_top_root,
        sample.clksel8,
    );
    records[ClockId::AclkHdcp1Root as usize] = selector_record(
        ClockId::AclkHdcp1Root,
        match (sample.clksel128 >> 5) & 0x3 {
            0 => Parent::Gpll,
            1 => Parent::Cpll,
            2 => Parent::None,
            _ => Parent::None,
        },
        (sample.clksel128 & 0x1f) + 1,
        bit(sample.gate59, 0),
        false,
        aclk_hdcp1_root,
        sample.clksel128,
    );

    records[ClockId::PclkHdmiTx0 as usize] = selector_record(
        ClockId::PclkHdmiTx0,
        Parent::PclkVo1Root,
        1,
        bit(sample.gate60, 11),
        bit(sample.softrst60, 11),
        pclk_hdmitx0,
        sample.gate60,
    );
    records[ClockId::HdmiTx0Ref as usize] = selector_record(
        ClockId::HdmiTx0Ref,
        Parent::AclkHdcp1Root,
        1,
        bit(sample.gate61, 0),
        bit(sample.softrst61, 0),
        hdmi_tx0_ref,
        sample.gate61,
    );
    records[ClockId::HdmiTx0Earc as usize] = selector_record(
        ClockId::HdmiTx0Earc,
        if bit(sample.clksel133, 6) {
            Parent::Cpll
        } else {
            Parent::Gpll
        },
        ((sample.clksel133 >> 1) & 0x1f) + 1,
        bit(sample.gate60, 15),
        false,
        hdmi_tx0_earc,
        sample.clksel133,
    );
    records[ClockId::HdmiHdp0 as usize] = selector_record(
        ClockId::HdmiHdp0,
        Parent::Xin24M,
        1,
        bit(sample.gate73, 12),
        bit(sample.softrst73, 12),
        hdmi_hdp0,
        sample.gate73,
    );
    records[ClockId::PclkHdptx0 as usize] = selector_record(
        ClockId::PclkHdptx0,
        Parent::PclkTopRoot,
        1,
        bit(sample.gate72, 5),
        bit(sample.softrst72, 5),
        pclk_hdptx0,
        sample.gate72,
    );
    records[ClockId::HdptxPhy0Ref as usize] = selector_record(
        ClockId::HdptxPhy0Ref,
        if bit(sample.pmu_clksel14, 14) {
            Parent::PPll
        } else {
            Parent::Xin24M
        },
        ((sample.pmu_clksel14 >> 9) & 0x1f) + 1,
        bit(sample.pmu_gate04, 7),
        false,
        hdptx_phy0_ref,
        sample.pmu_clksel14,
    );

    Inventory { records }
}

// ---------------------------------------------------------------------------
// The 1600x900@60 consistency observation
// ---------------------------------------------------------------------------

/// 1600x900 with H total 1800 and V total 1000 at 60 Hz implies exactly 108 MHz.
/// The refresh rate itself is NOT measured; this is arithmetic on D0a's geometry.
pub const IMPLIED_PIXEL_CLOCK_HZ: u64 = 1_800 * 1_000 * 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImpliedRatio {
    Equal,
    Times(u64),
    Divisor(u64),
    Other,
}

/// Ratio between the derived dclk_vp2 and the 108 MHz implied by the geometry.
/// This states the arithmetic relationship only; it does not claim a refresh
/// rate or that VOP-internal pixel dividers were sampled.
pub fn implied_ratio(rate_hz: u64) -> ImpliedRatio {
    if rate_hz == 0 {
        return ImpliedRatio::Other;
    }
    if rate_hz == IMPLIED_PIXEL_CLOCK_HZ {
        ImpliedRatio::Equal
    } else if rate_hz % IMPLIED_PIXEL_CLOCK_HZ == 0 {
        ImpliedRatio::Times(rate_hz / IMPLIED_PIXEL_CLOCK_HZ)
    } else if IMPLIED_PIXEL_CLOCK_HZ % rate_hz == 0 {
        ImpliedRatio::Divisor(IMPLIED_PIXEL_CLOCK_HZ / rate_hz)
    } else {
        ImpliedRatio::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pll(m: u32, p: u32, s: u32, k: u16) -> PllWords {
        PllWords {
            con0: m,
            con1: p | (s << 6),
            con2: u32::from(k),
        }
    }

    fn base_sample() -> Sample {
        Sample {
            gpll: pll(198, 2, 1, 0), // 1188 MHz
            cpll: pll(125, 1, 1, 0), // 1500 MHz
            v0pll: pll(54, 1, 0, 0), // 1296 MHz
            aupll: pll(32, 1, 0, 0), // 768 MHz
            npll: pll(85, 2, 0, 0),  // 1020 MHz
            ppll: pll(125, 3, 0, 0), // 1000 MHz
            ..Sample::default()
        }
    }

    // --- Grant record -----------------------------------------------------

    #[test]
    fn grant_round_trips_the_one_expected_span() {
        let grant = Grant::expected();
        let encoded = grant.encode();
        assert_eq!(encoded.len(), GRANT_LEN);
        assert_eq!(Grant::decode(&encoded), Some(grant));
        assert_eq!(grant.base, 0xfd7c_0000);
        assert_eq!(grant.len, 0x3_1000);
    }

    #[test]
    fn grant_rejects_absent_malformed_or_substituted_records() {
        let good = Grant::expected().encode();
        assert_eq!(Grant::decode(&[]), None);
        assert_eq!(Grant::decode(&[0]), None);
        assert_eq!(Grant::decode(&good[..GRANT_LEN - 1]), None);
        let mut longer = [0u8; GRANT_LEN + 1];
        longer[..GRANT_LEN].copy_from_slice(&good);
        assert_eq!(Grant::decode(&longer), None);

        let mut wrong_version = good;
        wrong_version[0] = 2;
        assert_eq!(Grant::decode(&wrong_version), None);

        let mut wrong_flags = good;
        wrong_flags[1] = GRANT_FLAG_READ_ONLY | 2;
        assert_eq!(Grant::decode(&wrong_flags), None);

        let mut write_flag = good;
        write_flag[1] = 2;
        assert_eq!(Grant::decode(&write_flag), None);

        let mut reserved = good;
        reserved[2] = 1;
        assert_eq!(Grant::decode(&reserved), None);

        let mut wrong_base = good;
        wrong_base[4] ^= 0x40;
        assert_eq!(Grant::decode(&wrong_base), None);

        let mut wrong_len = good;
        wrong_len[8] ^= 0x10;
        assert_eq!(Grant::decode(&wrong_len), None);
    }

    #[test]
    fn grant_carries_no_irq_or_extra_authority() {
        // The record is 12 bytes: version/flags/reserved plus base and length.
        assert_eq!(GRANT_LEN, 12);
        assert_eq!(CLOCK_PROBE_IRQ_BASE, 0);
        assert_eq!(CLOCK_PROBE_IRQ_COUNT, 0);
        // There is no write flag to smuggle in: only READ_ONLY is defined and
        // decode requires the flags byte to equal it exactly.
        assert_eq!(GRANT_FLAG_READ_ONLY, 1);
    }

    // --- PLL decoding -----------------------------------------------------

    #[test]
    fn pll_rates_follow_the_trm_formula() {
        assert_eq!(decode_pll(pll(198, 2, 1, 0)), Rate::Hz(1_188_000_000));
        assert_eq!(decode_pll(pll(125, 1, 1, 0)), Rate::Hz(1_500_000_000));
        assert_eq!(decode_pll(pll(125, 3, 0, 0)), Rate::Hz(1_000_000_000));
    }

    #[test]
    fn pll_bypass_is_24mhz_and_powerdown_is_unknown() {
        let bypass = PllWords {
            con0: (1 << 15) | 198,
            con1: 2 | (1 << 6),
            con2: 0,
        };
        assert_eq!(decode_pll(bypass), Rate::Hz(XIN24M));

        let down = PllWords {
            con0: 198,
            con1: (1 << 13) | 2 | (1 << 6),
            con2: 0,
        };
        assert_eq!(decode_pll(down), Rate::Unknown(Unknown::PllPoweredDown));
    }

    #[test]
    fn pll_malformed_fields_are_unknown_never_guessed() {
        assert_eq!(
            decode_pll(pll(63, 1, 0, 0)),
            Rate::Unknown(Unknown::PllMalformed)
        );
        assert_eq!(
            decode_pll(pll(64, 0, 0, 0)),
            Rate::Unknown(Unknown::PllMalformed)
        );
        assert_eq!(
            decode_pll(pll(64, 1, 7, 0)),
            Rate::Unknown(Unknown::PllMalformed)
        );
    }

    #[test]
    fn pll_fractional_k_is_signed_and_truncated_to_hz() {
        // K is a signed 16-bit DSM value. 24 MHz * (100 + 1/65536) truncated.
        let fractional = decode_pll(pll(100, 1, 0, 1));
        assert_eq!(fractional, Rate::Hz(2_400_000_366));

        // 786.432 MHz needs M >= 64: M=131, P=2, S=1, K=4719.
        let aupll_nominal = decode_pll(pll(131, 2, 1, 4719));
        assert_eq!(aupll_nominal, Rate::Hz(786_432_037));

        // Negative K: 24 MHz * (100 - 32768/65536) / 2 = 1.194 GHz.
        let negative = decode_pll(pll(100, 2, 0, 0x8000));
        assert_eq!(negative, Rate::Hz(1_194_000_000));
    }

    // --- selector decoding ------------------------------------------------

    #[test]
    fn dclk_vp2_gpll_div_11_is_108mhz() {
        let mut sample = base_sample();
        sample.clksel112 = 10; // src=GPLL, div_con=10 -> /11
        let inv = decode(&sample);
        let src = inv.get(ClockId::DclkVp2Src);
        assert_eq!(src.parent, Parent::Gpll as u8);
        assert_eq!(src.divider, 11);
        assert_eq!(src.rate(), Rate::Hz(108_000_000));
        assert!(!src.gate_disabled());
        let dclk = inv.get(ClockId::DclkVp2);
        assert_eq!(dclk.parent, Parent::DclkVp2Src as u8);
        assert_eq!(dclk.rate(), Rate::Hz(108_000_000));
        assert_eq!(implied_ratio(108_000_000), ImpliedRatio::Equal);
    }

    #[test]
    fn dclk_vp2_records_gate_and_reset_without_changing_the_rate() {
        let mut sample = base_sample();
        sample.clksel112 = 10;
        sample.gate52 = 1 << 12; // dclk_vp2_src disabled
        sample.gate53 = 1 << 1; // dclk_vp2 disabled
        sample.softrst53 = 1 << 1; // dresetn_vp2 asserted
        let inv = decode(&sample);
        assert!(inv.get(ClockId::DclkVp2Src).gate_disabled());
        assert!(inv.get(ClockId::DclkVp2).gate_disabled());
        assert!(inv.get(ClockId::DclkVp2).reset_asserted());
        assert_eq!(inv.get(ClockId::DclkVp2).rate(), Rate::Hz(108_000_000));
    }

    #[test]
    fn dclk_vp2_hdmi_phy_source_is_explicitly_unknown() {
        for (sel, parent) in [
            (1 << 11, Parent::HdmiPhyPixel0),
            (2 << 11, Parent::HdmiPhyPixel1),
        ] {
            let mut sample = base_sample();
            sample.clksel112 = sel;
            let inv = decode(&sample);
            let dclk = inv.get(ClockId::DclkVp2);
            assert_eq!(dclk.parent, parent as u8);
            assert_eq!(dclk.rate(), Rate::Unknown(Unknown::HdmiPhyPixel));
            assert_ne!(dclk.flags & FLAG_RATE_UNKNOWN, 0);
        }
        let mut reserved = base_sample();
        reserved.clksel112 = 3 << 11;
        assert_eq!(
            decode(&reserved).get(ClockId::DclkVp2).rate(),
            Rate::Unknown(Unknown::ReservedSelector)
        );
    }

    #[test]
    fn unknown_pll_reason_propagates_through_the_pixel_divider() {
        let mut sample = base_sample();
        sample.gpll = PllWords {
            con0: 198,
            con1: (1 << 13) | 2 | (1 << 6),
            con2: 0,
        };
        sample.clksel112 = 10; // GPLL / 11
        let inv = decode(&sample);
        assert_eq!(
            inv.get(ClockId::DclkVp2Src).rate(),
            Rate::Unknown(Unknown::PllPoweredDown)
        );
        assert_eq!(
            inv.get(ClockId::DclkVp2).rate(),
            Rate::Unknown(Unknown::PllPoweredDown)
        );
    }

    #[test]
    fn vop_root_and_sub_mux_are_decoded() {
        let mut sample = base_sample();
        sample.clksel110 = (0 << 5) | 2; // aclk_vop_root = GPLL / 3
        sample.clksel115 = 0;
        let inv = decode(&sample);
        assert_eq!(inv.get(ClockId::AclkVopRoot).rate(), Rate::Hz(396_000_000));
        assert_eq!(inv.get(ClockId::AclkVop).rate(), Rate::Hz(396_000_000));
        assert_eq!(inv.get(ClockId::AclkVop).parent, Parent::AclkVopRoot as u8);

        // aclk_vop_div2_src has no configuration register in the pinned TRM.
        sample.clksel115 = 1 << 9;
        let inv = decode(&sample);
        assert_eq!(
            inv.get(ClockId::AclkVop).rate(),
            Rate::Unknown(Unknown::AclkVopDiv2Unverified)
        );

        // SPLL is in SBUSCRU and deliberately not read.
        sample.clksel110 = 4 << 5;
        sample.clksel115 = 0;
        let inv = decode(&sample);
        assert_eq!(
            inv.get(ClockId::AclkVopRoot).rate(),
            Rate::Unknown(Unknown::SecureSpllNotRead)
        );
    }

    #[test]
    fn matrix_clocks_use_their_own_mux_and_divider() {
        let mut sample = base_sample();
        // clk_100m_src = CPLL / 15 = 100 MHz (CON0 [11], [10:6]).
        sample.clksel0 = (1 << 11) | (14 << 6);
        // clk_200m_src = GPLL / 6 = 198 MHz (CON1 [11], [10:6]).
        sample.clksel1 = (0 << 11) | (5 << 6);
        let inv = decode(&sample);
        assert_eq!(inv.get(ClockId::Clk100MSrc).rate(), Rate::Hz(100_000_000));
        assert_eq!(inv.get(ClockId::Clk100MSrc).divider, 15);
        assert_eq!(inv.get(ClockId::Clk200MSrc).rate(), Rate::Hz(198_000_000));
        assert_eq!(inv.get(ClockId::Clk200MSrc).divider, 6);

        // hclk_vop_root selects clk_200m_src; hclk_vop inherits it.
        sample.clksel110 = 0 << 10;
        let inv = decode(&sample);
        assert_eq!(
            inv.get(ClockId::HclkVopRoot).parent,
            Parent::Matrix200M as u8
        );
        assert_eq!(inv.get(ClockId::HclkVopRoot).rate(), Rate::Hz(198_000_000));
        assert_eq!(inv.get(ClockId::HclkVop).rate(), Rate::Hz(198_000_000));
        assert_eq!(inv.get(ClockId::HclkVop).parent, Parent::HclkVopRoot as u8);
    }

    #[test]
    fn hdmi0_clock_set_decodes_parents_gates_and_resets() {
        let mut sample = base_sample();
        // pclk_vo1_root = clk_100m_src (100 MHz) after the matrix setup above.
        sample.clksel0 = (1 << 11) | (14 << 6);
        sample.clksel129 = 1 << 2;
        // clk_hdmitx0_ref = GPLL / 2 = 594 MHz (div_con = 1).
        sample.clksel128 = 1;
        // earc = CPLL / 15 = 100 MHz.
        sample.clksel133 = (1 << 6) | (14 << 1);
        sample.gate60 = (1 << 11) | (1 << 15);
        sample.softrst60 = 1 << 11;
        sample.gate61 = 1 << 0;
        sample.softrst61 = 1 << 0;
        sample.gate73 = 1 << 12;
        sample.softrst73 = 1 << 12;
        let inv = decode(&sample);

        let pclk = inv.get(ClockId::PclkHdmiTx0);
        assert_eq!(pclk.parent, Parent::PclkVo1Root as u8);
        assert_eq!(pclk.rate(), Rate::Hz(100_000_000));
        assert!(pclk.gate_disabled());
        assert!(pclk.reset_asserted());

        let reference = inv.get(ClockId::HdmiTx0Ref);
        assert_eq!(reference.parent, Parent::AclkHdcp1Root as u8);
        assert_eq!(reference.rate(), Rate::Hz(594_000_000));
        assert!(reference.gate_disabled());
        assert!(reference.reset_asserted());

        let earc = inv.get(ClockId::HdmiTx0Earc);
        assert_eq!(earc.parent, Parent::Cpll as u8);
        assert_eq!(earc.divider, 15);
        assert_eq!(earc.rate(), Rate::Hz(100_000_000));
        assert!(earc.gate_disabled());

        let hdp = inv.get(ClockId::HdmiHdp0);
        assert_eq!(hdp.parent, Parent::Xin24M as u8);
        assert_eq!(hdp.rate(), Rate::Hz(XIN24M));
        assert!(hdp.gate_disabled());
        assert!(hdp.reset_asserted());
    }

    #[test]
    fn hdmi_ref_reports_the_unread_hdmi_rx_source_as_unknown() {
        let mut sample = base_sample();
        sample.clksel128 = 2 << 5;
        let inv = decode(&sample);
        assert_eq!(
            inv.get(ClockId::AclkHdcp1Root).rate(),
            Rate::Unknown(Unknown::HdmiRxRefsrc)
        );
        assert_eq!(
            inv.get(ClockId::HdmiTx0Ref).rate(),
            Rate::Unknown(Unknown::HdmiRxRefsrc)
        );
    }

    #[test]
    fn phy_reference_decodes_pmu1cru_mux_divider_and_gate() {
        let mut sample = base_sample();
        // PPLL / 5 = 200 MHz; PMU1CRU CLKSEL_CON14 [14] and [13:9].
        sample.pmu_clksel14 = (1 << 14) | (4 << 9);
        sample.pmu_gate04 = 0;
        let inv = decode(&sample);
        let phy = inv.get(ClockId::HdptxPhy0Ref);
        assert_eq!(phy.parent, Parent::PPll as u8);
        assert_eq!(phy.divider, 5);
        assert_eq!(phy.rate(), Rate::Hz(200_000_000));
        assert!(!phy.gate_disabled());

        sample.pmu_clksel14 = 0; // xin24m
        sample.pmu_gate04 = 1 << 7;
        let inv = decode(&sample);
        let phy = inv.get(ClockId::HdptxPhy0Ref);
        assert_eq!(phy.parent, Parent::Xin24M as u8);
        assert_eq!(phy.rate(), Rate::Hz(XIN24M));
        assert!(phy.gate_disabled());
    }

    #[test]
    fn pclk_hdptx0_inherits_pclk_top_root_and_records_its_reset() {
        let mut sample = base_sample();
        sample.clksel8 = 2 << 7; // xin24m
        sample.gate72 = 1 << 5;
        sample.softrst72 = 1 << 5;
        let inv = decode(&sample);
        let pclk = inv.get(ClockId::PclkHdptx0);
        assert_eq!(pclk.parent, Parent::PclkTopRoot as u8);
        assert_eq!(pclk.rate(), Rate::Hz(XIN24M));
        assert!(pclk.gate_disabled());
        assert!(pclk.reset_asserted());
    }

    // --- inventory shape and sampling discipline --------------------------

    #[test]
    fn every_record_is_present_in_enum_order_and_fixed_size() {
        use core::mem::size_of;
        assert_eq!(size_of::<ClockRecord>(), 24);
        let inv = decode(&base_sample());
        for (index, record) in inv.records.iter().enumerate() {
            assert_eq!(record.id as usize, index);
            assert_ne!(record.kind, u8::MAX);
        }
        assert_eq!(CLOCK_COUNT, 28);
        assert!(inv.get(ClockId::DclkVp2).gate_watched());
        assert!(inv.get(ClockId::DclkVp2).reset_watched());
        assert!(inv.get(ClockId::AclkVopRoot).gate_watched());
        assert!(!inv.get(ClockId::AclkVopRoot).reset_watched());
        assert!(!inv.get(ClockId::Gpll).gate_watched());
        assert!(!inv.get(ClockId::Gpll).reset_watched());
        assert_eq!(inv.get(ClockId::DclkVp2).id, ClockId::DclkVp2 as u8);
    }

    #[test]
    fn implied_ratio_states_only_integer_relationships() {
        assert_eq!(implied_ratio(0), ImpliedRatio::Other);
        assert_eq!(implied_ratio(108_000_000), ImpliedRatio::Equal);
        assert_eq!(implied_ratio(216_000_000), ImpliedRatio::Times(2));
        assert_eq!(implied_ratio(54_000_000), ImpliedRatio::Divisor(2));
        assert_eq!(implied_ratio(100_000_000), ImpliedRatio::Other);
    }

    struct Fake {
        pairs: [(u32, u32); 64],
        count: usize,
        reads: [u32; 64],
        read_count: usize,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                pairs: [(0, 0); 64],
                count: 0,
                reads: [0; 64],
                read_count: 0,
            }
        }

        fn set(&mut self, offset: u32, value: u32) {
            self.pairs[self.count] = (offset, value);
            self.count += 1;
        }

        fn value(&self, offset: u32) -> u32 {
            for index in 0..self.count {
                if self.pairs[index].0 == offset {
                    return self.pairs[index].1;
                }
            }
            panic!("unset CRU offset {offset:#x}");
        }
    }

    impl ReadCru for Fake {
        fn cru(&mut self, offset: u32) -> u32 {
            self.reads[self.read_count] = offset;
            self.read_count += 1;
            self.value(offset)
        }
    }

    #[test]
    fn sample_reads_each_watched_register_exactly_once() {
        let mut fake = Fake::new();
        for offset in [
            O_CLKSEL0,
            O_CLKSEL1,
            O_CLKSEL8,
            O_CLKSEL110,
            O_CLKSEL112,
            O_CLKSEL115,
            O_CLKSEL128,
            O_CLKSEL129,
            O_CLKSEL133,
            O_GATE0,
            O_GATE1,
            O_GATE52,
            O_GATE53,
            O_GATE59,
            O_GATE60,
            O_GATE61,
            O_GATE72,
            O_GATE73,
            O_SOFTRST52,
            O_SOFTRST53,
            O_SOFTRST60,
            O_SOFTRST61,
            O_SOFTRST72,
            O_SOFTRST73,
            O_V0PLL,
            O_V0PLL + 4,
            O_V0PLL + 8,
            O_AUPLL,
            O_AUPLL + 4,
            O_AUPLL + 8,
            O_CPLL,
            O_CPLL + 4,
            O_CPLL + 8,
            O_GPLL,
            O_GPLL + 4,
            O_GPLL + 8,
            O_NPLL,
            O_NPLL + 4,
            O_NPLL + 8,
            O_PPLL,
            O_PPLL + 4,
            O_PPLL + 8,
            O_PMU_CLKSEL14,
            O_PMU_GATE04,
        ] {
            fake.set(offset, 0);
        }
        let sample = Sample::read(&mut fake);
        assert_eq!(fake.read_count, 44);
        for index in 0..fake.read_count {
            let offset = fake.reads[index];
            assert_eq!(offset % 4, 0, "unaligned read {offset:#x}");
            assert!(u64::from(offset) + 4 <= CRU_LEN);
            for earlier in 0..index {
                assert_ne!(fake.reads[earlier], offset, "double read {offset:#x}");
            }
        }
        // Zeroed PLLs are malformed, not an invented rate.
        assert_eq!(
            decode(&sample).get(ClockId::Gpll).rate(),
            Rate::Unknown(Unknown::PllMalformed)
        );
    }
}
