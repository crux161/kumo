#![no_std]
//j415
//j416
//! Pure virtio constants and helpers shared by future transport drivers.

pub mod mmio {
    pub const MAGIC: u32 = 0x7472_6976;
    pub const VERSION_LEGACY: u32 = 1;
    pub const VERSION_MODERN: u32 = 2;

    pub const MAGIC_VALUE: u32 = 0x000;
    pub const VERSION: u32 = 0x004;
    pub const DEVICE_ID: u32 = 0x008;
    pub const VENDOR_ID: u32 = 0x00c;
    pub const DEVICE_FEATURES: u32 = 0x010;
    pub const DEVICE_FEATURES_SEL: u32 = 0x014;
    pub const DRIVER_FEATURES: u32 = 0x020;
    pub const DRIVER_FEATURES_SEL: u32 = 0x024;
    pub const QUEUE_SEL: u32 = 0x030;
    pub const QUEUE_NUM_MAX: u32 = 0x034;
    pub const QUEUE_NUM: u32 = 0x038;
    pub const QUEUE_READY: u32 = 0x044;
    pub const QUEUE_NOTIFY: u32 = 0x050;
    pub const INTERRUPT_STATUS: u32 = 0x060;
    pub const INTERRUPT_ACK: u32 = 0x064;
    pub const STATUS: u32 = 0x070;
    pub const QUEUE_DESC_LOW: u32 = 0x080;
    pub const QUEUE_DESC_HIGH: u32 = 0x084;
    pub const QUEUE_AVAIL_LOW: u32 = 0x090;
    pub const QUEUE_AVAIL_HIGH: u32 = 0x094;
    pub const QUEUE_USED_LOW: u32 = 0x0a0;
    pub const QUEUE_USED_HIGH: u32 = 0x0a4;
    pub const CONFIG_GENERATION: u32 = 0x0fc;
    pub const CONFIG: u32 = 0x100;

    pub const INTERRUPT_VRING: u32 = 1 << 0;
    pub const INTERRUPT_CONFIG: u32 = 1 << 1;
}

pub mod ids {
    pub const NONE: u32 = 0;
    pub const BLOCK: u32 = 2;
    pub const TRANSITIONAL_BLOCK: u32 = 0x1001;
}

pub mod block {
    pub const SECTOR_SIZE: u64 = 512;
    pub const CONFIG_CAPACITY: usize = 0;

    pub const FEATURE_RO: u64 = 1 << 5;
    pub const FEATURE_BLK_SIZE: u64 = 1 << 6;
    pub const FEATURE_FLUSH: u64 = 1 << 9;

    pub const T_IN: u32 = 0;
    pub const T_OUT: u32 = 1;
    pub const T_FLUSH: u32 = 4;
    pub const T_GET_ID: u32 = 8;

    pub const STATUS_OK: u8 = 0;
    pub const STATUS_IOERR: u8 = 1;
    pub const STATUS_UNSUPP: u8 = 2;
}

pub mod split_ring {
    pub const DESC_ALIGN: u64 = 16;
    pub const AVAIL_ALIGN: u64 = 2;
    pub const USED_ALIGN: u64 = 4;

    pub const DESC_LEN: u64 = 16;
    pub const AVAIL_HEADER_LEN: u64 = 4;
    pub const AVAIL_ENTRY_LEN: u64 = 2;
    pub const AVAIL_USED_EVENT_LEN: u64 = 2;
    pub const USED_HEADER_LEN: u64 = 4;
    pub const USED_ELEM_LEN: u64 = 8;
    pub const USED_AVAIL_EVENT_LEN: u64 = 2;

    pub const DESC_F_NEXT: u16 = 1;
    pub const DESC_F_WRITE: u16 = 2;
    pub const DESC_F_INDIRECT: u16 = 4;

    pub const AVAIL_F_NO_INTERRUPT: u16 = 1;
    pub const USED_F_NO_NOTIFY: u16 = 1;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    BadMagic,
    UnsupportedVersion,
    LegacyDevice,
    MissingDevice,
    UnsupportedDevice,
    ConfigTooShort,
    CapacityOverflow,
    QueueSizeZero,
    QueueSizeNotPowerOfTwo,
    QueueAlignInvalid,
    QueueLayoutOverflow,
    BlockCountZero,
    BlockDataLenOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioIdentity {
    pub magic: u32,
    pub version: u32,
    pub device_id: u32,
    pub vendor_id: u32,
}

impl MmioIdentity {
    pub const fn new(magic: u32, version: u32, device_id: u32, vendor_id: u32) -> Self {
        Self {
            magic,
            version,
            device_id,
            vendor_id,
        }
    }

    pub fn require_modern_block(self) -> Result<Self, Error> {
        if self.magic != mmio::MAGIC {
            return Err(Error::BadMagic);
        }
        match self.version {
            mmio::VERSION_MODERN => {}
            mmio::VERSION_LEGACY => return Err(Error::LegacyDevice),
            _ => return Err(Error::UnsupportedVersion),
        }
        match self.device_id {
            ids::NONE => Err(Error::MissingDevice),
            ids::BLOCK => Ok(self),
            _ => Err(Error::UnsupportedDevice),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockConfig {
    pub capacity_sectors: u64,
}

impl BlockConfig {
    pub fn from_bytes(config: &[u8]) -> Result<Self, Error> {
        let mut capacity = [0u8; 8];
        capacity.copy_from_slice(
            config
                .get(block::CONFIG_CAPACITY..block::CONFIG_CAPACITY + 8)
                .ok_or(Error::ConfigTooShort)?,
        );
        Ok(Self {
            capacity_sectors: u64::from_le_bytes(capacity),
        })
    }

    pub fn capacity_bytes(self) -> Result<u64, Error> {
        self.capacity_sectors
            .checked_mul(block::SECTOR_SIZE)
            .ok_or(Error::CapacityOverflow)
    }

    pub const fn is_read_only(features: u64) -> bool {
        features & block::FEATURE_RO != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SplitQueueLayout {
    pub queue_size: u16,
    pub align: u64,
    pub desc_offset: u64,
    pub avail_offset: u64,
    pub used_offset: u64,
    pub total_len: u64,
}

impl SplitQueueLayout {
    pub fn new(queue_size: u16, align: u64) -> Result<Self, Error> {
        if queue_size == 0 {
            return Err(Error::QueueSizeZero);
        }
        if !queue_size.is_power_of_two() {
            return Err(Error::QueueSizeNotPowerOfTwo);
        }
        if align < split_ring::USED_ALIGN || !align.is_power_of_two() {
            return Err(Error::QueueAlignInvalid);
        }

        let entries = u64::from(queue_size);
        let desc_offset = 0;
        let desc_len = entries
            .checked_mul(split_ring::DESC_LEN)
            .ok_or(Error::QueueLayoutOverflow)?;
        let avail_offset = desc_len;
        let avail_len = split_ring::AVAIL_HEADER_LEN
            .checked_add(
                entries
                    .checked_mul(split_ring::AVAIL_ENTRY_LEN)
                    .ok_or(Error::QueueLayoutOverflow)?,
            )
            .and_then(|len| len.checked_add(split_ring::AVAIL_USED_EVENT_LEN))
            .ok_or(Error::QueueLayoutOverflow)?;
        let used_offset = align_up(
            avail_offset
                .checked_add(avail_len)
                .ok_or(Error::QueueLayoutOverflow)?,
            align,
        )?;
        let used_len = split_ring::USED_HEADER_LEN
            .checked_add(
                entries
                    .checked_mul(split_ring::USED_ELEM_LEN)
                    .ok_or(Error::QueueLayoutOverflow)?,
            )
            .and_then(|len| len.checked_add(split_ring::USED_AVAIL_EVENT_LEN))
            .ok_or(Error::QueueLayoutOverflow)?;
        let total_len = used_offset
            .checked_add(used_len)
            .ok_or(Error::QueueLayoutOverflow)?;

        Ok(Self {
            queue_size,
            align,
            desc_offset,
            avail_offset,
            used_offset,
            total_len,
        })
    }

    pub const fn desc_table_len(self) -> u64 {
        self.avail_offset - self.desc_offset
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SplitDescriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

impl SplitDescriptor {
    pub const ENCODED_LEN: usize = 16;

    pub const fn new(addr: u64, len: u32, flags: u16, next: u16) -> Self {
        Self {
            addr,
            len,
            flags,
            next,
        }
    }

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..8].copy_from_slice(&self.addr.to_le_bytes());
        out[8..12].copy_from_slice(&self.len.to_le_bytes());
        out[12..14].copy_from_slice(&self.flags.to_le_bytes());
        out[14..16].copy_from_slice(&self.next.to_le_bytes());
        out
    }

    pub const fn with_next(mut self, next: u16) -> Self {
        self.flags |= split_ring::DESC_F_NEXT;
        self.next = next;
        self
    }

    pub const fn writable(mut self) -> Self {
        self.flags |= split_ring::DESC_F_WRITE;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockRequestHeader {
    pub request_type: u32,
    pub sector: u64,
}

impl BlockRequestHeader {
    pub const ENCODED_LEN: usize = 16;

    pub const fn read(sector: u64) -> Self {
        Self {
            request_type: block::T_IN,
            sector,
        }
    }

    pub const fn write(sector: u64) -> Self {
        Self {
            request_type: block::T_OUT,
            sector,
        }
    }

    pub const fn flush() -> Self {
        Self {
            request_type: block::T_FLUSH,
            sector: 0,
        }
    }

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..4].copy_from_slice(&self.request_type.to_le_bytes());
        out[4..8].copy_from_slice(&0u32.to_le_bytes());
        out[8..16].copy_from_slice(&self.sector.to_le_bytes());
        out
    }

    pub fn data_len_for_sectors(count: u32) -> Result<u32, Error> {
        if count == 0 {
            return Err(Error::BlockCountZero);
        }
        count
            .checked_mul(block::SECTOR_SIZE as u32)
            .ok_or(Error::BlockDataLenOverflow)
    }
}

fn align_up(value: u64, align: u64) -> Result<u64, Error> {
    let mask = align.checked_sub(1).ok_or(Error::QueueAlignInvalid)?;
    value
        .checked_add(mask)
        .map(|v| v & !mask)
        .ok_or(Error::QueueLayoutOverflow)
}

#[cfg(test)]
mod tests {
    use super::{
        block, ids, mmio, split_ring, BlockConfig, BlockRequestHeader, Error, MmioIdentity,
        SplitDescriptor, SplitQueueLayout,
    };

    #[test]
    fn mmio_offsets_match_modern_transport_layout() {
        assert_eq!(mmio::MAGIC_VALUE, 0x000);
        assert_eq!(mmio::VERSION, 0x004);
        assert_eq!(mmio::DEVICE_ID, 0x008);
        assert_eq!(mmio::VENDOR_ID, 0x00c);
        assert_eq!(mmio::DEVICE_FEATURES, 0x010);
        assert_eq!(mmio::DRIVER_FEATURES, 0x020);
        assert_eq!(mmio::QUEUE_SEL, 0x030);
        assert_eq!(mmio::QUEUE_READY, 0x044);
        assert_eq!(mmio::QUEUE_NOTIFY, 0x050);
        assert_eq!(mmio::INTERRUPT_STATUS, 0x060);
        assert_eq!(mmio::INTERRUPT_ACK, 0x064);
        assert_eq!(mmio::STATUS, 0x070);
        assert_eq!(mmio::QUEUE_DESC_LOW, 0x080);
        assert_eq!(mmio::QUEUE_AVAIL_LOW, 0x090);
        assert_eq!(mmio::QUEUE_USED_LOW, 0x0a0);
        assert_eq!(mmio::CONFIG_GENERATION, 0x0fc);
        assert_eq!(mmio::CONFIG, 0x100);
    }

    #[test]
    fn accepts_modern_mmio_block_identity() {
        let identity =
            MmioIdentity::new(mmio::MAGIC, mmio::VERSION_MODERN, ids::BLOCK, 0x4b55_4d4f);
        assert_eq!(identity.require_modern_block(), Ok(identity));
    }

    #[test]
    fn rejects_non_block_or_non_modern_identities() {
        assert_eq!(
            MmioIdentity::new(0, mmio::VERSION_MODERN, ids::BLOCK, 0).require_modern_block(),
            Err(Error::BadMagic)
        );
        assert_eq!(
            MmioIdentity::new(mmio::MAGIC, mmio::VERSION_LEGACY, ids::BLOCK, 0)
                .require_modern_block(),
            Err(Error::LegacyDevice)
        );
        assert_eq!(
            MmioIdentity::new(mmio::MAGIC, 3, ids::BLOCK, 0).require_modern_block(),
            Err(Error::UnsupportedVersion)
        );
        assert_eq!(
            MmioIdentity::new(mmio::MAGIC, mmio::VERSION_MODERN, ids::NONE, 0)
                .require_modern_block(),
            Err(Error::MissingDevice)
        );
        assert_eq!(
            MmioIdentity::new(
                mmio::MAGIC,
                mmio::VERSION_MODERN,
                ids::TRANSITIONAL_BLOCK,
                0
            )
            .require_modern_block(),
            Err(Error::UnsupportedDevice)
        );
    }

    #[test]
    fn decodes_block_capacity_as_le_512_byte_sectors() {
        let config = [0x34, 0x12, 0, 0, 0, 0, 0, 0];
        let block = BlockConfig::from_bytes(&config).unwrap();
        assert_eq!(block.capacity_sectors, 0x1234);
        assert_eq!(block.capacity_bytes(), Ok(0x1234 * 512));
    }

    #[test]
    fn rejects_short_or_overflowing_block_capacity() {
        assert_eq!(BlockConfig::from_bytes(&[0; 7]), Err(Error::ConfigTooShort));
        assert_eq!(
            BlockConfig {
                capacity_sectors: u64::MAX
            }
            .capacity_bytes(),
            Err(Error::CapacityOverflow)
        );
    }

    #[test]
    fn exposes_read_only_feature_and_common_request_status_constants() {
        assert!(!BlockConfig::is_read_only(0));
        assert!(BlockConfig::is_read_only(block::FEATURE_RO));
        assert_eq!(block::T_IN, 0);
        assert_eq!(block::T_OUT, 1);
        assert_eq!(block::T_FLUSH, 4);
        assert_eq!(block::STATUS_OK, 0);
        assert_eq!(block::STATUS_IOERR, 1);
        assert_eq!(block::STATUS_UNSUPP, 2);
    }

    #[test]
    fn split_queue_layout_matches_vring_size_formula() {
        let layout = SplitQueueLayout::new(8, 4096).unwrap();
        assert_eq!(
            layout,
            SplitQueueLayout {
                queue_size: 8,
                align: 4096,
                desc_offset: 0,
                avail_offset: 128,
                used_offset: 4096,
                total_len: 4166,
            }
        );
        assert_eq!(layout.desc_table_len(), 8 * split_ring::DESC_LEN);

        let larger = SplitQueueLayout::new(128, 4096).unwrap();
        assert_eq!(larger.avail_offset, 2048);
        assert_eq!(larger.used_offset, 4096);
        assert_eq!(larger.total_len, 5126);
    }

    #[test]
    fn split_queue_layout_rejects_unusable_queue_shapes() {
        assert_eq!(SplitQueueLayout::new(0, 4096), Err(Error::QueueSizeZero));
        assert_eq!(
            SplitQueueLayout::new(3, 4096),
            Err(Error::QueueSizeNotPowerOfTwo)
        );
        assert_eq!(SplitQueueLayout::new(8, 0), Err(Error::QueueAlignInvalid));
        assert_eq!(SplitQueueLayout::new(8, 2), Err(Error::QueueAlignInvalid));
        assert_eq!(SplitQueueLayout::new(8, 24), Err(Error::QueueAlignInvalid));
        assert_eq!(
            super::align_up(u64::MAX, 2),
            Err(Error::QueueLayoutOverflow)
        );
    }

    #[test]
    fn split_descriptor_encodes_in_virtio_little_endian_order() {
        let desc = SplitDescriptor::new(0x1122_3344_5566_7788, 0x99aa_bbcc, 0, 0)
            .with_next(5)
            .writable();
        assert_eq!(
            desc.encode(),
            [
                0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xcc, 0xbb, 0xaa, 0x99, 0x03, 0x00,
                0x05, 0x00,
            ]
        );
    }

    #[test]
    fn block_request_header_encodes_type_ioprio_and_sector() {
        assert_eq!(
            BlockRequestHeader::read(0x1122_3344_5566_7788).encode(),
            [
                0x00, 0x00, 0x00, 0x00, // request type
                0x00, 0x00, 0x00, 0x00, // ioprio
                0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
            ]
        );
        assert_eq!(BlockRequestHeader::write(7).request_type, block::T_OUT);
        assert_eq!(
            BlockRequestHeader::flush().encode()[0],
            block::T_FLUSH as u8
        );
    }

    #[test]
    fn block_request_data_len_is_sector_count_in_bytes() {
        assert_eq!(BlockRequestHeader::data_len_for_sectors(1), Ok(512));
        assert_eq!(BlockRequestHeader::data_len_for_sectors(8), Ok(4096));
        assert_eq!(
            BlockRequestHeader::data_len_for_sectors(0),
            Err(Error::BlockCountZero)
        );
        assert_eq!(
            BlockRequestHeader::data_len_for_sectors(u32::MAX),
            Err(Error::BlockDataLenOverflow)
        );
    }
}
