//j834
extern crate std;
use super::*;
use std::vec::Vec;

/// What the peer's kernel puts on the wire for one test datagram (no padding/FCS).
fn frame(seq: u32) -> Vec<u8> {
    let len = payload_len(seq);
    let mut payload = Vec::with_capacity(len);
    payload.extend_from_slice(&MAGIC);
    payload.extend_from_slice(&seq.to_le_bytes());
    payload.extend_from_slice(&(len as u16).to_le_bytes());
    payload.extend_from_slice(&[1, 0]);
    payload.extend((0..len - PAYLOAD_HEADER).map(|i| pattern(seq, i)));
    let total = (IP + UDP + len) as u16;
    let mut f = std::vec![0xff; 6];
    f.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x06, 0x08, 0x00]);
    let mut ip = std::vec![
        0x45, 0, 0, 0, 0x12, 0x34, 0, 0, 64, 17, 0, 0, 192, 168, 0, 106, 192, 168, 0, 255
    ];
    ip[2..4].copy_from_slice(&total.to_be_bytes());
    let c = !fold(ones_sum(&ip, 0));
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    let udp_len = (UDP + len) as u16;
    let mut udp = std::vec![0xc5, 0x8e];
    udp.extend_from_slice(&PORT.to_be_bytes());
    udp.extend_from_slice(&udp_len.to_be_bytes());
    udp.extend_from_slice(&[0, 0]);
    udp.extend_from_slice(&payload);
    let pseudo = ones_sum(&ip[12..20], 17 + u32::from(udp_len));
    let mut c = !fold(ones_sum(&udp, pseudo));
    if c == 0 {
        c = 0xffff;
    }
    udp[6..8].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&ip);
    f.extend_from_slice(&udp);
    f
}

fn test(c: Class) -> TestFrame {
    match c {
        Class::Test(t) => t,
        other => panic!("{other:?}"),
    }
}

#[test]
fn digests_match_their_published_vectors() {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv64(b"a"), 0xaf63_dc4c_8601_ec8c);
}

#[test]
fn every_scheduled_frame_is_60_to_1514_bytes_and_verifies() {
    for seq in (0..EXACT + BURST).chain([END]) {
        let f = frame(seq);
        assert!((60..=1514).contains(&f.len()), "seq {seq} len {}", f.len());
        let t = test(classify(&f));
        assert_eq!(
            (t.seq, t.verdict, t.trailing, t.fcs),
            (seq, Verdict::Valid, 0, false)
        );
        assert_eq!(t.payload_fnv, fnv64(&f[ETH + IP + UDP..]));
    }
    let sizes: Vec<_> = (0..EXACT).map(|s| frame(s).len()).collect();
    assert_eq!(
        (sizes[0], sizes[15], sizes[16], sizes[31]),
        (60, 75, 496, 511)
    );
    assert_eq!(
        (sizes[32], sizes[47], sizes[48], sizes[63]),
        (1008, 1023, 1499, 1514)
    );
}

#[test]
fn padding_and_a_passed_through_fcs_are_measured_not_assumed() {
    let mut f = frame(3);
    f.extend_from_slice(&[0; 3]);
    let t = test(classify(&f));
    assert_eq!((t.verdict, t.trailing, t.fcs), (Verdict::Valid, 3, false));
    let mut g = frame(40);
    g.extend_from_slice(&crc32(&g).to_le_bytes());
    let t = test(classify(&g));
    assert_eq!((t.verdict, t.trailing, t.fcs), (Verdict::Valid, 4, true));
}

#[test]
fn corruption_is_named_by_the_first_failing_check() {
    let base = frame(20);
    let mut ip = base.clone();
    ip[ETH + 8] ^= 1; // TTL: IPv4 header checksum
    assert_eq!(test(classify(&ip)).verdict, Verdict::IpChecksum);
    let mut udp = base.clone();
    let last = udp.len() - 1;
    udp[last] ^= 0x40;
    assert_eq!(test(classify(&udp)).verdict, Verdict::UdpChecksum);
    // Zero UDP checksum means "not computed"; then the payload check speaks.
    let mut unchecked = udp.clone();
    unchecked[ETH + IP + 6..ETH + IP + 8].copy_from_slice(&[0, 0]);
    assert_eq!(test(classify(&unchecked)).verdict, Verdict::Payload);
    let mut length = base.clone();
    length[ETH + IP + UDP + 12] ^= 1;
    length[ETH + IP + 6..ETH + IP + 8].copy_from_slice(&[0, 0]);
    assert_eq!(test(classify(&length)).verdict, Verdict::Length);
}

#[test]
fn other_traffic_and_malformed_test_headers_are_not_test_frames() {
    let mut arp = std::vec![0u8; 42];
    arp[12..14].copy_from_slice(&[0x08, 0x06]);
    assert_eq!(classify(&arp), Class::Arp);
    arp[12..14].copy_from_slice(&[0x86, 0xdd]);
    assert_eq!(classify(&arp), Class::Ipv6);
    arp[12..14].copy_from_slice(&[0x88, 0xcc]);
    assert_eq!(classify(&arp), Class::Other(0x88cc));
    assert_eq!(classify(&arp[..13]), Class::Other(0));
    let base = frame(5);
    let mut port = base.clone();
    port[ETH + IP + 3] ^= 1;
    assert_eq!(classify(&port), Class::Ipv4);
    let mut options = base.clone();
    options[ETH] = 0x46;
    assert_eq!(classify(&options), Class::Ipv4);
    assert_eq!(classify(&base[..base.len() - 1]), Class::Ipv4);
    let mut magic = base.clone();
    magic[ETH + IP + UDP] ^= 1;
    assert_eq!(classify(&magic), Class::Ipv4);
    for cut in 0..base.len() {
        let _ = classify(&base[..cut]);
    }
}

#[test]
fn the_tally_keeps_exact_burst_and_end_apart_and_its_digest_is_order_free() {
    let mut a = Tally::new();
    let mut b = Tally::new();
    let seqs = [0, 63, EXACT, EXACT + 1, EXACT + BURST - 1];
    for &s in &seqs {
        assert!(a.frame(&frame(s)).is_some());
    }
    for &s in seqs.iter().rev() {
        b.frame(&frame(s));
    }
    assert_eq!(a.burst_digest, b.burst_digest);
    assert_eq!((a.exact_received(), a.burst_received), (2, 3));
    assert_eq!(a.burst_bits[0] & 3, 3);
    assert_eq!(a.burst_bits[63] >> 63, 1);
    let want = [EXACT, EXACT + 1, EXACT + BURST - 1]
        .iter()
        .fold(0u64, |d, &s| {
            d.wrapping_add(burst_term(s, fnv64(&frame(s)[42..])))
        });
    assert_eq!(a.burst_digest, want);
    a.frame(&frame(EXACT));
    a.frame(&frame(0));
    a.frame(&frame(END));
    a.frame(&frame(EXACT + BURST));
    let mut bad = frame(7);
    bad[ETH + 8] ^= 1;
    a.frame(&bad);
    assert_eq!(
        (a.duplicates, a.end, a.unexpected_seq, a.invalid),
        (2, 1, 1, 1)
    );
    assert_eq!((a.burst_received, a.no_fcs, a.fcs), (3, 9, 0));
}

#[test]
fn aggregates_count_frames_drops_dummies_and_parse_errors() {
    use crate::rx::{Counts, Error};
    let mut t = Tally::new();
    t.aggregate(
        4000,
        Ok(Counts {
            frames: 3,
            dropped: 1,
            dummy: 4,
        }),
    );
    t.aggregate(
        100,
        Ok(Counts {
            frames: 1,
            dropped: 0,
            dummy: 1,
        }),
    );
    t.aggregate(9, Err(Error::TooShort));
    assert_eq!(
        (t.aggregates, t.frames, t.dropped, t.dummies, t.parse_errors),
        (3, 4, 1, 5, 1)
    );
    assert_eq!(
        (t.max_frames_per_aggregate, t.max_aggregate_bytes),
        (3, 4000)
    );
}

/// The same numbers are pinned in `scripts/ax88-rx-proof.py --self-test`, so the peer's
/// sender, the host checker and this tally agree on every payload byte.
#[test]
fn golden_values_pin_the_peer_payload_format() {
    for (seq, len, payload, term) in [
        (0, 18, 0xf995_58d9_0f5b_6892u64, 0x0b60_e5b2_60f4_8482u64),
        (63, 1472, 0xbcfa_71f7_edd4_756f, 0x9345_4c19_09f0_25e7),
        (64, 179, 0x9f9d_a6a6_9572_256f, 0x8288_d519_6dae_5c88),
        (
            EXACT + BURST - 1,
            1364,
            0xba30_f9e8_f25f_68b7,
            0x09c0_c5ec_cfd3_73a5,
        ),
        (END, 18, 0x96b8_b8e3_8f9b_f3c6, 0x9be6_4cc0_35a0_d789),
    ] {
        let f = frame(seq);
        assert_eq!(payload_len(seq), len);
        assert_eq!(fnv64(&f[42..]), payload);
        assert_eq!(burst_term(seq, payload), term);
    }
    let all = (EXACT..EXACT + BURST).fold(0u64, |d, s| {
        d.wrapping_add(burst_term(s, fnv64(&frame(s)[42..])))
    });
    assert_eq!(all, 0x1be1_d7bb_e710_5c2b);
}
