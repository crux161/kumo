//j828
use super::*;
use crate::{NormalTransfer, TransferRing, Trb};

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
            faults: 0
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
