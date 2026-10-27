#![no_std]
//j594
//j595
//j596
//j641
//j643

//! G6a's GPU channel contract (PLAN/013), shared by clients and drv-gpu. Recovery class:
//! soft-state. One channel owns one VM, one group/queue and its completion timeline; IDs are
//! local to that channel and become invalid when it closes. Reconnect and recreate after a
//! driver restart; never replay a Submit whose outcome is unknown.
//!
//! BufferCreate allocates DMA backing in the driver and returns a shared VMO handle with the
//! reply. Requests carry no handles or physical addresses. The transport must enforce those
//! handle counts, resolve IDs in this session, retain backing until the GPU is quiescent and
//! reject overlapping binds. Bind before GroupCreate; mappings remain fixed until close.
//! This library validates frames and arithmetic, not kernel rights or the server object table.
//! Clients stop writing bound buffers before Submit and reuse them after completion. This
//! cooperative first lane does not claim isolation from hostile command streams.
//!
//! G6b serves this contract from drv-gpu for one brokered client per boot.

pub mod c;
pub mod client;
pub mod session;
pub mod wire;
pub use wire::{Command, Reply, Request, Response, REPLY_LEN, REQUEST_LEN};

pub const PAGE_BYTES: u64 = 4096;
/// First lane: a 16 MiB client window, separate from G5's resident work buffers.
pub const VA_START: u64 = 0x2000_0000;
pub const VA_END: u64 = 0x2100_0000;
pub const MAX_BUFFER_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_STREAM_BYTES: u32 = 64 * 1024;
pub const RING_BYTES: u32 = 4096;

/// Read-only hardware identity and command-stream capabilities. No address, handle or
/// firmware interface pointer crosses this query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    pub gpu_id: u32,
    pub cs_features: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Status {
    BadRequest = 1,
    BadState = 2,
    NotFound = 3,
    Busy = 4,
    NoMemory = 5,
    DeviceLost = 6,
    NotSupported = 7,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BindFlags(pub u32);

impl BindFlags {
    pub const READ_ONLY: u32 = 1;
    pub const NO_EXECUTE: u32 = 2;
    pub const UNCACHED: u32 = 4;

    pub const fn contains(self, flag: u32) -> bool {
        self.0 & flag == flag
    }

    pub const fn valid(self) -> bool {
        self.0 & !7 == 0 && (self.contains(Self::READ_ONLY) || self.contains(Self::NO_EXECUTE))
    }
}

pub const fn valid_buffer_bytes(bytes: u64) -> bool {
    bytes > 0 && bytes <= MAX_BUFFER_BYTES && bytes % PAGE_BYTES == 0
}

/// A client-visible binding. The driver resolves `buffer` to its own DMA allocation; neither
/// this request nor its reply exposes the physical backing. W^X applies to GPU shader pages;
/// CSF command streams are read as data and may be bound no-execute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Binding {
    pub vm: u32,
    pub buffer: u32,
    pub va: u64,
    pub offset: u64,
    pub bytes: u64,
    pub flags: BindFlags,
}

impl Binding {
    pub fn validate(self, buffer_bytes: u64) -> Result<(), Status> {
        if self.vm == 0
            || self.buffer == 0
            || !self.flags.valid()
            || self.va % PAGE_BYTES != 0
            || self.offset % PAGE_BYTES != 0
            || !valid_buffer_bytes(self.bytes)
            || !valid_buffer_bytes(buffer_bytes)
            || !in_window(self.va, self.bytes)
            || self
                .offset
                .checked_add(self.bytes)
                .is_none_or(|end| end > buffer_bytes)
        {
            return Err(Status::BadRequest);
        }
        Ok(())
    }

    /// Call only on validated bindings. Adjacent mappings are allowed; aliases in one GPU
    /// address range are not. A buffer may have multiple disjoint mappings.
    pub fn overlaps(self, other: Self) -> bool {
        self.vm == other.vm
            && self.va < other.va.saturating_add(other.bytes)
            && other.va < self.va.saturating_add(self.bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Submission {
    pub group: u32,
    pub stream_va: u64,
    pub stream_bytes: u32,
    /// Must be exactly the group's next timeline point. Zero is never a job.
    pub signal_point: u64,
}

impl Submission {
    pub fn valid_shape(self) -> bool {
        self.group != 0
            && self.signal_point != 0
            && self.stream_va % 64 == 0
            && self.stream_bytes > 0
            && self.stream_bytes <= MAX_STREAM_BYTES
            && self.stream_bytes % 8 == 0
            && in_window(self.stream_va, u64::from(self.stream_bytes))
    }

    /// The server supplies the group's VM and a previously validated binding from this same
    /// session. The entire stream must fit one read-only binding. The driver samples its own
    /// latest flush ID and owns the ring, wrapper and sync object. — TIMBERDOODLE
    pub fn validate_for(self, group_vm: u32, binding: Binding) -> Result<(), Status> {
        binding.validate(MAX_BUFFER_BYTES)?;
        let stream_end = self
            .stream_va
            .checked_add(u64::from(self.stream_bytes))
            .ok_or(Status::BadRequest)?;
        let binding_end = binding
            .va
            .checked_add(binding.bytes)
            .ok_or(Status::BadRequest)?;
        if !self.valid_shape()
            || binding.vm != group_vm
            || !binding.flags.contains(BindFlags::READ_ONLY)
            || self.stream_va < binding.va
            || stream_end > binding_end
        {
            return Err(Status::BadRequest);
        }
        Ok(())
    }
}

fn in_window(va: u64, bytes: u64) -> bool {
    va >= VA_START && va.checked_add(bytes).is_some_and(|end| end <= VA_END)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncState {
    Pending,
    Complete { gpu_status: u32 },
}

/// Consume a decoded request ID before executing its mutation, including failed operations.
/// This prevents accidental replay within one live session; it is not restart deduplication.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RequestIds {
    last: u32,
}

impl RequestIds {
    pub fn accept(&mut self, id: u32) -> Result<(), Status> {
        if id == 0 || id <= self.last {
            return Err(Status::BadRequest);
        }
        self.last = id;
        Ok(())
    }
}

/// One in-flight job. Submit acceptance reserves a point; only the driver's completion path
/// advances it. Completion means GPU writes are visible to the CPU, after JOB/seqno/status
/// handling and the stream's cache clean. An error retires the group until reconnect.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Timeline {
    submitted: u64,
    completed: u64,
    gpu_status: u32,
}

impl Timeline {
    pub fn reserve(&mut self, point: u64) -> Result<(), Status> {
        if self.gpu_status != 0 {
            return Err(Status::DeviceLost);
        }
        if self.submitted != self.completed {
            return Err(Status::Busy);
        }
        if self.submitted.checked_add(1) != Some(point) {
            return Err(Status::BadRequest);
        }
        self.submitted = point;
        Ok(())
    }

    /// Roll back allocation/validation failure BEFORE ring publication. After any possible
    /// GPU visibility, retain the reservation/backing until completion or reset instead.
    pub fn abort_unpublished(&mut self, point: u64) -> Result<(), Status> {
        if point != self.submitted || self.submitted == self.completed {
            return Err(Status::BadState);
        }
        self.submitted = self.completed;
        Ok(())
    }

    pub fn complete(&mut self, point: u64, gpu_status: u32) -> Result<(), Status> {
        if point != self.submitted || self.submitted == self.completed {
            return Err(Status::BadState);
        }
        self.completed = point;
        self.gpu_status = gpu_status;
        Ok(())
    }

    pub fn query(self, point: u64) -> Result<SyncState, Status> {
        if point == 0 || point > self.submitted {
            return Err(Status::NotFound);
        }
        if point > self.completed {
            return Ok(SyncState::Pending);
        }
        Ok(SyncState::Complete {
            gpu_status: if point == self.completed {
                self.gpu_status
            } else {
                0
            },
        })
    }
}

#[cfg(test)]
mod tests;
