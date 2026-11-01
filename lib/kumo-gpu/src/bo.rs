//j744
//j745
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

/// The adapter owns successful mappings until an unmap succeeds. An unmap failure must
/// retain the mapping/pin; closing a VMO handle cannot release still-mapped backing.
/// Implementations must create RW Normal-NC mappings, never CPU executable mappings.
/// — KESTREL
pub trait Mapper<H> {
    fn map(&mut self, handle: &H, bytes: u64) -> Result<NonNull<u8>, MapError>;
    fn unmap(&mut self, address: NonNull<u8>, bytes: u64) -> Result<(), MapError>;
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
        let record = &mut self.records[index];
        if !record.destroyed {
            self.client.destroy_buffer(&record.buffer)?;
            record.destroyed = true;
            self.bindings.retain(|binding| binding.buffer != id);
        }
        self.unmap(id)?;
        let record = self.records.swap_remove(index);
        self.client.close_buffer_handle(record.buffer.handle);
        Ok(())
    }

    fn release_local(&mut self) {
        for record in self.records.drain(..) {
            if let Some(address) = record.address {
                let _ = self.mapper.unmap(address, record.buffer.bytes);
            }
            self.client.close_buffer_handle(record.buffer.handle);
        }
        self.bindings.clear();
        self.vm = None;
        self.device_open = false;
    }

    pub fn live_count(&self) -> usize {
        self.records.len()
    }
}

impl<T: Transport, M: Mapper<T::Handle>> Drop for Buffers<'_, T, M> {
    fn drop(&mut self) {
        if !self.records.is_empty() || self.device_open {
            self.client.abandon();
        }
        // No destroy replay on abandonment. A driver-held VMO keeps GPU backing alive;
        // a failed CPU unmap keeps the kernel mapping's own pin. — KESTREL
        self.release_local();
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
        self.release_local();
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
