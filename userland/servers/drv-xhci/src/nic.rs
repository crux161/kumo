//j831
//j835
//! NET4c-3 resident NIC lanes: the ordering rules, without hardware. drv-xhci issues
//! the commands and reads the contexts back; this decides whether the next step is
//! allowed. Arm only while Running. Stop only after every pending transfer was
//! cancelled through a verified NET4c-2 stop. Disable only once stopped. Reinitialize
//! a ring only after the controller read back Disabled, the one point at which it
//! cannot reach that memory. A failed step is sticky: rings, buffers and their parent
//! custody are retained and nothing is reused.
//! Recovery class: stateless; the custody parent holds the DMA. — TIMBERDOODLE

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NicLane {
    Rx,
    Status,
    /// NET7a bulk OUT; armed only while a frame is being handed to the bus.
    Tx,
}

const LANES: [NicLane; 3] = [NicLane::Rx, NicLane::Status, NicLane::Tx];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NicPhase {
    /// Rings initialized; the controller has no endpoint context naming them.
    Idle,
    /// Configure Endpoint succeeded and both contexts read Running.
    Running,
    /// Both lanes were cancelled through a verified stop, or never armed.
    Stopped,
    /// Deconfigure succeeded; contexts read Disabled and the slot Addressed.
    Disabled,
    /// A step failed or was refused. Everything stays retained.
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NicError {
    Phase(NicPhase),
    Pending(NicLane),
    NotPending(NicLane),
}

/// Validate the actual length before reading DMA, including a completion that
/// raced a verified stop. Error codes and impossible residuals expose no bytes.
/// — TIMBERDOODLE
pub fn completed_length(requested: u32, code: u8, remaining: u32) -> Option<usize> {
    if !matches!(code, 1 | 13) {
        return None;
    }
    requested.checked_sub(remaining).map(|n| n as usize)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NicCounts {
    pub configures: u32,
    pub arms: u32,
    pub completions: u32,
    pub cancels: u32,
    pub stops: u32,
    pub disables: u32,
    pub reclaims: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NicLifecycle {
    phase: NicPhase,
    pending: [bool; 3],
    counts: NicCounts,
}

impl NicLifecycle {
    pub const fn new() -> Self {
        Self {
            phase: NicPhase::Idle,
            pending: [false; 3],
            counts: NicCounts {
                configures: 0,
                arms: 0,
                completions: 0,
                cancels: 0,
                stops: 0,
                disables: 0,
                reclaims: 0,
            },
        }
    }

    pub const fn phase(&self) -> NicPhase {
        self.phase
    }

    pub const fn counts(&self) -> NicCounts {
        self.counts
    }

    pub const fn is_pending(&self, lane: NicLane) -> bool {
        self.pending[lane as usize]
    }

    /// Only freshly initialized Idle rings may be configured.
    pub const fn may_configure(&self) -> bool {
        matches!(self.phase, NicPhase::Idle)
    }

    /// Refuse and, unless already failed, become Failed: nothing is reused after a
    /// refused step.
    fn refuse(&mut self, error: NicError) -> Result<(), NicError> {
        self.phase = NicPhase::Failed;
        Err(error)
    }

    fn require(&mut self, phase: NicPhase) -> Result<(), NicError> {
        if self.phase == phase {
            Ok(())
        } else {
            let actual = self.phase;
            self.refuse(NicError::Phase(actual))
        }
    }

    pub fn fail(&mut self) {
        self.phase = NicPhase::Failed;
    }

    pub fn configured(&mut self) -> Result<(), NicError> {
        self.require(NicPhase::Idle)?;
        self.phase = NicPhase::Running;
        self.counts.configures += 1;
        Ok(())
    }

    pub fn armed(&mut self, lane: NicLane) -> Result<(), NicError> {
        self.require(NicPhase::Running)?;
        if self.pending[lane as usize] {
            return self.refuse(NicError::Pending(lane));
        }
        self.pending[lane as usize] = true;
        self.counts.arms += 1;
        Ok(())
    }

    /// The lane's transfer completed and its owner validated and reclaimed it.
    pub fn completed(&mut self, lane: NicLane) -> Result<(), NicError> {
        self.end(lane)?;
        self.counts.completions += 1;
        Ok(())
    }

    /// The lane's transfer was retired by a verified stop and repoint.
    pub fn cancelled(&mut self, lane: NicLane) -> Result<(), NicError> {
        self.end(lane)?;
        self.counts.cancels += 1;
        Ok(())
    }

    fn end(&mut self, lane: NicLane) -> Result<(), NicError> {
        self.require(NicPhase::Running)?;
        if !self.pending[lane as usize] {
            return self.refuse(NicError::NotPending(lane));
        }
        self.pending[lane as usize] = false;
        Ok(())
    }

    pub fn stopped(&mut self) -> Result<(), NicError> {
        self.require(NicPhase::Running)?;
        if let Some(lane) = LANES.into_iter().find(|&lane| self.pending[lane as usize]) {
            return self.refuse(NicError::Pending(lane));
        }
        self.phase = NicPhase::Stopped;
        self.counts.stops += 1;
        Ok(())
    }

    /// Deconfigure succeeded and the contexts read back Disabled/Addressed.
    pub fn disabled(&mut self) -> Result<(), NicError> {
        self.require(NicPhase::Stopped)?;
        self.phase = NicPhase::Disabled;
        self.counts.disables += 1;
        Ok(())
    }

    /// The rings were reinitialized; the next start may configure them again.
    pub fn reclaimed(&mut self) -> Result<(), NicError> {
        self.require(NicPhase::Disabled)?;
        self.phase = NicPhase::Idle;
        self.counts.reclaims += 1;
        Ok(())
    }
}

impl Default for NicLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cycle(life: &mut NicLifecycle, rx_completes: bool) {
        life.configured().unwrap();
        life.armed(NicLane::Rx).unwrap();
        life.armed(NicLane::Status).unwrap();
        if rx_completes {
            life.completed(NicLane::Rx).unwrap();
        } else {
            life.cancelled(NicLane::Rx).unwrap();
        }
        life.cancelled(NicLane::Status).unwrap();
        life.stopped().unwrap();
        assert!(!life.may_configure());
        life.disabled().unwrap();
        assert!(!life.may_configure());
        life.reclaimed().unwrap();
        assert!(life.may_configure());
    }

    #[test]
    fn normal_and_stop_racing_completions_never_expose_invalid_lengths() {
        for requested in [8, 32768] {
            for code in 0..=u8::MAX {
                for remaining in [0, 1, requested - 1, requested, requested + 1, u32::MAX] {
                    let actual = completed_length(requested, code, remaining);
                    if matches!(code, 1 | 13) && remaining <= requested {
                        assert_eq!(actual, Some((requested - remaining) as usize));
                    } else {
                        assert_eq!(actual, None);
                    }
                }
            }
        }
    }

    #[test]
    fn repeated_start_stop_disable_reclaim_keeps_exact_counts() {
        let mut life = NicLifecycle::new();
        assert!(life.may_configure());
        for n in 0..100 {
            cycle(&mut life, n % 3 == 0);
        }
        assert_eq!(
            life.counts(),
            NicCounts {
                configures: 100,
                arms: 200,
                completions: 34,
                cancels: 166,
                stops: 100,
                disables: 100,
                reclaims: 100,
            }
        );
        assert_eq!(life.phase(), NicPhase::Idle);
    }

    #[test]
    fn out_of_order_steps_fail_closed_and_stay_failed() {
        type Step = fn(&mut NicLifecycle) -> Result<(), NicError>;
        let steps: [(&str, Step); 7] = [
            ("configured", |l| l.configured()),
            ("arm rx", |l| l.armed(NicLane::Rx)),
            ("complete rx", |l| l.completed(NicLane::Rx)),
            ("cancel rx", |l| l.cancelled(NicLane::Rx)),
            ("stopped", |l| l.stopped()),
            ("disabled", |l| l.disabled()),
            ("reclaimed", |l| l.reclaimed()),
        ];
        // Legal step from each reachable phase; every other step must refuse.
        let setups: [(NicPhase, &[&str], fn(&mut NicLifecycle)); 4] = [
            (NicPhase::Idle, &["configured"], |_| {}),
            (NicPhase::Running, &["arm rx", "stopped"], |l| {
                l.configured().unwrap()
            }),
            (NicPhase::Stopped, &["disabled"], |l| {
                l.configured().unwrap();
                l.stopped().unwrap()
            }),
            (NicPhase::Disabled, &["reclaimed"], |l| {
                l.configured().unwrap();
                l.stopped().unwrap();
                l.disabled().unwrap()
            }),
        ];
        for (phase, legal, setup) in setups {
            for (name, step) in steps {
                let mut life = NicLifecycle::new();
                setup(&mut life);
                assert_eq!(life.phase(), phase);
                let result = step(&mut life);
                if legal.contains(&name) {
                    assert!(result.is_ok(), "{phase:?} {name}");
                } else {
                    assert!(result.is_err(), "{phase:?} {name}");
                    assert_eq!(life.phase(), NicPhase::Failed);
                    assert!(!life.may_configure());
                    for (_, again) in steps {
                        assert_eq!(again(&mut life), Err(NicError::Phase(NicPhase::Failed)));
                    }
                }
            }
        }
    }

    #[test]
    fn a_pending_lane_blocks_stop_and_a_lane_cannot_end_twice() {
        let mut life = NicLifecycle::new();
        life.configured().unwrap();
        life.armed(NicLane::Status).unwrap();
        assert_eq!(
            life.armed(NicLane::Status),
            Err(NicError::Pending(NicLane::Status))
        );
        assert_eq!(life.phase(), NicPhase::Failed);

        let mut life = NicLifecycle::new();
        life.configured().unwrap();
        life.armed(NicLane::Rx).unwrap();
        assert_eq!(life.stopped(), Err(NicError::Pending(NicLane::Rx)));

        let mut life = NicLifecycle::new();
        life.configured().unwrap();
        life.armed(NicLane::Rx).unwrap();
        life.completed(NicLane::Rx).unwrap();
        assert_eq!(
            life.cancelled(NicLane::Rx),
            Err(NicError::NotPending(NicLane::Rx))
        );
        life.fail();
        assert_eq!(life.reclaimed(), Err(NicError::Phase(NicPhase::Failed)));
    }

    #[test]
    fn a_pending_tx_transfer_blocks_the_verified_stop_like_any_lane() {
        let mut life = NicLifecycle::new();
        life.configured().unwrap();
        life.armed(NicLane::Tx).unwrap();
        assert_eq!(life.armed(NicLane::Tx), Err(NicError::Pending(NicLane::Tx)));
        let mut life = NicLifecycle::new();
        life.configured().unwrap();
        life.armed(NicLane::Tx).unwrap();
        assert_eq!(life.stopped(), Err(NicError::Pending(NicLane::Tx)));
        assert_eq!(life.phase(), NicPhase::Failed);
        let mut life = NicLifecycle::new();
        life.configured().unwrap();
        life.armed(NicLane::Tx).unwrap();
        life.completed(NicLane::Tx).unwrap();
        assert!(!life.is_pending(NicLane::Tx));
        life.stopped().unwrap();
    }
}
