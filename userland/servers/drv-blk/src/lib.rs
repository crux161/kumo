#![no_std]

//! `drv-blk` — a minimal ramdisk block device over a VMO.
//!
//! Mirrors soso's `ramdisk.c`: a linear block store backed by a single VMO that
//! the caller (sora) passes in at bootstrap. The block size is fixed at 512 bytes
//! (the FAT sector size); a read/write at LBA `lba` translates to a VMO offset
//! at `lba * 512`.

/// Logical block size — matches FAT and the historical MBR/GPT sector.
pub const BLOCK_SIZE: u64 = 512;

/// Command byte: read blocks.
pub const CMD_READ: u8 = 0x00;
/// Command byte: write blocks (read-only ramdisk — returns OK but ignores data).
pub const CMD_WRITE: u8 = 0x01;

/// Response status: success.
pub const STATUS_OK: u8 = 0x00;
/// Response status: LBA out of range.
pub const STATUS_BAD_LBA: u8 = 0x01;

/// A block device backed by a contiguous VMO.
pub struct BlockDevice {
    /// Total number of logical blocks.
    block_count: u64,
}

impl BlockDevice {
    /// Build a `BlockDevice` from a VMO of `vmo_len` bytes.
    pub const fn new(vmo_len: u64) -> Self {
        Self {
            block_count: vmo_len / BLOCK_SIZE,
        }
    }

    /// Number of logical blocks in the device.
    pub const fn block_count(&self) -> u64 {
        self.block_count
    }

    /// Return the VMO byte offset for `lba`.
    pub const fn offset_for_lba(lba: u64) -> u64 {
        lba * BLOCK_SIZE
    }

    /// Check whether `lba` through `lba + count` is within bounds.
    pub fn check_bounds(&self, lba: u64, count: u64) -> bool {
        count > 0 && lba < self.block_count && lba.saturating_add(count) <= self.block_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_len_device_has_zero_blocks() {
        let dev = BlockDevice::new(0);
        assert_eq!(dev.block_count(), 0);
    }

    #[test]
    fn exact_block_multiple() {
        let dev = BlockDevice::new(1024);
        assert_eq!(dev.block_count(), 2);
    }

    #[test]
    fn partial_sector_ignored() {
        let dev = BlockDevice::new(1023);
        assert_eq!(dev.block_count(), 1);
    }

    #[test]
    fn offset_is_lba_times_block_size() {
        assert_eq!(BlockDevice::offset_for_lba(0), 0);
        assert_eq!(BlockDevice::offset_for_lba(1), 512);
        assert_eq!(BlockDevice::offset_for_lba(100), 51200);
    }

    #[test]
    fn bounds_check_catches_oob_lba() {
        let dev = BlockDevice::new(1024); // 2 blocks
        assert!(dev.check_bounds(0, 1));
        assert!(dev.check_bounds(1, 1));
        assert!(dev.check_bounds(0, 2));
        assert!(!dev.check_bounds(2, 1)); // LBA out of range
        assert!(!dev.check_bounds(0, 3)); // count exceeds blocks
    }
}
