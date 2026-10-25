//j608
//j614

//! R1b: prove the VOP's active mirrored program changed before restoring visible pixels.
//!
//! R1's scrub did not prove a load, and its commits omitted global enable. Correct the commit,
//! keep the interface clocked with POST_DSP_CTRL.dsp_black_en
//! (bit 27), run distinct valid porches at the same 108 MHz / 60 Hz, and disable region0.
//! Observe that active program, restore and observe the target under black, then remove black.
//! TX reads wait until that final step: metal faults a disconnected TX read at +0x8e0.
//! Recovery is soft-state: a failed proof still restores; failed restoration disconnects and
//! enters standby. No TX, clock, reset, MMU, overlay or shared-control write. Register sources:
//! TRM Part 2 ch. 7 REG_CFG_DONE and POST_DSP_CTRL. — KESTREL

use crate::rk3588_mode::*;

/// Distinct sync widths and active starts, same active size, totals, pixel clock and refresh.
pub const PROOF_MODE: Mode = Mode {
    h_front: 20,
    h_sync: 84,
    v_front: 2,
    v_sync: 4,
    v_back: 94,
    ..DMT_1600X900_60_RB
};

/// More than two frames of either program, even allowing the measured /4 core clock.
pub const SETTLE_MS: u64 = 150;

/// POST_DSP_CTRL bit 27: black pixels while sync and the interface clock keep running.
pub const BLACK: u32 = 1 << 27;

/// A sampled program plus the VP2 load request. A matching register image alone is insufficient
/// while REG_CFG_DONE still reports a pending load.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Observation {
    pub mismatched: u32,
    pub load_pending: bool,
}

impl Observation {
    pub fn matches(self) -> bool {
        self.mismatched == 0 && !self.load_pending
    }
}

fn put(hw: &mut impl Hardware, offset: u32, value: u32) {
    hw.write(Block::Vop, offset, value);
    hw.barrier();
}

fn span(start: u32, end: u32) -> u32 {
    (start << 16) | end
}

/// Target under black, with VP2 and HDMI still running so its mirrors can latch.
pub fn blacked(target: &Program) -> Program {
    let mut image = *target;
    image.words[DSP_CTRL_INDEX].value |= BLACK;
    image
}

/// A valid, distinguishable internal program with no region0 framebuffer fetches. The old
/// target remains valid until the next frame loads the complete proof image. — KESTREL
pub fn proof(target: &Program) -> Program {
    let mut image = blacked(target);
    let mode = PROOF_MODE;
    let h_start = mode.h_sync + mode.h_back;
    let v_start = mode.v_sync + mode.v_back;
    for word in &mut image.words {
        word.value = match word.offset {
            VP2_HTOTAL_HS_END => span(mode.h_total(), mode.h_sync),
            VP2_HACT_ST_END => span(h_start, h_start + mode.h_active),
            VP2_VTOTAL_VS_END => span(mode.v_total(), mode.v_sync),
            VP2_VACT_ST_END => span(v_start, v_start + mode.v_active),
            ESMART2_R0_MST_CTL
            | ESMART2_R0_MST_YRGB
            | ESMART2_R0_VIR
            | ESMART2_R0_ACT_INFO
            | ESMART2_R0_DSP_INFO
            | ESMART2_R0_DSP_OFFSET => 0,
            _ => word.value,
        };
    }
    image
}

/// Stage an image under black and request its load with VP2 and HDMI running. The live
/// adapter waits on its port timer before observing it, keeping console IPC draining.
pub fn stage(hw: &mut impl Hardware, image: &Program) {
    let standby = hw.read(Block::Vop, VP2_DSP_CTRL) & STANDBY != 0;
    for word in &image.words[..DSP_CTRL_INDEX] {
        put(hw, word.offset, word.value);
    }
    let ctrl = image.words[DSP_CTRL_INDEX].value;
    // A checked call may span a frame: stage black before requesting a running port's load.
    // In standby, keep R1's commit-before-resume order so the first frame loads the image.
    // Both orders remain driver policy; the kernel writes one word. — KESTREL
    if standby {
        put(hw, REG_CFG_DONE, CFG_DONE_VP2);
        put(hw, VP2_DSP_CTRL, ctrl);
    } else {
        put(hw, VP2_DSP_CTRL, ctrl);
        put(hw, REG_CFG_DONE, CFG_DONE_VP2);
    }
}

pub fn observe(hw: &mut impl Hardware, image: &Program) -> Observation {
    let mut mismatched = 0;
    for (i, word) in image.words.iter().enumerate() {
        if hw.read(word.block, word.offset) != word.value {
            mismatched |= 1 << i;
        }
    }
    // The disconnected TX is not readable on metal (external abort at +0x8e0). Only sample
    // it once the complete active VOP program, including its interface enable, matches.
    // — KESTREL
    if mismatched == 0
        && image.words[IF_EN_INDEX].value != 0
        && image.words[DSP_CTRL_INDEX].value & BLACK == 0
    {
        for (i, word) in image.checks.iter().enumerate() {
            if hw.read(word.block, word.offset) != word.value {
                mismatched |= 1 << (CHECK_BIT + i as u32);
            }
        }
    }
    Observation {
        mismatched,
        load_pending: hw.read(Block::Vop, REG_CFG_DONE) & (1 << 2) != 0,
    }
}

/// Check the restored active program afresh before removing black. A failed restoration
/// disconnects HDMI and enters standby. — KESTREL
pub fn unblack(hw: &mut impl Hardware, target: &Program) -> Observation {
    let observation = observe(hw, &blacked(target));
    if observation.matches() {
        put(hw, VP2_DSP_CTRL, target.words[DSP_CTRL_INDEX].value);
        put(hw, REG_CFG_DONE, CFG_DONE_VP2);
    } else {
        stop(hw, target);
    }
    observation
}

pub fn stop(hw: &mut impl Hardware, target: &Program) {
    put(hw, DSP_INFACE_EN, 0);
    put(
        hw,
        VP2_DSP_CTRL,
        target.words[DSP_CTRL_INDEX].value | STANDBY,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    /// Reads expose active registers; writes to timing/region registers only alter mirrors.
    /// A commit stays pending in standby. This reproduces the J607 metal observation.
    struct Vop {
        active: BTreeMap<u32, u32>,
        mirror: BTreeMap<u32, u32>,
        tx: BTreeMap<u32, u32>,
        writes: Vec<(u32, u32)>,
        barriers: usize,
    }

    fn mirrored(offset: u32) -> bool {
        matches!(
            offset,
            VP2_DSP_CTRL
                | VP2_HTOTAL_HS_END
                | VP2_HACT_ST_END
                | VP2_VTOTAL_VS_END
                | VP2_VACT_ST_END
        ) || (ESMART2_R0_MST_CTL..=ESMART2_R0_DSP_OFFSET).contains(&offset)
    }

    fn target() -> Program {
        Program::build(
            DMT_1600X900_60_RB,
            Framebuffer {
                phys: 0xed0c_0000,
                width: 1600,
                height: 900,
                stride: 1600,
            },
        )
        .unwrap()
    }

    impl Vop {
        fn new(target: &Program) -> Self {
            let mut active: BTreeMap<_, _> =
                target.words.iter().map(|w| (w.offset, w.value)).collect();
            active.insert(DSP_INFACE_POL, REGDONE_IMD_EN);
            active.insert(REG_CFG_DONE, 0x8000);
            Self {
                mirror: active.clone(),
                active,
                tx: target.checks.iter().map(|w| (w.offset, w.value)).collect(),
                writes: Vec::new(),
                barriers: 0,
            }
        }

        fn frame(&mut self) {
            if self.active[&VP2_DSP_CTRL] & STANDBY == 0
                && self.active[&REG_CFG_DONE] & 4 != 0
                && self.active[&REG_CFG_DONE] & (1 << 15) != 0
            {
                for (&offset, &value) in &self.mirror {
                    if mirrored(offset) {
                        self.active.insert(offset, value);
                    }
                }
                *self.active.get_mut(&REG_CFG_DONE).unwrap() &= !4;
            }
        }
    }

    impl Hardware for Vop {
        fn read(&mut self, block: Block, offset: u32) -> u32 {
            if block == Block::Tx {
                assert_ne!(
                    self.active[&DSP_INFACE_EN], 0,
                    "disconnected TX read faults on metal"
                );
                assert_eq!(
                    self.active[&VP2_DSP_CTRL] & BLACK,
                    0,
                    "defer TX reads until black is removed"
                );
            }
            match block {
                Block::Vop => self.active.get(&offset),
                Block::Tx => self.tx.get(&offset),
            }
            .copied()
            .unwrap_or(0)
        }
        fn write(&mut self, block: Block, offset: u32, value: u32) {
            assert_eq!(block, Block::Vop, "R1b must never write TX");
            self.writes.push((offset, value));
            if offset == REG_CFG_DONE {
                let old = self.active[&offset];
                self.active.insert(
                    offset,
                    ((old & !(value >> 16)) | (value & (value >> 16))) & !(1 << 15)
                        | (value & (1 << 15)),
                );
            } else if offset == VP2_DSP_CTRL {
                self.mirror.insert(offset, value);
                let old = self.active[&offset];
                self.active
                    .insert(offset, (old & !STANDBY) | (value & STANDBY));
            } else if mirrored(offset) {
                self.mirror.insert(offset, value);
            } else {
                self.active.insert(offset, value);
            }
        }
        fn barrier(&mut self) {
            self.barriers += 1;
        }
        fn sleep_ms(&mut self, _: u64) {
            self.frame();
        }
    }

    #[test]
    fn a_frame_between_each_checked_write_cannot_consume_an_incomplete_program() {
        struct Framing(Vop);
        impl Hardware for Framing {
            fn read(&mut self, block: Block, offset: u32) -> u32 {
                self.0.read(block, offset)
            }
            fn write(&mut self, block: Block, offset: u32, value: u32) {
                self.0.write(block, offset, value);
            }
            fn barrier(&mut self) {
                self.0.barrier();
                self.0.frame();
            }
            fn sleep_ms(&mut self, ms: u64) {
                self.0.sleep_ms(ms);
            }
        }
        let target = target();
        for standby in [false, true] {
            let mut device = Framing(Vop::new(&target));
            if standby {
                device.write(
                    Block::Vop,
                    VP2_DSP_CTRL,
                    target.words[DSP_CTRL_INDEX].value | STANDBY,
                );
            }
            let image = blacked(&target);
            stage(&mut device, &image);
            assert!(observe(&mut device, &image).matches(), "standby={standby}");
        }
    }

    #[test]
    fn standby_scrub_does_not_prove_a_latch_but_running_proof_does() {
        let target = target();
        let mut vop = Vop::new(&target);
        let blanked = blank(&mut vop, &target, PLAN).unwrap();
        assert_eq!(blanked.scrub_readback[2], target.words[2].value);
        let proof = proof(&target);
        stage(&mut vop, &proof);
        let before = observe(&mut vop, &proof);
        assert!(!before.matches());
        assert!(before.load_pending);
        vop.frame();
        assert!(observe(&mut vop, &proof).matches());
        // All four mirrored timing words changed, and all six region0 words are zero.
        for offset in [
            VP2_HTOTAL_HS_END,
            VP2_HACT_ST_END,
            VP2_VTOTAL_VS_END,
            VP2_VACT_ST_END,
        ] {
            assert_ne!(
                vop.active[&offset],
                target
                    .words
                    .iter()
                    .find(|w| w.offset == offset)
                    .unwrap()
                    .value
            );
        }
        assert_eq!(vop.active[&DSP_INFACE_EN], target.words[IF_EN_INDEX].value);
        assert_ne!(vop.active[&VP2_DSP_CTRL] & BLACK, 0);
        assert_eq!(vop.active[&ESMART2_R0_MST_CTL], 0);
        assert_eq!(vop.active[&ESMART2_R0_MST_YRGB], 0);
        assert_eq!(vop.active[&VP2_DSP_CTRL] & STANDBY, 0);

        stage(&mut vop, &blacked(&target));
        assert!(!observe(&mut vop, &blacked(&target)).matches());
        assert_ne!(vop.active[&VP2_DSP_CTRL] & BLACK, 0);
        vop.frame();
        assert!(unblack(&mut vop, &target).matches());
        vop.frame();
        assert!(observe(&mut vop, &target).matches());
        assert_eq!(vop.barriers, vop.writes.len());
    }

    #[test]
    fn an_unlatched_restore_cannot_remove_black() {
        let target = target();
        let mut vop = Vop::new(&target);
        stage(&mut vop, &proof(&target));
        vop.frame();
        stage(&mut vop, &blacked(&target));
        assert!(!unblack(&mut vop, &target).matches());
        assert_eq!(vop.active[&DSP_INFACE_EN], 0);
        assert_ne!(vop.active[&VP2_DSP_CTRL] & STANDBY, 0);
        assert_ne!(vop.active[&VP2_DSP_CTRL] & BLACK, 0);
    }

    #[test]
    fn even_equal_active_words_cannot_hide_a_pending_commit() {
        let target = target();
        let mut vop = Vop::new(&target);
        stage(&mut vop, &blacked(&target));
        vop.frame();
        stage(&mut vop, &blacked(&target));
        let observed = unblack(&mut vop, &target);
        assert_eq!(observed.mismatched, 0);
        assert!(observed.load_pending);
        assert_eq!(vop.active[&DSP_INFACE_EN], 0);
    }

    #[test]
    fn every_corrupt_restoration_word_blocks_removing_black() {
        let target = target();
        for i in 0..WORDS {
            let mut vop = Vop::new(&target);
            stage(&mut vop, &blacked(&target));
            vop.frame();
            *vop.active.get_mut(&target.words[i].offset).unwrap() ^= 1;
            let observed = unblack(&mut vop, &target);
            assert_eq!(observed.mismatched, 1 << i);
            assert_eq!(vop.active[&DSP_INFACE_EN], 0);
            assert_ne!(vop.active[&VP2_DSP_CTRL] & STANDBY, 0);
        }
    }

    #[test]
    fn tx_checks_are_sampled_only_after_a_verified_reconnection() {
        let target = target();
        for i in 0..CHECKS {
            let mut vop = Vop::new(&target);
            stage(&mut vop, &proof(&target));
            vop.frame();
            assert!(observe(&mut vop, &proof(&target)).matches());
            *vop.tx.get_mut(&target.checks[i].offset).unwrap() ^= 1;
            stage(&mut vop, &blacked(&target));
            vop.frame();
            assert!(unblack(&mut vop, &target).matches());
            vop.frame();
            let observed = observe(&mut vop, &target);
            assert_eq!(observed.mismatched, 1 << (CHECK_BIT + i as u32));
        }
    }

    #[test]
    fn every_commit_carries_global_enable_as_well_as_the_vp2_write_mask() {
        assert_eq!(CFG_DONE_VP2, 0x0004_8004);
        let target = target();
        let mut vop = Vop::new(&target);
        stage(&mut vop, &proof(&target));
        vop.write(Block::Vop, REG_CFG_DONE, (1 << 18) | (1 << 2));
        vop.frame();
        assert!(observe(&mut vop, &proof(&target)).load_pending);
        vop.write(Block::Vop, REG_CFG_DONE, CFG_DONE_VP2);
        vop.frame();
        assert!(observe(&mut vop, &proof(&target)).matches());
    }

    #[test]
    fn proof_has_valid_porches_and_writes_only_the_existing_vop_program() {
        let target = target();
        let proof = proof(&target);
        let mode = PROOF_MODE;
        assert_eq!((mode.h_total(), mode.v_total()), (1800, 1000));
        assert!(mode.h_sync + mode.h_back + mode.h_active < mode.h_total());
        assert!(mode.v_sync + mode.v_back + mode.v_active < mode.v_total());
        assert_eq!(mode.pixel_hz, DMT_1600X900_60_RB.pixel_hz);
        let mut vop = Vop::new(&target);
        stage(&mut vop, &proof);
        stage(&mut vop, &blacked(&target));
        stop(&mut vop, &target);
        for &(offset, _) in &vop.writes {
            assert!(offset == REG_CFG_DONE || target.words.iter().any(|w| w.offset == offset));
            assert!(!RECORDED.contains(&offset));
        }
        assert_eq!(vop.barriers, vop.writes.len());
    }
}
