//j828
//j830
//! In-process endpoint actors on the one controller owner. Each lane has a bounded
//! serial notification queue and a fixed pure completion mailbox. Workers are only
//! polled with try_run_one; this adapter never parks or calls another actor.
//! Recovery class: stateless; quarantined DMA remains with the owner. — TIMBERDOODLE

use kumo_dispatch::SerialQueue;
use kumo_xhci::{
    CompletionRouter, CompletionStats, Event, RingRestart, RouteError, RoutedTransfer,
    TransferTicket, COMPLETION_LANES,
};

pub struct CompletionActors {
    router: CompletionRouter,
    queues: [SerialQueue<1>; COMPLETION_LANES],
}
fn ready(index: usize) -> usize {
    index
}
impl CompletionActors {
    pub const fn new() -> Self {
        Self {
            router: CompletionRouter::new(),
            queues: [const { SerialQueue::new() }; COMPLETION_LANES],
        }
    }
    pub fn ready(&self, slot: u8, dci: u8) -> Result<(), RouteError> {
        self.router.ready(slot, dci)
    }
    pub fn register(
        &mut self,
        slot: u8,
        dci: u8,
        tokens: &[u64],
        expected: u8,
    ) -> Result<TransferTicket, RouteError> {
        self.router.register(slot, dci, tokens, expected)
    }
    pub fn route_trb(&mut self, trb: kumo_xhci::Trb) -> Result<Option<Event>, RouteError> {
        let Some(event) = trb.decode_event() else {
            self.router.reject(
                Event::HostController { completion_code: 0 },
                RouteError::Invalid,
            );
            return Err(RouteError::Invalid);
        };
        if matches!(event, Event::Transfer { .. }) {
            let error = if trb.words()[3] & (1 << 2) != 0 {
                Some(RouteError::EventData)
            } else if trb.pointer() & 15 != 0 {
                Some(RouteError::Invalid)
            } else {
                None
            };
            if let Some(error) = error {
                self.router.reject(event, error);
                return Err(error);
            }
            self.route(event)?;
        } else if matches!(event, Event::HostController { .. }) {
            self.router.reject(event, RouteError::Unknown);
        }
        Ok(Some(event))
    }
    pub fn route(&mut self, event: Event) -> Result<(), RouteError> {
        let Some(index) = self.router.route(event)? else {
            return Ok(());
        };
        // Coalesce readiness, not event records; the mailbox preserves each event.
        // A full notification queue means this same serial lane is already runnable.
        // — TIMBERDOODLE
        if self.queues[index].is_empty() {
            if self.queues[index].submit_async(ready, index).is_err() {
                self.router.reject(event, RouteError::Overflow);
                return Err(RouteError::Overflow);
            }
        }
        Ok(())
    }
    pub fn take(&mut self, ticket: TransferTicket) -> Result<Option<RoutedTransfer>, RouteError> {
        if !self.router.has_ready(ticket)? {
            return Ok(None);
        }
        let queue = &self.queues[ticket.lane()];
        let worker = queue.worker().map_err(|_| RouteError::Busy)?;
        let result = worker.try_run_one();
        drop(worker);
        if result != Some(ticket.lane()) {
            self.router.fail(ticket, RouteError::Overflow)?;
            return Err(RouteError::Overflow);
        }
        let event = self.router.take(ticket)?;
        if self.router.has_ready(ticket)? {
            if queue.submit_async(ready, ticket.lane()).is_err() {
                self.router.fail(ticket, RouteError::Overflow)?;
                return Err(RouteError::Overflow);
            }
        }
        Ok(event)
    }
    pub fn finish(&mut self, ticket: TransferTicket) -> Result<(), RouteError> {
        self.router.has_ready(ticket)?;
        if !self.queues[ticket.lane()].is_empty() {
            return Err(RouteError::Incomplete);
        }
        self.router.finish(ticket)
    }
    pub fn fail(&mut self, ticket: TransferTicket, error: RouteError) {
        let _ = self.router.fail(ticket, error);
    }
    pub fn has_ready(&self, ticket: TransferTicket) -> Result<bool, RouteError> {
        self.router.has_ready(ticket)
    }
    pub fn reject(&mut self, event: Event, error: RouteError) {
        self.router.reject(event, error);
    }
    pub fn stats(&self) -> CompletionStats {
        self.router.stats()
    }
    pub fn begin_stop(
        &mut self,
        ticket: TransferTicket,
        restart: RingRestart,
    ) -> Result<(), RouteError> {
        self.router.begin_stop(ticket, restart)
    }
    pub fn register_stop(
        &mut self,
        slot: u8,
        dci: u8,
        restart: RingRestart,
    ) -> Result<TransferTicket, RouteError> {
        self.router.register_stop(slot, dci, restart)
    }
    pub fn stop_event(&self, ticket: TransferTicket) -> Result<Option<RoutedTransfer>, RouteError> {
        self.router.stop_event(ticket)
    }
    /// A stopped event leaves only a wake token in the lane's serial queue; consume it
    /// so the retired lane starts its next transfer with an empty queue. Unread normal
    /// events still refuse. — TIMBERDOODLE
    pub fn cancel(&mut self, ticket: TransferTicket) -> Result<(), RouteError> {
        if self.router.has_ready(ticket)? {
            return Err(RouteError::Incomplete);
        }
        let queue = &self.queues[ticket.lane()];
        if !queue.is_empty() {
            let worker = queue.worker().map_err(|_| RouteError::Busy)?;
            let result = worker.try_run_one();
            drop(worker);
            if result != Some(ticket.lane()) || !queue.is_empty() {
                self.router.fail(ticket, RouteError::Overflow)?;
                return Err(RouteError::Overflow);
            }
        }
        self.router.cancel(ticket)
    }
    pub fn occupied(&self) -> usize {
        self.router.occupied()
    }
}
impl Default for CompletionActors {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(slot: u8, dci: u8, ptr: u64) -> Event {
        Event::Transfer {
            slot_id: slot,
            endpoint_id: dci,
            trb_iova: ptr,
            completion_code: 1,
            remaining: 0,
        }
    }
    #[test]
    fn device_lane_actors_coalesce_wakes_without_coalescing_completions() {
        let mut a = CompletionActors::new();
        let ep0 = a.register(5, 1, &[0x1000, 0x1010, 0x1020], 6).unwrap();
        let hid = a.register(4, 3, &[0x2000], 1).unwrap();
        a.route(event(5, 1, 0x1010)).unwrap();
        a.route(event(4, 3, 0x2000)).unwrap();
        a.route(event(5, 1, 0x1020)).unwrap();
        assert_eq!(a.queues[ep0.lane()].len(), 1);
        assert_eq!(a.take(ep0).unwrap().unwrap().key.trb_iova, 0x1010);
        assert!(a.take(hid).unwrap().is_some());
        a.finish(hid).unwrap();
        assert_eq!(a.take(ep0).unwrap().unwrap().key.trb_iova, 0x1020);
        a.finish(ep0).unwrap();
        assert_eq!(a.take(ep0), Err(RouteError::Stale));
    }
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    const RESTART: RingRestart = RingRestart {
        dequeue_iova: 0x1010,
        cycle: true,
        link_iova: None,
    };
    fn coded(ptr: u64, completion_code: u8) -> Event {
        Event::Transfer {
            slot_id: 5,
            endpoint_id: 3,
            trb_iova: ptr,
            completion_code,
            remaining: 0,
        }
    }
    #[test]
    fn cancel_consumes_the_stop_wake_and_leaves_the_lane_reusable() {
        let mut a = CompletionActors::new();
        for _ in 0..3 {
            let t = a.register(5, 3, &[0x1000], 1).unwrap();
            a.begin_stop(t, RESTART).unwrap();
            a.route(coded(0x1000, 26)).unwrap();
            assert_eq!(a.queues[t.lane()].len(), 1);
            assert_eq!(a.take(t), Ok(None));
            a.cancel(t).unwrap();
            assert!(a.queues[t.lane()].is_empty());
        }
        let probe = a.register_stop(5, 3, RESTART).unwrap();
        a.route(coded(0x1010, 27)).unwrap();
        assert!(a.stop_event(probe).unwrap().is_some());
        a.cancel(probe).unwrap();
        assert_eq!(
            (a.stats().cancelled, a.stats().faults, a.occupied()),
            (4, 0, 1)
        );
    }
    #[test]
    fn cancel_refuses_while_a_racing_completion_is_unread() {
        let mut a = CompletionActors::new();
        let t = a.register(5, 3, &[0x1000], 1).unwrap();
        a.route(coded(0x1000, 1)).unwrap();
        a.begin_stop(t, RESTART).unwrap();
        a.route(coded(0x1010, 27)).unwrap();
        assert_eq!(a.cancel(t), Err(RouteError::Incomplete));
        assert_eq!(a.take(t).unwrap().unwrap().completion_code, 1);
        a.cancel(t).unwrap();
        assert!(a.queues[t.lane()].is_empty());
    }
}

#[cfg(test)]
mod raw_tests {
    use super::*;
    #[test]
    fn event_data_or_reserved_pointer_bits_cannot_alias_a_registered_trb() {
        for (pointer, flags, error) in [
            (0x1000, 4, RouteError::EventData),
            (0x1001, 0, RouteError::Invalid),
        ] {
            let mut a = CompletionActors::new();
            let t = a.register(5, 5, &[0x1000], 1).unwrap();
            let raw = kumo_xhci::Trb::from_words([
                pointer,
                0,
                1 << 24,
                (32 << 10) | (5 << 16) | (5 << 24) | flags,
            ]);
            assert_eq!(a.route_trb(raw), Err(error));
            assert_eq!(a.take(t), Err(RouteError::Quarantined));
            assert_eq!(a.ready(5, 5), Err(RouteError::Quarantined));
        }
    }
    #[test]
    fn unknown_event_type_retains_all_controller_lanes() {
        let mut a = CompletionActors::new();
        let t = a.register(5, 5, &[0x1000], 1).unwrap();
        assert_eq!(
            a.route_trb(kumo_xhci::Trb::zero()),
            Err(RouteError::Invalid)
        );
        assert_eq!(a.take(t), Err(RouteError::Quarantined));
        assert_eq!(a.ready(4, 3), Err(RouteError::Quarantined));
    }
}
