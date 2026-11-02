//j733
//j742
//j743
//j752
//j754

//! One brokered G6b session leases the prepared AS1/CSG0 after the boot proof. G8d-b makes
//! the lane re-grantable: each `g` on the long-lived handoff is one new session, and only a
//! contained close that proved quiescence lets the driver read the next one — a poisoned
//! close retains everything and never serves again. G8a gives the session an on-demand buffer
//! table: BufferDestroy unbinds every VA range the session still holds for one allocation,
//! flushes AS1, and only then releases that allocation's CPU mapping and driver VMO handle.
//! A confirmed soft reset releases whatever the channel still owned; a failed reset retains
//! every byte, because the resident driver cannot prove the GPU stopped. G8d-a makes that
//! tail a state machine (`drv_gpu::contain`): stop accepting, quiesce, close the session with
//! exact counts, then retire the driver's mappings — or poison and retain everything.
//! G8b adds live VmUnbind: clear exactly the session's page-table entries, then invalidate
//! only their packed MMU region. The serve loop is synchronous, so same-AS work is already
//! finished when the request is dispatched; no queue, MCU or reset operation is involved.

use alloc::vec::Vec;
use kumo_abi::Rights;

use super::*;
use drv_gpu::contain::Containment;
use drv_gpu::{client_job_wrapper, flush_range, protocol, JobSlot, JOB_WRAPPER_WORDS};
use protocol::{
    session::{Backend, Session, SubmitError, MAX_WAITS},
    Binding, Reply as GpuReply, Request, Response, Status as GpuStatus, Submission, SyncState,
    REQUEST_LEN,
};

/// One live client allocation: the session's object ID and the driver's own DMA mapping.
/// IDs are the session's, not array indices, so the table can grow, shrink and reorder.
/// — TIMBERDOODLE
struct BufferRecord {
    id: u32,
    handle: Handle,
    va: u64,
    bytes: u64,
    pages: Vec<u64>,
}

impl BufferRecord {
    /// Only before GPU publication, after checked unbind/flush, or after confirmed reset.
    /// — KESTREL
    fn release(self) {
        let _ = kumo_rt::vmar_unmap(self.va, self.bytes);
        let _ = kumo_rt::handle_close(self.handle);
    }
}

//j754
/// G7q-a: one piece of a tiler heap, the context page or one chunk, as its own VMO, so a chunk
/// added on tiler OOM (G7q-b) is released the same way. No CPU alias outlives creation: the
/// client never sees heap memory, and the driver writes only chunk headers. — TIMBERDOODLE
struct HeapPiece {
    handle: Handle,
    gpu_va: u64,
    bytes: u64,
    pages: Vec<u64>,
}

impl HeapPiece {
    /// Zeroed, cleaned pages from the kernel (as `create_buffer`'s are); nothing is mapped yet.
    fn allocate(gpu_va: u64, bytes: u64) -> Result<Self, GpuStatus> {
        let mut pages = Vec::new();
        let count = (bytes / PAGE_SIZE) as usize;
        pages
            .try_reserve_exact(count)
            .map_err(|_| GpuStatus::NoMemory)?;
        pages.resize(count, 0);
        let handle = valid_handle(kumo_rt::vmo_create_paged(bytes, &mut pages))
            .ok_or(GpuStatus::NoMemory)?;
        Ok(Self {
            handle,
            gpu_va,
            bytes,
            pages,
        })
    }

    /// Write a chunk header's link word through a transient NC alias of its first page. The
    /// rest of the 64-byte header stays the kernel's zeroes, as Panthor requires.
    fn write_link(&self, link: u64) -> Result<(), GpuStatus> {
        let (status, va) = kumo_rt::vmar_map_anywhere(
            self.handle,
            PAGE_SIZE,
            (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
        );
        if status != 0 {
            return Err(GpuStatus::NoMemory);
        }
        // SAFETY: the first word of the page just mapped READ|WRITE at `va`.
        unsafe { (va as *mut u64).write_volatile(link) };
        dsb();
        let _ = kumo_rt::vmar_unmap(va, PAGE_SIZE);
        Ok(())
    }

    fn range(&self) -> UnmapRange {
        UnmapRange {
            va: self.gpu_va,
            len: self.bytes,
        }
    }

    /// Only before the piece is mapped, after a checked unmap and flush, or after a confirmed
    /// reset.
    fn release(self) {
        let _ = kumo_rt::handle_close(self.handle);
    }
}

/// One live heap: its context page first, then its chunks in list order from the head.
struct HeapMemory {
    id: u32,
    pieces: Vec<HeapPiece>,
}

impl HeapMemory {
    fn release(self) {
        for piece in self.pieces {
            piece.release();
        }
    }
}

struct Pending {
    point: u64,
    seqno: u64,
    insert: u64,
    slot: JobSlot,
    words: [u64; JOB_WRAPPER_WORDS],
}

struct Hardware<'a> {
    device: &'a mut Device,
    shader_present: u64,
    mmu_features: u32,
    tiler_features: u32,
    texture_features: [u32; 4],
    core_features: u32,
    thread_max_threads: u32,
    thread_max_workgroup_size: u32,
    thread_features: u32,
    memory: protocol::DeviceMemory,
    tables: &'a mut TableDma,
    arena: &'a mut TableArena,
    root: u64,
    work: Mapping,
    input: u64,
    output: u64,
    windows: [McuWindow; 3],
    csg: CsgControl,
    cs: CsControl,
    buffers: Vec<BufferRecord>,
    reply_grant: Option<Handle>,
    pending: Option<Pending>,
    waiters: [Option<(u64, Handle)>; MAX_WAITS],
    group_report: protocol::GroupReport,
    status_vmo: Option<Handle>,
    heaps: Vec<HeapMemory>,
}

impl Backend for Hardware<'_> {
    //j754
    fn create_heap(
        &mut self,
        id: u32,
        spec: protocol::HeapSpec,
    ) -> Result<protocol::HeapPlacement, GpuStatus> {
        let chunk_bytes = u64::from(spec.chunk_bytes);
        let count = spec.initial_chunks as usize;
        self.heaps.try_reserve(1).map_err(|_| GpuStatus::NoMemory)?;
        let mut pieces: Vec<HeapPiece> = Vec::new();
        pieces
            .try_reserve_exact(count + 1)
            .map_err(|_| GpuStatus::NoMemory)?;
        // One contiguous run: the context page, then the chunks. Growth (G7q-b) places single
        // chunks in whatever gaps destroyed heaps left.
        let mut live: Vec<(u64, u64)> = Vec::new();
        live.try_reserve(self.heaps.iter().map(|heap| heap.pieces.len()).sum())
            .map_err(|_| GpuStatus::NoMemory)?;
        for piece in self.heaps.iter().flat_map(|heap| heap.pieces.iter()) {
            live.push((piece.gpu_va, piece.bytes));
        }
        let run = protocol::HEAP_CONTEXT_BYTES + chunk_bytes * count as u64;
        let base = drv_gpu::first_fit(&live, protocol::HEAP_VA_START, protocol::HEAP_VA_END, run)
            .ok_or(GpuStatus::NoMemory)?;
        // Allocate and link everything before any page table changes, so a refusal here maps
        // nothing and keeps nothing.
        let fail = |pieces: Vec<HeapPiece>, status| {
            for piece in pieces {
                piece.release();
            }
            Err(status)
        };
        match HeapPiece::allocate(base, protocol::HEAP_CONTEXT_BYTES) {
            Ok(context) => pieces.push(context),
            Err(status) => return fail(pieces, status),
        }
        // Panthor links each new initial chunk to the previous head and returns the last one
        // allocated, so the chunk at the highest address is the head and the first one ends
        // the list. — TIMBERDOODLE
        let mut previous: Option<u64> = None;
        for index in 0..count {
            let va = base + protocol::HEAP_CONTEXT_BYTES + chunk_bytes * index as u64;
            let chunk = match HeapPiece::allocate(va, chunk_bytes) {
                Ok(chunk) => chunk,
                Err(status) => return fail(pieces, status),
            };
            let link = previous.map_or(0, |next| drv_gpu::heap_chunk_link(next, chunk_bytes));
            if let Err(status) = chunk.write_link(link) {
                chunk.release();
                return fail(pieces, status);
            }
            previous = Some(va);
            pieces.push(chunk);
        }
        let head = previous.expect("at least one initial chunk");
        // List order: context, then chunks from the head down. — TIMBERDOODLE
        pieces[1..].reverse();
        let attrs = PageAttrs {
            cached: true,
            readonly: false,
            executable: false,
        };
        for piece in &pieces {
            if let Err(error) = drv_gpu::map_pages(
                self.tables,
                self.arena,
                self.root,
                piece.gpu_va,
                &piece.pages,
                attrs,
            ) {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: client heap {} map failed: {:?}\n", id, error),
                );
                // Some tables may name these pages now: keep them for retire, after reset.
                self.heaps.push(HeapMemory { id, pieces });
                return Err(GpuStatus::DeviceLost);
            }
        }
        dsb();
        if flush_page_tables(self.device, GROUP_AS).is_err() {
            self.heaps.push(HeapMemory { id, pieces });
            return Err(GpuStatus::DeviceLost);
        }
        log(
            log_level::NOTICE,
            format_args!(
                "drv-gpu: client heap {} context 0x{:x} chunks {}x{} head 0x{:x} (max {}, in flight {})\n",
                id, base, count, chunk_bytes, head, spec.max_chunks, spec.target_in_flight
            ),
        );
        self.heaps.push(HeapMemory { id, pieces });
        Ok(protocol::HeapPlacement {
            context_va: base,
            first_chunk_va: head,
        })
    }

    fn destroy_heap(&mut self, id: u32) -> Result<(), GpuStatus> {
        let index = self
            .heaps
            .iter()
            .position(|heap| heap.id == id)
            .ok_or(GpuStatus::NotFound)?;
        let mut ranges: Vec<UnmapRange> = Vec::new();
        ranges
            .try_reserve(self.heaps[index].pieces.len())
            .map_err(|_| GpuStatus::NoMemory)?;
        ranges.extend(self.heaps[index].pieces.iter().map(HeapPiece::range));
        // The same rule as destroy_buffer: a refusal changes no page, while a failed live
        // flush may leave cached mappings, so it is DeviceLost and containment cleans up.
        if let Err(error) = unmap_ranges(self.tables, self.root, &ranges) {
            log(
                log_level::ERR,
                format_args!("drv-gpu: client heap {} unmap failed: {:?}\n", id, error),
            );
            return Err(GpuStatus::DeviceLost);
        }
        dsb();
        flush_page_tables(self.device, GROUP_AS).map_err(|_| GpuStatus::DeviceLost)?;
        let heap = self.heaps.swap_remove(index);
        let pieces = heap.pieces.len();
        heap.release();
        log(
            log_level::NOTICE,
            format_args!(
                "drv-gpu: client heap {} destroyed; {} pieces unmapped\n",
                id, pieces
            ),
        );
        Ok(())
    }

    fn watch_group(&mut self) -> Result<protocol::GroupReport, GpuStatus> {
        if self.status_vmo.is_none() {
            let handle = valid_handle(kumo_rt::vmo_create(PAGE_SIZE)).ok_or(GpuStatus::NoMemory)?;
            self.status_vmo = Some(handle);
            if !self.write_group_report() {
                return Err(GpuStatus::DeviceLost);
            }
        }
        let grant = valid_handle(kumo_rt::handle_duplicate(
            self.status_vmo.unwrap(),
            Rights::READ | Rights::DUPLICATE | Rights::TRANSFER,
        ))
        .ok_or(GpuStatus::NoMemory)?;
        self.reply_grant = Some(grant);
        Ok(self.group_report)
    }
    fn info(&mut self) -> Result<protocol::DeviceInfo, GpuStatus> {
        // GPU_ID at aperture +0, already checked by probe before this server starts.
        Ok(protocol::DeviceInfo {
            gpu_id: read_aperture(0),
            cs_features: self.cs.features,
        })
    }
    fn topology(&mut self) -> Result<protocol::DeviceTopology, GpuStatus> {
        // The exact probe-attested mask also configured this session's CSG. No client
        // field selects a register, core mask or hardware slot. — KESTREL
        Ok(protocol::DeviceTopology {
            shader_present: self.shader_present,
        })
    }
    fn mmu_features(&mut self) -> Result<protocol::DeviceMmu, GpuStatus> {
        // The same MMU_FEATURES word bring-up read, and already required to report 48 virtual
        // address bits before this server ran. No client field selects a register, address
        // space or translation policy. — SAKER
        Ok(protocol::DeviceMmu {
            mmu_features: self.mmu_features,
        })
    }
    fn tiler_features(&mut self) -> Result<protocol::DeviceTiler, GpuStatus> {
        // A zero word means the tiler's shape is unknown, not that the device is unusable, so
        // this degrades to NotSupported and the session survives. The compute path this server
        // actually serves never depended on tiler properties. — SAKER
        if self.tiler_features == 0 {
            return Err(GpuStatus::NotSupported);
        }
        Ok(protocol::DeviceTiler {
            tiler_features: self.tiler_features,
        })
    }
    fn texture_features(&mut self) -> Result<protocol::DeviceTexture, GpuStatus> {
        // Element 0 is the optional-absence gate, exactly as in the tiler word; the other three
        // words were attested by the same powered read, so they cross verbatim, zero included.
        // — TIMBERDOODLE
        if self.texture_features[0] == 0 {
            return Err(GpuStatus::NotSupported);
        }
        Ok(protocol::DeviceTexture {
            texture_features: self.texture_features,
        })
    }
    fn core_features(&mut self) -> Result<protocol::DeviceCore, GpuStatus> {
        // Required in the query sense: bits [7:0] are the GPU variant upstream derives to
        // pick its model table entry, so the query must succeed. The word crosses verbatim,
        // zero included — the board measures exactly 0x00000000, and variant byte 0 is the
        // pinned table's G610 entry. — TIMBERDOODLE
        Ok(protocol::DeviceCore {
            core_features: self.core_features,
        })
    }
    fn thread_properties(&mut self) -> Result<protocol::DeviceThreads, GpuStatus> {
        // All three zero means the thread group is absent, which is the recoverable
        // NotSupported status; any other combination crosses verbatim, zeros included, and
        // the consumer decides whether the derivation is usable. Nothing is derived here.
        // — TIMBERDOODLE
        if self.thread_max_threads == 0
            && self.thread_max_workgroup_size == 0
            && self.thread_features == 0
        {
            return Err(GpuStatus::NotSupported);
        }
        Ok(protocol::DeviceThreads {
            max_threads: self.thread_max_threads,
            max_workgroup_size: self.thread_max_workgroup_size,
            thread_features: self.thread_features,
        })
    }
    fn memory_properties(&mut self) -> Result<protocol::DeviceMemory, GpuStatus> {
        // Required and verbatim: the three words bring-up read in its powered block and the
        // coherency protocol this driver programmed. COHERENCY_FEATURES reads zero on the
        // Orange Pi, which is data, not absence. Nothing is derived here. — TIMBERDOODLE
        Ok(self.memory)
    }
    fn create_vm(&mut self) -> Result<(), GpuStatus> {
        log(
            log_level::NOTICE,
            format_args!("drv-gpu: client VM 1 leases AS1\n"),
        );
        Ok(())
    }

    fn create_buffer(&mut self, id: u32, bytes: u64) -> Result<(), GpuStatus> {
        self.buffers
            .try_reserve(1)
            .map_err(|_| GpuStatus::NoMemory)?;
        let mut pages = Vec::new();
        let count = (bytes / PAGE_SIZE) as usize;
        pages
            .try_reserve_exact(count)
            .map_err(|_| GpuStatus::NoMemory)?;
        pages.resize(count, 0);
        let handle = valid_handle(kumo_rt::vmo_create_paged(bytes, &mut pages))
            .ok_or(GpuStatus::NoMemory)?;
        let (status, va) = kumo_rt::vmar_map_anywhere(
            handle,
            bytes,
            (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
        );
        if status != 0 {
            let _ = kumo_rt::handle_close(handle);
            return Err(GpuStatus::NoMemory);
        }
        let record = BufferRecord {
            id,
            handle,
            va,
            bytes,
            pages,
        };
        let Some(grant) = valid_handle(kumo_rt::handle_duplicate(
            handle,
            kumo_abi::Rights::READ
                | kumo_abi::Rights::WRITE
                | kumo_abi::Rights::DUPLICATE
                | kumo_abi::Rights::TRANSFER,
        )) else {
            record.release();
            return Err(GpuStatus::DeviceLost);
        };
        // The kernel already zeroed and cleaned every page before creating this NC alias.
        // — KESTREL
        self.buffers.push(record);
        self.reply_grant = Some(grant);
        log(
            log_level::INFO,
            format_args!("drv-gpu: client buffer {} shared, {} bytes\n", id, bytes),
        );
        Ok(())
    }

    fn bind(&mut self, b: Binding) -> Result<(), GpuStatus> {
        let record = self
            .buffers
            .iter()
            .find(|record| record.id == b.buffer)
            .ok_or(GpuStatus::NotFound)?;
        let first = (b.offset / PAGE_SIZE) as usize;
        let end = first + (b.bytes / PAGE_SIZE) as usize;
        let pages = record.pages.get(first..end).ok_or(GpuStatus::BadRequest)?;
        let attrs = PageAttrs {
            cached: !b.flags.contains(protocol::BindFlags::UNCACHED),
            readonly: b.flags.contains(protocol::BindFlags::READ_ONLY),
            executable: !b.flags.contains(protocol::BindFlags::NO_EXECUTE),
        };
        // A partial table update or failed live flush retires this session; its ranges are
        // never reused. Reset containment runs before any handle is closed. — TIMBERDOODLE
        if let Err(error) =
            drv_gpu::map_pages(self.tables, self.arena, self.root, b.va, pages, attrs)
        {
            log(
                log_level::ERR,
                format_args!("drv-gpu: client map failed: {:?}\n", error),
            );
            return Err(GpuStatus::DeviceLost);
        }
        dsb();
        flush_page_tables(self.device, GROUP_AS).map_err(|_| GpuStatus::DeviceLost)?;
        Ok(())
    }

    fn unbind(&mut self, bindings: &[Binding]) -> Result<(), GpuStatus> {
        // Fence invariant: the serve loop dispatches one request at a time and finishes the
        // pending job before it reads the next frame, so no same-AS work is in flight when
        // this runs. If that loop ever overlaps submission with request handling, this
        // method must wait for the group's queue to drain first. RESEARCH/017's live path is
        // the MMU's own packed-region invalidation; no CSG, JS, MCU, doorbell or reset
        // operation is added here. — TIMBERDOODLE
        if bindings.is_empty() {
            return Err(GpuStatus::BadRequest);
        }
        let mut ranges: Vec<UnmapRange> = Vec::new();
        // NoMemory is recoverable: nothing was cleared, so the session keeps the bindings.
        if ranges.try_reserve(bindings.len()).is_err() {
            return Err(GpuStatus::NoMemory);
        }
        let mut start = u64::MAX;
        let mut end = 0u64;
        for binding in bindings {
            let binding_end = binding
                .va
                .checked_add(binding.bytes)
                .ok_or(GpuStatus::BadRequest)?;
            start = start.min(binding.va);
            end = end.max(binding_end);
            ranges.push(UnmapRange {
                va: binding.va,
                len: binding.bytes,
            });
        }
        // The library preflights every page before its first store, so a refusal changes no
        // page. A live flush failure could leave cleared mappings the GPU still caches:
        // that is DeviceLost, and `serve`'s reset containment owns the cleanup.
        if let Err(error) = unmap_ranges(self.tables, self.root, &ranges) {
            log(
                log_level::ERR,
                format_args!("drv-gpu: client unbind failed: {:?}\n", error),
            );
            return Err(GpuStatus::DeviceLost);
        }
        dsb();
        flush_range(self.device, GROUP_AS, start, end - start).map_err(|error| {
            log(
                log_level::ERR,
                format_args!("drv-gpu: client unbind flush failed: {:?}\n", error),
            );
            GpuStatus::DeviceLost
        })?;
        log(
            log_level::INFO,
            format_args!(
                "drv-gpu: client unbound {} mapping(s) over {} bytes\n",
                bindings.len(),
                end - start
            ),
        );
        Ok(())
    }

    fn destroy_buffer(&mut self, id: u32, bindings: &[Binding]) -> Result<(), GpuStatus> {
        let index = self
            .buffers
            .iter()
            .position(|record| record.id == id)
            .ok_or(GpuStatus::NotFound)?;
        let mut ranges: Vec<UnmapRange> = Vec::new();
        // NoMemory is recoverable: the session keeps the allocation and its bindings, and the
        // caller may retry. — TIMBERDOODLE
        if ranges.try_reserve(bindings.len()).is_err() {
            return Err(GpuStatus::NoMemory);
        }
        ranges.extend(bindings.iter().map(|binding| UnmapRange {
            va: binding.va,
            len: binding.bytes,
        }));
        // The library preflights every page of every range before its first store, so a
        // refusal there changes no page. A live flush failure, by contrast, could leave
        // cleared mappings the GPU still caches: that is DeviceLost, and `serve`'s reset
        // containment owns the cleanup. — TIMBERDOODLE
        if let Err(error) = unmap_ranges(self.tables, self.root, &ranges) {
            log(
                log_level::ERR,
                format_args!("drv-gpu: client unmap failed: {:?}\n", error),
            );
            return Err(GpuStatus::DeviceLost);
        }
        if !ranges.is_empty() {
            dsb();
            flush_page_tables(self.device, GROUP_AS).map_err(|_| GpuStatus::DeviceLost)?;
        }
        // The GPU can no longer reach these pages: release the CPU mapping and the driver's
        // VMO handle now. The client's granted duplicate is the client's own. — TIMBERDOODLE
        let record = self.buffers.swap_remove(index);
        record.release();
        log(
            log_level::INFO,
            format_args!(
                "drv-gpu: client buffer {} destroyed; {} binding(s) unbound\n",
                id,
                bindings.len()
            ),
        );
        Ok(())
    }

    fn create_group(&mut self) -> Result<(), GpuStatus> {
        log(
            log_level::NOTICE,
            format_args!(
                "drv-gpu: client group 1 leases CSG0 queue0, hardware seqno base={}\n",
                BRINGUP_JOBS
            ),
        );
        Ok(())
    }

    fn submit(&mut self, job: Submission) -> Result<(), SubmitError> {
        if self.pending.is_some() {
            return Err(SubmitError::Unpublished(GpuStatus::Busy));
        }
        let seqno = BRINGUP_JOBS
            .checked_add(job.signal_point)
            .ok_or(SubmitError::Unpublished(GpuStatus::BadRequest))?;
        dsb();
        let words = client_job_wrapper(
            CsFeatures::decode(self.cs.features),
            JobCall {
                latest_flush: latest_flush_id(self.device),
                stream_va: job.stream_va,
                stream_len: job.stream_bytes,
                sync_va: AS1_SYNC_VA,
            },
        )
        .map_err(|_| SubmitError::Unpublished(GpuStatus::BadRequest))?;
        let iface = QueueIface::at(MCU_IO_PAGES_VA);
        let mut io = SharedGlb::new(self.device, self.input, self.output, self.windows);
        let insert = queue_insert(&mut io, iface);
        let extract = queue_extract(&mut io, iface);
        let slot = job_slot(insert, extract, RING_SIZE)
            .ok_or(SubmitError::Unpublished(GpuStatus::Busy))?;
        // Prepare only. Publish after the acceptance reply, with the interrupt already armed.
        self.pending = Some(Pending {
            point: job.signal_point,
            seqno,
            insert,
            slot,
            words,
        });
        Ok(())
    }

    /// Only called by [`Containment::run`], after a confirmed reset and after the session has
    /// been closed. The reset stopped the MCU and every group, so no GPU state can still
    /// reference these pages; release each CPU mapping and the driver's VMO handle. Nothing
    /// here runs on the failed-reset path: unproven quiescence retains every byte.
    /// — TIMBERDOODLE
    fn retire(&mut self) {
        for record in self.buffers.drain(..) {
            record.release();
        }
        for heap in self.heaps.drain(..) {
            heap.release();
        }
        self.pending = None;
    }
}

impl Hardware<'_> {
    fn write_group_report(&mut self) -> bool {
        let Some(handle) = self.status_vmo else {
            return true;
        };
        let bytes = self.group_report.encode(1);
        kumo_rt::vmo_write(handle, 0, bytes.as_ptr(), bytes.len()) == 0
    }
    fn fail_group(&mut self, report: protocol::GroupReport) {
        if self.group_report.state == protocol::GroupState::Usable {
            self.group_report = report;
            if !self.write_group_report() {
                log(
                    log_level::ERR,
                    format_args!("drv-gpu: failed to publish group status\n"),
                );
            }
        }
    }
    /// Arm a wait for `point`: create the wake-channel pair and hand the read end back for
    /// the reply. None means the pair could not be created or the bounded table is full; the
    /// caller reports NoMemory rather than an armed reply without its handle. — TIMBERDOODLE
    fn arm_fence_wait(&mut self, point: u64) -> Option<Handle> {
        if !self.waiters.iter().any(|slot| slot.is_none()) {
            return None;
        }
        let (read_raw, write_raw) = kumo_rt::channel_create_pair();
        let (Some(read), Some(write)) = (valid_handle(read_raw), valid_handle(write_raw)) else {
            for raw in [read_raw, write_raw] {
                if let Some(handle) = valid_handle(raw) {
                    let _ = kumo_rt::handle_close(handle);
                }
            }
            return None;
        };
        let free = self
            .waiters
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("free slot checked above");
        *free = Some((point, write));
        //j722
        // Per-frame lines belong at INFO: a churn run arms and wakes one fence per frame,
        // and at NOTICE the flood is echoed into the console for every frame, which is
        // exactly the load the lateness experiment measures. INFO keeps them visible on a
        // KUMO_CONSOLE_LOGLEVEL=6 bring-up image and silent at the default. — TIMBERDOODLE
        log(
            log_level::INFO,
            format_args!("drv-gpu: fence wait armed point {}\n", point),
        );
        Some(read)
    }

    /// Wake and release every armed waiter whose point has completed. — TIMBERDOODLE
    fn drain_fence_waits(&mut self, session: &Session) {
        for slot in &mut self.waiters {
            let satisfied = match slot {
                Some((point, _)) => {
                    matches!(session.fence_state(*point), Ok(SyncState::Complete { .. }))
                }
                None => false,
            };
            if satisfied {
                if let Some((point, handle)) = slot.take() {
                    let byte = [1u8];
                    let _ = kumo_rt::channel_write(handle, byte.as_ptr(), 1);
                    let _ = kumo_rt::handle_close(handle);
                    log(
                        log_level::INFO,
                        format_args!("drv-gpu: fence wait woken point {}\n", point),
                    );
                }
            }
        }
    }

    fn finish_pending(&mut self) -> Result<Option<u64>, GpuStatus> {
        let Some(job) = self.pending.take() else {
            return Ok(None);
        };
        let csg_bit = job_irq_csg(GROUP_SLOT);
        let iface = QueueIface::at(MCU_IO_PAGES_VA);
        let sync = self.work.va + (AS1_SYNC_VA - AS1_WORK_VA);
        let (input, output, windows, csg, cs, work) = (
            self.input,
            self.output,
            self.windows,
            self.csg,
            self.cs,
            self.work,
        );
        {
            let mut io = SharedGlb::new(self.device, input, output, windows);
            ack_group_events(&mut io, csg.input_va, csg.output_va);
        }
        clear_job_irqs(self.device, csg_bit | JOB_IRQ_GLOBAL_IF);
        let started = self.device.now_ns();
        let deadline = started.saturating_add(WORK_TIMEOUT_NS);
        let (mut waits, mut fired) = (0, 0);
        let (last, seqno, status) = loop {
            let publish = waits == 0;
            let remaining = deadline.saturating_sub(self.device.now_ns()).max(1_000);
            let irq = await_job_irq(self.device, csg_bit, remaining, |device| {
                if !publish {
                    return;
                }
                write_job_words(
                    job.insert,
                    job.slot,
                    &job.words,
                    RING_SIZE,
                    |offset, word| {
                        // SAFETY: write_job_words bounds each word to the mapped ring page.
                        unsafe { ((work.va + offset as u64) as *mut u64).write_volatile(word) };
                    },
                );
                dsb();
                {
                    let mut io = SharedGlb::new(&mut *device, input, output, windows);
                    publish_insert(&mut io, iface, job.slot.end);
                }
                dsb();
                {
                    let mut io = SharedGlb::new(&mut *device, input, output, windows);
                    wake_stream(&mut io, csg.input_va, csg.output_va, GROUP_SLOT, 0);
                }
                device.write32(csf_doorbell(GROUP_SLOT + 1), 1);
            })
            .map_err(|_| GpuStatus::DeviceLost)?;
            waits += 1;
            if irq.completion == Completion::Fired && irq.rawstat & csg_bit != 0 {
                fired += 1;
            }
            {
                let mut io = SharedGlb::new(&mut *self.device, input, output, windows);
                ack_group_events(&mut io, csg.input_va, csg.output_va);
            }
            // SAFETY: aligned words of the driver's retained Normal-NC queue sync page.
            let seqno = unsafe { (sync as *const u64).read_volatile() };
            let status = unsafe { ((sync + SYNC_STATUS_OFFSET) as *const u32).read_volatile() };
            if seqno >= job.seqno
                || irq.completion == Completion::TimedOut
                || self.device.now_ns() >= deadline
            {
                break (irq.completion, seqno, status);
            }
        };
        let outcome = classify_job_wait(seqno >= job.seqno, last, fired);
        if outcome != JobWait::Interrupt || seqno != job.seqno || status != 0 {
            let cs_status = {
                let mut io = SharedGlb::new(&mut *self.device, input, output, windows);
                read_cs_status(&mut io, cs.output_va)
            };
            let mmu_fault = mmu_rawstat(self.device) & (1u32 << GROUP_AS) != 0;
            self.fail_group(drv_gpu::classify_group_failure(
                outcome,
                status,
                cs_status.fault,
                cs_status.fatal,
                mmu_fault,
            ));
            log(
                log_level::ERR,
                format_args!(
                    "drv-gpu: client point {} failed: {:?} seqno={} status=0x{:x}\n",
                    job.point, outcome, seqno, status
                ),
            );
            report_stream_stall(self.device, input, output, windows, csg, cs);
            return Err(GpuStatus::DeviceLost);
        }
        //j722
        // One line per frame belongs at INFO for the same reason as the fence lines above.
        // — TIMBERDOODLE
        log(
            log_level::INFO,
            format_args!(
                "drv-gpu: client point {} complete on JOB: hw_seqno={} status=0x0 ({} us)\n",
                job.point,
                seqno,
                (self.device.now_ns() - started) / 1_000
            ),
        );
        Ok(Some(job.point))
    }
}

/// G8d-a's driver-side close tail. Service has already stopped accepting requests; the only
/// decision left is whether quiescence was proven. [`Containment::run`] owns the ordering and
/// the record; this wrapper supplies the one real reset and the one real backend.
fn contain(hw: &mut Hardware<'_>, session: &mut Session) -> Containment {
    Containment::run(soft_reset(hw.device), session, hw)
}

//j714
/// Serve exactly one granted session channel and return its [`Containment`] (DESIGN/024).
///
/// The channel is the session: when it closes, or a write to it fails, service has stopped
/// accepting and the contained close below runs before this function returns. Every
/// transport handle the session held — the reply grant, the waiter channels, this channel —
/// is closed on both the released and the poisoned path; only the metadata and DMA a poison
/// retains stay behind. The caller decides what happens next: a release licenses the next
/// `g`, and a poison parks with everything retained. Parking here would hide that decision
/// inside a function whose whole contract is one session. — TIMBERDOODLE
pub(super) fn serve(
    channel: Handle,
    device: &mut Device,
    tables: &mut TableDma,
    arena: &mut TableArena,
    root: u64,
    work: Mapping,
    input: u64,
    output: u64,
    windows: [McuWindow; 3],
    csg: CsgControl,
    cs: CsControl,
    shader_present: u64,
    mmu_features: u32,
    tiler_features: u32,
    texture_features: [u32; 4],
    core_features: u32,
    thread_max_threads: u32,
    thread_max_workgroup_size: u32,
    thread_features: u32,
    memory: protocol::DeviceMemory,
) -> Containment {
    let mut hw = Hardware {
        device,
        shader_present,
        mmu_features,
        tiler_features,
        texture_features,
        core_features,
        thread_max_threads,
        thread_max_workgroup_size,
        thread_features,
        memory,
        tables,
        arena,
        root,
        work,
        input,
        output,
        windows,
        csg,
        cs,
        buffers: Vec::new(),
        reply_grant: None,
        pending: None,
        waiters: [None; MAX_WAITS],
        group_report: protocol::GroupReport::USABLE,
        status_vmo: None,
        heaps: Vec::new(),
    };
    let mut session = Session::default();
    log(
        log_level::NOTICE,
        format_args!("drv-gpu: client session ready\n"),
    );
    //j714
    // One granted channel is one session: this loop reads only the channel it was handed and
    // ends when that channel does. A fresh session is a fresh `serve` over a fresh `g`, never
    // a second loop over a spent channel. — TIMBERDOODLE
    loop {
        let mut bytes = [0u8; REQUEST_LEN + 1];
        let (len, raw_handle) =
            kumo_rt::channel_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len());
        let unexpected = valid_handle(raw_handle);
        if let Some(handle) = unexpected {
            let _ = kumo_rt::handle_close(handle);
        }
        if len == 0 || len > bytes.len() {
            break;
        }
        let request = bytes.get(..len).and_then(Request::decode);
        let (id, mut result) = match request {
            Some(request) if unexpected.is_none() => {
                (request.id, session.dispatch(request, &mut hw))
            }
            _ => (0, Err(GpuStatus::BadRequest)),
        };
        let mut grant = if matches!(
            result,
            Ok(Response::BufferCreated { .. } | Response::GroupWatch { .. })
        ) {
            hw.reply_grant.take()
        } else {
            None
        };
        let armed_point = match result {
            Ok(Response::WaitArmed { point }) => Some(point),
            _ => None,
        };
        if let Some(point) = armed_point {
            match hw.arm_fence_wait(point) {
                Some(read) => grant = Some(read),
                None => result = Err(GpuStatus::NoMemory),
            }
        }
        let retired = result == Err(GpuStatus::DeviceLost)
            || matches!(result,
            Ok(Response::GroupWatch { report, .. }) if report.state != protocol::GroupState::Usable);
        if retired {
            hw.fail_group(protocol::GroupReport::UNKNOWN);
        }
        let frame = GpuReply { id, result }.encode();
        let sent = kumo_rt::channel_write_with_handle(
            channel,
            frame.as_ptr(),
            frame.len(),
            grant.unwrap_or(Handle(0)),
        );
        if sent != 0 {
            if let Some(handle) = grant {
                let _ = kumo_rt::handle_close(handle);
            }
            break;
        }
        if retired {
            break;
        }
        match hw.finish_pending() {
            Ok(Some(point)) => {
                if session.complete(point, 0).is_err() {
                    break;
                }
                hw.drain_fence_waits(&session);
            }
            Ok(None) => (),
            Err(_) => {
                hw.fail_group(protocol::GroupReport::UNKNOWN);
                break;
            }
        }
    }
    // G8d-a: the loop above was the only reader, so service has stopped accepting. Quiesce
    // first; only a confirmed reset licenses releasing anything. The record states exactly
    // what was released, and a failed reset yields retained=yes with the session poisoned.
    // — TIMBERDOODLE
    //j714
    // This record is also the return value: the release proof travels to the caller, which
    // asks it the one question left — whether another grant may be served. — TIMBERDOODLE
    hw.fail_group(protocol::GroupReport::UNKNOWN);
    let containment = contain(&mut hw, &mut session);
    log(log_level::NOTICE, format_args!("{}\n", containment));
    if let Err(error) = containment.reset {
        log(
            log_level::ERR,
            format_args!("drv-gpu: containment reset cause: {:?}\n", error),
        );
    }
    //j714
    // Transport state is not DMA: the grant and waiter channels close on both paths, so no
    // client can be handed a view into a session this driver will never serve again. The
    // buffers themselves were released only if containment proved the GPU stopped.
    if let Some(handle) = hw.reply_grant.take() {
        let _ = kumo_rt::handle_close(handle);
    }
    // This VMO is CPU metadata and was never DMA-visible. It closes on either containment
    // path; the client's READ-only grant independently retains the last record. — MILVUS
    if let Some(handle) = hw.status_vmo.take() {
        let _ = kumo_rt::handle_close(handle);
    }
    // The waiter channels close on both paths; a retained session keeps only its own metadata
    // and DMA, never a peer that could still be woken by a driver that will not serve it.
    for slot in &mut hw.waiters {
        if let Some((_, handle)) = slot.take() {
            let _ = kumo_rt::handle_close(handle);
        }
    }
    let _ = kumo_rt::handle_close(channel);
    //j714
    // Return, don't park: the caller owns the long-lived loop (DESIGN/024). A released
    // containment lets it read the next `g`; a poisoned one closes the handoff and parks
    // with everything retained. — TIMBERDOODLE
    containment
}
