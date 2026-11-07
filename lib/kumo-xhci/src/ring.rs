//j539
//j540
//j828
//j830

use crate::{Error, QwordSink, Trb, TrbSink, TrbSource};

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

/// Where a stopped endpoint must resume: the next producer TRB, whose cycle bit still
/// differs from `cycle` until software writes it. Set TR Dequeue uses `dequeue_iova` and
/// `cycle` as DCS. Right after a wrap the controller may report the Link TRB instead,
/// which names the same position. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingRestart {
    pub dequeue_iova: u64,
    pub cycle: bool,
    pub link_iova: Option<u64>,
}

impl RingRestart {
    /// True when a controller-reported stopped dequeue pointer names this position.
    pub fn names(self, iova: u64) -> bool {
        iova == self.dequeue_iova || self.link_iova == Some(iova)
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
    fn initialize<S: TrbSink + ?Sized>(segment: &mut S, base_iova: u64) -> Result<Self, Error> {
        let len = segment.trb_slots();
        validate_producer_segment(len, base_iova)?;
        for index in 0..len - 1 {
            segment.write_trb(index, Trb::zero());
        }
        segment.write_trb(len - 1, Trb::link(base_iova, false));
        Ok(Self {
            base_iova,
            segment_len: len,
            enqueue: 0,
            dequeue: 0,
            queued: 0,
            producer_cycle: true,
        })
    }

    fn enqueue<S: TrbSink + ?Sized>(
        &mut self,
        segment: &mut S,
        trb: Trb,
    ) -> Result<RingToken, Error> {
        self.check_memory(segment.trb_slots())?;
        if self.queued == self.capacity() {
            return Err(Error::RingFull);
        }

        let index = self.enqueue;
        segment.write_trb(index, trb.with_cycle(self.producer_cycle));
        let token = RingToken {
            iova: self.base_iova + index as u64 * TRB_BYTES,
        };
        self.enqueue += 1;
        if self.enqueue == self.segment_len - 1 {
            let link = self.enqueue;
            segment.write_trb(link, Trb::link(self.base_iova, self.producer_cycle));
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
    pub fn new<S: TrbSink + ?Sized>(segment: &mut S, base_iova: u64) -> Result<Self, Error> {
        ProducerRing::initialize(segment, base_iova).map(Self)
    }

    pub fn enqueue<S: TrbSink + ?Sized>(
        &mut self,
        segment: &mut S,
        trb: Trb,
    ) -> Result<RingToken, Error> {
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
    pub fn new<S: TrbSink + ?Sized>(segment: &mut S, base_iova: u64) -> Result<Self, Error> {
        ProducerRing::initialize(segment, base_iova).map(Self)
    }

    /// Predict physical ownership before making any cycle bit visible to DMA.
    pub fn preview<const N: usize>(&self) -> Result<[u64; N], Error> {
        if N == 0 || N > self.capacity() - self.queued() {
            return Err(Error::RingFull);
        }
        let mut result = [0; N];
        let mut index = self.0.enqueue;
        for slot in &mut result {
            *slot = self.0.base_iova + index as u64 * TRB_BYTES;
            index = advance(index, self.0.segment_len);
        }
        Ok(result)
    }

    pub fn enqueue<S: TrbSink + ?Sized>(
        &mut self,
        segment: &mut S,
        trb: Trb,
    ) -> Result<RingToken, Error> {
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

    /// The position a verified stop must repoint the controller to: past every TRB
    /// this ring has published.
    pub const fn restart(&self) -> RingRestart {
        let ring = &self.0;
        RingRestart {
            dequeue_iova: ring.base_iova + ring.enqueue as u64 * TRB_BYTES,
            cycle: ring.producer_cycle,
            link_iova: if ring.enqueue == 0 {
                Some(ring.base_iova + (ring.segment_len as u64 - 1) * TRB_BYTES)
            } else {
                None
            },
        }
    }

    /// Give up every queued TRB. Only call after the controller confirmed the endpoint
    /// Stopped and accepted a Set TR Dequeue to exactly `restart`; refuses if the ring
    /// published anything since. Returns how many TRBs were abandoned. — TIMBERDOODLE
    pub fn abandon(&mut self, restart: RingRestart) -> Result<usize, Error> {
        if restart != self.restart() {
            return Err(Error::RingPositionChanged);
        }
        let abandoned = self.0.queued;
        self.0.dequeue = self.0.enqueue;
        self.0.queued = 0;
        Ok(abandoned)
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
    pub fn new<S: TrbSink + ?Sized>(segment: &mut S, base_iova: u64) -> Result<Self, Error> {
        let len = segment.trb_slots();
        validate_event_segment(len, base_iova)?;
        for index in 0..len {
            segment.write_trb(index, Trb::zero());
        }
        Ok(Self {
            base_iova,
            segment_len: len,
            dequeue: 0,
            consumer_cycle: true,
        })
    }

    /// Pop the next controller-owned event. A cycle mismatch means the ring is presently empty.
    /// `segment` is a slice in tests and a volatile [`crate::DmaTrbs`] over device memory.
    pub fn pop<S: TrbSource + ?Sized>(&mut self, segment: &S) -> Result<Option<Trb>, Error> {
        if segment.trb_count() != self.segment_len {
            return Err(Error::RingMemoryLength);
        }
        let trb = segment.read_trb(self.dequeue);
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

    /// The entry as the two qwords the controller reads: the segment base, then the size word
    /// with its reserved upper half.
    pub const fn qwords(self) -> [u64; 2] {
        [
            self.words[0] as u64 | (self.words[1] as u64) << 32,
            self.words[2] as u64 | (self.words[3] as u64) << 32,
        ]
    }

    pub(crate) fn set_qword(&mut self, index: usize, value: u64) {
        self.words[index * 2] = value as u32;
        self.words[index * 2 + 1] = (value >> 32) as u32;
    }

    /// Write this entry where the controller reads it.
    pub fn write_to<S: QwordSink + ?Sized>(self, sink: &mut S) {
        let [base, size] = self.qwords();
        sink.write_qword(0, base);
        sink.write_qword(1, size);
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
    fn restart_names_the_next_producer_trb_and_abandon_requires_it_unchanged() {
        let mut memory = RingSegment::<4>::new();
        let mut ring = TransferRing::new(memory.entries_mut(), 0x8000).unwrap();
        let fresh = ring.restart();
        assert_eq!(
            fresh,
            RingRestart {
                dequeue_iova: 0x8000,
                cycle: true,
                link_iova: Some(0x8030),
            }
        );
        let trb = Trb::normal_transfer(NormalTransfer::interrupt_in(0x2_0000, 4)).unwrap();
        let first = ring.enqueue(memory.entries_mut(), trb).unwrap();
        let after = ring.restart();
        assert_eq!(after.dequeue_iova, 0x8010);
        assert_eq!(after.link_iova, None);
        assert!(after.names(0x8010) && !after.names(first.iova) && !after.names(0x8030));
        assert_eq!(ring.abandon(fresh), Err(Error::RingPositionChanged));
        assert_eq!(ring.queued(), 1);
        assert_eq!(ring.abandon(after), Ok(1));
        assert_eq!(ring.queued(), 0);
        assert_eq!(
            ring.reclaim_through(first.iova),
            Err(Error::CompletionNotPending)
        );
        // Abandoning around the wrap: three single-TRB TDs fill the lap, the producer
        // toggles its cycle, and the Link TRB is an alias of the restart position.
        for _ in 0..2 {
            ring.enqueue(memory.entries_mut(), trb).unwrap();
            let restart = ring.restart();
            assert_eq!(ring.abandon(restart), Ok(1));
        }
        let wrapped = ring.restart();
        assert_eq!(
            wrapped,
            RingRestart {
                dequeue_iova: 0x8000,
                cycle: false,
                link_iova: Some(0x8030),
            }
        );
        assert!(wrapped.names(0x8030));
        // The stale first-lap TRB at the restart position is invalid under the new DCS.
        assert_ne!(memory.entries()[0].cycle(), wrapped.cycle);
        assert_eq!(ring.abandon(wrapped), Ok(0));
        let next = ring.enqueue(memory.entries_mut(), trb).unwrap();
        assert_eq!(next.iova, 0x8000);
        assert_eq!(memory.entries()[0].cycle(), wrapped.cycle);
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
