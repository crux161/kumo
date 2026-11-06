//j799
//j801
//j802
//j803
//! One outstanding polled command per queue: NV4a Identify on the admin queue, and
//! the queue engine NV5 reuses for its I/O pair. One PRP page, legacy 4-byte doorbell
//! stride. D5 cooperative raw DMA, no containment. — TIMBERDOODLE

use crate::{start, u32at, u64at, Error, Memory, RegisterIo, Startup, CSTS, DEPTH, PAGE};
use core::fmt;

pub const DATA: u64 = 2 * PAGE;
pub const SQ_TAIL: u16 = 0x1000;
pub const CQ_HEAD: u16 = 0x1004;
// CAP.TO bounds RDY, not commands. This is a separate proof-run command timeout.
pub const COMMAND_TIMEOUT_NS: u64 = 30_000_000_000;

/// Legacy doorbells with CAP.DSTRD=0 (Startup::validate refuses any other stride).
pub const fn sq_tail(qid: u16) -> u16 {
    SQ_TAIL + 8 * qid
}
pub const fn cq_head(qid: u16) -> u16 {
    CQ_HEAD + 8 * qid
}

pub trait AdminIo: RegisterIo {
    fn dma_read32(&mut self, offset: u64) -> u32;
    fn dma_write32(&mut self, offset: u64, value: u32);
    /// Order subsequent completion/data loads after observing the new phase.
    fn dma_acquire(&mut self);
}

#[derive(Clone, Copy, Debug)]
pub enum Identify {
    Controller,
    ActiveNamespaces,
    Namespace(u32),
}
impl Identify {
    pub fn command(self, cid: u16, physical: u64) -> Result<[u32; 16], Error> {
        if physical == 0 || physical & (PAGE - 1) != 0 || physical >= 1 << 32 {
            return Err(Error::Memory);
        }
        let (nsid, cns) = match self {
            Self::Controller => (0, 1),
            Self::ActiveNamespaces => (0, 2),
            Self::Namespace(n) if n != 0 && n != u32::MAX => (n, 0),
            _ => return Err(Error::Data),
        };
        let mut words = [0; 16];
        words[0] = 6 | (u32::from(cid) << 16);
        words[1] = nsid;
        words[6] = physical as u32;
        words[7] = (physical >> 32) as u32;
        words[10] = cns;
        Ok(words)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Completion {
    pub cid: u16,
    pub status: u16,
    /// CQE DW0, command specific (Set Features returns the allocation here).
    pub result: u32,
    pub elapsed_ns: u64,
}

//j801
//j802
/// Commands on one SQ/CQ pair inside the DMA allocation. The admin pair is qid 0 at
/// pages 0/1; the I/O pair is qid 1 at pages 3/4. Polled `submit` keeps one command
/// in flight across all queues and zeroes the whole shared data page; NV6's interrupt
/// path stages several reads into disjoint slices of it and reaps them with `take`.
/// Any failed step poisons the instance permanently. — TIMBERDOODLE
pub struct Queue {
    qid: u16,
    sq: u64,
    cq: u64,
    tail: u16,
    head: u16,
    phase: u32,
    cid: u16,
    outstanding: u16,
    poisoned: bool,
}
pub type AdminQueue = Queue;
impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

/// One reaped completion queue entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry {
    pub cid: u16,
    pub status: u16,
    pub sqhd: u16,
    pub result: u32,
}
impl Entry {
    pub fn failed(&self) -> bool {
        (self.status >> 1) & 0x7ff != 0
    }
}

impl Queue {
    /// Requires a newly enabled controller with zeroed CQ phase bits.
    pub const fn new() -> Self {
        Self::at(0, 0, PAGE)
    }
    /// Requires a newly created queue pair whose CQ page is still zeroed.
    pub const fn at(qid: u16, sq: u64, cq: u64) -> Self {
        Self {
            qid,
            sq,
            cq,
            tail: 0,
            head: 0,
            phase: 1,
            cid: 1,
            outstanding: 0,
            poisoned: false,
        }
    }
    pub fn poisoned(&self) -> bool {
        self.poisoned
    }
    pub fn outstanding(&self) -> u16 {
        self.outstanding
    }
    /// Stop the instance after a failure outside its own steps (a lost delivery).
    pub fn poison(&mut self) {
        self.poisoned = true;
    }

    fn guarded<T>(&mut self, step: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        if self.poisoned {
            return Err(Error::State);
        }
        // Any unproven step permanently stops this queue instance. — TIMBERDOODLE
        self.poisoned = true;
        let result = step(self);
        if result.is_ok() {
            self.poisoned = false;
        }
        result
    }

    /// Write one SQE (CID field zero; the queue assigns it) without ringing the
    /// doorbell. One slot always stays empty, as the full-queue rule requires.
    pub fn stage(
        &mut self,
        io: &mut impl AdminIo,
        s: Startup,
        memory: Memory,
        mut words: [u32; 16],
    ) -> Result<u16, Error> {
        self.guarded(|q| {
            s.validate()?;
            if !memory.valid() || memory.session != s.session {
                return Err(Error::Memory);
            }
            if words[0] >> 16 != 0 || q.outstanding >= DEPTH - 1 {
                return Err(Error::Command);
            }
            let cid = q.cid;
            words[0] |= u32::from(cid) << 16;
            let slot = q.sq + u64::from(q.tail) * 64;
            for (i, word) in words.into_iter().enumerate() {
                io.dma_write32(slot + i as u64 * 4, word);
            }
            q.tail = (q.tail + 1) % DEPTH;
            q.cid = q.cid.wrapping_add(1);
            q.outstanding += 1;
            Ok(cid)
        })
    }

    /// Publish every staged SQE with one tail doorbell write.
    pub fn ring(&mut self, io: &mut impl AdminIo) -> Result<(), Error> {
        self.guarded(|q| {
            if io.read32(CSTS) & 3 != 1 {
                return Err(Error::Fatal);
            }
            io.barrier();
            io.write32(sq_tail(q.qid), u32::from(q.tail));
            io.barrier();
            Ok(())
        })
    }

    /// True when the CQE at the head carries the current phase. Consumes nothing.
    pub fn ready(&mut self, io: &mut impl AdminIo) -> Result<bool, Error> {
        self.guarded(|q| {
            if io.read32(CSTS) & 3 != 1 {
                return Err(Error::Fatal);
            }
            let last = io.dma_read32(q.cq + u64::from(q.head) * 16 + 12);
            Ok((last >> 16) & 1 == q.phase)
        })
    }

    /// Reap the CQE at the head if its phase is current. The head advances, but the
    /// CQ head doorbell is written only by `release`, after the caller has used the
    /// entry. An entry naming another SQ, an impossible SQHD or arriving with nothing
    /// outstanding poisons the queue.
    pub fn take(&mut self, io: &mut impl AdminIo) -> Result<Option<Entry>, Error> {
        self.guarded(|q| {
            if io.read32(CSTS) & 3 != 1 {
                return Err(Error::Fatal);
            }
            let completion = q.cq + u64::from(q.head) * 16;
            let last = io.dma_read32(completion + 12);
            if (last >> 16) & 1 != q.phase {
                return Ok(None);
            }
            io.dma_acquire();
            let queue = io.dma_read32(completion + 8);
            if queue >> 16 != u32::from(q.qid) || queue as u16 >= DEPTH || q.outstanding == 0 {
                return Err(Error::Completion);
            }
            let entry = Entry {
                cid: last as u16,
                status: (last >> 16) as u16,
                sqhd: queue as u16,
                result: io.dma_read32(completion),
            };
            q.head = (q.head + 1) % DEPTH;
            if q.head == 0 {
                q.phase ^= 1;
            }
            q.outstanding -= 1;
            Ok(Some(entry))
        })
    }

    /// Hand every reaped CQ slot back to the controller. For pin interrupts this is
    /// also what lets the controller deassert INTx. — TIMBERDOODLE
    pub fn release(&mut self, io: &mut impl AdminIo) -> Result<(), Error> {
        self.guarded(|q| {
            io.barrier();
            io.write32(cq_head(q.qid), u32::from(q.head));
            io.barrier();
            Ok(())
        })
    }

    pub fn identify(
        &mut self,
        io: &mut impl AdminIo,
        s: Startup,
        memory: Memory,
        kind: Identify,
        output: &mut [u8; PAGE as usize],
    ) -> Result<Completion, Error> {
        if self.qid != 0 {
            return Err(Error::State);
        }
        let words = kind.command(0, memory.physical.wrapping_add(DATA))?;
        self.submit(io, s, memory, words, output)
    }

    /// Submit `words` (CID field zero) and poll for its completion; `output` gets the
    /// first `output.len()` bytes of the data page. Requires an idle queue. Validation
    /// that touches no queue memory happens before this call.
    pub fn submit(
        &mut self,
        io: &mut impl AdminIo,
        s: Startup,
        memory: Memory,
        words: [u32; 16],
        output: &mut [u8],
    ) -> Result<Completion, Error> {
        if self.poisoned {
            return Err(Error::State);
        }
        let result = self.submit_inner(io, s, memory, words, output);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn submit_inner(
        &mut self,
        io: &mut impl AdminIo,
        s: Startup,
        memory: Memory,
        words: [u32; 16],
        output: &mut [u8],
    ) -> Result<Completion, Error> {
        s.validate()?;
        if !memory.valid() || memory.session != s.session {
            return Err(Error::Memory);
        }
        if output.len() > PAGE as usize
            || output.len() % 4 != 0
            || words[0] >> 16 != 0
            || self.outstanding != 0
        {
            return Err(Error::Command);
        }
        let begin = start(io)?;
        let deadline = begin.checked_add(COMMAND_TIMEOUT_NS).ok_or(Error::Clock)?;
        if io.read32(CSTS) & 3 != 1 {
            return Err(Error::Fatal);
        }
        for off in (DATA..DATA + PAGE).step_by(4) {
            io.dma_write32(off, 0);
        }
        let cid = self.stage(io, s, memory, words)?;
        self.ring(io)?;
        let mut delay = 1_000_000;
        for _ in 0..4096 {
            let entry = self.take(io)?;
            let now = start(io)?;
            if now < begin {
                return Err(Error::Clock);
            }
            if let Some(e) = entry {
                if e.cid != cid || e.sqhd != self.tail {
                    return Err(Error::Completion);
                }
                if e.failed() {
                    return Err(Error::Command);
                }
                for (i, out) in output.chunks_exact_mut(4).enumerate() {
                    out.copy_from_slice(&io.dma_read32(DATA + i as u64 * 4).to_le_bytes());
                }
                self.release(io)?;
                return Ok(Completion {
                    cid,
                    status: e.status,
                    result: e.result,
                    elapsed_ns: now - begin,
                });
            }
            if now >= deadline {
                return Err(Error::Timeout);
            }
            if !io.sleep_ns(delay.min(deadline - now)) {
                return Err(Error::Sleep);
            }
            delay = (delay * 2).min(50_000_000);
        }
        Err(Error::Clock)
    }
}

fn fixed_text<const N: usize>(b: &[u8]) -> Result<[u8; N], Error> {
    let value: [u8; N] = b.try_into().map_err(|_| Error::Data)?;
    if !value.iter().all(|&v| (32..=126).contains(&v)) || value.iter().all(|&v| v == b' ') {
        return Err(Error::Data);
    }
    Ok(value)
}
fn text(b: &[u8]) -> &str {
    core::str::from_utf8(b).unwrap().trim_end_matches(' ')
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Controller {
    pub vid: u16,
    pub ssvid: u16,
    pub serial: [u8; 20],
    pub model: [u8; 40],
    pub firmware: [u8; 8],
    pub mdts: u8,
    pub version: u32,
    pub namespaces: u32,
    pub vwc: bool,
}
impl Controller {
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        if b.len() != PAGE as usize {
            return Err(Error::Data);
        }
        let c = Self {
            vid: u16::from_le_bytes(b[..2].try_into().unwrap()),
            ssvid: u16::from_le_bytes(b[2..4].try_into().unwrap()),
            serial: fixed_text(&b[4..24])?,
            model: fixed_text(&b[24..64])?,
            firmware: fixed_text(&b[64..72])?,
            mdts: b[77],
            version: u32at(b, 80),
            namespaces: u32at(b, 516),
            vwc: b[525] & 1 != 0,
        };
        if matches!(c.vid, 0 | 0xffff) || c.namespaces == 0 {
            return Err(Error::Data);
        }
        Ok(c)
    }
}
impl fmt::Display for Controller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NV4 CONTROLLER vid={:#x} ssvid={:#x} model=\"{}\" serial=\"{}\" firmware=\"{}\" version={:#x} mdts={} nn={} vwc={}",
            self.vid,self.ssvid,text(&self.model),text(&self.serial),text(&self.firmware),self.version,self.mdts,self.namespaces,self.vwc)
    }
}

/// First active NSID; validate the whole returned list, not only its first word.
pub fn first_namespace(b: &[u8]) -> Result<u32, Error> {
    if b.len() != PAGE as usize {
        return Err(Error::Data);
    }
    let mut first = 0;
    let mut previous = 0;
    let mut ended = false;
    for chunk in b.chunks_exact(4) {
        let n = u32::from_le_bytes(chunk.try_into().unwrap());
        if n == 0 {
            ended = true;
            continue;
        }
        if ended || n == u32::MAX || n <= previous {
            return Err(Error::Data);
        }
        if first == 0 {
            first = n;
        }
        previous = n;
    }
    if first == 0 {
        Err(Error::Data)
    } else {
        Ok(first)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Namespace {
    pub id: u32,
    pub size: u64,
    pub capacity: u64,
    pub used: u64,
    pub lba_bytes: u32,
    pub bytes: u64,
    pub metadata: u16,
    pub protection: u8,
    pub format: u8,
}
impl Namespace {
    pub fn decode(id: u32, b: &[u8]) -> Result<Self, Error> {
        if b.len() != PAGE as usize || matches!(id, 0 | u32::MAX) || b[25] > 15 || b[26] & 0xe0 != 0
        {
            return Err(Error::Data);
        }
        let format = b[26] & 15;
        if format > b[25] {
            return Err(Error::Data);
        }
        let off = 128 + format as usize * 4;
        if b[off + 2] < 9 {
            return Err(Error::Data);
        }
        let lba_bytes = 1u32.checked_shl(b[off + 2] as u32).ok_or(Error::Data)?;
        let size = u64at(b, 0);
        let capacity = u64at(b, 8);
        let used = u64at(b, 16);
        if size == 0 || capacity == 0 || capacity > size || used > capacity {
            return Err(Error::Data);
        }
        Ok(Self {
            id,
            size,
            capacity,
            used,
            lba_bytes,
            bytes: size.checked_mul(u64::from(lba_bytes)).ok_or(Error::Data)?,
            metadata: u16::from_le_bytes(b[off..off + 2].try_into().unwrap()),
            protection: b[29] & 7,
            format,
        })
    }
    pub fn plain_blocks(&self) -> bool {
        self.metadata == 0 && self.protection == 0 && self.lba_bytes <= 4096
    }
}
impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f,"NV4 NAMESPACE nsid={} nsze={} ncap={} nuse={} lbaf={} lba={} bytes={} metadata={} protection={} plain-blocks={}",
            self.id,self.size,self.capacity,self.used,self.format,self.lba_bytes,self.bytes,self.metadata,self.protection,self.plain_blocks())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn startup() -> Startup {
        Startup {
            session: 1,
            bar: 0xf0000000,
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
            physical: 0x60000000,
        }
    }
    struct Fake {
        dma: [u32; (crate::DMA_BYTES / 4) as usize],
        now: u64,
        tail: u16,
        head: u16,
        phase: u32,
        wrong: u8,
        acquired: bool,
        barrier: bool,
        timeout: bool,
        fatal: bool,
        commands: Vec<[u32; 16]>,
        writes: usize,
        cq_heads: Vec<u32>,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                dma: [0; (crate::DMA_BYTES / 4) as usize],
                now: 0,
                tail: 0,
                head: 0,
                phase: 1,
                wrong: 0,
                acquired: false,
                barrier: false,
                timeout: false,
                fatal: false,
                commands: Vec::new(),
                writes: 0,
                cq_heads: Vec::new(),
            }
        }
    }
    impl RegisterIo for Fake {
        fn read32(&mut self, off: u16) -> u32 {
            assert_eq!(off, CSTS);
            if self.fatal {
                3
            } else {
                1
            }
        }
        fn write32(&mut self, off: u16, value: u32) {
            self.writes += 1;
            assert!(self.barrier);
            self.barrier = false;
            match off {
                SQ_TAIL => {
                    let mut command = [0; 16];
                    command.copy_from_slice(
                        &self.dma[self.tail as usize * 16..self.tail as usize * 16 + 16],
                    );
                    assert_eq!(command[0] & 0xffff, 6);
                    assert_eq!(command[6], (memory().physical + DATA) as u32);
                    assert_eq!(command[7], 0);
                    assert_eq!(command[8], 0);
                    assert_eq!(command[9], 0);
                    assert!(self.dma[DATA as usize / 4..].iter().all(|&w| w == 0));
                    let cid = command[0] >> 16;
                    self.commands.push(command);
                    self.tail = value as u16;
                    self.acquired = false;
                    if !self.timeout {
                        self.dma[DATA as usize / 4] = 0x12345678;
                        let cq = PAGE as usize / 4 + self.head as usize * 4;
                        self.dma[cq + 2] = value | if self.wrong == 2 { 1 << 16 } else { 0 };
                        if self.wrong == 3 {
                            self.dma[cq + 2] = (value + 1) % u32::from(DEPTH);
                        }
                        self.dma[cq + 3] = (cid + u32::from(self.wrong == 1))
                            | (self.phase << 16)
                            | if self.wrong == 4 { 1 << 17 } else { 0 };
                    }
                }
                CQ_HEAD => {
                    assert!(self.acquired);
                    self.cq_heads.push(value);
                    self.head = value as u16;
                    if value == 0 {
                        self.phase ^= 1;
                    }
                }
                _ => panic!("unexpected register write"),
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
            if off >= DATA || (off >= PAGE && off % 16 != 12) {
                assert!(self.acquired, "data/CQE read before phase acquire");
            }
            self.dma[off as usize / 4]
        }
        fn dma_write32(&mut self, off: u64, value: u32) {
            assert!(off < PAGE || off >= DATA, "host must not write CQ");
            self.barrier = false;
            self.writes += 1;
            self.dma[off as usize / 4] = value;
        }
        fn dma_acquire(&mut self) {
            self.acquired = true;
        }
    }
    #[test]
    fn encode_exact_identify_prp_and_reserved_fields() {
        for (kind, nsid, cns) in [
            (Identify::Controller, 0, 1),
            (Identify::ActiveNamespaces, 0, 2),
            (Identify::Namespace(7), 7, 0),
        ] {
            let w = kind.command(0x1234, 0x12345000).unwrap();
            let mut expected = [0; 16];
            expected[0] = 0x12340006;
            expected[1] = nsid;
            expected[6] = 0x12345000;
            expected[10] = cns;
            assert_eq!(w, expected);
        }
        assert!(Identify::Namespace(0).command(1, 4096).is_err());
        assert!(Identify::Namespace(u32::MAX).command(1, 4096).is_err());
        for addr in [0, 1, 0x100000000] {
            assert!(Identify::Controller.command(1, addr).is_err());
        }
    }
    #[test]
    fn queue_orders_data_and_completions_across_two_phase_wraps() {
        let mut io = Fake::new();
        let mut q = AdminQueue::new();
        let mut out = [0; 4096];
        for n in 1..=u32::from(DEPTH) * 2 + 3 {
            let c = q
                .identify(&mut io, startup(), memory(), Identify::Controller, &mut out)
                .unwrap();
            assert_eq!(u32::from(c.cid), n);
            assert_eq!(&out[..4], &0x12345678u32.to_le_bytes());
            assert_eq!(*io.cq_heads.last().unwrap(), n % u32::from(DEPTH));
        }
        assert_eq!(io.commands.len(), 67);
        assert_eq!(q.phase, 1);
    }
    #[test]
    fn mismatched_or_failed_completion_poison_without_head_release_or_retry() {
        for wrong in 1..=4 {
            let mut io = Fake::new();
            io.wrong = wrong;
            let mut q = AdminQueue::new();
            let mut out = [0; 4096];
            let e = q
                .identify(&mut io, startup(), memory(), Identify::Controller, &mut out)
                .unwrap_err();
            assert_eq!(
                e,
                if wrong == 4 {
                    Error::Command
                } else {
                    Error::Completion
                }
            );
            assert!(io.cq_heads.is_empty());
            let writes = io.writes;
            assert_eq!(
                q.identify(&mut io, startup(), memory(), Identify::Controller, &mut out),
                Err(Error::State)
            );
            assert_eq!(writes, io.writes);
        }
    }
    #[test]
    fn stale_phase_times_out_and_never_releases_or_retries() {
        let mut io = Fake::new();
        io.timeout = true;
        let mut q = AdminQueue::new();
        let mut out = [0; 4096];
        assert_eq!(
            q.identify(&mut io, startup(), memory(), Identify::Controller, &mut out),
            Err(Error::Timeout)
        );
        assert_eq!(io.now, COMMAND_TIMEOUT_NS);
        assert!(io.cq_heads.is_empty());
    }
    #[test]
    fn invalid_cap_memory_clock_or_fatal_state_do_no_dma_or_doorbell_write() {
        for case in 0..5 {
            let mut io = Fake::new();
            let mut s = startup();
            let mut m = memory();
            match case {
                0 => s.cap |= 1 << 32,
                1 => m.physical = 0xffffe000,
                2 => m.session = 2,
                3 => io.now = u64::MAX - 1,
                _ => io.fatal = true,
            }
            let mut q = AdminQueue::new();
            assert!(q
                .identify(&mut io, s, m, Identify::Controller, &mut [0; 4096])
                .is_err());
            assert_eq!(io.writes, 0);
        }
    }
    fn controller() -> [u8; 4096] {
        let mut b = [0; 4096];
        b[..2].copy_from_slice(&0x144du16.to_le_bytes());
        b[2..4].copy_from_slice(&0x144du16.to_le_bytes());
        b[4..72].fill(b' ');
        b[4..8].copy_from_slice(b"SN42");
        b[24..29].copy_from_slice(b"MODEL");
        b[64..67].copy_from_slice(b"FW1");
        b[77] = 5;
        b[80..84].copy_from_slice(&0x10300u32.to_le_bytes());
        b[516] = 1;
        b[525] = 1;
        b
    }
    #[test]
    fn decode_controller_and_refuse_truncation_unwritten_or_bad_text() {
        let mut b = controller();
        let c = Controller::decode(&b).unwrap();
        assert_eq!(c.vid, 0x144d);
        assert_eq!(c.mdts, 5);
        assert_eq!(c.version, 0x10300);
        assert!(c.vwc);
        assert!(Controller::decode(&b[..4095]).is_err());
        assert!(Controller::decode(&[0; 4096]).is_err());
        b[24] = 0x1b;
        assert!(Controller::decode(&b).is_err());
    }
    #[test]
    fn namespace_list_requires_sorted_unique_nonzero_ids_then_zero_padding() {
        let mut b = [0; 4096];
        assert!(first_namespace(&b).is_err());
        b[..4].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(first_namespace(&b), Ok(7));
        b[8] = 9;
        assert!(first_namespace(&b).is_err());
        b[8] = 0;
        b[4] = 7;
        assert!(first_namespace(&b).is_err());
        b[4] = 9;
        assert_eq!(first_namespace(&b), Ok(7));
        b[4..8].fill(255);
        assert!(first_namespace(&b).is_err());
    }
    #[test]
    fn namespace_geometry_checks_selected_format_capacity_overflow_and_metadata() {
        let mut b = [0; 4096];
        let blocks = 976773168u64;
        b[..8].copy_from_slice(&blocks.to_le_bytes());
        b[8..16].copy_from_slice(&blocks.to_le_bytes());
        b[130] = 9;
        let n = Namespace::decode(1, &b).unwrap();
        assert_eq!(n.bytes, 500107862016);
        assert!(n.plain_blocks());
        b[128] = 8;
        assert!(!Namespace::decode(1, &b).unwrap().plain_blocks());
        b[128] = 0;
        b[29] = 1;
        assert!(!Namespace::decode(1, &b).unwrap().plain_blocks());
        b[29] = 0;
        b[16..24].copy_from_slice(&(blocks + 1).to_le_bytes());
        assert!(Namespace::decode(1, &b).is_err());
        b[16..24].fill(0);
        b[8..16].copy_from_slice(&(blocks + 1).to_le_bytes());
        assert!(Namespace::decode(1, &b).is_err());
        b[8..16].copy_from_slice(&blocks.to_le_bytes());
        b[26] = 1;
        assert!(Namespace::decode(1, &b).is_err());
        b[26] = 0;
        b[130] = 32;
        assert!(Namespace::decode(1, &b).is_err());
        b[130] = 9;
        b[..8].fill(255);
        assert!(Namespace::decode(1, &b).is_err());
        assert!(Namespace::decode(0, &b).is_err());
    }
}
