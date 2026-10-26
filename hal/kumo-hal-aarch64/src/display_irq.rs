//j626
//j630

//! DESIGN/022 D1d-c2b: boot-retained first refusal, independent of driver/engine life.
//! Capture both banks on IRQ/timer checks before any Sora borrow, retain the first
//! observation, and gate checked control writes plus every IRQ enable/completion.
//! TRM Part2 MMU0/1 pp1047–1049 supplies register facts. No MMU register is written.
//! This still admits only paging-off/zero-mask diagnostics. A translated root owner,
//! MMU acknowledgement, verified containment and recovery remain later. — KESTREL

use crate::{display_iommu, display_unit, power_unit::PowerIo};
use core::sync::atomic::{AtomicU32, Ordering};

const IRQ: u32 = 188;
const BANKS: [u64; 2] = [0xfdd9_7e00, 0xfdd9_7f00];
const UNREGISTERED: u32 = 0;
const WATCHING: u32 = 1;
const REFUSED: u32 = 2;
const UPDATING: u32 = 3;
const LATCHING: u32 = 4;

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
    Busy,
}

/// Frozen first refusal, retained independently of IRQ handles or any engine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fault {
    pub error: Error,
    pub banks: Option<[Bank; 2]>,
    pub line_masked: bool,
}
impl Fault {
    pub fn records(self) -> [Option<kumo_abi::sys::DeviceCtxInfo>; 2] {
        use kumo_abi::sys::{device_ctx_fault as f, DeviceCtxInfo};
        let reason = match self.error {
            Error::NoUnit => 0,
            Error::NotReady => 1,
            Error::MmuSource => 2,
            Error::Paging => 3,
            Error::Quarantined => 4,
            Error::Busy => 5,
        };
        let base = f::KERNEL_DISPLAY
            | ((reason as u64) << f::REASON_SHIFT)
            | if self.line_masked { f::LINE_MASKED } else { 0 };
        let Some(banks) = self.banks else {
            return [
                Some(DeviceCtxInfo {
                    fault_record: base | (3 << f::BANK_SHIFT),
                    fault_addr: 0,
                }),
                None,
            ];
        };
        core::array::from_fn(|n| {
            let b = banks[n];
            Some(DeviceCtxInfo {
                fault_record: base
                    | f::SNAPSHOT_VALID
                    | b.status as u64
                    | (((b.raw & 3) as u64) << f::IRQ_SHIFT)
                    | ((n as u64) << f::BANK_SHIFT)
                    | if b.raw & !3 != 0 { f::UNKNOWN_RAW } else { 0 }
                    | if b.mask != 0 { f::NONZERO_MASK } else { 0 }
                    | if b.pending != 0 {
                        f::NONZERO_PENDING
                    } else {
                        0
                    },
                fault_addr: b.fault_address as u64,
            })
        })
    }
}

fn inspect(io: &mut impl PowerIo) -> Result<[Bank; 2], (Error, Option<[Bank; 2]>)> {
    let banks = sample(io).map_err(|e| (e, None))?;
    check(&banks).map_err(|e| (e, Some(banks)))?;
    Ok(banks)
}

// A timer may start reading before a source arrives. Re-observe after detection,
// with raw-source words bracketing metadata; a changing/inaccessible observation
// publishes no IOVA instead of presenting an old address as the new fault. — KESTREL
fn capture(io: &mut impl PowerIo) -> Option<[Bank; 2]> {
    let before = [
        word(io, BANKS[0] + 0x14).ok()?,
        word(io, BANKS[1] + 0x14).ok()?,
    ];
    let banks = sample(io).ok()?;
    let after = [
        word(io, BANKS[0] + 0x14).ok()?,
        word(io, BANKS[1] + 0x14).ok()?,
    ];
    if !display_unit::UNITS[0].ready(io)
        || check(&banks).is_ok()
        || (0..2).any(|n| before[n] != banks[n].raw || after[n] != banks[n].raw)
    {
        return None;
    }
    Some(banks)
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
        fault_after_reads: Option<usize>,
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
            if address == 0xfdd97e0c {
                if let Some(n) = self.fault_after_reads.as_mut() {
                    *n -= 1;
                    if *n == 0 {
                        self.fault_after_reads = None;
                        self.regs.insert(0xfdd97e04, 0x1a);
                        self.regs.insert(0xfdd97e0c, 0x60001000);
                        *self.regs.entry(0xfdd97e14).or_default() |= 3;
                    }
                }
            }
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
    #[test]
    fn timer_captures_both_banks_without_an_irq_holder_and_freezes_first_snapshot() {
        use kumo_abi::sys::device_ctx_fault as f;
        let guard = Guard::new();
        let mut io = Io::default();
        guard.register(DTB, &mut io, |_| true).unwrap();
        io.regs.insert(0xfdd97e04, 0x7fa);
        io.regs.insert(0xfdd97e0c, 0x60001000);
        io.regs.insert(0xfdd97e14, 1);
        io.regs.insert(0xfdd97f14, 0x202);
        io.regs.insert(0xfdd97f0c, 0x70002000);
        io.regs.insert(0xfdd97f1c, 3);
        io.regs.insert(0xfdd97f20, 2);
        let mut masks = Vec::new();
        let fault = guard
            .poll(27, &mut io, |irq| {
                masks.push(irq);
                true
            })
            .unwrap();
        assert_eq!(masks, [188]);
        assert_eq!(fault.error, Error::MmuSource);
        assert!(fault.line_masked);
        let records = fault.records();
        let a = records[0].unwrap();
        let b = records[1].unwrap();
        assert_eq!(a.fault_addr, 0x60001000);
        assert_eq!(a.fault_record as u32, 0x7fa);
        assert_eq!((a.fault_record >> f::IRQ_SHIFT) & 3, 1);
        assert_eq!((b.fault_record >> f::IRQ_SHIFT) & 3, 2);
        assert_eq!((b.fault_record >> f::BANK_SHIFT) & 3, 1);
        assert_ne!(b.fault_record & f::UNKNOWN_RAW, 0);
        assert_ne!(b.fault_record & f::NONZERO_MASK, 0);
        assert_ne!(b.fault_record & f::NONZERO_PENDING, 0);
        assert!(!guard.control_allowed());
        let reads = io.reads.len();
        io.regs.clear();
        assert_eq!(
            guard.poll(188, &mut io, |_| panic!("latched source remasked")),
            Some(fault)
        );
        assert!(!guard.permits(188, &mut io));
        assert_eq!(io.reads.len(), reads);
        assert!(io.writes.is_empty());
        assert_eq!(
            guard.register(DTB, &mut io, |_| true),
            Err(Error::Quarantined)
        );
        assert_eq!(guard.fault(), Some(fault));
    }
    #[test]
    fn missing_observation_and_failed_mask_are_explicit_and_do_not_forge_bank_data() {
        use kumo_abi::sys::device_ctx_fault as f;
        let guard = Guard::new();
        let mut io = Io::default();
        guard.register(DTB, &mut io, |_| true).unwrap();
        io.regs.insert(0xfd8d8150, 1 << 17);
        let fault = guard.poll(27, &mut io, |_| false).unwrap();
        assert_eq!(fault.error, Error::NotReady);
        assert!(fault.banks.is_none() && !fault.line_masked);
        let a = fault.records()[0].unwrap();
        assert_eq!(a.fault_addr, 0);
        assert_eq!(a.fault_record & (f::SNAPSHOT_VALID | f::LINE_MASKED), 0);
        assert_eq!((a.fault_record >> f::BANK_SHIFT) & 3, 3);
        assert!(fault.records()[1].is_none());
        assert!(!guard.control_allowed());
        assert!(io.writes.is_empty());
    }
    #[test]
    fn unknown_boards_other_irqs_and_owned_bus_update_do_not_read_mmu() {
        let guard = Guard::new();
        let mut io = Io::default();
        assert!(guard
            .poll(27, &mut io, |_| panic!("unknown mask"))
            .is_none());
        assert!(io.reads.is_empty());
        guard.register(DTB, &mut io, |_| true).unwrap();
        let reads = io.reads.len();
        assert!(guard.poll(33, &mut io, |_| panic!("other mask")).is_none());
        assert_eq!(io.reads.len(), reads);
        assert!(guard.begin_update(|irq| irq == 188));
        assert!(!guard.control_allowed());
        io.regs.insert(0xfd8d810c, 0x6000);
        assert!(guard.poll(27, &mut io, |_| panic!("busy mask")).is_none());
        assert!(!guard.permits(188, &mut io));
        assert_eq!(io.reads.len(), reads);
        io.regs.remove(&0xfd8d810c);
        assert!(guard.end_update(&mut io));
        assert!(guard.control_allowed());
        assert!(guard.fault().is_none());
    }
    #[test]
    fn update_drop_or_failed_final_observation_never_reopens_control() {
        for finish in [false, true] {
            let guard = Guard::new();
            let mut io = Io::default();
            guard.register(DTB, &mut io, |_| true).unwrap();
            assert!(guard.begin_update(|_| true));
            if finish {
                io.regs.insert(0xfdd97f14, 1);
                assert!(!guard.end_update(&mut io));
            } else {
                guard.fail_update();
            }
            assert!(!guard.control_allowed());
            assert!(!guard.begin_update(|_| true));
            assert!(!guard.permits(188, &mut io));
            assert!(guard.fault().is_some());
        }
    }
    #[test]
    fn timer_arrival_reobserves_address_and_torn_capture_never_publishes_old_iova() {
        let guard = Guard::new();
        let mut io = Io::default();
        guard.register(DTB, &mut io, |_| true).unwrap();
        io.regs.insert(0xfdd97e0c, 0xdead0000);
        io.fault_after_reads = Some(1);
        let fault = guard.poll(27, &mut io, |_| true).unwrap();
        assert_eq!(fault.banks.unwrap()[0].fault_address, 0x60001000);
        let guard = Guard::new();
        let mut io = Io::default();
        guard.register(DTB, &mut io, |_| true).unwrap();
        io.regs.insert(0xfdd97e14, 1);
        io.regs.insert(0xfdd97e0c, 0xdead0000);
        io.fault_after_reads = Some(2);
        let fault = guard.poll(27, &mut io, |_| true).unwrap();
        assert!(fault.banks.is_none());
        assert_eq!(fault.records()[0].unwrap().fault_addr, 0);
        assert_eq!(
            fault.records()[0].unwrap().fault_record
                & kumo_abi::sys::device_ctx_fault::SNAPSHOT_VALID,
            0
        );
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
    reason: AtomicU32,
    flags: AtomicU32,
    words: [AtomicU32; 10],
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
            reason: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            words: [const { AtomicU32::new(0) }; 10],
        }
    }
    fn refuse(&self, error: Error, banks: Option<[Bank; 2]>, line_masked: bool) {
        let old = self.state.load(Ordering::Acquire);
        if old == REFUSED || old == LATCHING {
            return;
        }
        if self
            .state
            .compare_exchange(old, LATCHING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let reason = match error {
            Error::NotReady => 1,
            Error::MmuSource => 2,
            Error::Paging => 3,
            Error::Busy => 5,
            _ => 4,
        };
        // LATCHING excludes readers until all frozen words precede the Release
        // publication. A later quiet observation cannot overwrite this cause. — KESTREL
        self.reason.store(reason, Ordering::Relaxed);
        let mut flags = if line_masked { 2 } else { 0 };
        if let Some(banks) = banks {
            flags |= 1;
            for (bank, words) in banks.iter().zip(self.words.chunks_exact(5)) {
                for (word, value) in words.iter().zip([
                    bank.status,
                    bank.fault_address,
                    bank.raw,
                    bank.mask,
                    bank.pending,
                ]) {
                    word.store(value, Ordering::Relaxed);
                }
            }
        }
        self.flags.store(flags, Ordering::Relaxed);
        self.state.store(REFUSED, Ordering::Release);
    }
    pub fn fault(&self) -> Option<Fault> {
        if self.state.load(Ordering::Acquire) != REFUSED {
            return None;
        }
        let flags = self.flags.load(Ordering::Relaxed);
        let error = match self.reason.load(Ordering::Relaxed) {
            1 => Error::NotReady,
            2 => Error::MmuSource,
            3 => Error::Paging,
            5 => Error::Busy,
            _ => Error::Quarantined,
        };
        let banks = (flags & 1 != 0).then(|| {
            core::array::from_fn(|n| {
                let w = &self.words[n * 5..n * 5 + 5];
                Bank {
                    status: w[0].load(Ordering::Relaxed),
                    fault_address: w[1].load(Ordering::Relaxed),
                    raw: w[2].load(Ordering::Relaxed),
                    mask: w[3].load(Ordering::Relaxed),
                    pending: w[4].load(Ordering::Relaxed),
                }
            })
        });
        Some(Fault {
            error,
            banks,
            line_masked: flags & 2 != 0,
        })
    }
    pub fn control_allowed(&self) -> bool {
        matches!(self.state.load(Ordering::Acquire), UNREGISTERED | WATCHING)
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
        if self.state.load(Ordering::Acquire) != WATCHING {
            return Err(Error::Quarantined);
        }
        if !mask(IRQ) {
            self.refuse(Error::NotReady, None, false);
            return Err(Error::NotReady);
        }
        self.observe(io, true)
    }
    fn observe(&self, io: &mut impl PowerIo, masked: bool) -> Result<[Bank; 2], Error> {
        let state = self.state.load(Ordering::Acquire);
        if state == UPDATING {
            return Err(Error::Busy);
        }
        if state != WATCHING {
            return Err(Error::Quarantined);
        }
        match inspect(io) {
            Ok(banks) if self.state.load(Ordering::Acquire) == WATCHING => Ok(banks),
            Ok(_) => Err(Error::Quarantined),
            Err((e, banks)) => {
                self.refuse(e, banks.and_then(|_| capture(io)), masked);
                Err(e)
            }
        }
    }
    pub fn permits(&self, irq: u32, io: &mut impl PowerIo) -> bool {
        if irq != IRQ || self.state.load(Ordering::Acquire) == UNREGISTERED {
            return true;
        }
        self.observe(io, false).is_ok()
    }
    /// Timer checks also run while a driver has the shared line masked. No driver
    /// capability, Sora borrow, allocation, MMU ack or paging change is needed. — KESTREL
    pub fn poll(
        &self,
        irq: u32,
        io: &mut impl PowerIo,
        mask: impl FnOnce(u32) -> bool,
    ) -> Option<Fault> {
        if irq >= 32 && irq != IRQ {
            return None;
        }
        match self.state.load(Ordering::Acquire) {
            UNREGISTERED | UPDATING | LATCHING => return None,
            REFUSED => return self.fault(),
            _ => {}
        }
        if let Err((error, banks)) = inspect(io) {
            let masked = mask(IRQ);
            self.refuse(error, banks.and_then(|_| capture(io)), masked);
        }
        self.fault()
    }
    /// Serialize trusted boot probes with checked control/IRQ operations. Drop or
    /// a failed operation never reopens this state automatically. — KESTREL
    pub fn begin_update(&self, mask: impl FnOnce(u32) -> bool) -> bool {
        if self
            .state
            .compare_exchange(WATCHING, UPDATING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        if !mask(IRQ) {
            self.refuse(Error::NotReady, None, false);
            return false;
        }
        true
    }
    pub fn end_update(&self, io: &mut impl PowerIo) -> bool {
        if self.state.load(Ordering::Acquire) != UPDATING {
            return false;
        }
        match inspect(io) {
            Ok(_) => self
                .state
                .compare_exchange(UPDATING, WATCHING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            Err((e, banks)) => {
                self.refuse(e, banks.and_then(|_| capture(io)), true);
                false
            }
        }
    }
    pub fn fail_update(&self) {
        self.refuse(Error::NotReady, None, true);
    }
}
