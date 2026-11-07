//j826
extern crate std;
use super::*;
use std::vec::Vec;

// Independently specified wire fixture: two packets (16/19 wire bytes), five
// bytes of packet padding, two dummy words, four arbitrary tail-padding bytes.
const GOLDEN: [u8; 64] = [
    0xa1, 0xa2, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 0xb1, 0xb2, 0x20, 0x21, 0x22, 0x23,
    0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0xee, 0xee, 0xee,
    0xee, 0xee, 0, 0, 16, 0, 0, 0, 0, 0x80, 3, 0, 19, 0, 0, 0, 0, 0x80, 0xab, 0xcd, 0xef, 0x12, 4,
    0, 40, 0,
];

fn fixture(records: &[(usize, u32)], tail_pad: bool) -> Vec<u8> {
    let mut data = Vec::new();
    for &(len, _) in records {
        for i in 0..len {
            data.push((i % 251) as u8);
        }
        while data.len() % 8 != 0 {
            data.push(0xcc);
        }
    }
    let offset = data.len();
    for &(len, flags) in records {
        data.extend_from_slice(&(((len as u32) << 16) | flags).to_le_bytes());
    }
    if tail_pad {
        data.extend_from_slice(&[0xff; 4]);
    }
    data.extend_from_slice(&(((offset as u32) << 16) | records.len() as u32).to_le_bytes());
    data
}

#[test]
fn golden_trailer_table_dummy_alignment_and_checksum_flags() {
    let a = Aggregate::parse(&GOLDEN).unwrap();
    assert_eq!(
        a.counts(),
        Counts {
            frames: 2,
            dropped: 0,
            dummy: 2
        }
    );
    assert_eq!(a.entries(), 4);
    let mut packets = a.packets();
    assert_eq!(packets.len(), 4);
    assert_eq!(
        packets.next(),
        Some(Packet::Frame {
            bytes: &GOLDEN[2..16],
            status: Status(0x0010_0000)
        })
    );
    assert_eq!(packets.next(), Some(Packet::Dummy));
    let Some(Packet::Frame { bytes, status }) = packets.next() else {
        panic!("missing second frame")
    };
    assert_eq!(bytes, &GOLDEN[18..35]);
    assert!(status.ip_checksum_error());
    assert!(status.transport_checksum_error());
    assert_eq!(status.raw(), 0x0013_0003);
    assert_eq!(packets.next(), Some(Packet::Dummy));
    assert_eq!(packets.next(), None);
    assert_eq!(packets.next(), None);
    assert_eq!(packets.len(), 0);
}

#[test]
fn old_firmware_without_dummy_or_optional_tail_padding() {
    for padding in [false, true] {
        let bytes = fixture(&[(62, 0), (63, 0), (64, 0)], padding);
        let a = Aggregate::parse(&bytes).unwrap();
        assert_eq!(
            a.counts(),
            Counts {
                frames: 3,
                dropped: 0,
                dummy: 0
            }
        );
        let lens: Vec<_> = a
            .packets()
            .map(|p| match p {
                Packet::Frame { bytes, .. } => bytes.len(),
                _ => 0,
            })
            .collect();
        assert_eq!(lens, [60, 61, 62]);
    }
}

#[test]
fn crc_drop_runt_oversize_consume_their_spans_and_keep_later_frames() {
    let bytes = fixture(
        &[
            (62, CRC),
            (63, DROP),
            (1, 0),
            (15, 0),
            (1517, 0),
            (1516, 0),
            (0, 0),
        ],
        false,
    );
    let a = Aggregate::parse(&bytes).unwrap();
    assert_eq!(
        a.counts(),
        Counts {
            frames: 1,
            dropped: 5,
            dummy: 1
        }
    );
    let mut p = a.packets();
    for (wire_len, reason) in [
        (62, DropReason::Crc),
        (63, DropReason::Hardware),
        (1, DropReason::Runt),
        (15, DropReason::Runt),
        (1517, DropReason::Oversize),
    ] {
        assert_eq!(p.next(), Some(Packet::Dropped { wire_len, reason }));
    }
    assert!(matches!(p.next(), Some(Packet::Frame { bytes, .. }) if bytes.len()==1514));
    assert_eq!(p.next(), Some(Packet::Dummy));
}

#[test]
fn all_truncations_of_golden_refuse() {
    for end in 0..GOLDEN.len() {
        assert!(Aggregate::parse(&GOLDEN[..end]).is_err(), "prefix {end}");
    }
}

#[test]
fn later_bad_packet_invalidates_aggregate_before_any_frame_is_exposed() {
    let mut bytes = GOLDEN;
    bytes[48..52].copy_from_slice(&0x1fff_0000u32.to_le_bytes());
    assert_eq!(Aggregate::parse(&bytes), Err(Error::PacketBounds));
    for flags in [CRC, DROP, CRC | DROP] {
        bytes[48..52].copy_from_slice(&(0x1fff_0000u32 | flags).to_le_bytes());
        assert_eq!(Aggregate::parse(&bytes), Err(Error::PacketBounds));
    }
}

#[test]
fn metadata_bounds_counts_and_unused_data_are_not_guessed() {
    for (count, offset, error) in [
        (0, 40, Error::Empty),
        (1025, 40, Error::TooManyEntries),
        (65535, 65535, Error::TooManyEntries),
        (4, 65535, Error::MetadataBounds),
        (4, 48, Error::MetadataBounds),
        (1, 40, Error::MetadataTail),
    ] {
        let mut b = GOLDEN;
        b[60..].copy_from_slice(&(((offset as u32) << 16) | count).to_le_bytes());
        assert_eq!(Aggregate::parse(&b), Err(error));
    }
    let mut b = fixture(&[(16, 0)], false);
    // Extend the data region by eight unused bytes, moving table/trailer intact.
    b.splice(16..16, [0u8; 8]);
    b[28..32].copy_from_slice(&0x0018_0001u32.to_le_bytes());
    assert_eq!(Aggregate::parse(&b), Err(Error::DataLength));
}

#[test]
fn exact_aggregate_and_metadata_budgets() {
    let bytes = fixture(
        &[(8191, 0), (8191, 0), (8191, 0), (8152, 0), (16, 0)],
        false,
    );
    assert_eq!(bytes.len(), AGGREGATE_MAX);
    assert_eq!(
        Aggregate::parse(&bytes).unwrap().counts(),
        Counts {
            frames: 1,
            dropped: 4,
            dummy: 0
        }
    );
    let mut too_long = bytes;
    too_long.push(0);
    assert_eq!(Aggregate::parse(&too_long), Err(Error::TooLong));
    let dummy = fixture(&std::vec![(0,DROP);METADATA_MAX], false);
    let a = Aggregate::parse(&dummy).unwrap();
    assert_eq!(a.packets().count(), METADATA_MAX);
    assert_eq!(a.counts().dummy, METADATA_MAX);
    assert_eq!(
        Aggregate::parse(&fixture(&std::vec![(0,0);METADATA_MAX+1], false)),
        Err(Error::TooManyEntries)
    );
}

#[test]
fn every_thirteen_bit_packet_length_is_bounded_and_preserves_exact_payload() {
    for len in 0..8192 {
        let bytes = fixture(&[(len, 0)], false);
        let a = Aggregate::parse(&bytes).unwrap();
        let p = a.packets().next().unwrap();
        match len {
            0 => assert_eq!(p, Packet::Dummy),
            1..=15 => assert_eq!(
                p,
                Packet::Dropped {
                    wire_len: len,
                    reason: DropReason::Runt
                }
            ),
            16..=1516 => {
                let Packet::Frame { bytes: frame, .. } = p else {
                    panic!("len{len}")
                };
                assert_eq!(frame, &bytes[2..len]);
            }
            _ => assert_eq!(
                p,
                Packet::Dropped {
                    wire_len: len,
                    reason: DropReason::Oversize
                }
            ),
        }
    }
}

#[test]
fn every_single_byte_mutation_and_trailer_u16_value_is_panic_free() {
    for at in 0..GOLDEN.len() {
        for value in 0..=255 {
            let mut b = GOLDEN;
            b[at] = value;
            if let Ok(a) = Aggregate::parse(&b) {
                assert_eq!(a.packets().count(), a.entries());
                assert_eq!(
                    a.counts().frames + a.counts().dropped + a.counts().dummy,
                    a.entries()
                );
                for p in a.packets() {
                    if let Packet::Frame { bytes, .. } = p {
                        assert!((FRAME_MIN..=FRAME_MAX).contains(&bytes.len()));
                    }
                }
            }
        }
    }
    for n in 0..=u16::MAX {
        for at in [60, 62] {
            let mut b = GOLDEN;
            b[at..at + 2].copy_from_slice(&n.to_le_bytes());
            if let Ok(a) = Aggregate::parse(&b) {
                assert_eq!(a.packets().count(), a.entries());
            }
        }
    }
}
