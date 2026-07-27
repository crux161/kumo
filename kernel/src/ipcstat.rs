//! S0 — what one keystroke actually costs.
//!
//! `DESIGN/018` puts the USB-HID input path at roughly nine syscalls and four context switches per
//! keystroke. That figure was obtained by reading the path and counting the calls on it, which is
//! a reasonable way to form a hypothesis and no way at all to justify a claim. PLAN/009 forbids
//! every later slice from claiming an improvement until a measured number exists. This is that
//! measurement.
//!
//! ## Two costs, not one — learned from the first run
//!
//! The first version of this file fitted a single slope: cost per device interrupt, with a fixed
//! per-interval constant cancelled between two intervals. The first measurement on metal returned
//! **108 context switches per interrupt**, which is absurd, and the absurdity was the finding.
//!
//! The system pays *two* costs, and only one of them is caused by input:
//!
//!   count  =  a · (device interrupts)  +  b · (elapsed seconds)
//!
//! There is a substantial **time-proportional background** — the scheduler tick, the idle floor
//! being pumped, resident servers waking on their own timers. It accrues whether or not anybody
//! touches the keyboard. Over a 30-second interval it dwarfs the input cost, and a one-term fit
//! has nowhere to put it except on the interrupts, which is exactly what produced 108.
//!
//! Two intervals give two equations, so both terms are recoverable — *provided the intervals
//! differ in the right way*. Two runs of the same length separate nothing no matter how long they
//! last. The pair must differ in the **ratio** of typing to time: one interval mostly typing, one
//! interval mostly waiting. [`Fit::separation_pct`] reports how well the pair managed that, so a
//! weak measurement announces itself instead of being quoted.
//!
//! Both axes are counted by the kernel — the denominator is *measured*, not "how many characters I
//! think I typed" — so a slip of the finger costs accuracy in neither.
//!
//! ## Two input paths, two denominators — learned from the second run
//!
//! The first working version counted only device interrupts. Driven over **serial** it reported
//! `devirq=0` and then blamed the operator's typing pattern, because serial input is *polled* by
//! the shell's REPL loop and raises no interrupt at all. The denominator was structurally zero and
//! the instrument said nothing useful about it.
//!
//! So both paths are counted, and they are deliberately *not* summed: they are different costs and
//! the difference is the interesting part. The USB path wakes a driver, crosses two address spaces
//! and acknowledges an interrupt; the serial path is a polled byte the kernel already had. The
//! second is the floor, the first is what PLAN/009's Tier A is trying to remove.
//!
//! ## The unit is a device interrupt, not a keystroke
//!
//! A USB HID keyboard reports on press *and* on release, and each report traverses the whole path:
//! interrupt, driver wake, decode, channel write, Sora wake, ttyd round trip. So a typed character
//! is normally **two** of the thing this counts. Reporting per-interrupt keeps the measured unit
//! the same as the counted one; the per-character figure is that doubled, and the report says so
//! rather than quietly picking one.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Device interrupts (SPI, i.e. `irq >= 32`) actually **delivered** to a driver.
///
/// Counted after delivery, not on entry: `signal_irq` can find `SoraState` borrowed, unmask the
/// line and return so the controller re-asserts. Counting at entry would score that retry twice
/// and inflate the denominator, which would flatter every number derived from it.
/// Below this conditioning the fit is reported but flagged: the pair could not really tell a
/// per-interrupt cost from a per-second one, and quoting the number without the caveat is how an
/// estimate becomes a fact by repetition.
const MIN_SEPARATION_PCT: i64 = 25;

static DEVICE_IRQS: AtomicU64 = AtomicU64::new(0);

pub fn note_device_irq() {
    DEVICE_IRQS.fetch_add(1, Ordering::Relaxed);
}

pub fn device_irqs() -> u64 {
    DEVICE_IRQS.load(Ordering::Relaxed)
}

/// Serial keystrokes delivered to userland. The shell's REPL polls the UART, so this path raises no
/// interrupt whatsoever — counting only [`DEVICE_IRQS`] measures a serial session as zero input.
static SERIAL_KEYS: AtomicU64 = AtomicU64::new(0);

pub fn note_serial_key() {
    SERIAL_KEYS.fetch_add(1, Ordering::Relaxed);
}

pub fn serial_keys() -> u64 {
    SERIAL_KEYS.load(Ordering::Relaxed)
}

/// Which input path a fit is measured against. They are never mixed: one USB character is two
/// reports (press and release) and one serial character is one byte, so a sum would be a rate per
/// nothing-in-particular.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputAxis {
    DeviceIrq,
    SerialKey,
}

impl InputAxis {
    pub const fn unit(self) -> &'static str {
        match self {
            InputAxis::DeviceIrq => "irq",
            InputAxis::SerialKey => "key",
        }
    }

    /// How many events one typed character costs on this path.
    pub const fn events_per_char(self) -> i64 {
        match self {
            InputAxis::DeviceIrq => 2, // press and release
            InputAxis::SerialKey => 1,
        }
    }

    fn of(self, i: Interval) -> u64 {
        match self {
            InputAxis::DeviceIrq => i.device_irqs,
            InputAxis::SerialKey => i.serial_keys,
        }
    }
}

/// One reading of every counter on the input path.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Sample {
    /// SVC traps from every EL0 process — Sora, the drivers, ttyd alike.
    pub syscalls: u32,
    /// Scheduler context switches (`dispatch_context`), voluntary and preemptive together.
    pub switches: u64,
    pub device_irqs: u64,
    pub serial_keys: u64,
    pub uptime_ns: u64,
}

/// The difference between two [`Sample`]s.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Interval {
    pub syscalls: u32,
    pub switches: u64,
    pub device_irqs: u64,
    pub serial_keys: u64,
    pub elapsed_ns: u64,
}

impl Sample {
    pub fn interval_since(self, base: Self) -> Interval {
        Interval {
            // The HAL's SVC counter is 32-bit. `wrapping_sub` is the correct difference across one
            // wrap, and one wrap is 4.29 billion syscalls — far beyond any measurement run.
            syscalls: self.syscalls.wrapping_sub(base.syscalls),
            switches: self.switches.saturating_sub(base.switches),
            device_irqs: self.device_irqs.saturating_sub(base.device_irqs),
            serial_keys: self.serial_keys.saturating_sub(base.serial_keys),
            elapsed_ns: self.uptime_ns.saturating_sub(base.uptime_ns),
        }
    }
}

/// The two-term cost model, in hundredths to stay in integer arithmetic — the kernel is soft-float
/// on x86 and has no business doing floating point for a diagnostic. Per-second terms are signed:
/// a series with no real time dependence lands near zero from either side, and rounding it up to
/// zero would hide that it was measured rather than assumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fit {
    pub syscalls_per_irq_x100: i64,
    pub syscalls_per_sec_x100: i64,
    pub switches_per_irq_x100: i64,
    pub switches_per_sec_x100: i64,
    /// How separable the two intervals were, 0-100. This is the conditioning of the system, not a
    /// confidence interval: it says whether the pair *could* distinguish the two costs, not
    /// whether the answer is right. Two intervals with the same typing-to-time ratio score 0 and
    /// yield nothing; a typing-heavy interval paired with a waiting-heavy one scores high.
    pub separation_pct: i64,
    /// How many intervals the estimate rests on. A weak `separation_pct` over many samples is a
    /// far better number than a strong one over two, and hiding the count would conceal that.
    pub samples: u64,
}

/// A least-squares estimate over every interval measured so far on one axis.
///
/// Two intervals give an exact solve, and that is all the earlier version could do — so three good
/// measurements arrived as three separate answers, each flagged weak, with no way to combine them.
/// Least squares subsumes the exact case (with n = 2 it *is* the exact solve) and lets a session
/// converge: each interval a human types is another equation, and the estimate tightens instead of
/// being replaced.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Accumulator {
    pub n: u64,
    /// Σi², Σi·t, Σt² — the normal-equation matrix.
    pub sii: u128,
    pub sit: u128,
    pub stt: u128,
    /// Σi·c and Σt·c for each series.
    pub sic_syscalls: u128,
    pub stc_syscalls: u128,
    pub sic_switches: u128,
    pub stc_switches: u128,
}

impl Accumulator {
    /// Fold one interval in. `axis` selects the input counter; time is in milliseconds.
    pub fn add(&mut self, interval: Interval, axis: InputAxis) {
        let i = u128::from(axis.of(interval));
        let t = u128::from(interval.elapsed_ns / 1_000_000);
        self.n += 1;
        self.sii += i * i;
        self.sit += i * t;
        self.stt += t * t;
        self.sic_syscalls += i * u128::from(interval.syscalls);
        self.stc_syscalls += t * u128::from(interval.syscalls);
        self.sic_switches += i * u128::from(interval.switches);
        self.stc_switches += t * u128::from(interval.switches);
    }

    /// Solve the normal equations. `None` until two intervals exist, or when the accumulated
    /// intervals all share one input-to-time ratio and the system stays singular.
    pub fn solve(&self) -> Option<Fit> {
        if self.n < 2 {
            return None;
        }
        let (sii, sit, stt) = (self.sii as i128, self.sit as i128, self.stt as i128);
        let det = sii * stt - sit * sit;
        if det == 0 {
            return None;
        }
        let solve_one = |sic: u128, stc: u128| -> (i64, i64) {
            let (sic, stc) = (sic as i128, stc as i128);
            let per_i = (sic * stt - stc * sit) * 100 / det;
            // Milliseconds in, per-second out.
            let per_t = (sii * stc - sit * sic) * 100_000 / det;
            (per_i as i64, per_t as i64)
        };
        let (syscalls_per_irq_x100, syscalls_per_sec_x100) =
            solve_one(self.sic_syscalls, self.stc_syscalls);
        let (switches_per_irq_x100, switches_per_sec_x100) =
            solve_one(self.sic_switches, self.stc_switches);
        let scale = sii * stt;
        Some(Fit {
            syscalls_per_irq_x100,
            syscalls_per_sec_x100,
            switches_per_irq_x100,
            switches_per_sec_x100,
            separation_pct: if scale == 0 {
                0
            } else {
                (det * 100 / scale) as i64
            },
            samples: self.n,
        })
    }
}

/// Solve one series (syscalls or switches) for its per-interrupt and per-second terms.
/// `det` is the shared system determinant, `i`/`t` the interrupt counts and elapsed milliseconds.
fn solve(c1: i128, c2: i128, i1: i128, t1: i128, i2: i128, t2: i128, det: i128) -> (i64, i64) {
    let per_irq = (c1 * t2 - c2 * t1) * 100 / det;
    // The time row's determinant is the negation of the interrupt row's. Scaling by 100_000
    // rather than 100 converts per-millisecond to per-second without a second rounding step.
    let per_sec = (c1 * i2 - c2 * i1) * 100_000 / -det;
    (per_irq as i64, per_sec as i64)
}

/// Fit from exactly two intervals — the exact solve, and what [`Accumulator`] reduces to at n = 2.
pub fn fit(a: Interval, b: Interval, axis: InputAxis) -> Option<Fit> {
    let mut acc = Accumulator::default();
    acc.add(a, axis);
    acc.add(b, axis);
    acc.solve()
}

// ---- the latched state behind the `ipcstat` shell command -----------------------------

static BASE_SYSCALLS: AtomicU32 = AtomicU32::new(0);
static BASE_SWITCHES: AtomicU64 = AtomicU64::new(0);
static BASE_IRQS: AtomicU64 = AtomicU64::new(0);
static BASE_KEYS: AtomicU64 = AtomicU64::new(0);
static BASE_UPTIME: AtomicU64 = AtomicU64::new(0);
static HAVE_BASE: AtomicBool = AtomicBool::new(false);

static PREV_SYSCALLS: AtomicU32 = AtomicU32::new(0);
static PREV_SWITCHES: AtomicU64 = AtomicU64::new(0);
static PREV_IRQS: AtomicU64 = AtomicU64::new(0);
static PREV_KEYS: AtomicU64 = AtomicU64::new(0);
static PREV_ELAPSED: AtomicU64 = AtomicU64::new(0);
static HAVE_PREV: AtomicBool = AtomicBool::new(false);

/// The session's least-squares accumulator, and the axis it was built on. Switching input paths
/// resets it: an estimate that mixed serial keys with USB reports would be a rate per nothing.
static ACC: (
    AtomicU64,
    AtomicU64,
    AtomicU64,
    AtomicU64,
    AtomicU64,
    AtomicU64,
    AtomicU64,
    AtomicU64,
) = (
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
);
/// 0 = none yet, 1 = device IRQ, 2 = serial key.
static ACC_AXIS: AtomicU32 = AtomicU32::new(0);

fn acc_load() -> Accumulator {
    Accumulator {
        n: ACC.0.load(Ordering::Relaxed),
        sii: u128::from(ACC.1.load(Ordering::Relaxed)),
        sit: u128::from(ACC.2.load(Ordering::Relaxed)),
        stt: u128::from(ACC.3.load(Ordering::Relaxed)),
        sic_syscalls: u128::from(ACC.4.load(Ordering::Relaxed)),
        stc_syscalls: u128::from(ACC.5.load(Ordering::Relaxed)),
        sic_switches: u128::from(ACC.6.load(Ordering::Relaxed)),
        stc_switches: u128::from(ACC.7.load(Ordering::Relaxed)),
    }
}

fn acc_store(a: Accumulator) {
    ACC.0.store(a.n, Ordering::Relaxed);
    ACC.1.store(a.sii as u64, Ordering::Relaxed);
    ACC.2.store(a.sit as u64, Ordering::Relaxed);
    ACC.3.store(a.stt as u64, Ordering::Relaxed);
    ACC.4.store(a.sic_syscalls as u64, Ordering::Relaxed);
    ACC.5.store(a.stc_syscalls as u64, Ordering::Relaxed);
    ACC.6.store(a.sic_switches as u64, Ordering::Relaxed);
    ACC.7.store(a.stc_switches as u64, Ordering::Relaxed);
}

/// Forget every accumulated interval — `ipcstat reset`.
pub fn reset() {
    acc_store(Accumulator::default());
    ACC_AXIS.store(0, Ordering::Relaxed);
    HAVE_BASE.store(false, Ordering::Relaxed);
    HAVE_PREV.store(false, Ordering::Relaxed);
}

/// Read every counter. `switch_count` is only valid once the user-thread harness exists.
pub fn sample() -> Sample {
    Sample {
        syscalls: kumo_hal::active::syscall_count(),
        switches: if crate::user_thread::is_started() {
            crate::user_thread::switch_count()
        } else {
            0
        },
        device_irqs: device_irqs(),
        serial_keys: serial_keys(),
        uptime_ns: kumo_hal::active::monotonic_nanos(),
    }
}

fn latch_base(s: Sample) {
    BASE_SYSCALLS.store(s.syscalls, Ordering::Relaxed);
    BASE_SWITCHES.store(s.switches, Ordering::Relaxed);
    BASE_IRQS.store(s.device_irqs, Ordering::Relaxed);
    BASE_KEYS.store(s.serial_keys, Ordering::Relaxed);
    BASE_UPTIME.store(s.uptime_ns, Ordering::Relaxed);
    HAVE_BASE.store(true, Ordering::Relaxed);
}

fn base() -> Sample {
    Sample {
        syscalls: BASE_SYSCALLS.load(Ordering::Relaxed),
        switches: BASE_SWITCHES.load(Ordering::Relaxed),
        device_irqs: BASE_IRQS.load(Ordering::Relaxed),
        serial_keys: BASE_KEYS.load(Ordering::Relaxed),
        uptime_ns: BASE_UPTIME.load(Ordering::Relaxed),
    }
}

fn latch_prev(i: Interval) {
    PREV_SYSCALLS.store(i.syscalls, Ordering::Relaxed);
    PREV_SWITCHES.store(i.switches, Ordering::Relaxed);
    PREV_IRQS.store(i.device_irqs, Ordering::Relaxed);
    PREV_KEYS.store(i.serial_keys, Ordering::Relaxed);
    PREV_ELAPSED.store(i.elapsed_ns, Ordering::Relaxed);
    HAVE_PREV.store(true, Ordering::Relaxed);
}

fn prev() -> Interval {
    Interval {
        syscalls: PREV_SYSCALLS.load(Ordering::Relaxed),
        switches: PREV_SWITCHES.load(Ordering::Relaxed),
        device_irqs: PREV_IRQS.load(Ordering::Relaxed),
        serial_keys: PREV_KEYS.load(Ordering::Relaxed),
        elapsed_ns: PREV_ELAPSED.load(Ordering::Relaxed),
    }
}

/// Render `x100` fixed point as a signed decimal with two places.
fn write_x100(out: &mut dyn Write, v: i64) {
    let sign = if v < 0 { "-" } else { "" };
    let m = v.unsigned_abs();
    let _ = write!(out, "{}{}.{:02}", sign, m / 100, m % 100);
}

/// The `ipcstat` command.
///
/// First call latches a baseline. Each later call closes an interval, prints it, and — once two
/// intervals exist — prints the fitted marginal cost. Usage is: `ipcstat`, type a short run,
/// `ipcstat`, type a much longer run, `ipcstat`. The two runs must differ in length; that
/// difference is the entire measurement.
pub fn report(out: &mut dyn Write) {
    let now = sample();

    if !HAVE_BASE.load(Ordering::Relaxed) {
        latch_base(now);
        let _ = write!(
            out,
            "ipcstat: baseline latched (syscalls={} switches={} devirq={})\r\n\
             ipcstat: now type a run of characters, then 'ipcstat' again\r\n",
            now.syscalls, now.switches, now.device_irqs
        );
        let _ = write!(out, "ipcstat: (serial keys={})\r\n", now.serial_keys);
        return;
    }

    let interval = now.interval_since(base());
    let ms = interval.elapsed_ns / 1_000_000;
    let _ = write!(
        out,
        "ipcstat: interval syscalls={} switches={} devirq={} keys={} in {}.{:03} s\r\n",
        interval.syscalls,
        interval.switches,
        interval.device_irqs,
        interval.serial_keys,
        ms / 1000,
        ms % 1000
    );
    // Rates on every interval, not only on a pair. An interval with no input at all is a direct
    // reading of the time-proportional background, which is worth having on its own.
    if ms > 0 {
        let _ = out.write_str("ipcstat: rates ");
        write_x100(out, (i64::from(interval.syscalls) * 100_000) / ms as i64);
        let _ = out.write_str(" syscalls/s  ");
        write_x100(out, (interval.switches as i64 * 100_000) / ms as i64);
        let _ = out.write_str(" switches/s\r\n");
    }

    // Choose the path that actually carried input; prefer USB when both did.
    let axis = if interval.device_irqs > 0 {
        Some(InputAxis::DeviceIrq)
    } else if interval.serial_keys > 0 {
        Some(InputAxis::SerialKey)
    } else {
        None
    };

    match axis {
        Some(axis) => {
            let tag = match axis {
                InputAxis::DeviceIrq => 1,
                InputAxis::SerialKey => 2,
            };
            // A change of input path invalidates everything accumulated: one serial key is one
            // character and one USB report is half of one, so a mixed estimate measures nothing.
            if ACC_AXIS.swap(tag, Ordering::Relaxed) != tag {
                acc_store(Accumulator::default());
            }
            let mut acc = acc_load();
            acc.add(interval, axis);
            acc_store(acc);

            match acc.solve() {
                Some(f) => {
                    let unit = axis.unit();
                    let _ = out.write_str("ipcstat: syscalls ");
                    write_x100(out, f.syscalls_per_irq_x100);
                    let _ = write!(out, "/{unit} + ");
                    write_x100(out, f.syscalls_per_sec_x100);
                    let _ = out.write_str("/s idle\r\n");

                    let _ = out.write_str("ipcstat: switches ");
                    write_x100(out, f.switches_per_irq_x100);
                    let _ = write!(out, "/{unit} + ");
                    write_x100(out, f.switches_per_sec_x100);
                    let _ = out.write_str("/s idle\r\n");

                    let per_char = axis.events_per_char();
                    let _ = write!(out, "ipcstat: per typed char ({per_char} {unit}) ");
                    write_x100(out, f.syscalls_per_irq_x100.saturating_mul(per_char));
                    let _ = out.write_str(" syscalls  ");
                    write_x100(out, f.switches_per_irq_x100.saturating_mul(per_char));
                    let _ = out.write_str(" switches\r\n");

                    let _ = write!(
                        out,
                        "ipcstat: least squares over {} intervals, separation {}%\r\n",
                        f.samples, f.separation_pct
                    );
                    if f.separation_pct < MIN_SEPARATION_PCT && f.samples < 4 {
                        let _ = out.write_str(
                            "ipcstat: weak - add intervals, or pair typing-heavy with waiting-heavy\r\n",
                        );
                    }
                }
                None => {
                    let _ = out.write_str(
                        "ipcstat: one interval so far - run it again after more typing\r\n",
                    );
                }
            }
        }
        None => {
            let _ = out.write_str(
                "ipcstat: no input reached the kernel on either path in that interval\r\n",
            );
        }
    }

    latch_prev(interval);
    latch_base(now);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interval(syscalls: u32, switches: u64, device_irqs: u64, ms: u64) -> Interval {
        Interval {
            syscalls,
            switches,
            device_irqs,
            serial_keys: 0,
            elapsed_ns: ms * 1_000_000,
        }
    }

    fn serial_interval(syscalls: u32, switches: u64, serial_keys: u64, ms: u64) -> Interval {
        Interval {
            syscalls,
            switches,
            device_irqs: 0,
            serial_keys,
            elapsed_ns: ms * 1_000_000,
        }
    }

    fn fit_irq(a: Interval, b: Interval) -> Option<Fit> {
        fit(a, b, InputAxis::DeviceIrq)
    }

    #[test]
    fn interval_subtracts_each_counter() {
        let base = Sample {
            syscalls: 100,
            switches: 50,
            device_irqs: 10,
            serial_keys: 0,
            uptime_ns: 1_000,
        };
        let now = Sample {
            syscalls: 175,
            switches: 70,
            device_irqs: 18,
            serial_keys: 0,
            uptime_ns: 5_000,
        };
        assert_eq!(
            now.interval_since(base),
            Interval {
                syscalls: 75,
                switches: 20,
                device_irqs: 8,
                serial_keys: 0,
                elapsed_ns: 4_000,
            }
        );
    }

    #[test]
    fn the_syscall_counter_survives_its_32_bit_wrap() {
        let base = Sample {
            syscalls: u32::MAX - 5,
            ..Sample::default()
        };
        let now = Sample {
            syscalls: 4, // wrapped
            ..Sample::default()
        };
        assert_eq!(now.interval_since(base).syscalls, 10);
    }

    #[test]
    fn fit_separates_input_cost_from_the_time_background() {
        // Construct two intervals from a known model: 9 syscalls and 4 switches per interrupt,
        // plus 40 switches and no syscalls per second of background.
        // A: typing-heavy — 200 interrupts in 10 s.  B: waiting-heavy — 20 interrupts in 60 s.
        let a = interval(200 * 9, 200 * 4 + 40 * 10, 200, 10_000);
        let b = interval(20 * 9, 20 * 4 + 40 * 60, 20, 60_000);
        let f = fit_irq(a, b).expect("fit");
        assert_eq!(f.syscalls_per_irq_x100, 900);
        assert_eq!(f.syscalls_per_sec_x100, 0);
        assert_eq!(f.switches_per_irq_x100, 400);
        assert_eq!(f.switches_per_sec_x100, 4000);
    }

    #[test]
    fn a_one_term_reading_of_that_same_data_would_have_been_wildly_wrong() {
        // The regression this file exists to prevent: on metal, two intervals of nearly equal
        // typing gave a naive slope of 108 switches per interrupt because the whole time
        // background landed on a 2-interrupt span. The two-term fit recovers the real numbers.
        let a = interval(1139, 1857, 121, 32_672);
        let b = interval(1155, 2073, 123, 37_816);
        let f = fit_irq(a, b).expect("fit");
        // Naive: (2073 - 1857) / (123 - 121) = 108 switches/irq. Actual, once time is accounted:
        assert!(
            (400..=500).contains(&f.switches_per_irq_x100),
            "switches/irq was {}",
            f.switches_per_irq_x100
        );
        assert!(
            (900..=1000).contains(&f.syscalls_per_irq_x100),
            "syscalls/irq was {}",
            f.syscalls_per_irq_x100
        );
        // And it flags itself: those two intervals barely differed in ratio.
        assert!(f.separation_pct < MIN_SEPARATION_PCT);
    }

    #[test]
    fn a_well_separated_pair_says_so() {
        let a = interval(200 * 9, 200 * 4 + 40 * 10, 200, 10_000);
        let b = interval(20 * 9, 20 * 4 + 40 * 60, 20, 60_000);
        assert!(fit_irq(a, b).expect("fit").separation_pct >= MIN_SEPARATION_PCT);
    }

    #[test]
    fn fit_does_not_care_which_interval_came_first() {
        let a = interval(1800, 1200, 200, 10_000);
        let b = interval(180, 2480, 20, 60_000);
        assert_eq!(fit_irq(a, b), fit_irq(b, a));
    }

    #[test]
    fn identical_typing_to_time_ratios_are_singular() {
        // Twice the interrupts in twice the time is the same equation written larger.
        let a = interval(900, 600, 100, 10_000);
        let b = interval(1800, 1200, 200, 20_000);
        assert!(fit_irq(a, b).is_none());
    }

    #[test]
    fn a_serial_session_registers_as_zero_input_on_the_interrupt_axis() {
        // The failure a serial-driven session actually produced: polled input raises no interrupt,
        // so the USB axis sees nothing at all. That must be distinguishable from "your two
        // intervals were too alike", which is what it was mistakenly reported as.
        let a = serial_interval(384, 550, 25, 9_140);
        let b = serial_interval(944, 1440, 61, 23_973);
        assert!(fit(a, b, InputAxis::DeviceIrq).is_none());
        // On the axis the input actually used, the same pair fits.
        let f = fit(a, b, InputAxis::SerialKey).expect("serial fit");
        assert!(f.syscalls_per_irq_x100 > 0, "{}", f.syscalls_per_irq_x100);
    }

    #[test]
    fn the_three_metal_intervals_converge_on_one_estimate() {
        // The USB session measured on the Orange Pi 5 Plus. Each pair, taken alone, was flagged
        // weak; together they are the S0 answer, and this is the fixture that pins it.
        let mut acc = Accumulator::default();
        for i in [
            interval(1982, 2681, 222, 40_375),
            interval(2208, 2285, 242, 27_311),
            interval(3870, 4330, 416, 56_968),
        ] {
            acc.add(i, InputAxis::DeviceIrq);
        }
        let f = acc.solve().expect("fit");
        assert_eq!(f.samples, 3);
        assert!(
            (900..=1050).contains(&f.syscalls_per_irq_x100),
            "syscalls/irq {}",
            f.syscalls_per_irq_x100
        );
        assert!(
            (480..=560).contains(&f.switches_per_irq_x100),
            "switches/irq {}",
            f.switches_per_irq_x100
        );
        // The idle background is the most tightly determined figure of the three.
        assert!(
            (3600..=4000).contains(&f.switches_per_sec_x100),
            "switches/s {}",
            f.switches_per_sec_x100
        );
    }

    #[test]
    fn two_intervals_still_give_the_exact_solve() {
        // Least squares must subsume the pairwise case rather than replace it.
        let a = interval(200 * 9, 200 * 4 + 40 * 10, 200, 10_000);
        let b = interval(20 * 9, 20 * 4 + 40 * 60, 20, 60_000);
        let f = fit(a, b, InputAxis::DeviceIrq).expect("fit");
        assert_eq!(f.samples, 2);
        assert_eq!(f.syscalls_per_irq_x100, 900);
        assert_eq!(f.switches_per_irq_x100, 400);
        assert_eq!(f.switches_per_sec_x100, 4000);
    }

    #[test]
    fn one_serial_key_is_one_typed_character_but_one_usb_char_is_two_reports() {
        assert_eq!(InputAxis::SerialKey.events_per_char(), 1);
        assert_eq!(InputAxis::DeviceIrq.events_per_char(), 2);
    }

    #[test]
    fn a_series_with_no_time_dependence_can_report_a_negative_background() {
        // Measurement noise puts a true-zero background on either side of zero. Reporting the
        // sign honestly is the point; clamping it would disguise a measured value as an assumed
        // one.
        let a = interval(905, 0, 100, 10_000);
        let b = interval(179, 0, 20, 60_000);
        let f = fit_irq(a, b).expect("fit");
        assert!(f.syscalls_per_sec_x100 < 0, "{}", f.syscalls_per_sec_x100);
    }
}
