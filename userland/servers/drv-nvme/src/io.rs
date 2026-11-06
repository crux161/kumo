//j801
//j802
//j803
//j805
//! NV5: one polled I/O queue pair and bounded, read-only media access.
//! Set Features (Number of Queues) asks for one SQ/CQ; Create I/O CQ precedes
//! Create I/O SQ, and teardown deletes the SQ before its CQ. Reads are one PRP
//! page at most, which is inside MDTS for every value (MPSMIN is 4 KiB). Any LBA
//! range that does not fit the namespace is refused before the SQ slot, the data
//! page or a doorbell is touched. NV8 adds Write/Flush encoding; no Format command.
//! D5 cooperative raw DMA; Road C stays open. — TIMBERDOODLE

use crate::admin::{AdminIo, Completion, Namespace, Queue, DATA};
use crate::{Error, Memory, Startup, DEPTH, PAGE};

pub const IO_QID: u16 = 1;
pub const IO_SQ: u64 = 3 * PAGE;
pub const IO_CQ: u64 = 4 * PAGE;

/// One PRP1-only transfer: dword aligned, below 4 GiB, not crossing a page.
fn prp(physical: u64, bytes: u64) -> Result<u64, Error> {
    if physical == 0
        || physical & 3 != 0
        || physical.checked_add(bytes).is_none_or(|end| end > 1 << 32)
        || (physical & (PAGE - 1)) + bytes > PAGE
    {
        return Err(Error::Memory);
    }
    Ok(physical)
}
fn page(physical: u64) -> Result<u64, Error> {
    if physical == 0 || physical & (PAGE - 1) != 0 || physical >= 1 << 32 {
        return Err(Error::Memory);
    }
    Ok(physical)
}
fn admin(opcode: u32, prp1: u64, cdw10: u32, cdw11: u32) -> [u32; 16] {
    let mut w = [0; 16];
    w[0] = opcode;
    w[6] = prp1 as u32;
    w[7] = (prp1 >> 32) as u32;
    w[10] = cdw10;
    w[11] = cdw11;
    w
}
/// Set Features, FID 07h: request one I/O SQ and one I/O CQ (both zero-based 0).
pub fn set_queue_count() -> [u32; 16] {
    admin(0x09, 0, 0x07, 0)
}
/// Create I/O CQ: physically contiguous, vector 0 (IV=0). NV6 sets IEN for pin
/// interrupts; NV5's polled pair leaves it clear.
pub fn create_cq(physical: u64, interrupts: bool) -> Result<[u32; 16], Error> {
    let cq = page(physical.checked_add(IO_CQ).ok_or(Error::Memory)?)?;
    Ok(admin(
        0x05,
        cq,
        (u32::from(DEPTH - 1) << 16) | u32::from(IO_QID),
        1 | (u32::from(interrupts) << 1),
    ))
}
/// Create I/O SQ bound to CQ 1: physically contiguous, QPRIO 0, NVM set 0.
pub fn create_sq(physical: u64) -> Result<[u32; 16], Error> {
    let sq = page(physical.checked_add(IO_SQ).ok_or(Error::Memory)?)?;
    Ok(admin(
        0x01,
        sq,
        (u32::from(DEPTH - 1) << 16) | u32::from(IO_QID),
        (u32::from(IO_QID) << 16) | 1,
    ))
}
pub fn delete_sq() -> [u32; 16] {
    admin(0x00, 0, u32::from(IO_QID), 0)
}
pub fn delete_cq() -> [u32; 16] {
    admin(0x04, 0, u32::from(IO_QID), 0)
}

/// NVM Read, PRP1 only. Refuses (Error::Range) a namespace with metadata or
/// protection, zero blocks, more than one page, or any LBA range that does not
/// end at or below NSZE, including overflow. PRP1 may sit inside the data page
/// (NV6 batches) but must be dword aligned and not cross a page. — TIMBERDOODLE
pub fn read_command(
    ns: &Namespace,
    lba: u64,
    blocks: u16,
    physical: u64,
) -> Result<[u32; 16], Error> {
    if !ns.plain_blocks() || blocks == 0 {
        return Err(Error::Range);
    }
    if u64::from(blocks) * u64::from(ns.lba_bytes) > PAGE {
        return Err(Error::Range);
    }
    if !lba
        .checked_add(u64::from(blocks))
        .is_some_and(|end| end <= ns.size)
    {
        return Err(Error::Range);
    }
    let data = prp(physical, u64::from(blocks) * u64::from(ns.lba_bytes))?;
    let mut w = [0; 16];
    w[0] = 0x02;
    w[1] = ns.id;
    w[6] = data as u32;
    w[7] = (data >> 32) as u32;
    w[10] = lba as u32;
    w[11] = (lba >> 32) as u32;
    w[12] = u32::from(blocks - 1);
    Ok(w)
}

/// NVM Write: same PRP/range bounds as Read, FUA/LR/PRINFO clear. The caller
/// separately authorizes the writable extent before invoking this. — TIMBERDOODLE
pub fn write_command(
    ns: &Namespace,
    lba: u64,
    blocks: u16,
    physical: u64,
) -> Result<[u32; 16], Error> {
    if matches!(ns.id, 0 | u32::MAX) {
        return Err(Error::Range);
    }
    let mut words = read_command(ns, lba, blocks, physical)?;
    words[0] = 0x01;
    Ok(words)
}
/// Flush one namespace, no data pointer or command-specific fields.
pub fn flush_command(ns: &Namespace) -> Result<[u32; 16], Error> {
    if matches!(ns.id, 0 | u32::MAX) || !ns.plain_blocks() {
        return Err(Error::Range);
    }
    let mut w = [0; 16];
    w[1] = ns.id;
    Ok(w)
}

/// Allocation from Set Features: (I/O SQs, I/O CQs), counted from one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Allocated {
    pub submission: u32,
    pub completion: u32,
}

pub struct IoPair {
    pub(crate) queue: Queue,
    pub(crate) ns: Namespace,
    pub allocated: Allocated,
}
impl IoPair {
    /// Requires the I/O SQ/CQ pages zeroed and an idle, ready admin queue.
    pub fn open(
        admin: &mut Queue,
        io: &mut impl AdminIo,
        s: Startup,
        memory: Memory,
        ns: Namespace,
        interrupts: bool,
    ) -> Result<Self, Error> {
        if !ns.plain_blocks() {
            return Err(Error::Data);
        }
        let cq = create_cq(memory.physical, interrupts)?;
        let sq = create_sq(memory.physical)?;
        let c = admin.submit(io, s, memory, set_queue_count(), &mut [])?;
        let allocated = Allocated {
            submission: (c.result & 0xffff) + 1,
            completion: (c.result >> 16) + 1,
        };
        if allocated.submission > 0xffff || allocated.completion > 0xffff {
            return Err(Error::Data);
        }
        admin.submit(io, s, memory, cq, &mut [])?;
        admin.submit(io, s, memory, sq, &mut [])?;
        Ok(Self {
            queue: Queue::at(IO_QID, IO_SQ, IO_CQ),
            ns,
            allocated,
        })
    }
    /// `output` must be exactly `blocks` logical blocks. A Range refusal leaves
    /// the queue usable: nothing was written or rung. — TIMBERDOODLE
    pub fn read(
        &mut self,
        io: &mut impl AdminIo,
        s: Startup,
        memory: Memory,
        lba: u64,
        blocks: u16,
        output: &mut [u8],
    ) -> Result<Completion, Error> {
        let words = read_command(&self.ns, lba, blocks, memory.physical.wrapping_add(DATA))?;
        if output.len() as u64 != u64::from(blocks) * u64::from(self.ns.lba_bytes) {
            return Err(Error::Range);
        }
        self.queue.submit(io, s, memory, words, output)
    }
    /// Delete SQ 1, then CQ 1. A poisoned I/O queue, or one with a command still
    /// outstanding, may still own DMA, so it is never deleted; the caller keeps every
    /// buffer instead.
    pub fn close(
        self,
        admin: &mut Queue,
        io: &mut impl AdminIo,
        s: Startup,
        memory: Memory,
    ) -> Result<(), Error> {
        if self.queue.poisoned() || self.queue.outstanding() != 0 {
            return Err(Error::State);
        }
        admin.submit(io, s, memory, delete_sq(), &mut [])?;
        admin.submit(io, s, memory, delete_cq(), &mut [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::admin::{cq_head, sq_tail};
    use crate::{RegisterIo, CSTS};
    use std::vec::Vec;

    fn startup() -> Startup {
        Startup {
            session: 1,
            bar: 0xf000_0000,
            cap: 0x303c033fff,
            vs: 0x10300,
            id: 0xa808144d,
            class_revision: 0x01080200,
            root_command: 7,
            endpoint_command: 6,
            msix_control: 32,
            irq: 292,
            pin: 1,
            legacy_mask: 0,
            mode: 0,
        }
    }
    fn memory() -> Memory {
        Memory {
            session: 1,
            physical: 0x6000_0000,
        }
    }
    fn namespace() -> Namespace {
        Namespace {
            id: 1,
            size: 976773168,
            capacity: 976773168,
            used: 393584640,
            lba_bytes: 512,
            bytes: 500107862016,
            metadata: 0,
            protection: 0,
            format: 0,
        }
    }
    fn block(lba: u64, i: usize) -> u8 {
        (lba.wrapping_mul(31) as usize + i * 7) as u8
    }

    /// A controller model with one admin pair and at most one I/O pair, both
    /// polled. It checks every command word the driver is allowed to send.
    struct Fake {
        dma: Vec<u32>,
        now: u64,
        tails: [u16; 2],
        heads: [u16; 2],
        phases: [u32; 2],
        cq: bool,
        sq: bool,
        features: bool,
        barrier: bool,
        acquired: bool,
        doorbells: Vec<(u16, u32)>,
        dma_writes: usize,
        admin: Vec<u32>,
        reads: Vec<(u64, u32)>,
        fail_opcode: Option<u32>,
        wrong_sqid: bool,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                dma: std::vec![0; (crate::DMA_BYTES / 4) as usize],
                now: 0,
                tails: [0; 2],
                heads: [0; 2],
                phases: [1; 2],
                cq: false,
                sq: false,
                features: false,
                barrier: false,
                acquired: false,
                doorbells: Vec::new(),
                dma_writes: 0,
                admin: Vec::new(),
                reads: Vec::new(),
                fail_opcode: None,
                wrong_sqid: false,
            }
        }
        fn complete(&mut self, q: usize, cid: u32, status: u32, result: u32) {
            let base = [PAGE, IO_CQ][q] as usize / 4 + self.heads[q] as usize * 4;
            let sqid = if self.wrong_sqid { 0 } else { q as u32 };
            self.dma[base] = result;
            self.dma[base + 2] = u32::from(self.tails[q]) | (sqid << 16);
            self.dma[base + 3] = cid | (self.phases[q] << 16) | (status << 17);
        }
        fn admin_command(&mut self, w: [u32; 16]) {
            let op = w[0] & 0xff;
            let cid = w[0] >> 16;
            self.admin.push(op);
            assert_eq!(w[1..6], [0; 5]);
            assert_eq!(w[8..10], [0; 2]);
            assert_eq!(w[12..], [0; 4]);
            let mut result = 0;
            match op {
                0x09 => {
                    assert!(!self.cq && !self.sq, "queue count after creation");
                    assert_eq!((w[6], w[7], w[10], w[11]), (0, 0, 7, 0));
                    self.features = true;
                    result = 0x0003_0003;
                }
                0x05 => {
                    assert!(self.features && !self.cq);
                    let cq = memory().physical + IO_CQ;
                    assert_eq!((w[6], w[7]), (cq as u32, 0));
                    assert_eq!((w[10], w[11]), (0x001f_0001, 1));
                    self.cq = true;
                }
                0x01 => {
                    assert!(self.cq && !self.sq, "SQ before its CQ");
                    let sq = memory().physical + IO_SQ;
                    assert_eq!((w[6], w[7]), (sq as u32, 0));
                    assert_eq!((w[10], w[11]), (0x001f_0001, 0x0001_0001));
                    self.sq = true;
                }
                0x00 => {
                    assert!(self.sq);
                    assert_eq!((w[6], w[7], w[10], w[11]), (0, 0, 1, 0));
                    self.sq = false;
                }
                0x04 => {
                    assert!(self.cq && !self.sq, "CQ deleted before its SQ");
                    assert_eq!((w[6], w[7], w[10], w[11]), (0, 0, 1, 0));
                    self.cq = false;
                }
                _ => panic!("unplanned admin opcode {op:#x}"),
            }
            let status = if self.fail_opcode == Some(op) {
                0x101
            } else {
                0
            };
            self.complete(0, cid, status, result);
        }
        fn read_command(&mut self, w: [u32; 16]) {
            assert!(self.sq && self.cq);
            assert_eq!(w[0] & 0xffff, 2);
            assert_eq!(w[1], 1);
            assert_eq!((w[6], w[7]), ((memory().physical + DATA) as u32, 0));
            assert_eq!(w[2..6], [0; 4]);
            assert_eq!(w[8..10], [0; 2]);
            assert_eq!(w[13..], [0; 3]);
            assert!(w[12] >> 16 == 0, "FUA/LR/PRINFO must stay clear");
            let lba = u64::from(w[10]) | (u64::from(w[11]) << 32);
            let blocks = (w[12] & 0xffff) + 1;
            assert!(
                lba + u64::from(blocks) <= namespace().size,
                "out of range read sent"
            );
            assert!(self.dma[DATA as usize / 4..][..1024]
                .iter()
                .all(|&v| v == 0));
            for i in 0..(blocks as usize * 512) {
                let word = &mut self.dma[DATA as usize / 4 + i / 4];
                *word |= u32::from(block(lba, i)) << ((i % 4) * 8);
            }
            self.reads.push((lba, blocks));
            self.complete(1, w[0] >> 16, 0, 0);
        }
    }
    impl RegisterIo for Fake {
        fn read32(&mut self, o: u16) -> u32 {
            assert_eq!(o, CSTS);
            1
        }
        fn write32(&mut self, o: u16, v: u32) {
            assert!(self.barrier, "doorbell without a preceding barrier");
            self.barrier = false;
            self.doorbells.push((o, v));
            let (q, tail) = match o {
                0x1000 => (0, true),
                0x1004 => (0, false),
                0x1008 => (1, true),
                0x100c => (1, false),
                _ => panic!("unplanned register write {o:#x}"),
            };
            if tail {
                let base = [0, IO_SQ][q] as usize / 4 + self.tails[q] as usize * 16;
                let mut w = [0; 16];
                w.copy_from_slice(&self.dma[base..base + 16]);
                self.tails[q] = v as u16;
                self.acquired = false;
                if q == 0 {
                    self.admin_command(w);
                } else {
                    self.read_command(w);
                }
            } else {
                assert!(self.acquired);
                self.heads[q] = v as u16;
                if v == 0 {
                    self.phases[q] ^= 1;
                }
            }
        }
        fn barrier(&mut self) {
            self.barrier = true;
        }
        fn now_ns(&mut self) -> u64 {
            self.now
        }
        fn sleep_ns(&mut self, n: u64) -> bool {
            self.now += n;
            true
        }
    }
    impl AdminIo for Fake {
        fn dma_read32(&mut self, off: u64) -> u32 {
            let cq_tag = (off >= PAGE && off < DATA || off >= IO_CQ) && off % 16 == 12;
            if !cq_tag {
                assert!(self.acquired, "data/CQE read before phase acquire");
            }
            self.dma[off as usize / 4]
        }
        fn dma_write32(&mut self, off: u64, value: u32) {
            assert!(
                off < PAGE || (DATA..IO_CQ).contains(&off),
                "host must not write a CQ"
            );
            self.barrier = false;
            self.dma_writes += 1;
            self.dma[off as usize / 4] = value;
        }
        fn dma_acquire(&mut self) {
            self.acquired = true;
        }
    }

    #[test]
    fn doorbells_follow_the_legacy_stride() {
        assert_eq!((sq_tail(0), cq_head(0)), (0x1000, 0x1004));
        assert_eq!((sq_tail(1), cq_head(1)), (0x1008, 0x100c));
    }

    #[test]
    fn pair_lifecycle_reads_exact_blocks_and_refuses_nsze_without_any_write() {
        let mut io = Fake::new();
        let mut admin = Queue::new();
        let ns = namespace();
        let mut pair = IoPair::open(&mut admin, &mut io, startup(), memory(), ns, false).unwrap();
        assert_eq!(
            pair.allocated,
            Allocated {
                submission: 4,
                completion: 4
            }
        );
        let mut out = [0u8; 512];
        for lba in [0, 1] {
            let c = pair
                .read(&mut io, startup(), memory(), lba, 1, &mut out)
                .unwrap();
            assert_eq!(c.status & !1, 0);
            assert!(out.iter().enumerate().all(|(i, &b)| b == block(lba, i)));
        }
        let (doorbells, writes) = (io.doorbells.len(), io.dma_writes);
        for (lba, blocks) in [
            (ns.size, 1),
            (ns.size - 1, 2),
            (u64::MAX, 1),
            (0, 0),
            (0, 9),
        ] {
            assert_eq!(
                pair.read(&mut io, startup(), memory(), lba, blocks, &mut out),
                Err(Error::Range)
            );
        }
        assert_eq!((io.doorbells.len(), io.dma_writes), (doorbells, writes));
        let c = pair
            .read(&mut io, startup(), memory(), ns.size - 1, 1, &mut out)
            .unwrap();
        assert_eq!(c.cid, 3, "refusals consumed no command identifier");
        assert!(out
            .iter()
            .enumerate()
            .all(|(i, &b)| b == block(ns.size - 1, i)));
        pair.close(&mut admin, &mut io, startup(), memory())
            .unwrap();
        assert_eq!(io.admin, [0x09, 0x05, 0x01, 0x00, 0x04]);
        assert_eq!(io.reads, [(0, 1), (1, 1), (ns.size - 1, 1)]);
        assert!(!io.sq && !io.cq);
        let rung: Vec<u16> = io.doorbells.iter().map(|&(o, _)| o).collect();
        assert_eq!(rung.iter().filter(|&&o| o == 0x1008).count(), 3);
        assert_eq!(rung.iter().filter(|&&o| o == 0x100c).count(), 3);
    }

    #[test]
    fn read_encoding_splits_slba_and_zero_bases_nlb() {
        let mut ns = namespace();
        ns.size = 1 << 40;
        let w = read_command(&ns, 0x1_2345_6789, 8, 0x1234_5000).unwrap();
        let mut want = [0; 16];
        want[0] = 2;
        want[1] = 1;
        want[6] = 0x1234_5000;
        want[10] = 0x2345_6789;
        want[11] = 1;
        want[12] = 7;
        assert_eq!(w, want);
        assert_eq!(read_command(&ns, 0, 1, 0x1_0000_0000), Err(Error::Memory));
        assert_eq!(
            read_command(&ns, 0, 1, 0xffff_fe00).map(|w| w[6]),
            Ok(0xffff_fe00)
        );
        assert_eq!(read_command(&ns, 0, 2, 0xffff_fe00), Err(Error::Memory));
        assert_eq!(read_command(&ns, 0, 1, 0x1234_5202), Err(Error::Memory));
        assert_eq!(read_command(&ns, 0, 1, 0x1234_5e04), Err(Error::Memory));
        assert_eq!(
            read_command(&ns, 0, 1, 0x1234_5200).map(|w| w[6]),
            Ok(0x1234_5200)
        );
        assert_eq!(create_cq(0x6000_0000, true).map(|w| w[11]), Ok(3));
        assert_eq!(create_cq(0x6000_0000, false).map(|w| w[11]), Ok(1));
        ns.lba_bytes = 4096;
        assert!(read_command(&ns, 0, 1, 0x1000).is_ok());
        assert_eq!(read_command(&ns, 0, 2, 0x1000), Err(Error::Range));
        let mut meta = namespace();
        meta.metadata = 8;
        assert_eq!(read_command(&meta, 0, 1, 0x1000), Err(Error::Range));
        let mut pi = namespace();
        pi.protection = 1;
        assert_eq!(read_command(&pi, 0, 1, 0x1000), Err(Error::Range));
    }

    #[test]
    fn non_plain_namespace_is_refused_before_any_admin_command() {
        let mut io = Fake::new();
        let mut ns = namespace();
        ns.metadata = 8;
        let mut admin = Queue::new();
        assert_eq!(
            IoPair::open(&mut admin, &mut io, startup(), memory(), ns, false).err(),
            Some(Error::Data)
        );
        assert!(io.doorbells.is_empty() && io.dma_writes == 0);
    }

    #[test]
    fn failed_create_poisons_admin_and_never_reaches_io_queue() {
        for op in [0x09, 0x05, 0x01] {
            let mut io = Fake::new();
            io.fail_opcode = Some(op);
            let mut admin = Queue::new();
            let e =
                IoPair::open(&mut admin, &mut io, startup(), memory(), namespace(), false).err();
            assert_eq!(e, Some(Error::Command));
            assert!(admin.poisoned());
            assert!(io.doorbells.iter().all(|&(o, _)| o < 0x1008));
            assert_eq!(
                io.doorbells.last().map(|d| d.0),
                Some(0x1000),
                "failed CQE never released"
            );
            assert_eq!(*io.admin.last().unwrap(), op);
        }
    }

    #[test]
    fn io_completion_naming_the_wrong_queue_poisons_and_blocks_deletion() {
        let mut io = Fake::new();
        let mut admin = Queue::new();
        let mut pair =
            IoPair::open(&mut admin, &mut io, startup(), memory(), namespace(), false).unwrap();
        io.wrong_sqid = true;
        let mut out = [0u8; 512];
        assert_eq!(
            pair.read(&mut io, startup(), memory(), 0, 1, &mut out),
            Err(Error::Completion)
        );
        assert!(!io.doorbells.iter().any(|&(o, _)| o == 0x100c));
        let before = io.doorbells.len();
        assert_eq!(
            pair.close(&mut admin, &mut io, startup(), memory()),
            Err(Error::State)
        );
        assert_eq!(
            io.doorbells.len(),
            before,
            "a poisoned queue is never deleted"
        );
        assert!(io.sq && io.cq);
    }

    #[test]
    fn io_phase_wraps_twice_with_cq1_head_doorbells() {
        let mut io = Fake::new();
        let mut admin = Queue::new();
        let mut pair =
            IoPair::open(&mut admin, &mut io, startup(), memory(), namespace(), false).unwrap();
        let mut out = [0u8; 512];
        for n in 0..u64::from(DEPTH) * 2 + 3 {
            pair.read(&mut io, startup(), memory(), n, 1, &mut out)
                .unwrap();
            assert!(out.iter().enumerate().all(|(i, &b)| b == block(n, i)));
            assert_eq!(
                io.doorbells.last(),
                Some(&(0x100c, ((n + 1) % u64::from(DEPTH)) as u32))
            );
        }
        pair.close(&mut admin, &mut io, startup(), memory())
            .unwrap();
    }
}
