//j930
//! One synchronous virtio-blk request at a time over an active split queue (SH8-d, j930).
//!
//! The queue and every request buffer live in ONE physically contiguous DMA region the driver
//! owns ([`DmaRegion`]). The device sees physical addresses, and the driver reads and writes
//! through offsets into its mapping. A request is a three-descriptor chain:
//! - the header (device-readable);
//! - the data (device-readable for `OUT`, device-writable for `IN`, absent for `FLUSH`);
//! - the status byte (device-writable).
//!
//! It is published in the available ring and announced with `QueueNotify`. Completion is the
//! used ring's index moving past the driver's last-seen value. Only one request is ever in
//! flight, so the chain always occupies descriptors 0–2 and no free list is needed. The
//! ordering rule: everything the device reads is written before the available index, and the
//! index before the notify ([`DmaRegion::barrier`] between them). The used index is read
//! before the status byte it publishes.

use crate::{
    block, mmio, ActiveSplitQueue, BlockRequestHeader, Error, RegisterIo, SplitDescriptor,
    SplitQueueLayout,
};

/// The driver's view of its DMA region: byte offsets from the region's start.
pub trait DmaRegion {
    /// The device-visible (physical) address of offset 0.
    fn device_addr(&self) -> u64;
    fn write(&mut self, offset: usize, bytes: &[u8]);
    fn read(&mut self, offset: usize, out: &mut [u8]);
    /// Order every earlier access before every later one, device-visibly (`dmb osh` on target).
    fn barrier(&mut self);
}

/// Where everything lives in the region: the split queue first (its used ring on its own page,
/// [`crate::SPLIT_QUEUE_ALIGN`]), then one page for the request header and status, then the
/// data buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionLayout {
    pub queue: SplitQueueLayout,
    pub header_offset: usize,
    pub status_offset: usize,
    pub data_offset: usize,
    pub data_len: usize,
    pub total_len: usize,
}

/// Sectors one request moves at most: 64 KiB, one drv-blk bulk command (`READ_VMO_MAX_BLOCKS`).
pub const MAX_SECTORS: u32 = 128;
/// The queue size the driver asks for (a single request needs three descriptors).
pub const QUEUE_SIZE: u16 = 8;

impl RegionLayout {
    pub fn new() -> Result<Self, Error> {
        let queue = SplitQueueLayout::new(QUEUE_SIZE, crate::SPLIT_QUEUE_ALIGN)?;
        let page = crate::SPLIT_QUEUE_ALIGN as usize;
        let header_offset = (queue.total_len as usize).div_ceil(page) * page;
        let status_offset = header_offset + BlockRequestHeader::ENCODED_LEN;
        let data_offset = header_offset + page;
        let data_len = (MAX_SECTORS as u64 * block::SECTOR_SIZE) as usize;
        Ok(Self {
            queue,
            header_offset,
            status_offset,
            data_offset,
            data_len,
            total_len: data_offset + data_len.div_ceil(page) * page,
        })
    }
}

/// What a request moves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    /// Read `count` sectors from `sector` into the data buffer.
    Read {
        sector: u64,
        count: u32,
    },
    /// Write `count` sectors at `sector` from the data buffer.
    Write {
        sector: u64,
        count: u32,
    },
    Flush,
}

/// How a request ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Ok,
    IoError,
    Unsupported,
    /// The device answered with a status byte virtio does not define.
    Malformed,
}

/// The driver's side of one active block queue.
pub struct BlockQueue {
    queue: ActiveSplitQueue,
    layout: RegionLayout,
    avail_idx: u16,
    used_seen: u16,
    in_flight: bool,
    pub capacity_sectors: u64,
}

impl BlockQueue {
    /// Wrap a queue `activate_block_queue` made live over `layout`'s region.
    pub fn new(queue: ActiveSplitQueue, layout: RegionLayout, capacity_sectors: u64) -> Self {
        Self {
            queue,
            layout,
            avail_idx: 0,
            used_seen: 0,
            in_flight: false,
            capacity_sectors,
        }
    }

    pub fn layout(&self) -> RegionLayout {
        self.layout
    }

    /// Build the chain, publish it and notify the device. The data buffer must already hold a
    /// write's bytes. Fails without touching the device on a bad range or a busy queue.
    pub fn submit<M: DmaRegion, IO: RegisterIo>(
        &mut self,
        mem: &mut M,
        io: &mut IO,
        op: Op,
    ) -> Result<(), Error> {
        if self.in_flight {
            return Err(Error::QueueUnavailable);
        }
        let (header, data) = match op {
            Op::Read { sector, count } => (BlockRequestHeader::read(sector), Some((count, true))),
            Op::Write { sector, count } => {
                (BlockRequestHeader::write(sector), Some((count, false)))
            }
            Op::Flush => (BlockRequestHeader::flush(), None),
        };
        if let Some((count, _)) = data {
            let len = BlockRequestHeader::data_len_for_sectors(count)?;
            let end = header.sector.checked_add(u64::from(count));
            if count > MAX_SECTORS || len as usize > self.layout.data_len {
                return Err(Error::BlockDataLenOverflow);
            }
            if end.is_none_or(|end| end > self.capacity_sectors) {
                return Err(Error::CapacityOverflow);
            }
        }
        let base = mem.device_addr();
        let at = |offset: usize| base + offset as u64;
        mem.write(self.layout.header_offset, &header.encode());
        // A sentinel the device must overwrite: a missing status write is never "ok".
        mem.write(self.layout.status_offset, &[0xFF]);
        let status = SplitDescriptor::new(at(self.layout.status_offset), 1, 0, 0).writable();
        let header_desc = SplitDescriptor::new(at(self.layout.header_offset), 16, 0, 0);
        let desc = self.layout.queue.desc_offset as usize;
        match data {
            Some((count, device_writes)) => {
                let len = count * block::SECTOR_SIZE as u32;
                let mut data_desc = SplitDescriptor::new(at(self.layout.data_offset), len, 0, 0);
                if device_writes {
                    data_desc = data_desc.writable();
                }
                mem.write(desc, &header_desc.with_next(1).encode());
                mem.write(desc + 16, &data_desc.with_next(2).encode());
                mem.write(desc + 32, &status.encode());
            }
            None => {
                mem.write(desc, &header_desc.with_next(1).encode());
                mem.write(desc + 16, &status.encode());
            }
        }
        let avail = self.layout.queue.avail_offset as usize;
        let slot = (self.avail_idx % self.layout.queue.queue_size) as usize;
        mem.write(avail + 4 + slot * 2, &0u16.to_le_bytes());
        mem.barrier();
        self.avail_idx = self.avail_idx.wrapping_add(1);
        mem.write(avail + 2, &self.avail_idx.to_le_bytes());
        mem.barrier();
        self.in_flight = true;
        io.write32(mmio::QUEUE_NOTIFY, u32::from(self.queue.index));
        Ok(())
    }

    /// The request's outcome once the device has used it; `None` while it is still in flight.
    pub fn complete<M: DmaRegion>(&mut self, mem: &mut M) -> Option<Outcome> {
        if !self.in_flight {
            return None;
        }
        let used = self.layout.queue.used_offset as usize;
        let mut idx = [0u8; 2];
        mem.read(used + 2, &mut idx);
        if u16::from_le_bytes(idx) == self.used_seen {
            return None;
        }
        mem.barrier();
        self.used_seen = self.used_seen.wrapping_add(1);
        self.in_flight = false;
        let mut status = [0u8; 1];
        mem.read(self.layout.status_offset, &mut status);
        Some(match status[0] {
            block::STATUS_OK => Outcome::Ok,
            block::STATUS_IOERR => Outcome::IoError,
            block::STATUS_UNSUPP => Outcome::Unsupported,
            _ => Outcome::Malformed,
        })
    }

    /// Read the data buffer's first `len` bytes (after a completed `Read`).
    pub fn data<M: DmaRegion>(&self, mem: &mut M, out: &mut [u8]) {
        let len = out.len().min(self.layout.data_len);
        mem.read(self.layout.data_offset, &mut out[..len]);
    }

    /// Fill the data buffer (before a `Write`).
    pub fn set_data<M: DmaRegion>(&self, mem: &mut M, bytes: &[u8]) {
        mem.write(
            self.layout.data_offset,
            &bytes[..bytes.len().min(self.layout.data_len)],
        );
    }
}

/// Acknowledge the device's interrupt: every bit it raised.
pub fn acknowledge_interrupt<IO: RegisterIo>(io: &mut IO) -> u32 {
    let status = io.read32(mmio::INTERRUPT_STATUS);
    if status != 0 {
        io.write32(mmio::INTERRUPT_ACK, status);
    }
    status
}

/// The disk's capacity in sectors, from the block config space (read twice across a generation
/// change, as the spec asks for multi-dword fields).
pub fn read_capacity<IO: RegisterIo>(io: &mut IO) -> u64 {
    loop {
        let before = io.read32(mmio::CONFIG_GENERATION);
        let low = u64::from(io.read32(mmio::CONFIG));
        let high = u64::from(io.read32(mmio::CONFIG + 4));
        if io.read32(mmio::CONFIG_GENERATION) == before {
            return low | high << 32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        activate_block_queue, device_features, negotiate_writable_block, split_ring,
        SplitQueueMemory,
    };
    extern crate std;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::vec;
    use std::vec::Vec;

    const BASE: u64 = 0x4_0000_0000;
    const SECTORS: u64 = 64;

    /// The guest RAM the device sees: one region at [`BASE`].
    type Ram = Rc<RefCell<Vec<u8>>>;

    struct Region(Ram);

    impl DmaRegion for Region {
        fn device_addr(&self) -> u64 {
            BASE
        }
        fn write(&mut self, offset: usize, bytes: &[u8]) {
            self.0.borrow_mut()[offset..offset + bytes.len()].copy_from_slice(bytes);
        }
        fn read(&mut self, offset: usize, out: &mut [u8]) {
            out.copy_from_slice(&self.0.borrow()[offset..offset + out.len()]);
        }
        fn barrier(&mut self) {}
    }

    /// A modern virtio-mmio block device, as QEMU behaves: it processes the available ring on
    /// `QueueNotify`, writes the used ring and raises the vring interrupt.
    struct Device {
        ram: Ram,
        disk: Vec<u8>,
        flushed: Vec<u8>,
        features: u64,
        driver_features: [u32; 2],
        dev_sel: u32,
        drv_sel: u32,
        status: u32,
        queue_ready: u32,
        queue_num: u32,
        addrs: [u32; 6],
        last_avail: u16,
        used_idx: u16,
        interrupt: u32,
        notifies: u32,
        fail_next: bool,
    }

    impl Device {
        fn new(ram: Ram, flush: bool) -> Self {
            Self {
                ram,
                disk: vec![0; (SECTORS * 512) as usize],
                flushed: vec![0; (SECTORS * 512) as usize],
                features: device_features::VERSION_1 | if flush { block::FEATURE_FLUSH } else { 0 },
                driver_features: [0; 2],
                dev_sel: 0,
                drv_sel: 0,
                status: 0,
                queue_ready: 0,
                queue_num: 0,
                addrs: [0; 6],
                last_avail: 0,
                used_idx: 0,
                interrupt: 0,
                notifies: 0,
                fail_next: false,
            }
        }

        fn addr(&self, i: usize) -> usize {
            ((u64::from(self.addrs[i]) | u64::from(self.addrs[i + 1]) << 32) - BASE) as usize
        }

        fn rd16(&self, at: usize) -> u16 {
            let ram = self.ram.borrow();
            u16::from_le_bytes([ram[at], ram[at + 1]])
        }

        fn process(&mut self) {
            let (desc, avail, used) = (self.addr(0), self.addr(2), self.addr(4));
            let n = self.queue_num as u16;
            while self.last_avail != self.rd16(avail + 2) {
                let head = self.rd16(avail + 4 + (self.last_avail % n) as usize * 2);
                self.last_avail = self.last_avail.wrapping_add(1);
                let mut chain = Vec::new();
                let mut i = head;
                loop {
                    let ram = self.ram.borrow();
                    let d = desc + i as usize * 16;
                    let addr = u64::from_le_bytes(ram[d..d + 8].try_into().unwrap());
                    let len = u32::from_le_bytes(ram[d + 8..d + 12].try_into().unwrap());
                    let flags = u16::from_le_bytes([ram[d + 12], ram[d + 13]]);
                    let next = u16::from_le_bytes([ram[d + 14], ram[d + 15]]);
                    chain.push(((addr - BASE) as usize, len as usize, flags));
                    if flags & split_ring::DESC_F_NEXT == 0 {
                        break;
                    }
                    i = next;
                }
                let (h, _, _) = chain[0];
                let (kind, sector) = {
                    let ram = self.ram.borrow();
                    (
                        u32::from_le_bytes(ram[h..h + 4].try_into().unwrap()),
                        u64::from_le_bytes(ram[h + 8..h + 16].try_into().unwrap()),
                    )
                };
                let (st, _, _) = *chain.last().unwrap();
                let at = sector as usize * 512;
                let status = if self.fail_next {
                    self.fail_next = false;
                    block::STATUS_IOERR
                } else {
                    match kind {
                        block::T_IN => {
                            let (d, len, flags) = chain[1];
                            assert_ne!(flags & split_ring::DESC_F_WRITE, 0);
                            self.ram.borrow_mut()[d..d + len]
                                .copy_from_slice(&self.disk[at..at + len]);
                            block::STATUS_OK
                        }
                        block::T_OUT => {
                            let (d, len, flags) = chain[1];
                            assert_eq!(flags & split_ring::DESC_F_WRITE, 0);
                            self.disk[at..at + len].copy_from_slice(&self.ram.borrow()[d..d + len]);
                            block::STATUS_OK
                        }
                        block::T_FLUSH if self.features & block::FEATURE_FLUSH != 0 => {
                            self.flushed.clone_from(&self.disk);
                            block::STATUS_OK
                        }
                        _ => block::STATUS_UNSUPP,
                    }
                };
                self.ram.borrow_mut()[st] = status;
                let slot = used + 4 + (self.used_idx % n) as usize * 8;
                {
                    let mut ram = self.ram.borrow_mut();
                    ram[slot..slot + 4].copy_from_slice(&u32::from(head).to_le_bytes());
                    ram[slot + 4..slot + 8].copy_from_slice(&1u32.to_le_bytes());
                }
                self.used_idx = self.used_idx.wrapping_add(1);
                let idx = self.used_idx.to_le_bytes();
                self.ram.borrow_mut()[used + 2..used + 4].copy_from_slice(&idx);
                self.interrupt |= mmio::INTERRUPT_VRING;
            }
        }
    }

    impl RegisterIo for Device {
        fn read32(&mut self, offset: u32) -> u32 {
            match offset {
                mmio::MAGIC_VALUE => mmio::MAGIC,
                mmio::VERSION => mmio::VERSION_MODERN,
                mmio::DEVICE_ID => crate::ids::BLOCK,
                mmio::VENDOR_ID => 0x554d_4551,
                mmio::DEVICE_FEATURES => (self.features >> (32 * self.dev_sel)) as u32,
                mmio::QUEUE_NUM_MAX => 256,
                mmio::QUEUE_READY => self.queue_ready,
                mmio::INTERRUPT_STATUS => self.interrupt,
                mmio::STATUS => self.status,
                mmio::CONFIG_GENERATION => 0,
                mmio::CONFIG => SECTORS as u32,
                o if o == mmio::CONFIG + 4 => 0,
                _ => 0,
            }
        }

        fn write32(&mut self, offset: u32, value: u32) {
            match offset {
                mmio::DEVICE_FEATURES_SEL => self.dev_sel = value,
                mmio::DRIVER_FEATURES_SEL => self.drv_sel = value,
                mmio::DRIVER_FEATURES => self.driver_features[self.drv_sel as usize] = value,
                mmio::STATUS => self.status = value,
                mmio::QUEUE_NUM => self.queue_num = value,
                mmio::QUEUE_READY => self.queue_ready = value,
                mmio::QUEUE_DESC_LOW => self.addrs[0] = value,
                mmio::QUEUE_DESC_HIGH => self.addrs[1] = value,
                mmio::QUEUE_AVAIL_LOW => self.addrs[2] = value,
                mmio::QUEUE_AVAIL_HIGH => self.addrs[3] = value,
                mmio::QUEUE_USED_LOW => self.addrs[4] = value,
                mmio::QUEUE_USED_HIGH => self.addrs[5] = value,
                mmio::QUEUE_NOTIFY => {
                    self.notifies += 1;
                    self.process();
                }
                mmio::INTERRUPT_ACK => self.interrupt &= !value,
                _ => {}
            }
        }
    }

    fn live(flush: bool) -> (Device, Region, BlockQueue) {
        let layout = RegionLayout::new().unwrap();
        let ram: Ram = Rc::new(RefCell::new(vec![0; layout.total_len]));
        let mut dev = Device::new(ram.clone(), flush);
        let negotiated = negotiate_writable_block(&mut dev).unwrap();
        assert_eq!(negotiated.supports_flush(), flush);
        let active = activate_block_queue(
            &mut dev,
            negotiated,
            0,
            SplitQueueMemory {
                device_addr: BASE,
                len: layout.header_offset as u64,
                preferred_size: QUEUE_SIZE,
            },
        )
        .unwrap();
        let capacity = read_capacity(&mut dev);
        (dev, Region(ram), BlockQueue::new(active, layout, capacity))
    }

    fn run(q: &mut BlockQueue, dev: &mut Device, mem: &mut Region, op: Op) -> Outcome {
        q.submit(mem, dev, op).unwrap();
        assert_eq!(acknowledge_interrupt(dev), mmio::INTERRUPT_VRING);
        assert_eq!(acknowledge_interrupt(dev), 0, "acknowledged");
        let outcome = q.complete(mem).unwrap();
        assert_eq!(q.complete(mem), None, "one completion per request");
        outcome
    }

    #[test]
    fn writes_reads_and_flushes_reach_the_disk_in_order() {
        let (mut dev, mut mem, mut q) = live(true);
        assert_eq!(q.capacity_sectors, SECTORS);
        let data: Vec<u8> = (0..1024).map(|i| (i * 7 % 251) as u8).collect();
        q.set_data(&mut mem, &data);
        assert_eq!(
            run(
                &mut q,
                &mut dev,
                &mut mem,
                Op::Write {
                    sector: 5,
                    count: 2
                }
            ),
            Outcome::Ok
        );
        assert_eq!(&dev.disk[5 * 512..7 * 512], &data[..]);
        assert!(
            dev.flushed.iter().all(|b| *b == 0),
            "not stable before the flush"
        );
        assert_eq!(run(&mut q, &mut dev, &mut mem, Op::Flush), Outcome::Ok);
        assert_eq!(&dev.flushed[5 * 512..7 * 512], &data[..]);
        q.set_data(&mut mem, &[0u8; 1024]);
        assert_eq!(
            run(
                &mut q,
                &mut dev,
                &mut mem,
                Op::Read {
                    sector: 5,
                    count: 2
                }
            ),
            Outcome::Ok
        );
        let mut back = [0u8; 1024];
        q.data(&mut mem, &mut back);
        assert_eq!(&back[..], &data[..]);
        // The rings wrap: many more requests than queue entries.
        for i in 0..40u64 {
            q.set_data(&mut mem, &[i as u8; 512]);
            assert_eq!(
                run(
                    &mut q,
                    &mut dev,
                    &mut mem,
                    Op::Write {
                        sector: i % SECTORS,
                        count: 1
                    }
                ),
                Outcome::Ok
            );
        }
        assert_eq!(dev.notifies, 43);
    }

    #[test]
    fn bad_requests_never_reach_the_device_and_failures_are_reported() {
        let (mut dev, mut mem, mut q) = live(false);
        for op in [
            Op::Read {
                sector: SECTORS,
                count: 1,
            },
            Op::Write {
                sector: SECTORS - 1,
                count: 2,
            },
            Op::Read {
                sector: 0,
                count: MAX_SECTORS + 1,
            },
            Op::Read {
                sector: u64::MAX,
                count: 1,
            },
        ] {
            assert!(q.submit(&mut mem, &mut dev, op).is_err(), "{op:?}");
        }
        assert_eq!(dev.notifies, 0);
        assert_eq!(
            run(&mut q, &mut dev, &mut mem, Op::Flush),
            Outcome::Unsupported
        );
        dev.fail_next = true;
        assert_eq!(
            run(
                &mut q,
                &mut dev,
                &mut mem,
                Op::Read {
                    sector: 0,
                    count: 1
                }
            ),
            Outcome::IoError
        );
        // A busy queue refuses a second request until the first completes.
        q.submit(
            &mut mem,
            &mut dev,
            Op::Read {
                sector: 0,
                count: 1,
            },
        )
        .unwrap();
        assert_eq!(
            q.submit(
                &mut mem,
                &mut dev,
                Op::Read {
                    sector: 0,
                    count: 1
                }
            ),
            Err(Error::QueueUnavailable)
        );
        assert_eq!(q.complete(&mut mem), Some(Outcome::Ok));
    }

    #[test]
    fn a_status_byte_the_device_never_wrote_is_not_success() {
        let (mut dev, mut mem, mut q) = live(true);
        q.submit(
            &mut mem,
            &mut dev,
            Op::Read {
                sector: 0,
                count: 1,
            },
        )
        .unwrap();
        // Forge a used index with the sentinel left in place.
        mem.write(q.layout().status_offset, &[0xFF]);
        assert_eq!(q.complete(&mut mem), Some(Outcome::Malformed));
    }
}
