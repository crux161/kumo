//j802
//j803
//j805
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
use crate::io::{flush_command, read_command, write_command, IoPair};
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
/// PortUnbind removes a watch, not packets already queued for it. An IRQ may win
/// just before its timeout fires during cleanup; drain only known retired timer
/// identities while retaining the current timer and command deadline. — TIMBERDOODLE
pub struct WaitSources {
    interrupt: u64,
    retired: [u64; 16],
    next: usize,
}
impl WaitSources {
    pub fn new(interrupt: u64) -> Self {
        Self {
            interrupt,
            retired: [0; 16],
            next: 0,
        }
    }
    pub fn retire(&mut self, timer: u64) {
        self.retired[self.next] = timer;
        self.next = (self.next + 1) % self.retired.len();
    }
    pub fn wait(
        &mut self,
        timer: u64,
        mut next: impl FnMut() -> u64,
    ) -> Result<(Wake, u32), Error> {
        if timer == 0 || self.interrupt == 0 || timer == self.interrupt {
            return Err(Error::Interrupt);
        }
        let mut skipped = 0;
        loop {
            let source = next();
            if source == self.interrupt {
                return Ok((Wake::Interrupt, skipped));
            }
            if source == timer {
                return Ok((Wake::Timeout, skipped));
            }
            if source != 0 {
                if let Some(i) = self.retired.iter().position(|&old| old == source) {
                    self.retired[i] = 0;
                    skipped += 1;
                    if skipped <= 16 {
                        continue;
                    }
                }
            }
            return Err(Error::Interrupt);
        }
    }
}

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

    /// NV8 Write: validate before touching DMA/SQ, retain the payload through the
    /// matching INTx completion. Extent authority is the caller's policy. — TIMBERDOODLE
    pub fn write_irq(
        &mut self,
        io: &mut impl AdminIo,
        line: &mut impl Line,
        s: Startup,
        memory: Memory,
        lba: u64,
        data: &[u8],
        stats: &mut Stats,
    ) -> Result<u64, Error> {
        s.validate()?;
        if !memory.valid() || memory.session != s.session {
            return Err(Error::Memory);
        }
        if self.queue.poisoned() || self.queue.outstanding() != 0 {
            return Err(Error::State);
        }
        let size = self.ns.lba_bytes as usize;
        if !self.ns.plain_blocks()
            || data.is_empty()
            || data.len() > PAGE as usize
            || data.len() % size != 0
        {
            return Err(Error::Range);
        }
        let w = write_command(
            &self.ns,
            lba,
            (data.len() / size) as u16,
            memory.physical.wrapping_add(DATA),
        )?;
        for (i, b) in data.chunks_exact(4).enumerate() {
            io.dma_write32(
                DATA + i as u64 * 4,
                u32::from_le_bytes(b.try_into().unwrap()),
            );
        }
        self.command_irq(io, line, s, memory, w, stats)
    }
    pub fn flush_irq(
        &mut self,
        io: &mut impl AdminIo,
        line: &mut impl Line,
        s: Startup,
        memory: Memory,
        stats: &mut Stats,
    ) -> Result<u64, Error> {
        self.command_irq(io, line, s, memory, flush_command(&self.ns)?, stats)
    }
    fn command_irq(
        &mut self,
        io: &mut impl AdminIo,
        line: &mut impl Line,
        s: Startup,
        memory: Memory,
        words: [u32; 16],
        stats: &mut Stats,
    ) -> Result<u64, Error> {
        if self.queue.poisoned() || self.queue.outstanding() != 0 {
            return Err(Error::State);
        }
        let begin = start(io)?;
        let queue = &mut self.queue;
        let result = (|| {
            let cid = queue.stage(io, s, memory, words)?;
            queue.ring(io)?;
            service(queue, io, line, begin, 1, stats, |_, entry| {
                if entry.cid != cid {
                    return Err(Error::Completion);
                }
                if entry.failed() {
                    return Err(Error::Command);
                }
                Ok(())
            })?;
            Ok(start(io)?.saturating_sub(begin))
        })();
        if result.is_err() {
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
        queued: Vec<(u32, u64, u64, u32)>,
        volatile: std::collections::BTreeMap<u64, [u8; 512]>,
        stable: std::collections::BTreeMap<u64, [u8; 512]>,
        command_error: bool,
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
                volatile: Default::default(),
                stable: Default::default(),
                command_error: false,
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
            let (cid, lba, slice, opcode) = self.queued.remove(0);
            match opcode {
                0 => self.stable = self.volatile.clone(),
                1 => {
                    let mut b = [0; 512];
                    for (i, x) in b.iter_mut().enumerate() {
                        *x = (self.dma[(slice as usize + i) / 4] >> ((i % 4) * 8)) as u8;
                    }
                    self.volatile.insert(lba, b);
                }
                2 => {
                    for i in 0..512 {
                        let value = self
                            .volatile
                            .get(&lba)
                            .map_or_else(|| block(lba, i), |b| b[i]);
                        let word = &mut self.dma[(slice as usize + i) / 4];
                        *word |= u32::from(value) << ((i % 4) * 8);
                    }
                }
                _ => panic!("unplanned opcode"),
            }
            let base = IO_CQ as usize / 4 + self.cq_tail as usize * 4;
            self.dma[base] = 0;
            self.dma[base + 2] = (1 << 16) | u32::from(self.tail);
            self.dma[base + 3] = cid | (self.phase << 16) | (u32::from(self.command_error) << 17);
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
                        let opcode = w[0] & 0xffff;
                        assert!(opcode <= 2);
                        assert_eq!(w[1], 1);
                        let lba = u64::from(w[10]) | (u64::from(w[11]) << 32);
                        let slice = if opcode == 0 {
                            assert!(w[2..].iter().all(|&x| x == 0));
                            0
                        } else {
                            assert_eq!(w[12], 0);
                            assert!(w[2..6].iter().all(|&x| x == 0));
                            assert_eq!(w[7], 0);
                            assert!(w[8..10].iter().chain(w[13..].iter()).all(|&x| x == 0));
                            let slice = u64::from(w[6]) - memory().physical;
                            assert!((DATA..IO_SQ).contains(&slice));
                            if opcode == 2 {
                                assert!(d.dma[slice as usize / 4..][..128].iter().all(|&x| x == 0));
                            }
                            slice
                        };
                        d.queued.push((w[0] >> 16, lba, slice, opcode));
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
    fn irq_cleanup_race_drains_retired_timer_without_consuming_active_deadline() {
        let mut sources = WaitSources::new(10);
        sources.retire(20); // IRQ won, but timer20 queued before its watch was removed.
        let mut events = std::collections::VecDeque::from([20, 10, 99]);
        assert_eq!(
            sources.wait(21, || events.pop_front().unwrap()),
            Ok((Wake::Interrupt, 1))
        );
        assert_eq!(events.pop_front(), Some(99));
        sources.retire(21);
        let mut events = std::collections::VecDeque::from([21, 22]);
        assert_eq!(
            sources.wait(22, || events.pop_front().unwrap()),
            Ok((Wake::Timeout, 1))
        );
        assert!(events.is_empty());
        sources.retire(22);
        assert_eq!(sources.wait(23, || 777), Err(Error::Interrupt));
        assert_eq!(sources.wait(23, || 0), Err(Error::Interrupt));
        let mut events = std::collections::VecDeque::from([22, 22]);
        assert_eq!(
            sources.wait(23, || events.pop_front().unwrap()),
            Err(Error::Interrupt)
        );
    }

    #[test]
    fn write_flush_and_readback_obey_dma_direction_and_durability_order() {
        let (d, mut io, mut line) = rig();
        let mut p = pair();
        unmask(&mut io).unwrap();
        let mut stats = Stats::default();
        let payload = crate::write::pattern();
        p.write_irq(
            &mut io,
            &mut line,
            startup(),
            memory(),
            crate::write::TEST_LBA,
            &payload,
            &mut stats,
        )
        .unwrap();
        assert_eq!(d.borrow().volatile[&crate::write::TEST_LBA], payload);
        assert!(d.borrow().stable.is_empty());
        p.flush_irq(&mut io, &mut line, startup(), memory(), &mut stats)
            .unwrap();
        assert_eq!(d.borrow().stable[&crate::write::TEST_LBA], payload);
        let mut out = [0; 512];
        p.read_irq(
            &mut io,
            &mut line,
            startup(),
            memory(),
            &[crate::write::TEST_LBA],
            &mut out,
            &mut stats,
        )
        .unwrap();
        assert_eq!(out, payload);
        assert_eq!((stats.entries, stats.deliveries), (3, 3));
        assert_eq!(d.borrow().completes, 3);
        let bytes = d.borrow().dma.clone();
        let bells = d.borrow().doorbells.clone();
        for (lba, len) in [
            (namespace().size, 512),
            (u64::MAX, 512),
            (0, 0),
            (0, 511),
            (0, 513),
            (0, 4097),
        ] {
            let input = std::vec![1;len];
            assert_eq!(
                p.write_irq(
                    &mut io,
                    &mut line,
                    startup(),
                    memory(),
                    lba,
                    &input,
                    &mut stats
                ),
                Err(Error::Range)
            );
            assert_eq!(d.borrow().dma, bytes);
            assert_eq!(d.borrow().doorbells, bells);
            assert!(!p.queue.poisoned());
        }
    }
    #[test]
    fn failed_write_or_flush_poisons_and_no_later_dma_is_reused() {
        for flush in [false, true] {
            let (d, mut io, mut line) = rig();
            let mut p = pair();
            unmask(&mut io).unwrap();
            let mut stats = Stats::default();
            d.borrow_mut().command_error = true;
            let result = if flush {
                p.flush_irq(&mut io, &mut line, startup(), memory(), &mut stats)
            } else {
                p.write_irq(
                    &mut io,
                    &mut line,
                    startup(),
                    memory(),
                    crate::write::TEST_LBA,
                    &crate::write::pattern(),
                    &mut stats,
                )
            };
            assert_eq!(result, Err(Error::Command));
            assert!(p.queue.poisoned());
            let bytes = d.borrow().dma.clone();
            let bells = d.borrow().doorbells.clone();
            assert_eq!(
                p.write_irq(
                    &mut io,
                    &mut line,
                    startup(),
                    memory(),
                    crate::write::TEST_LBA,
                    &crate::write::pattern(),
                    &mut stats
                ),
                Err(Error::State)
            );
            assert_eq!(
                p.flush_irq(&mut io, &mut line, startup(), memory(), &mut stats),
                Err(Error::State)
            );
            assert_eq!(d.borrow().dma, bytes);
            assert_eq!(d.borrow().doorbells, bells);
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
