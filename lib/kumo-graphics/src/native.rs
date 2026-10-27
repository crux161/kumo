//j636
//j638
//j639

//! J0's bounded serial cooperative native-image leases. One READ VMO accompanies an exact
//! request, after the producer observes GPU completion and the driver's output clean.
//! The producer freezes the image until a matching retired reply: Submit acceptance,
//! scanout admission and IRQ delivery alone never permit reuse. — KESTREL

use crate::{word, Surface, XRGB8888};

pub const REQUEST_LEN: usize = 64;
pub const REPLY_LEN: usize = 24;
pub const MAX_VMO_BYTES: u64 = 2 * 1024 * 1024;
pub const IMAGE_IOVA: u64 = 0x6300_0000;
/// The repeated-presentation proof is bounded to triangle, bars/ramp, triangle.
pub const PRESENTATIONS: u32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Image {
    pub sequence: u32,
    pub surface: Surface,
    pub offset: u64,
    pub vmo_bytes: u64,
    /// Observed successful GPU completion, including output cache visibility.
    /// This first cooperative lane carries no kernel fence or hostile-client claim.
    pub gpu_point: u64,
    pub checksum: u32,
}
impl Image {
    /// First join: one unscaled 256x128 linear XRGB image, page-exact in a bounded VMO.
    pub fn valid(self) -> bool {
        (1..=PRESENTATIONS).contains(&self.sequence)
            && self.gpu_point != 0
            && self.surface
                == (Surface {
                    width: 256,
                    height: 128,
                    stride: 256,
                    format: XRGB8888,
                })
            && self.offset % 4096 == 0
            && self.vmo_bytes != 0
            && self.vmo_bytes % 4096 == 0
            && self.vmo_bytes <= MAX_VMO_BYTES
            && self
                .offset
                .checked_add(self.bytes())
                .is_some_and(|end| end <= self.vmo_bytes)
    }
    pub const fn bytes(self) -> u64 {
        256 * 128 * 4
    }
    pub fn encode(self) -> [u8; REQUEST_LEN] {
        let mut out = [0; REQUEST_LEN];
        out[..4].copy_from_slice(b"KNP1");
        for (chunk, value) in out[4..24].chunks_exact_mut(4).zip([
            self.sequence,
            self.surface.width,
            self.surface.height,
            self.surface.stride,
            self.surface.format,
        ]) {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        for (chunk, value) in
            out[24..48]
                .chunks_exact_mut(8)
                .zip([self.offset, self.vmo_bytes, self.gpu_point])
        {
            chunk.copy_from_slice(&value.to_le_bytes());
        }
        out[48..52].copy_from_slice(&self.checksum.to_le_bytes());
        out
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != REQUEST_LEN
            || &bytes[..4] != b"KNP1"
            || bytes[52..].iter().any(|b| *b != 0)
        {
            return None;
        }
        let wide = |n| u64::from_le_bytes(bytes[n..n + 8].try_into().unwrap());
        let image = Self {
            sequence: word(bytes, 4),
            surface: Surface {
                width: word(bytes, 8),
                height: word(bytes, 12),
                stride: word(bytes, 16),
                format: word(bytes, 20),
            },
            offset: wide(24),
            vmo_bytes: wide(32),
            gpu_point: wide(40),
            checksum: word(bytes, 48),
        };
        image.valid().then_some(image)
    }
    pub fn checksum(self, mut read: impl FnMut(u64) -> u32) -> u32 {
        (0..self.bytes() / 4).fold(0x811c9dc5u32, |hash, n| {
            read(n)
                .to_le_bytes()
                .iter()
                .fold(hash, |h, b| (h ^ u32::from(*b)).wrapping_mul(0x01000193))
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reply {
    pub sequence: u32,
    pub gpu_point: u64,
    pub retired: bool,
}
impl Reply {
    /// A presenter that proves no image can still be scanned out may decline the
    /// diagnostic. Cancellation ends the session and NEVER authorizes image reuse. — KESTREL
    pub const fn cancelled() -> Self {
        Self {
            sequence: 0,
            gpu_point: 0,
            retired: false,
        }
    }
    pub fn is_cancelled(self) -> bool {
        self == Self::cancelled()
    }
    pub fn encode(self) -> [u8; REPLY_LEN] {
        let mut out = [0; REPLY_LEN];
        out[..4].copy_from_slice(b"KNR1");
        out[4..8].copy_from_slice(&self.sequence.to_le_bytes());
        out[8..16].copy_from_slice(&self.gpu_point.to_le_bytes());
        out[16..20].copy_from_slice(&u32::from(self.retired).to_le_bytes());
        out
    }
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != REPLY_LEN
            || &bytes[..4] != b"KNR1"
            || bytes[20..].iter().any(|b| *b != 0)
            || word(bytes, 16) > 1
        {
            return None;
        }
        Some(Self {
            sequence: word(bytes, 4),
            gpu_point: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            retired: word(bytes, 16) == 1,
        })
    }
    pub fn releases(self, image: Image) -> bool {
        self.retired && self.sequence == image.sequence && self.gpu_point == image.gpu_point
    }
}

/// One outstanding image, exact next sequence, increasing completed GPU point and a
/// fixed actual VMO identity/span. Call retire only after the presenter's checked
/// hardware replacement/unmap or its matching reply; this is not a kernel fence. — KESTREL
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Lease {
    retired: u32,
    point: u64,
    pending: Option<Image>,
    backing: Option<(u64, u64, u64)>,
}
impl Lease {
    pub fn can_render(&self) -> bool {
        self.pending.is_none() && self.retired < PRESENTATIONS
    }
    pub fn retired(&self) -> u32 {
        self.retired
    }
    pub fn complete(&self) -> bool {
        self.pending.is_none() && self.retired == PRESENTATIONS
    }
    pub fn begin(&mut self, image: Image, vmo_koid: u64) -> bool {
        let backing = (vmo_koid, image.offset, image.vmo_bytes);
        if !self.can_render()
            || !image.valid()
            || vmo_koid == 0
            || vmo_koid == u64::MAX
            || self.retired.checked_add(1) != Some(image.sequence)
            || image.gpu_point <= self.point
            || self.backing.is_some_and(|old| old != backing)
        {
            return false;
        }
        self.backing = Some(backing);
        self.pending = Some(image);
        true
    }
    pub fn retire(&mut self, reply: Reply) -> bool {
        let Some(image) = self.pending else {
            return false;
        };
        if !reply.releases(image) {
            return false;
        }
        self.retired = image.sequence;
        self.point = image.gpu_point;
        self.pending = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image() -> Image {
        Image {
            sequence: 1,
            surface: crate::DEMO,
            offset: 0x10000,
            vmo_bytes: 0x30000,
            gpu_point: 3,
            checksum: 0x8720cbc5,
        }
    }
    #[test]
    fn explicit_cancellation_is_canonical_and_never_retires_an_active_lease() {
        let cancel = Reply::cancelled();
        assert_eq!(Reply::decode(&cancel.encode()), Some(cancel));
        assert!(cancel.is_cancelled());
        let frame = image();
        assert!(!cancel.releases(frame));
        let mut lease = Lease::default();
        assert!(lease.begin(frame, 77));
        assert!(!lease.retire(cancel));
        assert!(!lease.can_render());
        assert!(!lease.complete());
        assert!(!Reply {
            sequence: 1,
            ..cancel
        }
        .is_cancelled());
        assert!(!Reply {
            gpu_point: 1,
            ..cancel
        }
        .is_cancelled());
        assert!(!Reply {
            retired: true,
            ..cancel
        }
        .is_cancelled());
    }

    #[test]
    fn serial_lease_blocks_gpu_reuse_until_exact_retirement_and_rejects_replay() {
        let mut producer = Lease::default();
        let mut presenter = Lease::default();
        for sequence in 1..=PRESENTATIONS {
            assert!(producer.can_render());
            let frame = Image {
                sequence,
                gpu_point: u64::from(sequence) + 3,
                ..image()
            };
            assert!(producer.begin(frame, 77));
            assert!(presenter.begin(frame, 77));
            assert!(!producer.can_render());
            assert!(!presenter.can_render());
            assert!(!producer.begin(frame, 77));
            let reply = Reply {
                sequence,
                gpu_point: frame.gpu_point,
                retired: true,
            };
            for bad in [
                Reply {
                    retired: false,
                    ..reply
                },
                Reply {
                    sequence: sequence + 1,
                    ..reply
                },
                Reply {
                    gpu_point: frame.gpu_point - 1,
                    ..reply
                },
            ] {
                assert!(!producer.retire(bad));
                assert!(!presenter.retire(bad));
                assert!(!producer.can_render());
                assert!(!presenter.can_render());
            }
            assert!(presenter.retire(reply));
            assert!(!producer.can_render()); // physical retirement still needs delivery
            assert!(producer.retire(reply));
            assert_eq!(producer.retired(), sequence);
            assert!(!producer.retire(reply));
            assert!(!presenter.begin(frame, 77));
        }
        assert!(producer.complete() && presenter.complete());
        assert!(!producer.can_render());
    }
    #[test]
    fn serial_lease_rejects_skips_stale_points_and_substituted_backing_without_mutation() {
        let mut lease = Lease::default();
        let first = Image {
            gpu_point: 4,
            ..image()
        };
        for koid in [0, u64::MAX] {
            assert!(!lease.begin(first, koid));
        }
        assert!(!lease.begin(
            Image {
                sequence: 2,
                ..first
            },
            77
        ));
        assert!(lease.begin(first, 77));
        assert!(lease.retire(Reply {
            sequence: 1,
            gpu_point: 4,
            retired: true
        }));
        let second = Image {
            sequence: 2,
            gpu_point: 5,
            ..first
        };
        let before = lease;
        for (bad, koid) in [
            (
                Image {
                    sequence: 3,
                    ..second
                },
                77,
            ),
            (
                Image {
                    gpu_point: 4,
                    ..second
                },
                77,
            ),
            (
                Image {
                    offset: 0,
                    ..second
                },
                77,
            ),
            (
                Image {
                    vmo_bytes: 0x40000,
                    ..second
                },
                77,
            ),
            (second, 78),
        ] {
            assert!(!lease.begin(bad, koid));
            assert_eq!(lease, before);
        }
        assert!(lease.begin(second, 77));
        let mut bytes = second.encode();
        bytes[4..8].copy_from_slice(&(PRESENTATIONS + 1).to_le_bytes());
        assert!(Image::decode(&bytes).is_none());
    }

    #[test]
    fn exact_image_and_retirement_frames_reject_truncation_reserved_and_stale_completion() {
        let i = image();
        let bytes = i.encode();
        assert_eq!(Image::decode(&bytes), Some(i));
        assert_eq!(Image::decode(&bytes[..63]), None);
        for n in [0, 52, 63] {
            let mut bad = bytes;
            bad[n] ^= 1;
            assert_eq!(Image::decode(&bad), None);
        }
        let r = Reply {
            sequence: 1,
            gpu_point: 3,
            retired: true,
        };
        assert_eq!(Reply::decode(&r.encode()), Some(r));
        assert!(r.releases(i));
        assert!(!Reply {
            retired: false,
            ..r
        }
        .releases(i));
        assert!(!Reply { gpu_point: 2, ..r }.releases(i));
        assert!(!Reply { sequence: 2, ..r }.releases(i));
        let mut bad = r.encode();
        bad[16] = 2;
        assert_eq!(Reply::decode(&bad), None);
        bad = r.encode();
        bad[23] = 1;
        assert_eq!(Reply::decode(&bad), None);
    }
    #[test]
    fn image_extents_are_exact_and_overflow_or_unsupported_layout_never_enters_scanout() {
        let i = image();
        assert!(i.valid());
        for bad in [
            Image {
                offset: u64::MAX - 4095,
                ..i
            },
            Image {
                vmo_bytes: 0x20000,
                ..i
            },
            Image {
                vmo_bytes: MAX_VMO_BYTES + 4096,
                ..i
            },
            Image { offset: 4, ..i },
            Image { gpu_point: 0, ..i },
            Image {
                surface: Surface {
                    stride: 257,
                    ..i.surface
                },
                ..i
            },
            Image {
                surface: Surface {
                    format: 2,
                    ..i.surface
                },
                ..i
            },
        ] {
            assert!(!bad.valid());
        }
        let mut words = 0;
        i.checksum(|_| {
            words += 1;
            0
        });
        assert_eq!(words, 32768);
    }
}
