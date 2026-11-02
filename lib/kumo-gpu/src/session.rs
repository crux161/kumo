//j711
//j742
//j743
//j752
//j754

//! Bounded per-channel object table. Commit metadata only after backend success. The backend
//! must retire on any partial map or uncertain publication; it must never free live DMA.

use alloc::vec::Vec;

use crate::{
    valid_buffer_bytes, Binding, Command, DeviceCore, DeviceInfo, DeviceMemory, DeviceMmu,
    DeviceTexture, DeviceThreads, DeviceTiler, DeviceTopology, HeapPlacement, HeapSpec, Request,
    RequestIds, Response, Status, Submission, SyncState, Timeline, MAX_HEAPS,
    MAX_TOTAL_BUFFER_BYTES,
};

pub const MAX_BINDINGS: usize = 256;
/// Bounded armed-fence points per session. Completed points are pruned before the bound is
/// enforced, so the table holds only waits that are still pending. — TIMBERDOODLE
pub const MAX_WAITS: usize = 4;

/// Backend submission failures distinguish work that never reached the ring from work whose
/// ownership is uncertain. The latter retires the whole session without inventing completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitError {
    Unpublished(Status),
    DeviceLost,
}

/// Exact object counts a [`Session::close`] gave up. The caller reports these once, as the
/// proof of what the containment path released rather than a claim that the hardware is idle;
/// quiescence is the reset's job and is established before close is called. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionRelease {
    pub buffers: u32,
    pub bindings: u32,
    pub waits: u32,
    /// Tiler heaps (G7q-a). Their pages are in `bytes` too, since they share the budget.
    pub heaps: u32,
    pub bytes: u64,
}

impl SessionRelease {
    /// The answer to a second, idempotent close: nothing was left to release. — TIMBERDOODLE
    pub const NONE: Self = Self {
        buffers: 0,
        bindings: 0,
        waits: 0,
        heaps: 0,
        bytes: 0,
    };

    pub fn is_empty(&self) -> bool {
        *self == Self::NONE
    }
}

pub trait Backend {
    fn info(&mut self) -> Result<DeviceInfo, Status>;
    /// Optional read-only extension. An unimplemented backend must not invent topology.
    fn topology(&mut self) -> Result<DeviceTopology, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent MMU properties.
    fn mmu_features(&mut self) -> Result<DeviceMmu, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent tiler properties.
    fn tiler_features(&mut self) -> Result<DeviceTiler, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent texture formats.
    fn texture_features(&mut self) -> Result<DeviceTexture, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent core features.
    fn core_features(&mut self) -> Result<DeviceCore, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent thread properties.
    fn thread_properties(&mut self) -> Result<DeviceThreads, Status> {
        Err(Status::NotSupported)
    }
    /// Optional read-only extension. An unimplemented backend must not invent memory-system
    /// words or a coherency protocol it never programmed.
    fn memory_properties(&mut self) -> Result<DeviceMemory, Status> {
        Err(Status::NotSupported)
    }
    fn create_vm(&mut self) -> Result<(), Status>;
    fn create_buffer(&mut self, id: u32, bytes: u64) -> Result<(), Status>;
    fn bind(&mut self, binding: Binding) -> Result<(), Status>;
    /// Remove every GPU VA mapping in `bindings` — exactly this session's live bindings for
    /// one contiguous range, in ascending VA order and never empty. Allowed while the group
    /// exists; the backend must order same-address-space work before it touches the tables.
    /// It clears the page-table entries and invalidates the packed region only; no CSG, JS,
    /// MCU, doorbell or reset operation belongs here. All-or-nothing: a recoverable status
    /// leaves every mapping intact and retryable, while a partial unmap or uncertain
    /// invalidation must answer `DeviceLost` so the session retires without assuming the GPU
    /// stopped. — TIMBERDOODLE
    fn unbind(&mut self, bindings: &[Binding]) -> Result<(), Status>;
    /// Unbind every GPU VA mapping in `bindings` — exactly this session's live bindings that
    /// name `id` — and release the allocation's backing. This is all-or-nothing: a recoverable
    /// status leaves every mapping and the backing intact and retryable, while any partial
    /// unmap or uncertain release must answer `DeviceLost` so the session retires without
    /// freeing DMA the GPU might still reach. — TIMBERDOODLE
    fn destroy_buffer(&mut self, id: u32, bindings: &[Binding]) -> Result<(), Status>;
    fn create_group(&mut self) -> Result<(), Status>;
    /// Prepare one READ-only CPU status VMO grant for the transport. Unsupported backends
    /// must refuse; a zero-filled or invented status record is not usable. — MILVUS
    fn watch_group(&mut self) -> Result<crate::GroupReport, Status> {
        Err(Status::NotSupported)
    }
    fn submit(&mut self, submission: Submission) -> Result<(), SubmitError>;
    /// G7q-a: allocate one heap's context page and initial chunks, link the chunks head-first,
    /// map everything no-execute in the heap window and flush. A recoverable status maps
    /// nothing and keeps nothing; a partial map is `DeviceLost`, and the backend keeps what it
    /// mapped for `retire`. Unsupported backends refuse. — TIMBERDOODLE
    fn create_heap(&mut self, _id: u32, _spec: HeapSpec) -> Result<HeapPlacement, Status> {
        Err(Status::NotSupported)
    }
    /// Unmap every page of heap `id`, flush, then release it. All-or-nothing, like
    /// `destroy_buffer`. — TIMBERDOODLE
    fn destroy_heap(&mut self, _id: u32) -> Result<(), Status> {
        Err(Status::NotSupported)
    }
    /// Release every backend-side claim this session still holds — DMA registrations, MMU
    /// mappings, queue references — **after** the caller has quiesced the device and the
    /// session has been closed. The default is a deliberate no-op, not a silent success: a
    /// backend that owns no tracked resource must not invent a release, and a backend that
    /// cannot prove quiescence must keep its claims. The method returns nothing because a
    /// failed or unknown release has no recoverable status here; the caller's reset result
    /// already decided whether this path may run at all. — TIMBERDOODLE
    fn retire(&mut self) {}
}

/// One live allocation in this session's own ID space. IDs are issued monotonically and never
/// reused, so a stale destroy can never name a later allocation. — TIMBERDOODLE
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Allocation {
    id: u32,
    bytes: u64,
}

#[derive(Default)]
pub struct Session {
    ids: RequestIds,
    vm: bool,
    group: bool,
    retired: bool,
    /// Set by [`Session::close`]. Separate from `retired`, which a backend DeviceLost may set
    /// first: only an explicit close reports released counts, and only once. — TIMBERDOODLE
    closed: bool,
    /// Live allocations, on demand: the budget — not a table size — bounds the session.
    /// — TIMBERDOODLE
    buffers: Vec<Allocation>,
    /// Bytes charged against [`MAX_TOTAL_BUFFER_BYTES`] by the live allocations above.
    total_buffer_bytes: u64,
    /// The last buffer ID issued; zero means none. Never decreases and is never reused.
    /// — TIMBERDOODLE
    next_buffer: u32,
    bindings: Vec<Binding>,
    timeline: Timeline,
    waits: [Option<u64>; MAX_WAITS],
    /// Live tiler heaps and the bytes each charges, in their own monotonic ID space.
    /// — TIMBERDOODLE
    heaps: Vec<Allocation>,
    next_heap: u32,
}

impl Session {
    pub fn dispatch(
        &mut self,
        request: Request,
        backend: &mut impl Backend,
    ) -> Result<Response, Status> {
        self.ids.accept(request.id)?;
        if self.retired {
            return Err(Status::DeviceLost);
        }
        let result = self.execute(request.command, backend);
        if result == Err(Status::DeviceLost)
            || matches!(result,
            Ok(Response::GroupWatch { report, .. }) if report.state != crate::GroupState::Usable)
        {
            self.retired = true;
        }
        result
    }

    /// Contained close: make the session terminal and forget every object it tracks, returning
    /// exact counts so the caller can report what this path released. Idempotent — a second
    /// call answers [`SessionRelease::NONE`] and touches nothing.
    ///
    /// This releases *metadata only*. It neither resets the device nor frees DMA: the caller
    /// must quiesce the GPU first, and then call [`Backend::retire`] to release the backing
    /// this session can no longer name. If quiescence is not proven, the caller must not call
    /// this at all — retain the metadata and the backing together and poison the session.
    /// — TIMBERDOODLE
    pub fn close(&mut self) -> SessionRelease {
        if self.closed {
            return SessionRelease::NONE;
        }
        self.closed = true;
        self.retired = true;
        let release = SessionRelease {
            buffers: self.buffers.len() as u32,
            bindings: self.bindings.len() as u32,
            waits: self.waits.iter().flatten().count() as u32,
            heaps: self.heaps.len() as u32,
            bytes: self.total_buffer_bytes,
        };
        self.buffers.clear();
        self.heaps.clear();
        self.bindings.clear();
        self.waits = [None; MAX_WAITS];
        self.total_buffer_bytes = 0;
        // The VM and group are backend objects torn down by the reset, not tracked allocations;
        // clearing the flags keeps the closed session from reading as still-created.
        self.vm = false;
        self.group = false;
        release
    }

    pub fn complete(&mut self, point: u64, status: u32) -> Result<(), Status> {
        if self.retired {
            return Err(Status::DeviceLost);
        }
        self.timeline.complete(point, status)
    }

    /// Read-only completion state for an armed fence; the server's wake drain consults it
    /// after each completion. — TIMBERDOODLE
    pub fn fence_state(&self, point: u64) -> Result<SyncState, Status> {
        if self.retired {
            return Err(Status::DeviceLost);
        }
        if !self.group {
            return Err(Status::BadState);
        }
        self.timeline.query(point)
    }

    fn execute(
        &mut self,
        command: Command,
        backend: &mut impl Backend,
    ) -> Result<Response, Status> {
        match command {
            Command::GetInfo => backend.info().map(Response::Info),
            Command::GetTopology => {
                let topology = backend.topology()?;
                if topology.shader_present == 0 {
                    return Err(Status::DeviceLost);
                }
                Ok(Response::Topology(topology))
            }
            Command::GetMmuFeatures => {
                let mmu = backend.mmu_features()?;
                if mmu.mmu_features == 0 {
                    return Err(Status::DeviceLost);
                }
                Ok(Response::Mmu(mmu))
            }
            Command::GetTilerFeatures => {
                let tiler = backend.tiler_features()?;
                // A zero word cannot cross the wire. Unlike topology or MMU features it means
                // the tiler's shape is unknown rather than the device unusable, so it degrades
                // to a recoverable refusal instead of retiring the session. — SAKER
                if tiler.tiler_features == 0 {
                    return Err(Status::NotSupported);
                }
                Ok(Response::Tiler(tiler))
            }
            Command::GetTextureFeatures => {
                let texture = backend.texture_features()?;
                // Same optional-property rule as tiler, applied to element 0 only: a zero
                // there is unknown capability, not an unusable device, so it degrades instead
                // of retiring. Elements 1..3 cross verbatim, zero included — they were read
                // in the same powered block, so a zero is an attested value. — TIMBERDOODLE
                if texture.texture_features[0] == 0 {
                    return Err(Status::NotSupported);
                }
                Ok(Response::Texture(texture))
            }
            Command::GetCoreFeatures => {
                // Required in the query sense — any failure aborts the report — but the word
                // crosses verbatim, zero included: the Orange Pi 5 Plus measures exactly
                // 0x00000000 (J650), and variant byte 0 is the pinned Mesa table's G610 entry,
                // so zero is a valid attestation, not a failed read. — TIMBERDOODLE
                Ok(Response::Core(backend.core_features()?))
            }
            Command::GetThreadProperties => {
                // Crossed verbatim, all three words zero included: unlike tiler and texture
                // features, an all-zero group is a legitimate absence the consumer records by
                // keeping missing-property bit 4, not a refusal this layer invents. Only the
                // backend's own NotSupported crosses as the recoverable NotSupported error,
                // exactly as the tiler and texture arms do. — TIMBERDOODLE
                Ok(Response::Threads(backend.thread_properties()?))
            }
            Command::GetMemoryProperties => {
                // Required in the query sense, like core features, and crossed verbatim with no
                // zero guard: COHERENCY_FEATURES reads zero on the Orange Pi and protocol 0 is
                // ACE-Lite, so a zero word is data here, and deciding what any word implies is
                // the consumer's job. — TIMBERDOODLE
                Ok(Response::Memory(backend.memory_properties()?))
            }
            Command::VmCreate => {
                if self.vm {
                    return Err(Status::BadState);
                }
                backend.create_vm()?;
                self.vm = true;
                Ok(Response::VmCreated { vm: 1 })
            }
            Command::BufferCreate { bytes } => {
                // Creation is legal once the group exists (j711): it allocates backing and
                // binds nothing; a mapping still needs VmBind, which G8b allows post-group.
                // — TIMBERDOODLE
                if !self.vm {
                    return Err(Status::BadState);
                }
                if !valid_buffer_bytes(bytes) {
                    return Err(Status::BadRequest);
                }
                // Budget, not table size, bounds the session: the live byte total is the
                // policy, and the number of buffers follows from their sizes. — TIMBERDOODLE
                let total = self
                    .total_buffer_bytes
                    .checked_add(bytes)
                    .filter(|total| *total <= MAX_TOTAL_BUFFER_BYTES)
                    .ok_or(Status::NoMemory)?;
                let id = self.next_buffer.checked_add(1).ok_or(Status::NoMemory)?;
                // Reserve before the backend allocates. A failed reservation must reach no
                // backend call, or the session would own DMA it cannot name. — TIMBERDOODLE
                self.buffers.try_reserve(1).map_err(|_| Status::NoMemory)?;
                backend.create_buffer(id, bytes)?;
                // IDs issue only after success, so a failed create burns no ID and a stale
                // destroy can never name this or a later allocation. — TIMBERDOODLE
                self.buffers.push(Allocation { id, bytes });
                self.total_buffer_bytes = total;
                self.next_buffer = id;
                Ok(Response::BufferCreated { buffer: id, bytes })
            }
            Command::BufferDestroy { buffer } => {
                // Destroy's implicit unbind batch is legal once the group exists (j711): the
                // serve loop's synchronous ordering fences same-address-space work, the
                // backend clears exactly this buffer's live mappings and flushes, and the
                // metadata clears only after backend success. — TIMBERDOODLE
                if !self.vm {
                    return Err(Status::BadState);
                }
                let position = self
                    .buffers
                    .iter()
                    .position(|allocation| allocation.id == buffer)
                    .ok_or(Status::NotFound)?;
                let mut batch = Vec::new();
                batch
                    .try_reserve(self.bindings.len())
                    .map_err(|_| Status::NoMemory)?;
                for binding in self.bindings.iter() {
                    if binding.buffer == buffer {
                        batch.push(*binding);
                    }
                }
                // All-or-nothing: a recoverable failure leaves every mapping and the backing
                // intact and retryable; a partial unmap or uncertain release must answer
                // DeviceLost, which retires this session before any metadata commits.
                // — TIMBERDOODLE
                backend.destroy_buffer(buffer, &batch)?;
                self.bindings.retain(|binding| binding.buffer != buffer);
                let allocation = self.buffers.remove(position);
                self.total_buffer_bytes -= allocation.bytes;
                Ok(Response::BufferDestroyed { buffer })
            }
            Command::VmBind(binding) => {
                // Live binds are allowed after GroupCreate (G8b): the backend fences
                // same-address-space work and invalidates the affected page-table region, so
                // no queue is stopped for this mutation. RESEARCH/017. — TIMBERDOODLE
                if !self.vm {
                    return Err(Status::BadState);
                }
                if binding.vm != 1 {
                    return Err(Status::NotFound);
                }
                let bytes = self
                    .buffers
                    .iter()
                    .find(|allocation| allocation.id == binding.buffer)
                    .map(|allocation| allocation.bytes)
                    .ok_or(Status::NotFound)?;
                binding.validate(bytes)?;
                if self.bindings.iter().any(|other| binding.overlaps(*other)) {
                    return Err(Status::BadRequest);
                }
                if self.bindings.len() == MAX_BINDINGS {
                    return Err(Status::NoMemory);
                }
                self.bindings.try_reserve(1).map_err(|_| Status::NoMemory)?;
                backend.bind(binding)?;
                self.bindings.push(binding);
                Ok(Response::VmBound)
            }
            Command::VmUnbind(range) => {
                // G8b's live counterpart. The range is validated against the live table
                // before any backend call: it must cover whole bindings with no hole, no
                // overhang and no foreign VM. The table changes only after the backend
                // confirms the clear and invalidation, so a recoverable refusal is
                // retryable and a DeviceLost answer retires without a half-updated table.
                // — TIMBERDOODLE
                if !self.vm {
                    return Err(Status::BadState);
                }
                if range.vm != 1 {
                    return Err(Status::NotFound);
                }
                if !range.valid_shape() {
                    return Err(Status::BadRequest);
                }
                // valid_shape proved this addition cannot overflow and stays in the window.
                let end = range.va + range.bytes;
                let mut batch = Vec::new();
                batch
                    .try_reserve(self.bindings.len())
                    .map_err(|_| Status::NoMemory)?;
                for binding in self.bindings.iter().copied() {
                    if binding.va >= end || range.va >= binding.va + binding.bytes {
                        continue;
                    }
                    // An intersecting binding must be wholly inside the request: a range
                    // that splits one leaves an undefined half-mapping, so refuse it.
                    if binding.va < range.va || binding.va + binding.bytes > end {
                        return Err(Status::BadRequest);
                    }
                    batch.push(binding);
                }
                if batch.is_empty() {
                    return Err(Status::NotFound);
                }
                // Sort by VA (at most MAX_BINDINGS entries) and require an exact tiling:
                // every byte of the request belongs to exactly one live binding.
                for i in 1..batch.len() {
                    let mut j = i;
                    while j > 0 && batch[j - 1].va > batch[j].va {
                        batch.swap(j - 1, j);
                        j -= 1;
                    }
                }
                let mut cursor = range.va;
                for binding in &batch {
                    if binding.va != cursor {
                        return Err(Status::BadRequest);
                    }
                    cursor += binding.bytes;
                }
                if cursor != end {
                    return Err(Status::BadRequest);
                }
                backend.unbind(&batch)?;
                self.bindings
                    .retain(|binding| binding.va < range.va || binding.va >= end);
                Ok(Response::VmUnbound(range))
            }
            Command::GroupCreate { vm } => {
                if !self.vm || self.group {
                    return Err(Status::BadState);
                }
                if vm != 1 {
                    return Err(Status::NotFound);
                }
                backend.create_group()?;
                self.group = true;
                Ok(Response::GroupCreated { group: 1 })
            }
            Command::GroupWatch { group } => {
                if !self.group {
                    return Err(Status::BadState);
                }
                if group != 1 {
                    return Err(Status::NotFound);
                }
                let report = backend.watch_group()?;
                if !report.valid() {
                    return Err(Status::DeviceLost);
                }
                Ok(Response::GroupWatch { group, report })
            }
            Command::Submit(job) => {
                if !self.group {
                    return Err(Status::BadState);
                }
                if job.group != 1 {
                    return Err(Status::NotFound);
                }
                if !self
                    .bindings
                    .iter()
                    .any(|b| job.validate_for(1, *b).is_ok())
                {
                    return Err(Status::BadRequest);
                }
                self.timeline.reserve(job.signal_point)?;
                match backend.submit(job) {
                    Ok(()) => Ok(Response::Submitted {
                        point: job.signal_point,
                    }),
                    Err(SubmitError::Unpublished(status)) => {
                        self.timeline.abort_unpublished(job.signal_point)?;
                        Err(status)
                    }
                    Err(SubmitError::DeviceLost) => Err(Status::DeviceLost),
                }
            }
            Command::SyncRead { group, point } => {
                if !self.group {
                    return Err(Status::BadState);
                }
                if group != 1 {
                    return Err(Status::NotFound);
                }
                Ok(Response::Sync {
                    point,
                    state: self.timeline.query(point)?,
                })
            }
            Command::FenceWait { group, point } => {
                if !self.group {
                    return Err(Status::BadState);
                }
                if group != 1 {
                    return Err(Status::NotFound);
                }
                match self.timeline.query(point)? {
                    SyncState::Complete { gpu_status } => {
                        Ok(Response::WaitDone { point, gpu_status })
                    }
                    SyncState::Pending => {
                        // Completed points do not hold a slot: the server's wake drain
                        // re-queries `fence_state`, so pruning loses nothing. — TIMBERDOODLE
                        for slot in &mut self.waits {
                            if let Some(armed) = *slot {
                                if matches!(self.timeline.query(armed)?, SyncState::Complete { .. })
                                {
                                    *slot = None;
                                }
                            }
                        }
                        if !self.waits.iter().any(|slot| *slot == Some(point)) {
                            let Some(free) = self.waits.iter_mut().find(|slot| slot.is_none())
                            else {
                                return Err(Status::Busy);
                            };
                            *free = Some(point);
                        }
                        Ok(Response::WaitArmed { point })
                    }
                }
            }
            Command::HeapCreate(spec) => {
                // A heap needs only the VM, as Panthor's does: Gallium creates it after the
                // group, panvk per queue. — TIMBERDOODLE
                if !self.vm {
                    return Err(Status::BadState);
                }
                if spec.vm != 1 {
                    return Err(Status::NotFound);
                }
                if self.heaps.len() >= MAX_HEAPS {
                    return Err(Status::NoMemory);
                }
                // Heaps share the buffers' budget: tiler memory is DMA the session holds.
                let bytes = spec.initial_bytes();
                let total = self
                    .total_buffer_bytes
                    .checked_add(bytes)
                    .filter(|total| *total <= MAX_TOTAL_BUFFER_BYTES)
                    .ok_or(Status::NoMemory)?;
                let id = self.next_heap.checked_add(1).ok_or(Status::NoMemory)?;
                self.heaps.try_reserve(1).map_err(|_| Status::NoMemory)?;
                let placement = backend.create_heap(id, spec)?;
                // The backend has mapped memory by now, so a placement outside the window is
                // not refusable: retire, and let containment release it. — TIMBERDOODLE
                if !placement.valid() {
                    return Err(Status::DeviceLost);
                }
                self.heaps.push(Allocation { id, bytes });
                self.total_buffer_bytes = total;
                self.next_heap = id;
                Ok(Response::HeapCreated {
                    heap: id,
                    placement,
                })
            }
            Command::HeapDestroy { heap } => {
                if !self.vm {
                    return Err(Status::BadState);
                }
                let position = self
                    .heaps
                    .iter()
                    .position(|allocation| allocation.id == heap)
                    .ok_or(Status::NotFound)?;
                backend.destroy_heap(heap)?;
                let allocation = self.heaps.swap_remove(position);
                self.total_buffer_bytes -= allocation.bytes;
                Ok(Response::HeapDestroyed {
                    heap,
                    bytes: allocation.bytes,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{BindFlags, SyncState, Unbind, MAX_BUFFER_BYTES, PAGE_BYTES, VA_START};
    use std::vec::Vec;

    #[derive(Default)]
    struct Fake {
        calls: Vec<Command>,
        /// Every destroy the backend was asked to perform, with the exact binding batch the
        /// session derived — recorded before the injected failure so a test can distinguish
        /// "never called" from "called and refused". — TIMBERDOODLE
        destroyed: Vec<(u32, Vec<Binding>)>,
        /// Every unbind batch the backend was asked to perform, recorded before the injected
        /// failure so a test can distinguish "never called" from "called and refused".
        /// — TIMBERDOODLE
        unbound: Vec<Vec<Binding>>,
        fail: Option<Status>,
        submit_error: Option<SubmitError>,
        topology: Option<u64>,
        mmu_features: Option<u32>,
        tiler_features: Option<u32>,
        texture_features: Option<[u32; 4]>,
        core_features: Option<u32>,
        threads: Option<DeviceThreads>,
        memory: Option<DeviceMemory>,
        group_report: Option<crate::GroupReport>,
        /// How many times the containment layer asked the backend to release its own claims.
        /// A unit count, not a success flag: `retire` has nothing to answer. — TIMBERDOODLE
        retire_calls: u32,
        /// G7q-a: heaps are supported only when set; `bad_placement` answers one outside the
        /// window. — TIMBERDOODLE
        heaps: bool,
        bad_placement: bool,
    }
    impl Fake {
        fn record(&mut self, command: Command) -> Result<(), Status> {
            self.calls.push(command);
            self.fail.take().map_or(Ok(()), Err)
        }
    }
    impl Backend for Fake {
        fn create_heap(&mut self, id: u32, spec: HeapSpec) -> Result<HeapPlacement, Status> {
            if !self.heaps {
                return Err(Status::NotSupported);
            }
            self.record(Command::HeapCreate(spec))?;
            let context_va = crate::HEAP_VA_START + u64::from(id) * 0x40_0000;
            Ok(HeapPlacement {
                context_va: if self.bad_placement {
                    VA_START
                } else {
                    context_va
                },
                first_chunk_va: context_va + PAGE_BYTES,
            })
        }
        fn destroy_heap(&mut self, heap: u32) -> Result<(), Status> {
            self.record(Command::HeapDestroy { heap })
        }
        fn watch_group(&mut self) -> Result<crate::GroupReport, Status> {
            self.record(Command::GroupWatch { group: 1 })?;
            self.group_report.ok_or(Status::NotSupported)
        }
        fn topology(&mut self) -> Result<DeviceTopology, Status> {
            self.record(Command::GetTopology)?;
            self.topology
                .map(|shader_present| DeviceTopology { shader_present })
                .ok_or(Status::NotSupported)
        }
        fn mmu_features(&mut self) -> Result<DeviceMmu, Status> {
            self.record(Command::GetMmuFeatures)?;
            self.mmu_features
                .map(|mmu_features| DeviceMmu { mmu_features })
                .ok_or(Status::NotSupported)
        }
        fn tiler_features(&mut self) -> Result<DeviceTiler, Status> {
            self.record(Command::GetTilerFeatures)?;
            self.tiler_features
                .map(|tiler_features| DeviceTiler { tiler_features })
                .ok_or(Status::NotSupported)
        }
        fn texture_features(&mut self) -> Result<DeviceTexture, Status> {
            self.record(Command::GetTextureFeatures)?;
            self.texture_features
                .map(|texture_features| DeviceTexture { texture_features })
                .ok_or(Status::NotSupported)
        }
        fn core_features(&mut self) -> Result<DeviceCore, Status> {
            self.record(Command::GetCoreFeatures)?;
            self.core_features
                .map(|core_features| DeviceCore { core_features })
                .ok_or(Status::NotSupported)
        }
        fn thread_properties(&mut self) -> Result<DeviceThreads, Status> {
            self.record(Command::GetThreadProperties)?;
            self.threads.ok_or(Status::NotSupported)
        }
        fn memory_properties(&mut self) -> Result<DeviceMemory, Status> {
            self.record(Command::GetMemoryProperties)?;
            self.memory.ok_or(Status::NotSupported)
        }
        fn info(&mut self) -> Result<DeviceInfo, Status> {
            self.record(Command::GetInfo)?;
            Ok(DeviceInfo {
                gpu_id: 0xa8670005,
                cs_features: 0x7085f,
            })
        }
        fn create_vm(&mut self) -> Result<(), Status> {
            self.record(Command::VmCreate)
        }
        fn create_buffer(&mut self, _: u32, bytes: u64) -> Result<(), Status> {
            self.record(Command::BufferCreate { bytes })
        }
        fn bind(&mut self, b: Binding) -> Result<(), Status> {
            self.record(Command::VmBind(b))
        }
        fn destroy_buffer(&mut self, id: u32, bindings: &[Binding]) -> Result<(), Status> {
            self.destroyed.push((id, bindings.to_vec()));
            self.record(Command::BufferDestroy { buffer: id })
        }
        fn unbind(&mut self, bindings: &[Binding]) -> Result<(), Status> {
            self.unbound.push(bindings.to_vec());
            let va = bindings.first().map_or(0, |binding| binding.va);
            let bytes = bindings.iter().map(|binding| binding.bytes).sum();
            self.record(Command::VmUnbind(Unbind { vm: 1, va, bytes }))
        }
        fn create_group(&mut self) -> Result<(), Status> {
            self.record(Command::GroupCreate { vm: 1 })
        }
        fn submit(&mut self, s: Submission) -> Result<(), SubmitError> {
            self.calls.push(Command::Submit(s));
            self.submit_error.take().map_or(Ok(()), Err)
        }
        fn retire(&mut self) {
            self.retire_calls += 1;
        }
    }
    fn binding() -> Binding {
        Binding {
            vm: 1,
            buffer: 1,
            va: VA_START,
            offset: 0,
            bytes: PAGE_BYTES,
            flags: BindFlags(BindFlags::READ_ONLY | BindFlags::NO_EXECUTE),
        }
    }
    fn job(point: u64) -> Command {
        Command::Submit(Submission {
            group: 1,
            stream_va: VA_START,
            stream_bytes: 48,
            signal_point: point,
        })
    }
    fn send(s: &mut Session, b: &mut Fake, command: Command) -> Result<Response, Status> {
        let id = s.ids.last + 1;
        let request = Request { id, command };
        let decoded = Request::decode(&request.encode()).unwrap();
        s.dispatch(decoded, b)
    }
    fn ready() -> (Session, Fake) {
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        send(&mut s, &mut b, Command::VmBind(binding())).unwrap();
        send(&mut s, &mut b, Command::GroupCreate { vm: 1 }).unwrap();
        (s, b)
    }

    #[test]
    fn fence_wait_arms_dedups_prunes_and_reports_completion() {
        let (mut s, mut b) = ready();
        send(&mut s, &mut b, job(1)).unwrap();
        assert_eq!(
            send(&mut s, &mut b, Command::FenceWait { group: 1, point: 1 }),
            Ok(Response::WaitArmed { point: 1 })
        );
        // Idempotent arm: the same pending point occupies one slot and still replies armed.
        assert_eq!(
            send(&mut s, &mut b, Command::FenceWait { group: 1, point: 1 }),
            Ok(Response::WaitArmed { point: 1 })
        );
        assert_eq!(s.waits.iter().flatten().count(), 1);
        assert_eq!(s.fence_state(1), Ok(SyncState::Pending));
        s.complete(1, 0).unwrap();
        assert_eq!(
            send(&mut s, &mut b, Command::FenceWait { group: 1, point: 1 }),
            Ok(Response::WaitDone {
                point: 1,
                gpu_status: 0
            })
        );
        assert_eq!(s.fence_state(1), Ok(SyncState::Complete { gpu_status: 0 }));
        // A completed arm's slot is pruned when the next point arms.
        send(&mut s, &mut b, job(2)).unwrap();
        assert_eq!(
            send(&mut s, &mut b, Command::FenceWait { group: 1, point: 2 }),
            Ok(Response::WaitArmed { point: 2 })
        );
        assert_eq!(s.waits.iter().flatten().count(), 1);
    }

    #[test]
    fn fence_wait_requires_a_group_and_rejects_unknown_points() {
        let mut b = Fake::default();
        let mut s = Session::default();
        assert_eq!(
            send(&mut s, &mut b, Command::FenceWait { group: 1, point: 1 }),
            Err(Status::BadState)
        );
        let (mut s, mut b) = ready();
        assert_eq!(
            send(&mut s, &mut b, Command::FenceWait { group: 2, point: 1 }),
            Err(Status::NotFound)
        );
        // Point 0 is rejected by the wire layer itself, exactly as SyncRead's frame is, so
        // only points above `submitted` reach the session's NotFound path.
        assert_eq!(
            send(&mut s, &mut b, Command::FenceWait { group: 1, point: 1 }),
            Err(Status::NotFound)
        );
    }
    #[test]
    fn topology_is_read_only_unsupported_is_recoverable_and_zero_retires() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTopology),
            Err(Status::NotSupported)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        backend.topology = Some(0x8000000000000001);
        let topology = Response::Topology(DeviceTopology {
            shader_present: 0x8000000000000001,
        });
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTopology),
            Ok(topology)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTopology),
            Ok(topology)
        );
        assert!(session.buffers.is_empty());
        assert!(session.bindings.is_empty());
        backend.topology = Some(0);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTopology),
            Err(Status::DeviceLost)
        );
        let calls = backend.calls.len();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetInfo),
            Err(Status::DeviceLost)
        );
        assert_eq!(backend.calls.len(), calls);
    }

    #[test]
    fn mmu_features_are_read_only_unsupported_is_recoverable_and_zero_retires() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetMmuFeatures),
            Err(Status::NotSupported)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        // The register word crosses undecoded. 0x2830 is what the Orange Pi attests; its
        // [7:0] field is the 48-bit virtual-address width the consumer derives. — SAKER
        backend.mmu_features = Some(0x2830);
        let mmu = Response::Mmu(DeviceMmu {
            mmu_features: 0x2830,
        });
        assert_eq!(
            send(&mut session, &mut backend, Command::GetMmuFeatures),
            Ok(mmu)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GetMmuFeatures),
            Ok(mmu)
        );
        assert!(session.buffers.is_empty());
        assert!(session.bindings.is_empty());
        backend.mmu_features = Some(0);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetMmuFeatures),
            Err(Status::DeviceLost)
        );
        let calls = backend.calls.len();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTopology),
            Err(Status::DeviceLost)
        );
        assert_eq!(backend.calls.len(), calls);
    }

    #[test]
    fn core_features_are_read_only_unsupported_is_recoverable_and_zero_is_a_valid_attestation() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetCoreFeatures),
            Err(Status::NotSupported)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        // The register word crosses undecoded. The modeled word carries variant byte 0 — the
        // pinned Mesa table's requirement for a G610 entry to resolve — plus an unrelated bit
        // above the byte so nothing on this side selects [7:0]. — TIMBERDOODLE
        backend.core_features = Some(0x0000_1000);
        let core = Response::Core(DeviceCore {
            core_features: 0x0000_1000,
        });
        assert_eq!(
            send(&mut session, &mut backend, Command::GetCoreFeatures),
            Ok(core)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GetCoreFeatures),
            Ok(core)
        );
        assert!(session.buffers.is_empty());
        assert!(session.bindings.is_empty());
        // The board's measured word is exactly zero: variant byte 0 is the pinned table's
        // G610 entry, so zero is a valid attestation, not a failed read, and the session
        // stays live. — TIMBERDOODLE
        backend.core_features = Some(0);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetCoreFeatures),
            Ok(Response::Core(DeviceCore { core_features: 0 }))
        );
        let calls = backend.calls.len();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetInfo),
            Ok(Response::Info(DeviceInfo {
                gpu_id: 0xa8670005,
                cs_features: 0x7085f,
            }))
        );
        assert_eq!(backend.calls.len(), calls + 1);
    }

    #[test]
    fn tiler_features_refuse_zero_recoverably_instead_of_retiring_the_session() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTilerFeatures),
            Err(Status::NotSupported)
        );
        // RESEARCH/009 predicts 0x809: a 512-byte bin and 8 levels. The session only carries
        // the word; upstream decodes it. — SAKER
        backend.tiler_features = Some(0x809);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTilerFeatures),
            Ok(Response::Tiler(DeviceTiler {
                tiler_features: 0x809
            }))
        );
        // What separates this opcode from topology and MMU: an unknown tiler shape degrades
        // instead of killing the session, and later work still proceeds. — SAKER
        backend.tiler_features = Some(0);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTilerFeatures),
            Err(Status::NotSupported)
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert!(session.buffers.is_empty());
        assert!(session.bindings.is_empty());
    }

    #[test]
    fn texture_features_follow_the_same_optional_rule_as_tiler_features() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTextureFeatures),
            Err(Status::NotSupported)
        );
        // Bit 25 of element 0 is AFRC presence on arch 10, and elements 1..3 are attested by
        // the same register read, so the whole array must cross with every bit intact rather
        // than being reduced to one boolean on this side. — TIMBERDOODLE
        backend.texture_features = Some([0x0200_0001, 0, 0x8000_0000, 0]);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTextureFeatures),
            Ok(Response::Texture(DeviceTexture {
                texture_features: [0x0200_0001, 0, 0x8000_0000, 0]
            }))
        );
        // Only element 0 is the absence gate; zeroes in elements 1..3 are attested values.
        backend.texture_features = Some([0x0200_0000, 0, 0, 0]);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTextureFeatures),
            Ok(Response::Texture(DeviceTexture {
                texture_features: [0x0200_0000, 0, 0, 0]
            }))
        );
        backend.texture_features = Some([0, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff]);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetTextureFeatures),
            Err(Status::NotSupported)
        );
        // Absence degrades: the session still creates its VM and group afterwards. — SAKER
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert!(session.buffers.is_empty());
        assert!(session.bindings.is_empty());
    }

    #[test]
    fn thread_properties_cross_verbatim_unsupported_is_recoverable_and_zero_words_are_an_absence() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetThreadProperties),
            Err(Status::NotSupported)
        );
        // A modeled G610-shaped group: 1024 threads/core, 1024 threads/workgroup, and a
        // THREAD_FEATURES word carrying 3 tasks/core in [31:24] and 0x100005 registers in
        // [21:0]. Upstream derives those fields; this layer must not. — TIMBERDOODLE
        let threads = DeviceThreads {
            max_threads: 1024,
            max_workgroup_size: 1024,
            thread_features: 0x0310_0005,
        };
        backend.threads = Some(threads);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetThreadProperties),
            Ok(Response::Threads(threads))
        );
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        // All-zero words are a legitimate absence, not a refusal: the backend owns that
        // decision, and the words still cross verbatim so the consumer can keep its missing
        // bit. — TIMBERDOODLE
        backend.threads = Some(DeviceThreads {
            max_threads: 0,
            max_workgroup_size: 0,
            thread_features: 0,
        });
        assert_eq!(
            send(&mut session, &mut backend, Command::GetThreadProperties),
            Ok(Response::Threads(DeviceThreads {
                max_threads: 0,
                max_workgroup_size: 0,
                thread_features: 0
            }))
        );
        // The session is still live afterwards. — TIMBERDOODLE
        assert_eq!(
            send(&mut session, &mut backend, Command::GroupCreate { vm: 1 }),
            Ok(Response::GroupCreated { group: 1 })
        );
        assert!(session.buffers.is_empty());
        assert!(session.bindings.is_empty());
    }

    #[test]
    fn memory_properties_cross_verbatim_and_an_unimplemented_backend_invents_nothing() {
        let mut session = Session::default();
        let mut backend = Fake::default();
        assert_eq!(
            send(&mut session, &mut backend, Command::GetMemoryProperties),
            Err(Status::NotSupported)
        );
        // The Orange Pi's measured words (J653): 0x0712_0306, 0x301, coherency features zero,
        // and the "none" protocol (31) drv-gpu writes before L2 power-up. — TIMBERDOODLE
        let board = DeviceMemory {
            l2_features: 0x0712_0306,
            mem_features: 0x301,
            coherency_features: 0,
            coherency_protocol: 31,
        };
        backend.memory = Some(board);
        assert_eq!(
            send(&mut session, &mut backend, Command::GetMemoryProperties),
            Ok(Response::Memory(board))
        );
        // Every word zero, protocol 0 (ACE-Lite) included, still crosses unchanged; and full
        // 32-bit words are not narrowed. — TIMBERDOODLE
        for memory in [
            DeviceMemory {
                l2_features: 0,
                mem_features: 0,
                coherency_features: 0,
                coherency_protocol: 0,
            },
            DeviceMemory {
                l2_features: u32::MAX,
                mem_features: u32::MAX,
                coherency_features: u32::MAX,
                coherency_protocol: u32::MAX,
            },
        ] {
            backend.memory = Some(memory);
            assert_eq!(
                send(&mut session, &mut backend, Command::GetMemoryProperties),
                Ok(Response::Memory(memory))
            );
        }
        // A read-only query leaves the session live and consumes no object. — TIMBERDOODLE
        assert_eq!(
            send(&mut session, &mut backend, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        assert!(session.buffers.is_empty());
        assert!(session.bindings.is_empty());
    }

    #[test]
    fn info_query_is_available_before_vm_creation_and_does_not_consume_objects() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        assert_eq!(
            send(&mut s, &mut b, Command::GetInfo),
            Ok(Response::Info(DeviceInfo {
                gpu_id: 0xa8670005,
                cs_features: 0x7085f
            }))
        );
        assert_eq!(
            send(&mut s, &mut b, Command::VmCreate),
            Ok(Response::VmCreated { vm: 1 })
        );
        assert_eq!(b.calls.as_slice(), &[Command::GetInfo, Command::VmCreate]);
    }

    #[test]
    fn two_submissions_remain_pending_until_backend_completion() {
        let (mut s, mut b) = ready();
        for point in 1..=2 {
            assert_eq!(
                send(&mut s, &mut b, job(point)),
                Ok(Response::Submitted { point })
            );
            assert_eq!(
                send(&mut s, &mut b, Command::SyncRead { group: 1, point }),
                Ok(Response::Sync {
                    point,
                    state: SyncState::Pending
                })
            );
            s.complete(point, 0).unwrap();
            assert_eq!(
                send(&mut s, &mut b, Command::SyncRead { group: 1, point }),
                Ok(Response::Sync {
                    point,
                    state: SyncState::Complete { gpu_status: 0 }
                })
            );
        }
    }
    #[test]
    fn invalid_ids_overlap_and_replay_never_reach_backend() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        send(&mut s, &mut b, Command::VmBind(binding())).unwrap();
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Err(Status::BadRequest)
        );
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::VmBind(Binding {
                    buffer: 99,
                    ..binding()
                })
            ),
            Err(Status::NotFound)
        );
        assert_eq!(b.calls.len(), calls);
        send(&mut s, &mut b, Command::GroupCreate { vm: 1 }).unwrap();
        let calls = b.calls.len();
        // G8b lifts the frozen-binding rule: this bind is legal after the group exists, so
        // the occupied VA is an overlap refusal rather than a state refusal. — TIMBERDOODLE
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Err(Status::BadRequest)
        );
        // Creation is legal after the group now (j711); an existing group is the refused
        // state transition this test keeps guarding. — TIMBERDOODLE
        assert_eq!(
            send(&mut s, &mut b, Command::GroupCreate { vm: 1 }),
            Err(Status::BadState)
        );
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::Submit(Submission {
                    group: 99,
                    stream_va: VA_START,
                    stream_bytes: 48,
                    signal_point: 1
                })
            ),
            Err(Status::NotFound)
        );
        assert_eq!(
            s.dispatch(
                Request {
                    id: 1,
                    command: Command::VmCreate
                },
                &mut b
            ),
            Err(Status::BadRequest)
        );
        assert_eq!(b.calls.len(), calls);
    }

    #[test]
    fn live_bind_and_unbind_work_after_the_group_exists() {
        let (mut s, mut b) = ready();
        let range = Unbind {
            vm: 1,
            va: VA_START,
            bytes: PAGE_BYTES,
        };
        assert_eq!(
            send(&mut s, &mut b, Command::VmUnbind(range)),
            Ok(Response::VmUnbound(range))
        );
        assert_eq!(b.unbound.len(), 1);
        assert_eq!(b.unbound[0], std::vec![binding()]);
        assert!(s.bindings.is_empty());
        // The VA is free and the group still exists: live rebind is legal.
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Ok(Response::VmBound)
        );
        assert_eq!(s.bindings.len(), 1);
        // A submission still validates against the live mapping.
        assert_eq!(
            send(&mut s, &mut b, job(1)),
            Ok(Response::Submitted { point: 1 })
        );
    }

    #[test]
    fn vm_unbind_requires_a_live_vm_and_the_sessions_own_vm_id() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        let range = Unbind {
            vm: 1,
            va: VA_START,
            bytes: PAGE_BYTES,
        };
        assert_eq!(
            send(&mut s, &mut b, Command::VmUnbind(range)),
            Err(Status::BadState)
        );
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        let calls = b.calls.len();
        // Any other nonzero VM is foreign: NotFound, and never a backend call.
        for vm in [2, 3, u32::MAX] {
            assert_eq!(
                send(&mut s, &mut b, Command::VmUnbind(Unbind { vm, ..range })),
                Err(Status::NotFound)
            );
        }
        assert_eq!(b.calls.len(), calls);
        assert!(b.unbound.is_empty());
    }

    #[test]
    fn vm_unbind_refuses_unknown_partial_hole_and_foreign_ranges_without_mutation() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        send(
            &mut s,
            &mut b,
            Command::BufferCreate {
                bytes: 2 * PAGE_BYTES,
            },
        )
        .unwrap();
        send(&mut s, &mut b, Command::VmBind(binding())).unwrap();
        let wide = Binding {
            buffer: 2,
            va: VA_START + 2 * PAGE_BYTES,
            bytes: 2 * PAGE_BYTES,
            ..binding()
        };
        send(&mut s, &mut b, Command::VmBind(wide)).unwrap();
        let calls = b.calls.len();
        let range = |va, bytes| Unbind { vm: 1, va, bytes };
        // Unknown: no live binding intersects the range.
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::VmUnbind(range(VA_START + 8 * PAGE_BYTES, PAGE_BYTES))
            ),
            Err(Status::NotFound)
        );
        // Partial: the request names one page of a two-page live binding.
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::VmUnbind(range(VA_START + 2 * PAGE_BYTES, PAGE_BYTES))
            ),
            Err(Status::BadRequest)
        );
        // Hole: the range covers the first binding plus the unmapped page between them.
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::VmUnbind(range(VA_START, 2 * PAGE_BYTES))
            ),
            Err(Status::BadRequest)
        );
        // Overhang past the last live binding is also a hole in the tiling.
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::VmUnbind(range(VA_START, 5 * PAGE_BYTES))
            ),
            Err(Status::BadRequest)
        );
        assert_eq!(b.calls.len(), calls);
        assert!(b.unbound.is_empty());
        assert_eq!(s.bindings.len(), 2);
    }

    #[test]
    fn vm_unbind_accepts_adjacent_bindings_tiled_by_one_request() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        let first = binding();
        let second = Binding {
            buffer: 2,
            va: VA_START + PAGE_BYTES,
            ..binding()
        };
        send(&mut s, &mut b, Command::VmBind(first)).unwrap();
        send(&mut s, &mut b, Command::VmBind(second)).unwrap();
        send(&mut s, &mut b, Command::GroupCreate { vm: 1 }).unwrap();
        let range = Unbind {
            vm: 1,
            va: VA_START,
            bytes: 2 * PAGE_BYTES,
        };
        assert_eq!(
            send(&mut s, &mut b, Command::VmUnbind(range)),
            Ok(Response::VmUnbound(range))
        );
        assert_eq!(b.unbound, std::vec![std::vec![first, second]]);
        assert!(s.bindings.is_empty());
    }

    #[test]
    fn vm_unbind_failure_keeps_the_table_retryable_or_retires_on_uncertainty() {
        let (mut s, mut b) = ready();
        let range = Unbind {
            vm: 1,
            va: VA_START,
            bytes: PAGE_BYTES,
        };
        b.fail = Some(Status::BadRequest);
        assert_eq!(
            send(&mut s, &mut b, Command::VmUnbind(range)),
            Err(Status::BadRequest)
        );
        assert_eq!(s.bindings.len(), 1);
        assert!(!s.retired);
        // The recoverable refusal committed nothing, so the same request retries.
        assert_eq!(
            send(&mut s, &mut b, Command::VmUnbind(range)),
            Ok(Response::VmUnbound(range))
        );
        assert!(s.bindings.is_empty());
        send(&mut s, &mut b, Command::VmBind(binding())).unwrap();
        b.fail = Some(Status::DeviceLost);
        assert_eq!(
            send(&mut s, &mut b, Command::VmUnbind(range)),
            Err(Status::DeviceLost)
        );
        assert!(s.retired);
        // The table still names the binding: only a proven reset may close the session.
        assert_eq!(s.bindings.len(), 1);
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::GetInfo),
            Err(Status::DeviceLost)
        );
        assert_eq!(b.calls.len(), calls);
    }

    #[test]
    fn post_group_create_bind_destroy_is_live_and_retryable() {
        let (mut s, mut b) = ready();
        // Allocation is legal with the group live (j711); the new buffer starts unbound.
        assert_eq!(
            send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }),
            Ok(Response::BufferCreated {
                buffer: 2,
                bytes: PAGE_BYTES
            })
        );
        let second = Binding {
            buffer: 2,
            va: VA_START + MAX_BINDINGS as u64 * PAGE_BYTES,
            ..binding()
        };
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(second)),
            Ok(Response::VmBound)
        );
        // A recoverable refusal leaves the allocation and its mapping retryable.
        b.fail = Some(Status::NoMemory);
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 2 }),
            Err(Status::NoMemory)
        );
        assert!(s.buffers.iter().any(|allocation| allocation.id == 2));
        assert!(s.bindings.iter().any(|binding| binding.buffer == 2));
        b.fail = None;
        // The implicit batch reaches the backend with exactly this buffer's live binding.
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 2 }),
            Ok(Response::BufferDestroyed { buffer: 2 })
        );
        let (id, batch) = b.destroyed.last().cloned().expect("destroy recorded");
        assert_eq!(id, 2);
        assert_eq!(batch, std::vec![second]);
        assert!(s.bindings.iter().all(|binding| binding.buffer != 2));
        assert!(s.buffers.iter().all(|allocation| allocation.id != 2));
    }

    #[test]
    fn failures_do_not_commit_allocations_or_bindings_and_enforce_budget() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        b.fail = Some(Status::NoMemory);
        assert_eq!(
            send(&mut s, &mut b, Command::VmCreate),
            Err(Status::NoMemory)
        );
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        b.fail = Some(Status::NoMemory);
        assert_eq!(
            send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }),
            Err(Status::NoMemory)
        );
        // The failed create reached the backend but committed nothing, so the ID it would
        // have received is still issued to the next successful allocation. — TIMBERDOODLE
        let budget_cap = (MAX_TOTAL_BUFFER_BYTES / MAX_BUFFER_BYTES) as u32;
        for expected in 1..=budget_cap {
            assert_eq!(
                send(
                    &mut s,
                    &mut b,
                    Command::BufferCreate {
                        bytes: MAX_BUFFER_BYTES
                    }
                ),
                Ok(Response::BufferCreated {
                    buffer: expected,
                    bytes: MAX_BUFFER_BYTES
                })
            );
        }
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }),
            Err(Status::NoMemory)
        );
        assert_eq!(b.calls.len(), calls);
        b.fail = Some(Status::NoMemory);
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Err(Status::NoMemory)
        );
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Ok(Response::VmBound)
        );
    }
    #[test]
    fn raised_budget_and_binding_batches_destroy_reclaim_and_keep_ids_monotonic() {
        assert_eq!(MAX_BUFFER_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_TOTAL_BUFFER_BYTES, 128 * 1024 * 1024);
        assert_eq!(MAX_BINDINGS, 256);
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(
            &mut s,
            &mut b,
            Command::BufferCreate {
                bytes: MAX_BUFFER_BYTES,
            },
        )
        .unwrap();
        send(
            &mut s,
            &mut b,
            Command::BufferCreate {
                bytes: MAX_BUFFER_BYTES,
            },
        )
        .unwrap();
        for i in 0..MAX_BINDINGS {
            send(
                &mut s,
                &mut b,
                Command::VmBind(Binding {
                    va: VA_START + i as u64 * PAGE_BYTES,
                    offset: i as u64 * PAGE_BYTES,
                    ..binding()
                }),
            )
            .unwrap();
        }
        send(&mut s, &mut b, Command::GroupCreate { vm: 1 }).unwrap();
        send(&mut s, &mut b, Command::BufferDestroy { buffer: 1 }).unwrap();
        assert_eq!(b.destroyed[0].1.len(), MAX_BINDINGS);
        assert!(s.bindings.is_empty());
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::BufferCreate {
                    bytes: MAX_BUFFER_BYTES
                }
            ),
            Ok(Response::BufferCreated {
                buffer: 3,
                bytes: MAX_BUFFER_BYTES
            })
        );
        assert_eq!(s.close().bytes, MAX_TOTAL_BUFFER_BYTES);
        assert!(core::mem::size_of::<Session>() < 1024);
    }

    #[test]
    fn budget_and_binding_quotas_and_partial_map_failure_bound_the_session() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        assert_eq!(
            send(&mut s, &mut b, Command::GroupCreate { vm: 1 }),
            Err(Status::BadState)
        );
        assert!(b.calls.is_empty());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        let budget_cap = (MAX_TOTAL_BUFFER_BYTES / MAX_BUFFER_BYTES) as u32;
        for _ in 0..budget_cap {
            send(
                &mut s,
                &mut b,
                Command::BufferCreate {
                    bytes: MAX_BUFFER_BYTES,
                },
            )
            .unwrap();
        }
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }),
            Err(Status::NoMemory)
        );
        assert_eq!(b.calls.len(), calls);
        for i in 0..MAX_BINDINGS {
            send(
                &mut s,
                &mut b,
                Command::VmBind(Binding {
                    va: VA_START + i as u64 * PAGE_BYTES,
                    ..binding()
                }),
            )
            .unwrap();
        }
        let calls = b.calls.len();
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::VmBind(Binding {
                    va: VA_START + MAX_BINDINGS as u64 * PAGE_BYTES,
                    ..binding()
                })
            ),
            Err(Status::NoMemory)
        );
        assert_eq!(b.calls.len(), calls);

        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        b.fail = Some(Status::DeviceLost);
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Err(Status::DeviceLost)
        );
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::VmBind(binding())),
            Err(Status::DeviceLost)
        );
        assert_eq!(b.calls.len(), calls);
    }

    #[test]
    fn publication_uncertainty_retires_and_unpublished_failure_can_retry() {
        let (mut s, mut b) = ready();
        b.submit_error = Some(SubmitError::Unpublished(Status::Busy));
        assert_eq!(send(&mut s, &mut b, job(1)), Err(Status::Busy));
        assert_eq!(
            send(&mut s, &mut b, job(1)),
            Ok(Response::Submitted { point: 1 })
        );
        assert_eq!(send(&mut s, &mut b, job(2)), Err(Status::Busy));
        s.complete(1, 0).unwrap();
        b.submit_error = Some(SubmitError::DeviceLost);
        assert_eq!(send(&mut s, &mut b, job(2)), Err(Status::DeviceLost));
        assert_eq!(s.complete(2, 0), Err(Status::DeviceLost));
        assert_eq!(
            send(&mut s, &mut b, Command::SyncRead { group: 1, point: 2 }),
            Err(Status::DeviceLost)
        );
    }

    #[test]
    fn buffer_create_is_on_demand_and_destroy_returns_budget() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        // The fixed four-entry table is gone: eight small allocations all succeed, each with
        // its own monotonically issued ID. — TIMBERDOODLE
        for expected in 1..=8 {
            assert_eq!(
                send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }),
                Ok(Response::BufferCreated {
                    buffer: expected,
                    bytes: PAGE_BYTES
                })
            );
        }
        assert_eq!(s.buffers.len(), 8);
        assert_eq!(s.total_buffer_bytes, 8 * PAGE_BYTES);
        // Destroy returns its bytes to the budget and never reissues its ID.
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 3 }),
            Ok(Response::BufferDestroyed { buffer: 3 })
        );
        assert_eq!(s.total_buffer_bytes, 7 * PAGE_BYTES);
        assert!(!s.buffers.iter().any(|allocation| allocation.id == 3));
        assert_eq!(
            send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }),
            Ok(Response::BufferCreated {
                buffer: 9,
                bytes: PAGE_BYTES
            })
        );
    }

    #[test]
    fn buffer_destroy_rejects_unknown_foreign_and_double_ids_without_backend_calls() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        // No VM yet: the object does not exist and the refusal must not reach the backend.
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 1 }),
            Err(Status::BadState)
        );
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        let calls = b.calls.len();
        // An ID never issued in this session — a foreign or stale ID — is NotFound.
        for buffer in [2, u32::MAX] {
            assert_eq!(
                send(&mut s, &mut b, Command::BufferDestroy { buffer }),
                Err(Status::NotFound)
            );
        }
        assert_eq!(b.calls.len(), calls);
        assert!(b.destroyed.is_empty());
        send(&mut s, &mut b, Command::BufferDestroy { buffer: 1 }).unwrap();
        let calls = b.calls.len();
        // The double destroy is NotFound too: the ID was consumed by the first success.
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 1 }),
            Err(Status::NotFound)
        );
        assert_eq!(b.calls.len(), calls);
        assert_eq!(b.destroyed.len(), 1);
    }

    #[test]
    fn buffer_destroy_is_live_once_the_group_exists() {
        let (mut s, mut b) = ready();
        let calls = b.calls.len();
        // j711: a live group no longer freezes destruction. The backend receives the implicit
        // unbind batch, the metadata retires, and the session keeps serving. — TIMBERDOODLE
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 1 }),
            Ok(Response::BufferDestroyed { buffer: 1 })
        );
        assert_eq!(b.calls.len(), calls + 1);
        assert_eq!(b.destroyed.last().cloned(), Some((1, std::vec![binding()])));
        assert!(s.buffers.is_empty());
        assert_eq!(s.total_buffer_bytes, 0);
        assert_eq!(s.bindings.len(), 0);
        // The group is still live: the next allocation, binding and submission proceed.
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        let rebound = Binding {
            buffer: 2,
            ..binding()
        };
        send(&mut s, &mut b, Command::VmBind(rebound)).unwrap();
        send(&mut s, &mut b, job(1)).unwrap();
    }

    #[test]
    fn buffer_destroy_unbinds_every_mapping_before_releasing_the_allocation() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(
            &mut s,
            &mut b,
            Command::BufferCreate {
                bytes: 2 * PAGE_BYTES,
            },
        )
        .unwrap();
        // Two disjoint mappings of the one buffer, plus one mapping of a second buffer that
        // must survive the destroy. — TIMBERDOODLE
        for (va, offset) in [(VA_START, 0), (VA_START + 4 * PAGE_BYTES, PAGE_BYTES)] {
            send(
                &mut s,
                &mut b,
                Command::VmBind(Binding {
                    va,
                    offset,
                    bytes: PAGE_BYTES,
                    ..binding()
                }),
            )
            .unwrap();
        }
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        send(
            &mut s,
            &mut b,
            Command::VmBind(Binding {
                buffer: 2,
                va: VA_START + MAX_BINDINGS as u64 * PAGE_BYTES,
                ..binding()
            }),
        )
        .unwrap();
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 1 }),
            Ok(Response::BufferDestroyed { buffer: 1 })
        );
        // The backend saw exactly the two live mappings that name buffer 1 — never another
        // buffer's binding, never the whole table. — TIMBERDOODLE
        assert_eq!(b.destroyed.len(), 1);
        let (id, unbound) = &b.destroyed[0];
        assert_eq!(*id, 1);
        assert_eq!(unbound.len(), 2);
        assert!(unbound.iter().all(|binding| binding.buffer == 1));
        assert_eq!(s.bindings.len(), 1);
        assert!(s.bindings.iter().all(|binding| binding.buffer == 2));
        assert!(!s.buffers.iter().any(|allocation| allocation.id == 1));
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::VmBind(Binding {
                    va: VA_START + 16 * PAGE_BYTES,
                    ..binding()
                })
            ),
            Err(Status::NotFound)
        );
    }

    #[test]
    fn buffer_destroy_failure_keeps_the_allocation_retryable_or_retires_on_uncertainty() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        send(&mut s, &mut b, Command::VmBind(binding())).unwrap();
        // A recoverable refusal changed nothing: the mapping is still live and a retry works.
        b.fail = Some(Status::NoMemory);
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 1 }),
            Err(Status::NoMemory)
        );
        assert_eq!(s.buffers.len(), 1);
        assert_eq!(s.total_buffer_bytes, PAGE_BYTES);
        assert_eq!(s.bindings.len(), 1);
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 1 }),
            Ok(Response::BufferDestroyed { buffer: 1 })
        );
        assert!(s.buffers.is_empty());
        assert_eq!(s.total_buffer_bytes, 0);
        assert!(s.bindings.is_empty());

        // Uncertainty — a partial unmap or a failed live flush — retires the session before
        // any metadata commits, so a later destroy can never double-free the backing.
        // — TIMBERDOODLE
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        b.fail = Some(Status::DeviceLost);
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 1 }),
            Err(Status::DeviceLost)
        );
        assert_eq!(s.buffers.len(), 1);
        assert_eq!(s.total_buffer_bytes, PAGE_BYTES);
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::BufferDestroy { buffer: 1 }),
            Err(Status::DeviceLost)
        );
        assert_eq!(b.calls.len(), calls);
    }
    #[test]
    fn close_is_idempotent_and_reports_exact_counts() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        send(
            &mut s,
            &mut b,
            Command::BufferCreate {
                bytes: 2 * PAGE_BYTES,
            },
        )
        .unwrap();
        for (buffer, va) in [(1, VA_START), (2, VA_START + PAGE_BYTES)] {
            send(
                &mut s,
                &mut b,
                Command::VmBind(Binding {
                    buffer,
                    va,
                    ..binding()
                }),
            )
            .unwrap();
        }
        send(&mut s, &mut b, Command::GroupCreate { vm: 1 }).unwrap();
        send(&mut s, &mut b, job(1)).unwrap();
        send(&mut s, &mut b, Command::FenceWait { group: 1, point: 1 }).unwrap();
        assert_eq!(s.waits.iter().flatten().count(), 1);

        // Close answers with the metadata it dropped, exactly once. — TIMBERDOODLE
        assert_eq!(
            s.close(),
            SessionRelease {
                buffers: 2,
                bindings: 2,
                waits: 1,
                heaps: 0,
                bytes: 3 * PAGE_BYTES,
            }
        );
        assert!(s.closed);
        assert!(s.retired);
        assert!(s.buffers.is_empty());
        assert!(s.bindings.is_empty());
        assert!(s.waits.iter().all(Option::is_none));
        assert_eq!(s.total_buffer_bytes, 0);
        assert!(!s.vm && !s.group);
        // A second close is a no-op, not a second release. — TIMBERDOODLE
        assert_eq!(s.close(), SessionRelease::NONE);
        assert!(s.close().is_empty());
        // The backend is untouched by close alone: retiring is the caller's separate step,
        // ordered after a proven reset, so close can never smuggle in an unquiesced free.
        assert_eq!(b.retire_calls, 0);
    }

    #[test]
    fn dispatch_after_close_is_device_lost_and_never_reaches_the_backend() {
        let (mut s, mut b) = ready();
        assert!(!s.close().is_empty());
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::GetInfo),
            Err(Status::DeviceLost)
        );
        assert_eq!(
            send(&mut s, &mut b, Command::VmCreate),
            Err(Status::DeviceLost)
        );
        assert_eq!(b.calls.len(), calls);
        assert_eq!(s.complete(1, 0), Err(Status::DeviceLost));
    }

    /// A backend may not report a release it did not perform: the default `retire` answers
    /// nothing and leaves the backend's claims exactly as they were. — TIMBERDOODLE
    #[test]
    fn default_retire_never_invents_success() {
        #[derive(Default)]
        struct Passive {
            claims: u32,
        }
        impl Backend for Passive {
            fn info(&mut self) -> Result<DeviceInfo, Status> {
                Err(Status::NotSupported)
            }
            fn create_vm(&mut self) -> Result<(), Status> {
                Err(Status::NotSupported)
            }
            fn create_buffer(&mut self, _: u32, _: u64) -> Result<(), Status> {
                Err(Status::NotSupported)
            }
            fn bind(&mut self, _: Binding) -> Result<(), Status> {
                Err(Status::NotSupported)
            }
            fn destroy_buffer(&mut self, _: u32, _: &[Binding]) -> Result<(), Status> {
                Err(Status::NotSupported)
            }
            fn unbind(&mut self, _: &[Binding]) -> Result<(), Status> {
                Err(Status::NotSupported)
            }
            fn create_group(&mut self) -> Result<(), Status> {
                Err(Status::NotSupported)
            }
            fn submit(&mut self, _: Submission) -> Result<(), SubmitError> {
                Err(SubmitError::Unpublished(Status::NotSupported))
            }
        }
        let mut passive = Passive { claims: 3 };
        passive.retire();
        assert_eq!(passive.claims, 3);
    }
    #[test]
    fn group_watch_requires_ownership_refuses_absence_and_terminal_snapshot_retires() {
        let mut s = Session::default();
        let mut b = Fake::default();
        assert_eq!(
            send(&mut s, &mut b, Command::GroupWatch { group: 1 }),
            Err(Status::BadState)
        );
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, Command::GroupCreate { vm: 1 }).unwrap();
        assert_eq!(
            send(&mut s, &mut b, Command::GroupWatch { group: 2 }),
            Err(Status::NotFound)
        );
        assert_eq!(
            send(&mut s, &mut b, Command::GroupWatch { group: 1 }),
            Err(Status::NotSupported)
        );
        b.group_report = Some(crate::GroupReport::USABLE);
        assert_eq!(
            send(&mut s, &mut b, Command::GroupWatch { group: 1 }),
            Ok(Response::GroupWatch {
                group: 1,
                report: crate::GroupReport::USABLE
            })
        );
        b.group_report = Some(crate::GroupReport::TIMED_OUT);
        assert_eq!(
            send(&mut s, &mut b, Command::GroupWatch { group: 1 }),
            Ok(Response::GroupWatch {
                group: 1,
                report: crate::GroupReport::TIMED_OUT
            })
        );
        let calls = b.calls.len();
        assert_eq!(
            send(&mut s, &mut b, Command::GetInfo),
            Err(Status::DeviceLost)
        );
        assert_eq!(b.calls.len(), calls);
    }

    //j754
    fn heap(chunk_bytes: u32, initial_chunks: u32, max_chunks: u32) -> Command {
        Command::HeapCreate(HeapSpec {
            vm: 1,
            chunk_bytes,
            initial_chunks,
            max_chunks,
            target_in_flight: 65535,
        })
    }

    #[test]
    fn heaps_need_a_vm_their_own_vm_and_a_backend_that_supports_them() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        assert_eq!(
            send(&mut s, &mut b, heap(128 * 1024, 1, 1)),
            Err(Status::BadState)
        );
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        let mut spec = HeapSpec {
            vm: 2,
            chunk_bytes: 128 * 1024,
            initial_chunks: 1,
            max_chunks: 1,
            target_in_flight: 1,
        };
        assert_eq!(
            send(&mut s, &mut b, Command::HeapCreate(spec)),
            Err(Status::NotFound)
        );
        spec.vm = 1;
        // The default backend refuses, and the refusal charges and issues nothing.
        assert_eq!(
            send(&mut s, &mut b, Command::HeapCreate(spec)),
            Err(Status::NotSupported)
        );
        assert_eq!(s.total_buffer_bytes, 0);
        b.heaps = true;
        assert!(matches!(
            send(&mut s, &mut b, Command::HeapCreate(spec)),
            Ok(Response::HeapCreated { heap: 1, .. })
        ));
        // Before any group, as Panthor allows.
        assert!(!s.group);
    }

    #[test]
    fn heaps_charge_the_shared_budget_and_return_it_on_destroy() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        b.heaps = true;
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        let two_mib = 2 * 1024 * 1024;
        // Gallium's default: 2 MiB x 5, up to 64.
        assert!(matches!(
            send(&mut s, &mut b, heap(two_mib, 5, 64)),
            Ok(Response::HeapCreated { heap: 1, .. })
        ));
        let charged = PAGE_BYTES + 5 * u64::from(two_mib);
        assert_eq!(s.total_buffer_bytes, charged);
        // Buffers and heaps draw on one budget: what is left is all a buffer may take.
        let left = MAX_TOTAL_BUFFER_BYTES - charged;
        assert!(left < MAX_BUFFER_BYTES * 2);
        send(
            &mut s,
            &mut b,
            Command::BufferCreate {
                bytes: MAX_BUFFER_BYTES,
            },
        )
        .unwrap();
        assert_eq!(
            send(
                &mut s,
                &mut b,
                Command::BufferCreate {
                    bytes: MAX_BUFFER_BYTES
                }
            ),
            Err(Status::NoMemory)
        );
        assert_eq!(
            send(&mut s, &mut b, Command::HeapDestroy { heap: 1 }),
            Ok(Response::HeapDestroyed {
                heap: 1,
                bytes: charged
            })
        );
        assert_eq!(s.total_buffer_bytes, MAX_BUFFER_BYTES);
        send(
            &mut s,
            &mut b,
            Command::BufferCreate {
                bytes: MAX_BUFFER_BYTES,
            },
        )
        .unwrap();
    }

    #[test]
    fn heap_ids_are_monotonic_never_reused_and_capped() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        b.heaps = true;
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        for id in 1..=MAX_HEAPS as u32 {
            assert!(matches!(
                send(&mut s, &mut b, heap(128 * 1024, 1, 4)),
                Ok(Response::HeapCreated { heap, .. }) if heap == id
            ));
        }
        assert_eq!(
            send(&mut s, &mut b, heap(128 * 1024, 1, 4)),
            Err(Status::NoMemory)
        );
        send(&mut s, &mut b, Command::HeapDestroy { heap: 3 }).unwrap();
        assert_eq!(
            send(&mut s, &mut b, Command::HeapDestroy { heap: 3 }),
            Err(Status::NotFound)
        );
        assert_eq!(
            send(&mut s, &mut b, Command::HeapDestroy { heap: 99 }),
            Err(Status::NotFound)
        );
        let next = MAX_HEAPS as u32 + 1;
        assert!(matches!(
            send(&mut s, &mut b, heap(128 * 1024, 1, 4)),
            Ok(Response::HeapCreated { heap, .. }) if heap == next
        ));
    }

    #[test]
    fn a_refused_heap_charges_nothing_and_a_bad_placement_retires() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        b.heaps = true;
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        b.fail = Some(Status::NoMemory);
        assert_eq!(
            send(&mut s, &mut b, heap(128 * 1024, 2, 2)),
            Err(Status::NoMemory)
        );
        assert_eq!(
            (s.total_buffer_bytes, s.heaps.len(), s.next_heap),
            (0, 0, 0)
        );
        // A placement outside the window means the backend mapped something it should not have:
        // that cannot be refused, so the session retires.
        b.bad_placement = true;
        assert_eq!(
            send(&mut s, &mut b, heap(128 * 1024, 2, 2)),
            Err(Status::DeviceLost)
        );
        assert!(s.retired);
        assert_eq!(s.heaps.len(), 0);
    }

    #[test]
    fn a_failed_heap_destroy_keeps_the_heap_and_close_counts_heaps() {
        let (mut s, mut b) = (Session::default(), Fake::default());
        b.heaps = true;
        send(&mut s, &mut b, Command::VmCreate).unwrap();
        send(&mut s, &mut b, heap(256 * 1024, 2, 8)).unwrap();
        send(&mut s, &mut b, Command::BufferCreate { bytes: PAGE_BYTES }).unwrap();
        b.fail = Some(Status::NoMemory);
        assert_eq!(
            send(&mut s, &mut b, Command::HeapDestroy { heap: 1 }),
            Err(Status::NoMemory)
        );
        assert_eq!(s.heaps.len(), 1);
        let release = s.close();
        assert_eq!(release.heaps, 1);
        assert_eq!(release.buffers, 1);
        assert_eq!(release.bytes, PAGE_BYTES + 2 * 256 * 1024 + PAGE_BYTES);
        assert_eq!(s.close(), SessionRelease::NONE);
    }
}
