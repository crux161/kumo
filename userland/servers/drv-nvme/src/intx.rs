//j802
//j803
//! NV6: INTx-driven completions for the I/O pair (I/O CQ 1 created with IEN=1, IV=0).
//! KUMO's DESIGN/016 contract holds unchanged: the kernel masks the level SPI when it
//! fires and unmasks it only on InterruptComplete. A delivery is serviced by reaping
//! every posted CQE, consuming the data, writing the CQ head doorbell (which is what
//! lets the controller deassert INTA), reading CSTS to push that posted write to the
//! device, and only then completing the interrupt. A delivery that reaps nothing is
//! stray or late; more than STORM_LIMIT of them inside one operation is a storm and
//! fails it with the line left masked by the kernel. Vector 0 is gated by INTMS/INTMC
//! (pin-based, MSI-X disabled); admin commands only run while it is masked. No root,
//! client or config write: the client INTA mask was inherited open. — TIMBERDOODLE

use crate::admin::{AdminIo, Entry, Queue, COMMAND_TIMEOUT_NS, DATA};
use crate::io::{read_command, IoPair};
use crate::{start, Error, Memory, Startup, CSTS, PAGE};

pub const INTMS: u16 = 0x0c;
pub const INTMC: u16 = 0x10;
/// Deliveries that reap nothing, per operation, before the line counts as a storm.
pub const STORM_LIMIT: u32 = 8;
/// One wait slice; a longer silence is re-checked against the command deadline.
pub const SLICE_NS: u64 = 1_000_000_000;
/// Window in which no further delivery is expected.
pub const QUIET_NS: u64 = 100_000_000;
/// Poll period for a completion that must arrive while vector 0 is masked.
pub const MASKED_POLL_NS: u64 = 10_000_000;
/// Reads per interrupt-driven batch.
pub const BATCH: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wake {
    Interrupt,
    Timeout,
}

/// The bound interrupt as the driver sees it. `wait` returns Interrupt only for a
/// delivery the kernel recorded (its count consumed); the line is then masked until
/// `complete`.
pub trait Line {
    fn wait(&mut self, ns: u64) -> Result<Wake, Error>;
    fn complete(&mut self) -> Result<(), Error>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    /// Deliveries serviced (each completed exactly once).
    pub deliveries: u32,
    /// Deliveries that reaped no entry (stray, late or masked).
    pub empty: u32,
    /// Entries reaped by deliveries.
    pub entries: u32,
    /// Most entries reaped by one delivery (>1 means completions were coalesced).
    pub max_batch: u32,
}

/// Vector 0 masked (INTMS), read back.
pub fn mask(io: &mut impl AdminIo) -> Result<(), Error> {
    io.write32(INTMS, 1);
    io.barrier();
    if io.read32(INTMS) & 1 == 0 {
        return Err(Error::Programming);
    }
    Ok(())
}
/// Vector 0 unmasked (INTMC), read back through INTMS.
pub fn unmask(io: &mut impl AdminIo) -> Result<(), Error> {
    io.write32(INTMC, 1);
    io.barrier();
    if io.read32(INTMS) & 1 != 0 {
        return Err(Error::Programming);
    }
    Ok(())
}

/// Wait for deliveries until `left` entries are reaped or the command deadline from
/// `begin` passes. `on_entry` validates and consumes each entry before the head is
/// released. A deadline with an entry posted but never delivered is Error::Interrupt.
fn service<IO: AdminIo>(
    queue: &mut Queue,
    io: &mut IO,
    line: &mut impl Line,
    begin: u64,
    mut left: u32,
    stats: &mut Stats,
    mut on_entry: impl FnMut(&mut IO, Entry) -> Result<(), Error>,
) -> Result<(), Error> {
    let deadline = begin.checked_add(COMMAND_TIMEOUT_NS).ok_or(Error::Clock)?;
    let mut empty = 0;
    while left > 0 {
        let now = start(io)?;
        if now < begin {
            return Err(Error::Clock);
        }
        if now >= deadline {
            return Err(if queue.ready(io)? {
                Error::Interrupt
            } else {
                Error::Timeout
            });
        }
        if line.wait(SLICE_NS.min(deadline - now))? == Wake::Timeout {
            continue;
        }
        stats.deliveries += 1;
        let mut batch = 0;
        while let Some(entry) = queue.take(io)? {
            if batch == left {
                return Err(Error::Completion);
            }
            on_entry(io, entry)?;
            batch += 1;
        }
        if batch == 0 {
            stats.empty += 1;
            empty += 1;
            if empty > STORM_LIMIT {
                return Err(Error::Storm);
            }
        } else {
            queue.release(io)?;
            if io.read32(CSTS) & 3 != 1 {
                return Err(Error::Fatal);
            }
            left -= batch;
            stats.entries += batch;
            stats.max_batch = stats.max_batch.max(batch);
        }
        line.complete()?;
    }
    Ok(())
}

/// Watch for `ns` with nothing outstanding. Every delivery in the window is late or
/// stray: it must reap nothing, and is completed. Returns how many arrived.
pub fn quiet(
    queue: &mut Queue,
    io: &mut impl AdminIo,
    line: &mut impl Line,
    ns: u64,
    stats: &mut Stats,
) -> Result<u32, Error> {
    let begin = start(io)?;
    let end = begin.checked_add(ns).ok_or(Error::Clock)?;
    let mut late = 0;
    loop {
        let now = start(io)?;
        if now < begin {
            return Err(Error::Clock);
        }
        if now >= end || line.wait(end - now)? == Wake::Timeout {
            return Ok(late);
        }
        late += 1;
        stats.deliveries += 1;
        stats.empty += 1;
        if queue.take(io)?.is_some() {
            return Err(Error::Completion);
        }
        if late > STORM_LIMIT {
            return Err(Error::Storm);
        }
        line.complete()?;
    }
}

fn copy_slice(io: &mut impl AdminIo, index: usize, out: &mut [u8]) {
    let base = DATA + (index * out.len()) as u64;
    for (i, chunk) in out.chunks_exact_mut(4).enumerate() {
        chunk.copy_from_slice(&io.dma_read32(base + i as u64 * 4).to_le_bytes());
    }
}

/// A delivery while vector 0 is masked reaps nothing (the entry is left for the
/// unmask proof) and is completed; past STORM_LIMIT it is a storm.
fn masked_delivery(
    line: &mut impl Line,
    proof: &mut Masked,
    stats: &mut Stats,
) -> Result<(), Error> {
    proof.deliveries += 1;
    stats.deliveries += 1;
    stats.empty += 1;
    if proof.deliveries > STORM_LIMIT {
        return Err(Error::Storm);
    }
    line.complete()
}

/// What the masked-vector proof observed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Masked {
    /// Deliveries while vector 0 was masked (a compliant controller gives none).
    pub deliveries: u32,
    /// Ring to the completion being visible in the CQ with no delivery.
    pub posted_ns: u64,
    /// INTMC to the entry being reaped by a delivery.
    pub unmask_ns: u64,
}

impl IoPair {
    /// Stage one read per LBA into consecutive slices of the data page, ring once,
    /// then reap them from deliveries. `out` holds the blocks in `lbas` order. Every
    /// range is refused before any slice, SQ entry or doorbell is written.
    pub fn read_irq(
        &mut self,
        io: &mut impl AdminIo,
        line: &mut impl Line,
        s: Startup,
        memory: Memory,
        lbas: &[u64],
        out: &mut [u8],
        stats: &mut Stats,
    ) -> Result<u64, Error> {
        let size = self.ns.lba_bytes as usize;
        if lbas.is_empty() || lbas.len() > BATCH || out.len() != lbas.len() * size {
            return Err(Error::Range);
        }
        if (lbas.len() * size) as u64 > PAGE {
            return Err(Error::Range);
        }
        let mut words = [[0u32; 16]; BATCH];
        for (i, &lba) in lbas.iter().enumerate() {
            let prp = memory.physical.wrapping_add(DATA + (i * size) as u64);
            words[i] = read_command(&self.ns, lba, 1, prp)?;
        }
        if self.queue.outstanding() != 0 {
            return Err(Error::State);
        }
        for off in (DATA..DATA + out.len() as u64).step_by(4) {
            io.dma_write32(off, 0);
        }
        let begin = start(io)?;
        let queue = &mut self.queue;
        let result = (|| {
            let mut cids = [0u16; BATCH];
            for (i, w) in words.iter().take(lbas.len()).enumerate() {
                cids[i] = queue.stage(io, s, memory, *w)?;
            }
            queue.ring(io)?;
            let mut done = [false; BATCH];
            service(
                queue,
                io,
                line,
                begin,
                lbas.len() as u32,
                stats,
                |io, entry| {
                    let i = (0..lbas.len())
                        .find(|&i| cids[i] == entry.cid && !done[i])
                        .ok_or(Error::Completion)?;
                    if entry.failed() {
                        return Err(Error::Command);
                    }
                    copy_slice(io, i, &mut out[i * size..(i + 1) * size]);
                    done[i] = true;
                    Ok(())
                },
            )?;
            Ok(start(io)?.saturating_sub(begin))
        })();
        if result.is_err() {
            // Something may still be in flight or asserted: no later step may reuse it.
            queue.poison();
        }
        result
    }

    /// Late/stray window on this pair's queue.
    pub fn quiet(
        &mut self,
        io: &mut impl AdminIo,
        line: &mut impl Line,
        ns: u64,
        stats: &mut Stats,
    ) -> Result<u32, Error> {
        quiet(&mut self.queue, io, line, ns, stats)
    }

    /// Mask vector 0, read one block, and require its completion to post without a
    /// delivery and stay undelivered for QUIET_NS; then unmask, and require the
    /// pending entry to raise INTA and be reaped by a delivery. — TIMBERDOODLE
    pub fn read_masked(
        &mut self,
        io: &mut impl AdminIo,
        line: &mut impl Line,
        s: Startup,
        memory: Memory,
        lba: u64,
        out: &mut [u8],
        stats: &mut Stats,
    ) -> Result<Masked, Error> {
        let words = read_command(&self.ns, lba, 1, memory.physical.wrapping_add(DATA))?;
        if out.len() != self.ns.lba_bytes as usize {
            return Err(Error::Range);
        }
        if self.queue.outstanding() != 0 {
            return Err(Error::State);
        }
        let queue = &mut self.queue;
        let result = (|| {
            mask(io)?;
            for off in (DATA..DATA + out.len() as u64).step_by(4) {
                io.dma_write32(off, 0);
            }
            let begin = start(io)?;
            let deadline = begin.checked_add(COMMAND_TIMEOUT_NS).ok_or(Error::Clock)?;
            let cid = queue.stage(io, s, memory, words)?;
            queue.ring(io)?;
            let mut proof = Masked::default();
            loop {
                if queue.ready(io)? {
                    break;
                }
                let now = start(io)?;
                if now < begin {
                    return Err(Error::Clock);
                }
                if now >= deadline {
                    return Err(Error::Timeout);
                }
                if line.wait(MASKED_POLL_NS.min(deadline - now))? == Wake::Interrupt {
                    masked_delivery(line, &mut proof, stats)?;
                }
            }
            proof.posted_ns = start(io)?.saturating_sub(begin);
            let hold = start(io)?;
            let hold_end = hold.checked_add(QUIET_NS).ok_or(Error::Clock)?;
            loop {
                let now = start(io)?;
                if now < hold {
                    return Err(Error::Clock);
                }
                if now >= hold_end || line.wait(hold_end - now)? == Wake::Timeout {
                    break;
                }
                masked_delivery(line, &mut proof, stats)?;
            }
            let unmasked = start(io)?;
            unmask(io)?;
            service(queue, io, line, unmasked, 1, stats, |io, entry| {
                if entry.cid != cid {
                    return Err(Error::Completion);
                }
                if entry.failed() {
                    return Err(Error::Command);
                }
                copy_slice(io, 0, out);
                Ok(())
            })?;
            proof.unmask_ns = start(io)?.saturating_sub(unmasked);
            Ok(proof)
        })();
        if result.is_err() {
            queue.poison();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::admin::{Namespace, Queue};
    use crate::io::{Allocated, IO_CQ, IO_SQ};
    use crate::RegisterIo;
    use std::{cell::RefCell, rc::Rc, vec::Vec};

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
            used: 0,
            lba_bytes: 512,
            bytes: 500107862016,
            metadata: 0,
            protection: 0,
            format: 0,
        }
    }
    fn block(lba: u64, i: usize) -> u8 {
        (lba.wrapping_mul(29) as usize + i * 3) as u8
    }

    /// One I/O pair behind a pin-interrupt controller and a kernel-style line.
    /// INTA is asserted while vector 0 is unmasked (or `ignore_mask`) and the
    /// controller has posted entries beyond the host's CQ head doorbell.
    struct Dev {
        dma: Vec<u32>,
        now: u64,
        tail: u16,
        head_db: u16,
        cq_tail: u16,
        phase: u32,
        masked: bool,
        /// Commands rung but not yet posted (cid, lba, slice).
        queued: Vec<(u32, u64, u64)>,
        /// Post at most one completion per `wait` (no coalescing).
        serial: bool,
        /// Keep INTA asserted for this many deliveries after a head release.
        linger: u32,
        lingering: u32,
        ignore_mask: bool,
        strays: u32,
        gic_enabled: bool,
        completes: u32,
        doorbells: Vec<(u16, u32)>,
        barrier: bool,
        acquired: bool,
        intms_writes: Vec<u16>,
    }
    impl Dev {
        fn new() -> Self {
            Self {
                dma: std::vec![0; (crate::DMA_BYTES / 4) as usize],
                now: 1,
                tail: 0,
                head_db: 0,
                cq_tail: 0,
                phase: 1,
                masked: true,
                queued: Vec::new(),
                serial: false,
                linger: 0,
                lingering: 0,
                ignore_mask: false,
                strays: 0,
                gic_enabled: true,
                completes: 0,
                doorbells: Vec::new(),
                barrier: false,
                acquired: false,
                intms_writes: Vec::new(),
            }
        }
        fn post_one(&mut self) -> bool {
            if self.queued.is_empty() {
                return false;
            }
            let (cid, lba, slice) = self.queued.remove(0);
            for i in 0..512 {
                let word = &mut self.dma[(slice as usize + i) / 4];
                *word |= u32::from(block(lba, i)) << ((i % 4) * 8);
            }
            let base = IO_CQ as usize / 4 + self.cq_tail as usize * 4;
            self.dma[base] = 0;
            self.dma[base + 2] = (1 << 16) | u32::from(self.tail);
            self.dma[base + 3] = cid | (self.phase << 16);
            self.cq_tail = (self.cq_tail + 1) % crate::DEPTH;
            if self.cq_tail == 0 {
                self.phase ^= 1;
            }
            true
        }
        fn post(&mut self) {
            if self.serial {
                // The next completion posts only after the previous one was released.
                if self.cq_tail == self.head_db {
                    self.post_one();
                }
            } else {
                while self.post_one() {}
            }
        }
        fn asserted(&self) -> bool {
            (!self.masked || self.ignore_mask) && self.cq_tail != self.head_db || self.lingering > 0
        }
    }
    struct Io(Rc<RefCell<Dev>>);
    struct FakeLine(Rc<RefCell<Dev>>);
    impl RegisterIo for Io {
        fn read32(&mut self, o: u16) -> u32 {
            let d = self.0.borrow();
            match o {
                CSTS => 1,
                INTMS => u32::from(d.masked),
                _ => panic!("unplanned read {o:#x}"),
            }
        }
        fn write32(&mut self, o: u16, v: u32) {
            let mut d = self.0.borrow_mut();
            match o {
                INTMS => {
                    assert_eq!(v, 1);
                    d.masked = true;
                    d.intms_writes.push(o);
                }
                INTMC => {
                    assert_eq!(v, 1);
                    d.masked = false;
                    d.intms_writes.push(o);
                }
                0x1008 => {
                    assert!(d.barrier);
                    let mut t = d.tail;
                    while t != v as u16 {
                        let base = IO_SQ as usize / 4 + t as usize * 16;
                        let w: Vec<u32> = d.dma[base..base + 16].to_vec();
                        assert_eq!(w[0] & 0xffff, 2);
                        let lba = u64::from(w[10]) | (u64::from(w[11]) << 32);
                        let slice = u64::from(w[6]) - memory().physical;
                        assert!((DATA..IO_SQ).contains(&slice));
                        assert!(d.dma[slice as usize / 4..][..128].iter().all(|&x| x == 0));
                        d.queued.push((w[0] >> 16, lba, slice));
                        t = (t + 1) % crate::DEPTH;
                    }
                    d.tail = v as u16;
                    d.acquired = false;
                    d.doorbells.push((o, v));
                    d.post();
                }
                0x100c => {
                    assert!(d.barrier && d.acquired);
                    d.head_db = v as u16;
                    if d.linger > 0 {
                        d.lingering = d.linger;
                    }
                    d.doorbells.push((o, v));
                }
                _ => panic!("unplanned write {o:#x}"),
            }
            d.barrier = false;
        }
        fn barrier(&mut self) {
            self.0.borrow_mut().barrier = true;
        }
        fn now_ns(&mut self) -> u64 {
            self.0.borrow().now
        }
        fn sleep_ns(&mut self, n: u64) -> bool {
            self.0.borrow_mut().now += n;
            true
        }
    }
    impl AdminIo for Io {
        fn dma_read32(&mut self, off: u64) -> u32 {
            let d = self.0.borrow();
            if off < IO_CQ || off % 16 != 12 {
                assert!(d.acquired, "data/CQE read before phase acquire at {off:#x}");
            }
            d.dma[off as usize / 4]
        }
        fn dma_write32(&mut self, off: u64, value: u32) {
            assert!(off < IO_CQ && !(crate::PAGE..DATA).contains(&off));
            let mut d = self.0.borrow_mut();
            d.barrier = false;
            d.dma[off as usize / 4] = value;
        }
        fn dma_acquire(&mut self) {
            self.0.borrow_mut().acquired = true;
        }
    }
    impl Line for FakeLine {
        fn wait(&mut self, ns: u64) -> Result<Wake, Error> {
            let mut d = self.0.borrow_mut();
            d.post();
            if d.gic_enabled && (d.asserted() || d.strays > 0) {
                if d.strays > 0 && !d.asserted() {
                    d.strays -= 1;
                }
                if d.lingering > 0 {
                    d.lingering -= 1;
                }
                d.gic_enabled = false;
                return Ok(Wake::Interrupt);
            }
            d.now += ns;
            Ok(Wake::Timeout)
        }
        fn complete(&mut self) -> Result<(), Error> {
            let mut d = self.0.borrow_mut();
            assert!(!d.gic_enabled, "complete without a delivery");
            d.gic_enabled = true;
            d.completes += 1;
            Ok(())
        }
    }
    fn pair() -> IoPair {
        IoPair {
            queue: Queue::at(1, IO_SQ, IO_CQ),
            ns: namespace(),
            allocated: Allocated {
                submission: 1,
                completion: 1,
            },
        }
    }
    fn rig() -> (Rc<RefCell<Dev>>, Io, FakeLine) {
        let d = Rc::new(RefCell::new(Dev::new()));
        (d.clone(), Io(d.clone()), FakeLine(d))
    }
    fn check(out: &[u8], lbas: &[u64]) {
        for (n, &lba) in lbas.iter().enumerate() {
            assert!(out[n * 512..(n + 1) * 512]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == block(lba, i)));
        }
    }

    #[test]
    fn single_read_is_reaped_released_then_completed() {
        let (d, mut io, mut line) = rig();
        let mut p = pair();
        unmask(&mut io).unwrap();
        let mut stats = Stats::default();
        let mut out = [0u8; 512];
        p.read_irq(
            &mut io,
            &mut line,
            startup(),
            memory(),
            &[0],
            &mut out,
            &mut stats,
        )
        .unwrap();
        check(&out, &[0]);
        assert_eq!(
            stats,
            Stats {
                deliveries: 1,
                empty: 0,
                entries: 1,
                max_batch: 1
            }
        );
        let d = d.borrow();
        assert_eq!(d.doorbells, [(0x1008, 1), (0x100c, 1)]);
        assert!(d.gic_enabled && d.completes == 1);
    }

    #[test]
    fn one_doorbell_two_reads_coalesce_into_one_delivery() {
        let (_, mut io, mut line) = rig();
        let mut p = pair();
        unmask(&mut io).unwrap();
        let mut stats = Stats::default();
        let lbas = [1, 976773167];
        let mut out = [0u8; 1024];
        p.read_irq(
            &mut io,
            &mut line,
            startup(),
            memory(),
            &lbas,
            &mut out,
            &mut stats,
        )
        .unwrap();
        check(&out, &lbas);
        assert_eq!(
            (stats.deliveries, stats.max_batch, stats.entries),
            (1, 2, 2)
        );
    }

    #[test]
    fn serial_completions_take_one_delivery_each() {
        let (d, mut io, mut line) = rig();
        d.borrow_mut().serial = true;
        let mut p = pair();
        unmask(&mut io).unwrap();
        let mut stats = Stats::default();
        let lbas = [3, 4, 5];
        let mut out = [0u8; 1536];
        p.read_irq(
            &mut io,
            &mut line,
            startup(),
            memory(),
            &lbas,
            &mut out,
            &mut stats,
        )
        .unwrap();
        check(&out, &lbas);
        assert_eq!(
            (stats.deliveries, stats.max_batch, stats.entries),
            (3, 1, 3)
        );
        assert_eq!(d.borrow().completes, 3);
    }

    #[test]
    fn late_and_stray_deliveries_are_completed_without_reaping() {
        let (d, mut io, mut line) = rig();
        d.borrow_mut().linger = 2;
        let mut p = pair();
        unmask(&mut io).unwrap();
        let mut stats = Stats::default();
        let mut out = [0u8; 512];
        p.read_irq(
            &mut io,
            &mut line,
            startup(),
            memory(),
            &[7],
            &mut out,
            &mut stats,
        )
        .unwrap();
        assert_eq!(p.quiet(&mut io, &mut line, QUIET_NS, &mut stats), Ok(2));
        d.borrow_mut().strays = 3;
        assert_eq!(p.quiet(&mut io, &mut line, QUIET_NS, &mut stats), Ok(3));
        assert_eq!(stats.empty, 5);
        assert_eq!(p.quiet(&mut io, &mut line, QUIET_NS, &mut stats), Ok(0));
        assert!(d.borrow().gic_enabled);
    }

    #[test]
    fn stray_storm_fails_the_operation_with_the_line_masked() {
        let (d, mut io, mut line) = rig();
        d.borrow_mut().strays = 100;
        let mut p = pair();
        let mut stats = Stats::default();
        assert_eq!(
            p.quiet(&mut io, &mut line, QUIET_NS, &mut stats),
            Err(Error::Storm)
        );
        assert!(
            !d.borrow().gic_enabled,
            "storm leaves the kernel mask in place"
        );
    }

    #[test]
    fn masked_vector_holds_a_posted_entry_until_unmask() {
        let (d, mut io, mut line) = rig();
        let mut p = pair();
        unmask(&mut io).unwrap();
        let mut stats = Stats::default();
        let mut out = [0u8; 512];
        let m = p
            .read_masked(
                &mut io,
                &mut line,
                startup(),
                memory(),
                0,
                &mut out,
                &mut stats,
            )
            .unwrap();
        check(&out, &[0]);
        assert_eq!(m.deliveries, 0);
        assert!(m.posted_ns < COMMAND_TIMEOUT_NS && m.unmask_ns < SLICE_NS);
        assert_eq!(stats.deliveries, 1);
        assert_eq!(d.borrow().intms_writes, [INTMC, INTMS, INTMC]);
    }

    #[test]
    fn controller_ignoring_the_mask_is_a_storm_not_a_livelock() {
        let (d, mut io, mut line) = rig();
        d.borrow_mut().ignore_mask = true;
        let mut p = pair();
        let mut stats = Stats::default();
        let mut out = [0u8; 512];
        assert_eq!(
            p.read_masked(
                &mut io,
                &mut line,
                startup(),
                memory(),
                0,
                &mut out,
                &mut stats
            ),
            Err(Error::Storm)
        );
    }

    #[test]
    fn missing_delivery_is_reported_as_interrupt_failure() {
        let (d, mut io, mut line) = rig();
        // Vector left masked: the entry posts but INTA never asserts.
        let mut p = pair();
        let mut stats = Stats::default();
        let mut out = [0u8; 512];
        assert_eq!(
            p.read_irq(
                &mut io,
                &mut line,
                startup(),
                memory(),
                &[0],
                &mut out,
                &mut stats
            ),
            Err(Error::Interrupt)
        );
        assert_eq!(stats.deliveries, 0);
        assert!(d.borrow().doorbells.iter().all(|&(o, _)| o != 0x100c));
        assert!(p.queue.poisoned());
        // The read may still be in flight: the pair refuses deletion before any admin write.
        assert_eq!(
            p.close(&mut Queue::new(), &mut io, startup(), memory()),
            Err(Error::State)
        );
    }

    #[test]
    fn refusals_precede_every_write() {
        let (d, mut io, mut line) = rig();
        let mut p = pair();
        let mut stats = Stats::default();
        let size = namespace().size;
        for (lbas, len) in [
            (&[size][..], 512),
            (&[0, size][..], 1024),
            (&[][..], 0),
            (&[0, 1, 2, 3, 4][..], 2560),
            (&[0][..], 1024),
        ] {
            let mut out = std::vec![0u8; len];
            assert_eq!(
                p.read_irq(
                    &mut io,
                    &mut line,
                    startup(),
                    memory(),
                    lbas,
                    &mut out,
                    &mut stats
                ),
                Err(Error::Range)
            );
        }
        let d = d.borrow();
        assert!(d.doorbells.is_empty() && d.dma.iter().all(|&w| w == 0));
        assert!(!p.queue.poisoned());
    }
}
