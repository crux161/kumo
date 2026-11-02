#![no_std]
//j744
//j746
//j749
//j751
//j752

//! G6a's GPU channel contract (PLAN/013), shared by clients and drv-gpu. Recovery class:
//! soft-state. One channel owns one VM, one group/queue and its completion timeline; IDs are
//! local to that channel and become invalid when it closes. Reconnect and recreate after a
//! driver restart; never replay a Submit whose outcome is unknown.
//!
//! BufferCreate allocates DMA backing in the driver on demand, bounded by a total live-memory
//! budget rather than a fixed object count, and returns a shared VMO handle with the reply.
//! BufferDestroy releases one allocation: the session first unbinds every GPU VA mapping that
//! names it, then the backend releases the backing. Creation, destruction and their implicit
//! unbind batches are legal after GroupCreate: the driver orders same-address-space work and
//! flushes the space, while explicit VmBind and VmUnbind (opcode 15) narrow the invalidation
//! to the packed page-table region (RESEARCH/017) without stopping any queue.
//! Requests carry no handles or physical addresses. The transport must enforce those
//! handle counts, resolve IDs in this session, retain backing until the GPU is quiescent and
//! reject overlapping binds. Unbind names an exact tiling of live bindings and never exposes
//! backing the GPU may still reach.
//! This library validates frames and arithmetic, not kernel rights or the server object table.
//! Clients stop writing bound buffers before Submit and reuse them after completion. This
//! cooperative first lane does not claim isolation from hostile command streams.
//!
//! G6b serves this contract from drv-gpu for one brokered client per boot.

extern crate alloc;

pub mod bo;
pub mod c;
pub mod c_sync;
pub mod client;
pub mod group;
pub mod session;
pub mod sync;
pub mod wire;
pub use group::{GroupReport, GroupState, GroupWatch};
pub use wire::{Command, Reply, Request, Response, REPLY_LEN, REQUEST_LEN};

pub const PAGE_BYTES: u64 = 4096;
/// A 256 MiB client window, separate from G5's resident work buffers.
pub const VA_START: u64 = 0x2000_0000;
pub const VA_END: u64 = 0x3000_0000;
// Valhall's blend descriptor keeps only the low 32 bits of a blend shader's PC, so every
// executable binding a client makes must share one 4 GiB region (G7o-e). Panthor's auto-VA
// enforces it with a split heap; here the whole window is one region. — TIMBERDOODLE
const _: () = assert!(VA_START >> 32 == (VA_END - 1) >> 32);
/// The largest single allocation. A buffer's GPU binding must fit wholly inside this length.
pub const MAX_BUFFER_BYTES: u64 = 64 * 1024 * 1024;
/// Live DMA bytes one channel may hold at once. G8a removes the fixed four-entry table: a
/// BufferCreate succeeds while the *budget* has room, so the count of live buffers is a
/// consequence of their sizes (at most `MAX_TOTAL_BUFFER_BYTES / PAGE_BYTES` of 4 KiB
/// buffers) rather than a separate policy limit. Destroy returns a buffer's bytes to the
/// budget. Physical-frame reclamation is the kernel's; this is the protocol's accounting.
/// — TIMBERDOODLE
pub const MAX_TOTAL_BUFFER_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_STREAM_BYTES: u32 = 64 * 1024;
pub const RING_BYTES: u32 = 4096;

/// Read-only hardware identity and command-stream capabilities. No address, handle or
/// firmware interface pointer crosses this query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    pub gpu_id: u32,
    pub cs_features: u32,
}

/// Stable shader topology attested during driver bring-up, also used for its group masks.
/// This is a core-ID bitmap, not a count or a device-memory/firmware address. — KESTREL
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceTopology {
    pub shader_present: u64,
}

/// The raw `MMU_FEATURES` word attested during driver bring-up. It crosses the channel
/// undecoded on purpose: bits [7:0] are the hardware virtual-address width, and deriving a
/// range from them is the consumer's job, so this channel never invents an address space. — SAKER
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceMmu {
    pub mmu_features: u32,
}

/// The raw `TILER_FEATURES` word, also crossed undecoded. Unlike the two properties above, a
/// zero here means the tiler's shape is unknown rather than the device unusable, so backends
/// report it as `NotSupported` and the session survives. — SAKER
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceTiler {
    pub tiler_features: u32,
}

/// The complete raw `TEXTURE_FEATURES[0..3]` array from GPU_CONTROL +0x0B0..+0x0BC,
/// crossed undecoded like the other properties. Element 0 is the optional-absence gate:
/// upstream reads it as the compressed-format bitmask and, at bit 25, as AFRC presence, and
/// a zero there means the backend has no attestation at all, so it degrades to
/// `NotSupported`. Elements 1..3 cross verbatim, zero included — they are attested by the
/// same register read, not derived, and no consumer here decodes them. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceTexture {
    pub texture_features: [u32; 4],
}

/// The raw `CORE_FEATURES` word, crossed undecoded like its siblings. This is a *required*
/// property in the query sense: upstream derives the GPU variant from bits [7:0] (`core_features
/// & 0xff` in `panthor_kmod.c`) and matches it against its model table, so a failed query
/// aborts the report. The word itself is attested verbatim, zero included — the Orange Pi 5
/// Plus measures exactly 0x00000000, and variant byte 0 is the pinned table's G610 entry.
/// The derivation stays the consumer's; this channel never invents a variant. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceCore {
    pub core_features: u32,
}

/// Three raw thread-property words, crossed undecoded like their siblings:
/// `THREAD_MAX_THREADS` (GPU_CONTROL +0x0A0), `THREAD_MAX_WORKGROUP_SIZE` (+0x0A4) and
/// `THREAD_FEATURES` (+0x0AC), whose `[21:0]` field is registers per core and `[31:24]`
/// maximum tasks per core. Upstream's `panthor_dev_query_thread_props` copies the first two
/// into `max_threads_per_core` and `max_threads_per_wg`, shifts and masks the third into
/// `max_tasks_per_core` and `num_registers_per_core`, and assumes the TLS instance count
/// equals the thread count. The derivations stay the consumer's; this channel attests only
/// the words. All three may be zero together, which means the group is absent, not that the
/// device is unusable. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceThreads {
    pub max_threads: u32,
    pub max_workgroup_size: u32,
    pub thread_features: u32,
}

/// The memory-system words, crossed undecoded like their siblings: `L2_FEATURES` (GPU_CONTROL
/// +0x004), `MEM_FEATURES` (+0x010) and `COHERENCY_FEATURES` (+0x300), plus the
/// `GPU_COHERENCY_PROTOCOL` value the driver itself programmed before L2 power-up (31 is
/// "none"). Upstream derives the L2 slice count from `MEM_FEATURES[11:8]` and the bus width
/// from `L2_FEATURES[31:24]`, and sets `is_io_coherent` from the protocol the kernel selected,
/// which is what `coherency_protocol` is here. The raw `COHERENCY_FEATURES` word is the
/// hardware's half of that decision and no consumer decodes it. Required: every word is read
/// unconditionally at bring-up. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceMemory {
    pub l2_features: u32,
    pub mem_features: u32,
    pub coherency_features: u32,
    pub coherency_protocol: u32,
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

/// A live-unbind request: remove this session's mappings covering exactly `va..va + bytes` on
/// `vm`. The range must tile whole live bindings with no partial, missing or foreign coverage;
/// page alignment and the same `0x2000_0000..0x3000_0000` window as VmBind apply. Unlike a
/// binding, one request may name several adjacent bindings. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Unbind {
    pub vm: u32,
    pub va: u64,
    pub bytes: u64,
}

impl Unbind {
    /// The same window and alignment rules as a binding, plus a nonempty range. Ownership and
    /// exact tiling are the session's checks, not this arithmetic helper's. — TIMBERDOODLE
    pub fn valid_shape(self) -> bool {
        self.vm != 0
            && self.bytes > 0
            && self.va % PAGE_BYTES == 0
            && self.bytes % PAGE_BYTES == 0
            && in_window(self.va, self.bytes)
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

/// A point on a driver-owned completion timeline (DESIGN/023). `gpu-done` points are queue
/// seqnos. A fence is a value until G8c-b adds capability tokens. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fence {
    pub group: u32,
    pub point: u64,
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
