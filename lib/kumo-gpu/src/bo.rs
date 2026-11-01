//j744
//! Native BO ownership for a borrowed session. Kernel handles stay in Rust; CPU mappings
//! are full-buffer, read/write Normal-NC data mappings, including executable GPU BOs.

use alloc::vec::Vec;
use core::ptr::NonNull;

use crate::client::{Buffer, Client, Error as ClientError, Transport};
use crate::{BindFlags, MAX_BUFFER_BYTES, PAGE_BYTES};

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

/// The caller creates the service VM before allocating BOs (device/VM ownership is G7o-d).
/// All C BOs and pointers are scoped to a with_buffer_api call. Explicit free is the normal
/// path. Abandonment invalidates the client and drops only local claims: the caller must
/// close the endpoint and reconnect, and drv-gpu still resets before freeing its DMA.
/// — KESTREL
pub struct Buffers<'a, T: Transport, M: Mapper<T::Handle>> {
    pub(crate) client: &'a mut Client<T>,
    mapper: &'a mut M,
    records: Vec<Record<T::Handle>>,
}

impl<'a, T: Transport, M: Mapper<T::Handle>> Buffers<'a, T, M> {
    pub fn new(client: &'a mut Client<T>, mapper: &'a mut M) -> Self {
        Self {
            client,
            mapper,
            records: Vec::new(),
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
        }
        self.unmap(id)?;
        let record = self.records.swap_remove(index);
        self.client.close_buffer_handle(record.buffer.handle);
        Ok(())
    }

    pub fn live_count(&self) -> usize {
        self.records.len()
    }
}

impl<T: Transport, M: Mapper<T::Handle>> Drop for Buffers<'_, T, M> {
    fn drop(&mut self) {
        if !self.records.is_empty() {
            self.client.abandon();
        }
        // No destroy replay on abandonment. A driver-held VMO keeps GPU backing alive;
        // a failed CPU unmap keeps the kernel mapping's own pin. — KESTREL
        for record in self.records.drain(..) {
            if let Some(address) = record.address {
                let _ = self.mapper.unmap(address, record.buffer.bytes);
            }
            self.client.close_buffer_handle(record.buffer.handle);
        }
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
