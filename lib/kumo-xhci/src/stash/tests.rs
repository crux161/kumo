//j828
//j830
use super::*;
use crate::{NormalTransfer, RingRestart, TransferRing, Trb};

fn raw(slot: u8, dci: u8, ptr: u64) -> Event {
    Event::Transfer {
        slot_id: slot,
        endpoint_id: dci,
        trb_iova: ptr,
        completion_code: 1,
        remaining: 0,
    }
}

#[test]
fn simultaneous_nic_rx_tx_status_hid_storage_deliver_once_to_exact_receipts() {
    let mut router = CompletionRouter::new();
    let specs = [
        (5, 5, 0x1000),
        (5, 6, 0x2000),
        (5, 3, 0x3000),
        (4, 3, 0x4000),
        (6, 5, 0x5000),
    ];
    let tickets = specs.map(|(s, d, p)| router.register(s, d, &[p], 1).unwrap());
    for n in [3, 1, 4, 2, 0] {
        let (s, d, p) = specs[n];
        router.route(raw(s, d, p)).unwrap();
    }
    for n in [4, 0, 2, 1, 3] {
        let e = router.take(tickets[n]).unwrap().unwrap();
        let (s, d, p) = specs[n];
        assert_eq!(
            (e.key.slot, e.key.dci, e.key.trb_iova, e.key.generation),
            (s, d, p, tickets[n].generation())
        );
        assert_eq!(router.take(tickets[n]), Ok(None));
        router.finish(tickets[n]).unwrap();
    }
    assert_eq!(
        router.stats(),
        CompletionStats {
            registered: 5,
            routed: 5,
            consumed: 5,
            retired: 5,
            faults: 0,
            stopped: 0,
            cancelled: 0,
        }
    );
}

#[test]
fn wrong_endpoint_never_steals_a_live_ring_and_other_lanes_progress() {
    let mut r = CompletionRouter::new();
    let a = r.register(5, 5, &[0x1000], 1).unwrap();
    let b = r.register(5, 6, &[0x2000], 1).unwrap();
    assert_eq!(r.route(raw(5, 6, 0x1000)), Err(RouteError::Unknown));
    assert_eq!(r.take(b), Err(RouteError::Quarantined));
    r.route(raw(5, 5, 0x1000)).unwrap();
    assert!(r.take(a).unwrap().is_some());
    r.finish(a).unwrap();
    assert_eq!(r.ready(5, 6), Err(RouteError::Quarantined));
}

#[test]
fn generation_receipts_and_deferred_events_never_cross_a_reused_address() {
    let mut r = CompletionRouter::new();
    let old = r.register(5, 5, &[0x1000], 1).unwrap();
    r.route(raw(5, 5, 0x1000)).unwrap();
    let old_event = r.take(old).unwrap().unwrap();
    r.finish(old).unwrap();
    let fresh = r.register(5, 5, &[0x1000], 1).unwrap();
    assert_ne!(old.generation(), fresh.generation());
    assert_eq!(r.take(old), Err(RouteError::Stale));
    assert_eq!(r.route_tagged(old_event), Err(RouteError::Stale));
    assert_eq!(r.fail(old, RouteError::Unknown), Err(RouteError::Stale));
    assert!(!r.has_ready(fresh).unwrap());
    r.route(raw(5, 5, 0x1000)).unwrap();
    assert_eq!(
        r.take(fresh).unwrap().unwrap().key.generation,
        fresh.generation()
    );
    r.finish(fresh).unwrap();
}

#[test]
fn ep0_events_keep_arrival_order_and_cannot_retire_with_unread_or_missing_stage() {
    let mut r = CompletionRouter::new();
    let t = r.register(5, 1, &[0x1000, 0x1010, 0x1020], 6).unwrap();
    r.route(raw(5, 1, 0x1020)).unwrap();
    r.route(raw(5, 1, 0x1010)).unwrap();
    assert_eq!(r.finish(t), Err(RouteError::Incomplete));
    assert_eq!(r.take(t).unwrap().unwrap().key.trb_iova, 0x1020);
    assert_eq!(r.finish(t), Err(RouteError::Incomplete));
    assert_eq!(r.take(t).unwrap().unwrap().key.trb_iova, 0x1010);
    r.finish(t).unwrap();
    // Routing preserves order; the exact-control model separately refuses Status first.
}

#[test]
fn full_or_duplicate_mailbox_quarantines_only_affected_lane_without_eviction() {
    let mut r = CompletionRouter::new();
    let t = r.register(5, 1, &[0x1000, 0x1010, 0x1020], 7).unwrap();
    for ptr in [0x1000, 0x1010, 0x1020] {
        r.route(raw(5, 1, ptr)).unwrap();
    }
    let retained = r.lanes[t.lane].events;
    assert_eq!(r.route(raw(5, 1, 0x1030)), Err(RouteError::Overflow));
    assert_eq!(r.lanes[t.lane].events, retained);
    assert_eq!(r.take(t), Err(RouteError::Quarantined));
    assert_eq!(r.finish(t), Err(RouteError::Quarantined));
    let other = r.register(4, 3, &[0x4000], 1).unwrap();
    r.route(raw(4, 3, 0x4000)).unwrap();
    assert!(r.take(other).unwrap().is_some());
    r.finish(other).unwrap();
    let mut r = CompletionRouter::new();
    let t = r.register(5, 5, &[0x1000], 1).unwrap();
    r.route(raw(5, 5, 0x1000)).unwrap();
    let first = r.take(t).unwrap();
    assert_eq!(r.route(raw(5, 5, 0x1000)), Err(RouteError::Duplicate));
    assert_eq!(r.lanes[t.lane].events[0], first);
}

#[test]
fn all_budget_and_invalid_registration_refusals_are_before_publication() {
    let mut r = CompletionRouter::new();
    for (slot, dci, tokens, mask) in [
        (0, 1, &[0x1000][..], 1),
        (1, 0, &[0x1000], 1),
        (1, 32, &[0x1000], 1),
        (1, 1, &[], 1),
        (1, 1, &[0], 1),
        (1, 1, &[0x1001], 1),
        (1, 1, &[0x1000, 0x1000], 3),
        (1, 1, &[0x1000], 0),
        (1, 1, &[0x1000], 2),
        (1, 1, &[0x1000, 0x1010, 0x1020, 0x1030], 15),
    ] {
        assert_eq!(
            r.register(slot, dci, tokens, mask),
            Err(RouteError::Invalid)
        );
    }
    for slot in 1..=COMPLETION_LANES as u8 {
        r.register(slot, 1, &[0x1000 + u64::from(slot) * 0x100], 1)
            .unwrap();
    }
    assert_eq!(r.register(99, 1, &[0x8000], 1), Err(RouteError::Capacity));
    assert_eq!(r.register(1, 1, &[0x9000], 1), Err(RouteError::Busy));
    let mut r = CompletionRouter::new();
    r.next_generation = u64::MAX;
    assert_eq!(
        r.register(1, 1, &[0x1000], 1),
        Err(RouteError::GenerationExhausted)
    );
    assert_eq!(r.stats().registered, 0);
}

#[test]
fn uncertain_lane_never_reuses_or_overlaps_its_dma_tokens() {
    let mut r = CompletionRouter::new();
    let t = r.register(5, 5, &[0x1000], 1).unwrap();
    assert_eq!(r.register(6, 5, &[0x1000], 1), Err(RouteError::Invalid));
    r.fail(t, RouteError::Incomplete).unwrap();
    assert_eq!(r.register(5, 5, &[0x2000], 1), Err(RouteError::Quarantined));
    assert_eq!(r.register(6, 5, &[0x1000], 1), Err(RouteError::Invalid));
    assert_eq!(r.route(raw(5, 5, 0x1000)), Err(RouteError::Quarantined));
}

#[test]
fn preview_does_not_publish_or_change_cycle_and_matches_enqueues_across_wraps() {
    let mut memory = [Trb::zero(); 8];
    let mut ring = TransferRing::new(&mut memory, 0x4000).unwrap();
    for _ in 0..50 {
        let before = memory;
        let cycle = ring.producer_cycle();
        let predicted = ring.preview::<3>().unwrap();
        assert_eq!(before, memory);
        assert_eq!(ring.producer_cycle(), cycle);
        for ptr in predicted {
            assert_eq!(
                ring.enqueue(
                    &mut memory,
                    Trb::normal_transfer(NormalTransfer::bulk_in(0x8000, 8)).unwrap()
                )
                .unwrap()
                .iova,
                ptr
            );
        }
        assert_eq!(ring.reclaim_through(predicted[2]), Ok(3));
    }
    assert_eq!(ring.preview::<8>(), Err(crate::Error::RingFull));
}

#[test]
fn invalid_hardware_slot_or_endpoint_cannot_overwrite_an_empty_owner() {
    for (slot, dci) in [(0, 0), (0, 1), (1, 0), (1, 32), (255, 255)] {
        let mut r = CompletionRouter::new();
        let t = r.register(5, 5, &[0x1000], 1).unwrap();
        assert_eq!(r.route(raw(slot, dci, 0x1000)), Err(RouteError::Invalid));
        assert_eq!(r.take(t), Err(RouteError::Quarantined));
    }
}

fn coded(slot: u8, dci: u8, ptr: u64, completion_code: u8, remaining: u32) -> Event {
    Event::Transfer {
        slot_id: slot,
        endpoint_id: dci,
        trb_iova: ptr,
        completion_code,
        remaining,
    }
}

const AFTER: RingRestart = RingRestart {
    dequeue_iova: 0x1010,
    cycle: true,
    link_iova: None,
};
const WRAPPED: RingRestart = RingRestart {
    dequeue_iova: 0x2000,
    cycle: false,
    link_iova: Some(0x20f0),
};

#[test]
fn in_progress_stop_event_is_the_lanes_last_word_and_cancel_retires_it() {
    let mut r = CompletionRouter::new();
    let t = r.register(5, 3, &[0x1000], 1).unwrap();
    assert_eq!(r.stop_event(t), Err(RouteError::Invalid));
    assert_eq!(r.cancel(t), Err(RouteError::Invalid));
    r.begin_stop(t, AFTER).unwrap();
    assert_eq!(r.begin_stop(t, AFTER), Err(RouteError::Busy));
    assert_eq!(r.cancel(t), Err(RouteError::Incomplete));
    r.route(coded(5, 3, 0x1000, 26, 4)).unwrap();
    let stop = r.stop_event(t).unwrap().unwrap();
    assert_eq!((stop.completion_code, stop.remaining), (26, 4));
    assert_eq!(stop.key.generation, t.generation());
    // A stopped event is not a completion: nothing to take, finish refuses.
    assert_eq!(r.take(t), Ok(None));
    assert_eq!(r.finish(t), Err(RouteError::Busy));
    r.cancel(t).unwrap();
    assert_eq!(r.take(t), Err(RouteError::Stale));
    assert_eq!(r.cancel(t), Err(RouteError::Stale));
    // The tokens are free again for the next transfer on the same endpoint.
    let next = r.register(5, 3, &[0x1000], 1).unwrap();
    assert_ne!(next.generation(), t.generation());
    let stats = r.stats();
    assert_eq!(
        (
            stats.registered,
            stats.stopped,
            stats.cancelled,
            stats.retired,
            stats.faults
        ),
        (2, 1, 1, 0, 0)
    );
}

#[test]
fn force_stopped_event_may_name_only_the_restart_or_its_link_alias() {
    for (ptr, code, ok) in [
        (0x2000, 27, true),
        (0x20f0, 27, true),
        (0x2010, 27, false),
        (0x2000, 26, false),
        (0x2000, 28, false),
        (0x1000, 27, false),
    ] {
        let mut r = CompletionRouter::new();
        let t = r.register_stop(5, 3, WRAPPED).unwrap();
        let routed = r.route(coded(5, 3, ptr, code, 0));
        if ok {
            routed.unwrap();
            r.cancel(t).unwrap();
            assert_eq!(r.stats().cancelled, 1);
            assert_eq!(r.stats().registered, 0);
        } else {
            assert_eq!(routed, Err(RouteError::Unknown));
            assert_eq!(r.cancel(t), Err(RouteError::Quarantined));
            assert_eq!(r.ready(5, 3), Err(RouteError::Quarantined));
        }
    }
}

#[test]
fn a_completion_racing_the_stop_must_be_consumed_before_cancel() {
    let mut r = CompletionRouter::new();
    let t = r.register(5, 3, &[0x1000], 1).unwrap();
    r.route(coded(5, 3, 0x1000, 1, 0)).unwrap();
    r.begin_stop(t, AFTER).unwrap();
    // The completed TRB cannot also be the stopped one.
    let mut twice = CompletionRouter::new();
    let u = twice.register(5, 3, &[0x1000], 1).unwrap();
    twice.route(coded(5, 3, 0x1000, 1, 0)).unwrap();
    twice.begin_stop(u, AFTER).unwrap();
    assert_eq!(
        twice.route(coded(5, 3, 0x1000, 26, 0)),
        Err(RouteError::Unknown)
    );
    // The real order: completion, then the Force Stopped Event at the restart.
    r.route(coded(5, 3, 0x1010, 27, 0)).unwrap();
    assert_eq!(r.cancel(t), Err(RouteError::Incomplete));
    let done = r.take(t).unwrap().unwrap();
    assert_eq!((done.completion_code, done.key.trb_iova), (1, 0x1000));
    r.cancel(t).unwrap();
}

#[test]
fn any_event_after_the_stop_faults_the_lane_and_blocks_cancel() {
    for late in [
        coded(5, 3, 0x1010, 27, 0),
        coded(5, 3, 0x1000, 1, 0),
        coded(5, 3, 0x1000, 26, 0),
    ] {
        let mut r = CompletionRouter::new();
        let t = r.register(5, 3, &[0x1000], 1).unwrap();
        r.begin_stop(t, AFTER).unwrap();
        r.route(coded(5, 3, 0x1000, 26, 4)).unwrap();
        assert_eq!(r.route(late), Err(RouteError::Duplicate));
        assert_eq!(r.cancel(t), Err(RouteError::Quarantined));
        assert_eq!(r.stats().faults, 1);
    }
}

#[test]
fn stopped_codes_without_a_fence_keep_the_net4b_retention_rules() {
    let mut r = CompletionRouter::new();
    let t = r.register(5, 3, &[0x1000], 1).unwrap();
    assert_eq!(
        r.route(coded(5, 3, 0x1010, 27, 0)),
        Err(RouteError::Unknown)
    );
    assert_eq!(r.cancel(t), Err(RouteError::Quarantined));
    let mut r = CompletionRouter::new();
    let t = r.register(5, 3, &[0x1000], 1).unwrap();
    r.route(coded(5, 3, 0x1000, 26, 4)).unwrap();
    assert!(r.take(t).unwrap().is_some());
    assert_eq!(r.finish(t), Err(RouteError::Quarantined));
    // An idle endpoint with no fence still faults on a stray stopped event.
    let mut r = CompletionRouter::new();
    assert_eq!(
        r.route(coded(6, 3, 0x2000, 27, 0)),
        Err(RouteError::Unknown)
    );
    assert_eq!(r.register_stop(6, 3, WRAPPED), Err(RouteError::Quarantined));
}

#[test]
fn stop_fence_refusals_happen_before_any_command_is_issued() {
    let mut r = CompletionRouter::new();
    for restart in [
        RingRestart {
            dequeue_iova: 0,
            ..AFTER
        },
        RingRestart {
            dequeue_iova: 0x1018,
            ..AFTER
        },
        RingRestart {
            link_iova: Some(0x1008),
            ..AFTER
        },
        RingRestart {
            link_iova: Some(0),
            ..AFTER
        },
    ] {
        assert_eq!(r.register_stop(5, 3, restart), Err(RouteError::Invalid));
        let t = r.register(5, 4, &[0x3000], 1).unwrap();
        assert_eq!(r.begin_stop(t, restart), Err(RouteError::Invalid));
        r.route(coded(5, 4, 0x3000, 1, 0)).unwrap();
        r.take(t).unwrap();
        r.finish(t).unwrap();
    }
    let busy = r.register(5, 3, &[0x1000], 1).unwrap();
    assert_eq!(r.register_stop(5, 3, AFTER), Err(RouteError::Busy));
    r.fail(busy, RouteError::Incomplete).unwrap();
    assert_eq!(r.begin_stop(busy, AFTER), Err(RouteError::Quarantined));
    assert_eq!(r.register_stop(5, 3, AFTER), Err(RouteError::Quarantined));
    for (slot, dci) in [(0, 3), (5, 0), (5, 32)] {
        assert_eq!(r.register_stop(slot, dci, AFTER), Err(RouteError::Invalid));
    }
}

#[test]
fn repeated_stop_and_cancel_cycles_keep_a_flat_lane_census() {
    let mut memory = [Trb::zero(); 16];
    let mut ring = TransferRing::new(&mut memory, 0x4000).unwrap();
    let mut r = CompletionRouter::new();
    let other = r.register(4, 3, &[0x9000], 1).unwrap();
    let occupied = r.occupied() + 1;
    let trb = Trb::normal_transfer(NormalTransfer::interrupt_in(0x8000, 4)).unwrap();
    for cycle in 0..100u64 {
        let [token] = ring.preview::<1>().unwrap();
        let t = r.register(5, 3, &[token], 1).unwrap();
        ring.enqueue(&mut memory, trb).unwrap();
        let restart = ring.restart();
        r.begin_stop(t, restart).unwrap();
        // Alternate the two legal shapes: interrupted TD, or FSE after completion.
        if cycle % 2 == 0 {
            r.route(coded(5, 3, token, 26, 4)).unwrap();
        } else {
            r.route(coded(5, 3, token, 13, 1)).unwrap();
            r.route(coded(5, 3, restart.dequeue_iova, 27, 0)).unwrap();
            r.take(t).unwrap().unwrap();
        }
        r.cancel(t).unwrap();
        assert_eq!(ring.abandon(restart), Ok(1));
        // Probe the repointed ring: an idle stop must name the same restart.
        let probe = r.register_stop(5, 3, ring.restart()).unwrap();
        r.route(coded(5, 3, ring.restart().dequeue_iova, 27, 0))
            .unwrap();
        r.cancel(probe).unwrap();
        assert_eq!((r.occupied(), ring.queued()), (occupied, 0));
    }
    let stats = r.stats();
    assert_eq!(
        (
            stats.registered,
            stats.stopped,
            stats.cancelled,
            stats.faults
        ),
        (101, 200, 200, 0)
    );
    assert_eq!(stats.consumed, 50);
    r.route(coded(4, 3, 0x9000, 1, 0)).unwrap();
    assert!(r.take(other).unwrap().is_some());
    r.finish(other).unwrap();
}
