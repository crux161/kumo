//j747
//j751
//j752
//j757
//j789
//! Native BO ownership for a borrowed session. Kernel handles stay in Rust; CPU mappings
//! are full-buffer, read/write Normal-NC data mappings, including executable GPU BOs.

use alloc::vec::Vec;
use core::ptr::NonNull;

use crate::client::{Buffer, Client, Error as ClientError, SessionTransport, Transport};
use crate::{BindFlags, Binding, Unbind, MAX_BUFFER_BYTES, PAGE_BYTES, VA_END, VA_START};

// These are the pinned pan_kmod flags, checked against Mesa by the C bridge. — KESTREL
pub const EXECUTABLE: u32 = 1;
pub const NO_MMAP: u32 = 1 << 2;
pub const GPU_UNCACHED: u32 = 1 << 5;
pub const SUPPORTED_FLAGS: u32 = EXECUTABLE | NO_MMAP | GPU_UNCACHED;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapError {
    NoMemory,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    Woken,
    TimedOut,
    Closed,
    Failed,
    Unsupported,
}

/// The adapter owns successful mappings until an unmap succeeds. An unmap failure must
/// retain the mapping/pin; closing a VMO handle cannot release still-mapped backing.
/// Implementations must create RW Normal-NC mappings, never CPU executable mappings.
/// — KESTREL
pub trait Mapper<H> {
    fn map(&mut self, handle: &H, bytes: u64) -> Result<NonNull<u8>, MapError>;
    fn unmap(&mut self, address: NonNull<u8>, bytes: u64) -> Result<(), MapError>;
    /// Read CPU metadata via the granted VMO's READ right. No writable mapping is needed.
    /// A backend without this operation must refuse, never invent a status. — MILVUS
    fn read(&mut self, _handle: &H, _bytes: &mut [u8]) -> Result<(), MapError> {
        Err(MapError::Failed)
    }
    /// Bound one wake-channel wait; do not close the borrowed handle or claim GPU
    /// completion. The shared owner closes it after every outcome. — MILVUS
    fn wait(&mut self, _wake: &H, _timeout_ns: u64) -> WaitOutcome {
        WaitOutcome::Unsupported
    }
    /// Publish/observe shared memory around native operations. Device adapters must use
    /// the barrier their mappings require; the default serves coherent host models.
    /// — MILVUS
    fn barrier(&mut self) {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Client(ClientError),
    InvalidRequest,
    NotFound,
    NoMemory,
    Map(MapError),
    Unmap(MapError),
}
impl From<ClientError> for Error {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct Info {
    pub id: u32,
    pub flags: u32,
    pub bytes: u64,
}

struct Record<H> {
    buffer: Buffer<H>,
    flags: u32,
    address: Option<NonNull<u8>>,
    // A failed CPU unmap after acknowledged BufferDestroy is retryable locally, without
    // sending a second destroy or mapping an allocation the service has released. — KESTREL
    destroyed: bool,
}

#[derive(Clone, Copy)]
struct Vm {
    id: u32,
    start: u64,
    end: u64,
}

/// Logical VM identity and one map request; physical backing stays in the Rust BO table.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct VmBinding {
    pub vm: u32,
    pub buffer: u32,
    pub va: u64,
    pub offset: u64,
    pub bytes: u64,
}

/// Rust owns BO grants and CPU aliases for the borrowed callback scope. The BO-only API
/// requires an existing service VM; with_device_api opens one and always closes its owned
/// session on normal return. Native logical VMs lease that single service address space.
/// Abandonment drops only local claims; drv-gpu resets before freeing its DMA. — KESTREL
pub struct Buffers<'a, T: Transport, M: Mapper<T::Handle>> {
    pub(crate) client: &'a mut Client<T>,
    mapper: &'a mut M,
    records: Vec<Record<T::Handle>>,
    device_open: bool,
    vm: Option<Vm>,
    next_vm: u32,
    bindings: Vec<Binding>,
    pub(crate) native: Option<crate::c_sync::Context<T::Handle>>,
    pub(crate) group_report: Option<(u64, crate::GroupReport)>,
    /// G7q-c: live tiler heaps, all owned by the current logical VM. — TIMBERDOODLE
    heaps: Vec<u32>,
}

impl<'a, T: Transport, M: Mapper<T::Handle>> Buffers<'a, T, M> {
    pub fn new(client: &'a mut Client<T>, mapper: &'a mut M) -> Self {
        Self {
            client,
            mapper,
            records: Vec::new(),
            device_open: false,
            vm: None,
            next_vm: 0,
            bindings: Vec::new(),
            native: None,
            group_report: None,
            heaps: Vec::new(),
        }
    }

    pub fn create(&mut self, bytes: u64, flags: u32) -> Result<Info, Error> {
        if flags & !SUPPORTED_FLAGS != 0 || bytes == 0 {
            return Err(Error::InvalidRequest);
        }
        let bytes = bytes
            .checked_add(PAGE_BYTES - 1)
            .map(|value| value & !(PAGE_BYTES - 1))
            .filter(|bytes| *bytes <= MAX_BUFFER_BYTES)
            .ok_or(Error::InvalidRequest)?;
        self.records.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let buffer = self.client.create_buffer(bytes)?;
        let info = Info {
            id: buffer.id,
            flags,
            bytes: buffer.bytes,
        };
        self.records.push(Record {
            buffer,
            flags,
            address: None,
            destroyed: false,
        });
        Ok(info)
    }

    pub fn map(&mut self, id: u32) -> Result<NonNull<u8>, Error> {
        if self.client.is_lost() {
            return Err(ClientError::DeviceLost.into());
        }
        let record = self
            .records
            .iter_mut()
            .find(|record| record.buffer.id == id && !record.destroyed)
            .ok_or(Error::NotFound)?;
        if record.flags & NO_MMAP != 0 {
            return Err(Error::InvalidRequest);
        }
        if let Some(address) = record.address {
            return Ok(address);
        }
        let address = self
            .mapper
            .map(&record.buffer.handle, record.buffer.bytes)
            .map_err(Error::Map)?;
        record.address = Some(address);
        Ok(address)
    }

    pub fn unmap(&mut self, id: u32) -> Result<(), Error> {
        let record = self
            .records
            .iter_mut()
            .find(|record| record.buffer.id == id)
            .ok_or(Error::NotFound)?;
        if let Some(address) = record.address {
            self.mapper
                .unmap(address, record.buffer.bytes)
                .map_err(Error::Unmap)?;
            record.address = None;
        }
        Ok(())
    }

    pub fn destroy(&mut self, id: u32) -> Result<(), Error> {
        let index = self
            .records
            .iter()
            .position(|record| record.buffer.id == id)
            .ok_or(Error::NotFound)?;
        let lost = self.client.is_lost();
        let record = &mut self.records[index];
        if !record.destroyed {
            // VK6-a: once the session is lost the driver's reset contains its buffers, and the
            // endpoint is closed or closing, so a destroy releases only the local claim. A
            // destroy that discovers the loss still reports it. — TIMBERDOODLE
            if !lost {
                self.client.destroy_buffer(&record.buffer)?;
            }
            record.destroyed = true;
            self.bindings.retain(|binding| binding.buffer != id);
        }
        self.unmap(id)?;
        let record = self.records.swap_remove(index);
        self.client.close_buffer_handle(record.buffer.handle);
        Ok(())
    }

    /// Release every local claim. With `keep_aliases` (a device close inside the API scope,
    /// which is how the C bridge reports loss), each live CPU alias stays mapped, with its
    /// grant handle, until its BO's unmap or destroy or the scope's end. Vulkan's lost-device
    /// rule keeps host access to mapped memory valid (contents undefined), and panvk reads its
    /// own subqueue contexts through these maps after a loss (PLAN/015 §8.49 D1a). The GPU is
    /// already reset and the session closed; a kept mapping only holds its VMO's frames until
    /// it goes, as every client mapping does. — TIMBERDOODLE
    fn release_local(&mut self, keep_aliases: bool) {
        if let Some(mut native) = self.native.take() {
            if let Some(report) = native.capture_group(self.mapper) {
                self.group_report = Some(report);
            }
            native.clear(self.client);
        }
        for record in core::mem::take(&mut self.records) {
            if keep_aliases && record.address.is_some() {
                self.records.push(record);
                continue;
            }
            if let Some(address) = record.address {
                let _ = self.mapper.unmap(address, record.buffer.bytes);
            }
            self.client.close_buffer_handle(record.buffer.handle);
        }
        self.bindings.clear();
        // The server releases a closed session's heaps with its reset; nothing to send.
        self.heaps.clear();
        self.vm = None;
        self.device_open = false;
    }

    pub fn live_count(&self) -> usize {
        self.records.len()
    }

    /// Legacy direct access for J747's diagnostic. Do not interleave direct group/submit
    /// operations with the native C bridge, or bypass the store's BO/VM bookkeeping.
    /// The bridge owns those operations once in use. — MILVUS
    pub fn native_client(&mut self) -> &mut Client<T> {
        self.client
    }

    pub(crate) fn wait_native(&mut self, wake: &T::Handle, timeout_ns: u64) -> WaitOutcome {
        self.mapper.wait(wake, timeout_ns)
    }
    pub(crate) fn barrier_native(&mut self) {
        self.mapper.barrier();
    }
    pub(crate) fn read_native(
        &mut self,
        handle: &T::Handle,
        bytes: &mut [u8],
    ) -> Result<(), MapError> {
        self.mapper.read(handle, bytes)
    }
}

impl<T: Transport, M: Mapper<T::Handle>> Drop for Buffers<'_, T, M> {
    fn drop(&mut self) {
        if !self.records.is_empty() || self.device_open {
            self.client.abandon();
        }
        // No destroy replay on abandonment. A driver-held VMO keeps GPU backing alive;
        // a failed CPU unmap keeps the kernel mapping's own pin. — KESTREL
        self.release_local(false);
    }
}

impl<T: SessionTransport, M: Mapper<T::Handle>> Buffers<'_, T, M> {
    pub fn open_device(&mut self) -> Result<(), Error> {
        if self.client.is_lost() {
            return Err(ClientError::DeviceLost.into());
        }
        if self.device_open || !self.records.is_empty() {
            return Err(Error::InvalidRequest);
        }
        // Create the one service VM before publishing a native device. Mesa's logical VMs
        // lease this AS; destroying/recreating one does not send a second VmCreate. — KESTREL
        if self.client.create_vm()? != 1 {
            self.client.abandon();
            return Err(ClientError::Protocol.into());
        }
        self.device_open = true;
        Ok(())
    }
    pub fn close_device(&mut self) {
        // Release only local claims, then close the endpoint so the driver contains DMA.
        // Endpoint close is idempotent, even after protocol/device loss. — KESTREL
        // Live CPU aliases outlive the close until their unmap or destroy (VK6-a). — TIMBERDOODLE
        self.release_local(true);
        self.client.close_session();
    }
    /// The API scope's end: nothing can unmap or destroy after it, so every alias goes too.
    pub fn finish(&mut self) {
        self.release_local(false);
        self.client.close_session();
    }
    pub fn device_ready(&self) -> bool {
        self.device_open && !self.client.is_lost()
    }
    pub fn create_native_vm(&mut self, start: u64, bytes: u64) -> Result<u32, Error> {
        if !self.device_ready() {
            return Err(ClientError::DeviceLost.into());
        }
        let end = start.checked_add(bytes).ok_or(Error::InvalidRequest)?;
        if self.vm.is_some()
            || bytes == 0
            || start % PAGE_BYTES != 0
            || bytes % PAGE_BYTES != 0
            || start < VA_START
            || end > VA_END
        {
            return Err(Error::InvalidRequest);
        }
        let id = self.next_vm.checked_add(1).ok_or(Error::NoMemory)?;
        self.next_vm = id;
        self.vm = Some(Vm { id, start, end });
        Ok(id)
    }
    fn native_vm(&self, id: u32) -> Result<Vm, Error> {
        if !self.device_ready() {
            return Err(ClientError::DeviceLost.into());
        }
        self.vm.filter(|vm| vm.id == id).ok_or(Error::NotFound)
    }
    pub(crate) fn check_native_vm(&self, id: u32) -> Result<(), Error> {
        self.native_vm(id).map(|_| ())
    }
    pub(crate) fn check_native_submission(
        &self,
        vm: u32,
        submission: crate::Submission,
    ) -> Result<(), Error> {
        self.native_vm(vm)?;
        if submission.group != 1 || !submission.valid_shape() {
            return Err(Error::InvalidRequest);
        }
        self.bindings
            .iter()
            .any(|binding| submission.validate_for(1, *binding).is_ok())
            .then_some(())
            .ok_or(Error::InvalidRequest)
    }
    pub fn bind_native(&mut self, request: VmBinding) -> Result<(), Error> {
        let vm = self.native_vm(request.vm)?;
        let record = self
            .records
            .iter()
            .find(|record| record.buffer.id == request.buffer && !record.destroyed)
            .ok_or(Error::NotFound)?;
        let binding = Binding {
            vm: 1,
            buffer: request.buffer,
            va: request.va,
            offset: request.offset,
            bytes: request.bytes,
            flags: bind_flags(record.flags),
        };
        binding
            .validate(record.buffer.bytes)
            .map_err(|_| Error::InvalidRequest)?;
        if binding.va < vm.start
            || binding.va + binding.bytes > vm.end
            || self.bindings.iter().any(|other| binding.overlaps(*other))
        {
            return Err(Error::InvalidRequest);
        }
        if self.bindings.len() == crate::session::MAX_BINDINGS {
            return Err(Error::NoMemory);
        }
        self.bindings.try_reserve(1).map_err(|_| Error::NoMemory)?;
        self.client.bind(binding)?;
        self.bindings.push(binding);
        Ok(())
    }
    pub fn unbind_native(&mut self, id: u32, va: u64, bytes: u64) -> Result<(), Error> {
        let vm = self.native_vm(id)?;
        let range = Unbind { vm: 1, va, bytes };
        if !range.valid_shape() || va < vm.start || va + bytes > vm.end {
            return Err(Error::InvalidRequest);
        }
        let end = va + bytes;
        if self
            .bindings
            .iter()
            .any(|b| b.va < end && va < b.va + b.bytes && (b.va < va || b.va + b.bytes > end))
        {
            return Err(Error::InvalidRequest);
        }
        let mut cursor = va;
        while cursor < end {
            let binding = self
                .bindings
                .iter()
                .find(|b| b.va == cursor)
                .ok_or(Error::NotFound)?;
            cursor += binding.bytes;
        }
        self.client.unbind(range)?;
        self.bindings.retain(|b| b.va < va || b.va >= end);
        Ok(())
    }
    pub fn destroy_native_vm(&mut self, id: u32) -> Result<(), Error> {
        self.native_vm(id)?;
        // A VM's heaps go first, as Panthor's VM teardown takes its heap pool. A retry
        // resumes at the first heap still live. — TIMBERDOODLE
        while let Some(heap) = self.heaps.last().copied() {
            self.client.destroy_heap(heap)?;
            self.heaps.pop();
        }
        // A retry resumes at the first remaining binding; successful invalidations are
        // never replayed. The C void destructor makes any failure terminal. — KESTREL
        while let Some(binding) = self.bindings.last().copied() {
            self.client.unbind(Unbind {
                vm: 1,
                va: binding.va,
                bytes: binding.bytes,
            })?;
            self.bindings.pop();
        }
        self.vm = None;
        Ok(())
    }
}

//j757
impl<T: SessionTransport, M: Mapper<T::Handle>> Buffers<'_, T, M> {
    /// G7q-c: one tiler heap for logical VM `vm`, in TILER_HEAP_CREATE's shape. The server's VM
    /// is always 1; the logical VM decides ownership, so destroying it destroys its heaps.
    /// Shapes Panthor refuses, and an eighth live heap, fail here with no I/O. — TIMBERDOODLE
    pub fn create_native_heap(
        &mut self,
        vm: u32,
        chunk_bytes: u32,
        initial_chunks: u32,
        max_chunks: u32,
        target_in_flight: u32,
    ) -> Result<(u32, crate::HeapPlacement), Error> {
        self.native_vm(vm)?;
        let spec = crate::HeapSpec {
            vm: 1,
            chunk_bytes,
            initial_chunks,
            max_chunks,
            target_in_flight,
        };
        if !spec.valid_shape() {
            return Err(Error::InvalidRequest);
        }
        if self.heaps.len() >= crate::MAX_HEAPS {
            return Err(Error::NoMemory);
        }
        self.heaps.try_reserve(1).map_err(|_| Error::NoMemory)?;
        let created = self.client.create_heap(spec)?;
        self.heaps.push(created.0);
        Ok(created)
    }

    /// Release one heap this owner created; returns the bytes the budget got back.
    pub fn destroy_native_heap(&mut self, heap: u32) -> Result<u64, Error> {
        if !self.device_ready() {
            return Err(ClientError::DeviceLost.into());
        }
        let index = self
            .heaps
            .iter()
            .position(|live| *live == heap)
            .ok_or(Error::NotFound)?;
        let bytes = self.client.destroy_heap(heap)?;
        self.heaps.swap_remove(index);
        Ok(bytes)
    }
}

pub const fn bind_flags(flags: u32) -> BindFlags {
    BindFlags(
        if flags & EXECUTABLE != 0 {
            BindFlags::READ_ONLY
        } else {
            BindFlags::NO_EXECUTE
        } | if flags & GPU_UNCACHED != 0 {
            BindFlags::UNCACHED
        } else {
            0
        },
    )
}

#[cfg(test)]
#[path = "bo_tests.rs"]
mod tests;
