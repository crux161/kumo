//j830
//! NET4c-2 endpoint quiescence: the command and context half of a cancel. The router
//! (`stash`) owns the event half. A transfer ring may be abandoned only when, in order,
//! Stop Endpoint succeeded after its stopped event arrived; the Output Endpoint Context
//! reads Stopped with its dequeue pointer at that event's TRB; Set TR Dequeue to the
//! ring restart succeeded; and the context reads back Stopped at exactly that pointer
//! and cycle. Every other answer retains the lane and its DMA. A Halted or Error
//! endpoint needs Reset Endpoint, which this slice deliberately does not issue.
//! Source rules: xHCI 1.2 sections 4.6.9, 4.6.10 and table 4-2. — TIMBERDOODLE

use crate::{
    is_stop_completion, RingRestart, RoutedTransfer, COMPLETION_CONTEXT_STATE_ERROR,
    COMPLETION_SUCCESS,
};

/// Context State Error while the context still reads Running means the controller
/// handled Stop before the doorbell started the endpoint; Linux reissues Stop then.
pub const STOP_RETRIES: u8 = 2;

/// Output Endpoint Context EP State (xHCI table 6-8).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointState {
    Disabled,
    Running,
    Halted,
    Stopped,
    Error,
    Reserved(u8),
}

impl EndpointState {
    pub const fn from_raw(raw: u8) -> Self {
        match raw & 7 {
            0 => Self::Disabled,
            1 => Self::Running,
            2 => Self::Halted,
            3 => Self::Stopped,
            4 => Self::Error,
            other => Self::Reserved(other),
        }
    }
}

/// The fields of an Output Endpoint Context that a stop proof reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndpointSnapshot {
    pub state: EndpointState,
    pub dequeue_iova: u64,
    pub dequeue_cycle: bool,
}

impl EndpointSnapshot {
    /// Decode dwords 0 through 3 of an Output Endpoint Context (xHCI 6.2.3).
    pub const fn from_words(words: [u32; 4]) -> Self {
        Self {
            state: EndpointState::from_raw(words[0] as u8),
            dequeue_iova: (words[2] as u64 & !0xf) | ((words[3] as u64) << 32),
            dequeue_cycle: words[2] & 1 != 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuiesceError {
    /// More owned TRBs than one registered transfer can hold.
    Invalid,
    /// Called out of the Stop, Repoint, Done order.
    OutOfOrder,
    /// Stop Endpoint completed with a code other than Success or Context State Error.
    StopFailed(u8),
    /// Context State Error and the endpoint is Halted or Error: Reset Endpoint needed.
    NeedsReset(EndpointState),
    /// Context State Error repeated with the endpoint still Running.
    RetryExhausted,
    /// Context State Error with a Stopped context and no stopped event: nothing this
    /// slice issued explains it, so the lane is retained.
    StoppedWithoutEvent,
    /// Stop Endpoint succeeded but no stopped event preceded its completion.
    NoStopEvent,
    /// A stopped event accompanied a failed command, or carries a non-stop code.
    UnexpectedStopEvent,
    /// The context does not read Stopped.
    NotStopped(EndpointState),
    /// The stopped context does not point at the stopped event's TRB, or that TRB is
    /// neither owned nor the restart position.
    DequeueMismatch { event: u64, context: u64 },
    /// Set TR Dequeue completed with a non-Success code.
    SetDequeueFailed(u8),
    /// After Set TR Dequeue the context does not hold exactly the restart pointer/cycle.
    ReadbackMismatch {
        dequeue_iova: u64,
        dequeue_cycle: bool,
    },
}

/// What the owner must do after a Stop Endpoint command completes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopVerdict {
    /// Issue Stop Endpoint again; the endpoint had not started yet.
    Retry,
    /// Issue Set TR Dequeue with exactly these fields.
    Repoint {
        dequeue_iova: u64,
        dequeue_cycle: bool,
    },
}

/// Evidence of one completed quiescence, for the owner's record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuiesceReport {
    pub stop: RoutedTransfer,
    pub stopped: EndpointSnapshot,
    pub repointed: EndpointSnapshot,
    pub retries: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Stopping,
    Repointing {
        stop: RoutedTransfer,
        stopped: EndpointSnapshot,
    },
    Done,
    Failed,
}

/// One endpoint's stop/repoint proof. The owner issues the commands and reads the
/// context; this decides whether the next step is permitted. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndpointQuiesce {
    restart: RingRestart,
    owned: [u64; 3],
    count: usize,
    retries: u8,
    phase: Phase,
}

impl EndpointQuiesce {
    /// `owned` lists the TRBs of the transfer being cancelled; empty for an idle stop.
    pub fn new(restart: RingRestart, owned: &[u64]) -> Result<Self, QuiesceError> {
        if owned.len() > 3 {
            return Err(QuiesceError::Invalid);
        }
        let mut tokens = [0; 3];
        tokens[..owned.len()].copy_from_slice(owned);
        Ok(Self {
            restart,
            owned: tokens,
            count: owned.len(),
            retries: 0,
            phase: Phase::Stopping,
        })
    }

    pub const fn retries(&self) -> u8 {
        self.retries
    }

    fn names(&self, iova: u64) -> bool {
        self.owned[..self.count].contains(&iova) || self.restart.names(iova)
    }

    /// Judge a Stop Endpoint completion. `stop` is the router's accepted stopped event
    /// and `context` the Output Endpoint Context read after the completion.
    pub fn stop_completed(
        &mut self,
        completion_code: u8,
        stop: Option<RoutedTransfer>,
        context: EndpointSnapshot,
    ) -> Result<StopVerdict, QuiesceError> {
        if self.phase != Phase::Stopping {
            return Err(QuiesceError::OutOfOrder);
        }
        let verdict = self.judge_stop(completion_code, stop, context);
        if verdict.is_err() {
            self.phase = Phase::Failed;
        }
        verdict
    }

    fn judge_stop(
        &mut self,
        completion_code: u8,
        stop: Option<RoutedTransfer>,
        context: EndpointSnapshot,
    ) -> Result<StopVerdict, QuiesceError> {
        match completion_code {
            COMPLETION_SUCCESS => {
                let event = stop.ok_or(QuiesceError::NoStopEvent)?;
                if !is_stop_completion(event.completion_code) {
                    return Err(QuiesceError::UnexpectedStopEvent);
                }
                if context.state != EndpointState::Stopped {
                    return Err(QuiesceError::NotStopped(context.state));
                }
                let pointer = event.key.trb_iova;
                if context.dequeue_iova != pointer || !self.names(pointer) {
                    return Err(QuiesceError::DequeueMismatch {
                        event: pointer,
                        context: context.dequeue_iova,
                    });
                }
                self.phase = Phase::Repointing {
                    stop: event,
                    stopped: context,
                };
                Ok(StopVerdict::Repoint {
                    dequeue_iova: self.restart.dequeue_iova,
                    dequeue_cycle: self.restart.cycle,
                })
            }
            COMPLETION_CONTEXT_STATE_ERROR => {
                if stop.is_some() {
                    return Err(QuiesceError::UnexpectedStopEvent);
                }
                match context.state {
                    EndpointState::Running if self.retries < STOP_RETRIES => {
                        self.retries += 1;
                        Ok(StopVerdict::Retry)
                    }
                    EndpointState::Running => Err(QuiesceError::RetryExhausted),
                    EndpointState::Halted | EndpointState::Error => {
                        Err(QuiesceError::NeedsReset(context.state))
                    }
                    EndpointState::Stopped => Err(QuiesceError::StoppedWithoutEvent),
                    other => Err(QuiesceError::NotStopped(other)),
                }
            }
            other => Err(QuiesceError::StopFailed(other)),
        }
    }

    /// Judge a Set TR Dequeue completion and the context read after it. Success is the
    /// only point at which the owner may cancel the router lane and abandon the ring.
    pub fn repointed(
        &mut self,
        completion_code: u8,
        context: EndpointSnapshot,
    ) -> Result<QuiesceReport, QuiesceError> {
        let Phase::Repointing { stop, stopped } = self.phase else {
            return Err(QuiesceError::OutOfOrder);
        };
        self.phase = Phase::Failed;
        if completion_code != COMPLETION_SUCCESS {
            return Err(QuiesceError::SetDequeueFailed(completion_code));
        }
        if context.state != EndpointState::Stopped {
            return Err(QuiesceError::NotStopped(context.state));
        }
        if context.dequeue_iova != self.restart.dequeue_iova
            || context.dequeue_cycle != self.restart.cycle
        {
            return Err(QuiesceError::ReadbackMismatch {
                dequeue_iova: context.dequeue_iova,
                dequeue_cycle: context.dequeue_cycle,
            });
        }
        self.phase = Phase::Done;
        Ok(QuiesceReport {
            stop,
            stopped,
            repointed: context,
            retries: self.retries,
        })
    }
}

#[cfg(test)]
mod tests;
