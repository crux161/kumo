//j626

//! DESIGN/022 D1d-c2a: retain a zero-MMU-source guard across driver/engine replacement.
//! TRM Part 2 MMU0/1 pp. 1047–1049 supplies these register facts. No MMU register is
//! written. This permits only the current paging-off, zero-mask diagnostic; a real
//! translated fault owner/recovery path remains a separate gate. — KESTREL

use crate::{display_iommu, display_unit, power_unit::PowerIo};
use core::sync::atomic::{AtomicU32, Ordering};

const IRQ: u32 = 188;
const BANKS: [u64; 2] = [0xfdd9_7e00, 0xfdd9_7f00];
const UNREGISTERED: u32 = 0;
const WATCHING: u32 = 1;
const REFUSED: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bank {
    pub status: u32,
    pub fault_address: u32,
    pub raw: u32,
    pub mask: u32,
    pub pending: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NoUnit,
    NotReady,
    MmuSource,
    Paging,
    Quarantined,
}

fn word(io: &mut impl PowerIo, address: u64) -> Result<u32, Error> {
    if !display_unit::UNITS[0].ready(io) {
        return Err(Error::NotReady);
    }
    Ok(io.read(address))
}

fn sample(io: &mut impl PowerIo) -> Result<[Bank; 2], Error> {
    let mut banks = [Bank {
        status: 0,
        fault_address: 0,
        raw: 0,
        mask: 0,
        pending: 0,
    }; 2];
    for (bank, base) in banks.iter_mut().zip(BANKS) {
        *bank = Bank {
            status: word(io, base + 4)?,
            fault_address: word(io, base + 0xc)?,
            raw: word(io, base + 0x14)?,
            mask: word(io, base + 0x1c)?,
            pending: word(io, base + 0x20)?,
        };
    }
    if !display_unit::UNITS[0].ready(io) {
        return Err(Error::NotReady);
    }
    Ok(banks)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::{collections::BTreeMap, vec::Vec};

    const DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    const WORDS: [u64; 10] = [
        0xfdd9_7e04,
        0xfdd9_7e0c,
        0xfdd9_7e14,
        0xfdd9_7e1c,
        0xfdd9_7e20,
        0xfdd9_7f04,
        0xfdd9_7f0c,
        0xfdd9_7f14,
        0xfdd9_7f1c,
        0xfdd9_7f20,
    ];

    #[derive(Default)]
    struct Io {
        regs: BTreeMap<u64, u32>,
        reads: Vec<u64>,
        writes: Vec<(u64, u32)>,
        power_loss_after: Option<u64>,
    }

    impl Io {
        fn mmu_reads(&self) -> Vec<u64> {
            self.reads
                .iter()
                .copied()
                .filter(|p| WORDS.contains(p))
                .collect()
        }
    }

    impl PowerIo for Io {
        fn read(&mut self, address: u64) -> u32 {
            self.reads.push(address);
            let value = *self.regs.get(&address).unwrap_or(&0);
            if self.power_loss_after == Some(address) {
                self.regs.insert(0xfd8d_8150, 1 << 17);
            }
            value
        }

        fn write(&mut self, address: u64, value: u32) {
            self.writes.push((address, value));
        }

        fn now_ns(&mut self) -> u64 {
            panic!("IRQ observation must not poll a timer");
        }
    }

    #[test]
    fn quiet_banks_read_ten_literal_words_and_preserve_stale_fault_metadata() {
        let guard = Guard::new();
        let mut io = Io::default();
        io.regs.insert(0xfdd9_7e04, 0x7f8);
        io.regs.insert(0xfdd9_7f04, 0x18);
        io.regs.insert(0xfdd9_7e0c, 0xdead_0000);
        io.regs.insert(0xfdd9_7f0c, 0xbeef_0000);
        let banks = guard.register(DTB, &mut io, |_| true).unwrap();
        assert_eq!(
            banks[0],
            Bank {
                status: 0x7f8,
                fault_address: 0xdead_0000,
                raw: 0,
                mask: 0,
                pending: 0
            }
        );
        assert_eq!(banks[1].fault_address, 0xbeef_0000);
        assert_eq!(io.mmu_reads(), WORDS);
        io.reads.clear();
        assert!(guard.permits(188, &mut io));
        assert_eq!(io.mmu_reads(), WORDS);
        assert!(io.writes.is_empty());
    }

    #[test]
    fn physical_mask_failure_refuses_before_any_mmu_read_and_cannot_be_reregistered() {
        let guard = Guard::new();
        let mut io = Io::default();
        let mut masked = Vec::new();
        assert_eq!(
            guard.register(DTB, &mut io, |irq| {
                masked.push(irq);
                false
            }),
            Err(Error::NotReady)
        );
        assert_eq!(masked, [188]);
        assert!(io.reads.is_empty());
        assert!(!guard.permits(188, &mut io));
        assert_eq!(
            guard.register(DTB, &mut io, |_| panic!("failed registration retried")),
            Err(Error::Quarantined)
        );
        assert!(io.reads.is_empty() && io.writes.is_empty());
    }

    #[test]
    fn every_raw_mask_and_pending_bit_in_either_bank_refuses() {
        for address in [
            0xfdd9_7e14,
            0xfdd9_7e1c,
            0xfdd9_7e20,
            0xfdd9_7f14,
            0xfdd9_7f1c,
            0xfdd9_7f20,
        ] {
            for bit in 0..32 {
                let guard = Guard::new();
                let mut io = Io::default();
                io.regs.insert(address, 1 << bit);
                assert_eq!(
                    guard.register(DTB, &mut io, |_| true),
                    Err(Error::MmuSource)
                );
                assert!(!guard.permits(188, &mut io));
                assert!(io.writes.is_empty());
            }
        }
    }

    #[test]
    fn paging_or_active_page_fault_in_either_bank_is_not_a_quiet_bypass() {
        for address in [0xfdd9_7e04, 0xfdd9_7f04] {
            for (status, error) in [(0x19, Error::Paging), (0x1a, Error::MmuSource)] {
                let guard = Guard::new();
                let mut io = Io::default();
                io.regs.insert(address, status);
                assert_eq!(guard.register(DTB, &mut io, |_| true), Err(error));
                assert!(io.writes.is_empty());
            }
        }
    }

    #[test]
    fn a_fault_arriving_while_masked_latches_across_quiet_reads_and_reregistration() {
        let guard = Guard::new();
        let mut io = Io::default();
        let bytes = DTB.to_vec();
        guard.register(&bytes, &mut io, |_| true).unwrap();
        drop(bytes);
        assert!(guard.permits(188, &mut io));
        // A later completion gets fresh raw status even though the interrupt mask is zero.
        io.regs.insert(0xfdd9_7f14, 2);
        assert!(!guard.permits(188, &mut io));
        io.regs.clear();
        io.reads.clear();
        assert!(!guard.permits(188, &mut io));
        assert_eq!(
            guard.register(DTB, &mut io, |_| true),
            Err(Error::Quarantined)
        );
        assert!(io.reads.is_empty() && io.writes.is_empty());
    }

    #[test]
    fn unknown_topology_and_unrelated_irqs_do_not_touch_mmu_registers() {
        let guard = Guard::new();
        let mut io = Io::default();
        assert!(guard.permits(188, &mut io));
        assert_eq!(
            guard.register(&[], &mut io, |_| panic!("unknown unit masked a line")),
            Err(Error::NoUnit)
        );
        assert!(io.reads.is_empty());
        guard.register(DTB, &mut io, |_| true).unwrap();
        io.reads.clear();
        io.regs.insert(0xfd8d_8150, 1 << 17);
        for irq in [0, 30, 187, 189, u32::MAX] {
            assert!(guard.permits(irq, &mut io));
        }
        assert!(io.reads.is_empty() && io.writes.is_empty());
        assert!(!guard.permits(188, &mut io));
    }

    #[test]
    fn all_power_guards_and_loss_after_each_word_refuse_without_cleanup() {
        for (address, bit) in [
            (0xfd8d_8150, 17),
            (0xfd8d_810c, 13),
            (0xfd8d_810c, 14),
            (0xfd8d_8118, 13),
            (0xfd8d_8118, 14),
            (0xfd8d_8120, 13),
            (0xfd8d_8120, 14),
        ] {
            let guard = Guard::new();
            let mut io = Io::default();
            io.regs.insert(address, 1 << bit);
            assert_eq!(guard.register(DTB, &mut io, |_| true), Err(Error::NotReady));
            assert!(io.mmu_reads().is_empty() && io.writes.is_empty());
        }
        for (index, address) in WORDS.into_iter().enumerate() {
            let guard = Guard::new();
            let mut io = Io {
                power_loss_after: Some(address),
                ..Io::default()
            };
            assert_eq!(guard.register(DTB, &mut io, |_| true), Err(Error::NotReady));
            assert_eq!(io.mmu_reads(), WORDS[..=index]);
            io.regs.clear();
            io.reads.clear();
            io.power_loss_after = None;
            assert!(!guard.permits(188, &mut io));
            assert!(io.reads.is_empty() && io.writes.is_empty());
        }
    }
}

fn check(banks: &[Bank; 2]) -> Result<(), Error> {
    // Unknown interrupt bits refuse too. An old fault address/direction/bus ID alone
    // is not a live source, and idle/replay-empty is not an IRQ completion proof. — KESTREL
    if banks
        .iter()
        .any(|b| b.raw != 0 || b.mask != 0 || b.pending != 0 || b.status & 2 != 0)
    {
        return Err(Error::MmuSource);
    }
    if banks.iter().any(|b| b.status & 1 != 0) {
        return Err(Error::Paging);
    }
    Ok(())
}

/// Boot-owned reservation, independent of any userspace handle or SyscallEngine.
/// Registration never resets a prior refusal. No public clear/recovery operation.
/// A quiet read cannot prove DMA containment or authorize paging. — KESTREL
pub struct Guard {
    state: AtomicU32,
}

impl Default for Guard {
    fn default() -> Self {
        Self::new()
    }
}

impl Guard {
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(UNREGISTERED),
        }
    }

    pub fn register(
        &self,
        bytes: &[u8],
        io: &mut impl PowerIo,
        mask: impl FnOnce(u32) -> bool,
    ) -> Result<[Bank; 2], Error> {
        display_iommu::discover(bytes).ok_or(Error::NoUnit)?;
        let _ = self.state.compare_exchange(
            UNREGISTERED,
            WATCHING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if self.state.load(Ordering::Acquire) == REFUSED {
            return Err(Error::Quarantined);
        }
        // Reserve/mask the physical line before the first bank read. An unknown
        // topology cannot mask an unrelated source. No MMU mask is written. — KESTREL
        if !mask(IRQ) {
            self.state.store(REFUSED, Ordering::Release);
            return Err(Error::NotReady);
        }
        self.observe(io)
    }

    fn observe(&self, io: &mut impl PowerIo) -> Result<[Bank; 2], Error> {
        if self.state.load(Ordering::Acquire) == REFUSED {
            return Err(Error::Quarantined);
        }
        let result = sample(io).and_then(|banks| {
            check(&banks)?;
            Ok(banks)
        });
        if result.is_err() {
            self.state.store(REFUSED, Ordering::Release);
        }
        // A concurrent refusal wins over this call's earlier quiet observation.
        // Reads remain finite; a fabric access that never returns is not bounded. — KESTREL
        if result.is_ok() && self.state.load(Ordering::Acquire) == REFUSED {
            return Err(Error::Quarantined);
        }
        result
    }

    pub fn permits(&self, irq: u32, io: &mut impl PowerIo) -> bool {
        if irq != IRQ || self.state.load(Ordering::Acquire) == UNREGISTERED {
            return true;
        }
        self.observe(io).is_ok()
    }
}
