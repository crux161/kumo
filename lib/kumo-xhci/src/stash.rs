//j545

//! Transfer events popped by a poller that was waiting on a different slot.
//!
//! One event ring serves every device on a controller. A driver that polls for one slot's
//! completion pops whatever the controller wrote first, and before J545 it dropped every event
//! that named another slot. That was harmless while only one transfer was ever in flight. Once
//! the resident keyboard loop also serves block reads for a USB stick, a keystroke can complete
//! while a READ(10) is being polled. Dropping that event would leave the keyboard's one
//! outstanding interrupt transfer "pending" forever, and typing would stop. The stash keeps such
//! events for their own slot's next poll.

/// One Transfer Event set aside for its slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StashedTransfer {
    pub slot_id: u8,
    pub completion_code: u8,
    pub remaining: u32,
    pub trb_iova: u64,
}

/// Events the stash holds at once. A slot has at most one transfer outstanding per endpoint in
/// this driver, so a handful covers every device that can complete during another's poll.
pub const STASH_CAPACITY: usize = 8;

/// A small FIFO of [`StashedTransfer`]s, searched by slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferEventStash {
    events: [Option<StashedTransfer>; STASH_CAPACITY],
    /// Arrival counter, so the oldest event for a slot is taken first and the oldest overall is
    /// the one a full stash gives up.
    sequence: [u32; STASH_CAPACITY],
    next: u32,
}

impl TransferEventStash {
    pub const fn new() -> Self {
        Self {
            events: [None; STASH_CAPACITY],
            sequence: [0; STASH_CAPACITY],
            next: 0,
        }
    }

    /// Keep `event` for its slot. When the stash is full the oldest event is dropped to make
    /// room and returned, so the caller can report the loss.
    pub fn push(&mut self, event: StashedTransfer) -> Option<StashedTransfer> {
        let (index, dropped) = match self.events.iter().position(Option::is_none) {
            Some(free) => (free, None),
            None => {
                let oldest = self
                    .oldest_matching(|_| true)
                    .expect("a full stash is not empty");
                (oldest, self.events[oldest])
            }
        };
        self.events[index] = Some(event);
        self.sequence[index] = self.next;
        self.next = self.next.wrapping_add(1);
        dropped
    }

    /// Remove and return the oldest event stashed for `slot_id`.
    pub fn take(&mut self, slot_id: u8) -> Option<StashedTransfer> {
        let index = self.oldest_matching(|event| event.slot_id == slot_id)?;
        self.events[index].take()
    }

    pub fn len(&self) -> usize {
        self.events.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn oldest_matching(&self, wanted: impl Fn(&StashedTransfer) -> bool) -> Option<usize> {
        let base = self.next;
        self.events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.as_ref().is_some_and(&wanted))
            // Ages measured back from `next` survive the counter wrapping.
            .max_by_key(|(index, _)| base.wrapping_sub(self.sequence[*index]))
            .map(|(index, _)| index)
    }
}

impl Default for TransferEventStash {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(slot_id: u8, trb_iova: u64) -> StashedTransfer {
        StashedTransfer {
            slot_id,
            completion_code: 1,
            remaining: 0,
            trb_iova,
        }
    }

    #[test]
    fn a_keystroke_completed_during_a_storage_read_waits_for_the_keyboard() {
        // J545's case: the stick is slot 4, the keyboard slot 3. The block read's poll pops the
        // keyboard's completion first and must leave it for the keyboard loop.
        let mut stash = TransferEventStash::new();
        assert_eq!(stash.push(event(3, 0xcb5e00)), None);
        assert_eq!(stash.take(4), None);
        assert_eq!(stash.take(3), Some(event(3, 0xcb5e00)));
        assert_eq!(stash.take(3), None);
        assert!(stash.is_empty());
    }

    #[test]
    fn events_for_one_slot_come_back_in_arrival_order() {
        let mut stash = TransferEventStash::new();
        stash.push(event(3, 0x10));
        stash.push(event(5, 0x20));
        stash.push(event(3, 0x30));
        assert_eq!(stash.len(), 3);
        assert_eq!(stash.take(3).unwrap().trb_iova, 0x10);
        assert_eq!(stash.take(3).unwrap().trb_iova, 0x30);
        assert_eq!(stash.take(5).unwrap().trb_iova, 0x20);
    }

    #[test]
    fn a_full_stash_gives_up_its_oldest_event() {
        let mut stash = TransferEventStash::new();
        for index in 0..STASH_CAPACITY as u64 {
            assert_eq!(stash.push(event(2, index)), None);
        }
        assert_eq!(stash.push(event(7, 99)), Some(event(2, 0)));
        assert_eq!(stash.len(), STASH_CAPACITY);
        assert_eq!(stash.take(7).unwrap().trb_iova, 99);
        assert_eq!(stash.take(2).unwrap().trb_iova, 1);
    }

    #[test]
    fn order_survives_the_arrival_counter_wrapping() {
        let mut stash = TransferEventStash::new();
        stash.next = u32::MAX - 1;
        stash.push(event(3, 0xa));
        stash.push(event(3, 0xb));
        stash.push(event(3, 0xc));
        assert_eq!(stash.take(3).unwrap().trb_iova, 0xa);
        assert_eq!(stash.take(3).unwrap().trb_iova, 0xb);
        assert_eq!(stash.take(3).unwrap().trb_iova, 0xc);
    }
}
