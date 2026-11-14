//j893
//! drv-thermal: the RK3588 TS-ADC (temperature) driver, and later the fan curve.
//!
//! T0 (j893) reads the seven on-die sensors and reports them; the fan is not touched. The pure
//! parts live here so the host can test them: the code-to-temperature table, the clock and reset
//! checks, the TS-ADC start sequence, the live-DTB gate Sora launches by, and the status record
//! the shell's `thermal` word reads. Every number comes from the RK3588 TRM part 1 (chapter 2
//! CRU, chapter 6 GRF/IOC, chapter 14 TS-ADC) and the board's live DTB; the ratified cut is
//! `build/verification/thermal/thermal-cut.md` (rev 2, CRUX 2026-11-14). — TEAL

#![no_std]

use core::fmt::{self, Write};

/// `tsadc@fec00000`: the controller's registers end at 0x220, one page.
pub const TSADC_BASE: u64 = 0xfec0_0000;
pub const TSADC_LEN: u64 = 0x1000;
pub const TSADC_COMPATIBLE: &[u8] = b"rockchip,rk3588-tsadc";

/// The CRU's first page: TS-ADC and PWM block 3 clock, gate and reset words all sit below 0x1000.
/// Granted read-only; nothing here writes the CRU (the reset defaults are what the sensor needs).
pub const CRU_BASE: u64 = 0xfd7c_0000;
pub const CRU_LEN: u64 = 0x1000;

/// The pad blocks and PWM block T1 programs; T0 only reads them, to report the state it found.
pub const BUS_IOC_BASE: u64 = 0xfd5f_8000;
pub const VCCIO3_5_IOC_BASE: u64 = 0xfd5f_a000;
/// `pwm@febf0000`: PWM12..15. PWM12 drives the fan (header pin 16), PWM13 captures its tach (18).
pub const PWM_BASE: u64 = 0xfebf_0000;
pub const PAGE_LEN: u64 = 0x1000;

pub const CHANNELS: usize = 7;
/// TRM 14.1 and the DTB's thermal zones, in channel order.
pub const CHANNEL_NAMES: [&str; CHANNELS] =
    ["package", "a76-01", "a76-23", "a55", "center", "gpu", "npu"];

/// TS-ADC register offsets (TRM 14.4).
pub mod tsadc {
    pub const AUTO_CON: u32 = 0x04;
    pub const AUTO_STATUS: u32 = 0x08;
    pub const AUTO_SRC: u32 = 0x0c;
    pub const GPIO_EN: u32 = 0x18;
    pub const CRU_EN: u32 = 0x1c;
    pub const DATA0: u32 = 0x2c;
    pub const COMP0_SHUT: u32 = 0x10c;
    pub const AUTO_PERIOD: u32 = 0x154;
    /// `AUTO_CON[0]`: auto conversion on. `[1]`: q_sel, inverted output (negative coefficient).
    pub const AUTO_EN: u32 = 1 << 0;
    pub const Q_SEL: u32 = 1 << 1;
    /// `AUTO_STATUS[2]`: auto mode in progress.
    pub const AUTO_RUNNING: u32 = 1 << 2;
    /// Channels 0..6, the low half of `AUTO_SRC` (upper half is the write mask).
    pub const ALL_CHANNELS: u32 = (1 << super::CHANNELS) - 1;
    /// Twelve-bit conversion result.
    pub const DATA_MASK: u32 = 0xfff;

    pub const fn data(channel: usize) -> u32 {
        DATA0 + 4 * channel as u32
    }

    pub const fn comp_shut(channel: usize) -> u32 {
        COMP0_SHUT + 4 * channel as u32
    }
}

/// CRU offsets (TRM 2.x, main CRU at `CRU_BASE`).
pub mod cru {
    /// `[8]` clk_tsadc mux (1 = OSC 24 MHz), `[7:0]` divider (`div + 1`). Reset: OSC / 12.
    pub const CLKSEL_CON41: u32 = 0x3a4;
    /// `[1:0]` clk_pwm3 mux (2 = OSC 24 MHz). Reset: 2.
    pub const CLKSEL_CON60: u32 = 0x3f0;
    /// `[0]` pclk_tsadc, `[1]` clk_tsadc gates (1 = clock off).
    pub const GATE_CON12: u32 = 0x830;
    /// `[9]` pclk_pwm3, `[10]` clk_pwm3, `[11]` clk_pwm3_capture gates.
    pub const GATE_CON15: u32 = 0x83c;
    /// `[0]` presetn_tsadc, `[1]` resetn_tsadc (1 = held in reset).
    pub const SOFTRST_CON12: u32 = 0xa30;
    /// `[9]` presetn_pwm3, `[10]` resetn_pwm3.
    pub const SOFTRST_CON15: u32 = 0xa3c;
    /// `[1]` tsadc_trig_glbrst_en: a TS-ADC shutdown request resets the chip.
    pub const GLB_RST_CON: u32 = 0xc10;
}

/// `BUS_IOC_GPIO3B_IOMUX_SEL_H` (GPIO3_B4..B7, four bits each) and the pull word for GPIO3_B.
pub const BUS_IOC_GPIO3B_IOMUX_SEL_H: u32 = 0x6c;
pub const VCCIO3_5_IOC_GPIO3B_P: u32 = 0x134;

/// PWM block offsets for channel `n` within the block (TRM 18.4): PWM12 is `n = 0`.
pub mod pwm {
    pub const fn period(channel: u32) -> u32 {
        0x04 + 0x10 * channel
    }
    pub const fn duty(channel: u32) -> u32 {
        0x08 + 0x10 * channel
    }
    pub const fn ctrl(channel: u32) -> u32 {
        0x0c + 0x10 * channel
    }
    pub const INTSTS: u32 = 0x40;
    pub const VERSION: u32 = 0x5c;
}

/// TRM Table 14-1, "Temperature Code Mapping": (code, m°C), linear between the points. Linux's
/// table (215/285/350/395) reads about 4 °C cooler near 85 °C; for a fan the TRM's is the safe
/// side.
pub const CODE_TABLE: [(u32, i32); 4] =
    [(220, -40_000), (285, 25_000), (345, 85_000), (385, 125_000)];

/// One channel's conversion result in m°C, or `None` when the code lies outside the table, which
/// the sensor cannot produce while it works (0 is "never converted").
pub fn code_to_millicelsius(code: u32) -> Option<i32> {
    let code = code & tsadc::DATA_MASK;
    let (first, last) = (CODE_TABLE[0], CODE_TABLE[CODE_TABLE.len() - 1]);
    if code < first.0 || code > last.0 {
        return None;
    }
    let mut lower = first;
    for upper in CODE_TABLE.iter().copied().skip(1) {
        if code <= upper.0 {
            let span = (upper.0 - lower.0) as i32;
            let step = (code - lower.0) as i32;
            return Some(lower.1 + (upper.1 - lower.1) * step / span);
        }
        lower = upper;
    }
    None
}

/// The CRU words the driver depends on, as read. Gates and resets must be open before any
/// TS-ADC (or PWM) register is touched: an APB access to a gated block can hang the bus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clocks {
    pub clksel41: u32,
    pub clksel60: u32,
    pub gate12: u32,
    pub gate15: u32,
    pub softrst12: u32,
    pub softrst15: u32,
    pub glb_rst: u32,
}

impl Clocks {
    pub fn read(mut read: impl FnMut(u32) -> u32) -> Self {
        Self {
            clksel41: read(cru::CLKSEL_CON41),
            clksel60: read(cru::CLKSEL_CON60),
            gate12: read(cru::GATE_CON12),
            gate15: read(cru::GATE_CON15),
            softrst12: read(cru::SOFTRST_CON12),
            softrst15: read(cru::SOFTRST_CON15),
            glb_rst: read(cru::GLB_RST_CON),
        }
    }

    /// pclk and clk ungated, both resets released.
    pub fn tsadc_live(&self) -> bool {
        self.gate12 & 0b11 == 0 && self.softrst12 & 0b11 == 0
    }

    /// clk_tsadc in Hz when it comes from the 24 MHz oscillator; `None` for the GPLL branch,
    /// whose rate this driver does not know.
    pub fn tsadc_hz(&self) -> Option<u32> {
        if self.clksel41 & (1 << 8) == 0 {
            return None;
        }
        Some(24_000_000 / ((self.clksel41 & 0xff) + 1))
    }

    /// TRM 14.5.2: CLK_DEM_TS "should be set to 2MHz". The reset default (OSC / 12) is.
    pub fn tsadc_ready(&self) -> bool {
        self.tsadc_live() && self.tsadc_hz() == Some(2_000_000)
    }

    /// pclk, clk and capture clock of PWM block 3 ungated, resets released.
    pub fn pwm3_live(&self) -> bool {
        self.gate15 & (0b111 << 9) == 0 && self.softrst15 & (0b11 << 9) == 0
    }

    /// clk_pwm3 in Hz when it is the 24 MHz oscillator (the reset default).
    pub fn pwm3_hz(&self) -> Option<u32> {
        (self.clksel60 & 0b11 == 2).then_some(24_000_000)
    }

    /// Whether a TS-ADC shutdown request would reset the chip (`CRU_GLB_RST_CON[1]`).
    pub fn tshut_reset_armed(&self) -> bool {
        self.glb_rst & (1 << 1) != 0
    }
}

/// Register access the start sequence needs; the binary maps the page, the tests record.
pub trait Regs {
    fn read(&mut self, offset: u32) -> u32;
    fn write(&mut self, offset: u32, value: u32);
}

/// The TS-ADC as the driver found it, before touching it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub auto_con: u32,
    pub auto_status: u32,
    pub auto_src: u32,
    pub gpio_en: u32,
    pub cru_en: u32,
    pub period: u32,
    pub shut0: u32,
}

impl Found {
    pub fn read(regs: &mut impl Regs) -> Self {
        Self {
            auto_con: regs.read(tsadc::AUTO_CON),
            auto_status: regs.read(tsadc::AUTO_STATUS),
            auto_src: regs.read(tsadc::AUTO_SRC),
            gpio_en: regs.read(tsadc::GPIO_EN),
            cru_en: regs.read(tsadc::CRU_EN),
            period: regs.read(tsadc::AUTO_PERIOD),
            shut0: regs.read(tsadc::comp_shut(0)),
        }
    }

    /// Already converting all seven channels in the orientation [`CODE_TABLE`] assumes.
    pub fn usable(&self) -> bool {
        self.auto_con & (tsadc::AUTO_EN | tsadc::Q_SEL) == tsadc::AUTO_EN
            && self.auto_src & tsadc::ALL_CHANNELS == tsadc::ALL_CHANNELS
    }
}

/// TRM 14.5.1: select the channels, then set `AUTO_CON[0]`. Auto mode stops first so the source
/// change lands between rounds, and q_sel is cleared so codes rise with temperature. Every write
/// is hi-word masked: the tshut polarity, the firmware's shutdown thresholds and its
/// GPIO/CRU shutdown enables are left exactly as found. Returns false when the controller was
/// already usable and nothing was written.
pub fn start_auto(regs: &mut impl Regs, found: &Found) -> bool {
    if found.usable() {
        return false;
    }
    let mode_mask = (tsadc::AUTO_EN | tsadc::Q_SEL) << 16;
    regs.write(tsadc::AUTO_CON, mode_mask);
    regs.write(
        tsadc::AUTO_SRC,
        (tsadc::ALL_CHANNELS << 16) | tsadc::ALL_CHANNELS,
    );
    regs.write(tsadc::AUTO_CON, mode_mask | tsadc::AUTO_EN);
    true
}

/// One reading of all seven channels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub millicelsius: [Option<i32>; CHANNELS],
}

impl Sample {
    pub fn read(regs: &mut impl Regs) -> Self {
        let mut sample = Self::default();
        for (channel, slot) in sample.millicelsius.iter_mut().enumerate() {
            *slot = code_to_millicelsius(regs.read(tsadc::data(channel)));
        }
        sample
    }

    pub fn complete(&self) -> bool {
        self.millicelsius.iter().all(Option::is_some)
    }

    /// The hottest valid channel.
    pub fn max(&self) -> Option<i32> {
        self.millicelsius.iter().flatten().copied().max()
    }
}

/// True when the live DTB describes the RK3588 TS-ADC this driver knows, enabled, at
/// [`TSADC_BASE`]. Sora launches drv-thermal only then: QEMU, the X13s and x86 have none.
pub fn tsadc_in_dtb(dtb: &[u8]) -> bool {
    let Some(fdt) = kumo_fdt::Fdt::new(dtb) else {
        return false;
    };
    fdt.nodes().any(|node| {
        node.property(b"compatible")
            .is_some_and(|list| kumo_fdt::string_list_contains(list, TSADC_COMPATIBLE))
            && kumo_fdt::status_enabled(node)
            && node.property(b"reg").is_some_and(|reg| {
                reg.len() >= 16
                    && reg[..8] == TSADC_BASE.to_be_bytes()
                    && u64::from_be_bytes(reg[8..16].try_into().unwrap_or([0; 8])) >= 0x220
            })
    })
}

/// Bootstrap tags on the channel Sora hands over in x1. Each grant is one message whose single
/// tag byte names the handle it carries; [`BOOT_GO`] (no handle) ends the grants, and from then
/// on the same channel carries [`STATUS_REQUEST`]s.
pub const BOOT_CRU: u8 = b'K';
pub const BOOT_BUS_IOC: u8 = b'I';
pub const BOOT_VCCIO3_5_IOC: u8 = b'V';
pub const BOOT_PWM: u8 = b'P';
pub const BOOT_GO: u8 = b'G';

/// The shell's request byte; the reply is one [`STATUS_LEN`]-byte record.
pub const STATUS_REQUEST: u8 = b'S';
pub const STATUS_LEN: usize = 32;
pub const STATUS_MAGIC: [u8; 4] = *b"THM1";
pub const STATUS_VERSION: u8 = 1;
/// A channel without a usable reading.
pub const NO_READING: i16 = i16::MIN;

/// `Status::flags`.
pub const FLAG_SENSOR_OK: u8 = 1 << 0;
pub const FLAG_FAN_DRIVEN: u8 = 1 << 1;
pub const FLAG_FAILSAFE: u8 = 1 << 2;
pub const FLAG_STALL: u8 = 1 << 3;
pub const FLAG_CLOCK_FAULT: u8 = 1 << 4;

/// The record the `thermal` word prints. T0 fills temperatures and `samples`; duty and rpm stay
/// zero until T1 drives the fan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub flags: u8,
    pub duty_permille: u16,
    pub celsius: [i16; CHANNELS],
    pub rpm: u16,
    pub samples: u32,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            flags: 0,
            duty_permille: 0,
            celsius: [NO_READING; CHANNELS],
            rpm: 0,
            samples: 0,
        }
    }
}

impl Status {
    /// Whole degrees from a sample (the TRM table resolves 1 °C).
    pub fn set_sample(&mut self, sample: &Sample) {
        for (out, reading) in self.celsius.iter_mut().zip(sample.millicelsius) {
            *out = reading.map_or(NO_READING, |m| (m / 1000) as i16);
        }
    }

    pub fn max(&self) -> Option<i16> {
        self.celsius
            .iter()
            .copied()
            .filter(|c| *c != NO_READING)
            .max()
    }

    pub fn encode(&self) -> [u8; STATUS_LEN] {
        let mut out = [0u8; STATUS_LEN];
        out[..4].copy_from_slice(&STATUS_MAGIC);
        out[4] = STATUS_VERSION;
        out[5] = self.flags;
        out[6..8].copy_from_slice(&self.duty_permille.to_le_bytes());
        for (index, celsius) in self.celsius.iter().enumerate() {
            let at = 8 + 2 * index;
            out[at..at + 2].copy_from_slice(&celsius.to_le_bytes());
        }
        out[22..24].copy_from_slice(&self.rpm.to_le_bytes());
        out[24..28].copy_from_slice(&self.samples.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != STATUS_LEN || bytes[..4] != STATUS_MAGIC || bytes[4] != STATUS_VERSION {
            return None;
        }
        let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let mut celsius = [NO_READING; CHANNELS];
        for (index, slot) in celsius.iter_mut().enumerate() {
            *slot = u16_at(8 + 2 * index) as i16;
        }
        Some(Self {
            flags: bytes[5],
            duty_permille: u16_at(6),
            celsius,
            rpm: u16_at(22),
            samples: u32::from_le_bytes(bytes[24..28].try_into().ok()?),
        })
    }

    /// `package 47C a76-01 48C ... npu 46C max 48C`, the dmesg and shell line body.
    pub fn write_temperatures(&self, out: &mut impl Write) -> fmt::Result {
        for (name, celsius) in CHANNEL_NAMES.iter().zip(self.celsius) {
            if celsius == NO_READING {
                write!(out, "{name} ?? ")?;
            } else {
                write!(out, "{name} {celsius}C ")?;
            }
        }
        match self.max() {
            Some(max) => write!(out, "max {max}C"),
            None => write!(out, "max ??"),
        }
    }
}

/// A fixed buffer `core::fmt::Write` target that truncates instead of failing.
pub struct Line<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> Line<N> {
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl<const N: usize> Default for Line<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Write for Line<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let take = text.len().min(N - self.len);
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// When the driver logs: a NOTICE once the hottest channel has moved this far from the last
/// NOTICE, and an INFO line every [`REPORT_EVERY_SAMPLES`] samples (one sample per second).
pub const NOTICE_DELTA_C: i16 = 5;
pub const REPORT_EVERY_SAMPLES: u32 = 60;
pub const SAMPLE_PERIOD_NS: u64 = 1_000_000_000;

/// Whether a new hottest reading should be announced at NOTICE level.
pub fn notice_due(last_noticed: Option<i16>, max: Option<i16>) -> bool {
    match (last_noticed, max) {
        (Some(last), Some(now)) => (now - last).abs() >= NOTICE_DELTA_C,
        (None, Some(_)) | (Some(_), None) => true,
        (None, None) => false,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    #[test]
    fn trm_table_points_and_between() {
        assert_eq!(code_to_millicelsius(220), Some(-40_000));
        assert_eq!(code_to_millicelsius(285), Some(25_000));
        assert_eq!(code_to_millicelsius(345), Some(85_000));
        assert_eq!(code_to_millicelsius(385), Some(125_000));
        // Table 14-1 is 1 °C per code throughout.
        assert_eq!(code_to_millicelsius(300), Some(40_000));
        assert_eq!(code_to_millicelsius(330), Some(70_000));
        assert_eq!(code_to_millicelsius(0), None);
        assert_eq!(code_to_millicelsius(219), None);
        assert_eq!(code_to_millicelsius(386), None);
        assert_eq!(code_to_millicelsius(0x1000 | 300), Some(40_000));
    }

    #[test]
    fn reset_defaults_are_ready() {
        // TRM reset values: CLKSEL_CON41 [8]=1 [7:0]=0x0b, CLKSEL_CON60 [1:0]=2, all gates and
        // resets 0.
        let clocks = Clocks {
            clksel41: 0x0000_030b, // uart1 divider 1 in [13:9] does not matter
            clksel60: 0x2,
            ..Clocks::default()
        };
        assert_eq!(clocks.tsadc_hz(), Some(2_000_000));
        assert!(clocks.tsadc_ready());
        assert!(clocks.pwm3_live());
        assert_eq!(clocks.pwm3_hz(), Some(24_000_000));
        assert!(!clocks.tshut_reset_armed());
    }

    #[test]
    fn gated_or_reset_or_gpll_is_not_ready() {
        let base = Clocks {
            clksel41: 0x10b,
            clksel60: 2,
            ..Clocks::default()
        };
        assert!(!Clocks {
            gate12: 0b10,
            ..base
        }
        .tsadc_ready());
        assert!(!Clocks {
            softrst12: 0b01,
            ..base
        }
        .tsadc_ready());
        assert!(!Clocks {
            clksel41: 0x00b,
            ..base
        }
        .tsadc_ready());
        assert!(!Clocks {
            clksel41: 0x105,
            ..base
        }
        .tsadc_ready());
        assert!(!Clocks {
            gate15: 1 << 11,
            ..base
        }
        .pwm3_live());
        assert!(!Clocks {
            softrst15: 1 << 10,
            ..base
        }
        .pwm3_live());
        assert!(Clocks {
            glb_rst: 0b10,
            ..base
        }
        .tshut_reset_armed());
    }

    struct Fake {
        regs: Vec<u32>,
        writes: Vec<(u32, u32)>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                regs: std::vec![0; 0x100],
                writes: Vec::new(),
            }
        }
    }

    impl Regs for Fake {
        fn read(&mut self, offset: u32) -> u32 {
            self.regs[(offset / 4) as usize]
        }
        fn write(&mut self, offset: u32, value: u32) {
            self.writes.push((offset, value));
        }
    }

    #[test]
    fn start_programs_a_cold_controller_with_masked_writes() {
        let mut fake = Fake::default();
        let found = Found::read(&mut fake);
        assert!(!found.usable());
        assert!(start_auto(&mut fake, &found));
        assert_eq!(
            fake.writes,
            [
                (tsadc::AUTO_CON, 0x0003_0000),
                (tsadc::AUTO_SRC, 0x007f_007f),
                (tsadc::AUTO_CON, 0x0003_0001),
            ]
        );
        // tshut polarity (AUTO_CON[8]) is outside every write mask.
        assert!(fake.writes.iter().all(|(_, v)| v & (1 << 24) == 0));
    }

    #[test]
    fn start_leaves_a_running_controller_alone() {
        let mut fake = Fake::default();
        fake.regs[(tsadc::AUTO_CON / 4) as usize] = 0x101;
        fake.regs[(tsadc::AUTO_SRC / 4) as usize] = 0x7f;
        let found = Found::read(&mut fake);
        assert!(found.usable());
        assert!(!start_auto(&mut fake, &found));
        assert!(fake.writes.is_empty());
        // ...but not an inverted one.
        fake.regs[(tsadc::AUTO_CON / 4) as usize] = 0x3;
        let found = Found::read(&mut fake);
        assert!(!found.usable());
    }

    #[test]
    fn sample_reads_seven_channels() {
        let mut fake = Fake::default();
        for channel in 0..CHANNELS {
            fake.regs[(tsadc::data(channel) / 4) as usize] = 300 + channel as u32;
        }
        let sample = Sample::read(&mut fake);
        assert!(sample.complete());
        assert_eq!(sample.max(), Some(46_000));
        fake.regs[(tsadc::data(5) / 4) as usize] = 0;
        let sample = Sample::read(&mut fake);
        assert!(!sample.complete());
        assert_eq!(sample.millicelsius[5], None);
        assert_eq!(sample.max(), Some(46_000));
    }

    #[test]
    fn status_round_trips_and_formats() {
        let mut status = Status {
            flags: FLAG_SENSOR_OK,
            samples: 61,
            ..Status::default()
        };
        let mut sample = Sample::default();
        for (channel, slot) in sample.millicelsius.iter_mut().enumerate() {
            *slot = Some(40_000 + 1000 * channel as i32);
        }
        sample.millicelsius[6] = None;
        status.set_sample(&sample);
        let decoded = Status::decode(&status.encode()).unwrap();
        assert_eq!(decoded, status);
        let mut line = Line::<160>::new();
        decoded.write_temperatures(&mut line).unwrap();
        assert_eq!(
            core::str::from_utf8(line.as_bytes()).unwrap(),
            "package 40C a76-01 41C a76-23 42C a55 43C center 44C gpu 45C npu ?? max 45C"
        );
        assert!(Status::decode(&[0; STATUS_LEN]).is_none());
        assert!(Status::decode(&status.encode()[..31]).is_none());
    }

    #[test]
    fn line_truncates() {
        let mut line = Line::<4>::new();
        write!(line, "abcdef").unwrap();
        assert_eq!(line.as_bytes(), b"abcd");
    }

    #[test]
    fn notices_on_five_degree_moves() {
        assert!(notice_due(None, Some(40)));
        assert!(!notice_due(Some(40), Some(44)));
        assert!(notice_due(Some(40), Some(45)));
        assert!(notice_due(Some(40), Some(35)));
        assert!(notice_due(Some(40), None));
        assert!(!notice_due(None, None));
    }

    #[test]
    fn dtb_gate_matches_only_the_rk3588_board() {
        let opi5 = include_bytes!("../../../../rk3588-orangepi-5-plus.dtb");
        let x13s = include_bytes!("../../../../sc8280xp-lenovo-thinkpad-x13s.dtb");
        assert!(tsadc_in_dtb(opi5));
        assert!(!tsadc_in_dtb(x13s));
        assert!(!tsadc_in_dtb(&[]));
    }
}
