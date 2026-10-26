//j631
//j632

//! DESIGN/022: bounded live controller, called only with a published table owner.
//! The kernel DeviceCtx path supplies the retaining root owner. Uncertain outcomes retain
//! ownership and prohibit automatic resume/bypass; a successful detach stays held.
//! TRM MMU0/1 pp1047–1049 and pinned Linux reset/bit31 workaround. — KESTREL

use crate::{
    display_dma,
    display_irq::Guard,
    display_mmu,
    display_register::RegisterIo,
    display_unit,
    rockchip_v2::{DetachedRoot, PublishedRoot},
};
const BANKS: [u64; 2] = [0xfdd9_7e00, 0xfdd9_7f00];
const LIMIT_NS: u64 = 100_000_000;
const MAX_POLLS: usize = 8192;

pub trait ControlIo: RegisterIo {
    fn mask_shared(&mut self) -> bool;
    fn enable_shared(&mut self) -> bool;
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bank {
    pub root: u32,
    pub status: u32,
    pub raw: u32,
    pub mask: u32,
    pub pending: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Diagnostic,
    Live,
    Blocked,
    Quarantined,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Reset,
    Enable,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NoUnit,
    InvalidRoot,
    Epoch,
    State,
    Guard,
    NotReady,
    Irq,
    Dma(display_dma::Error),
    Root([Bank; 2]),
    Fault([Bank; 2]),
    Timeout(Stage, [Bank; 2]),
    Routes,
}
pub struct Activation {
    pub previous: Option<DetachedRoot>,
    pub epoch: u64,
    pub banks: [Bank; 2],
    pub resumed: display_dma::State,
}
/// A single physical controller. No Clone/Drop cleanup; retain this with the owning
/// domain on every error, including any held producer and completed reset witness.
/// The caller owns/cleans every candidate table and VMO pin before entry. — KESTREL
pub struct Controller {
    unit: display_dma::Unit,
    held: Option<display_dma::Stopped>,
    root: u64,
    epoch: u64,
    pending_root: u64,
    detached: Option<DetachedRoot>,
    phase: Phase,
}
impl core::fmt::Debug for Controller {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DisplayController")
            .field("phase", &self.phase)
            .field("root", &self.root)
            .field("epoch", &self.epoch)
            .field("pending_root", &self.pending_root)
            .finish_non_exhaustive()
    }
}
fn ready(io: &mut impl RegisterIo) -> Result<(), Error> {
    display_unit::UNITS[0]
        .ready(io)
        .then_some(())
        .ok_or(Error::NotReady)
}
fn word(io: &mut impl RegisterIo, p: u64) -> Result<u32, Error> {
    ready(io)?;
    Ok(io.read(p))
}
fn write(io: &mut impl RegisterIo, p: u64, v: u32) -> Result<(), Error> {
    ready(io)?;
    io.write(p, v);
    Ok(())
}
fn sample(io: &mut impl RegisterIo) -> Result<[Bank; 2], Error> {
    let mut banks = [Bank {
        root: 0,
        status: 0,
        raw: 0,
        mask: 0,
        pending: 0,
    }; 2];
    for (b, p) in banks.iter_mut().zip(BANKS) {
        *b = Bank {
            root: word(io, p)?,
            status: word(io, p + 4)?,
            raw: word(io, p + 0x14)?,
            mask: word(io, p + 0x1c)?,
            pending: word(io, p + 0x20)?,
        };
    }
    ready(io)?;
    Ok(banks)
}
fn no_fault(banks: [Bank; 2]) -> Result<(), Error> {
    if banks
        .iter()
        .any(|b| b.status & 6 != 0 || b.raw != 0 || b.pending != 0 || b.mask & !3 != 0)
    {
        return Err(Error::Fault(banks));
    }
    Ok(())
}
fn require(banks: [Bank; 2], root: u64, paging: bool, mask: u32) -> Result<(), Error> {
    no_fault(banks)?;
    if banks.iter().any(|b| b.root != root as u32) {
        return Err(Error::Root(banks));
    }
    if banks
        .iter()
        .any(|b| (b.status & 1 != 0) != paging || b.mask != mask)
    {
        return Err(Error::State);
    }
    Ok(())
}
fn wait(io: &mut impl RegisterIo, stage: Stage, root: u64) -> Result<[Bank; 2], Error> {
    let start = io.now_ns();
    let mut last = sample(io)?;
    for _ in 0..MAX_POLLS {
        last = sample(io)?;
        no_fault(last)?;
        if io.now_ns().checked_sub(start).is_none_or(|n| n >= LIMIT_NS) {
            return Err(Error::Timeout(stage, last));
        }
        match stage {
            Stage::Reset => {
                if last
                    .iter()
                    .all(|b| b.root == 0 && b.status & 7 == 0 && b.mask == 0)
                {
                    return Ok(last);
                }
                if last.iter().any(|b| b.root != 0 && b.root != root as u32) {
                    return Err(Error::Root(last));
                }
            }
            Stage::Enable => {
                if last.iter().any(|b| b.root != root as u32 || b.mask != 3) {
                    return Err(Error::Root(last));
                }
                if last.iter().all(|b| b.status & 1 != 0) {
                    return Ok(last);
                }
            }
        }
    }
    Err(Error::Timeout(stage, last))
}
fn force_reset(io: &mut impl RegisterIo, root: u64) -> Result<(), Error> {
    if sample(io)?.iter().any(|b| b.root != root as u32) {
        return Err(Error::State);
    }
    for b in BANKS {
        write(io, b + 8, 6)?;
    }
    io.barrier();
    wait(io, Stage::Reset, root)?;
    Ok(())
}
fn routes(io: &mut impl RegisterIo) -> Result<display_mmu::Routes, Error> {
    let mut result = display_mmu::Routes {
        global: word(io, 0xfdd90024)?,
        clusters: [0; 4],
        esmart: [0; 4],
    };
    for (v, p) in result
        .clusters
        .iter_mut()
        .zip([0xfdd91100, 0xfdd91300, 0xfdd91500, 0xfdd91700])
    {
        *v = word(io, p)?;
    }
    for (v, p) in result
        .esmart
        .iter_mut()
        .zip([0xfdd91808, 0xfdd91a08, 0xfdd91c08, 0xfdd91e08])
    {
        *v = word(io, p)?;
    }
    ready(io)?;
    Ok(result)
}
fn seal(io: &mut impl RegisterIo) -> Result<(), Error> {
    let before = routes(io)?;
    write(io, 0xfdd90024, before.global & !((1 << 31) | (1 << 20)))?;
    for (v, p) in before
        .clusters
        .into_iter()
        .zip([0xfdd91100, 0xfdd91300, 0xfdd91500, 0xfdd91700])
    {
        write(io, p, v & !(1 << 14))?;
    }
    for (v, p) in before
        .esmart
        .into_iter()
        .zip([0xfdd91808, 0xfdd91a08, 0xfdd91c08, 0xfdd91e08])
    {
        write(io, p, v & !4)?;
    }
    io.barrier();
    let after = routes(io)?;
    if after.global != before.global & !((1 << 31) | (1 << 20))
        || after.clusters != before.clusters.map(|v| v & !(1 << 14))
        || after.esmart != before.esmart.map(|v| v & !4)
        || after.check().is_err()
    {
        return Err(Error::Routes);
    }
    Ok(())
}
impl Controller {
    pub fn discover(bytes: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            unit: display_dma::Unit::discover(bytes).map_err(|_| Error::NoUnit)?,
            held: None,
            root: 0,
            epoch: 0,
            pending_root: 0,
            detached: None,
            phase: Phase::Diagnostic,
        })
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn binding(&self) -> (u64, u64) {
        (self.root, self.epoch)
    }
    pub fn next_epoch(&self) -> Result<u64, Error> {
        self.epoch.checked_add(1).ok_or(Error::Epoch)
    }
    fn failed(&mut self, guard: &Guard, io: &mut impl ControlIo) {
        let masked = io.mask_shared();
        guard.fail_control(io, masked);
        self.phase = Phase::Quarantined;
    }
    fn hold(&mut self, io: &mut impl RegisterIo) -> Result<(), Error> {
        if self.held.is_none() {
            self.held = Some(self.unit.stop(io).map_err(Error::Dma)?);
        }
        self.held
            .as_mut()
            .ok_or(Error::State)?
            .check_held(io)
            .map_err(Error::Dma)?;
        Ok(())
    }
    pub fn activate(
        &mut self,
        new_root: PublishedRoot<'_>,
        epoch: u64,
        guard: &Guard,
        io: &mut impl ControlIo,
    ) -> Result<Activation, Error> {
        let address = new_root.physical();
        if address == 0 || address & 4095 != 0 || address >= 1 << 32 || address == self.root {
            return Err(Error::InvalidRoot);
        }
        if epoch != self.next_epoch()? {
            return Err(Error::Epoch);
        }
        if self.phase == Phase::Quarantined || guard.binding() != self.binding() {
            return Err(Error::State);
        }
        if !guard.begin_update(|_| io.mask_shared()) {
            self.phase = Phase::Quarantined;
            return Err(Error::Guard);
        }
        self.pending_root = address;
        let result = self.activate_inner(new_root, epoch, guard, io);
        match result {
            Ok(result) => Ok(result),
            Err(e) => {
                self.failed(guard, io);
                Err(e)
            }
        }
    }
    fn activate_inner(
        &mut self,
        new_root: PublishedRoot<'_>,
        epoch: u64,
        guard: &Guard,
        io: &mut impl ControlIo,
    ) -> Result<Activation, Error> {
        self.hold(io)?;
        let initial = sample(io)?;
        require(
            initial,
            self.root,
            self.root != 0,
            if self.root == 0 { 0 } else { 3 },
        )?;
        if self.root == 0 {
            // A nonzero owned sentinel distinguishes a completed reset from an
            // inaccessible register that merely reads zero. Paging is off. — KESTREL
            for b in BANKS {
                write(io, b, new_root.physical() as u32)?;
            }
            io.barrier();
            require(sample(io)?, new_root.physical(), false, 0)?;
            force_reset(io, new_root.physical())?;
        } else {
            force_reset(io, self.root)?;
            self.detached = Some(DetachedRoot::after_reset(self.root));
        }
        if !guard.claim_update(&new_root, epoch) {
            return Err(Error::Guard);
        }
        for b in BANKS {
            // cfg_mode1 makes held-frame updates immediate. Bit31 is the pinned
            // RK3588 Linux fetch-time-limit workaround (TRM calls it reserved). — KESTREL
            let config = word(io, b + 0x24)? | 0x80000002;
            write(io, b + 0x24, config)?;
            io.barrier();
            if word(io, b + 0x24)? != config {
                return Err(Error::State);
            }
            write(io, b, new_root.physical() as u32)?;
        }
        io.barrier();
        require(sample(io)?, new_root.physical(), false, 0)?;
        for b in BANKS {
            write(io, b + 0x1c, 3)?;
        }
        io.barrier();
        require(sample(io)?, new_root.physical(), false, 3)?;
        for b in BANKS {
            write(io, b + 8, 0)?;
        }
        io.barrier();
        wait(io, Stage::Enable, new_root.physical())?;
        seal(io)?;
        self.held
            .as_mut()
            .ok_or(Error::State)?
            .check_held(io)
            .map_err(Error::Dma)?;
        if !guard.arm(io) {
            return Err(Error::Guard);
        }
        if !io.enable_shared() {
            return Err(Error::Irq);
        }
        let resumed = self
            .held
            .as_mut()
            .ok_or(Error::State)?
            .resume(io)
            .map_err(Error::Dma)?;
        if !guard.finish_live(io) {
            return Err(Error::Guard);
        }
        let banks = sample(io)?;
        require(banks, new_root.physical(), true, 3)?;
        self.root = new_root.physical();
        self.epoch = epoch;
        self.pending_root = 0;
        self.held = None;
        self.phase = Phase::Live;
        Ok(Activation {
            previous: self.detached.take(),
            epoch,
            banks,
            resumed,
        })
    }
    pub fn detach(
        &mut self,
        root: PublishedRoot<'_>,
        epoch: u64,
        guard: &Guard,
        io: &mut impl ControlIo,
    ) -> Result<DetachedRoot, Error> {
        if self.phase != Phase::Live
            || root.physical() != self.root
            || epoch != self.epoch
            || guard.binding() != self.binding()
        {
            return Err(Error::State);
        }
        if !guard.begin_update(|_| io.mask_shared()) {
            self.phase = Phase::Quarantined;
            return Err(Error::Guard);
        }
        let result = (|| {
            self.hold(io)?;
            require(sample(io)?, self.root, true, 3)?;
            force_reset(io, self.root)?;
            let proof = DetachedRoot::after_reset(self.root);
            if !guard.finish_blocked(io) {
                return Err(Error::Guard);
            }
            self.root = 0;
            self.phase = Phase::Blocked;
            Ok(proof)
        })();
        if result.is_err() {
            self.failed(guard, io);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        power_unit::PowerIo,
        rockchip_v2::{OfflineTable, PublishedTable, TableMemory},
    };
    extern crate std;
    use std::{collections::BTreeMap, vec::Vec};
    const DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    struct Memory {
        root: u64,
        page: [u32; 1024],
    }
    impl TableMemory for Memory {
        fn allocate(&mut self) -> Option<u64> {
            Some(self.root)
        }
        fn read(&self, _: u64, i: usize) -> u32 {
            self.page[i]
        }
        fn write(&mut self, _: u64, i: usize, v: u32) {
            self.page[i] = v;
        }
        fn release(&mut self, _: u64) {}
    }
    fn table(root: u64) -> PublishedTable<Memory> {
        OfflineTable::new(Memory {
            root,
            page: [0; 1024],
        })
        .unwrap()
        .publish(|_| {})
    }
    struct Io {
        regs: BTreeMap<u64, u32>,
        writes: Vec<(u64, u32)>,
        events: Vec<&'static str>,
        resets: u32,
        enables: u32,
        ignore: Option<u64>,
        power_after: Option<u64>,
        hold: bool,
        irq: bool,
        now: u64,
        tick: u64,
    }
    impl Io {
        fn new() -> Self {
            let mut regs = BTreeMap::new();
            for p in [0xfdd90c00, 0xfdd90d00, 0xfdd90f00] {
                regs.insert(p, 0x8000000f);
            }
            regs.insert(0xfdd90e00, 0xf);
            regs.insert(0xfdd90024, 0xe0000010);
            regs.insert(0xfdd900cc, 0x41);
            for p in [0xfdd91100, 0xfdd91300, 0xfdd91500, 0xfdd91700] {
                regs.insert(p, 0xabc0c000);
            }
            for p in [0xfdd91808, 0xfdd91a08, 0xfdd91c08, 0xfdd91e08] {
                regs.insert(p, 0x12340004);
            }
            for b in BANKS {
                regs.insert(b + 4, 0x18);
            }
            Self {
                regs,
                writes: Vec::new(),
                events: Vec::new(),
                resets: 3,
                enables: 3,
                ignore: None,
                power_after: None,
                hold: true,
                irq: true,
                now: 0,
                tick: 1000,
            }
        }
        fn resumes(&self) -> usize {
            self.writes
                .iter()
                .filter(|&&(p, v)| p == 0xfdd90e00 && v >> 31 == 0)
                .count()
        }
    }
    impl PowerIo for Io {
        fn read(&mut self, p: u64) -> u32 {
            if (0xfdd90000..0xfdd98000).contains(&p) {
                assert_eq!(
                    self.regs.get(&0xfd8d810c).copied().unwrap_or(0) & 0x6000,
                    0,
                    "VOP read while idle"
                );
            }
            if p == 0xfdd900cc && self.hold && self.regs[&0xfdd90e00] >> 31 != 0 {
                *self.regs.entry(p).or_default() |= 0x40;
            }
            if p == 0xfdd90068 && self.regs[&0xfdd90e00] >> 31 == 0 {
                *self.regs.entry(p).or_default() += 1 << 16;
            }
            *self.regs.get(&p).unwrap_or(&0)
        }
        fn write(&mut self, p: u64, v: u32) {
            self.writes.push((p, v));
            if self.ignore == Some(p) {
                return;
            }
            if p == 0xfd8d810c {
                let old = self.regs.get(&p).copied().unwrap_or(0);
                let value = (old & !(v >> 16)) | (v & (v >> 16));
                for a in [p, 0xfd8d8118, 0xfd8d8120] {
                    self.regs.insert(a, value);
                }
            } else if p == 0xfdd900c4 {
                *self.regs.entry(0xfdd900cc).or_default() &= !(v & (v >> 16));
            } else if let Some(n) = BANKS.iter().position(|b| p == *b + 8) {
                if v == 6 && self.resets & (1 << n) != 0 {
                    for offset in [0, 0x14, 0x1c, 0x20, 0x24] {
                        self.regs.insert(BANKS[n] + offset, 0);
                    }
                    self.regs.insert(BANKS[n] + 4, 0x18);
                    self.events.push("reset");
                }
                if v == 0 && self.enables & (1 << n) != 0 {
                    *self.regs.entry(BANKS[n] + 4).or_default() |= 1;
                    self.events.push("paging");
                }
            } else {
                self.regs.insert(p, v);
            }
            if p == 0xfdd90e00 && v >> 31 == 0 {
                self.events.push("resume");
            }
            if self.power_after == Some(p) {
                self.regs.insert(0xfd8d8150, 1 << 17);
            }
        }
        fn now_ns(&mut self) -> u64 {
            self.now = self.now.wrapping_add(self.tick);
            self.now
        }
    }
    impl RegisterIo for Io {
        fn barrier(&mut self) {}
    }
    impl ControlIo for Io {
        fn mask_shared(&mut self) -> bool {
            self.events.push("mask");
            true
        }
        fn enable_shared(&mut self) -> bool {
            self.events.push("enable");
            self.irq
        }
    }
    fn setup() -> (Controller, Guard, Io) {
        let mut io = Io::new();
        let g = Guard::new();
        g.register(DTB, &mut io, |_| true).unwrap();
        (Controller::discover(DTB).unwrap(), g, io)
    }
    #[test]
    fn publication_both_resets_paging_sealed_routes_and_irq_precede_resume() {
        let (mut c, g, mut io) = setup();
        let a = table(0x1000);
        let activation = c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
        assert!(activation.previous.is_none());
        assert_eq!(activation.epoch, 1);
        assert!(activation
            .banks
            .iter()
            .all(|b| b.root == 0x1000 && b.status & 7 == 1 && b.mask == 3));
        assert_eq!(activation.resumed.ports[2], 0xf);
        assert_eq!(
            io.events,
            ["mask", "reset", "reset", "paging", "paging", "enable", "resume"]
        );
        assert_eq!(io.regs[&0xfdd90024], 0x60000010);
        for p in [0xfdd91100, 0xfdd91300, 0xfdd91500, 0xfdd91700] {
            assert_eq!(io.regs[&p], 0xabc08000);
        }
        for p in [0xfdd91808, 0xfdd91a08, 0xfdd91c08, 0xfdd91e08] {
            assert_eq!(io.regs[&p], 0x12340000);
        }
        for b in BANKS {
            assert_eq!(io.regs[&(b + 0x24)], 0x80000002);
        }
        assert_eq!(c.binding(), (0x1000, 1));
        assert_eq!(g.binding(), c.binding());
        assert!(g.control_allowed() && !g.power_allowed() && g.permits(188, &mut io));
        assert_eq!(c.phase(), Phase::Live);
    }
    #[test]
    fn replacement_reclaims_only_old_root_and_detach_keeps_producer_held() {
        let (mut c, g, mut io) = setup();
        let a = table(0x1000);
        let b = table(0x2000);
        let d = table(0x3000);
        c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
        let change = c.activate(b.root_ref(), 2, &g, &mut io).unwrap();
        assert!(a.reclaim(change.previous.unwrap()).is_ok());
        let proof = c.detach(b.root_ref(), 2, &g, &mut io).unwrap();
        assert!(b.reclaim(proof).is_ok());
        assert_eq!(io.resumes(), 2);
        assert_eq!(c.phase(), Phase::Blocked);
        assert_eq!(c.binding(), (0, 2));
        assert_eq!(g.binding(), c.binding());
        assert!(!g.control_allowed() && !g.power_allowed() && !g.permits(188, &mut io));
        assert_ne!(io.regs[&0xfdd90e00] >> 31, 0);
        let held_writes = io
            .writes
            .iter()
            .filter(|&&(p, v)| p == 0xfdd90e00 && v >> 31 != 0)
            .count();
        c.activate(d.root_ref(), 3, &g, &mut io).unwrap();
        assert_eq!(
            io.writes
                .iter()
                .filter(|&&(p, v)| p == 0xfdd90e00 && v >> 31 != 0)
                .count(),
            held_writes
        );
        assert_eq!(io.resumes(), 3);
        assert_eq!(c.binding(), (0x3000, 3));
    }
    #[test]
    fn failed_live_replacement_retains_old_witness_and_candidate_without_resuming() {
        for reset_completed in [false, true] {
            let (mut c, g, mut io) = setup();
            let a = table(0x1000);
            let b = table(0x2000);
            c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
            if reset_completed {
                io.ignore = Some(BANKS[1] + 0x24);
            } else {
                io.resets = 1;
            }
            assert!(c.activate(b.root_ref(), 2, &g, &mut io).is_err());
            assert_eq!(io.resumes(), 1);
            assert_eq!(c.phase(), Phase::Quarantined);
            assert_eq!(c.binding(), (0x1000, 1));
            assert_eq!(c.pending_root, 0x2000);
            assert_eq!(c.detached.is_some(), reset_completed);
            assert_eq!(
                g.fault().unwrap().epoch,
                if reset_completed { 2 } else { 1 }
            );
            assert!(!g.control_allowed() && !g.power_allowed());
        }
    }
    #[test]
    fn stale_epoch_same_root_and_invalid_root_never_enter_hardware_update() {
        let (mut c, g, mut io) = setup();
        let a = table(0x1000);
        let zero = table(0);
        assert!(matches!(
            c.activate(a.root_ref(), 2, &g, &mut io),
            Err(Error::Epoch)
        ));
        assert!(matches!(
            c.activate(zero.root_ref(), 1, &g, &mut io),
            Err(Error::InvalidRoot)
        ));
        assert!(io.writes.is_empty());
        c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
        let writes = io.writes.clone();
        assert!(matches!(
            c.activate(a.root_ref(), 2, &g, &mut io),
            Err(Error::InvalidRoot)
        ));
        assert!(matches!(
            c.detach(a.root_ref(), 0, &g, &mut io),
            Err(Error::State)
        ));
        assert_eq!(io.writes, writes);
    }
    #[test]
    fn each_partial_reset_enable_and_control_readback_failure_retains_and_never_resumes() {
        for n in 0..2 {
            for stage in 0..6 {
                let (mut c, g, mut io) = setup();
                let a = table(0x1000);
                match stage {
                    0 => io.resets &= !(1 << n),
                    1 => io.enables &= !(1 << n),
                    2 => io.ignore = Some(BANKS[n]),
                    3 => io.ignore = Some(BANKS[n] + 0x24),
                    4 => io.ignore = Some(BANKS[n] + 0x1c),
                    _ => io.power_after = Some(BANKS[n]),
                }
                assert!(
                    c.activate(a.root_ref(), 1, &g, &mut io).is_err(),
                    "bank{n} stage{stage}"
                );
                assert_eq!(io.resumes(), 0);
                assert_eq!(c.phase(), Phase::Quarantined);
                assert_eq!(c.pending_root, 0x1000);
                assert!(!g.control_allowed());
                let fault = g.fault().unwrap();
                assert!(fault.line_masked);
                let writes = io.writes.clone();
                assert!(c.activate(a.root_ref(), 1, &g, &mut io).is_err());
                assert_eq!(io.writes, writes);
                assert_eq!(g.fault(), Some(fault));
            }
        }
    }
    #[test]
    fn route_irq_hold_and_fault_failures_never_resume_or_restore_bypass() {
        for stage in 0..13 {
            let (mut c, g, mut io) = setup();
            let a = table(0x1000);
            match stage {
                0 => io.irq = false,
                1 => io.hold = false,
                2 => {
                    io.regs.insert(BANKS[1] + 0x14, 1);
                }
                3 => io.ignore = Some(0xfdd90024),
                4..=7 => {
                    io.ignore = Some([0xfdd91100, 0xfdd91300, 0xfdd91500, 0xfdd91700][stage - 4])
                }
                8..=11 => {
                    io.ignore = Some([0xfdd91808, 0xfdd91a08, 0xfdd91c08, 0xfdd91e08][stage - 8])
                }
                _ => io.tick = u64::MAX,
            }
            assert!(
                c.activate(a.root_ref(), 1, &g, &mut io).is_err(),
                "stage{stage}"
            );
            assert_eq!(io.resumes(), 0);
            assert_eq!(c.phase(), Phase::Quarantined);
            assert!(!g.control_allowed() && !g.power_allowed());
            assert_eq!(io.events.last(), Some(&"mask"));
            assert!(io
                .writes
                .iter()
                .filter(|&&(p, _)| p == 0xfdd90024)
                .all(|&(_, v)| v & (1 << 31) == 0));
        }
    }
    #[test]
    fn a_live_fault_freezes_epoch_and_blocks_old_or_new_root_operations() {
        let (mut c, g, mut io) = setup();
        let a = table(0x1000);
        let b = table(0x2000);
        c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
        io.regs.insert(BANKS[1] + 0x14, 1);
        io.regs.insert(BANKS[1] + 4, 0x1b);
        io.regs.insert(BANKS[1] + 0xc, 0x70001000);
        let fault = g.poll(27, &mut io, |_| true).unwrap();
        assert_eq!(fault.epoch, 1);
        assert_eq!(fault.banks.unwrap()[1].fault_address, 0x70001000);
        assert!(!g.control_allowed());
        let writes = io.writes.clone();
        assert!(c.activate(b.root_ref(), 2, &g, &mut io).is_err());
        assert_eq!(io.writes, writes);
        assert_eq!(c.phase(), Phase::Quarantined);
        assert!(c.detach(a.root_ref(), 1, &g, &mut io).is_err());
        assert_eq!(g.fault(), Some(fault));
    }
    #[test]
    fn armed_irq_state_excludes_control_and_cannot_become_diagnostic_again() {
        let (_, g, mut io) = setup();
        let a = table(0x1000);
        assert!(g.begin_update(|_| true));
        assert!(!g.claim_update(&a.root_ref(), 0));
        assert!(g.claim_update(&a.root_ref(), 1));
        for b in BANKS {
            io.regs.insert(b, 0x1000);
            io.regs.insert(b + 4, 0x19);
            io.regs.insert(b + 0x1c, 3);
        }
        assert!(g.arm(&mut io));
        assert!(g.permits(188, &mut io));
        assert!(!g.control_allowed() && !g.power_allowed());
        assert!(g.finish_live(&mut io));
        assert!(g.control_allowed());
        assert!(g.begin_update(|_| true));
        assert!(!g.end_update(&mut io));
        assert!(!g.control_allowed());
        assert_eq!(g.fault().unwrap().epoch, 1);
    }
    #[test]
    fn wrong_live_root_or_missing_mask_in_either_bank_latches_refusal() {
        for n in 0..2 {
            for mismatch in [true, false] {
                let (mut c, g, mut io) = setup();
                let a = table(0x1000);
                c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
                io.regs.insert(
                    BANKS[n] + if mismatch { 0 } else { 0x1c },
                    if mismatch { 0x2000 } else { 0 },
                );
                assert!(!g.permits(188, &mut io));
                assert!(!g.control_allowed());
                assert_eq!(
                    g.fault().unwrap().error,
                    if mismatch {
                        crate::display_irq::Error::RootMismatch
                    } else {
                        crate::display_irq::Error::MmuSource
                    }
                );
            }
        }
    }
}
