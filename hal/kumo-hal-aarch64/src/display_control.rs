//j631
//j632
//j634
//j635

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
    Repair,
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
    Config(u32),
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
fn wait(io: &mut impl RegisterIo, stage: Stage, roots: [u64; 2]) -> Result<[Bank; 2], Error> {
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
                if last
                    .iter()
                    .zip(roots)
                    .any(|(b, root)| b.root != 0 && b.root != root as u32)
                {
                    return Err(Error::Root(last));
                }
            }
            Stage::Repair => return Err(Error::State),
            Stage::Enable => {
                if last
                    .iter()
                    .zip(roots)
                    .any(|(b, root)| b.root != root as u32 || b.mask != 3)
                {
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
fn force_reset(io: &mut impl RegisterIo, roots: [u64; 2]) -> Result<(), Error> {
    if sample(io)?
        .iter()
        .zip(roots)
        .any(|(b, root)| b.root != root as u32)
    {
        return Err(Error::State);
    }
    for b in BANKS {
        write(io, b + 8, 6)?;
    }
    io.barrier();
    wait(io, Stage::Reset, roots)?;
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
            force_reset(io, [new_root.physical(); 2])?;
        } else {
            force_reset(io, [self.root; 2])?;
            self.detached = Some(DetachedRoot::after_reset(self.root));
        }
        self.install(new_root, epoch, guard, io)
    }
    fn install(
        &mut self,
        new_root: PublishedRoot<'_>,
        epoch: u64,
        guard: &Guard,
        io: &mut impl ControlIo,
    ) -> Result<Activation, Error> {
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
        wait(io, Stage::Enable, [new_root.physical(); 2])?;
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
    /// Repair only a stable active READ page fault. Old/candidate tables and pins
    /// must remain owned throughout retry; only later fresh drain/reset releases old.
    /// The kernel separately proves that every candidate mapping is approved GOP. — KESTREL
    pub fn recover(
        &mut self,
        old: PublishedRoot<'_>,
        new: PublishedRoot<'_>,
        epoch: u64,
        guard: &Guard,
        io: &mut impl ControlIo,
    ) -> Result<Activation, Error> {
        let fault = guard.fault().ok_or(Error::Guard)?;
        let banks = fault.banks.ok_or(Error::State)?;
        if self.phase != Phase::Live
            || self.pending_root != 0
            || self.detached.is_some()
            || old.physical() != self.root
            || guard.binding() != self.binding()
            || fault.epoch != self.epoch
            || epoch != self.next_epoch()?
        {
            return Err(Error::State);
        }
        if new.physical() == 0
            || new.physical() & 4095 != 0
            || new.physical() >= 1 << 32
            || new.physical() == self.root
        {
            return Err(Error::InvalidRoot);
        }
        if !banks.iter().any(|b| b.raw & 1 != 0)
            || banks.iter().any(|b| {
                b.status & 5 != 1
                    || b.raw & !1 != 0
                    || b.mask != 3
                    || b.pending & !1 != 0
                    || (b.status & 2 != 0) != (b.raw & 1 != 0)
                    || (b.raw & 1 != 0 && b.status & 0x20 != 0)
            })
        {
            return Err(Error::Fault(banks.map(|b| Bank {
                root: self.root as u32,
                status: b.status,
                raw: b.raw,
                mask: b.mask,
                pending: b.pending,
            })));
        }
        if !guard.begin_recovery(self.epoch, |_| io.mask_shared()) {
            return Err(Error::Guard);
        }
        self.pending_root = new.physical();
        let result = (|| {
            let current = sample(io)?;
            for n in 0..2 {
                let b = current[n];
                let frozen = banks[n];
                if b.root != self.root as u32
                    || b.status & 7 != (frozen.status & 7)
                    || b.raw != frozen.raw
                    || b.mask != 3
                    || b.pending != b.raw
                    || (b.raw & 1 != 0 && (b.status & 0x7e0) != (frozen.status & 0x7e0))
                {
                    return Err(Error::Fault(current));
                }
                if b.raw & 1 != 0 && word(io, BANKS[n] + 0xc)? != frozen.fault_address {
                    return Err(Error::Fault(current));
                }
                let config = word(io, BANKS[n] + 0x24)?;
                if config & 2 == 0 {
                    return Err(Error::Config(config));
                }
            }
            if routes(io)?.check().is_err() {
                return Err(Error::Routes);
            }
            for b in BANKS {
                write(io, b + 0x1c, 0)?;
                write(io, b, new.physical() as u32)?;
            }
            io.barrier();
            // A quiet bank may defer its root latch until the blocked frame advances.
            // Both publications stay owned; the faulting bank must select the repair root.
            // Neither bank's old ownership is released before a fresh drain/full reset. — KESTREL
            let old_root = self.root as u32;
            let owned_roots = |observed: [Bank; 2]| {
                observed.iter().enumerate().all(|(n, b)| {
                    b.root == new.physical() as u32 || (banks[n].raw == 0 && b.root == old_root)
                })
            };
            let selected = sample(io)?;
            if !owned_roots(selected) || selected.iter().any(|b| b.mask != 0) {
                return Err(Error::Root(selected));
            }
            for (n, b) in BANKS.into_iter().enumerate() {
                if banks[n].raw & 1 != 0 {
                    write(io, b + 0x18, 1)?;
                }
                write(io, b + 8, 4)?;
            }
            io.barrier();
            for (n, b) in BANKS.into_iter().enumerate() {
                if banks[n].raw & 1 != 0 {
                    write(io, b + 8, 5)?;
                }
            }
            io.barrier();
            let start = io.now_ns();
            let mut repaired = false;
            for _ in 0..MAX_POLLS {
                let observed = sample(io)?;
                if !owned_roots(observed)
                    || observed.iter().any(|b| {
                        b.status & 5 != 1 || b.mask != 0 || b.raw & !1 != 0 || b.pending != 0
                    })
                {
                    return Err(Error::Fault(observed));
                }
                if io.now_ns().checked_sub(start).is_none_or(|n| n >= LIMIT_NS) {
                    return Err(Error::Timeout(Stage::Repair, observed));
                }
                if observed.iter().all(|b| b.status & 2 == 0 && b.raw == 0) {
                    repaired = true;
                    break;
                }
            }
            if !repaired {
                return Err(Error::Timeout(Stage::Repair, sample(io)?));
            }
            self.hold(io)?;
            let drained = sample(io)?;
            no_fault(drained)?;
            if !owned_roots(drained) || drained.iter().any(|b| b.status & 1 == 0 || b.mask != 0) {
                return Err(Error::Root(drained));
            }
            force_reset(io, drained.map(|b| u64::from(b.root)))?;
            self.detached = Some(DetachedRoot::after_reset(self.root));
            self.install(new, epoch, guard, io)
        })();
        if result.is_err() {
            self.failed(guard, io);
        }
        result
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
            force_reset(io, [self.root; 2])?;
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
        replay: bool,
        defer_quiet_root: bool,
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
                replay: true,
                defer_quiet_root: false,
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
            if let Some(b) = BANKS.iter().find(|b| p == **b + 0x20) {
                return self.regs.get(&(*b + 0x14)).copied().unwrap_or(0)
                    & self.regs.get(&(*b + 0x1c)).copied().unwrap_or(0);
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
            } else if p == 0xfdd900c0 {
                let old = self.regs.get(&p).copied().unwrap_or(0);
                let mask = v >> 16;
                self.regs.insert(p, (old & !mask) | (v & mask));
            } else if p == 0xfdd900c4 {
                for a in [0xfdd900cc, 0xfdd900c8] {
                    *self.regs.entry(a).or_default() &= !(v & (v >> 16));
                }
            } else if let Some(b) = BANKS.iter().find(|b| p == **b + 0x18) {
                *self.regs.entry(*b + 0x14).or_default() &= !v;
            } else if self.defer_quiet_root
                && BANKS
                    .iter()
                    .any(|b| p == *b && self.regs[&(*b + 4)] & 3 == 1)
            {
                // The old quiet root remains reachable until the checked full reset. — KESTREL
            } else if let Some(n) = BANKS.iter().position(|b| p == *b + 8) {
                if v == 5 && self.replay {
                    *self.regs.entry(BANKS[n] + 4).or_default() &= !2;
                    *self.regs.entry(BANKS[n] + 0x14).or_default() &= !1;
                    self.events.push("retry");
                }
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
    fn final_vblank_close_preserves_kernel_mmu_source_and_neighbors_or_frozen_fault() {
        for faulted in [false, true] {
            let (mut c, g, mut io) = setup();
            let a = table(0x1000);
            c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
            io.regs.insert(0xfdd900c0, 9);
            io.regs.insert(0xfdd900c8, 9);
            if faulted {
                io.regs.insert(BANKS[0] + 0x14, 1);
                io.regs.insert(BANKS[0] + 4, 0x1b);
                g.poll(27, &mut io, |_| true);
            }
            let first = g.fault();
            io.events.clear();
            io.writes.clear();
            assert_eq!(g.retire_vblank(&mut io), Some(true));
            assert_eq!(io.regs[&0xfdd900c0], 8);
            assert_eq!(io.regs[&0xfdd900c8], 8);
            assert_eq!(io.events, std::vec!["mask"]);
            assert!(io
                .writes
                .iter()
                .all(|(p, _)| *p == 0xfdd900c0 || *p == 0xfdd900c4));
            assert_eq!(g.fault(), first);
            for b in BANKS {
                assert_eq!(io.regs[&(b + 0x1c)], 3);
            }
        }
        let g = Guard::new();
        let mut io = Io::new();
        assert_eq!(g.retire_vblank(&mut io), None);
        assert!(io.events.is_empty() && io.writes.is_empty());
    }
    #[test]
    fn quiet_orphan_line_stays_owned_by_kernel_and_power_loss_never_enables_it() {
        for power_lost in [false, true] {
            let (mut c, g, mut io) = setup();
            let a = table(0x1000);
            c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
            io.events.clear();
            io.writes.clear();
            io.regs.insert(0xfdd900c0, 1);
            if power_lost {
                io.power_after = Some(0xfdd900c0);
            }
            assert_eq!(g.retire_vblank(&mut io), Some(!power_lost));
            assert_eq!(
                io.events,
                if power_lost {
                    std::vec!["mask"]
                } else {
                    std::vec!["mask", "enable"]
                }
            );
            assert!(io
                .writes
                .iter()
                .all(|(p, _)| *p == 0xfdd900c0 || *p == 0xfdd900c4));
            assert_eq!(g.fault().is_some(), power_lost);
        }
    }
    fn page_fault(g: &Guard, io: &mut Io, n: usize) -> crate::display_irq::Fault {
        io.regs.insert(BANKS[n] + 4, 0x1b);
        io.regs.insert(BANKS[n] + 0x14, 1);
        io.regs.insert(BANKS[n] + 0xc, 0x70000000);
        g.poll(27, io, |_| true).unwrap()
    }
    #[test]
    fn read_fault_repair_retries_with_both_owners_then_resets_before_releasing_old_epoch() {
        for n in 0..2 {
            let (mut c, g, mut io) = setup();
            let a = table(0x1000);
            let b = table(0x2000);
            c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
            page_fault(&g, &mut io, n);
            io.defer_quiet_root = true;
            io.events.clear();
            io.writes.clear();
            let report = c
                .recover(a.root_ref(), b.root_ref(), 2, &g, &mut io)
                .unwrap();
            assert!(a.reclaim(report.previous.unwrap()).is_ok());
            assert_eq!(
                io.events,
                ["mask", "retry", "reset", "reset", "paging", "paging", "enable", "resume"]
            );
            assert_eq!(c.binding(), (0x2000, 2));
            assert_eq!(g.binding(), c.binding());
            assert!(g.fault().is_none());
            assert!(g.control_allowed());
            let next = page_fault(&g, &mut io, n);
            assert_eq!(next.epoch, 2);
        }
    }
    #[test]
    fn failed_retry_or_partial_reset_never_reclaims_resumes_or_overwrites_first_fault() {
        for partial_reset in [false, true] {
            let (mut c, g, mut io) = setup();
            let a = table(0x1000);
            let b = table(0x2000);
            c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
            let first = page_fault(&g, &mut io, 0);
            if partial_reset {
                io.resets = 1;
            } else {
                io.replay = false;
            }
            io.events.clear();
            io.writes.clear();
            assert!(c
                .recover(a.root_ref(), b.root_ref(), 2, &g, &mut io)
                .is_err());
            assert_eq!(c.phase(), Phase::Quarantined);
            assert_eq!(c.pending_root, 0x2000);
            assert_eq!(g.fault(), Some(first));
            assert!(!io.events.contains(&"resume"));
            assert!(!g.control_allowed());
            if !partial_reset {
                assert!(io
                    .writes
                    .iter()
                    .all(|(p, v)| !BANKS.iter().any(|b| *p == *b + 8 && *v == 6)));
            }
        }
    }
    #[test]
    fn changed_fault_metadata_write_fault_and_wrong_epoch_are_rejected_without_repair_writes() {
        for mode in 0..5 {
            let (mut c, g, mut io) = setup();
            let a = table(0x1000);
            let b = table(0x2000);
            c.activate(a.root_ref(), 1, &g, &mut io).unwrap();
            if mode == 1 {
                io.regs.insert(BANKS[0] + 4, 0x3b);
                io.regs.insert(BANKS[0] + 0x14, 1);
                g.poll(27, &mut io, |_| true);
            } else {
                page_fault(&g, &mut io, 0);
            }
            if mode == 0 {
                io.regs.insert(BANKS[0] + 0xc, 0x71000000);
            } else if mode == 3 {
                io.regs.insert(BANKS[1], 0x3000);
            } else if mode == 4 {
                io.ignore = Some(BANKS[0]);
            }
            let first = g.fault();
            io.writes.clear();
            assert!(c
                .recover(
                    a.root_ref(),
                    b.root_ref(),
                    if mode == 2 { 3 } else { 2 },
                    &g,
                    &mut io
                )
                .is_err());
            if mode != 4 {
                assert!(io.writes.is_empty());
            } else {
                assert!(!io.events.contains(&"retry"));
                assert_eq!(c.phase(), Phase::Quarantined);
            }
            assert_eq!(g.fault(), first);
        }
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
