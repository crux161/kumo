//j545
//j828
//! One bounded mailbox per endpoint, registered before publishing DMA. Each receipt
//! names a nonwrapping software generation. Failed registrations stay quarantined;
//! retirement requires every requested event consumed and no unread event. Raw xHCI
//! events carry addresses, not generations: reuse is allowed only after that completion
//! fence; uncertain rings are never reused here. Recovery is NET4c. — TIMBERDOODLE

use crate::Event;

pub const COMPLETION_LANES: usize = 32;
pub const COMPLETION_TRBS: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteError {
    Invalid,
    Capacity,
    Busy,
    Quarantined,
    Stale,
    Unknown,
    Duplicate,
    Overflow,
    Incomplete,
    GenerationExhausted,
    EventData,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferTicket {
    lane: usize,
    generation: u64,
    slot: u8,
    dci: u8,
}
impl TransferTicket {
    pub const fn lane(self) -> usize {
        self.lane
    }
    pub const fn generation(self) -> u64 {
        self.generation
    }
    pub const fn slot(self) -> u8 {
        self.slot
    }
    pub const fn dci(self) -> u8 {
        self.dci
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletionKey {
    pub slot: u8,
    pub dci: u8,
    pub trb_iova: u64,
    pub generation: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoutedTransfer {
    pub key: CompletionKey,
    pub completion_code: u8,
    pub remaining: u32,
}
impl RoutedTransfer {
    pub const fn event(self) -> Event {
        Event::Transfer {
            slot_id: self.key.slot,
            endpoint_id: self.key.dci,
            trb_iova: self.key.trb_iova,
            completion_code: self.completion_code,
            remaining: self.remaining,
        }
    }
}

#[derive(Clone, Copy)]
struct Lane {
    slot: u8,
    dci: u8,
    generation: u64,
    active: bool,
    fault: Option<RouteError>,
    tokens: [u64; COMPLETION_TRBS],
    count: usize,
    expected: u8,
    seen: u8,
    taken: u8,
    events: [Option<RoutedTransfer>; COMPLETION_TRBS],
    order: [u8; COMPLETION_TRBS],
    next: u8,
}
impl Lane {
    const EMPTY: Self = Self {
        slot: 0,
        dci: 0,
        generation: 0,
        active: false,
        fault: None,
        tokens: [0; 3],
        count: 0,
        expected: 0,
        seen: 0,
        taken: 0,
        events: [None; 3],
        order: [0; 3],
        next: 0,
    };
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompletionStats {
    pub registered: u64,
    pub routed: u64,
    pub consumed: u64,
    pub retired: u64,
    pub faults: u64,
}

pub struct CompletionRouter {
    lanes: [Lane; COMPLETION_LANES],
    next_generation: u64,
    controller_fault: bool,
    stats: CompletionStats,
}
impl CompletionRouter {
    pub const fn new() -> Self {
        Self {
            lanes: [Lane::EMPTY; COMPLETION_LANES],
            next_generation: 1,
            controller_fault: false,
            stats: CompletionStats {
                registered: 0,
                routed: 0,
                consumed: 0,
                retired: 0,
                faults: 0,
            },
        }
    }
    pub const fn stats(&self) -> CompletionStats {
        self.stats
    }
    pub fn ready(&self, slot: u8, dci: u8) -> Result<(), RouteError> {
        if slot == 0 || !(1..=31).contains(&dci) {
            return Err(RouteError::Invalid);
        }
        if self.controller_fault {
            return Err(RouteError::Quarantined);
        }
        if let Some(l) = self.lanes.iter().find(|l| l.slot == slot && l.dci == dci) {
            if l.fault.is_some() {
                return Err(RouteError::Quarantined);
            }
            if l.active {
                return Err(RouteError::Busy);
            }
        } else if self.lanes.iter().all(|l| l.slot != 0) {
            return Err(RouteError::Capacity);
        }
        if self.next_generation == u64::MAX {
            return Err(RouteError::GenerationExhausted);
        }
        Ok(())
    }
    /// `expected` marks the IOC-bearing TRBs. Other owned TRBs may report errors,
    /// but may not silently be forgotten during retirement. — TIMBERDOODLE
    pub fn register(
        &mut self,
        slot: u8,
        dci: u8,
        tokens: &[u64],
        expected: u8,
    ) -> Result<TransferTicket, RouteError> {
        self.ready(slot, dci)?;
        if tokens.is_empty()
            || tokens.len() > COMPLETION_TRBS
            || expected == 0
            || expected >> tokens.len() != 0
        {
            return Err(RouteError::Invalid);
        }
        for (i, &token) in tokens.iter().enumerate() {
            if token == 0
                || token & 15 != 0
                || tokens[..i].contains(&token)
                || self
                    .lanes
                    .iter()
                    .any(|l| l.active && l.tokens[..l.count].contains(&token))
            {
                return Err(RouteError::Invalid);
            }
        }
        let index = self
            .lanes
            .iter()
            .position(|l| l.slot == slot && l.dci == dci)
            .or_else(|| self.lanes.iter().position(|l| l.slot == 0))
            .ok_or(RouteError::Capacity)?;
        let generation = self.next_generation;
        self.next_generation += 1;
        let mut lane = Lane {
            slot,
            dci,
            generation,
            active: true,
            count: tokens.len(),
            expected,
            ..Lane::EMPTY
        };
        lane.tokens[..tokens.len()].copy_from_slice(tokens);
        self.lanes[index] = lane;
        self.stats.registered = self.stats.registered.saturating_add(1);
        Ok(TransferTicket {
            lane: index,
            generation,
            slot,
            dci,
        })
    }
    fn checked(&self, ticket: TransferTicket) -> Result<usize, RouteError> {
        let l = self.lanes.get(ticket.lane).ok_or(RouteError::Stale)?;
        if l.generation != ticket.generation
            || l.slot != ticket.slot
            || l.dci != ticket.dci
            || !l.active
        {
            return Err(RouteError::Stale);
        }
        if self.controller_fault || l.fault.is_some() {
            return Err(RouteError::Quarantined);
        }
        Ok(ticket.lane)
    }
    pub fn fail(&mut self, ticket: TransferTicket, error: RouteError) -> Result<(), RouteError> {
        let i = self.checked(ticket)?;
        self.fault(i, error);
        Ok(())
    }
    fn fault(&mut self, i: usize, error: RouteError) {
        if self.lanes[i].fault.is_none() {
            self.stats.faults = self.stats.faults.saturating_add(1);
        }
        self.lanes[i].fault = Some(error);
    }
    pub fn reject(&mut self, event: Event, error: RouteError) {
        if let Event::Transfer {
            slot_id,
            endpoint_id,
            ..
        } = event
        {
            if let Some(i) = self
                .lanes
                .iter()
                .position(|l| l.slot == slot_id && l.dci == endpoint_id && l.slot != 0)
            {
                self.fault(i, error);
                return;
            }
        }
        self.controller_fault = true;
        self.stats.faults = self.stats.faults.saturating_add(1);
    }
    /// Stamp at the sole hardware event consumer; already deferred work keeps this
    /// receipt generation instead of looking up a later registration. — TIMBERDOODLE
    pub fn route(&mut self, event: Event) -> Result<Option<usize>, RouteError> {
        let Event::Transfer {
            slot_id,
            endpoint_id,
            trb_iova,
            completion_code,
            remaining,
        } = event
        else {
            return Ok(None);
        };
        if slot_id == 0 || !(1..=31).contains(&endpoint_id) {
            self.controller_fault = true;
            self.stats.faults = self.stats.faults.saturating_add(1);
            return Err(RouteError::Invalid);
        }
        let index = self
            .lanes
            .iter()
            .position(|l| l.slot == slot_id && l.dci == endpoint_id);
        let Some(i) = index else {
            if let Some(i) = self.lanes.iter().position(|l| l.slot == 0) {
                self.lanes[i] = Lane {
                    slot: slot_id,
                    dci: endpoint_id,
                    ..Lane::EMPTY
                };
                self.fault(i, RouteError::Unknown);
            } else {
                self.controller_fault = true;
                self.stats.faults = self.stats.faults.saturating_add(1);
            }
            return Err(RouteError::Unknown);
        };
        if !self.lanes[i].active {
            self.fault(i, RouteError::Unknown);
            return Err(RouteError::Unknown);
        }
        let event = RoutedTransfer {
            key: CompletionKey {
                slot: slot_id,
                dci: endpoint_id,
                trb_iova,
                generation: self.lanes[i].generation,
            },
            completion_code,
            remaining,
        };
        self.route_tagged(event).map(Some)
    }
    pub fn route_tagged(&mut self, event: RoutedTransfer) -> Result<usize, RouteError> {
        let Some(i) = self
            .lanes
            .iter()
            .position(|l| l.slot == event.key.slot && l.dci == event.key.dci)
        else {
            return Err(RouteError::Stale);
        };
        let l = &self.lanes[i];
        if !l.active || l.generation != event.key.generation {
            return Err(RouteError::Stale);
        }
        if self.controller_fault || l.fault.is_some() {
            return Err(RouteError::Quarantined);
        }
        let Some(n) = l.tokens[..l.count]
            .iter()
            .position(|&token| token == event.key.trb_iova)
        else {
            let error = if l.seen.count_ones() as usize == COMPLETION_TRBS {
                RouteError::Overflow
            } else {
                RouteError::Unknown
            };
            self.fault(i, error);
            return Err(error);
        };
        if l.seen & (1 << n) != 0 {
            self.fault(i, RouteError::Duplicate);
            return Err(RouteError::Duplicate);
        }
        let l = &mut self.lanes[i];
        l.events[n] = Some(event);
        l.order[n] = l.next;
        l.next += 1;
        l.seen |= 1 << n;
        self.stats.routed = self.stats.routed.saturating_add(1);
        Ok(i)
    }
    pub fn has_ready(&self, ticket: TransferTicket) -> Result<bool, RouteError> {
        let i = self.checked(ticket)?;
        Ok(self.lanes[i].seen & !self.lanes[i].taken != 0)
    }
    pub fn take(&mut self, ticket: TransferTicket) -> Result<Option<RoutedTransfer>, RouteError> {
        let i = self.checked(ticket)?;
        let l = &mut self.lanes[i];
        let n = (0..l.count)
            .filter(|n| l.seen & (1 << n) != 0 && l.taken & (1 << n) == 0)
            .min_by_key(|&n| l.order[n]);
        let Some(n) = n else {
            return Ok(None);
        };
        l.taken |= 1 << n;
        self.stats.consumed = self.stats.consumed.saturating_add(1);
        Ok(l.events[n])
    }
    /// Completion fence only: caller must validate status/residue before reclaiming
    /// its ring. There is no finish/cancel path for uncertain transfers. — TIMBERDOODLE
    pub fn finish(&mut self, ticket: TransferTicket) -> Result<(), RouteError> {
        let i = self.checked(ticket)?;
        let l = &self.lanes[i];
        if l.taken & l.expected != l.expected || l.seen != l.taken {
            return Err(RouteError::Incomplete);
        }
        if l.events
            .iter()
            .flatten()
            .any(|e| !matches!(e.completion_code, 1 | 13))
        {
            self.fault(i, RouteError::Unknown);
            return Err(RouteError::Quarantined);
        }
        self.lanes[i].active = false;
        self.stats.retired = self.stats.retired.saturating_add(1);
        Ok(())
    }
}
impl Default for CompletionRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
