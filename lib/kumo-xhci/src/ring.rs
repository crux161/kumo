use crate::{Error, Trb};

const TRB_BYTES: u64 = 16;
const RING_ALIGNMENT: u64 = 64;
const MAX_SEGMENT_TRBS: usize = 4096;

/// Statically sized, cache-line-aligned backing suitable for a mapped DMA ring segment.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingSegment<const N: usize> {
    entries: [Trb; N],
}

impl<const N: usize> RingSegment<N> {
    pub const fn new() -> Self {
        Self {
            entries: [Trb::zero(); N],
        }
    }

    pub const fn entries(&self) -> &[Trb; N] {
        &self.entries
    }

    pub fn entries_mut(&mut self) -> &mut [Trb; N] {
        &mut self.entries
    }
}

impl<const N: usize> Default for RingSegment<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Device-visible address of one queued TRB.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingToken {
    pub iova: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProducerRing {
    base_iova: u64,
    segment_len: usize,
    enqueue: usize,
    dequeue: usize,
    queued: usize,
    producer_cycle: bool,
}

impl ProducerRing {
    fn initialize(segment: &mut [Trb], base_iova: u64) -> Result<Self, Error> {
        validate_producer_segment(segment.len(), base_iova)?;
        segment.fill(Trb::zero());
        let link = segment.len() - 1;
        segment[link] = Trb::link(base_iova, false);
        Ok(Self {
            base_iova,
            segment_len: segment.len(),
            enqueue: 0,
            dequeue: 0,
            queued: 0,
            producer_cycle: true,
        })
    }

    fn enqueue(&mut self, segment: &mut [Trb], trb: Trb) -> Result<RingToken, Error> {
        self.check_memory(segment.len())?;
        if self.queued == self.capacity() {
            return Err(Error::RingFull);
        }

        let index = self.enqueue;
        segment[index] = trb.with_cycle(self.producer_cycle);
        let token = RingToken {
            iova: self.base_iova + index as u64 * TRB_BYTES,
        };
        self.enqueue += 1;
        if self.enqueue == self.segment_len - 1 {
            let link = self.enqueue;
            segment[link] = Trb::link(self.base_iova, self.producer_cycle);
            self.enqueue = 0;
            self.producer_cycle = !self.producer_cycle;
        }
        self.queued += 1;
        Ok(token)
    }

    fn reclaim_through(&mut self, completed_iova: u64) -> Result<usize, Error> {
        let Some(target) = self.index_for(completed_iova) else {
            return Err(Error::CompletionNotPending);
        };
        let mut index = self.dequeue;
        for released in 1..=self.queued {
            if index == target {
                self.dequeue = advance(index, self.segment_len);
                self.queued -= released;
                return Ok(released);
            }
            index = advance(index, self.segment_len);
        }
        Err(Error::CompletionNotPending)
    }

    const fn capacity(&self) -> usize {
        self.segment_len - 1
    }

    const fn queued(&self) -> usize {
        self.queued
    }

    const fn producer_cycle(&self) -> bool {
        self.producer_cycle
    }

    fn check_memory(&self, len: usize) -> Result<(), Error> {
        if len == self.segment_len {
            Ok(())
        } else {
            Err(Error::RingMemoryLength)
        }
    }

    fn index_for(&self, iova: u64) -> Option<usize> {
        let offset = iova.checked_sub(self.base_iova)?;
        if offset % TRB_BYTES != 0 {
            return None;
        }
        let index = (offset / TRB_BYTES) as usize;
        (index < self.segment_len - 1).then_some(index)
    }
}

/// Software-producer command ring with a Link TRB in the last segment entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandRing(ProducerRing);

impl CommandRing {
    pub fn new(segment: &mut [Trb], base_iova: u64) -> Result<Self, Error> {
        ProducerRing::initialize(segment, base_iova).map(Self)
    }

    pub fn enqueue(&mut self, segment: &mut [Trb], trb: Trb) -> Result<RingToken, Error> {
        if !trb.trb_type().is_some_and(|kind| kind.is_command()) {
            return Err(Error::WrongRingType);
        }
        self.0.enqueue(segment, trb)
    }

    pub fn reclaim_through(&mut self, completed_iova: u64) -> Result<usize, Error> {
        self.0.reclaim_through(completed_iova)
    }

    pub const fn queued(&self) -> usize {
        self.0.queued()
    }

    pub const fn capacity(&self) -> usize {
        self.0.capacity()
    }

    pub const fn producer_cycle(&self) -> bool {
        self.0.producer_cycle()
    }
}

/// Software-producer endpoint transfer ring with a Link TRB in the last segment entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferRing(ProducerRing);

impl TransferRing {
    pub fn new(segment: &mut [Trb], base_iova: u64) -> Result<Self, Error> {
        ProducerRing::initialize(segment, base_iova).map(Self)
    }

    pub fn enqueue(&mut self, segment: &mut [Trb], trb: Trb) -> Result<RingToken, Error> {
        if !trb.trb_type().is_some_and(|kind| kind.is_transfer()) {
            return Err(Error::WrongRingType);
        }
        self.0.enqueue(segment, trb)
    }

    pub fn reclaim_through(&mut self, completed_iova: u64) -> Result<usize, Error> {
        self.0.reclaim_through(completed_iova)
    }

    pub const fn queued(&self) -> usize {
        self.0.queued()
    }

    pub const fn capacity(&self) -> usize {
        self.0.capacity()
    }

    pub const fn producer_cycle(&self) -> bool {
        self.0.producer_cycle()
    }
}

/// Software-consumer state for one Event Ring segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventRing {
    base_iova: u64,
    segment_len: usize,
    dequeue: usize,
    consumer_cycle: bool,
}

impl EventRing {
    pub fn new(segment: &mut [Trb], base_iova: u64) -> Result<Self, Error> {
        validate_event_segment(segment.len(), base_iova)?;
        segment.fill(Trb::zero());
        Ok(Self {
            base_iova,
            segment_len: segment.len(),
            dequeue: 0,
            consumer_cycle: true,
        })
    }

    /// Pop the next controller-owned event. A cycle mismatch means the ring is presently empty.
    pub fn pop(&mut self, segment: &[Trb]) -> Result<Option<Trb>, Error> {
        if segment.len() != self.segment_len {
            return Err(Error::RingMemoryLength);
        }
        let trb = segment[self.dequeue];
        if trb.cycle() != self.consumer_cycle {
            return Ok(None);
        }
        self.dequeue += 1;
        if self.dequeue == self.segment_len {
            self.dequeue = 0;
            self.consumer_cycle = !self.consumer_cycle;
        }
        Ok(Some(trb))
    }

    /// Value to write to ERDP after consuming events.
    pub const fn dequeue_iova(&self) -> u64 {
        self.base_iova + self.dequeue as u64 * TRB_BYTES
    }

    pub const fn consumer_cycle(&self) -> bool {
        self.consumer_cycle
    }
}

/// One 16-byte Event Ring Segment Table entry.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventRingSegmentTableEntry {
    words: [u32; 4],
}

impl EventRingSegmentTableEntry {
    pub fn new(segment_iova: u64, trb_count: usize) -> Result<Self, Error> {
        validate_event_segment(trb_count, segment_iova)?;
        Ok(Self {
            words: [
                segment_iova as u32,
                (segment_iova >> 32) as u32,
                trb_count as u32,
                0,
            ],
        })
    }

    pub const fn words(self) -> [u32; 4] {
        self.words
    }
}

fn validate_producer_segment(len: usize, base_iova: u64) -> Result<(), Error> {
    if len < 2 {
        return Err(Error::RingTooSmall);
    }
    if len > MAX_SEGMENT_TRBS {
        return Err(Error::RingTooLarge);
    }
    validate_base(base_iova, len)
}

fn validate_event_segment(len: usize, base_iova: u64) -> Result<(), Error> {
    if len < 16 {
        return Err(Error::RingTooSmall);
    }
    if len > MAX_SEGMENT_TRBS {
        return Err(Error::RingTooLarge);
    }
    validate_base(base_iova, len)
}

fn validate_base(base_iova: u64, len: usize) -> Result<(), Error> {
    if base_iova & (RING_ALIGNMENT - 1) != 0 {
        return Err(Error::MisalignedIova);
    }
    base_iova
        .checked_add(len as u64 * TRB_BYTES)
        .ok_or(Error::InvalidField)?;
    Ok(())
}

const fn advance(index: usize, segment_len: usize) -> usize {
    if index + 1 == segment_len - 1 {
        0
    } else {
        index + 1
    }
}

#[cfg(test)]
mod tests {
    use core::mem::{align_of, size_of};

    use crate::{NormalTransfer, Trb, TrbType};

    use super::*;

    #[test]
    fn owned_ring_segment_has_hardware_alignment() {
        assert_eq!(align_of::<RingSegment<16>>(), 64);
        assert_eq!(size_of::<RingSegment<16>>(), 16 * 16);
    }

    #[test]
    fn command_ring_wraps_cycle_and_rejects_overwrite() {
        let mut memory = RingSegment::<4>::new();
        let mut ring = CommandRing::new(memory.entries_mut(), 0x4000).unwrap();
        let first = ring
            .enqueue(memory.entries_mut(), Trb::no_op_command())
            .unwrap();
        let second = ring
            .enqueue(memory.entries_mut(), Trb::no_op_command())
            .unwrap();
        let third = ring
            .enqueue(memory.entries_mut(), Trb::no_op_command())
            .unwrap();
        assert_eq!(
            (first.iova, second.iova, third.iova),
            (0x4000, 0x4010, 0x4020)
        );
        assert!(memory.entries()[..3].iter().all(|trb| trb.cycle()));
        assert_eq!(memory.entries()[3].trb_type(), Some(TrbType::Link));
        assert!(memory.entries()[3].cycle());
        assert!(!ring.producer_cycle());
        assert_eq!(
            ring.enqueue(memory.entries_mut(), Trb::no_op_command()),
            Err(Error::RingFull)
        );

        assert_eq!(ring.reclaim_through(first.iova), Ok(1));
        let wrapped = ring
            .enqueue(memory.entries_mut(), Trb::no_op_command())
            .unwrap();
        assert_eq!(wrapped.iova, 0x4000);
        assert!(!memory.entries()[0].cycle());
    }

    #[test]
    fn reclaim_through_releases_a_multi_trb_td() {
        let mut memory = RingSegment::<8>::new();
        let mut ring = TransferRing::new(memory.entries_mut(), 0x8000).unwrap();
        let config = NormalTransfer::interrupt_in(0x20_0000, 8);
        let one = ring
            .enqueue(memory.entries_mut(), Trb::normal_transfer(config).unwrap())
            .unwrap();
        let two = ring
            .enqueue(memory.entries_mut(), Trb::normal_transfer(config).unwrap())
            .unwrap();
        let three = ring
            .enqueue(memory.entries_mut(), Trb::normal_transfer(config).unwrap())
            .unwrap();
        assert_eq!(ring.reclaim_through(two.iova), Ok(2));
        assert_eq!(ring.queued(), 1);
        assert_eq!(
            ring.reclaim_through(one.iova),
            Err(Error::CompletionNotPending)
        );
        assert_eq!(ring.reclaim_through(three.iova), Ok(1));
    }

    #[test]
    fn event_ring_consumes_fake_controller_writes_and_toggles_on_wrap() {
        let mut memory = RingSegment::<16>::new();
        let mut ring = EventRing::new(memory.entries_mut(), 0xc000).unwrap();
        for entry in memory.entries_mut() {
            *entry = Trb::from_words([
                0,
                0,
                1 << 24,
                ((TrbType::HostControllerEvent as u32) << 10) | 1,
            ]);
        }
        for _ in 0..16 {
            assert!(ring.pop(memory.entries()).unwrap().is_some());
        }
        assert_eq!(ring.dequeue_iova(), 0xc000);
        assert!(!ring.consumer_cycle());
        assert!(ring.pop(memory.entries()).unwrap().is_none());

        memory.entries_mut()[0] = memory.entries()[0].with_cycle(false);
        assert!(ring.pop(memory.entries()).unwrap().is_some());
        assert_eq!(ring.dequeue_iova(), 0xc010);
    }

    #[test]
    fn event_segment_table_validates_spec_bounds_and_alignment() {
        let entry = EventRingSegmentTableEntry::new(0x10_0000, 16).unwrap();
        assert_eq!(entry.words(), [0x10_0000, 0, 16, 0]);
        assert_eq!(
            EventRingSegmentTableEntry::new(0x10_0010, 16),
            Err(Error::MisalignedIova)
        );
        assert_eq!(
            EventRingSegmentTableEntry::new(0x10_0000, 15),
            Err(Error::RingTooSmall)
        );
    }

    #[test]
    fn ring_kind_checks_keep_commands_and_transfers_separate() {
        let mut command_memory = RingSegment::<4>::new();
        let mut command = CommandRing::new(command_memory.entries_mut(), 0x4000).unwrap();
        let normal = Trb::normal_transfer(NormalTransfer::interrupt_in(0x8000, 8)).unwrap();
        assert_eq!(
            command.enqueue(command_memory.entries_mut(), normal),
            Err(Error::WrongRingType)
        );
    }
}
