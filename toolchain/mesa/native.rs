//j744
//! Kernel mapping adapter for the native Mesa bridge. No FD or physical-address route.
use core::ptr::NonNull;
use kumo_abi::{Handle, VmarFlags};
use kumo_gpu::bo::{MapError, Mapper};

pub struct Memory;
impl Mapper<Handle> for Memory {
    fn map(&mut self, vmo: &Handle, bytes: u64) -> Result<NonNull<u8>, MapError> {
        let (status, va) = kumo_rt::vmar_map_anywhere(
            *vmo,
            bytes,
            (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
        );
        if status != 0 {
            return Err(MapError::NoMemory);
        }
        NonNull::new(va as *mut u8).ok_or(MapError::Failed)
    }
    fn unmap(&mut self, address: NonNull<u8>, bytes: u64) -> Result<(), MapError> {
        if kumo_rt::vmar_unmap(address.as_ptr() as u64, bytes) != 0 {
            return Err(MapError::Failed);
        }
        Ok(())
    }
}
