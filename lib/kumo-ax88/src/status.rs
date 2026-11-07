//j831
//! Interrupt-endpoint status: exactly eight bytes, two little-endian words. The supplied
//! ax88179_status() reads only the link bit, bit 16 of the first word, and ignores
//! shorter transfers; this parser refuses them. Wire fact only, not a PHY or speed claim.
//! — TIMBERDOODLE

pub const STATUS_LEN: usize = 8;
const LINK: u32 = 1 << 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Length,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptStatus {
    words: [u32; 2],
}

impl InterruptStatus {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let bytes: &[u8; STATUS_LEN] = bytes.try_into().map_err(|_| Error::Length)?;
        Ok(Self {
            words: [
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            ],
        })
    }

    pub const fn link_up(self) -> bool {
        self.words[0] & LINK != 0
    }

    pub const fn words(self) -> [u32; 2] {
        self.words
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_is_bit_sixteen_of_the_first_little_endian_word() {
        let up = InterruptStatus::parse(&[0xa1, 0, 0x01, 0, 0x78, 0x56, 0x34, 0x12]).unwrap();
        assert!(up.link_up());
        assert_eq!(up.words(), [0x0001_00a1, 0x1234_5678]);
        let down = InterruptStatus::parse(&[0xff, 0xff, 0xfe, 0xff, 0, 0, 0x01, 0]).unwrap();
        assert!(!down.link_up());
        // Bit 16 of the second word is not the link.
        let second = InterruptStatus::parse(&[0, 0, 0, 0, 0, 0, 0x01, 0]).unwrap();
        assert!(!second.link_up());
    }

    #[test]
    fn every_other_length_is_refused() {
        let bytes = [0u8; 16];
        for len in (0..=16).filter(|&n| n != STATUS_LEN) {
            assert_eq!(InterruptStatus::parse(&bytes[..len]), Err(Error::Length));
        }
    }
}
