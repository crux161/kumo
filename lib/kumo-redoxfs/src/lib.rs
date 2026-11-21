//j926
//j928
//j930
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//! RedoxFS over an explicitly granted `drv-blk` volume (SH8-a).
//! Recovery class: disk-backed. No state is durable until the block driver flushes it.

use drv_blk::{Request, BLOCK_SIZE as SECTOR_SIZE, REQUEST_LEN, STATUS_OK};
use redoxfs::Disk;
use syscall::error::{Error, Result, EINVAL, EIO, EROFS};

pub use redoxfs;
//j930 (SH8-d): `Bulk`, `exchange_bulk` and `BULK_BYTES` move up to 64 KiB per request over a
// transport that carries a VMO; anything else keeps the one-sector protocol. — TEAL

/// One synchronous exchange on a volume-scoped block channel. The driver, rather than
/// this client, owns partition offsets and access rights. `None` means transport failure.
pub trait BlockTransport {
    fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize>;

    /// j930 (SH8-d): send a `CMD_READ_VMO`/`CMD_WRITE_VMO` frame carrying the transport's own
    /// bulk VMO (data at offset 0). A write's bytes go into the VMO before the request; a read's
    /// come out after an OK reply. `None`: this transport has no bulk path, and the disk falls
    /// back to one sector per request.
    fn exchange_bulk(
        &mut self,
        _request: &[u8],
        _payload: Bulk<'_>,
        _reply: &mut [u8],
    ) -> Option<usize> {
        None
    }
}

/// The data of one bulk exchange.
pub enum Bulk<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

/// Bytes one bulk request moves at most (`drv_blk::READ_VMO_MAX_BLOCKS` sectors, 64 KiB).
pub const BULK_BYTES: usize = drv_blk::READ_VMO_MAX_BLOCKS as usize * SECTOR_SIZE as usize;

/// RedoxFS uses 4096-byte blocks; the existing driver protocol addresses 512-byte sectors.
/// Every request stays inside the granted volume, including requests spanning many blocks.
pub struct BlockDisk<T> {
    transport: T,
    sectors: u64,
    /// j930: false once the transport or its driver has no bulk path; never asked again.
    bulk: bool,
    /// Requests sent, for proofs of the bulk path's effect.
    pub requests: u64,
}

impl<T: BlockTransport> BlockDisk<T> {
    pub fn new(transport: T, sectors: u64) -> Result<Self> {
        if sectors == 0
            || !sectors.is_multiple_of(redoxfs::BLOCK_SIZE / SECTOR_SIZE)
            || sectors.checked_mul(SECTOR_SIZE).is_none()
        {
            return Err(Error::new(EINVAL));
        }
        Ok(Self {
            transport,
            sectors,
            bulk: true,
            requests: 0,
        })
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    /// The transport, for a serve loop's diagnostics and for tests that inject faults. — TEAL
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
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

    /// One bulk request. `Ok(false)` when there is no bulk path (this request and every later
    /// one go sector by sector); an error status is the request's error.
    fn bulk_exchange(&mut self, frame: &[u8], payload: Bulk<'_>) -> Result<bool> {
        let mut reply = [0xFF; 2];
        self.requests += 1;
        match self.transport.exchange_bulk(frame, payload, &mut reply) {
            None => {
                self.bulk = false;
                Ok(false)
            }
            Some(1) if reply[0] == drv_blk::STATUS_UNSUPPORTED => {
                self.bulk = false;
                Ok(false)
            }
            Some(1) => check_status(reply[0]).map(|()| true),
            Some(_) => Err(Error::new(EIO)),
        }
    }

    fn status(&mut self, request: &[u8]) -> Result<()> {
        self.requests += 1;
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
        let mut done = 0;
        while self.bulk && buffer.len() - done > SECTOR_SIZE as usize {
            let end = (done + BULK_BYTES).min(buffer.len());
            let chunk = &mut buffer[done..end];
            let sector = start + (done as u64 / SECTOR_SIZE);
            let count = (chunk.len() as u64 / SECTOR_SIZE) as u16;
            let frame = drv_blk::encode_read_vmo(sector, count, 0);
            if !self.bulk_exchange(&frame, Bulk::Read(chunk))? {
                break;
            }
            done += chunk.len();
        }
        let start = start + done as u64 / SECTOR_SIZE;
        for (i, sector) in buffer[done..]
            .chunks_exact_mut(SECTOR_SIZE as usize)
            .enumerate()
        {
            self.requests += 1;
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
        let mut done = 0;
        while self.bulk && buffer.len() - done > SECTOR_SIZE as usize {
            let end = (done + BULK_BYTES).min(buffer.len());
            let chunk = &buffer[done..end];
            let sector = start + (done as u64 / SECTOR_SIZE);
            let count = (chunk.len() as u64 / SECTOR_SIZE) as u16;
            let frame = drv_blk::encode_write_vmo(sector, count, 0);
            if !self.bulk_exchange(&frame, Bulk::Write(chunk))? {
                break;
            }
            done += chunk.len();
        }
        let start = start + done as u64 / SECTOR_SIZE;
        for (i, sector) in buffer[done..]
            .chunks_exact(SECTOR_SIZE as usize)
            .enumerate()
        {
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
mod fuzz;
#[cfg(test)]
mod tests;
