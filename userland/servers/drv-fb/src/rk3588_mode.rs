//j607
//j608

//! R1 (DESIGN/020 Amendment D0): KUMO rebuilds the HDMI0 mode's VOP2 and TX program from its
//! own mode table, after scrubbing it to TRM reset values with VP2 in standby.
//!
//! J607's first metal run took an asynchronous SError at the image's first barrier. The scrub
//! had already written every VOP word with no fault, so the only stores new to that barrier
//! were the two HDMI TX words. The TX words are now [`Program::checks`], verified but never
//! written, and every write is followed by its own barrier so any remaining fault's ELR names
//! its register.
//!
//! Recovery class: soft-state. Everything here goes through [`Hardware`], so the host tests
//! check every write and its order. The image is built from named TRM fields, not copied from
//! firmware: [`Program::build`] turns VESA DMT 1600x900@60 (reduced blanking) and the claimed
//! GOP framebuffer into register words. [`run`] first refuses unless every image word already
//! equals the live word (KUMO's table describes exactly what is on the glass). It then blanks
//! (standby, interface off), scrubs, waits so the blank is visible, writes the image, commits it
//! and leaves standby.
//!
//! Untouched on purpose: the overlay layer/port selection (shared by all four VPs), the VOP
//! MMUs, the PHY and clocks, and POST2 `+0x30..+0x40` (only `PRE_SCAN_HTIMING0` is documented;
//! R1 records those words but does not write them). Register sources: RK3588 TRM Part 2 ch. 7
//! (VOP2) and ch. 24 (HDMI TX). — TIMBERDOODLE

/// The writable VOP2 window: system, overlay, the four post blocks, cluster/esmart windows and
/// the LUTs, but not the MMU page at `+0x7000`.
pub const VOP_WINDOW_LEN: u64 = 0x7000;
/// The writable HDMI0 TX page.
pub const TX_WINDOW_LEN: u64 = 0x1000;

pub const REG_CFG_DONE: u32 = 0x0000;
pub const DSP_INFACE_EN: u32 = 0x0028;
pub const DSP_INFACE_CTRL: u32 = 0x002c;
pub const DSP_INFACE_POL: u32 = 0x0030;
/// POST2 (video port 2) block.
pub const VP2: u32 = 0x0e00;
pub const VP2_DSP_CTRL: u32 = VP2;
pub const VP2_CLK_CTRL: u32 = VP2 + 0x0c;
pub const VP2_HTOTAL_HS_END: u32 = VP2 + 0x48;
pub const VP2_HACT_ST_END: u32 = VP2 + 0x4c;
pub const VP2_VTOTAL_VS_END: u32 = VP2 + 0x50;
pub const VP2_VACT_ST_END: u32 = VP2 + 0x54;
/// ESMART2 region0.
pub const ESMART2: u32 = 0x1c00;
pub const ESMART2_R0_MST_CTL: u32 = ESMART2 + 0x10;
pub const ESMART2_R0_MST_YRGB: u32 = ESMART2 + 0x14;
pub const ESMART2_R0_VIR: u32 = ESMART2 + 0x1c;
pub const ESMART2_R0_ACT_INFO: u32 = ESMART2 + 0x20;
pub const ESMART2_R0_DSP_INFO: u32 = ESMART2 + 0x24;
pub const ESMART2_R0_DSP_OFFSET: u32 = ESMART2 + 0x28;
/// HDMI TX words.
pub const TX_HDCP2LOGIC_CONFIG0: u32 = 0x08e0;
pub const TX_LINK_CONFIG0: u32 = 0x0968;

/// `POST_DSP_CTRL` bit 31, `vop_standby_en_imd`: immediate; the port stops fetching at the end
/// of the current frame and its output is blank.
pub const STANDBY: u32 = 1 << 31;
/// `DSP_INFACE_POL` bit 28: the interface registers take effect immediately.
pub const REGDONE_IMD_EN: u32 = 1 << 28;
/// `REG_CFG_DONE` for port 2: mask bit 18, VP2 request bit 2, and global enable bit 15.
/// The global enable is included in each commit by both reference implementations; omitting
/// it left our requests pending on metal. This is register data, not copied driver code.
/// — KESTREL
pub const CFG_DONE_VP2: u32 = (1 << 18) | (1 << 15) | (1 << 2);

/// POST2 words R1 records but never writes. `+0x30` is `PRE_SCAN_HTIMING0`; `+0x34..+0x40`
/// have no field description in the TRM.
pub const RECORDED: [u32; 5] = [VP2 + 0x30, VP2 + 0x34, VP2 + 0x38, VP2 + 0x3c, VP2 + 0x40];

/// A progressive mode in porch terms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mode {
    pub h_active: u32,
    pub h_front: u32,
    pub h_sync: u32,
    pub h_back: u32,
    pub v_active: u32,
    pub v_front: u32,
    pub v_sync: u32,
    pub v_back: u32,
    pub pixel_hz: u64,
}

impl Mode {
    pub const fn h_total(&self) -> u32 {
        self.h_active + self.h_front + self.h_sync + self.h_back
    }

    pub const fn v_total(&self) -> u32 {
        self.v_active + self.v_front + self.v_sync + self.v_back
    }
}

/// KUMO's mode table has one entry: VESA DMT 1600x900@60, reduced blanking, 108 MHz.
pub const DMT_1600X900_60_RB: Mode = Mode {
    h_active: 1600,
    h_front: 24,
    h_sync: 80,
    h_back: 96,
    v_active: 900,
    v_front: 1,
    v_sync: 3,
    v_back: 96,
    pixel_hz: 108_000_000,
};

/// The scanout buffer: the GOP framebuffer drv-fb already claimed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Framebuffer {
    pub phys: u64,
    pub width: u32,
    pub height: u32,
    /// Pixels per line; ARGB8888, so also 32-bit words per line.
    pub stride: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Block {
    Vop,
    Tx,
}

/// One register in the image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Word {
    pub block: Block,
    pub offset: u32,
    pub value: u32,
}

const fn vop(offset: u32, value: u32) -> Word {
    Word {
        block: Block::Vop,
        offset,
        value,
    }
}

const fn tx(offset: u32, value: u32) -> Word {
    Word {
        block: Block::Tx,
        offset,
        value,
    }
}

const fn span(start: u32, end: u32) -> u32 {
    ((start & 0x1fff) << 16) | (end & 0x1fff)
}

pub const WORDS: usize = 14;
pub const CHECKS: usize = 2;
/// Mismatch masks put `checks[i]` at bit `CHECK_BIT + i`, above every `words` bit.
pub const CHECK_BIT: u32 = 16;

/// The register image. `words` are written, in order: the scrubbed VOP words, the interface
/// enable, and `DSP_CTRL` last (index [`DSP_CTRL_INDEX`]), which leaves standby. `checks` are
/// TX words that must already hold KUMO's values; R1 never writes them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Program {
    pub words: [Word; WORDS],
    pub checks: [Word; CHECKS],
}

pub const DSP_CTRL_INDEX: usize = WORDS - 1;
/// Indices of the image words that are scrubbed to their TRM reset value (0 for every one).
pub const SCRUBBED: [usize; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
/// Index of `DSP_INFACE_EN`, which the blank clears first and the rebuild sets last.
pub const IF_EN_INDEX: usize = 12;

impl Program {
    /// Build the image for one mode on HDMI0 from VP2 and ESMART2 region0. `None` when the
    /// framebuffer does not fit the mode or the 32-bit scanout address.
    pub fn build(mode: Mode, fb: Framebuffer) -> Option<Self> {
        if fb.width != mode.h_active
            || fb.height != mode.v_active
            || fb.stride < fb.width
            || fb.stride > 0xffff
            || fb.phys == 0
            || fb.phys > u64::from(u32::MAX)
            || fb.phys % 4 != 0
            || mode.h_total() > 0x1fff
            || mode.v_total() > 0x1fff
        {
            return None;
        }
        let h_start = mode.h_sync + mode.h_back;
        let v_start = mode.v_sync + mode.v_back;
        let size = span(mode.v_active - 1, mode.h_active - 1);
        // HDMI interface arithmetic (TRM ch. 7 `DSP_INFACE_CTRL`, `POST_CLK_CTRL`): the core
        // scans 4 pixels per clock (dclk / 4), the interface carries 2 per clock (dclk / 2), and
        // the post output runs at dclk. With dclk = 108 MHz that is the measured 27/54/108 MHz.
        const CORE_DIV4: u32 = 2;
        const IF_PIX_DIV2: u32 = 1 << 18;
        const IF_DCLK_DIV4: u32 = 2 << 16;
        // HDMI/eDP combo 0 fed by video output 2, in HDMI mode.
        const HDMIEDP0_FROM_VP2: u32 = 2 << 16;
        const HDMIEDP0_HDMI: u32 = 2 << 2;
        // ESMART region0: enabled, ARGB8888 (format 0), no swaps.
        const MST_EN_ARGB8888: u32 = 1;
        // POST_DSP_CTRL: RGB101010 output (mode 0xf), not in standby.
        const OUT_RGB101010: u32 = 0xf;
        // LINK_CONFIG0 bit 4: DVI (CRUX decision 4); HDCP2LOGIC_CONFIG0 bit 0: bypass HDCP2.
        const DVI: u32 = 1 << 4;
        const HDCP2_BYPASS: u32 = 1;
        Some(Self {
            words: [
                vop(VP2_CLK_CTRL, CORE_DIV4),
                vop(DSP_INFACE_CTRL, IF_PIX_DIV2 | IF_DCLK_DIV4),
                vop(VP2_HTOTAL_HS_END, span(mode.h_total(), mode.h_sync)),
                vop(VP2_HACT_ST_END, span(h_start, h_start + mode.h_active)),
                vop(VP2_VTOTAL_VS_END, span(mode.v_total(), mode.v_sync)),
                vop(VP2_VACT_ST_END, span(v_start, v_start + mode.v_active)),
                vop(ESMART2_R0_MST_CTL, MST_EN_ARGB8888),
                vop(ESMART2_R0_MST_YRGB, fb.phys as u32),
                vop(ESMART2_R0_VIR, fb.stride),
                vop(ESMART2_R0_ACT_INFO, size),
                vop(ESMART2_R0_DSP_INFO, size),
                vop(ESMART2_R0_DSP_OFFSET, 0),
                vop(DSP_INFACE_EN, HDMIEDP0_FROM_VP2 | HDMIEDP0_HDMI),
                vop(VP2_DSP_CTRL, OUT_RGB101010),
            ],
            checks: [
                tx(TX_LINK_CONFIG0, DVI),
                tx(TX_HDCP2LOGIC_CONFIG0, HDCP2_BYPASS),
            ],
        })
    }
}

/// Register access plus time. No other method exists, so the host tests see everything.
pub trait Hardware {
    fn read(&mut self, block: Block, offset: u32) -> u32;
    fn write(&mut self, block: Block, offset: u32, value: u32);
    /// Order every earlier device write before what follows.
    fn barrier(&mut self);
    fn sleep_ms(&mut self, ms: u64);
}

/// Timing choices for the visible sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Plan {
    /// After entering standby: lets the current frame end (a frame is ~16.7 ms).
    pub standby_settle_ms: u64,
    /// The deliberate blank CRUX watches for, between [`blank`] and [`rebuild`].
    pub dwell_ms: u64,
    /// Between [`rebuild`] and [`verify`]: the TX monitor re-measures a few frames.
    pub verify_after_ms: u64,
}

pub const PLAN: Plan = Plan {
    standby_settle_ms: 50,
    dwell_ms: 2000,
    verify_after_ms: 500,
};

/// What R1 did. Bit `i` of a mask is `Program::words[i]`; bit `CHECK_BIT + i` is
/// `Program::checks[i]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Nothing was written: the interface registers do not update immediately.
    NotImmediate { pol: u32 },
    /// Nothing was written: these image words differ from the live hardware.
    Refused { mismatched: u32 },
    /// Blanked, scrubbed and rebuilt. `scrub_readback` is what the scrubbed words read after the
    /// scrub; `mismatched` is non-zero only if the readback still differs after one retry.
    Rebuilt {
        retried: bool,
        mismatched: u32,
        scrub_readback: [u32; SCRUBBED.len()],
    },
}

fn mismatches(hw: &mut impl Hardware, program: &Program) -> u32 {
    let mut mask = 0;
    for (index, word) in program.words.iter().enumerate() {
        if hw.read(word.block, word.offset) != word.value {
            mask |= 1 << index;
        }
    }
    for (index, word) in program.checks.iter().enumerate() {
        if hw.read(word.block, word.offset) != word.value {
            mask |= 1 << (CHECK_BIT + index as u32);
        }
    }
    mask
}

/// One write and its own barrier: an asynchronous bus error surfaces at the very next
/// instruction, so the exception's ELR names the register that refused it.
fn put(hw: &mut impl Hardware, block: Block, offset: u32, value: u32) {
    hw.write(block, offset, value);
    hw.barrier();
}

/// The scrub's outcome, carried from [`blank`] to [`verify`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Blanked {
    pub scrub_readback: [u32; SCRUBBED.len()],
}

/// Refuse unless KUMO's image equals the live program; then blank (standby, interface off),
/// scrub every scrubbed word to its TRM reset value and commit. On `Err`, nothing was written.
pub fn blank(hw: &mut impl Hardware, program: &Program, plan: Plan) -> Result<Blanked, Outcome> {
    let pol = hw.read(Block::Vop, DSP_INFACE_POL);
    if pol & REGDONE_IMD_EN == 0 {
        return Err(Outcome::NotImmediate { pol });
    }
    let mismatched = mismatches(hw, program);
    if mismatched != 0 {
        return Err(Outcome::Refused { mismatched });
    }
    let ctrl = program.words[DSP_CTRL_INDEX];
    put(hw, ctrl.block, ctrl.offset, ctrl.value | STANDBY);
    hw.sleep_ms(plan.standby_settle_ms);
    let if_en = program.words[IF_EN_INDEX];
    put(hw, if_en.block, if_en.offset, 0);
    for &index in &SCRUBBED {
        let word = program.words[index];
        put(hw, word.block, word.offset, 0);
    }
    put(hw, Block::Vop, REG_CFG_DONE, CFG_DONE_VP2);
    let mut scrub_readback = [0; SCRUBBED.len()];
    for (slot, &index) in scrub_readback.iter_mut().zip(SCRUBBED.iter()) {
        let word = program.words[index];
        *slot = hw.read(word.block, word.offset);
    }
    Ok(Blanked { scrub_readback })
}

/// Write KUMO's image, commit it and leave standby (the last write).
pub fn rebuild(hw: &mut impl Hardware, program: &Program) {
    for word in &program.words[..DSP_CTRL_INDEX] {
        put(hw, word.block, word.offset, word.value);
    }
    put(hw, Block::Vop, REG_CFG_DONE, CFG_DONE_VP2);
    let ctrl = program.words[DSP_CTRL_INDEX];
    put(hw, ctrl.block, ctrl.offset, ctrl.value);
}

/// Read the image back; on a difference, rebuild once more and read again. VP2 is out of
/// standby on every path.
pub fn verify(hw: &mut impl Hardware, program: &Program, blanked: Blanked) -> Outcome {
    let mut mismatched = mismatches(hw, program);
    let retried = mismatched != 0;
    if retried {
        rebuild(hw, program);
        mismatched = mismatches(hw, program);
    }
    Outcome::Rebuilt {
        retried,
        mismatched,
        scrub_readback: blanked.scrub_readback,
    }
}

/// The whole sequence in one call, sleeping where the live adapter arms timers.
pub fn run(hw: &mut impl Hardware, program: &Program, plan: Plan) -> Outcome {
    let blanked = match blank(hw, program, plan) {
        Ok(blanked) => blanked,
        Err(outcome) => return outcome,
    };
    hw.sleep_ms(plan.dwell_ms);
    rebuild(hw, program);
    hw.sleep_ms(plan.verify_after_ms);
    verify(hw, program, blanked)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Op {
        Read(Block, u32),
        Write(Block, u32, u32),
        Barrier,
        Sleep(u64),
    }

    /// Plain memory per block, optionally refusing to latch some writes.
    struct Fake {
        regs: BTreeMap<(u8, u32), u32>,
        ops: Vec<Op>,
        /// Offsets that ignore their first N writes.
        ignore_first: BTreeMap<u32, usize>,
        /// Offsets that latch N writes and then ignore every later one.
        freeze_after: BTreeMap<u32, usize>,
    }

    fn key(block: Block, offset: u32) -> (u8, u32) {
        (
            match block {
                Block::Vop => 0,
                Block::Tx => 1,
            },
            offset,
        )
    }

    impl Fake {
        /// The live words D0a, D0d and R0 measured on the Orange Pi.
        fn metal() -> Self {
            let mut regs = BTreeMap::new();
            let vop_words = [
                (DSP_INFACE_EN, 0x0002_0008),
                (DSP_INFACE_CTRL, 0x0006_0000),
                (DSP_INFACE_POL, 0x1000_0000),
                (VP2_DSP_CTRL, 0x0000_000f),
                (VP2_CLK_CTRL, 0x0000_0002),
                (VP2_HTOTAL_HS_END, 0x0708_0050),
                (VP2_HACT_ST_END, 0x00b0_06f0),
                (VP2_VTOTAL_VS_END, 0x03e8_0003),
                (VP2_VACT_ST_END, 0x0063_03e7),
                (ESMART2_R0_MST_CTL, 0x0000_0001),
                (ESMART2_R0_MST_YRGB, 0xed0c_0000),
                (ESMART2_R0_VIR, 0x0000_0640),
                (ESMART2_R0_ACT_INFO, 0x0383_063f),
                (ESMART2_R0_DSP_INFO, 0x0383_063f),
                (ESMART2_R0_DSP_OFFSET, 0),
            ];
            for (offset, value) in vop_words {
                regs.insert(key(Block::Vop, offset), value);
            }
            regs.insert(key(Block::Tx, TX_LINK_CONFIG0), 0x10);
            regs.insert(key(Block::Tx, TX_HDCP2LOGIC_CONFIG0), 1);
            Self {
                regs,
                ops: Vec::new(),
                ignore_first: BTreeMap::new(),
                freeze_after: BTreeMap::new(),
            }
        }

        fn writes(&self) -> Vec<(Block, u32, u32)> {
            self.ops
                .iter()
                .filter_map(|op| match *op {
                    Op::Write(block, offset, value) => Some((block, offset, value)),
                    _ => None,
                })
                .collect()
        }

        fn position(&self, wanted: Op) -> usize {
            self.ops.iter().position(|&op| op == wanted).unwrap()
        }
    }

    impl Hardware for Fake {
        fn read(&mut self, block: Block, offset: u32) -> u32 {
            self.ops.push(Op::Read(block, offset));
            *self.regs.get(&key(block, offset)).unwrap_or(&0)
        }

        fn write(&mut self, block: Block, offset: u32, value: u32) {
            self.ops.push(Op::Write(block, offset, value));
            if let Some(left) = self.ignore_first.get_mut(&offset) {
                if *left > 0 {
                    *left -= 1;
                    return;
                }
            }
            if let Some(left) = self.freeze_after.get_mut(&offset) {
                if *left == 0 {
                    return;
                }
                *left -= 1;
            }
            if !(block == Block::Vop && offset == REG_CFG_DONE) {
                self.regs.insert(key(block, offset), value);
            }
        }

        fn barrier(&mut self) {
            self.ops.push(Op::Barrier);
        }

        fn sleep_ms(&mut self, ms: u64) {
            self.ops.push(Op::Sleep(ms));
        }
    }

    fn gop() -> Framebuffer {
        Framebuffer {
            phys: 0xed0c_0000,
            width: 1600,
            height: 900,
            stride: 1600,
        }
    }

    #[test]
    fn kumos_table_builds_exactly_the_program_measured_on_the_glass() {
        let program = Program::build(DMT_1600X900_60_RB, gop()).unwrap();
        let mut fake = Fake::metal();
        assert_eq!(mismatches(&mut fake, &program), 0);
        assert_eq!(DMT_1600X900_60_RB.h_total(), 1800);
        assert_eq!(DMT_1600X900_60_RB.v_total(), 1000);
        // 108 MHz over 1800 x 1000 is exactly 60 Hz.
        assert_eq!(
            DMT_1600X900_60_RB.pixel_hz,
            u64::from(DMT_1600X900_60_RB.h_total()) * u64::from(DMT_1600X900_60_RB.v_total()) * 60
        );
        // The DMT porches equal the TX monitor's measurement (D0d).
        let d0d = crate::rk3588_hdmi::D0A_TIMING;
        assert_eq!(
            (
                d0d.h_active,
                d0d.h_front,
                d0d.h_sync,
                d0d.h_back,
                d0d.v_active,
                d0d.v_front,
                d0d.v_sync,
                d0d.v_back
            ),
            (1600, 24, 80, 96, 900, 1, 3, 96)
        );
    }

    #[test]
    fn every_write_stays_in_its_window_and_off_the_untouchable_registers() {
        let program = Program::build(DMT_1600X900_60_RB, gop()).unwrap();
        let mut fake = Fake::metal();
        let outcome = run(&mut fake, &program, PLAN);
        assert!(matches!(outcome, Outcome::Rebuilt { mismatched: 0, .. }));
        for (block, offset, _) in fake.writes() {
            assert_eq!(offset % 4, 0);
            match block {
                Block::Vop => {
                    assert!(u64::from(offset) + 4 <= VOP_WINDOW_LEN);
                    // Overlay (0x600..0x700), other ports, other windows, recorded words.
                    assert!(!(0x0600..0x0700).contains(&offset));
                    assert!(
                        offset < 0x0c00 || (VP2..VP2 + 0x100).contains(&offset) || offset >= 0x1000
                    );
                    assert!(!RECORDED.contains(&offset));
                    assert!(offset < 0x1000 || (ESMART2..ESMART2 + 0x30).contains(&offset));
                    assert_ne!(offset, DSP_INFACE_POL);
                }
                Block::Tx => panic!("R1 wrote the HDMI TX at {offset:#x}"),
            }
        }
        // Every write is followed immediately by its own barrier.
        for (index, op) in fake.ops.iter().enumerate() {
            if matches!(op, Op::Write(..)) {
                assert_eq!(fake.ops.get(index + 1), Some(&Op::Barrier));
            }
        }
        for check in program.checks {
            assert_eq!(check.block, Block::Tx);
            assert!(u64::from(check.offset) + 4 <= TX_WINDOW_LEN);
        }
    }

    #[test]
    fn the_sequence_blanks_scrubs_dwells_rebuilds_and_leaves_standby_last() {
        let program = Program::build(DMT_1600X900_60_RB, gop()).unwrap();
        let mut fake = Fake::metal();
        let outcome = run(&mut fake, &program, PLAN);
        let Outcome::Rebuilt {
            retried,
            mismatched,
            scrub_readback,
        } = outcome
        else {
            panic!("expected a rebuild, got {outcome:?}");
        };
        assert!(!retried);
        assert_eq!(mismatched, 0);
        assert_eq!(scrub_readback, [0; SCRUBBED.len()]);

        let standby = fake.position(Op::Write(Block::Vop, VP2_DSP_CTRL, 0x8000_000f));
        let if_off = fake.position(Op::Write(Block::Vop, DSP_INFACE_EN, 0));
        let first_scrub = fake.position(Op::Write(Block::Vop, VP2_CLK_CTRL, 0));
        let dwell = fake.position(Op::Sleep(PLAN.dwell_ms));
        let verify_wait = fake.position(Op::Sleep(PLAN.verify_after_ms));
        let rebuild = fake.position(Op::Write(Block::Vop, VP2_CLK_CTRL, 2));
        let if_on = fake.position(Op::Write(Block::Vop, DSP_INFACE_EN, 0x0002_0008));
        let resume = fake.position(Op::Write(Block::Vop, VP2_DSP_CTRL, 0x0000_000f));
        assert!(standby < if_off && if_off < first_scrub && first_scrub < dwell);
        assert!(dwell < rebuild && rebuild < if_on && if_on < resume && resume < verify_wait);
        // Two commits: one after the scrub, one after the image, both before resuming.
        let commits: Vec<usize> = fake
            .ops
            .iter()
            .enumerate()
            .filter(|(_, &op)| op == Op::Write(Block::Vop, REG_CFG_DONE, CFG_DONE_VP2))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(commits.len(), 2);
        assert!(first_scrub < commits[0] && commits[0] < dwell);
        assert!(if_on < commits[1] && commits[1] < resume);
        // Standby is entered once and the last write leaves it.
        let writes = fake.writes();
        assert_eq!(
            *writes.last().unwrap(),
            (Block::Vop, VP2_DSP_CTRL, 0x0000_000f)
        );
        assert_eq!(
            writes
                .iter()
                .filter(|&&(_, offset, value)| offset == VP2_DSP_CTRL && value & STANDBY != 0)
                .count(),
            1
        );
        // Every scrub writes the TRM reset value, 0.
        let scrubs = &writes[2..2 + SCRUBBED.len()];
        assert!(scrubs.iter().all(|&(_, _, value)| value == 0));
        // The final state is exactly the image.
        let mut check = Fake {
            regs: fake.regs.clone(),
            ops: Vec::new(),
            ignore_first: BTreeMap::new(),
            freeze_after: BTreeMap::new(),
        };
        assert_eq!(mismatches(&mut check, &program), 0);
    }

    #[test]
    fn any_difference_from_the_live_program_means_no_write_at_all() {
        let program = Program::build(DMT_1600X900_60_RB, gop()).unwrap();
        for (offset, value) in [
            (VP2_HACT_ST_END, 0x00b0_06f1),
            (ESMART2_R0_MST_YRGB, 0xed00_0000),
            (DSP_INFACE_EN, 0x0001_0008),
        ] {
            let mut fake = Fake::metal();
            fake.regs.insert(key(Block::Vop, offset), value);
            let outcome = run(&mut fake, &program, PLAN);
            assert!(matches!(outcome, Outcome::Refused { mismatched } if mismatched != 0));
            assert!(fake.writes().is_empty());
        }
        let mut fake = Fake::metal();
        fake.regs.insert(key(Block::Tx, TX_LINK_CONFIG0), 0);
        assert!(matches!(
            run(&mut fake, &program, PLAN),
            Outcome::Refused { mismatched } if mismatched == 1 << CHECK_BIT
        ));
        assert!(fake.writes().is_empty());

        let mut fake = Fake::metal();
        fake.regs.insert(key(Block::Vop, DSP_INFACE_POL), 0);
        assert_eq!(
            run(&mut fake, &program, PLAN),
            Outcome::NotImmediate { pol: 0 }
        );
        assert!(fake.writes().is_empty());
    }

    #[test]
    fn a_word_that_does_not_latch_is_retried_once_and_reported() {
        let program = Program::build(DMT_1600X900_60_RB, gop()).unwrap();
        // Ignores the scrub only: the readback records it, the image then latches.
        let mut fake = Fake::metal();
        fake.ignore_first.insert(VP2_VTOTAL_VS_END, 1);
        let outcome = run(&mut fake, &program, PLAN);
        let Outcome::Rebuilt {
            retried,
            mismatched,
            scrub_readback,
        } = outcome
        else {
            panic!("expected a rebuild, got {outcome:?}");
        };
        assert!(!retried);
        assert_eq!(mismatched, 0);
        assert_eq!(scrub_readback[4], 0x03e8_0003);

        // Latches the scrub's zero, then ignores the image and its retry: reported, and the
        // port still leaves standby.
        let mut fake = Fake::metal();
        fake.freeze_after.insert(VP2_HTOTAL_HS_END, 1);
        let outcome = run(&mut fake, &program, PLAN);
        let Outcome::Rebuilt {
            retried,
            mismatched,
            ..
        } = outcome
        else {
            panic!("expected a rebuild report, got {outcome:?}");
        };
        assert!(retried);
        assert_eq!(mismatched, 1 << 2);
        assert_eq!(
            *fake.writes().last().unwrap(),
            (Block::Vop, VP2_DSP_CTRL, 0x0000_000f)
        );
    }

    #[test]
    fn a_framebuffer_that_does_not_fit_builds_no_program() {
        let base = gop();
        for fb in [
            Framebuffer {
                width: 1920,
                ..base
            },
            Framebuffer {
                height: 1080,
                ..base
            },
            Framebuffer {
                stride: 1599,
                ..base
            },
            Framebuffer {
                phys: 0x1_0000_0000,
                ..base
            },
            Framebuffer { phys: 0, ..base },
            Framebuffer {
                phys: 0xed0c_0002,
                ..base
            },
        ] {
            assert_eq!(Program::build(DMT_1600X900_60_RB, fb), None);
        }
        // A padded stride is fine: VIR carries it.
        let padded = Program::build(
            DMT_1600X900_60_RB,
            Framebuffer {
                stride: 1664,
                ..base
            },
        )
        .unwrap();
        assert_eq!(padded.words[8], vop(ESMART2_R0_VIR, 1664));
    }
}
