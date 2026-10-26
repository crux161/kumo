//j628

//! DESIGN/022: owned-directory/reset roundtrip while the supported producer is held.
//! TRM Part2 MMU0/1 pp1047–1049: command6 resets all MMU registers. The pinned
//! J613 primary oracle observes both DTE words returning to zero. Paging stays off.
//! This bypass-only probe is not translated DMA teardown or IRQ ownership. — KESTREL

use crate::{display_dma, display_register::RegisterIo, display_unit, rockchip_v2};
const BANKS: [u64; 2] = [0xfdd9_7e00, 0xfdd9_7f00];
const BUDGET_NS: u64 = 100_000_000;
const MAX_POLLS: usize = 8192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bank {
    pub directory: u32,
    pub status: u32,
    pub raw: u32,
    pub mask: u32,
    pub pending: u32,
}
#[derive(Debug)]
pub struct Report {
    pub detached: rockchip_v2::DetachedRoot,
    pub published: [u32; 2],
    pub reset: [Bank; 2],
    pub stop: display_dma::StopReport,
    pub resumed: display_dma::State,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRoot,
    Dma(display_dma::Error),
    NotReady,
    NotQuiet([Bank; 2]),
    RootMismatch { expected: u32, banks: [Bank; 2] },
    Timeout([Bank; 2]),
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
fn sample(io: &mut impl RegisterIo) -> Result<[Bank; 2], Error> {
    let mut states = [Bank {
        directory: 0,
        status: 0,
        raw: 0,
        mask: 0,
        pending: 0,
    }; 2];
    for (s, base) in states.iter_mut().zip(BANKS) {
        *s = Bank {
            directory: word(io, base)?,
            status: word(io, base + 4)?,
            raw: word(io, base + 0x14)?,
            mask: word(io, base + 0x1c)?,
            pending: word(io, base + 0x20)?,
        };
    }
    ready(io)?;
    Ok(states)
}
fn quiet(states: [Bank; 2]) -> Result<(), Error> {
    if states
        .iter()
        .any(|s| s.status & 7 != 0 || s.raw | s.mask | s.pending != 0)
    {
        return Err(Error::NotQuiet(states));
    }
    Ok(())
}

/// The caller owns/cleans the supplied private directory and all VMO pins BEFORE
/// entry. On every error that publication remains retained; no implicit resume.
/// No userspace is running during this bootstrap-only experiment. — KESTREL
pub fn probe(bytes: &[u8], root: u64, io: &mut impl RegisterIo) -> Result<Report, Error> {
    if root == 0 || root % 4096 != 0 || root >= 1 << 32 {
        return Err(Error::InvalidRoot);
    }
    let unit = display_dma::Unit::discover(bytes).map_err(Error::Dma)?;
    let mut stopped = unit.stop(io).map_err(Error::Dma)?;
    let stop = stopped.status();
    stopped.check_held(io).map_err(Error::Dma)?;
    let initial = sample(io)?;
    quiet(initial)?;
    if initial.iter().any(|s| s.directory != 0) {
        return Err(Error::RootMismatch {
            expected: 0,
            banks: initial,
        });
    }
    // DTE_ADDR is a root-address register, not a valid-tagged directory entry.
    // Below 4 GiB its exact word is the page address; no descriptor flags. — KESTREL
    let directory = root as u32;
    for base in BANKS {
        ready(io)?;
        io.write(base, directory);
    }
    io.barrier();
    let published = sample(io)?;
    quiet(published)?;
    if published.iter().any(|s| s.directory != directory) {
        return Err(Error::RootMismatch {
            expected: directory,
            banks: published,
        });
    }
    stopped.check_held(io).map_err(Error::Dma)?;
    for base in BANKS {
        ready(io)?;
        io.write(base + 8, 6);
    }
    io.barrier();
    let start = io.now_ns();
    let mut last = published;
    for _ in 0..MAX_POLLS {
        last = sample(io)?;
        quiet(last)?;
        if io
            .now_ns()
            .checked_sub(start)
            .is_none_or(|n| n >= BUDGET_NS)
        {
            return Err(Error::Timeout(last));
        }
        if last.iter().all(|s| s.directory == 0) {
            stopped.check_held(io).map_err(Error::Dma)?;
            let detached = rockchip_v2::DetachedRoot::after_reset(root);
            // This probe never enabled translation or changed bypass. The original
            // authorized GOP backing remains; explicitly restore the producer. — KESTREL
            let resumed = stopped.resume(io).map_err(Error::Dma)?;
            return Ok(Report {
                detached,
                published: [directory; 2],
                reset: last,
                stop,
                resumed,
            });
        }
        if last
            .iter()
            .any(|s| s.directory != 0 && s.directory != directory)
        {
            return Err(Error::RootMismatch {
                expected: directory,
                banks: last,
            });
        }
    }
    Err(Error::Timeout(last))
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
    const ROOT: u64 = 0x12345000;
    struct Io {
        regs: BTreeMap<u64, u32>,
        writes: Vec<(u64, u32)>,
        resets: u32,
        roots: u32,
        hold: bool,
        now: u64,
        times: VecDeque<u64>,
        power_after_mmu_write: bool,
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
            for b in BANKS {
                regs.insert(b + 4, 0x18);
            }
            Self {
                regs,
                writes: Vec::new(),
                resets: 3,
                roots: 3,
                hold: true,
                now: 0,
                times: VecDeque::new(),
                power_after_mmu_write: false,
            }
        }
        fn mmu_writes(&self) -> Vec<(u64, u32)> {
            self.writes
                .iter()
                .copied()
                .filter(|(p, _)| *p >= BANKS[0])
                .collect()
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
            if p == 0xfd8d8150 && self.power_after_mmu_write && !self.mmu_writes().is_empty() {
                return 1 << 17;
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
            if p == 0xfd8d810c {
                let old = self.regs.get(&p).copied().unwrap_or(0);
                let value = (old & !(v >> 16)) | (v & (v >> 16));
                for a in [p, 0xfd8d8118, 0xfd8d8120] {
                    self.regs.insert(a, value);
                }
            } else if p == 0xfdd900c4 {
                *self.regs.entry(0xfdd900cc).or_default() &= !(v & (v >> 16));
            } else if let Some(n) = BANKS.iter().position(|b| p == *b || p == *b + 8) {
                if p == BANKS[n] && self.roots & (1 << n) != 0 {
                    self.regs.insert(p, v & !0xfff);
                }
                if p == BANKS[n] + 8 && v == 6 && self.resets & (1 << n) != 0 {
                    self.regs.insert(BANKS[n], 0);
                }
            } else {
                self.regs.insert(p, v);
            }
        }
        fn now_ns(&mut self) -> u64 {
            self.now = self.times.pop_front().unwrap_or(self.now + 1000);
            self.now
        }
    }
    impl RegisterIo for Io {
        fn barrier(&mut self) {}
    }
    #[test]
    fn owned_root_readback_then_both_resets_precede_explicit_resume() {
        let mut io = Io::new();
        let report = probe(DTB, ROOT, &mut io).unwrap();
        assert_eq!(report.published, [0x12345000; 2]);
        assert!(report
            .reset
            .iter()
            .all(|s| s.directory == 0 && s.status == 0x18));
        assert_eq!(report.resumed.ports[2], 0xf);
        assert_eq!(
            io.mmu_writes(),
            [
                (BANKS[0], 0x12345000),
                (BANKS[1], 0x12345000),
                (BANKS[0] + 8, 6),
                (BANKS[1] + 8, 6)
            ]
        );
        let reset = io.writes.iter().rposition(|(_, v)| *v == 6).unwrap();
        let resume = io
            .writes
            .iter()
            .position(|(p, v)| *p == 0xfdd90e00 && *v == 0xf)
            .unwrap();
        assert!(reset < resume);
    }
    #[test]
    fn invalid_root_and_unknown_topology_never_write_hardware() {
        for root in [0, ROOT + 1, 1 << 32, u64::MAX] {
            let mut io = Io::new();
            assert!(matches!(probe(DTB, root, &mut io), Err(Error::InvalidRoot)));
            assert!(io.writes.is_empty());
        }
        let mut io = Io::new();
        assert!(matches!(
            probe(&[], ROOT, &mut io),
            Err(Error::Dma(display_dma::Error::NoUnit))
        ));
        assert!(io.writes.is_empty());
    }
    #[test]
    fn preexisting_root_paging_fault_stall_or_irq_sources_never_change_mmu() {
        for b in BANKS {
            for (off, val) in [
                (0, 0x1001),
                (4, 0x19),
                (4, 0x1a),
                (4, 0x1c),
                (0x14, 1),
                (0x1c, 2),
                (0x20, 1),
            ] {
                let mut io = Io::new();
                io.regs.insert(b + off, val);
                assert!(probe(DTB, ROOT, &mut io).is_err());
                assert!(io.mmu_writes().is_empty());
                assert_eq!(io.regs[&0xfdd90e00], 0x8000000f);
            }
        }
    }
    #[test]
    fn either_unwritable_bank_refuses_without_reset_or_resume() {
        for roots in [0, 1, 2] {
            let mut io = Io::new();
            io.roots = roots;
            assert!(matches!(
                probe(DTB, ROOT, &mut io),
                Err(Error::RootMismatch { .. })
            ));
            assert_eq!(io.mmu_writes().len(), 2);
            assert_eq!(io.regs[&0xfdd90e00], 0x8000000f);
        }
    }
    #[test]
    fn partial_reset_never_returns_a_detached_witness_or_resumes() {
        for resets in [0, 1, 2] {
            let mut io = Io::new();
            io.resets = resets;
            assert!(matches!(probe(DTB, ROOT, &mut io), Err(Error::Timeout(_))));
            assert_eq!(io.regs[&0xfdd90e00], 0x8000000f);
            assert_eq!(io.mmu_writes().len(), 4);
        }
    }
    #[test]
    fn power_loss_after_first_root_write_retains_the_stopped_state() {
        let mut io = Io::new();
        io.power_after_mmu_write = true;
        assert!(matches!(probe(DTB, ROOT, &mut io), Err(Error::NotReady)));
        assert_eq!(io.mmu_writes(), [(BANKS[0], 0x12345000)]);
        assert_eq!(io.regs[&0xfdd90e00], 0x8000000f);
    }
    #[test]
    fn missing_hold_never_exposes_a_root_and_late_reset_cannot_pass() {
        let mut io = Io::new();
        io.hold = false;
        assert!(matches!(
            probe(DTB, ROOT, &mut io),
            Err(Error::Dma(display_dma::Error::Timeout(..)))
        ));
        assert!(io.mmu_writes().is_empty());
        // Three waits (HOLD, bus-idle, bus-open) each consume start/observation,
        // followed by reset start and its deliberately late observation. — KESTREL
        let mut io = Io::new();
        io.times = [0, 1, 2, 3, 4, 5, 6, BUDGET_NS + 6].into_iter().collect();
        assert!(matches!(probe(DTB, ROOT, &mut io), Err(Error::Timeout(_))));
        assert_eq!(io.regs[&0xfdd90e00], 0x8000000f);
    }
}
