//j826
extern crate std;
use super::*;

#[test]
fn golden_normal_and_exact_max_packet_prefixes() {
    for (len, mps, expected) in [
        (14, 512, [14, 0, 0, 0, 0, 0, 0, 0]),
        (56, 64, [56, 0, 0, 0, 0, 0x80, 0, 0x80]),
        (504, 512, [0xf8, 1, 0, 0, 0, 0x80, 0, 0x80]),
        (1016, 1024, [0xf8, 3, 0, 0, 0, 0x80, 0, 0x80]),
        (1514, 512, [0xea, 5, 0, 0, 0, 0, 0, 0]),
    ] {
        let frame = std::vec![0x5a;len];
        let mut out = std::vec![0xcc;len+8+4];
        assert_eq!(encode(&frame, mps, &mut out), Ok(len + 8));
        assert_eq!(out[..8], expected);
        assert_eq!(out[8..len + 8], frame);
        assert_eq!(out[len + 8..], [0xcc; 4]);
    }
}

#[test]
fn lengths_around_each_usb_boundary_do_not_add_bytes_or_tso() {
    for mps in [64, 512, 1024] {
        for multiple in 1..=24 {
            let boundary = usize::from(mps) * multiple;
            for total in [boundary - 1, boundary, boundary + 1] {
                if total < 8 || !(FRAME_MIN..=FRAME_MAX).contains(&(total - 8)) {
                    continue;
                }
                let h = Header::new(total - 8, mps).unwrap();
                let prefix = h.encode();
                assert_eq!(h.transfer_len(), total);
                assert_eq!(
                    u32::from_le_bytes(prefix[..4].try_into().unwrap()) as usize,
                    total - 8
                );
                assert_eq!(
                    &prefix[4..],
                    if total == boundary {
                        &[0, 0x80, 0, 0x80]
                    } else {
                        &[0, 0, 0, 0]
                    }
                );
            }
        }
    }
}

#[test]
fn every_frame_length_round_trips_exact_bytes_at_each_supported_speed() {
    let frame: [u8; FRAME_MAX] = core::array::from_fn(|n| (n % 251) as u8);
    let mut out = [0xcc; FRAME_MAX + 8 + 8];
    for mps in [64, 512, 1024] {
        for len in FRAME_MIN..=FRAME_MAX {
            out.fill(0xcc);
            let n = encode(&frame[..len], mps, &mut out).unwrap();
            assert_eq!(n, len + 8);
            assert_eq!(&out[8..n], &frame[..len]);
            assert!(out[n..].iter().all(|b| *b == 0xcc));
        }
    }
}

#[test]
fn invalid_lengths_max_packets_and_all_short_outputs_are_unchanged() {
    let mut out = [0x55; 1600];
    for len in [0, 1, 13, 1515, u16::MAX as usize, usize::MAX] {
        assert_eq!(Header::new(len, 512), Err(Error::FrameLength));
    }
    for mps in [0, 1, 8, 16, 32, 63, 65, 511, 513, 1023, 1025, u16::MAX] {
        assert_eq!(encode(&[0; 14], mps, &mut out), Err(Error::MaxPacket));
        assert_eq!(out, [0x55; 1600]);
    }
    for len in [14, 504, 1016, 1514] {
        let frame = std::vec![0;len];
        for cap in 0..len + 8 {
            assert_eq!(
                encode(&frame, 512, &mut out[..cap]),
                Err(Error::OutputShort)
            );
            assert_eq!(out, [0x55; 1600]);
        }
    }
}
