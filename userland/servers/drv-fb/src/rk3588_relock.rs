//j609

//! R2 PHY0 reconstruction (DESIGN/020 D0). Userspace owns every step and the measured PHY
//! image; the kernel changes one reset line per authorized call. No shared register mapping
//! is writable. Recovery is soft-state: retry once, then retain standby and name the step.
//! Register/ordering sources: TRM Part 1 ch. 2/6 and Part 2 ch. 27, RESEARCH/014. — KESTREL

use crate::rk3588_phy_image::IMAGE;

pub const GRF_CON: u32 = 0;
pub const GRF_STATUS: u32 = 0x80;
pub const PLL: u32 = 1 << 7;
pub const BIAS_BGR: u32 = (1 << 6) | (1 << 5);
pub const CLOCK_READY: u32 = 1 << 2;
pub const READY: u32 = (1 << 3) | (1 << 2) | (1 << 1);

pub trait Hardware {
    fn phy_read(&mut self, offset: u32) -> u32;
    fn phy_write(&mut self, offset: u32, value: u32);
    fn grf_read(&mut self, offset: u32) -> u32;
    fn grf_write(&mut self, offset: u32, value: u32);
    fn reset(&mut self, line: u32, asserted: bool) -> bool;
    fn now_us(&mut self) -> u64;
    fn delay_us(&mut self, us: u64);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {
    Reset,
    Unlock,
    Clock,
    Lanes,
    Readback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Failure {
    pub step: Step,
    pub status: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Locked {
    pub clock_us: u64,
    pub lanes_us: u64,
    pub retried: bool,
}

fn reset(hw: &mut impl Hardware, line: u32, asserted: bool) -> Result<(), Failure> {
    if hw.reset(line, asserted) {
        Ok(())
    } else {
        Err(Failure {
            step: Step::Reset,
            status: line,
        })
    }
}

fn grf(hw: &mut impl Hardware, mask: u32, value: u32) {
    hw.grf_write(GRF_CON, (mask << 16) | (value & mask));
}

fn wait(
    hw: &mut impl Hardware,
    mask: u32,
    value: u32,
    us: u64,
    step: Step,
) -> Result<u64, Failure> {
    let start = hw.now_us();
    let mut status = 0;
    // Bounded even if the timer fails to advance. — KESTREL
    for _ in 0..20_000 {
        status = hw.grf_read(GRF_STATUS);
        let elapsed = hw.now_us().saturating_sub(start);
        if status & mask == value {
            return Ok(elapsed);
        }
        if elapsed >= us {
            break;
        }
        hw.delay_us(5);
    }
    Err(Failure { step, status })
}

/// Count differences in documented writable fields. Never reads TX or an unlisted PHY word.
pub fn mismatches(hw: &mut impl Hardware) -> u32 {
    IMAGE
        .iter()
        .filter(|w| hw.phy_read(w.offset) & w.mask != w.value & w.mask)
        .count() as u32
}

fn replay(hw: &mut impl Hardware, lanes: bool) {
    for word in &IMAGE {
        if (word.offset >= 0x800) != lanes {
            continue;
        }
        let old = hw.phy_read(word.offset);
        hw.phy_write(word.offset, (old & !word.mask) | (word.value & word.mask));
    }
}

fn attempt(hw: &mut impl Hardware) -> Result<(u64, u64), Failure> {
    reset(hw, 0, true)?; // apb
    hw.delay_us(20);
    reset(hw, 0, false)?;
    for line in [3, 2, 1] {
        reset(hw, line, true)?;
    }
    grf(hw, PLL | BIAS_BGR, 0);
    // Loss of clock/lock is evidence that this is a re-lock, not retained firmware lock.
    // — KESTREL
    wait(hw, READY, 0, 1000, Step::Unlock)?;
    replay(hw, false);
    grf(hw, BIAS_BGR, BIAS_BGR);
    hw.delay_us(10);
    reset(hw, 1, false)?;
    hw.delay_us(10);
    grf(hw, PLL, PLL);
    hw.delay_us(10);
    reset(hw, 2, false)?;
    let clock_us = wait(hw, CLOCK_READY, CLOCK_READY, 400, Step::Clock)?;
    replay(hw, true);
    reset(hw, 3, false)?;
    grf(hw, BIAS_BGR, BIAS_BGR);
    let lanes_us = wait(hw, READY, READY, 5000, Step::Lanes)?;
    let differences = mismatches(hw);
    if differences != 0 {
        return Err(Failure {
            step: Step::Readback,
            status: differences,
        });
    }
    Ok((clock_us, lanes_us))
}

/// At most two attempts. Caller keeps VP2 in standby until success and owns resuming video.
pub fn run(hw: &mut impl Hardware) -> Result<Locked, Failure> {
    for retried in [false, true] {
        match attempt(hw) {
            Ok((clock_us, lanes_us)) => {
                return Ok(Locked {
                    clock_us,
                    lanes_us,
                    retried,
                })
            }
            Err(error) if retried => return Err(error),
            Err(_) => {}
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::{collections::BTreeMap, vec::Vec};
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Op {
        Reset(u32, bool),
        Phy(u32, u32),
        Grf(u32, u32),
        Delay(u64),
    }
    struct Io {
        regs: BTreeMap<u32, u32>,
        ops: Vec<Op>,
        con: u32,
        status: u32,
        now: u64,
        attempts: u32,
        fail_clock: u32,
        fail_lanes: bool,
        stuck_lock: bool,
        stuck_word: Option<u32>,
        deny: Option<u32>,
        resets: [bool; 4],
    }
    impl Io {
        fn live() -> Self {
            Self {
                regs: IMAGE.iter().map(|w| (w.offset, w.value)).collect(),
                ops: Vec::new(),
                con: 0xe0,
                status: READY,
                now: 0,
                attempts: 0,
                fail_clock: 0,
                fail_lanes: false,
                stuck_lock: false,
                stuck_word: None,
                deny: None,
                resets: [false; 4],
            }
        }
        fn update(&mut self) {
            if self.stuck_lock {
                self.status = READY;
            } else if self.con & PLL == 0 || self.resets[1] || self.resets[2] {
                self.status = 0;
            } else if self.attempts <= self.fail_clock {
                self.status = 0;
            } else if self.resets[3] || self.fail_lanes {
                self.status = CLOCK_READY;
            } else {
                self.status = READY;
            }
        }
    }
    impl Hardware for Io {
        fn phy_read(&mut self, o: u32) -> u32 {
            assert!(!self.resets[0]);
            self.regs[&o]
        }
        fn phy_write(&mut self, o: u32, v: u32) {
            assert!(!self.resets[0]);
            let word = IMAGE
                .iter()
                .find(|w| w.offset == o)
                .expect("write outside documented image");
            assert_eq!(v & !word.mask, self.regs[&o] & !word.mask);
            self.ops.push(Op::Phy(o, v));
            if self.stuck_word != Some(o) {
                self.regs.insert(o, v);
            }
        }
        fn grf_read(&mut self, o: u32) -> u32 {
            match o {
                0 => self.con,
                0x80 => self.status,
                _ => panic!("unlisted GRF read"),
            }
        }
        fn grf_write(&mut self, o: u32, v: u32) {
            assert_eq!(o, 0);
            assert_eq!((v >> 16) & !0xe0, 0);
            self.ops.push(Op::Grf(o, v));
            self.con = (self.con & !(v >> 16)) | (v & (v >> 16));
            self.update();
        }
        fn reset(&mut self, line: u32, asserted: bool) -> bool {
            self.ops.push(Op::Reset(line, asserted));
            if self.deny == Some(line) {
                return false;
            }
            self.resets[line as usize] = asserted;
            if line == 0 && asserted {
                self.attempts += 1;
                for word in &IMAGE {
                    self.regs.insert(word.offset, 0x1000_0000);
                }
            }
            self.update();
            true
        }
        fn now_us(&mut self) -> u64 {
            self.now += 1;
            self.now
        }
        fn delay_us(&mut self, us: u64) {
            self.ops.push(Op::Delay(us));
            self.now += us;
        }
    }
    #[test]
    fn reset_reconstructs_the_measured_image_then_regains_clock_and_lane_lock() {
        let mut io = Io::live();
        assert_eq!(mismatches(&mut io), 0);
        let locked = run(&mut io).unwrap();
        assert!(!locked.retried);
        assert_eq!(io.attempts, 1);
        assert_eq!(mismatches(&mut io), 0);
        assert_eq!(io.status, READY);
        assert_eq!(io.resets, [false; 4]);
        assert_eq!(
            &io.ops[..6],
            &[
                Op::Reset(0, true),
                Op::Delay(20),
                Op::Reset(0, false),
                Op::Reset(3, true),
                Op::Reset(2, true),
                Op::Reset(1, true)
            ]
        );
        let cmn_release = io
            .ops
            .iter()
            .position(|o| *o == Op::Reset(2, false))
            .unwrap();
        let lane_write = io
            .ops
            .iter()
            .position(|o| matches!(o,Op::Phy(offset,_) if *offset>=0x800))
            .unwrap();
        let lane_release = io
            .ops
            .iter()
            .position(|o| *o == Op::Reset(3, false))
            .unwrap();
        assert!(cmn_release < lane_write && lane_write < lane_release);
        assert_eq!(
            io.ops.iter().filter(|o| matches!(o, Op::Phy(..))).count(),
            IMAGE.len()
        );
    }
    #[test]
    fn one_retry_recovers_but_failures_have_bounded_named_steps() {
        let mut io = Io::live();
        io.fail_clock = 1;
        assert!(run(&mut io).unwrap().retried);
        assert_eq!(io.attempts, 2);
        let mut io = Io::live();
        io.fail_clock = 2;
        assert_eq!(run(&mut io).unwrap_err().step, Step::Clock);
        assert_eq!(io.attempts, 2);
        assert!(io.now < 3000);
        let mut io = Io::live();
        io.fail_lanes = true;
        assert_eq!(run(&mut io).unwrap_err().step, Step::Lanes);
        assert_eq!(io.attempts, 2);
        assert!(io.now < 12_000);
        let mut io = Io::live();
        io.stuck_lock = true;
        assert_eq!(run(&mut io).unwrap_err().step, Step::Unlock);
        assert_eq!(io.attempts, 2);
        let mut io = Io::live();
        io.stuck_word = Some(0x144);
        assert_eq!(run(&mut io).unwrap_err().step, Step::Readback);
        assert_eq!(io.attempts, 2);
        let mut io = Io::live();
        io.deny = Some(0);
        assert_eq!(run(&mut io).unwrap_err().step, Step::Reset);
        assert!(!io
            .ops
            .iter()
            .any(|o| matches!(o, Op::Phy(..) | Op::Grf(..))));
    }
    #[test]
    fn image_is_unique_bounded_masked_and_pins_the_108_mhz_tmds_parameters() {
        let get = |o| IMAGE.iter().find(|w| w.offset == o).unwrap().value;
        assert_eq!(get(0x144), 135);
        assert_eq!(get(0x154), 135);
        assert_eq!(get(0x168) >> 4, 5);
        assert_eq!(get(0x178) & 0x40, 0);
        assert_eq!(24_000_000u64 * 135 / 6 * 2 / 10, 108_000_000);
        for (i, word) in IMAGE.iter().enumerate() {
            assert!(
                word.offset < 0x2000 && word.offset % 4 == 0 && word.mask > 0 && word.mask <= 0xff
            );
            if i > 0 {
                assert!(IMAGE[i - 1].offset < word.offset);
            }
            assert!(word.offset < 0x400 || word.offset >= 0x800);
        }
    }
}
