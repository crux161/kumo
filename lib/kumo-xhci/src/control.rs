//j827
//! Bounded exact-length EP0 data transfers. Stateless requests; one pipe owns one
//! outstanding transfer. A failed pipe has no reset/reuse API: the controller owner
//! retains its ring and buffer until separately proved quiescent. — TIMBERDOODLE

use crate::{Error, Event, SetupTransferType, TransferRing, Trb, TrbSink};

pub const CONTROL_DATA_MAX: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlError {
    Length,
    Payload,
    Address,
    Slot,
    Deadline,
    Busy,
    Quarantined,
    NoPending,
    Unexpected,
    Order,
    Completion(u8),
    Residual(u32),
    Timeout,
    EventOverflow,
    Ring(Error),
}

#[derive(Debug, Eq, PartialEq)]
pub struct ControlData<'a> {
    trbs: [Trb; 3],
    length: usize,
    outbound: &'a [u8],
    direction_in: bool,
}

impl<'a> ControlData<'a> {
    pub fn new(
        setup: [u8; 8],
        buffer_iova: u64,
        buffer_capacity: usize,
        outbound: &'a [u8],
    ) -> Result<Self, ControlError> {
        let length = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
        if length == 0 || length > CONTROL_DATA_MAX || length > buffer_capacity {
            return Err(ControlError::Length);
        }
        let direction_in = setup[0] & 0x80 != 0;
        if (direction_in && !outbound.is_empty()) || (!direction_in && outbound.len() != length) {
            return Err(ControlError::Payload);
        }
        if buffer_iova == 0 || buffer_iova.checked_add(length as u64 - 1).is_none() {
            return Err(ControlError::Address);
        }
        let setup = Trb::setup_stage(
            setup,
            if direction_in {
                SetupTransferType::In
            } else {
                SetupTransferType::Out
            },
            0,
        )
        .map_err(ControlError::Ring)?;
        let mut data = Trb::data_stage(buffer_iova, length as u32, 0, 0, direction_in)
            .map_err(ControlError::Ring)?
            .words();
        // Each stage is its own TD, not chained to Status. Data IOC+ISP exposes the
        // actual residue, including short packets; Status IOC proves the handshake.
        // xHCI 4.10.1 permits multiple IOC events (6.4.1.2 recommends Status only).
        // This intentionally matches NET0's two-event exact-read proof. — TIMBERDOODLE
        data[3] = (data[3] & !(1 << 4)) | (1 << 5) | (1 << 2);
        let status = Trb::status_stage(0, !direction_in, true).map_err(ControlError::Ring)?;
        Ok(Self {
            trbs: [setup, Trb::from_words(data), status],
            length,
            outbound,
            direction_in,
        })
    }
    pub const fn length(&self) -> usize {
        self.length
    }
    pub const fn direction_in(&self) -> bool {
        self.direction_in
    }
    pub const fn outbound(&self) -> &[u8] {
        self.outbound
    }
    pub const fn trbs(&self) -> [Trb; 3] {
        self.trbs
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlProgress {
    Waiting,
    Complete(usize),
    Unrelated(Event),
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    tokens: [u64; 3],
    length: usize,
    end_ns: u64,
    data_done: bool,
}

#[derive(Debug)]
pub struct ControlPipe {
    slot: u8,
    pending: Option<Pending>,
    quarantined: bool,
}

impl ControlPipe {
    pub fn new(slot: u8) -> Result<Self, ControlError> {
        if slot == 0 {
            return Err(ControlError::Slot);
        }
        Ok(Self {
            slot,
            pending: None,
            quarantined: false,
        })
    }
    pub fn is_idle(&self) -> bool {
        !self.quarantined && self.pending.is_none()
    }
    pub const fn is_quarantined(&self) -> bool {
        self.quarantined
    }
    /// Preflight must precede writes to a DMA buffer or publishing any ring entry.
    pub fn preflight<S: TrbSink + ?Sized>(
        &self,
        ring: &TransferRing,
        segment: &S,
        now_ns: u64,
        budget_ns: u64,
    ) -> Result<(), ControlError> {
        if self.quarantined {
            return Err(ControlError::Quarantined);
        }
        if self.pending.is_some() || ring.queued() != 0 {
            return Err(ControlError::Busy);
        }
        if budget_ns == 0 || now_ns.checked_add(budget_ns).is_none() {
            return Err(ControlError::Deadline);
        }
        if segment.trb_slots() != ring.capacity() + 1 {
            return Err(ControlError::Ring(Error::RingMemoryLength));
        }
        if ring.capacity() < 3 {
            return Err(ControlError::Ring(Error::RingFull));
        }
        Ok(())
    }
    pub fn submit<S: TrbSink + ?Sized>(
        &mut self,
        request: &ControlData<'_>,
        ring: &mut TransferRing,
        segment: &mut S,
        now_ns: u64,
        budget_ns: u64,
    ) -> Result<(), ControlError> {
        self.preflight(ring, segment, now_ns, budget_ns)?;
        let mut tokens = [0; 3];
        for (index, trb) in request.trbs.into_iter().enumerate() {
            // Preflight established space/type/backing for all three entries. Still
            // quarantine on an unexpected enqueue error; never roll back published DMA.
            // — TIMBERDOODLE
            match ring.enqueue(segment, trb) {
                Ok(token) => tokens[index] = token.iova,
                Err(error) => return self.fail(ControlError::Ring(error)),
            }
        }
        self.pending = Some(Pending {
            tokens,
            length: request.length,
            end_ns: now_ns + budget_ns,
            data_done: false,
        });
        Ok(())
    }
    pub fn check_deadline(&mut self, now_ns: u64) -> Result<(), ControlError> {
        if self.quarantined {
            return Err(ControlError::Quarantined);
        }
        if self.pending.is_some_and(|p| now_ns >= p.end_ns) {
            return self.fail(ControlError::Timeout);
        }
        Ok(())
    }
    pub fn observe(
        &mut self,
        event: Event,
        now_ns: u64,
        ring: &mut TransferRing,
    ) -> Result<ControlProgress, ControlError> {
        self.check_deadline(now_ns)?;
        let Some(mut p) = self.pending else {
            return self.fail(ControlError::NoPending);
        };
        let Event::Transfer {
            trb_iova,
            remaining,
            completion_code,
            endpoint_id,
            slot_id,
        } = event
        else {
            return Ok(ControlProgress::Unrelated(event));
        };
        if slot_id != self.slot || endpoint_id != 1 {
            return Ok(ControlProgress::Unrelated(event));
        }
        if !p.tokens.contains(&trb_iova) {
            return self.fail(ControlError::Unexpected);
        }
        if completion_code != 1 {
            return self.fail(ControlError::Completion(completion_code));
        }
        if remaining != 0 {
            return self.fail(ControlError::Residual(remaining));
        }
        if trb_iova == p.tokens[1] {
            if p.data_done {
                return self.fail(ControlError::Order);
            }
            p.data_done = true;
            self.pending = Some(p);
            return Ok(ControlProgress::Waiting);
        }
        if trb_iova != p.tokens[2] || !p.data_done {
            return self.fail(ControlError::Order);
        }
        if ring.queued() != 3 {
            return self.fail(ControlError::Busy);
        }
        match ring.reclaim_through(p.tokens[2]) {
            Ok(3) => {}
            Ok(_) => return self.fail(ControlError::Unexpected),
            Err(error) => return self.fail(ControlError::Ring(error)),
        }
        self.pending = None;
        Ok(ControlProgress::Complete(p.length))
    }
    /// Failure is terminal for this pipe. No completion/timeout alone authorizes DMA reuse.
    pub fn fail<T>(&mut self, error: ControlError) -> Result<T, ControlError> {
        self.quarantined = true;
        Err(error)
    }
}

#[cfg(test)]
mod tests;
