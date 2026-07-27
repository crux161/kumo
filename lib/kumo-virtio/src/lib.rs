#![no_std]
//j415
//j416
//j515
//j516
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

/// Device-status bits shared by every modern virtio transport.
pub mod device_status {
    pub const ACKNOWLEDGE: u32 = 1 << 0;
    pub const DRIVER: u32 = 1 << 1;
    pub const DRIVER_OK: u32 = 1 << 2;
    pub const FEATURES_OK: u32 = 1 << 3;
    pub const DEVICE_NEEDS_RESET: u32 = 1 << 6;
    pub const FAILED: u32 = 1 << 7;
}

/// Transport feature bits understood by KUMO's first modern virtio driver.
pub mod device_features {
    pub const VERSION_1: u64 = 1 << 32;
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
    ResetTimedOut,
    MissingVersion1,
    ReadOnlyDevice,
    FeaturesRejected,
    DeviceNeedsReset,
    QueueUnavailable,
    QueueAlreadyReady,
    QueueMemoryMisaligned,
    QueueMemoryTooSmall,
    QueueAddressOverflow,
    QueueRejected,
    DriverStatusInvalid,
    DriverRejected,
}

/// A dword register port for the modern virtio-MMIO register window. A live
/// implementation performs volatile accesses; tests use a replayable fake.
pub trait RegisterIo {
    fn read32(&mut self, offset: u32) -> u32;
    fn write32(&mut self, offset: u32, value: u32);
}

/// Result of the feature-negotiation phase. Queue memory is deliberately not
/// configured yet, so [`device_status::DRIVER_OK`] must still be clear.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedFeatures {
    pub identity: MmioIdentity,
    pub offered: u64,
    pub accepted: u64,
}

impl NegotiatedFeatures {
    pub const fn supports_flush(self) -> bool {
        self.accepted & block::FEATURE_FLUSH != 0
    }
}

/// A contiguous, zeroed, device-visible span reserved for one split virtqueue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SplitQueueMemory {
    pub device_addr: u64,
    pub len: u64,
    pub preferred_size: u16,
}

/// The addresses accepted for an active split virtqueue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveSplitQueue {
    pub index: u16,
    pub layout: SplitQueueLayout,
    pub descriptor: u64,
    pub available: u64,
    pub used: u64,
}

/// Keep the device-written used ring on its own page in the first block
/// transport. Caller-supplied queue memory uses this alignment too.
pub const SPLIT_QUEUE_ALIGN: u64 = 4096;

/// Maximum status reads allowed while a device completes reset. The live
/// driver can retry the whole probe later; an absent or wedged device cannot
/// hold its server forever.
pub const RESET_POLL_LIMIT: usize = 1024;

/// Reset and negotiate the minimal feature set for KUMO's first writable
/// virtio-MMIO block backend.
///
/// The driver requires the modern VERSION_1 contract and refuses a read-only
/// disk. It accepts FLUSH when offered and ignores all other optional features.
/// Queue selection, address programming, and DRIVER_OK belong to the next
/// phase, after the caller has allocated device-visible ring memory.
pub fn negotiate_writable_block<IO: RegisterIo>(io: &mut IO) -> Result<NegotiatedFeatures, Error> {
    let identity = MmioIdentity::new(
        io.read32(mmio::MAGIC_VALUE),
        io.read32(mmio::VERSION),
        io.read32(mmio::DEVICE_ID),
        io.read32(mmio::VENDOR_ID),
    )
    .require_modern_block()?;

    io.write32(mmio::STATUS, 0);
    let mut reset = false;
    for _ in 0..RESET_POLL_LIMIT {
        if io.read32(mmio::STATUS) == 0 {
            reset = true;
            break;
        }
    }
    if !reset {
        return Err(Error::ResetTimedOut);
    }

    let mut progress = device_status::ACKNOWLEDGE;
    io.write32(mmio::STATUS, progress);
    progress |= device_status::DRIVER;
    io.write32(mmio::STATUS, progress);

    let offered = read_device_features(io);
    if offered & device_features::VERSION_1 == 0 {
        return fail_negotiation(io, progress, Error::MissingVersion1);
    }
    if BlockConfig::is_read_only(offered) {
        return fail_negotiation(io, progress, Error::ReadOnlyDevice);
    }

    let accepted = device_features::VERSION_1 | (offered & block::FEATURE_FLUSH);
    write_driver_features(io, accepted);

    progress |= device_status::FEATURES_OK;
    io.write32(mmio::STATUS, progress);
    let observed = io.read32(mmio::STATUS);
    if observed & device_status::DEVICE_NEEDS_RESET != 0 {
        return fail_negotiation(
            io,
            progress & !device_status::FEATURES_OK,
            Error::DeviceNeedsReset,
        );
    }
    if observed & device_status::FEATURES_OK == 0 {
        return fail_negotiation(
            io,
            progress & !device_status::FEATURES_OK,
            Error::FeaturesRejected,
        );
    }

    Ok(NegotiatedFeatures {
        identity,
        offered,
        accepted,
    })
}

/// Program one modern split queue and make the negotiated block device live.
///
/// Queue memory must already be zeroed, device-visible, and
/// [`SPLIT_QUEUE_ALIGN`] aligned. The requested queue size is a power-of-two
/// ceiling; a smaller power of two is selected when required by `QueueNumMax`.
/// `DRIVER_OK` is written only after `QueueReady` reads back as one.
pub fn activate_block_queue<IO: RegisterIo>(
    io: &mut IO,
    negotiated: NegotiatedFeatures,
    queue_index: u16,
    memory: SplitQueueMemory,
) -> Result<ActiveSplitQueue, Error> {
    if negotiated.accepted & device_features::VERSION_1 == 0 {
        return Err(Error::MissingVersion1);
    }
    if memory.preferred_size == 0 {
        return Err(Error::QueueSizeZero);
    }
    if !memory.preferred_size.is_power_of_two() {
        return Err(Error::QueueSizeNotPowerOfTwo);
    }
    if memory.device_addr & (SPLIT_QUEUE_ALIGN - 1) != 0 {
        return Err(Error::QueueMemoryMisaligned);
    }

    let ready_status =
        device_status::ACKNOWLEDGE | device_status::DRIVER | device_status::FEATURES_OK;
    let observed = io.read32(mmio::STATUS);
    if observed & device_status::DEVICE_NEEDS_RESET != 0 {
        return Err(Error::DeviceNeedsReset);
    }
    if observed != ready_status {
        return Err(Error::DriverStatusInvalid);
    }

    io.write32(mmio::QUEUE_SEL, u32::from(queue_index));
    if io.read32(mmio::QUEUE_READY) != 0 {
        return fail_activation(io, ready_status, Error::QueueAlreadyReady);
    }

    let queue_size = match bounded_queue_size(io.read32(mmio::QUEUE_NUM_MAX), memory.preferred_size)
    {
        Ok(queue_size) => queue_size,
        Err(error) => return fail_activation(io, ready_status, error),
    };
    let layout = match SplitQueueLayout::new(queue_size, SPLIT_QUEUE_ALIGN) {
        Ok(layout) => layout,
        Err(error) => return fail_activation(io, ready_status, error),
    };
    if layout.total_len > memory.len {
        return fail_activation(io, ready_status, Error::QueueMemoryTooSmall);
    }
    if memory.device_addr.checked_add(layout.total_len).is_none() {
        return fail_activation(io, ready_status, Error::QueueAddressOverflow);
    }

    let descriptor = match memory.device_addr.checked_add(layout.desc_offset) {
        Some(address) => address,
        None => return fail_activation(io, ready_status, Error::QueueAddressOverflow),
    };
    let available = match memory.device_addr.checked_add(layout.avail_offset) {
        Some(address) => address,
        None => return fail_activation(io, ready_status, Error::QueueAddressOverflow),
    };
    let used = match memory.device_addr.checked_add(layout.used_offset) {
        Some(address) => address,
        None => return fail_activation(io, ready_status, Error::QueueAddressOverflow),
    };

    io.write32(mmio::QUEUE_NUM, u32::from(queue_size));
    write_address(io, mmio::QUEUE_DESC_LOW, descriptor);
    write_address(io, mmio::QUEUE_AVAIL_LOW, available);
    write_address(io, mmio::QUEUE_USED_LOW, used);
    io.write32(mmio::QUEUE_READY, 1);
    if io.read32(mmio::QUEUE_READY) != 1 {
        return fail_activation(io, ready_status, Error::QueueRejected);
    }

    let observed = io.read32(mmio::STATUS);
    if observed & device_status::DEVICE_NEEDS_RESET != 0 {
        return fail_activation(io, ready_status, Error::DeviceNeedsReset);
    }
    if observed != ready_status {
        return fail_activation(io, ready_status, Error::DriverStatusInvalid);
    }

    io.write32(mmio::STATUS, ready_status | device_status::DRIVER_OK);
    let observed = io.read32(mmio::STATUS);
    if observed & device_status::DEVICE_NEEDS_RESET != 0 {
        return fail_activation(io, ready_status, Error::DeviceNeedsReset);
    }
    if observed != (ready_status | device_status::DRIVER_OK) {
        return fail_activation(io, ready_status, Error::DriverRejected);
    }

    Ok(ActiveSplitQueue {
        index: queue_index,
        layout,
        descriptor,
        available,
        used,
    })
}

fn bounded_queue_size(maximum: u32, preferred: u16) -> Result<u16, Error> {
    let limit = maximum.min(u32::from(preferred));
    if limit == 0 {
        return Err(Error::QueueUnavailable);
    }
    let queue_size = 1 << (u32::BITS - 1 - limit.leading_zeros());
    Ok(queue_size as u16)
}

fn write_address<IO: RegisterIo>(io: &mut IO, low_offset: u32, address: u64) {
    io.write32(low_offset, address as u32);
    io.write32(low_offset + 4, (address >> 32) as u32);
}

fn fail_activation<IO: RegisterIo, T>(
    io: &mut IO,
    ready_status: u32,
    error: Error,
) -> Result<T, Error> {
    io.write32(mmio::STATUS, ready_status | device_status::FAILED);
    Err(error)
}

fn read_device_features<IO: RegisterIo>(io: &mut IO) -> u64 {
    io.write32(mmio::DEVICE_FEATURES_SEL, 0);
    let low = io.read32(mmio::DEVICE_FEATURES) as u64;
    io.write32(mmio::DEVICE_FEATURES_SEL, 1);
    let high = io.read32(mmio::DEVICE_FEATURES) as u64;
    low | high << 32
}

fn write_driver_features<IO: RegisterIo>(io: &mut IO, features: u64) {
    io.write32(mmio::DRIVER_FEATURES_SEL, 0);
    io.write32(mmio::DRIVER_FEATURES, features as u32);
    io.write32(mmio::DRIVER_FEATURES_SEL, 1);
    io.write32(mmio::DRIVER_FEATURES, (features >> 32) as u32);
}

fn fail_negotiation<IO: RegisterIo, T>(
    io: &mut IO,
    progress: u32,
    error: Error,
) -> Result<T, Error> {
    io.write32(mmio::STATUS, progress | device_status::FAILED);
    Err(error)
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
        activate_block_queue, block, device_features, device_status, ids, mmio,
        negotiate_writable_block, split_ring, BlockConfig, BlockRequestHeader, Error, MmioIdentity,
        RegisterIo, SplitDescriptor, SplitQueueLayout, SplitQueueMemory, RESET_POLL_LIMIT,
    };

    struct FakeRegisters {
        magic: u32,
        version: u32,
        device_id: u32,
        vendor_id: u32,
        offered: u64,
        status: u32,
        device_features_sel: u32,
        driver_features_sel: u32,
        driver_features: u64,
        reset_reads: usize,
        reset_pending: bool,
        reject_features: bool,
        needs_reset: bool,
        queue_sel: u32,
        queue_num_max: u32,
        queue_num: u32,
        queue_ready: u32,
        descriptor: u64,
        available: u64,
        used: u64,
        reject_queue: bool,
        reject_driver: bool,
        needs_reset_on_queue: bool,
        writes: [(u32, u32); 32],
        write_len: usize,
    }

    impl FakeRegisters {
        fn modern(offered: u64) -> Self {
            Self {
                magic: mmio::MAGIC,
                version: mmio::VERSION_MODERN,
                device_id: ids::BLOCK,
                vendor_id: 0x4b55_4d4f,
                offered,
                status: device_status::DRIVER_OK,
                device_features_sel: 0,
                driver_features_sel: 0,
                driver_features: 0,
                reset_reads: 0,
                reset_pending: false,
                reject_features: false,
                needs_reset: false,
                queue_sel: 0,
                queue_num_max: 8,
                queue_num: 0,
                queue_ready: 0,
                descriptor: 0,
                available: 0,
                used: 0,
                reject_queue: false,
                reject_driver: false,
                needs_reset_on_queue: false,
                writes: [(0, 0); 32],
                write_len: 0,
            }
        }

        fn record(&mut self, offset: u32, value: u32) {
            self.writes[self.write_len] = (offset, value);
            self.write_len += 1;
        }

        fn last_write(&self) -> (u32, u32) {
            self.writes[self.write_len - 1]
        }
    }

    impl RegisterIo for FakeRegisters {
        fn read32(&mut self, offset: u32) -> u32 {
            match offset {
                mmio::MAGIC_VALUE => self.magic,
                mmio::VERSION => self.version,
                mmio::DEVICE_ID => self.device_id,
                mmio::VENDOR_ID => self.vendor_id,
                mmio::DEVICE_FEATURES => {
                    if self.device_features_sel == 0 {
                        self.offered as u32
                    } else {
                        (self.offered >> 32) as u32
                    }
                }
                mmio::QUEUE_NUM_MAX => self.queue_num_max,
                mmio::QUEUE_READY => self.queue_ready,
                mmio::STATUS => {
                    if self.reset_pending {
                        if self.reset_reads != 0 {
                            self.reset_reads -= 1;
                        }
                        if self.reset_reads == 0 {
                            self.status = 0;
                            self.reset_pending = false;
                        }
                    }
                    self.status
                }
                _ => 0,
            }
        }

        fn write32(&mut self, offset: u32, value: u32) {
            self.record(offset, value);
            match offset {
                mmio::DEVICE_FEATURES_SEL => self.device_features_sel = value,
                mmio::DRIVER_FEATURES_SEL => self.driver_features_sel = value,
                mmio::DRIVER_FEATURES => {
                    if self.driver_features_sel == 0 {
                        self.driver_features =
                            (self.driver_features & !(u32::MAX as u64)) | u64::from(value);
                    } else {
                        self.driver_features =
                            (self.driver_features & u32::MAX as u64) | (u64::from(value) << 32);
                    }
                }
                mmio::QUEUE_SEL => self.queue_sel = value,
                mmio::QUEUE_NUM => self.queue_num = value,
                mmio::QUEUE_DESC_LOW => {
                    self.descriptor = (self.descriptor & !u64::from(u32::MAX)) | u64::from(value);
                }
                mmio::QUEUE_DESC_HIGH => {
                    self.descriptor =
                        (self.descriptor & u64::from(u32::MAX)) | (u64::from(value) << 32);
                }
                mmio::QUEUE_AVAIL_LOW => {
                    self.available = (self.available & !u64::from(u32::MAX)) | u64::from(value);
                }
                mmio::QUEUE_AVAIL_HIGH => {
                    self.available =
                        (self.available & u64::from(u32::MAX)) | (u64::from(value) << 32);
                }
                mmio::QUEUE_USED_LOW => {
                    self.used = (self.used & !u64::from(u32::MAX)) | u64::from(value);
                }
                mmio::QUEUE_USED_HIGH => {
                    self.used = (self.used & u64::from(u32::MAX)) | (u64::from(value) << 32);
                }
                mmio::QUEUE_READY => {
                    if !self.reject_queue {
                        self.queue_ready = value;
                    }
                    if self.needs_reset_on_queue {
                        self.status |= device_status::DEVICE_NEEDS_RESET;
                    }
                }
                mmio::STATUS if value == 0 => {
                    self.reset_pending = true;
                    if self.reset_reads == 0 {
                        self.status = 0;
                        self.reset_pending = false;
                    }
                }
                mmio::STATUS => {
                    self.status = if value & device_status::DRIVER_OK != 0 && self.reject_driver {
                        value & !device_status::DRIVER_OK
                    } else if value & device_status::FEATURES_OK != 0 && self.reject_features {
                        value & !device_status::FEATURES_OK
                    } else if value & device_status::FEATURES_OK != 0 && self.needs_reset {
                        value | device_status::DEVICE_NEEDS_RESET
                    } else {
                        value
                    };
                }
                _ => {}
            }
        }
    }

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
    fn writable_negotiation_accepts_only_version_one_and_optional_flush() {
        let offered =
            device_features::VERSION_1 | block::FEATURE_FLUSH | block::FEATURE_BLK_SIZE | (1 << 35);
        let mut io = FakeRegisters::modern(offered);

        let negotiated = negotiate_writable_block(&mut io).unwrap();

        assert_eq!(negotiated.identity.vendor_id, 0x4b55_4d4f);
        assert_eq!(negotiated.offered, offered);
        assert_eq!(
            negotiated.accepted,
            device_features::VERSION_1 | block::FEATURE_FLUSH
        );
        assert!(negotiated.supports_flush());
        assert_eq!(io.driver_features, negotiated.accepted);
        assert_eq!(
            &io.writes[..io.write_len],
            &[
                (mmio::STATUS, 0),
                (mmio::STATUS, device_status::ACKNOWLEDGE),
                (
                    mmio::STATUS,
                    device_status::ACKNOWLEDGE | device_status::DRIVER,
                ),
                (mmio::DEVICE_FEATURES_SEL, 0),
                (mmio::DEVICE_FEATURES_SEL, 1),
                (mmio::DRIVER_FEATURES_SEL, 0),
                (mmio::DRIVER_FEATURES, block::FEATURE_FLUSH as u32),
                (mmio::DRIVER_FEATURES_SEL, 1),
                (mmio::DRIVER_FEATURES, 1),
                (
                    mmio::STATUS,
                    device_status::ACKNOWLEDGE | device_status::DRIVER | device_status::FEATURES_OK,
                ),
            ]
        );
        assert_eq!(io.status & device_status::DRIVER_OK, 0);
    }

    #[test]
    fn negotiation_waits_for_reset_and_leaves_flush_optional() {
        let mut io = FakeRegisters::modern(device_features::VERSION_1);
        io.reset_reads = 3;

        let negotiated = negotiate_writable_block(&mut io).unwrap();

        assert!(!negotiated.supports_flush());
        assert_eq!(negotiated.accepted, device_features::VERSION_1);
    }

    #[test]
    fn negotiation_rejects_incompatible_or_refused_features() {
        let failed = device_status::ACKNOWLEDGE | device_status::DRIVER | device_status::FAILED;

        let mut missing_version = FakeRegisters::modern(block::FEATURE_FLUSH);
        assert_eq!(
            negotiate_writable_block(&mut missing_version),
            Err(Error::MissingVersion1)
        );
        assert_eq!(missing_version.last_write(), (mmio::STATUS, failed));

        let mut read_only = FakeRegisters::modern(device_features::VERSION_1 | block::FEATURE_RO);
        assert_eq!(
            negotiate_writable_block(&mut read_only),
            Err(Error::ReadOnlyDevice)
        );
        assert_eq!(read_only.last_write(), (mmio::STATUS, failed));

        let mut rejected = FakeRegisters::modern(device_features::VERSION_1);
        rejected.reject_features = true;
        assert_eq!(
            negotiate_writable_block(&mut rejected),
            Err(Error::FeaturesRejected)
        );
        assert_eq!(rejected.last_write(), (mmio::STATUS, failed));

        let mut needs_reset = FakeRegisters::modern(device_features::VERSION_1);
        needs_reset.needs_reset = true;
        assert_eq!(
            negotiate_writable_block(&mut needs_reset),
            Err(Error::DeviceNeedsReset)
        );
        assert_eq!(needs_reset.last_write(), (mmio::STATUS, failed));
    }

    #[test]
    fn negotiation_rejects_bad_identity_and_bounded_reset_timeout() {
        let mut wrong_device = FakeRegisters::modern(device_features::VERSION_1);
        wrong_device.device_id = 3;
        assert_eq!(
            negotiate_writable_block(&mut wrong_device),
            Err(Error::UnsupportedDevice)
        );
        assert_eq!(wrong_device.write_len, 0);

        let mut wedged = FakeRegisters::modern(device_features::VERSION_1);
        wedged.reset_reads = RESET_POLL_LIMIT + 1;
        assert_eq!(
            negotiate_writable_block(&mut wedged),
            Err(Error::ResetTimedOut)
        );
        assert_eq!(&wedged.writes[..wedged.write_len], &[(mmio::STATUS, 0)]);
    }

    #[test]
    fn activation_programs_one_queue_before_driver_ok() {
        let mut io = FakeRegisters::modern(device_features::VERSION_1 | block::FEATURE_FLUSH);
        let negotiated = negotiate_writable_block(&mut io).unwrap();
        let first_activation_write = io.write_len;

        let queue = activate_block_queue(
            &mut io,
            negotiated,
            0,
            SplitQueueMemory {
                device_addr: 0x1234_5678_0000,
                len: 8192,
                preferred_size: 8,
            },
        )
        .unwrap();

        assert_eq!(queue.index, 0);
        assert_eq!(queue.layout.queue_size, 8);
        assert_eq!(queue.descriptor, 0x1234_5678_0000);
        assert_eq!(queue.available, 0x1234_5678_0080);
        assert_eq!(queue.used, 0x1234_5678_1000);
        assert_eq!(io.queue_num, 8);
        assert_eq!(io.descriptor, queue.descriptor);
        assert_eq!(io.available, queue.available);
        assert_eq!(io.used, queue.used);
        assert_eq!(io.queue_ready, 1);
        assert_eq!(
            &io.writes[first_activation_write..io.write_len],
            &[
                (mmio::QUEUE_SEL, 0),
                (mmio::QUEUE_NUM, 8),
                (mmio::QUEUE_DESC_LOW, 0x5678_0000),
                (mmio::QUEUE_DESC_HIGH, 0x1234),
                (mmio::QUEUE_AVAIL_LOW, 0x5678_0080),
                (mmio::QUEUE_AVAIL_HIGH, 0x1234),
                (mmio::QUEUE_USED_LOW, 0x5678_1000),
                (mmio::QUEUE_USED_HIGH, 0x1234),
                (mmio::QUEUE_READY, 1),
                (
                    mmio::STATUS,
                    device_status::ACKNOWLEDGE
                        | device_status::DRIVER
                        | device_status::FEATURES_OK
                        | device_status::DRIVER_OK,
                ),
            ]
        );
    }

    #[test]
    fn activation_bounds_queue_size_to_device_maximum() {
        let mut io = FakeRegisters::modern(device_features::VERSION_1);
        io.queue_num_max = 6;
        let negotiated = negotiate_writable_block(&mut io).unwrap();

        let queue = activate_block_queue(
            &mut io,
            negotiated,
            3,
            SplitQueueMemory {
                device_addr: 0x2000,
                len: 8192,
                preferred_size: 8,
            },
        )
        .unwrap();

        assert_eq!(io.queue_sel, 3);
        assert_eq!(queue.layout.queue_size, 4);
        assert_eq!(io.queue_num, 4);
    }

    #[test]
    fn activation_rejects_unusable_queue_memory() {
        let mut io = FakeRegisters::modern(device_features::VERSION_1);
        let negotiated = negotiate_writable_block(&mut io).unwrap();
        let before = io.write_len;

        assert_eq!(
            activate_block_queue(
                &mut io,
                negotiated,
                0,
                SplitQueueMemory {
                    device_addr: 0x1008,
                    len: 8192,
                    preferred_size: 8,
                },
            ),
            Err(Error::QueueMemoryMisaligned)
        );
        assert_eq!(io.write_len, before);

        assert_eq!(
            activate_block_queue(
                &mut io,
                negotiated,
                0,
                SplitQueueMemory {
                    device_addr: 0x1000,
                    len: 4096,
                    preferred_size: 8,
                },
            ),
            Err(Error::QueueMemoryTooSmall)
        );
        assert_eq!(io.last_write().0, mmio::STATUS);
        assert_ne!(io.status & device_status::FAILED, 0);

        let mut overflow = FakeRegisters::modern(device_features::VERSION_1);
        let negotiated = negotiate_writable_block(&mut overflow).unwrap();
        assert_eq!(
            activate_block_queue(
                &mut overflow,
                negotiated,
                0,
                SplitQueueMemory {
                    device_addr: u64::MAX - 4095,
                    len: u64::MAX,
                    preferred_size: 8,
                },
            ),
            Err(Error::QueueAddressOverflow)
        );
    }

    #[test]
    fn activation_never_sets_driver_ok_for_a_refused_queue() {
        let failed = device_status::ACKNOWLEDGE
            | device_status::DRIVER
            | device_status::FEATURES_OK
            | device_status::FAILED;

        let mut already_ready = FakeRegisters::modern(device_features::VERSION_1);
        let negotiated = negotiate_writable_block(&mut already_ready).unwrap();
        already_ready.queue_ready = 1;
        assert_eq!(
            activate_block_queue(
                &mut already_ready,
                negotiated,
                0,
                SplitQueueMemory {
                    device_addr: 0x1000,
                    len: 8192,
                    preferred_size: 8,
                },
            ),
            Err(Error::QueueAlreadyReady)
        );
        assert_eq!(already_ready.last_write(), (mmio::STATUS, failed));

        let mut unavailable = FakeRegisters::modern(device_features::VERSION_1);
        unavailable.queue_num_max = 0;
        let negotiated = negotiate_writable_block(&mut unavailable).unwrap();
        assert_eq!(
            activate_block_queue(
                &mut unavailable,
                negotiated,
                0,
                SplitQueueMemory {
                    device_addr: 0x1000,
                    len: 8192,
                    preferred_size: 8,
                },
            ),
            Err(Error::QueueUnavailable)
        );
        assert_eq!(unavailable.last_write(), (mmio::STATUS, failed));

        let mut rejected = FakeRegisters::modern(device_features::VERSION_1);
        rejected.reject_queue = true;
        let negotiated = negotiate_writable_block(&mut rejected).unwrap();
        assert_eq!(
            activate_block_queue(
                &mut rejected,
                negotiated,
                0,
                SplitQueueMemory {
                    device_addr: 0x1000,
                    len: 8192,
                    preferred_size: 8,
                },
            ),
            Err(Error::QueueRejected)
        );
        assert_eq!(rejected.last_write(), (mmio::STATUS, failed));
        assert_eq!(rejected.status & device_status::DRIVER_OK, 0);
        assert!(!rejected.writes[..rejected.write_len]
            .iter()
            .any(
                |&(offset, value)| offset == mmio::STATUS && value & device_status::DRIVER_OK != 0
            ));
    }

    #[test]
    fn activation_fails_if_driver_ok_or_device_health_is_refused() {
        let mut rejected = FakeRegisters::modern(device_features::VERSION_1);
        rejected.reject_driver = true;
        let negotiated = negotiate_writable_block(&mut rejected).unwrap();
        assert_eq!(
            activate_block_queue(
                &mut rejected,
                negotiated,
                0,
                SplitQueueMemory {
                    device_addr: 0x1000,
                    len: 8192,
                    preferred_size: 8,
                },
            ),
            Err(Error::DriverRejected)
        );
        assert_eq!(rejected.status & device_status::DRIVER_OK, 0);
        assert_ne!(rejected.status & device_status::FAILED, 0);

        let mut needs_reset = FakeRegisters::modern(device_features::VERSION_1);
        needs_reset.needs_reset_on_queue = true;
        let negotiated = negotiate_writable_block(&mut needs_reset).unwrap();
        assert_eq!(
            activate_block_queue(
                &mut needs_reset,
                negotiated,
                0,
                SplitQueueMemory {
                    device_addr: 0x1000,
                    len: 8192,
                    preferred_size: 8,
                },
            ),
            Err(Error::DeviceNeedsReset)
        );
        assert_eq!(needs_reset.status & device_status::DRIVER_OK, 0);
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
