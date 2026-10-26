//j627
//j628

//! DESIGN/022: quiesce the supported VP2 scanout, then drain both VOP BIUs.
//! VOP TRM Part2 pp. 863, 870–877, 933: standby completes at fresh display-hold.
//! PMU TRM Part1 BIU idle request/ack/status supplies the whole-bus drain witness.
//! No VOP access occurs while either BIU is idle. No mode/PHY sequence. — KESTREL

use crate::{display_iommu, display_register::RegisterIo, display_unit};
const PORTS: [u64; 4] = [0xfdd9_0c00, 0xfdd9_0d00, 0xfdd9_0e00, 0xfdd9_0f00];
const VP2: u64 = 0xfdd9_0e00;
const HOLD_RAW: u64 = 0xfdd9_00cc;
const HOLD_CLEAR: u64 = 0xfdd9_00c4;
const COUNTER: u64 = 0xfdd9_0068;
const STANDBY: u32 = 1 << 31;
const HOLD: u32 = 1 << 6;
const PWR: u64 = 0xfd8d_8150;
const REQUEST: u64 = 0xfd8d_810c;
const ACK: u64 = 0xfd8d_8118;
const IDLE: u64 = 0xfd8d_8120;
const BUS: u32 = (1 << 13) | (1 << 14);
const BUDGET_NS: u64 = 50_000_000;
const MAX_POLLS: usize = 8192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BusState {
    pub request: u32,
    pub ack: u32,
    pub idle: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StopReport {
    pub bus: BusState,
    pub hold: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct State {
    pub ports: [u32; 4],
    pub hold: u32,
    pub counter: u32,
    pub writeback: u32,
    pub lut: u32,
    pub axi: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Hold,
    BusIdle,
    BusOpen,
    Resume,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NoUnit,
    NotReady,
    Changed,
    NotStopped,
    Unsupported(State),
    Timeout(Stage, [u32; 3]),
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    BusHeld,
    RegistersReady,
    Running,
}

pub struct Unit(());
/// Owning mechanism state, without a memory-release permit. Drop neither resumes
/// nor frees tables/pins. Caller must keep control/clocks and backing on error. — KESTREL
pub struct Stopped {
    before: State,
    bus: BusState,
    hold: u32,
    phase: Phase,
}

fn ready(io: &mut impl RegisterIo) -> Result<(), Error> {
    display_unit::UNITS[0]
        .ready(io)
        .then_some(())
        .ok_or(Error::NotReady)
}
fn word(io: &mut impl RegisterIo, address: u64) -> Result<u32, Error> {
    ready(io)?;
    Ok(io.read(address))
}
fn power(io: &mut impl RegisterIo) -> Result<(), Error> {
    (io.read(PWR) & (1 << 17) == 0)
        .then_some(())
        .ok_or(Error::NotReady)
}
fn bus(io: &mut impl RegisterIo) -> Result<BusState, Error> {
    power(io)?;
    let state = BusState {
        request: io.read(REQUEST),
        ack: io.read(ACK),
        idle: io.read(IDLE),
    };
    power(io)?;
    Ok(state)
}
fn expired(io: &mut impl RegisterIo, start: u64) -> bool {
    io.now_ns()
        .checked_sub(start)
        .is_none_or(|n| n >= BUDGET_NS)
}
fn wait_bus(io: &mut impl RegisterIo, held: bool) -> Result<BusState, Error> {
    let start = io.now_ns();
    let stage = if held { Stage::BusIdle } else { Stage::BusOpen };
    let target = if held { BUS } else { 0 };
    let mut last = [0; 3];
    for _ in 0..MAX_POLLS {
        let s = bus(io)?;
        last = [s.request, s.ack, s.idle];
        if s.request & BUS != target {
            return Err(Error::Changed);
        }
        if expired(io, start) {
            return Err(Error::Timeout(stage, last));
        }
        if s.ack & BUS == target && s.idle & BUS == target {
            return Ok(s);
        }
    }
    Err(Error::Timeout(stage, last))
}
fn supported(s: State) -> bool {
    [0, 1, 3].iter().all(|&p| s.ports[p] & STANDBY != 0)
        && s.ports[2] & ((1 << 30) | STANDBY) == 0
        && s.writeback & 1 == 0
        && s.lut & 1 == 0
        && s.axi & 3 == 0
}
impl Unit {
    pub fn discover(bytes: &[u8]) -> Result<Self, Error> {
        display_iommu::discover(bytes)
            .map(|_| Self(()))
            .ok_or(Error::NoUnit)
    }
    pub fn read(&self, io: &mut impl RegisterIo) -> Result<State, Error> {
        let mut ports = [0; 4];
        for (v, p) in ports.iter_mut().zip(PORTS) {
            *v = word(io, p)?;
        }
        let state = State {
            ports,
            hold: word(io, HOLD_RAW)?,
            counter: (word(io, COUNTER)? >> 16) & 0x1fff,
            writeback: word(io, 0xfdd9_0040)?,
            lut: word(io, 0xfdd9_0024)?,
            axi: word(io, 0xfdd9_0010)?,
        };
        ready(io)?;
        Ok(state)
    }
    pub fn stop(&self, io: &mut impl RegisterIo) -> Result<Stopped, Error> {
        let before = self.read(io)?;
        if !supported(before) {
            return Err(Error::Unsupported(before));
        }
        ready(io)?;
        io.write(HOLD_CLEAR, (HOLD << 16) | HOLD);
        io.barrier();
        if word(io, HOLD_RAW)? & HOLD != 0 {
            return Err(Error::Changed);
        }
        if word(io, VP2)? != before.ports[2] {
            return Err(Error::Changed);
        }
        ready(io)?;
        io.write(VP2, before.ports[2] | STANDBY);
        io.barrier();
        let start = io.now_ns();
        let mut last = [0; 3];
        let mut held = false;
        for _ in 0..MAX_POLLS {
            let control = word(io, VP2)?;
            let hold = word(io, HOLD_RAW)?;
            last = [control, hold, 0];
            if control != before.ports[2] | STANDBY {
                return Err(Error::Changed);
            }
            if expired(io, start) {
                return Err(Error::Timeout(Stage::Hold, last));
            }
            if hold & HOLD != 0 {
                held = true;
                break;
            }
        }
        if !held {
            return Err(Error::Timeout(Stage::Hold, last));
        }
        // No APB/VOP reads beyond this request until both BIUs are reopened.
        // The PMU lies outside those BIUs and remains safe to observe. — KESTREL
        ready(io)?;
        io.write(REQUEST, (BUS << 16) | BUS);
        io.barrier();
        let bus = wait_bus(io, true)?;
        Ok(Stopped {
            before,
            bus,
            hold: last[1],
            phase: Phase::BusHeld,
        })
    }
}
impl Stopped {
    pub fn status(&self) -> StopReport {
        StopReport {
            bus: self.bus,
            hold: self.hold,
        }
    }
    /// Reopen configuration while every producer remains held. This is the window
    /// for a future checked MMU reset/root update, not permission to restart DMA. — KESTREL
    pub fn registers_ready(&mut self, io: &mut impl RegisterIo) -> Result<(), Error> {
        if self.phase == Phase::Running {
            return Err(Error::NotStopped);
        }
        if self.phase == Phase::RegistersReady {
            return ready(io);
        }
        let s = bus(io)?;
        if [s.request, s.ack, s.idle].iter().any(|v| v & BUS != BUS) {
            return Err(Error::Changed);
        }
        power(io)?;
        io.write(REQUEST, BUS << 16);
        io.barrier();
        wait_bus(io, false)?;
        ready(io)?;
        self.phase = Phase::RegistersReady;
        Ok(())
    }
    /// Recheck the held producer after reopening configuration. Callers must retain
    /// exclusive control/clock ownership throughout their register operation. — KESTREL
    pub fn check_held(&mut self, io: &mut impl RegisterIo) -> Result<State, Error> {
        self.registers_ready(io)?;
        let current = Unit(()).read(io)?;
        if current.ports[2] != self.before.ports[2] | STANDBY
            || [0, 1, 3]
                .iter()
                .any(|&p| current.ports[p] != self.before.ports[p])
            || current.writeback & 1 != 0
            || current.lut & 1 != 0
            || current.axi & 3 != 0
        {
            return Err(Error::Changed);
        }
        Ok(current)
    }
    /// Explicit restart only after the owning runtime's root/pin contract permits.
    /// Failure never invokes this automatically or clears a saved standby word. — KESTREL
    pub fn resume(&mut self, io: &mut impl RegisterIo) -> Result<State, Error> {
        let current = self.check_held(io)?;
        ready(io)?;
        io.write(VP2, self.before.ports[2]);
        io.barrier();
        let start = io.now_ns();
        let mut last = [0; 3];
        for _ in 0..MAX_POLLS {
            let control = word(io, VP2)?;
            let counter = (word(io, COUNTER)? >> 16) & 0x1fff;
            last = [control, counter, current.counter];
            if control != self.before.ports[2] {
                return Err(Error::Changed);
            }
            if expired(io, start) {
                return Err(Error::Timeout(Stage::Resume, last));
            }
            if counter != current.counter {
                ready(io)?;
                io.write(HOLD_CLEAR, (HOLD << 16) | HOLD);
                io.barrier();
                self.phase = Phase::Running;
                return Unit(()).read(io);
            }
        }
        Err(Error::Timeout(Stage::Resume, last))
    }
}

pub fn probe(bytes: &[u8], io: &mut impl RegisterIo) -> Result<(State, StopReport, State), Error> {
    let unit = Unit::discover(bytes)?;
    let before = unit.read(io)?;
    let mut stopped = unit.stop(io)?;
    let blocked = stopped.status();
    let resumed = stopped.resume(io)?;
    Ok((before, blocked, resumed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power_unit::PowerIo;
    extern crate std;
    use std::{
        collections::{BTreeMap, VecDeque},
        vec::Vec,
    };
    const DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    struct Io {
        regs: BTreeMap<u64, u32>,
        reads: Vec<u64>,
        writes: Vec<(u64, u32)>,
        hold_works: bool,
        ack: u32,
        idle: u32,
        tick: u64,
        now: u64,
        times: VecDeque<u64>,
        power_after: Option<usize>,
    }
    impl Io {
        fn new() -> Self {
            let mut regs = BTreeMap::new();
            for p in [0xfdd9_0c00, 0xfdd9_0d00, 0xfdd9_0f00] {
                regs.insert(p, 0x8000_0000);
            }
            regs.insert(0xfdd9_0e00, 0xf);
            regs.insert(0xfdd9_00cc, 0x41);
            regs.insert(0xfdd9_0024, 0xe000_0010);
            regs.insert(REQUEST, 1 << 5);
            regs.insert(ACK, 1 << 5);
            regs.insert(IDLE, 1 << 5);
            Self {
                regs,
                reads: Vec::new(),
                writes: Vec::new(),
                hold_works: true,
                ack: BUS,
                idle: BUS,
                tick: 1000,
                now: 0,
                times: VecDeque::new(),
                power_after: None,
            }
        }
    }
    impl PowerIo for Io {
        fn read(&mut self, p: u64) -> u32 {
            if (0xfdd9_0000..0xfdd9_8000).contains(&p)
                && self.regs.get(&REQUEST).copied().unwrap_or(0) & BUS != 0
            {
                panic!("VOP read while its BIU is idle: {p:#x}");
            }
            self.reads.push(p);
            if p == PWR && self.power_after.is_some_and(|n| self.writes.len() >= n) {
                return 1 << 17;
            }
            if p == HOLD_RAW
                && self.hold_works
                && self.regs.get(&VP2).copied().unwrap_or(0) & STANDBY != 0
            {
                *self.regs.entry(p).or_default() |= HOLD;
            }
            if p == COUNTER && self.regs.get(&VP2).copied().unwrap_or(0) & STANDBY == 0 {
                *self.regs.entry(p).or_default() += 1 << 16;
            }
            *self.regs.get(&p).unwrap_or(&0)
        }
        fn write(&mut self, p: u64, v: u32) {
            self.writes.push((p, v));
            if p == HOLD_CLEAR {
                let old = *self.regs.get(&HOLD_RAW).unwrap_or(&0);
                self.regs.insert(HOLD_RAW, old & !(v & (v >> 16)));
            } else if p == REQUEST {
                let mask = v >> 16;
                let old = *self.regs.get(&REQUEST).unwrap_or(&0);
                let requested = (old & !mask) | (v & mask);
                self.regs.insert(REQUEST, requested);
                let target = requested & BUS;
                self.regs.insert(ACK, (1 << 5) | (self.ack & target));
                self.regs.insert(IDLE, (1 << 5) | (self.idle & target));
            } else {
                self.regs.insert(p, v);
            }
        }
        fn now_ns(&mut self) -> u64 {
            self.now = self.times.pop_front().unwrap_or(self.now + self.tick);
            self.now
        }
    }
    impl RegisterIo for Io {
        fn barrier(&mut self) {}
    }

    #[test]
    fn fresh_hold_biu_drain_reopen_and_resume_preserve_other_fields() {
        let mut io = Io::new();
        let (before, blocked, after) = probe(DTB, &mut io).unwrap();
        assert_eq!(blocked.hold & 0x40, 0x40);
        assert_eq!(
            [
                blocked.bus.request & BUS,
                blocked.bus.ack & BUS,
                blocked.bus.idle & BUS
            ],
            [BUS; 3]
        );
        assert_eq!(before.ports, after.ports);
        assert_ne!(before.counter, after.counter);
        assert_eq!(
            io.writes,
            [
                (0xfdd9_00c4, 0x0040_0040),
                (0xfdd9_0e00, 0x8000_000f),
                (0xfd8d_810c, 0x6000_6000),
                (0xfd8d_810c, 0x6000_0000),
                (0xfdd9_0e00, 0xf),
                (0xfdd9_00c4, 0x0040_0040)
            ]
        );
        for p in [REQUEST, ACK, IDLE] {
            assert_eq!(io.regs[&p], 1 << 5);
        }
        assert_eq!(io.regs[&0xfdd9_0024], 0xe000_0010);
    }

    #[test]
    fn unsupported_producers_and_foreign_standby_refuse_before_writes() {
        let unit = Unit::discover(DTB).unwrap();
        for (p, v) in [
            (PORTS[0], 0),
            (PORTS[1], 0),
            (PORTS[3], 0),
            (VP2, 0x8000_000f),
            (VP2, 0x4000_000f),
            (0xfdd9_0040, 1),
            (0xfdd9_0024, 0xe000_0011),
            (0xfdd9_0010, 1),
        ] {
            let mut io = Io::new();
            io.regs.insert(p, v);
            assert!(matches!(unit.stop(&mut io), Err(Error::Unsupported(_))));
            assert!(io.writes.is_empty());
        }
    }

    #[test]
    fn stale_hold_clears_and_missing_new_hold_never_requests_bus_idle_or_resumes() {
        let unit = Unit::discover(DTB).unwrap();
        let mut io = Io::new();
        io.hold_works = false;
        io.times = [0, BUDGET_NS].into_iter().collect();
        assert!(matches!(
            unit.stop(&mut io),
            Err(Error::Timeout(Stage::Hold, _))
        ));
        assert_eq!(io.writes, [(HOLD_CLEAR, 0x0040_0040), (VP2, 0x8000_000f)]);
        assert_eq!(io.regs[&HOLD_RAW] & HOLD, 0);
    }

    #[test]
    fn both_bus_acknowledgements_and_idle_states_are_required_and_never_auto_released() {
        let unit = Unit::discover(DTB).unwrap();
        for (ack, idle) in [(1 << 13, BUS), (BUS, 1 << 14), (0, BUS), (BUS, 0)] {
            let mut io = Io::new();
            io.ack = ack;
            io.idle = idle;
            io.times = [0, 1, 1, BUDGET_NS + 1].into_iter().collect();
            assert!(matches!(
                unit.stop(&mut io),
                Err(Error::Timeout(Stage::BusIdle, _))
            ));
            assert_eq!(io.writes.last(), Some(&(REQUEST, 0x6000_6000)));
            assert_eq!(io.regs[&VP2], 0x8000_000f);
        }
    }

    #[test]
    fn unknown_topology_power_and_post_request_loss_do_no_cleanup() {
        let mut io = Io::new();
        assert_eq!(probe(&[], &mut io), Err(Error::NoUnit));
        assert!(io.reads.is_empty() && io.writes.is_empty());
        let unit = Unit::discover(DTB).unwrap();
        for (p, bit) in [
            (PWR, 17),
            (REQUEST, 13),
            (REQUEST, 14),
            (ACK, 13),
            (ACK, 14),
            (IDLE, 13),
            (IDLE, 14),
        ] {
            let mut io = Io::new();
            io.regs.insert(p, 1 << bit);
            assert!(matches!(unit.stop(&mut io), Err(Error::NotReady)));
            assert!(io.writes.is_empty());
        }
        for n in [2, 3] {
            let mut io = Io::new();
            io.power_after = Some(n);
            assert!(matches!(unit.stop(&mut io), Err(Error::NotReady)));
            assert_eq!(io.writes.len(), n);
            assert_eq!(io.regs[&VP2], 0x8000_000f);
        }
    }

    #[test]
    fn backward_and_stopped_clocks_cannot_approve_or_wait_forever() {
        let unit = Unit::discover(DTB).unwrap();
        let mut io = Io::new();
        io.times = [100, 99].into_iter().collect();
        assert!(matches!(
            unit.stop(&mut io),
            Err(Error::Timeout(Stage::Hold, _))
        ));
        let mut io = Io::new();
        io.hold_works = false;
        io.tick = 0;
        assert!(matches!(
            unit.stop(&mut io),
            Err(Error::Timeout(Stage::Hold, _))
        ));
        assert_eq!(io.writes.len(), 2);
    }

    #[test]
    fn reopening_only_enables_register_access_and_does_not_resume_or_restore_changed_control() {
        let unit = Unit::discover(DTB).unwrap();
        let mut io = Io::new();
        let mut stopped = unit.stop(&mut io).unwrap();
        stopped.registers_ready(&mut io).unwrap();
        assert_eq!(io.regs[&VP2], 0x8000_000f);
        assert_eq!(io.writes.last(), Some(&(REQUEST, 0x6000_0000)));
        io.regs.insert(VP2, 0x8000_000e);
        assert_eq!(stopped.resume(&mut io), Err(Error::Changed));
        assert_eq!(io.regs[&VP2], 0x8000_000e);
        assert!(io.writes.iter().all(|&(p, v)| p != VP2 || v & STANDBY != 0));
    }
}
