//j549
//j892
//! Whole-sector backing over the existing `drv-blk` wire protocol.

use kumo_fatfs::{SectorReader, SectorWriter, SECTOR_SIZE};

/// One synchronous request/reply exchange; `None` means the channel failed.
pub trait BlockTransport {
    fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize>;

    /// [`Self::exchange`] with a copy of `vmo` carried by the request (SH7-d3b). `None` when
    /// the transport cannot carry handles or the copy cannot be made.
    fn exchange_with_vmo(
        &mut self,
        _request: &[u8],
        _vmo: u32,
        _reply: &mut [u8],
    ) -> Option<usize> {
        None
    }
}

/// A filesystem's narrowed volume capability. Sector zero is the partition's first block.
pub struct BlockSectors<T> {
    transport: T,
    block_count: u64,
    /// SH7-d3b: the driver answered `STATUS_UNSUPPORTED` to `CMD_READ_VMO`; stop asking.
    no_vmo_reads: bool,
}

impl<T: BlockTransport> BlockSectors<T> {
    pub const fn new(transport: T, block_count: u64) -> Self {
        Self {
            transport,
            block_count,
            no_vmo_reads: false,
        }
    }

    fn status(&mut self, request: &[u8]) -> bool {
        let mut reply = [0xFF; 2];
        self.transport.exchange(request, &mut reply) == Some(1) && reply[0] == drv_blk::STATUS_OK
    }
}

impl<T: BlockTransport> SectorReader for BlockSectors<T> {
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
        if lba as u64 >= self.block_count {
            return false;
        }
        let request = drv_blk::Request::read(lba as u64, 1).encode();
        let mut reply = [0u8; 1 + SECTOR_SIZE];
        if self.transport.exchange(&request, &mut reply) != Some(reply.len())
            || reply[0] != drv_blk::STATUS_OK
        {
            return false;
        }
        buf.copy_from_slice(&reply[1..]);
        true
    }

    fn read_sectors_to_vmo(
        &mut self,
        lba: u32,
        count: u32,
        vmo: u32,
        vmo_offset: u64,
    ) -> Option<bool> {
        self.read_run_to_vmo(lba, count, vmo, vmo_offset)
    }
}

impl<T: BlockTransport> BlockSectors<T> {
    /// SH7-d3b: `count` sectors from `lba` into `vmo` at `vmo_offset`, as `CMD_READ_VMO`
    /// requests of at most `READ_VMO_MAX_BLOCKS`. `None` when the driver or the transport
    /// cannot (and nothing was read), so the caller falls back to sector reads.
    fn read_run_to_vmo(&mut self, lba: u32, count: u32, vmo: u32, vmo_offset: u64) -> Option<bool> {
        if self.no_vmo_reads {
            return None;
        }
        let end = (lba as u64).checked_add(count as u64)?;
        if end > self.block_count {
            return Some(false);
        }
        let mut done = 0u32;
        while done < count {
            let take = (count - done).min(drv_blk::READ_VMO_MAX_BLOCKS as u32);
            let request = drv_blk::encode_read_vmo(
                (lba + done) as u64,
                take as u16,
                vmo_offset + done as u64 * SECTOR_SIZE as u64,
            );
            let mut reply = [0xFF; 2];
            let answered = self.transport.exchange_with_vmo(&request, vmo, &mut reply);
            match (answered, reply[0]) {
                (Some(1), drv_blk::STATUS_OK) => done += take,
                (Some(1), drv_blk::STATUS_UNSUPPORTED) | (None, _) if done == 0 => {
                    if answered.is_some() {
                        self.no_vmo_reads = true;
                    }
                    return None;
                }
                _ => return Some(false),
            }
        }
        Some(true)
    }
}

impl<T: BlockTransport> SectorWriter for BlockSectors<T> {
    fn write_sector(&mut self, lba: u32, buf: &[u8; SECTOR_SIZE]) -> bool {
        if lba as u64 >= self.block_count {
            return false;
        }
        let mut request = [0u8; drv_blk::REQUEST_LEN + SECTOR_SIZE];
        request[..drv_blk::REQUEST_LEN]
            .copy_from_slice(&drv_blk::Request::write(lba as u64, 1).encode());
        request[drv_blk::REQUEST_LEN..].copy_from_slice(buf);
        self.status(&request)
    }

    fn flush(&mut self) -> bool {
        self.status(&drv_blk::Request::flush().encode())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    struct MemoryBlocks {
        sector: [u8; SECTOR_SIZE],
        flushes: usize,
    }

    impl BlockTransport for MemoryBlocks {
        fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize> {
            let decoded = drv_blk::Request::decode(request).unwrap();
            reply[0] = drv_blk::STATUS_OK;
            match decoded.cmd {
                drv_blk::CMD_READ => {
                    assert_eq!(decoded, drv_blk::Request::read(0, 1));
                    reply[1..].copy_from_slice(&self.sector);
                    Some(reply.len())
                }
                drv_blk::CMD_WRITE => {
                    assert_eq!(decoded, drv_blk::Request::write(0, 1));
                    assert_eq!(request.len(), drv_blk::REQUEST_LEN + SECTOR_SIZE);
                    self.sector
                        .copy_from_slice(drv_blk::split_write_frame(request).unwrap().1);
                    Some(1)
                }
                drv_blk::CMD_FLUSH => {
                    assert_eq!(decoded, drv_blk::Request::flush());
                    self.flushes += 1;
                    Some(1)
                }
                _ => panic!("unexpected block request"),
            }
        }
    }

    #[test]
    fn a_sector_is_written_flushed_and_read_through_the_block_protocol() {
        let mut disk = BlockSectors::new(
            MemoryBlocks {
                sector: [0; SECTOR_SIZE],
                flushes: 0,
            },
            1,
        );
        let sector = [0xA5; SECTOR_SIZE];
        assert!(disk.write_sector(0, &sector));
        assert!(disk.flush());
        let mut read = [0; SECTOR_SIZE];
        assert!(disk.read_sector(0, &mut read));
        assert_eq!(read, sector);
        assert_eq!(disk.transport.flushes, 1);
        assert!(!disk.read_sector(1, &mut read));
        assert!(!disk.write_sector(1, &sector));
    }

    /// SH7-d3b: a driver that records each CMD_READ_VMO it gets and answers `status`.
    struct VmoBlocks {
        status: u8,
        requests: std::vec::Vec<(u64, u16, u64, u32)>,
        carry: bool,
    }

    impl BlockTransport for VmoBlocks {
        fn exchange(&mut self, _: &[u8], _: &mut [u8]) -> Option<usize> {
            panic!("a VMO read never falls back by itself");
        }

        fn exchange_with_vmo(
            &mut self,
            request: &[u8],
            vmo: u32,
            reply: &mut [u8],
        ) -> Option<usize> {
            if !self.carry {
                return None;
            }
            match drv_blk::volume_request(request, 0, 1 << 20, true).unwrap() {
                drv_blk::VolumeRequest::ReadVmo {
                    lba,
                    count,
                    vmo_offset,
                } => self.requests.push((lba, count, vmo_offset, vmo)),
                other => panic!("{other:?}"),
            }
            reply[0] = self.status;
            Some(1)
        }
    }

    #[test]
    fn vmo_runs_go_out_in_128_block_requests_and_unsupported_is_remembered() {
        let mut disk = BlockSectors::new(
            VmoBlocks {
                status: drv_blk::STATUS_OK,
                requests: std::vec::Vec::new(),
                carry: true,
            },
            1 << 20,
        );
        assert_eq!(disk.read_sectors_to_vmo(10, 300, 7, 0x1000), Some(true));
        assert_eq!(
            disk.transport.requests,
            [
                (10, 128, 0x1000, 7),
                (138, 128, 0x1000 + 128 * 512, 7),
                (266, 44, 0x1000 + 256 * 512, 7)
            ]
        );
        // Past the volume: a failed read, nothing sent.
        assert_eq!(
            disk.read_sectors_to_vmo((1 << 20) - 1, 2, 7, 0),
            Some(false)
        );
        assert_eq!(disk.transport.requests.len(), 3);

        // A driver without CMD_READ_VMO: decline once, then never ask again.
        disk.transport.status = drv_blk::STATUS_UNSUPPORTED;
        assert_eq!(disk.read_sectors_to_vmo(0, 1, 7, 0), None);
        assert_eq!(disk.read_sectors_to_vmo(0, 1, 7, 0), None);
        assert_eq!(disk.transport.requests.len(), 4);

        // A transport that cannot carry the VMO declines without being remembered.
        let mut plain = BlockSectors::new(
            VmoBlocks {
                status: drv_blk::STATUS_OK,
                requests: std::vec::Vec::new(),
                carry: false,
            },
            8,
        );
        assert_eq!(plain.read_sectors_to_vmo(0, 1, 7, 0), None);
        plain.transport.carry = true;
        assert_eq!(plain.read_sectors_to_vmo(0, 1, 7, 0), Some(true));

        // An I/O error mid-run is a failed read, not a fallback.
        let mut failing = BlockSectors::new(
            VmoBlocks {
                status: drv_blk::STATUS_IO_ERROR,
                requests: std::vec::Vec::new(),
                carry: true,
            },
            1 << 20,
        );
        assert_eq!(failing.read_sectors_to_vmo(0, 4, 7, 0), Some(false));
    }

    struct BadReply(Option<usize>, u8);
    impl BlockTransport for BadReply {
        fn exchange(&mut self, _: &[u8], reply: &mut [u8]) -> Option<usize> {
            reply[0] = self.1;
            self.0
        }
    }

    #[test]
    fn failed_empty_or_malformed_acknowledgements_never_report_success() {
        for (len, status) in [
            (None, 0),
            (Some(0), 0),
            (Some(2), 0),
            (Some(1), drv_blk::STATUS_IO_ERROR),
            (Some(1), drv_blk::STATUS_READ_ONLY),
        ] {
            let mut disk = BlockSectors::new(BadReply(len, status), 1);
            assert!(!disk.write_sector(0, &[0; SECTOR_SIZE]));
            assert!(!disk.flush());
            assert!(!disk.read_sector(0, &mut [0; SECTOR_SIZE]));
        }
    }
}
