//j894
//! Ratified j894 fan policy and RK3588 PWM12/13 access. — KESTREL
use crate::{pwm, Regs, Sample};

pub const CLOCK_HZ: u32 = 24_000_000;
pub const PERIOD: u32 = 960;
pub const FLOOR: u16 = 200;
pub const FULL: u16 = 1000;
pub const RAMP_START_MC: i32 = 50_000;
pub const RAMP_END_MC: i32 = 75_000;
pub const HYSTERESIS_MC: i32 = 3_000;
pub const MAX_JUMP_MC: i32 = 20_000;
pub const STALL_NS: u64 = 3_000_000_000;
pub const RETRY_NS: u64 = 30_000_000_000;
pub const IOMUX_WRITE: u32 = 0x0ff0_0bb0;
pub const PULL_WRITE: u32 = 0x3000_3000;
pub const CTRL_RUN: u32 = 0b1011; // enabled, continuous, duty positive, inactive negative
pub const CTRL_CAPTURE: u32 = (1 << 10) | (1 << 2) | 1; // crystal clock, capture, enabled
pub const CAPTURE_EDGE: u32 = 1 << 1;
const LOCK: u32 = 1 << 6;
const INT_EN: u32 = 0x44;

pub fn curve(max_mc: i32) -> u16 {
    if max_mc <= RAMP_START_MC {
        FLOOR
    } else if max_mc >= RAMP_END_MC {
        FULL
    } else {
        FLOOR
            + ((max_mc - RAMP_START_MC) * i32::from(FULL - FLOOR) / (RAMP_END_MC - RAMP_START_MC))
                as u16
    }
}

/// Start high while disabled, then full-duty continuous output. Sora calls this before muxing.
/// Only PWM12 is changed; PWM14/15 are never touched. — KESTREL
pub fn start_full(regs: &mut impl Regs) {
    regs.write(pwm::ctrl(0), (1 << 4) | (1 << 3) | (1 << 1));
    regs.write(pwm::period(0), PERIOD);
    regs.write(pwm::duty(0), PERIOD);
    regs.write(pwm::ctrl(0), CTRL_RUN);
}

pub fn set_duty(regs: &mut impl Regs, permille: u16) {
    let duty = u32::from(permille.clamp(FLOOR, FULL)) * PERIOD / u32::from(FULL);
    regs.write(pwm::ctrl(0), CTRL_RUN | LOCK);
    regs.write(pwm::period(0), PERIOD);
    regs.write(pwm::duty(0), duty);
    regs.write(pwm::ctrl(0), CTRL_RUN);
}

pub fn start_capture(regs: &mut impl Regs) {
    regs.write(pwm::ctrl(1), 0);
    // Poll latched status, with no GIC interrupt authority and no other channel's IRQ changed.
    let enabled = regs.read(INT_EN);
    regs.write(INT_EN, enabled & !CAPTURE_EDGE);
    regs.write(pwm::INTSTS, CAPTURE_EDGE); // W1C only channel 1
    regs.write(pwm::ctrl(1), CTRL_CAPTURE);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Tach {
    pub edge: bool,
    pub rpm: u16,
}

pub fn rpm(high: u32, low: u32) -> Option<u16> {
    if high == 0 || low == 0 {
        return None;
    }
    let value = 30 * u64::from(CLOCK_HZ) / (u64::from(high) + u64::from(low));
    u16::try_from(value).ok().filter(|r| *r != 0)
}

pub fn read_tach(regs: &mut impl Regs) -> Tach {
    if regs.read(pwm::INTSTS) & CAPTURE_EDGE == 0 {
        return Tach::default();
    }
    let high = regs.read(pwm::period(1));
    let low = regs.read(pwm::duty(1));
    regs.write(pwm::INTSTS, CAPTURE_EDGE);
    Tach {
        edge: true,
        rpm: rpm(high, low).unwrap_or(0),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Auto,
    SensorFault,
    ClockFault,
    Stall,
}
impl State {
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::SensorFault => "sensor fault",
            Self::ClockFault => "clock fault",
            Self::Stall => "stall",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Output {
    pub duty: u16,
    pub state: State,
    pub retry: bool,
}

pub struct Controller {
    previous: Option<Sample>,
    anchor: Option<i32>,
    duty: u16,
    last_edge: u64,
    stalled_at: Option<u64>,
}
impl Controller {
    pub fn new(now: u64) -> Self {
        Self {
            previous: None,
            anchor: None,
            duty: FULL,
            last_edge: now,
            stalled_at: None,
        }
    }
    pub fn step(
        &mut self,
        now: u64,
        sample: Sample,
        sensor_running: bool,
        clocks_ok: bool,
        tach: Tach,
    ) -> Output {
        if tach.edge && tach.rpm > 0 {
            self.last_edge = now;
        }
        let jump = self.previous.is_some_and(|last| {
            sample
                .millicelsius
                .iter()
                .zip(last.millicelsius)
                .any(|(a, b)| a.zip(b).is_some_and(|(a, b)| (a - b).abs() > MAX_JUMP_MC))
        });
        // A jump needs a second consistent sample to recover; invalid channels never become a baseline.
        self.previous = sample.complete().then_some(sample);
        let mut retry = false;
        if let Some(start) = self.stalled_at {
            if now.saturating_sub(start) >= RETRY_NS {
                retry = true;
                self.stalled_at = if now.saturating_sub(self.last_edge) < STALL_NS {
                    None
                } else {
                    Some(now)
                };
            }
        } else if now.saturating_sub(self.last_edge) >= STALL_NS {
            self.stalled_at = Some(now);
        }
        let state = if !clocks_ok {
            State::ClockFault
        } else if !sensor_running || !sample.complete() || jump {
            State::SensorFault
        } else if self.stalled_at.is_some() {
            State::Stall
        } else {
            State::Auto
        };
        if state != State::Auto {
            self.duty = FULL;
            self.anchor = None;
        } else if let Some(max) = sample.max() {
            let target = curve(max);
            if self.anchor.is_none()
                || target > self.duty
                || self.anchor.is_some_and(|a| max <= a - HYSTERESIS_MC)
            {
                self.duty = target;
                self.anchor = Some(max);
            }
        }
        Output {
            duty: self.duty,
            state,
            retry,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    fn sample(c: i32) -> Sample {
        Sample {
            millicelsius: [Some(c * 1000); 7],
        }
    }
    fn edge() -> Tach {
        Tach {
            edge: true,
            rpm: 2500,
        }
    }
    #[test]
    fn curve_boundaries_and_never_zero() {
        assert_eq!(curve(49_000), 200);
        assert_eq!(curve(50_000), 200);
        assert_eq!(curve(62_500), 600);
        assert_eq!(curve(75_000), 1000);
        for t in -40_000..=125_000 {
            assert!((200..=1000).contains(&curve(t)));
        }
    }
    #[test]
    fn hysteresis_then_immediate_heat_response() {
        let mut c = Controller::new(0);
        assert_eq!(c.step(0, sample(65), true, true, edge()).duty, 680);
        assert_eq!(c.step(1, sample(63), true, true, edge()).duty, 680);
        assert_eq!(c.step(2, sample(62), true, true, edge()).duty, 584);
        assert_eq!(c.step(3, sample(64), true, true, edge()).duty, 648);
    }
    #[test]
    fn all_faults_go_full_and_recover_after_a_consistent_sample() {
        let mut c = Controller::new(0);
        assert_eq!(c.step(0, sample(40), true, true, edge()).duty, 200);
        assert_eq!(
            c.step(1, sample(70), true, true, edge()).state,
            State::SensorFault
        );
        assert_eq!(c.step(2, sample(70), true, true, edge()).duty, 840);
        let mut invalid = sample(40);
        invalid.millicelsius[5] = None;
        assert_eq!(c.step(3, invalid, true, true, edge()).duty, 1000);
        assert_eq!(
            c.step(4, sample(40), false, true, edge()).state,
            State::SensorFault
        );
        assert_eq!(
            c.step(5, sample(40), true, false, edge()).state,
            State::ClockFault
        );
        assert_eq!(c.step(6, sample(40), true, true, edge()).duty, 200);
    }
    #[test]
    fn stall_uses_elapsed_time_and_holds_full_until_retry() {
        let mut c = Controller::new(0);
        assert_eq!(
            c.step(2_999_999_999, sample(40), true, true, Tach::default())
                .state,
            State::Auto
        );
        assert_eq!(
            c.step(STALL_NS, sample(40), true, true, Tach::default())
                .state,
            State::Stall
        );
        assert_eq!(
            c.step(STALL_NS + 1, sample(40), true, true, edge()).duty,
            1000
        );
        let out = c.step(STALL_NS + RETRY_NS, sample(40), true, true, edge());
        assert!(out.retry);
        assert_eq!(out.state, State::Auto);
        assert_eq!(out.duty, 200);
    }
    #[test]
    fn tach_conversion_handles_stale_zero_and_overflow() {
        assert_eq!(rpm(60000, 60000), Some(6000));
        assert_eq!(rpm(120000, 120000), Some(3000));
        assert_eq!(rpm(0, 120000), None);
        assert_eq!(rpm(1, 1), None);
        assert_eq!(rpm(u32::MAX, u32::MAX), None);
    }
    #[test]
    fn masked_pads_preserve_every_other_field() {
        for value in [0u32, 0xffff, 0xa55a] {
            for (word, mask, want) in [(IOMUX_WRITE, 0x0ff0, 0x0bb0), (PULL_WRITE, 0x3000, 0x3000)]
            {
                assert_eq!(word >> 16, mask);
                let after = (value & !mask) | (word & mask);
                assert_eq!(after & !mask, value & !mask);
                assert_eq!(after & mask, want);
            }
        }
    }
    #[test]
    fn hardware_sequences_keep_tach_and_other_channels_separate() {
        struct Fake {
            writes: std::vec::Vec<(u32, u32)>,
        }
        impl Regs for Fake {
            fn read(&mut self, _: u32) -> u32 {
                0
            }
            fn write(&mut self, o: u32, v: u32) {
                self.writes.push((o, v));
            }
        }
        let mut f = Fake {
            writes: std::vec::Vec::new(),
        };
        start_full(&mut f);
        assert_eq!(f.writes, [(12, 26), (4, 960), (8, 960), (12, 11)]);
        f.writes.clear();
        set_duty(&mut f, 200);
        assert_eq!(f.writes, [(12, 75), (4, 960), (8, 192), (12, 11)]);
        f.writes.clear();
        start_capture(&mut f);
        assert_eq!(f.writes, [(28, 0), (68, 0), (64, 2), (28, 1029)]);
    }
}
