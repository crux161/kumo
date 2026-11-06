#![no_std]
#![deny(unsafe_code)]
//j798
//j799
//j801
//j802
//! Legacy NVM admin lifecycle, polled Identify (NV3/NV4a), one I/O pair (NV5) and its
//! INTx-driven completions (NV6).
//! Cooperative raw DMA (D5); no SMMUv3 isolation. Pure sequencing over RegisterIo.
//! — TIMBERDOODLE

use core::fmt;

pub const PAGE: u64 = 4096;
/// Admin SQ, admin CQ, data page, I/O SQ, I/O CQ.
pub const DMA_BYTES: u64 = 5 * PAGE;
pub const MMIO_BYTES: u64 = 2 * PAGE;
pub mod admin;
pub mod intx;
pub mod io;
pub const DEPTH: u16 = 32;
pub const BOOT_LEN: usize = 64;
pub const MEMORY_LEN: usize = 32;
pub const PROOF_LEN: usize = 80;
pub const ACK: u8 = b'A';
pub const CC: u16 = 0x14;
pub const CSTS: u16 = 0x1c;
const CC_CONFIG: u32 = (6 << 16) | (4 << 20);
const COMPLETE: u32 = 7; // disabled, ready, then quiesced

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Bootstrap = 1,
    Power = 2,
    Mapping = 3,
    Identity = 4,
    Memory = 5,
    Ack = 6,
    Capability = 7,
    Fatal = 8,
    Timeout = 9,
    Clock = 10,
    Sleep = 11,
    State = 12,
    Programming = 13,
    Completion = 14,
    Command = 15,
    Data = 16,
    Range = 17,
    Interrupt = 18,
    Storm = 19,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bootstrap => "invalid bootstrap",
            Self::Power => "inherit admission refused",
            Self::Mapping => "mapping failed",
            Self::Identity => "CAP/VS changed",
            Self::Memory => "invalid DMA allocation",
            Self::Ack => "DMA custody not acknowledged",
            Self::Capability => "unsupported legacy NVM/4K/depth capability",
            Self::Fatal => "invalid/fatal controller status",
            Self::Timeout => "RDY transition timed out",
            Self::Clock => "clock invalid or polling bound spent",
            Self::Sleep => "poll timer failed",
            Self::State => "shutdown or unsupported controller state",
            Self::Programming => "controller register readback disagrees",
            Self::Completion => "completion identity/queue disagrees",
            Self::Command => "admin command failed",
            Self::Data => "invalid or unsupported Identify data",
            Self::Range => "LBA range outside the namespace or one page",
            Self::Interrupt => "INTx bind/wait/complete failed or a completion had no delivery",
            Self::Storm => "INTx deliveries without progress beyond the storm bound",
        })
    }
}
fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn u64at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
fn put32(b: &mut [u8], i: usize, v: u32) {
    b[i..i + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], i: usize, v: u64) {
    b[i..i + 8].copy_from_slice(&v.to_le_bytes());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Startup {
    pub session: u32,
    pub bar: u64,
    pub cap: u64,
    pub vs: u32,
    pub id: u32,
    pub class_revision: u32,
    pub root_command: u16,
    pub endpoint_command: u16,
    pub msix_control: u16,
    /// INTx (NV6): the root's legacy INTID, the endpoint's Interrupt Pin, and the APB
    /// client's inherited INTR_MASK_LEGACY. KUMO never writes the client mask; INTA
    /// must already be open (bit 0 clear) or the owner refuses. — TIMBERDOODLE
    pub irq: u32,
    pub pin: u8,
    pub legacy_mask: u32,
}
impl Startup {
    pub fn validate(&self) -> Result<(), Error> {
        if self.session == 0
            || self.bar != 0xf000_0000
            || self.class_revision >> 8 != 0x010802
            || matches!(self.id as u16, 0 | 0xffff)
            || self.root_command & 6 != 6
            || self.endpoint_command & 6 != 6
            || self.msix_control & (1 << 15) != 0
            || self.endpoint_command & (1 << 10) != 0
            || self.pin != 1
            || !(32..1020).contains(&self.irq)
            || self.legacy_mask & 1 != 0
        {
            return Err(Error::Bootstrap);
        }
        // Only the observed legacy 1.x NVM/4K case: newer ready modes need a separate proof.
        if self.vs >> 16 != 1
            || self.vs < 0x10100
            || self.cap == 0
            || self.cap == u64::MAX
            || (self.cap >> 32) & 15 != 0
            || self.cap & 0xffff < u64::from(DEPTH - 1)
            || (self.cap >> 37) & 1 == 0
            || (self.cap >> 48) & 15 != 0
        {
            return Err(Error::Capability);
        }
        Ok(())
    }
    pub fn timeout_ns(&self) -> u64 {
        (((self.cap >> 24) & 255) + 1) * 500_000_000
    }
    pub fn encode(&self) -> [u8; BOOT_LEN] {
        let mut b = [0; BOOT_LEN];
        b[..8].copy_from_slice(b"NVM3BOOT");
        put32(&mut b, 8, 2);
        put32(&mut b, 12, self.session);
        put64(&mut b, 16, self.bar);
        put64(&mut b, 24, self.cap);
        put32(&mut b, 32, self.vs);
        put32(&mut b, 36, self.id);
        put32(&mut b, 40, self.class_revision);
        for (i, v) in [
            (44, self.root_command),
            (46, self.endpoint_command),
            (48, self.msix_control),
        ] {
            b[i..i + 2].copy_from_slice(&v.to_le_bytes());
        }
        b[50] = self.pin;
        put32(&mut b, 52, self.irq);
        put32(&mut b, 56, self.legacy_mask);
        b
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != BOOT_LEN
            || &b[..8] != b"NVM3BOOT"
            || u32at(b, 8) != 2
            || b[51] != 0
            || b[60..].iter().any(|&v| v != 0)
        {
            return None;
        }
        let s = Self {
            session: u32at(b, 12),
            bar: u64at(b, 16),
            cap: u64at(b, 24),
            vs: u32at(b, 32),
            id: u32at(b, 36),
            class_revision: u32at(b, 40),
            root_command: u16::from_le_bytes(b[44..46].try_into().ok()?),
            endpoint_command: u16::from_le_bytes(b[46..48].try_into().ok()?),
            msix_control: u16::from_le_bytes(b[48..50].try_into().ok()?),
            pin: b[50],
            irq: u32at(b, 52),
            legacy_mask: u32at(b, 56),
        };
        s.validate().ok()?;
        Some(s)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Memory {
    pub session: u32,
    pub physical: u64,
}
impl Memory {
    pub fn valid(&self) -> bool {
        self.session != 0
            && self.physical != 0
            && self.physical & (PAGE - 1) == 0
            && self
                .physical
                .checked_add(DMA_BYTES)
                .is_some_and(|end| end <= 1 << 32)
    }
    pub fn encode(&self) -> [u8; MEMORY_LEN] {
        let mut b = [0; MEMORY_LEN];
        b[..8].copy_from_slice(b"NV3MEMRY");
        put32(&mut b, 8, 1);
        put32(&mut b, 12, self.session);
        put64(&mut b, 16, self.physical);
        put64(&mut b, 24, DMA_BYTES);
        b
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != MEMORY_LEN
            || &b[..8] != b"NV3MEMRY"
            || u32at(b, 8) != 1
            || u64at(b, 24) != DMA_BYTES
        {
            return None;
        }
        let m = Self {
            session: u32at(b, 12),
            physical: u64at(b, 16),
        };
        m.valid().then_some(m)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Proof {
    pub session: u32,
    pub status: u32,
    pub flags: u32,
    pub sq: u64,
    pub cq: u64,
    pub disable_ns: u64,
    pub enable_ns: u64,
    pub stop_ns: u64,
    pub ready_csts: u32,
    pub final_csts: u32,
}
impl Proof {
    pub fn releases(&self, memory: Memory) -> bool {
        self.complete()
            && memory.valid()
            && self.session == memory.session
            && self.sq == memory.physical
            && self.cq == memory.physical + PAGE
    }
    pub fn complete(&self) -> bool {
        self.status == 0
            && self.flags == COMPLETE
            && self.ready_csts & 3 == 1
            && self.final_csts & 3 == 0
            && Memory {
                session: self.session,
                physical: self.sq,
            }
            .valid()
            && self.cq == self.sq + PAGE
    }
    pub fn encode(&self) -> [u8; PROOF_LEN] {
        let mut b = [0; PROOF_LEN];
        b[..8].copy_from_slice(b"NV3PROOF");
        put32(&mut b, 8, 1);
        put32(&mut b, 12, self.session);
        put32(&mut b, 16, self.status);
        put32(&mut b, 20, self.flags);
        for (i, v) in [
            (24, self.sq),
            (32, self.cq),
            (40, self.disable_ns),
            (48, self.enable_ns),
            (56, self.stop_ns),
        ] {
            put64(&mut b, i, v);
        }
        put32(&mut b, 64, self.ready_csts);
        put32(&mut b, 68, self.final_csts);
        b
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != PROOF_LEN
            || &b[..8] != b"NV3PROOF"
            || u32at(b, 8) != 1
            || b[72..].iter().any(|&v| v != 0)
        {
            return None;
        }
        Some(Self {
            session: u32at(b, 12),
            status: u32at(b, 16),
            flags: u32at(b, 20),
            sq: u64at(b, 24),
            cq: u64at(b, 32),
            disable_ns: u64at(b, 40),
            enable_ns: u64at(b, 48),
            stop_ns: u64at(b, 56),
            ready_csts: u32at(b, 64),
            final_csts: u32at(b, 68),
        })
    }
}
impl fmt::Display for Proof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f,"NVME ADMIN session={} status={} flags={:#x} depth={} sq={:#x} cq={:#x} disable={}us enable={}us stop={}us ready-csts={:#x} final-csts={:#x} {}",
        self.session,self.status,self.flags,DEPTH,self.sq,self.cq,self.disable_ns/1000,self.enable_ns/1000,self.stop_ns/1000,self.ready_csts,self.final_csts,if self.complete(){"OK"}else{"REFUSED; retain DMA"})
    }
}

pub trait RegisterIo {
    fn read32(&mut self, offset: u16) -> u32;
    fn write32(&mut self, offset: u16, value: u32);
    fn barrier(&mut self);
    fn now_ns(&mut self) -> u64;
    fn sleep_ns(&mut self, ns: u64) -> bool;
}
fn start(io: &mut impl RegisterIo) -> Result<u64, Error> {
    let n = io.now_ns();
    if n == u64::MAX {
        Err(Error::Clock)
    } else {
        Ok(n)
    }
}
fn wait(
    io: &mut impl RegisterIo,
    ready: bool,
    begin: u64,
    budget: u64,
) -> Result<(u32, u64), Error> {
    let deadline = begin.checked_add(budget).ok_or(Error::Clock)?;
    let mut delay = 1_000_000;
    for _ in 0..4096 {
        let state = io.read32(CSTS);
        let now = start(io)?;
        if now < begin {
            return Err(Error::Clock);
        }
        if state == u32::MAX || state & 2 != 0 {
            return Err(Error::Fatal);
        }
        if now > deadline {
            return Err(Error::Timeout);
        }
        if (state & 1 != 0) == ready {
            return Ok((state, now - begin));
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
fn write64(io: &mut impl RegisterIo, offset: u16, value: u64) {
    io.write32(offset, value as u32);
    io.write32(offset + 4, (value >> 32) as u32);
}
fn read64(io: &mut impl RegisterIo, offset: u16) -> u64 {
    u64::from(io.read32(offset)) | (u64::from(io.read32(offset + 4)) << 32)
}

/// Caller has zeroed uncached pages and acknowledged parent custody before entry.
/// No command/doorbell exists in this function. Any failure after custody keeps the
/// whole allocation retained; never guess that a timed-out controller stopped. — TIMBERDOODLE
pub fn exercise(io: &mut impl RegisterIo, s: Startup, m: Memory) -> Proof {
    exercise_with(io, s, m, |_| Ok(()))
}

/// Run one ready-state operation before the same proved stop. Operation failure
/// retains the active controller's buffers just like a transition failure. — TIMBERDOODLE
pub fn exercise_with<R: RegisterIo>(
    io: &mut R,
    s: Startup,
    m: Memory,
    operation: impl FnOnce(&mut R) -> Result<(), Error>,
) -> Proof {
    let mut p = Proof {
        session: s.session,
        sq: m.physical,
        cq: m.physical.saturating_add(PAGE),
        ..Proof::default()
    };
    let result = (|| -> Result<(), Error> {
        s.validate()?;
        if !m.valid() || m.session != s.session {
            return Err(Error::Memory);
        }
        let cap = u64::from(io.read32(0)) | (u64::from(io.read32(4)) << 32);
        if cap != s.cap || io.read32(8) != s.vs {
            return Err(Error::Identity);
        }
        let cc = io.read32(CC);
        if cc == u32::MAX || cc & (3 << 14) != 0 {
            return Err(Error::State);
        }
        let budget = s.timeout_ns();
        // Settle an inherited transition before toggling EN; mismatched EN/RDY is
        // not permission to perform the undefined opposite transition (Base p60).
        let initial = start(io)?;
        wait(io, cc & 1 != 0, initial, budget)?;
        let begin = start(io)?;
        if cc & 1 != 0 {
            io.write32(CC, cc & !1);
            io.barrier();
        }
        let (_, elapsed) = wait(io, false, begin, budget)?;
        p.disable_ns = elapsed;
        p.flags |= 1;
        if io.read32(CC) & 1 != 0 {
            return Err(Error::Programming);
        }
        io.write32(0x0c, u32::MAX); // INTMS, only with MSI-X disabled (bootstrap gate).
        io.write32(0x24, u32::from(DEPTH - 1) | (u32::from(DEPTH - 1) << 16));
        write64(io, 0x28, m.physical);
        write64(io, 0x30, m.physical + PAGE);
        io.barrier();
        if io.read32(0x0c) & 1 == 0
            || io.read32(0x24) != (u32::from(DEPTH - 1) | (u32::from(DEPTH - 1) << 16))
            || read64(io, 0x28) != m.physical
            || read64(io, 0x30) != m.physical + PAGE
        {
            return Err(Error::Programming);
        }
        let begin = start(io)?;
        io.write32(CC, CC_CONFIG | 1);
        io.barrier();
        let (state, elapsed) = wait(io, true, begin, budget)?;
        p.enable_ns = elapsed;
        p.ready_csts = state;
        p.flags |= 2;
        if io.read32(CC) != CC_CONFIG | 1 {
            return Err(Error::Programming);
        }
        operation(io)?;
        let begin = start(io)?;
        io.write32(CC, CC_CONFIG);
        io.barrier();
        let (state, elapsed) = wait(io, false, begin, budget)?;
        p.stop_ns = elapsed;
        p.final_csts = state;
        p.flags |= 4;
        if io.read32(CC) & 1 != 0 {
            return Err(Error::Programming);
        }
        Ok(())
    })();
    if let Err(e) = result {
        p.status = e as u32;
    }
    p
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;
    #[test]
    fn ready_operation_runs_before_stop_and_failure_retains_active_queues() {
        let mut io = Fake::new();
        let p = exercise_with(&mut io, startup(), memory(), |io| {
            assert_eq!(io.cc & 1, 1);
            assert_eq!(io.state & 3, 1);
            Err(Error::Data)
        });
        assert_eq!(p.status, Error::Data as u32);
        assert_eq!(p.flags, 3);
        assert!(!p.releases(memory()));
        assert_eq!(io.cc & 1, 1);
        let mut io = Fake::new();
        let p = exercise_with(&mut io, startup(), memory(), |io| {
            assert_eq!(io.state & 3, 1);
            Ok(())
        });
        assert!(p.releases(memory()));
        assert_eq!(io.cc & 1, 0);
    }

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
        }
    }
    fn memory() -> Memory {
        Memory {
            session: 1,
            physical: 0x6000_0000,
        }
    }
    struct Fake {
        cc: u32,
        state: u32,
        now: u64,
        pending: Option<u32>,
        delay: u8,
        stuck: Option<bool>,
        fatal: bool,
        sleep_ok: bool,
        fail_stop: bool,
        drop_queue: bool,
        enables: u8,
        writes: Vec<(u16, u32)>,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                cc: CC_CONFIG | 1,
                state: 1,
                now: 0,
                pending: None,
                delay: 0,
                stuck: None,
                fatal: false,
                sleep_ok: true,
                fail_stop: false,
                drop_queue: false,
                enables: 0,
                writes: Vec::new(),
            }
        }
    }
    impl RegisterIo for Fake {
        fn read32(&mut self, o: u16) -> u32 {
            if self.drop_queue && o == 0x28 {
                return 0;
            }
            match o {
                0 => startup().cap as u32,
                4 => (startup().cap >> 32) as u32,
                8 => startup().vs,
                CC => self.cc,
                CSTS => {
                    if self.delay > 0 {
                        self.delay -= 1;
                    } else if let Some(n) = self.pending.take() {
                        self.state = n;
                    }
                    if self.fatal {
                        2
                    } else {
                        self.state
                    }
                }
                0xc | 0x24 | 0x28 | 0x2c | 0x30 | 0x34 => self
                    .writes
                    .iter()
                    .rev()
                    .find_map(|&(a, v)| (a == o).then_some(v))
                    .unwrap_or(0),
                _ => panic!("unplanned register read {o:#x}"),
            }
        }
        fn write32(&mut self, o: u16, v: u32) {
            assert!(matches!(o, CC | 0x0c | 0x24 | 0x28 | 0x2c | 0x30 | 0x34));
            if o == CC {
                assert_eq!(
                    self.state & 1,
                    self.cc & 1,
                    "EN transition before RDY settled"
                );
                self.cc = v;
                if v & 1 != 0 {
                    self.enables += 1;
                }
                if self.stuck != Some(v & 1 != 0)
                    && !(self.fail_stop && self.enables != 0 && v & 1 == 0)
                {
                    self.pending = Some(v & 1);
                    self.delay = 1;
                }
            } else if o != 0x0c {
                assert_eq!(
                    (self.cc & 1, self.state & 1),
                    (0, 0),
                    "queue write while enabled"
                );
            }
            self.writes.push((o, v));
        }
        fn barrier(&mut self) {}
        fn now_ns(&mut self) -> u64 {
            self.now
        }
        fn sleep_ns(&mut self, n: u64) -> bool {
            self.now += n;
            self.sleep_ok
        }
    }
    #[test]
    fn queue_lifecycle_orders_writes_and_proves_quiescence() {
        let mut io = Fake::new();
        let p = exercise(&mut io, startup(), memory());
        assert!(p.complete(), "{p}");
        assert_eq!(
            io.writes,
            [
                (CC, CC_CONFIG),
                (0xc, u32::MAX),
                (0x24, 0x1f001f),
                (0x28, 0x60000000),
                (0x2c, 0),
                (0x30, 0x60001000),
                (0x34, 0),
                (CC, CC_CONFIG | 1),
                (CC, CC_CONFIG)
            ]
        );
        assert_eq!((io.cc & 1, io.state & 1), (0, 0));
        assert!(p.enable_ns > 0 && p.stop_ns > 0);
    }
    #[test]
    fn restart_from_disabled_does_not_clear_en_again() {
        let mut io = Fake::new();
        let first = exercise(&mut io, startup(), memory());
        assert!(first.complete());
        io.writes.clear();
        assert!(exercise(&mut io, startup(), memory()).complete());
        assert_eq!(io.writes[0], (0xc, u32::MAX));
    }
    #[test]
    fn disabled_ack_is_required_before_any_queue_address() {
        let mut io = Fake::new();
        io.stuck = Some(false);
        let p = exercise(&mut io, startup(), memory());
        assert_eq!(p.status, Error::Timeout as u32);
        assert_eq!(p.flags, 0);
        assert_eq!(io.writes, [(CC, CC_CONFIG)]);
    }
    #[test]
    fn failed_enable_keeps_the_allocation_owned_and_never_guesses_reset() {
        let mut io = Fake::new();
        io.stuck = Some(true);
        let p = exercise(&mut io, startup(), memory());
        assert_eq!(p.status, Error::Timeout as u32);
        assert_eq!(p.flags, 1);
        assert_eq!(io.writes.last(), Some(&(CC, CC_CONFIG | 1)));
        assert!(!p.complete());
    }
    #[test]
    fn inherited_transition_settles_before_clear() {
        let mut io = Fake::new();
        io.state = 0;
        io.pending = Some(1);
        io.delay = 2;
        assert!(exercise(&mut io, startup(), memory()).complete());
    }
    #[test]
    fn bad_capability_custody_or_allocation_never_writes() {
        let mut cases = Vec::new();
        for field in 0..10 {
            let mut s = startup();
            match field {
                0 => s.cap = 0,
                1 => s.vs = 0x20000,
                2 => s.msix_control |= 1 << 15,
                3 => s.endpoint_command = 2,
                4 => s.cap |= 1 << 48,
                5 => s.endpoint_command |= 1 << 10,
                6 => s.pin = 2,
                7 => s.irq = 31,
                8 => s.legacy_mask = 0xffff,
                _ => s.cap &= !0xffff,
            };
            cases.push((s, memory()));
        }
        for phys in [0, 1, 0xffff_f000, u64::MAX] {
            cases.push((
                startup(),
                Memory {
                    physical: phys,
                    ..memory()
                },
            ));
        }
        cases.push((
            startup(),
            Memory {
                session: 2,
                ..memory()
            },
        ));
        for (s, m) in cases {
            let mut io = Fake::new();
            assert!(!exercise(&mut io, s, m).complete());
            assert!(io.writes.is_empty());
        }
    }
    #[test]
    fn fatal_and_timer_failures_refuse_without_faking_release() {
        let mut io = Fake::new();
        io.fatal = true;
        let p = exercise(&mut io, startup(), memory());
        assert_eq!(p.status, Error::Fatal as u32);
        assert!(io.writes.is_empty());
        let mut io = Fake::new();
        io.sleep_ok = false;
        let p = exercise(&mut io, startup(), memory());
        assert_eq!(p.status, Error::Sleep as u32);
        assert!(!p.complete());
    }
    #[test]
    fn messages_are_bounded_and_release_requires_complete_matching_proof() {
        let s = startup();
        assert_eq!(Startup::decode(&s.encode()), Some(s));
        assert_eq!(Memory::decode(&memory().encode()), Some(memory()));
        for (at, value) in [(8, 1), (51, 1), (60, 1), (50, 0)] {
            let mut b = s.encode();
            b[at] = value;
            assert!(Startup::decode(&b).is_none(), "byte {at}");
        }
        assert!(Startup::decode(&s.encode()[..63]).is_none());
        let mut io = Fake::new();
        let p = exercise(&mut io, s, memory());
        assert!(p.releases(memory()));
        assert!(!p.releases(Memory {
            session: 2,
            ..memory()
        }));
        assert!(!p.releases(Memory {
            physical: 0x7000_0000,
            ..memory()
        }));
        assert_eq!(Proof::decode(&p.encode()), Some(p));
        assert!(!Proof { flags: 3, ..p }.complete());
        assert!(!Proof { final_csts: 1, ..p }.complete());
        assert!(!Proof { status: 9, ..p }.complete());
        assert!(Memory {
            physical: 0xffff_b000,
            ..memory()
        }
        .valid());
        assert!(!Memory {
            physical: 0xffff_c000,
            ..memory()
        }
        .valid());
        for extent in [8192, 12288] {
            let mut old = memory().encode();
            put64(&mut old, 24, extent);
            assert!(Memory::decode(&old).is_none());
        }
    }

    #[test]
    fn final_stop_timeout_never_authorizes_custody_release() {
        let mut io = Fake::new();
        io.fail_stop = true;
        let p = exercise(&mut io, startup(), memory());
        assert_eq!(p.status, Error::Timeout as u32);
        assert_eq!(p.flags, 3);
        assert!(!p.releases(memory()));
    }

    #[test]
    fn queue_readback_failure_precedes_enable() {
        let mut io = Fake::new();
        io.drop_queue = true;
        let p = exercise(&mut io, startup(), memory());
        assert_eq!(p.status, Error::Programming as u32);
        assert_eq!(p.flags, 1);
        assert_eq!(io.enables, 0);
    }

    #[test]
    fn clock_overflow_refuses_before_any_write() {
        let mut io = Fake::new();
        io.now = u64::MAX - 1;
        assert_eq!(
            exercise(&mut io, startup(), memory()).status,
            Error::Clock as u32
        );
        assert!(io.writes.is_empty());
    }
}
