//j827
use super::*;

fn request(direction_in: bool, len: u16, payload: &[u8]) -> Result<ControlData<'_>, ControlError> {
    let [lo, hi] = len.to_le_bytes();
    ControlData::new(
        [
            if direction_in { 0xc0 } else { 0x40 },
            1,
            0x34,
            0,
            1,
            0,
            lo,
            hi,
        ],
        0x8000,
        256,
        payload,
    )
}
fn event(token: u64, code: u8, remaining: u32) -> Event {
    Event::Transfer {
        trb_iova: token,
        remaining,
        completion_code: code,
        endpoint_id: 1,
        slot_id: 5,
    }
}
fn active() -> (ControlPipe, TransferRing, [Trb; 16]) {
    let mut memory = [Trb::zero(); 16];
    let mut ring = TransferRing::new(&mut memory, 0x4000).unwrap();
    let mut pipe = ControlPipe::new(5).unwrap();
    pipe.submit(
        &request(true, 1, &[]).unwrap(),
        &mut ring,
        &mut memory,
        100,
        1000,
    )
    .unwrap();
    (pipe, ring, memory)
}

#[test]
fn literal_in_out_setup_data_status_and_payload() {
    let input = request(true, 1, &[]).unwrap();
    assert_eq!(
        input.trbs().map(Trb::words),
        [
            [0x0034_01c0, 0x0001_0001, 8, 0x0003_0840],
            [0x8000, 0, 1, 0x0001_0c24],
            [0, 0, 0, 0x1020],
        ]
    );
    let data = [0x80, 0x02];
    let output = request(false, 2, &data).unwrap();
    assert_eq!(
        output.trbs().map(Trb::words),
        [
            [0x0034_0140, 0x0002_0001, 8, 0x0002_0840],
            [0x8000, 0, 2, 0x0c24],
            [0, 0, 0, 0x0001_1020],
        ]
    );
    assert_eq!(output.outbound(), [0x80, 2]);
    assert_eq!(output.length(), 2);
    assert!(!output.direction_in());
    assert!(input.direction_in());
    assert_eq!(input.outbound(), []);
}

#[test]
fn every_setup_length_payload_bounds_and_iova_boundaries() {
    let payload = [0xa5; 256];
    for len in 0..=u16::MAX {
        let ok = (1..=256).contains(&len);
        assert_eq!(request(true, len, &[]).is_ok(), ok);
        assert_eq!(
            request(false, len, &payload[..usize::from(len.min(256))]).is_ok(),
            ok
        );
    }
    assert_eq!(request(true, 2, &[0, 0]), Err(ControlError::Payload));
    for len in [0, 1, 3, 256] {
        assert_eq!(
            request(false, 2, &payload[..len]),
            Err(ControlError::Payload)
        );
    }
    let setup = [0x80, 6, 0, 1, 0, 0, 2, 0];
    assert_eq!(
        ControlData::new(setup, 0x8000, 1, &[]),
        Err(ControlError::Length)
    );
    assert_eq!(
        ControlData::new(setup, 0, 256, &[]),
        Err(ControlError::Address)
    );
    assert_eq!(
        ControlData::new(setup, u64::MAX, 256, &[]),
        Err(ControlError::Address)
    );
    assert_eq!(
        ControlData::new(setup, 0xffff, 256, &[]),
        Err(ControlError::Ring(Error::BufferCrosses64K))
    );
    assert!(ControlData::new(setup, 0xfffe, 256, &[]).is_ok());
}

#[test]
fn all_preflight_refusals_leave_ring_memory_unchanged() {
    assert!(matches!(ControlPipe::new(0), Err(ControlError::Slot)));
    let mut small = [Trb::zero(); 3];
    let mut ring = TransferRing::new(&mut small, 0x4000).unwrap();
    let before = small;
    let mut pipe = ControlPipe::new(5).unwrap();
    let req = request(true, 1, &[]).unwrap();
    assert_eq!(
        pipe.submit(&req, &mut ring, &mut small, 0, 10),
        Err(ControlError::Ring(Error::RingFull))
    );
    assert_eq!(small, before);
    assert_eq!(ring.queued(), 0);
    assert!(pipe.is_idle());
    let mut memory = [Trb::zero(); 16];
    let mut ring = TransferRing::new(&mut memory, 0x4000).unwrap();
    for (now, budget) in [(0, 0), (u64::MAX, 1), (u64::MAX - 1, 2)] {
        let before = memory;
        assert_eq!(
            pipe.submit(&req, &mut ring, &mut memory, now, budget),
            Err(ControlError::Deadline)
        );
        assert_eq!(memory, before);
        assert_eq!(ring.queued(), 0);
    }
    assert_eq!(
        pipe.submit(&req, &mut ring, &mut small, 0, 1),
        Err(ControlError::Ring(Error::RingMemoryLength))
    );
    ring.enqueue(&mut memory, req.trbs()[0]).unwrap();
    let before = memory;
    assert_eq!(
        pipe.submit(&req, &mut ring, &mut memory, 0, 10),
        Err(ControlError::Busy)
    );
    assert_eq!(memory, before);
}

#[test]
fn data_then_status_is_the_only_success_and_reclaim_point() {
    let (mut pipe, mut ring, mut memory) = active();
    let before = memory;
    assert_eq!(ring.queued(), 3);
    assert_eq!(
        pipe.observe(event(0x4010, 1, 0), 101, &mut ring),
        Ok(ControlProgress::Waiting)
    );
    assert_eq!(ring.queued(), 3);
    assert_eq!(
        pipe.submit(
            &request(true, 1, &[]).unwrap(),
            &mut ring,
            &mut memory,
            102,
            10
        ),
        Err(ControlError::Busy)
    );
    assert_eq!(memory, before);
    assert_eq!(
        pipe.observe(event(0x4020, 1, 0), 103, &mut ring),
        Ok(ControlProgress::Complete(1))
    );
    assert_eq!(ring.queued(), 0);
    assert!(pipe.is_idle());
}

#[test]
fn foreign_slot_endpoint_or_nontransfer_keeps_ownership_with_its_caller() {
    let (mut pipe, mut ring, _) = active();
    for e in [
        Event::Transfer {
            slot_id: 4,
            endpoint_id: 1,
            trb_iova: 0x4010,
            completion_code: 1,
            remaining: 0,
        },
        Event::Transfer {
            slot_id: 5,
            endpoint_id: 2,
            trb_iova: 0x4010,
            completion_code: 1,
            remaining: 0,
        },
        Event::PortStatusChange {
            port_id: 1,
            completion_code: 1,
        },
    ] {
        assert_eq!(
            pipe.observe(e, 101, &mut ring),
            Ok(ControlProgress::Unrelated(e))
        );
        assert_eq!(ring.queued(), 3);
        assert!(!pipe.is_quarantined());
    }
    assert_eq!(
        pipe.observe(event(0x4010, 1, 0), 102, &mut ring),
        Ok(ControlProgress::Waiting)
    );
    assert_eq!(
        pipe.observe(event(0x4020, 1, 0), 103, &mut ring),
        Ok(ControlProgress::Complete(1))
    );
}

#[test]
fn stall_error_short_bad_residue_unknown_pointer_and_reordered_events_quarantine() {
    for token in [0x4000, 0x4010, 0x4020] {
        for cc in 0..=u8::MAX {
            if cc == 1 {
                continue;
            }
            let (mut pipe, mut ring, mut memory) = active();
            assert_eq!(
                pipe.observe(event(token, cc, 0), 101, &mut ring),
                Err(ControlError::Completion(cc))
            );
            assert!(pipe.is_quarantined());
            assert_eq!(ring.queued(), 3);
            assert_eq!(
                pipe.submit(
                    &request(true, 1, &[]).unwrap(),
                    &mut ring,
                    &mut memory,
                    102,
                    1000
                ),
                Err(ControlError::Quarantined)
            );
        }
    }
    for residue in [1, 2, 256, 257, 0x00ff_ffff, u32::MAX] {
        for token in [0x4010, 0x4020] {
            let (mut pipe, mut ring, _) = active();
            assert_eq!(
                pipe.observe(event(token, 1, residue), 101, &mut ring),
                Err(ControlError::Residual(residue))
            );
            assert_eq!(ring.queued(), 3);
            assert!(pipe.is_quarantined());
        }
    }
    for (token, error) in [
        (0x4000, ControlError::Order),
        (0x4020, ControlError::Order),
        (0x4030, ControlError::Unexpected),
    ] {
        let (mut pipe, mut ring, _) = active();
        assert_eq!(pipe.observe(event(token, 1, 0), 101, &mut ring), Err(error));
        assert!(pipe.is_quarantined());
        assert_eq!(ring.queued(), 3);
    }
    let (mut pipe, mut ring, _) = active();
    pipe.observe(event(0x4010, 1, 0), 101, &mut ring).unwrap();
    assert_eq!(
        pipe.observe(event(0x4010, 1, 0), 102, &mut ring),
        Err(ControlError::Order)
    );
    assert_eq!(ring.queued(), 3);
}

#[test]
fn deadline_before_or_after_data_retains_dma_and_rejects_late_completion() {
    for have_data in [false, true] {
        let (mut pipe, mut ring, mut memory) = active();
        if have_data {
            pipe.observe(event(0x4010, 1, 0), 101, &mut ring).unwrap();
        }
        assert_eq!(pipe.check_deadline(1099), Ok(()));
        assert_eq!(pipe.check_deadline(1100), Err(ControlError::Timeout));
        assert_eq!(
            pipe.observe(event(0x4020, 1, 0), 1101, &mut ring),
            Err(ControlError::Quarantined)
        );
        assert_eq!(ring.queued(), 3);
        assert_eq!(
            pipe.submit(
                &request(true, 1, &[]).unwrap(),
                &mut ring,
                &mut memory,
                1102,
                100
            ),
            Err(ControlError::Quarantined)
        );
    }
    let (mut pipe, mut ring, _) = active();
    assert_eq!(
        pipe.observe(event(0x4010, 1, 0), 1100, &mut ring),
        Err(ControlError::Timeout)
    );
    assert_eq!(ring.queued(), 3);
}

#[test]
fn repeated_in_out_transfers_wrap_ring_without_leaking_entries_or_payloads() {
    let mut memory = [Trb::zero(); 8];
    let mut ring = TransferRing::new(&mut memory, 0x4000).unwrap();
    let mut pipe = ControlPipe::new(5).unwrap();
    let mut cycles = 0;
    let mut last_cycle = ring.producer_cycle();
    for turn in 0..1000 {
        let bytes = [turn as u8, (turn >> 8) as u8];
        let req = request(turn % 2 == 0, 2, if turn % 2 == 0 { &[] } else { &bytes }).unwrap();
        pipe.submit(&req, &mut ring, &mut memory, 0, 10).unwrap();
        let p = pipe.pending.unwrap();
        assert_eq!(
            pipe.observe(event(p.tokens[1], 1, 0), 1, &mut ring),
            Ok(ControlProgress::Waiting)
        );
        assert_eq!(
            pipe.observe(event(p.tokens[2], 1, 0), 2, &mut ring),
            Ok(ControlProgress::Complete(2))
        );
        assert_eq!(ring.queued(), 0);
        assert!(pipe.is_idle());
        if last_cycle != ring.producer_cycle() {
            cycles += 1;
            last_cycle = ring.producer_cycle();
        }
    }
    assert!(cycles > 400);
}
