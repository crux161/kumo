//j824

//! Register-sized synchronous call payloads (AX2).
//!
//! [`Syscall::ChannelCall`](crate::Syscall::ChannelCall) and
//! [`Syscall::ChannelReply`](crate::Syscall::ChannelReply) carry at most [`MAX_CALL_BYTES`]
//! bytes in [`CALL_WORDS`] 64-bit registers. Both ends of a call share this one encoding:
//! little-endian bytes, zero-padded to a whole number of words. A payload longer than the cap
//! is refused, never truncated. Nothing here allocates; callers pass fixed arrays.

/// The most bytes one `ChannelCall` or `ChannelReply` can carry.
pub const MAX_CALL_BYTES: usize = 32;

/// The number of 64-bit words that carry a call payload (aarch64 `x2..=x5`).
pub const CALL_WORDS: usize = 4;

const BYTES_PER_WORD: usize = 8;

/// Pack up to [`MAX_CALL_BYTES`] payload bytes into [`CALL_WORDS`] little-endian, zero-padded
/// words. Returns `None` when `bytes` is longer than 32; an over-long payload is refused, not
/// truncated.
pub fn pack_call_payload(bytes: &[u8]) -> Option<[u64; CALL_WORDS]> {
    if bytes.len() > MAX_CALL_BYTES {
        return None;
    }
    let mut words = [0u64; CALL_WORDS];
    for (i, &byte) in bytes.iter().enumerate() {
        words[i / BYTES_PER_WORD] |= u64::from(byte) << (8 * (i % BYTES_PER_WORD));
    }
    Some(words)
}

/// Unpack exactly `len` payload bytes from [`CALL_WORDS`] little-endian words into `out`,
/// ignoring any bytes past `len` in the words. Returns `Some(len)` on success, or `None` when
/// `len` exceeds [`MAX_CALL_BYTES`] or `out` cannot hold `len` bytes.
pub fn unpack_call_payload(words: [u64; CALL_WORDS], len: usize, out: &mut [u8]) -> Option<usize> {
    if len > MAX_CALL_BYTES || out.len() < len {
        return None;
    }
    for (i, byte) in out.iter_mut().take(len).enumerate() {
        *byte = (words[i / BYTES_PER_WORD] >> (8 * (i % BYTES_PER_WORD))) as u8;
    }
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every position differs, so a misplaced word or a wrong shift cannot round-trip.
    fn pattern() -> [u8; MAX_CALL_BYTES] {
        let mut bytes = [0u8; MAX_CALL_BYTES];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        bytes
    }

    #[test]
    fn every_length_round_trips_exactly() {
        let bytes = pattern();
        for len in 0..=MAX_CALL_BYTES {
            let words = pack_call_payload(&bytes[..len]).expect("len <= 32 packs");
            let mut out = [0xffu8; MAX_CALL_BYTES];
            assert_eq!(unpack_call_payload(words, len, &mut out[..len]), Some(len));
            assert_eq!(&out[..len], &bytes[..len], "length {len}");

            // The encoding is zero-padded: unpacking the whole width reveals the same bytes
            // followed by zeros, and every byte the payload did not name is zero in word form.
            let mut padded = [0xffu8; MAX_CALL_BYTES];
            assert_eq!(
                unpack_call_payload(words, MAX_CALL_BYTES, &mut padded),
                Some(MAX_CALL_BYTES)
            );
            assert_eq!(&padded[..len], &bytes[..len]);
            assert!(padded[len..].iter().all(|&byte| byte == 0));
            for (i, word) in words.iter().enumerate() {
                if i * BYTES_PER_WORD >= len {
                    assert_eq!(*word, 0, "word {i} past the payload must be zero");
                }
            }
            if len % BYTES_PER_WORD != 0 {
                let used = (len % BYTES_PER_WORD) * 8;
                assert_eq!(
                    words[len / BYTES_PER_WORD] >> used,
                    0,
                    "unused high bytes of the last word must be zero"
                );
            }
        }
    }

    #[test]
    fn exact_bytes_land_in_the_expected_registers() {
        // "ABCDEFGHIJ" is ten bytes: two whole words plus a partial one.
        let words = pack_call_payload(b"ABCDEFGHIJ").unwrap();
        assert_eq!(words, [0x4847_4645_4443_4241, 0x0000_0000_0000_4a49, 0, 0]);
        assert_eq!(pack_call_payload(&[]), Some([0u64; CALL_WORDS]));

        let mut out = [0u8; 5];
        assert_eq!(
            unpack_call_payload([0x8877_6655_4433_2211, 0, 0, 0], 5, &mut out),
            Some(5)
        );
        assert_eq!(out, [0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(unpack_call_payload([0; CALL_WORDS], 0, &mut []), Some(0));
    }

    #[test]
    fn thirty_three_bytes_are_refused_in_both_directions() {
        let over = [0u8; MAX_CALL_BYTES + 1];
        assert_eq!(pack_call_payload(&over), None);

        let mut out = [0u8; MAX_CALL_BYTES + 1];
        assert_eq!(
            unpack_call_payload([0; CALL_WORDS], MAX_CALL_BYTES + 1, &mut out),
            None
        );
        // Exactly 32 is still fine.
        assert_eq!(
            unpack_call_payload([0; CALL_WORDS], MAX_CALL_BYTES, &mut out[..MAX_CALL_BYTES]),
            Some(MAX_CALL_BYTES)
        );
    }

    #[test]
    fn bytes_past_len_are_ignored() {
        let words = [u64::MAX; CALL_WORDS];
        let mut out = [0xaau8; MAX_CALL_BYTES];
        assert_eq!(unpack_call_payload(words, 3, &mut out[..3]), Some(3));
        assert_eq!(out[..3], [0xff, 0xff, 0xff]);
        // Byte 3 was never written: only `len` bytes may be touched.
        assert_eq!(out[3], 0xaa);
    }

    #[test]
    fn a_short_out_buffer_is_refused() {
        let mut out = [0u8; 4];
        assert_eq!(unpack_call_payload([0; CALL_WORDS], 5, &mut out), None);
        assert_eq!(unpack_call_payload([0; CALL_WORDS], 4, &mut out), Some(4));
    }
}
