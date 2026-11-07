//j830
use super::*;
use crate::{
    CompletionKey, CompletionRouter, Event, NormalTransfer, RouteError, TransferRing, Trb, TrbType,
};

const RESTART: RingRestart = RingRestart {
    dequeue_iova: 0x1010,
    cycle: true,
    link_iova: None,
};

fn stopped(code: u8, ptr: u64, remaining: u32) -> RoutedTransfer {
    RoutedTransfer {
        key: CompletionKey {
            slot: 5,
            dci: 3,
            trb_iova: ptr,
            generation: 1,
        },
        completion_code: code,
        remaining,
    }
}

fn context(state: EndpointState, dequeue_iova: u64, dequeue_cycle: bool) -> EndpointSnapshot {
    EndpointSnapshot {
        state,
        dequeue_iova,
        dequeue_cycle,
    }
}

#[test]
fn output_endpoint_context_words_decode_state_pointer_and_cycle() {
    let snapshot = EndpointSnapshot::from_words([0x0003_0403, 0x0040_0026, 0x1234_5671, 0x1]);
    assert_eq!(
        snapshot,
        context(EndpointState::Stopped, 0x1_1234_5670, true)
    );
    let states = [0u8, 1, 2, 3, 4, 5, 6, 7].map(EndpointState::from_raw);
    assert_eq!(
        states,
        [
            EndpointState::Disabled,
            EndpointState::Running,
            EndpointState::Halted,
            EndpointState::Stopped,
            EndpointState::Error,
            EndpointState::Reserved(5),
            EndpointState::Reserved(6),
            EndpointState::Reserved(7),
        ]
    );
    assert_eq!(
        EndpointSnapshot::from_words([1, 0, 0x4000, 0]).dequeue_cycle,
        false
    );
}

#[test]
fn interrupted_transfer_is_proven_stopped_then_repointed_past_its_trb() {
    let mut q = EndpointQuiesce::new(RESTART, &[0x1000]).unwrap();
    assert_eq!(
        q.repointed(1, context(EndpointState::Stopped, 0x1010, true)),
        Err(QuiesceError::OutOfOrder)
    );
    let mut q = EndpointQuiesce::new(RESTART, &[0x1000]).unwrap();
    let event = stopped(26, 0x1000, 4);
    let at_stop = context(EndpointState::Stopped, 0x1000, true);
    assert_eq!(
        q.stop_completed(1, Some(event), at_stop),
        Ok(StopVerdict::Repoint {
            dequeue_iova: 0x1010,
            dequeue_cycle: true
        })
    );
    assert_eq!(
        q.stop_completed(1, Some(event), at_stop),
        Err(QuiesceError::OutOfOrder)
    );
    let after = context(EndpointState::Stopped, 0x1010, true);
    assert_eq!(
        q.repointed(1, after),
        Ok(QuiesceReport {
            stop: event,
            stopped: at_stop,
            repointed: after,
            retries: 0,
        })
    );
    assert_eq!(q.repointed(1, after), Err(QuiesceError::OutOfOrder));
}

#[test]
fn force_stopped_event_at_restart_or_link_alias_is_accepted_and_others_are_not() {
    let wrapped = RingRestart {
        dequeue_iova: 0x2000,
        cycle: false,
        link_iova: Some(0x20f0),
    };
    for (ptr, ok) in [
        (0x2000, true),
        (0x20f0, true),
        (0x2010, false),
        (0x1000, false),
    ] {
        let mut q = EndpointQuiesce::new(wrapped, &[]).unwrap();
        let verdict = q.stop_completed(
            1,
            Some(stopped(27, ptr, 0)),
            context(EndpointState::Stopped, ptr, true),
        );
        if ok {
            assert_eq!(
                verdict,
                Ok(StopVerdict::Repoint {
                    dequeue_iova: 0x2000,
                    dequeue_cycle: false
                })
            );
        } else {
            assert_eq!(
                verdict,
                Err(QuiesceError::DequeueMismatch {
                    event: ptr,
                    context: ptr
                })
            );
        }
    }
}

#[test]
fn every_unproven_stop_answer_fails_closed() {
    let event = stopped(26, 0x1000, 4);
    let good = context(EndpointState::Stopped, 0x1000, true);
    for (code, stop, ctx, error) in [
        (1, None, good, QuiesceError::NoStopEvent),
        (
            1,
            Some(stopped(1, 0x1000, 0)),
            good,
            QuiesceError::UnexpectedStopEvent,
        ),
        (
            1,
            Some(event),
            context(EndpointState::Running, 0x1000, true),
            QuiesceError::NotStopped(EndpointState::Running),
        ),
        (
            1,
            Some(event),
            context(EndpointState::Stopped, 0x1010, true),
            QuiesceError::DequeueMismatch {
                event: 0x1000,
                context: 0x1010,
            },
        ),
        (19, Some(event), good, QuiesceError::UnexpectedStopEvent),
        (
            19,
            None,
            context(EndpointState::Halted, 0x1000, true),
            QuiesceError::NeedsReset(EndpointState::Halted),
        ),
        (
            19,
            None,
            context(EndpointState::Error, 0x1000, true),
            QuiesceError::NeedsReset(EndpointState::Error),
        ),
        (19, None, good, QuiesceError::StoppedWithoutEvent),
        (
            19,
            None,
            context(EndpointState::Disabled, 0, false),
            QuiesceError::NotStopped(EndpointState::Disabled),
        ),
        (5, None, good, QuiesceError::StopFailed(5)),
        (11, Some(event), good, QuiesceError::StopFailed(11)),
    ] {
        let mut q = EndpointQuiesce::new(RESTART, &[0x1000]).unwrap();
        assert_eq!(q.stop_completed(code, stop, ctx), Err(error));
        // A failed proof cannot be resumed into a repoint.
        assert_eq!(
            q.repointed(1, context(EndpointState::Stopped, 0x1010, true)),
            Err(QuiesceError::OutOfOrder)
        );
        assert_eq!(
            q.stop_completed(1, Some(event), good),
            Err(QuiesceError::OutOfOrder)
        );
    }
    assert_eq!(
        EndpointQuiesce::new(RESTART, &[0x1000, 0x1010, 0x1020, 0x1030]),
        Err(QuiesceError::Invalid)
    );
}

#[test]
fn context_state_error_while_running_retries_a_bounded_number_of_times() {
    let mut q = EndpointQuiesce::new(RESTART, &[0x1000]).unwrap();
    let running = context(EndpointState::Running, 0x1000, true);
    for _ in 0..STOP_RETRIES {
        assert_eq!(q.stop_completed(19, None, running), Ok(StopVerdict::Retry));
    }
    assert_eq!(q.retries(), STOP_RETRIES);
    assert_eq!(
        q.stop_completed(19, None, running),
        Err(QuiesceError::RetryExhausted)
    );
    let mut q = EndpointQuiesce::new(RESTART, &[0x1000]).unwrap();
    assert_eq!(q.stop_completed(19, None, running), Ok(StopVerdict::Retry));
    let event = stopped(26, 0x1000, 4);
    let at_stop = context(EndpointState::Stopped, 0x1000, true);
    assert!(q.stop_completed(1, Some(event), at_stop).is_ok());
    let report = q
        .repointed(1, context(EndpointState::Stopped, 0x1010, true))
        .unwrap();
    assert_eq!(report.retries, 1);
}

#[test]
fn repoint_is_accepted_only_with_an_exact_stopped_readback() {
    for (code, ctx, error) in [
        (
            19,
            context(EndpointState::Stopped, 0x1010, true),
            QuiesceError::SetDequeueFailed(19),
        ),
        (
            5,
            context(EndpointState::Stopped, 0x1010, true),
            QuiesceError::SetDequeueFailed(5),
        ),
        (
            1,
            context(EndpointState::Running, 0x1010, true),
            QuiesceError::NotStopped(EndpointState::Running),
        ),
        (
            1,
            context(EndpointState::Stopped, 0x1000, true),
            QuiesceError::ReadbackMismatch {
                dequeue_iova: 0x1000,
                dequeue_cycle: true,
            },
        ),
        (
            1,
            context(EndpointState::Stopped, 0x1010, false),
            QuiesceError::ReadbackMismatch {
                dequeue_iova: 0x1010,
                dequeue_cycle: false,
            },
        ),
    ] {
        let mut q = EndpointQuiesce::new(RESTART, &[0x1000]).unwrap();
        q.stop_completed(
            1,
            Some(stopped(26, 0x1000, 4)),
            context(EndpointState::Stopped, 0x1000, true),
        )
        .unwrap();
        assert_eq!(q.repointed(code, ctx), Err(error));
        assert_eq!(
            q.repointed(1, context(EndpointState::Stopped, 0x1010, true)),
            Err(QuiesceError::OutOfOrder)
        );
    }
}

/// A spec-shaped single endpoint: a doorbell starts the TRB at the dequeue pointer if
/// its cycle matches DCS; Stop reports that TRB (Stopped) or the dequeue pointer (Force
/// Stopped Event); Set TR Dequeue is accepted only while Stopped.
struct FakeEndpoint {
    state: EndpointState,
    dequeue: u64,
    dcs: bool,
    in_progress: Option<u64>,
}

impl FakeEndpoint {
    fn doorbell(&mut self, base: u64, memory: &[Trb]) {
        self.state = EndpointState::Running;
        let trb = memory[((self.dequeue - base) / 16) as usize];
        if trb.cycle() == self.dcs && trb.trb_type() == Some(TrbType::Normal) {
            self.in_progress = Some(self.dequeue);
        }
    }
    fn stop(&mut self) -> (Event, u8) {
        self.state = EndpointState::Stopped;
        let (ptr, code, remaining) = match self.in_progress.take() {
            Some(ptr) => (ptr, 26, 4),
            None => (self.dequeue, 27, 0),
        };
        let event = Event::Transfer {
            slot_id: 5,
            endpoint_id: 3,
            trb_iova: ptr,
            completion_code: code,
            remaining,
        };
        (event, 1)
    }
    fn set_dequeue(&mut self, iova: u64, dcs: bool) -> u8 {
        if self.state != EndpointState::Stopped {
            return 19;
        }
        self.dequeue = iova;
        self.dcs = dcs;
        1
    }
    fn context(&self) -> EndpointSnapshot {
        context(self.state, self.dequeue, self.dcs)
    }
}

/// Router fence + quiescence proof + ring abandon, as the driver composes them.
fn quiesce(
    router: &mut CompletionRouter,
    ring: &mut TransferRing,
    hc: &mut FakeEndpoint,
    pending: Option<(crate::TransferTicket, u64)>,
) -> Result<QuiesceReport, RouteError> {
    let restart = ring.restart();
    let (ticket, owned) = match pending {
        Some((ticket, token)) => {
            router.begin_stop(ticket, restart)?;
            (ticket, [token])
        }
        None => (router.register_stop(5, 3, restart)?, [0]),
    };
    let owned: &[u64] = if pending.is_some() { &owned } else { &[] };
    let mut q = EndpointQuiesce::new(restart, owned).unwrap();
    let (event, code) = hc.stop();
    router.route(event)?;
    let StopVerdict::Repoint {
        dequeue_iova,
        dequeue_cycle,
    } = q
        .stop_completed(code, router.stop_event(ticket)?, hc.context())
        .map_err(|_| RouteError::Incomplete)?
    else {
        return Err(RouteError::Busy);
    };
    let code = hc.set_dequeue(dequeue_iova, dequeue_cycle);
    let report = q
        .repointed(code, hc.context())
        .map_err(|_| RouteError::Incomplete)?;
    router.cancel(ticket)?;
    assert_eq!(ring.abandon(restart).unwrap(), owned.len());
    Ok(report)
}

#[test]
fn spec_model_cancels_and_probes_across_ring_wraps_with_a_flat_census() {
    let mut memory = [Trb::zero(); 16];
    let mut ring = TransferRing::new(&mut memory, 0x4000).unwrap();
    let mut router = CompletionRouter::new();
    let mut hc = FakeEndpoint {
        state: EndpointState::Running,
        dequeue: 0x4000,
        dcs: true,
        in_progress: None,
    };
    let trb = Trb::normal_transfer(NormalTransfer::interrupt_in(0x8000, 4)).unwrap();
    let mut wraps = 0;
    for cycle in 0..40 {
        let [token] = ring.preview::<1>().unwrap();
        let ticket = router.register(5, 3, &[token], 1).unwrap();
        ring.enqueue(&mut memory, trb).unwrap();
        wraps += ring.restart().link_iova.is_some() as u32;
        hc.doorbell(0x4000, &memory);
        let report = quiesce(&mut router, &mut ring, &mut hc, Some((ticket, token))).unwrap();
        assert_eq!(
            (report.stop.completion_code, report.stop.key.trb_iova),
            (26, token)
        );
        // Probe: a doorbell with no new TD must not revive the cancelled one.
        hc.doorbell(0x4000, &memory);
        let probe = quiesce(&mut router, &mut ring, &mut hc, None).unwrap();
        assert_eq!(
            (probe.stop.completion_code, probe.stop.key.trb_iova),
            (27, ring.restart().dequeue_iova),
            "cycle {cycle}"
        );
        assert_eq!(ring.queued(), 0);
    }
    let stats = router.stats();
    assert_eq!(
        (
            stats.registered,
            stats.stopped,
            stats.cancelled,
            stats.faults
        ),
        (40, 80, 80, 0)
    );
    assert_eq!(router.occupied(), 1);
    // 40 single-TRB TDs on 15 usable slots: two wraps, so the cycle is back to 1.
    assert_eq!(wraps, 2);
    assert!(ring.producer_cycle());
    assert_eq!(ring.restart().dequeue_iova, 0x4000 + 10 * 16);
}

#[test]
fn skipping_the_repoint_lets_a_probe_find_the_cancelled_trb_and_the_router_refuses_it() {
    let mut memory = [Trb::zero(); 16];
    let mut ring = TransferRing::new(&mut memory, 0x4000).unwrap();
    let mut router = CompletionRouter::new();
    let mut hc = FakeEndpoint {
        state: EndpointState::Running,
        dequeue: 0x4000,
        dcs: true,
        in_progress: None,
    };
    let trb = Trb::normal_transfer(NormalTransfer::interrupt_in(0x8000, 4)).unwrap();
    let [token] = ring.preview::<1>().unwrap();
    let ticket = router.register(5, 3, &[token], 1).unwrap();
    ring.enqueue(&mut memory, trb).unwrap();
    hc.doorbell(0x4000, &memory);
    let restart = ring.restart();
    router.begin_stop(ticket, restart).unwrap();
    let (event, _) = hc.stop();
    router.route(event).unwrap();
    // Wrongly retire without Set TR Dequeue: the endpoint still points at the TD.
    router.cancel(ticket).unwrap();
    ring.abandon(restart).unwrap();
    hc.doorbell(0x4000, &memory);
    let probe = router.register_stop(5, 3, ring.restart()).unwrap();
    let (event, _) = hc.stop();
    assert_eq!(router.route(event), Err(RouteError::Unknown));
    assert_eq!(router.cancel(probe), Err(RouteError::Quarantined));
}
