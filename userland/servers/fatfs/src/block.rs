//j549
//! Whole-sector backing over the existing `drv-blk` wire protocol.

use kumo_fatfs::{SectorReader, SectorWriter, SECTOR_SIZE};

/// One synchronous request/reply exchange; `None` means the channel failed.
pub trait BlockTransport {
    fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize>;
}

/// A filesystem's narrowed volume capability. Sector zero is the partition's first block.
pub struct BlockSectors<T> {
    transport: T,
    block_count: u64,
}

impl<T: BlockTransport> BlockSectors<T> {
    pub const fn new(transport: T, block_count: u64) -> Self {
        Self {
            transport,
            block_count,
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
