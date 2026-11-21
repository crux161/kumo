//j926
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//! RedoxFS over an explicitly granted `drv-blk` volume (SH8-a).
//! Recovery class: disk-backed. No state is durable until the block driver flushes it.

use drv_blk::{Request, BLOCK_SIZE as SECTOR_SIZE, REQUEST_LEN, STATUS_OK};
use redoxfs::Disk;
use syscall::error::{Error, Result, EINVAL, EIO, EROFS};

pub use redoxfs;

/// One synchronous exchange on a volume-scoped block channel. The driver, rather than
/// this client, owns partition offsets and access rights. `None` means transport failure.
pub trait BlockTransport {
    fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize>;
}

/// RedoxFS uses 4096-byte blocks; the existing driver protocol addresses 512-byte sectors.
/// Every request stays inside the granted volume, including requests spanning many blocks.
pub struct BlockDisk<T> {
    transport: T,
    sectors: u64,
}

impl<T: BlockTransport> BlockDisk<T> {
    pub fn new(transport: T, sectors: u64) -> Result<Self> {
        if sectors == 0
            || !sectors.is_multiple_of(redoxfs::BLOCK_SIZE / SECTOR_SIZE)
            || sectors.checked_mul(SECTOR_SIZE).is_none()
        {
            return Err(Error::new(EINVAL));
        }
        Ok(Self { transport, sectors })
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    fn span(&self, block: u64, len: usize) -> Result<u64> {
        let start = block
            .checked_mul(redoxfs::BLOCK_SIZE / SECTOR_SIZE)
            .ok_or(Error::new(EINVAL))?;
        if !len.is_multiple_of(SECTOR_SIZE as usize) {
            return Err(Error::new(EINVAL));
        }
        let end = start
            .checked_add((len / SECTOR_SIZE as usize) as u64)
            .ok_or(Error::new(EINVAL))?;
        if end > self.sectors {
            return Err(Error::new(EIO));
        }
        Ok(start)
    }

    fn status(&mut self, request: &[u8]) -> Result<()> {
        // Room for an extra byte detects a malformed oversized reply. — KESTREL
        let mut reply = [0xFF; 2];
        if self.transport.exchange(request, &mut reply) != Some(1) {
            return Err(Error::new(EIO));
        }
        check_status(reply[0])
    }
}

fn check_status(status: u8) -> Result<()> {
    match status {
        STATUS_OK => Ok(()),
        drv_blk::STATUS_READ_ONLY => Err(Error::new(EROFS)),
        _ => Err(Error::new(EIO)),
    }
}

impl<T: BlockTransport> Disk for BlockDisk<T> {
    unsafe fn read_at(&mut self, block: u64, buffer: &mut [u8]) -> Result<usize> {
        let start = self.span(block, buffer.len())?;
        for (i, sector) in buffer.chunks_exact_mut(SECTOR_SIZE as usize).enumerate() {
            let mut reply = [0u8; 1 + SECTOR_SIZE as usize];
            let len = self
                .transport
                .exchange(&Request::read(start + i as u64, 1).encode(), &mut reply)
                .ok_or(Error::new(EIO))?;
            if len != reply.len() {
                return Err(Error::new(EIO));
            }
            check_status(reply[0])?;
            sector.copy_from_slice(&reply[1..]);
        }
        Ok(buffer.len())
    }

    unsafe fn write_at(&mut self, block: u64, buffer: &[u8]) -> Result<usize> {
        let start = self.span(block, buffer.len())?;
        for (i, sector) in buffer.chunks_exact(SECTOR_SIZE as usize).enumerate() {
            let mut request = [0u8; REQUEST_LEN + SECTOR_SIZE as usize];
            request[..REQUEST_LEN].copy_from_slice(&Request::write(start + i as u64, 1).encode());
            request[REQUEST_LEN..].copy_from_slice(sector);
            self.status(&request)?;
        }
        Ok(buffer.len())
    }

    fn size(&mut self) -> Result<u64> {
        Ok(self.sectors * SECTOR_SIZE)
    }

    fn sync(&mut self) -> Result<()> {
        self.status(&Request::flush().encode())
    }
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
