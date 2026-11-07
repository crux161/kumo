//j829
//! Private parent/driver DMA custody protocol. The parent's imported VMO handle is
//! kept until reset; ACK authorizes publication, not confinement or safe release.
//! Stateless wire codec, fixed parent ledger, no allocation or syscalls. — TIMBERDOODLE

pub const DMA_CUSTODY_LEN: usize = 32;
pub const DMA_ACK_LEN: usize = 8;
pub const DMA_CUSTODY_MAX: usize = 32;
pub const DMA_ALLOCATION_MAX: u64 = 4 * 1024 * 1024;
pub const DMA_TOTAL_MAX: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CustodyError {
    Frame,
    Alignment,
    Overflow,
    Controller,
    Sequence,
    Handle,
    Capacity,
    Ack,
    Overlap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaMemory {
    pub sequence: u32,
    pub bytes: u64,
    pub physical: u64,
    pub controller: u64,
}
impl DmaMemory {
    pub fn validate(&self) -> Result<(), CustodyError> {
        if self.sequence == 0 {
            return Err(CustodyError::Sequence);
        }
        if self.bytes == 0
            || self.bytes > DMA_ALLOCATION_MAX
            || self.bytes & 4095 != 0
            || self.physical == 0
            || self.physical & 4095 != 0
        {
            return Err(CustodyError::Alignment);
        }
        if self.physical.checked_add(self.bytes).is_none() {
            return Err(CustodyError::Overflow);
        }
        if self.controller == 0 || self.controller & 4095 != 0 {
            return Err(CustodyError::Controller);
        }
        Ok(())
    }
    pub fn encode(self) -> [u8; DMA_CUSTODY_LEN] {
        let mut out = [0; DMA_CUSTODY_LEN];
        out[..4].copy_from_slice(b"XDM1");
        out[4..8].copy_from_slice(&self.sequence.to_le_bytes());
        out[8..16].copy_from_slice(&self.bytes.to_le_bytes());
        out[16..24].copy_from_slice(&self.physical.to_le_bytes());
        out[24..32].copy_from_slice(&self.controller.to_le_bytes());
        out
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, CustodyError> {
        if bytes.len() != DMA_CUSTODY_LEN || bytes[..4] != *b"XDM1" {
            return Err(CustodyError::Frame);
        }
        let memory = Self {
            sequence: u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| CustodyError::Frame)?),
            bytes: u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| CustodyError::Frame)?),
            physical: u64::from_le_bytes(
                bytes[16..24].try_into().map_err(|_| CustodyError::Frame)?,
            ),
            controller: u64::from_le_bytes(
                bytes[24..32].try_into().map_err(|_| CustodyError::Frame)?,
            ),
        };
        memory.validate()?;
        Ok(memory)
    }
    pub fn acknowledge(self, ack: &[u8]) -> Result<Self, CustodyError> {
        self.validate()?;
        if ack.len() != DMA_ACK_LEN
            || ack[..4] != *b"XDA1"
            || ack[4..8] != self.sequence.to_le_bytes()
        {
            return Err(CustodyError::Ack);
        }
        Ok(self)
    }
}

#[derive(Clone, Copy)]
struct Held {
    memory: DmaMemory,
    handle: u32,
}

/// Accepted handles remain in the parent's handle table, even after peer death or
/// failed ACK delivery. No release/reset method exists before quiescence. — TIMBERDOODLE
pub struct DmaCustodyLedger {
    controller: u64,
    held: [Option<Held>; DMA_CUSTODY_MAX],
    count: usize,
    total: u64,
}
impl DmaCustodyLedger {
    pub const fn new() -> Self {
        Self {
            controller: 0,
            held: [None; DMA_CUSTODY_MAX],
            count: 0,
            total: 0,
        }
    }
    pub fn configure(&mut self, controller: u64) -> Result<(), CustodyError> {
        if controller == 0 || controller & 4095 != 0 {
            return Err(CustodyError::Controller);
        }
        if self.controller != 0 {
            return Err(CustodyError::Controller);
        }
        self.controller = controller;
        Ok(())
    }
    pub const fn count(&self) -> usize {
        self.count
    }
    pub const fn bytes(&self) -> u64 {
        self.total
    }
    pub fn contains_handle(&self, handle: u32) -> bool {
        self.held.iter().flatten().any(|h| h.handle == handle)
    }
    pub fn accept(
        &mut self,
        memory: DmaMemory,
        handle: u32,
    ) -> Result<[u8; DMA_ACK_LEN], CustodyError> {
        memory.validate()?;
        if memory.controller != self.controller {
            return Err(CustodyError::Controller);
        }
        if memory.sequence as usize != self.count + 1 {
            return Err(CustodyError::Sequence);
        }
        if handle == 0 || self.contains_handle(handle) {
            return Err(CustodyError::Handle);
        }
        let total = self
            .total
            .checked_add(memory.bytes)
            .ok_or(CustodyError::Overflow)?;
        if self.count == DMA_CUSTODY_MAX || total > DMA_TOTAL_MAX {
            return Err(CustodyError::Capacity);
        }
        let end = memory.physical + memory.bytes;
        if self.held.iter().flatten().any(|h| {
            memory.physical < h.memory.physical + h.memory.bytes && h.memory.physical < end
        }) {
            return Err(CustodyError::Overlap);
        }
        self.held[self.count] = Some(Held { memory, handle });
        self.count += 1;
        self.total = total;
        let mut ack = [0; DMA_ACK_LEN];
        ack[..4].copy_from_slice(b"XDA1");
        ack[4..].copy_from_slice(&memory.sequence.to_le_bytes());
        Ok(ack)
    }
}
impl Default for DmaCustodyLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
