//j829
use super::*;
fn memory(sequence: u32) -> DmaMemory {
    DmaMemory {
        sequence,
        bytes: 4096,
        physical: 0x200000 + u64::from(sequence) * 4096,
        controller: 0xfc400000,
    }
}
fn ledger() -> DmaCustodyLedger {
    let mut l = DmaCustodyLedger::new();
    l.configure(0xfc400000).unwrap();
    l
}

#[test]
fn literal_memory_and_ack_wire_and_every_truncation() {
    let m = DmaMemory {
        sequence: 1,
        bytes: 4096,
        physical: 0x200000,
        controller: 0xfc400000,
    };
    let bytes = [
        0x58, 0x44, 0x4d, 0x31, 1, 0, 0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0, 0, 0,
        0, 0, 0x40, 0xfc, 0, 0, 0, 0,
    ];
    assert_eq!(m.encode(), bytes);
    assert_eq!(DmaMemory::decode(&bytes), Ok(m));
    for n in 0..bytes.len() {
        assert_eq!(DmaMemory::decode(&bytes[..n]), Err(CustodyError::Frame));
    }
    assert!(DmaMemory::decode(&[0; 33]).is_err());
    let ack = *b"XDA1\x01\0\0\0";
    assert_eq!(m.acknowledge(&ack), Ok(m));
    for n in 0..8 {
        assert_eq!(m.acknowledge(&ack[..n]), Err(CustodyError::Ack));
    }
    for bad in [
        &b"XDN1\x01\0\0\0"[..],
        &b"XDA1\x02\0\0\0"[..],
        &b"XDA1\0\0\0\0"[..],
        &[0; 9][..],
    ] {
        assert_eq!(m.acknowledge(bad), Err(CustodyError::Ack));
    }
}

#[test]
fn parent_stores_alias_before_ack_and_retains_it_after_child_or_ack_failure() {
    let mut l = ledger();
    let m = memory(1);
    let ack = l.accept(m, 77).unwrap();
    assert_eq!(l.count(), 1);
    assert_eq!(l.bytes(), 4096);
    assert!(l.contains_handle(77));
    assert_eq!(m.acknowledge(&ack), Ok(m));
    // Dropping/losing the ACK or child does not call a release operation on this ledger.
    assert_eq!(l.accept(memory(2), 0), Err(CustodyError::Handle));
    assert_eq!(l.accept(memory(2), 77), Err(CustodyError::Handle));
    assert_eq!(l.count(), 1);
    assert!(l.contains_handle(77));
}

#[test]
fn wrong_controller_sequence_overlap_and_budget_never_replace_accepted_aliases() {
    let mut l = ledger();
    l.accept(memory(1), 77).unwrap();
    let mut m = memory(2);
    m.controller = 0xfc000000;
    assert_eq!(l.accept(m, 78), Err(CustodyError::Controller));
    assert_eq!(l.accept(memory(1), 78), Err(CustodyError::Sequence));
    assert_eq!(l.accept(memory(3), 78), Err(CustodyError::Sequence));
    let mut m = memory(2);
    m.physical = memory(1).physical;
    assert_eq!(l.accept(m, 78), Err(CustodyError::Overlap));
    for n in 2..=DMA_CUSTODY_MAX as u32 {
        l.accept(memory(n), 76 + n).unwrap();
    }
    assert_eq!(l.accept(memory(33), 999), Err(CustodyError::Capacity));
    assert_eq!(l.count(), 32);
    assert!(l.contains_handle(77));
    assert_eq!(l.configure(0xfc400000), Err(CustodyError::Controller));
}

#[test]
fn malformed_physical_spans_and_byte_budgets_are_rejected() {
    for bytes in [0, 1, 4095, 4097, DMA_ALLOCATION_MAX + 4096, u64::MAX] {
        let mut m = memory(1);
        m.bytes = bytes;
        assert!(m.validate().is_err());
    }
    for physical in [0, 1, 4095, u64::MAX, u64::MAX - 4095] {
        let mut m = memory(1);
        m.physical = physical;
        assert!(m.validate().is_err());
    }
    let mut l = ledger();
    for n in 1..=4 {
        let mut m = memory(n);
        m.physical = u64::from(n) * DMA_ALLOCATION_MAX;
        m.bytes = DMA_ALLOCATION_MAX;
        l.accept(m, n).unwrap();
    }
    assert_eq!(l.bytes(), DMA_TOTAL_MAX);
    let mut m = memory(5);
    m.physical = 0x2000000;
    assert_eq!(l.accept(m, 5), Err(CustodyError::Capacity));
    assert_eq!(l.count(), 4);
}

#[test]
fn no_publication_without_exact_ack_for_that_allocation() {
    let m = memory(1);
    let mut publications = 0;
    for ack in [&[][..], &b"XDN1\x01\0\0\0"[..], &b"XDA1\x02\0\0\0"[..]] {
        if m.acknowledge(ack).is_ok() {
            publications += 1;
        }
    }
    assert_eq!(publications, 0);
    let mut l = ledger();
    let ack = l.accept(m, 99).unwrap();
    if m.acknowledge(&ack).is_ok() {
        publications += 1;
    }
    assert_eq!(publications, 1);
    assert!(l.contains_handle(99));
}
