//! AX1: per-address-space ASID allocation. Pure logic only — no register access,
//! no assembly, no allocation.
//!
//! AArch64 tags TTBR0 translations with an ASID (TTBR0_EL1 bits [63:48]) whose width
//! the core reports in `ID_AA64MMFR0_EL1.ASIDBits` (8 or 16 bits; see
//! [`decode_asid_bits`]). ASID 0 is reserved for the kernel's own TTBR0 identity map,
//! so this allocator never hands it out.
//!
//! # Caller contract
//!
//! * **Exclusion.** One allocator belongs to one core and has no internal locking.
//!   The owner serializes `allocate`/`release`/`rollover`. Cross-CPU ASID and
//!   generation rules belong to PLAN/014 MP4; this module implements only the
//!   one-core policy PLAN/021 AX1 introduces first.
//! * **Before `release`.** The ASID must already be invalidated in the TLB for every
//!   PE that could hold a translation for it (on one core: `tlbi aside1{,is}` by
//!   ASID). `release` immediately makes the number allocatable again, so a stale
//!   translation must not be able to survive into its next owner.
//! * **Before `rollover`.** The caller has performed a full local invalidation
//!   (`tlbi vmalle1{,is}`). `rollover` then starts a new generation and keeps the
//!   live ASIDs and their numbers.
//!
//! The generation returned with each allocation is an advisory 32-bit tag: it
//! increments on every [`AsidAllocator::rollover`] so a caller can tell allocations
//! from either side of a full flush apart. It wraps (`u32::MAX` -> `0`); compare
//! generations with equality, never ordering.
//!
//! # Size
//!
//! The bitmap is sized for the architectural maximum, 16-bit ASIDs: 65536 values,
//! one of them reserved, held in 1024 `u64` words (8 KiB). It is a plain value, so
//! place it in owned boot state, not on a deep IRQ stack.

/// ASID width the hardware can report through `ID_AA64MMFR0_EL1.ASIDBits`.
const MAX_ASID_BITS: u8 = 16;
const BITS_PER_WORD: usize = 64;
/// 2^16 ASID values / 64 bits per word.
const BITMAP_WORDS: usize = 1024;

/// The shift of the ASID field in `TTBR0_EL1` (bits [63:48]).
const TTBR0_ASID_SHIFT: u32 = 48;
/// `TTBR0_EL1` address bits [47:0] are the translation-table base plus the CnP bit.
const TTBR0_ADDRESS_MASK: u64 = (1u64 << TTBR0_ASID_SHIFT) - 1;
/// CnP (bit 0) selects shared versus private TTBR0 walks; KUMO leaves it clear.
const TTBR0_CNP: u64 = 1;

/// A hardware ASID: the value programmed into `TTBR0_EL1` bits [63:48].
///
/// The wrapper deliberately permits 0, because decoding a TTBR0 written by
/// someone else (for example the kernel identity map) must be able to report the
/// kernel's reserved ASID. Only [`AsidAllocator::allocate`] is forbidden from
/// producing it.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Asid(u16);

impl Asid {
    /// The kernel identity map's reserved ASID. Never returned by `allocate`.
    pub const KERNEL: Asid = Asid(0);

    /// Wrap a numeric ASID. Values wider than the hardware's reported width are
    /// rejected later, by the allocator that owns that width.
    pub const fn new(raw: u16) -> Self {
        Asid(raw)
    }

    /// The numeric value as programmed into TTBR0.
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Whether this is the kernel's reserved ASID 0.
    pub const fn is_kernel(self) -> bool {
        self.0 == 0
    }
}

/// Result of [`AsidAllocator::allocate`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllocOutcome {
    /// A fresh ASID, together with the allocator's generation at the time.
    Allocated { asid: Asid, generation: u32 },
    /// Every ASID of the configured width is live. Invalidate the whole local TLB,
    /// release any address spaces that died, then call `rollover` and retry.
    Exhausted,
}

/// Why [`AsidAllocator::release`] refused an ASID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsidError {
    /// ASID 0 belongs to the kernel identity map and is never owned by the allocator.
    KernelReserved,
    /// The ASID is not live: never allocated, already released, or wider than this
    /// allocator's configured width.
    NotLive,
}

/// Fixed-size, first-fit ASID allocator for one core.
///
/// `allocate` returns the lowest-numbered free ASID above 0. `release` makes a live
/// ASID free again, and `rollover` keeps live ASIDs while starting a new generation
/// after a full TLB invalidation. The struct has no heap, atomics, or locks.
#[derive(Clone)]
pub struct AsidAllocator {
    width_bits: u8,
    generation: u32,
    live_count: u32,
    /// Bit `n` set means ASID `n` is live. ASID 0 is never set.
    bitmap: [u64; BITMAP_WORDS],
}

impl AsidAllocator {
    /// Create an allocator for `width_bits` of ASID space.
    ///
    /// Accepts 1..=16. Hardware uses 8 or 16 (whatever `decode_asid_bits` reports
    /// from the core); the small widths exist so rollover and exhaustion are
    /// reachable in tests.
    pub const fn new(width_bits: u8) -> Option<Self> {
        if width_bits == 0 || width_bits > MAX_ASID_BITS {
            return None;
        }
        Some(Self {
            width_bits,
            generation: 0,
            live_count: 0,
            bitmap: [0; BITMAP_WORDS],
        })
    }

    /// Number of ASIDs this allocator can ever hand out: `2^width - 1` (ASID 0
    /// excluded).
    pub const fn capacity(&self) -> u32 {
        (1u32 << self.width_bits) - 1
    }

    /// The current generation tag. Starts at 0 and increments on every rollover,
    /// wrapping from `u32::MAX` to 0.
    pub const fn generation(&self) -> u32 {
        self.generation
    }

    /// Number of ASIDs currently live (allocated and not released).
    pub const fn live_count(&self) -> u32 {
        self.live_count
    }

    /// Allocate the lowest free ASID.
    ///
    /// This does not touch the TLB and does not consult the generation: an ASID
    /// released through [`Self::release`] is immediately reusable, because that
    /// call's contract already invalidated it.
    #[must_use]
    pub fn allocate(&mut self) -> AllocOutcome {
        let words = self.words_needed();
        for word in 0..words {
            let free = self.word_mask(word) & !self.bitmap[word];
            if free != 0 {
                let bit = free.trailing_zeros() as usize;
                let raw = (word * BITS_PER_WORD + bit) as u16;
                self.bitmap[word] |= 1u64 << bit;
                self.live_count += 1;
                return AllocOutcome::Allocated {
                    asid: Asid::new(raw),
                    generation: self.generation,
                };
            }
        }
        AllocOutcome::Exhausted
    }

    /// Mark a live ASID free.
    ///
    /// # Caller contract
    /// The ASID must already have been invalidated in the TLB (`tlbi aside1{,is}` by
    /// ASID, broadcast as the platform requires) for every PE that could cache a
    /// translation for it. The next `allocate` may return the same number, and a
    /// stale translation surviving that reuse would alias two address spaces.
    ///
    /// ASID 0 is [`AsidError::KernelReserved`]; an ASID that is not live, is already
    /// released, or is outside the configured width is [`AsidError::NotLive`].
    pub fn release(&mut self, asid: Asid) -> Result<(), AsidError> {
        let raw = asid.raw() as usize;
        if raw == 0 {
            return Err(AsidError::KernelReserved);
        }
        if raw > self.max_asid() as usize {
            return Err(AsidError::NotLive);
        }
        let word = raw / BITS_PER_WORD;
        let bit = raw % BITS_PER_WORD;
        let mask = 1u64 << bit;
        if self.bitmap[word] & mask == 0 {
            return Err(AsidError::NotLive);
        }
        self.bitmap[word] &= !mask;
        self.live_count -= 1;
        Ok(())
    }

    /// Start a new generation after the caller has invalidated the whole local TLB.
    ///
    /// Bumps the generation (wrapping from `u32::MAX` to 0, which is harmless
    /// because every rollover is preceded by a full invalidation) and returns it.
    /// Live ASIDs stay live and keep their numbers; every non-live ASID is
    /// allocatable (released ones already were — this clears any reservation that
    /// is not live, so no stale allocation can outlive the flush).
    ///
    /// This never revokes a live ASID, so `allocate` can still return `Exhausted`
    /// afterwards if all ASIDs of the width are live.
    pub fn rollover(&mut self) -> u32 {
        self.generation = self.generation.wrapping_add(1);
        for word in 0..BITMAP_WORDS {
            let keep = self.word_mask(word);
            self.bitmap[word] &= keep;
        }
        self.generation
    }

    /// Highest ASID value the configured width can express.
    fn max_asid(&self) -> u32 {
        (1u32 << self.width_bits) - 1
    }

    /// Number of bitmap words that can hold a value of this width.
    fn words_needed(&self) -> usize {
        ((1usize << self.width_bits) + BITS_PER_WORD - 1) / BITS_PER_WORD
    }

    /// Bit mask of the ASID slots that exist in `word`, with ASID 0 excluded in
    /// word 0. Bits beyond the configured width are always 0.
    fn word_mask(&self, word: usize) -> u64 {
        let first = word * BITS_PER_WORD;
        let values = 1usize << self.width_bits;
        if first >= values {
            return 0;
        }
        let bits = core::cmp::min(BITS_PER_WORD, values - first);
        let mask = if bits == BITS_PER_WORD {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        if word == 0 {
            mask & !1
        } else {
            mask
        }
    }
}

/// Decode `ID_AA64MMFR0_EL1.ASIDBits` (bits [7:4]) into the number of ASID bits.
///
/// `0b0000` is the 8-bit implementation, `0b0010` the 16-bit one; every other
/// encoding is reserved and yields `None`. A caller that gets `None` must not
/// program a non-zero ASID.
pub fn decode_asid_bits(mmfr0: u64) -> Option<u8> {
    match (mmfr0 >> 4) & 0xf {
        0b0000 => Some(8),
        0b0010 => Some(16),
        _ => None,
    }
}

/// Encode a TTBR0 value: `root` in bits [47:0], `asid` in bits [63:48].
///
/// Returns `None` when `root` already uses a bit that the encoding needs:
/// * bits [63:48] would collide with the ASID, and
/// * bit 0 is CnP, which must stay clear for a private TTBR0 walk.
///
/// Table alignment is the caller's contract (KUMO roots are frame-aligned); the
/// remaining address bits are preserved verbatim.
#[must_use]
pub fn encode_ttbr0(root: u64, asid: Asid) -> Option<u64> {
    if root & !TTBR0_ADDRESS_MASK != 0 || root & TTBR0_CNP != 0 {
        return None;
    }
    Some(((asid.raw() as u64) << TTBR0_ASID_SHIFT) | root)
}

/// The ASID field of a TTBR0 value (bits [63:48]).
pub const fn asid_of_ttbr0(value: u64) -> Asid {
    Asid::new((value >> TTBR0_ASID_SHIFT) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocated(outcome: AllocOutcome) -> (Asid, u32) {
        match outcome {
            AllocOutcome::Allocated { asid, generation } => (asid, generation),
            AllocOutcome::Exhausted => panic!("expected an allocation, got Exhausted"),
        }
    }

    #[test]
    fn width_8_allocates_one_through_255_then_exhausts() {
        let mut alloc = AsidAllocator::new(8).unwrap();
        assert_eq!(alloc.capacity(), 255);
        assert_eq!(alloc.generation(), 0);
        assert_eq!(alloc.live_count(), 0);
        for expected in 1..=255u16 {
            let (asid, generation) = allocated(alloc.allocate());
            assert_eq!(asid, Asid::new(expected));
            assert_eq!(generation, 0);
            assert_eq!(alloc.live_count(), expected as u32);
        }
        assert_eq!(alloc.allocate(), AllocOutcome::Exhausted);
        // Exhaustion changes nothing.
        assert_eq!(alloc.live_count(), 255);
        assert_eq!(alloc.allocate(), AllocOutcome::Exhausted);
    }

    #[test]
    fn release_makes_an_asid_allocatable_again() {
        let mut alloc = AsidAllocator::new(8).unwrap();
        for _ in 0..3 {
            allocated(alloc.allocate());
        }
        assert_eq!(alloc.release(Asid::new(2)), Ok(()));
        assert_eq!(alloc.live_count(), 2);
        let (asid, generation) = allocated(alloc.allocate());
        assert_eq!(asid, Asid::new(2));
        assert_eq!(generation, 0);
        assert_eq!(alloc.live_count(), 3);
    }

    #[test]
    fn double_and_unknown_release_are_rejected() {
        let mut alloc = AsidAllocator::new(8).unwrap();
        // Never allocated.
        assert_eq!(alloc.release(Asid::new(1)), Err(AsidError::NotLive));
        allocated(alloc.allocate());
        assert_eq!(alloc.release(Asid::new(1)), Ok(()));
        // Double release.
        assert_eq!(alloc.release(Asid::new(1)), Err(AsidError::NotLive));
        // Representable but never allocated.
        assert_eq!(alloc.release(Asid::new(255)), Err(AsidError::NotLive));
        // Outside this allocator's width.
        assert_eq!(alloc.release(Asid::new(256)), Err(AsidError::NotLive));
        assert_eq!(alloc.release(Asid::new(u16::MAX)), Err(AsidError::NotLive));
        // The kernel's slot is refused distinctly.
        assert_eq!(alloc.release(Asid::KERNEL), Err(AsidError::KernelReserved));
        assert_eq!(alloc.live_count(), 0);
    }

    #[test]
    fn rollover_keeps_live_asids_and_frees_only_released_ones() {
        let mut alloc = AsidAllocator::new(8).unwrap();
        for _ in 0..5 {
            allocated(alloc.allocate()); // 1..=5 live
        }
        assert_eq!(alloc.release(Asid::new(3)), Ok(()));
        assert_eq!(alloc.live_count(), 4);

        assert_eq!(alloc.rollover(), 1);
        assert_eq!(alloc.generation(), 1);
        assert_eq!(alloc.live_count(), 4);

        // A released ASID did not become live; one that stayed live did not die.
        assert_eq!(alloc.release(Asid::new(3)), Err(AsidError::NotLive));
        for live in [1u16, 2, 4, 5] {
            assert_eq!(alloc.release(Asid::new(live)), Ok(()));
        }
        assert_eq!(alloc.live_count(), 0);
        assert_eq!(alloc.release(Asid::new(1)), Err(AsidError::NotLive));

        // The released slot is allocatable in the new generation and reports it.
        let mut alloc = AsidAllocator::new(8).unwrap();
        for _ in 0..5 {
            allocated(alloc.allocate());
        }
        assert_eq!(alloc.release(Asid::new(3)), Ok(()));
        alloc.rollover();
        let (asid, generation) = allocated(alloc.allocate());
        assert_eq!(asid, Asid::new(3));
        assert_eq!(generation, 1);
    }

    #[test]
    fn tiny_width_forces_repeated_rollover() {
        let mut alloc = AsidAllocator::new(2).unwrap();
        assert_eq!(alloc.capacity(), 3);
        for round in 0..20u32 {
            assert_eq!(alloc.generation(), round);
            for expected in 1..=3u16 {
                assert_eq!(allocated(alloc.allocate()).0, Asid::new(expected));
            }
            assert_eq!(alloc.live_count(), 3);
            assert_eq!(alloc.allocate(), AllocOutcome::Exhausted);
            for asid in 1..=3u16 {
                assert_eq!(alloc.release(Asid::new(asid)), Ok(()));
            }
            assert_eq!(alloc.rollover(), round.wrapping_add(1));
        }
        assert_eq!(alloc.generation(), 20);
    }

    #[test]
    fn rollover_keeps_live_asids_across_generation_wrap() {
        let mut alloc = AsidAllocator::new(2).unwrap();
        allocated(alloc.allocate()); // ASID 1 stays live
                                     // A generation reset would need 2^32 flushes on hardware; drive the field
                                     // directly so the wrap is exercised on the host.
        alloc.generation = u32::MAX;
        assert_eq!(alloc.rollover(), 0);
        assert_eq!(alloc.generation(), 0);
        assert_eq!(alloc.live_count(), 1);
        let (asid, generation) = allocated(alloc.allocate());
        assert_eq!(asid, Asid::new(2)); // 1 was not renumbered or freed
        assert_eq!(generation, 0);
        assert_eq!(alloc.rollover(), 1);
        assert_eq!(alloc.generation(), 1);
    }

    #[test]
    fn capacity_matches_the_width_excluding_asid_zero() {
        for width in 1..=16u8 {
            let alloc = AsidAllocator::new(width).unwrap();
            assert_eq!(alloc.capacity(), (1u32 << width) - 1);
            assert_eq!(alloc.generation(), 0);
            assert_eq!(alloc.live_count(), 0);
        }
        for width in [0u8, 17, 32, 255] {
            assert!(AsidAllocator::new(width).is_none());
        }
    }

    #[test]
    fn asid_zero_is_never_allocated_at_any_width() {
        for width in 1..=16u8 {
            let mut alloc = AsidAllocator::new(width).unwrap();
            let fill = core::cmp::min(alloc.capacity(), 64);
            for _ in 0..fill {
                let (asid, _) = allocated(alloc.allocate());
                assert!(!asid.is_kernel());
                assert_ne!(asid.raw(), 0);
            }
            assert_eq!(alloc.release(Asid::KERNEL), Err(AsidError::KernelReserved));
        }
    }

    #[test]
    fn width_16_reaches_the_architectural_maximum() {
        let mut alloc = AsidAllocator::new(16).unwrap();
        assert_eq!(alloc.capacity(), 65535);
        // Fill every one of the 1024 bitmap words, then prove exhaustion.
        for expected in 1..=65535u16 {
            let (asid, generation) = allocated(alloc.allocate());
            assert_eq!(asid, Asid::new(expected));
            assert_eq!(generation, 0);
        }
        assert_eq!(alloc.live_count(), 65535);
        assert_eq!(alloc.allocate(), AllocOutcome::Exhausted);
        // The highest ASID is live and can be released and reused.
        assert_eq!(alloc.release(Asid::new(65535)), Ok(()));
        assert_eq!(alloc.live_count(), 65534);
        assert_eq!(allocated(alloc.allocate()).0, Asid::new(65535));
    }

    #[test]
    fn decode_asid_bits_reads_only_field_7_4() {
        // Set every bit except the ASIDBits field, then sweep the field.
        let surroundings = !0xf0u64;
        for field in 0..16u64 {
            let mmfr0 = surroundings | (field << 4);
            let expected = match field {
                0b0000 => Some(8),
                0b0010 => Some(16),
                _ => None,
            };
            assert_eq!(decode_asid_bits(mmfr0), expected, "field {field:#06b}");
        }
        // Bits outside [7:4] never leak into the result.
        assert_eq!(decode_asid_bits(0xffff_ffff_ffff_ff0f), Some(8));
        assert_eq!(decode_asid_bits(0xffff_ffff_ffff_ff2f), Some(16));
        assert_eq!(decode_asid_bits(0x0000_0000_0000_0f00), Some(8));
        assert_eq!(decode_asid_bits(0), Some(8));
    }

    const ROOT: u64 = 0x0000_0000_4010_0000;

    #[test]
    fn ttbr0_round_trips_the_asid_and_the_root() {
        for raw in [0u16, 1, 0xff, 0x100, 0x1234, 0xffff] {
            let asid = Asid::new(raw);
            let value = encode_ttbr0(ROOT, asid).unwrap();
            assert_eq!(value >> 48, raw as u64);
            assert_eq!(value & 0x0000_ffff_ffff_ffff, ROOT);
            assert_eq!(value & 1, 0); // CnP stays clear
            assert_eq!(asid_of_ttbr0(value), asid);
        }
        assert_eq!(asid_of_ttbr0(ROOT), Asid::KERNEL);
        assert_eq!(asid_of_ttbr0(u64::MAX), Asid::new(0xffff));
    }

    #[test]
    fn ttbr0_encoding_is_48_bit_aligned_and_preserves_address_bits() {
        let value = encode_ttbr0(ROOT, Asid::new(0xbeef)).unwrap();
        assert_eq!(value, (0xbeefu64 << 48) | ROOT);
        assert_eq!(value & 0xffff_0000_0000_0000, 0xbeefu64 << 48);
        assert_eq!(value & 0x0000_ffff_ffff_ffff, ROOT);
        // Bit 0 (CnP) is the only encoding collision below the ASID field.
        assert!(encode_ttbr0(ROOT | 1, Asid::new(1)).is_none());
        assert!(encode_ttbr0(ROOT | 0x800, Asid::new(1)).is_some());
        assert_eq!(
            encode_ttbr0(ROOT | 0x800, Asid::new(1)).unwrap(),
            (1u64 << 48) | ROOT | 0x800
        );
    }

    #[test]
    fn ttbr0_rejects_non_zero_high_bits_on_the_root() {
        assert!(encode_ttbr0(1 << 48, Asid::KERNEL).is_none());
        assert!(encode_ttbr0(ROOT | (1 << 48), Asid::new(1)).is_none());
        assert!(encode_ttbr0(1 << 63, Asid::new(1)).is_none());
        assert!(encode_ttbr0(u64::MAX, Asid::new(0xffff)).is_none());
        // The full 48-bit address range is still accepted.
        let wide_root = 0x0000_ffff_ffff_f000;
        let value = encode_ttbr0(wide_root, Asid::new(2)).unwrap();
        assert_eq!(value & 0x0000_ffff_ffff_ffff, wide_root);
        assert_eq!(asid_of_ttbr0(value), Asid::new(2));
    }

    #[test]
    fn kernel_asid_encodes_and_never_allocates() {
        let value = encode_ttbr0(ROOT, Asid::KERNEL).unwrap();
        assert_eq!(value, ROOT);
        assert_eq!(asid_of_ttbr0(value), Asid::KERNEL);
        let mut alloc = AsidAllocator::new(8).unwrap();
        let (first, _) = allocated(alloc.allocate());
        assert_eq!(first, Asid::new(1));
        assert!(!first.is_kernel());
    }
}
