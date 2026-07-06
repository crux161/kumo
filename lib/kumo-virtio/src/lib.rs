#![no_std]
//j415
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    BadMagic,
    UnsupportedVersion,
    LegacyDevice,
    MissingDevice,
    UnsupportedDevice,
    ConfigTooShort,
    CapacityOverflow,
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

#[cfg(test)]
mod tests {
    use super::{block, ids, mmio, BlockConfig, Error, MmioIdentity};

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
}
