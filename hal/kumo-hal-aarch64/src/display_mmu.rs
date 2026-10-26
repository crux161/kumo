//j623
//j624

//! DESIGN/022 D1d-b2h1/h2: bounded stall/drain and VOP routing guards.
//! Facts: RK3588 TRM Part 2, MMU0/1 pp. 1047–1048 and VOP controls
//! pp. 854, 953, 955, plus the pinned J613 Linux register oracle.
//! No GPL implementation is imported. Only ENABLE_STALL is written.
//!
//! Success describes the translators at this instant. It is not VOP-wide DMA
//! containment, a table/backing release permit, detach or invalidation.
//! The routing guard checks all reviewed bypass fields and automatic MMU reset.
//! The caller still needs exclusive root/control ownership, held clocks and the
//! shared fault owner. A readback is not a permanent seal. — KESTREL

use crate::{display_iommu, display_register::RegisterIo, display_unit};

const BANKS: [u64; 2] = [0xfdd9_7e00, 0xfdd9_7f00];
const DIRECTORY: u64 = 0;
const STATUS: u64 = 4;
const COMMAND: u64 = 8;
const RAW_FAULT: u64 = 0x14;
const ENABLE_STALL: u32 = 2;
const PAGING: u32 = 1;
const PAGE_FAULT: u32 = 1 << 1;
const STALL: u32 = 1 << 2;
const IDLE: u32 = 1 << 3;
const REPLAY_EMPTY: u32 = 1 << 4;
// The TRM reserves bit 31; the pinned oracle names a negative stall indication.
// It cannot serve as positive evidence and must be clear on completion. — KESTREL
const STALL_NOT_ACTIVE: u32 = 1 << 31;
const TIMEOUT_NS: u64 = 1_000_000;
const MAX_POLLS: usize = 1024;
const GLOBAL_ROUTE: u64 = 0xfdd9_0024;
const CLUSTER_ROUTES: [u64; 4] = [0xfdd9_1100, 0xfdd9_1300, 0xfdd9_1500, 0xfdd9_1700];
const ESMART_ROUTES: [u64; 4] = [0xfdd9_1808, 0xfdd9_1a08, 0xfdd9_1c08, 0xfdd9_1e08];

/// Unfiltered readback, not an ownership token. Check inactive layers too: a
/// subsequent checked plane enable must not inherit an unnoticed bypass. — KESTREL
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Routes {
    pub global: u32,
    pub clusters: [u32; 4],
    pub esmart: [u32; 4],
}

impl Routes {
    pub fn check(&self) -> Result<(), Error> {
        if self.global & (1 << 31) != 0
            || self.clusters.iter().any(|word| word & (1 << 14) != 0)
            || self.esmart.iter().any(|word| word & (1 << 2) != 0)
        {
            return Err(Error::Bypass);
        }
        // Fault-triggered reset could undo a translated state after observation.
        // Do not rely on a RID threshold or a supposedly harmless reset mode. — KESTREL
        if self.global & (1 << 20) != 0 {
            return Err(Error::AutomaticReset);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BankState {
    pub directory: u32,
    pub status: u32,
    pub faults: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRoot,
    NoUnit,
    NotReady,
    NotTranslated,
    RootMismatch,
    Fault,
    Timeout,
    Bypass,
    AutomaticReset,
}

fn unit(bytes: &[u8]) -> Result<(), Error> {
    display_iommu::discover(bytes)
        .map(|_| ())
        .ok_or(Error::NoUnit)
}

fn ready(io: &mut impl RegisterIo) -> Result<(), Error> {
    display_unit::UNITS[0]
        .ready(io)
        .then_some(())
        .ok_or(Error::NotReady)
}

fn sample(io: &mut impl RegisterIo) -> Result<[BankState; 2], Error> {
    let mut states = [BankState {
        directory: 0,
        status: 0,
        faults: 0,
    }; 2];
    for (state, bank) in states.iter_mut().zip(BANKS) {
        ready(io)?;
        *state = BankState {
            directory: io.read(bank + DIRECTORY),
            status: io.read(bank + STATUS),
            faults: io.read(bank + RAW_FAULT),
        };
    }
    Ok(states)
}

/// Read-only, power/BIU-gated observation. Do not infer DMA containment from a
/// disabled translator or a quiet IRQ. This function issues no command. — KESTREL
pub fn read_state(bytes: &[u8], io: &mut impl RegisterIo) -> Result<[BankState; 2], Error> {
    unit(bytes)?;
    sample(io)
}

fn route_word(io: &mut impl RegisterIo, address: u64) -> Result<u32, Error> {
    ready(io)?;
    Ok(io.read(address))
}

fn routes(io: &mut impl RegisterIo) -> Result<Routes, Error> {
    let mut result = Routes {
        global: route_word(io, GLOBAL_ROUTE)?,
        clusters: [0; 4],
        esmart: [0; 4],
    };
    for (word, address) in result.clusters.iter_mut().zip(CLUSTER_ROUTES) {
        *word = route_word(io, address)?;
    }
    for (word, address) in result.esmart.iter_mut().zip(ESMART_ROUTES) {
        *word = route_word(io, address)?;
    }
    Ok(result)
}

/// Read-only live routing audit under the same topology and PMU/BIU guards.
/// This does not clear a bypass/reset bit or authorize a context. — KESTREL
pub fn read_routes(bytes: &[u8], io: &mut impl RegisterIo) -> Result<Routes, Error> {
    unit(bytes)?;
    routes(io)
}

/// Pure request preflight, not a completed-stall check. Command callers supply
/// their kernel owner's expected root; read-only diagnostics can test a candidate.
/// No observation here is an ownership/release capability. — KESTREL
pub fn check_request(states: &[BankState; 2], root: u64) -> Result<(), Error> {
    if root == 0 || root & 4095 != 0 || root >= 1 << 32 {
        return Err(Error::InvalidRoot);
    }
    if states.iter().any(|b| b.status & PAGING == 0) {
        return Err(Error::NotTranslated);
    }
    if states.iter().any(|b| b.directory != root as u32) {
        return Err(Error::RootMismatch);
    }
    if states
        .iter()
        .any(|b| b.status & PAGE_FAULT != 0 || b.faults != 0)
    {
        return Err(Error::Fault);
    }
    Ok(())
}

fn drained(states: &[BankState; 2]) -> bool {
    states.iter().all(|b| {
        b.status & (PAGING | STALL | IDLE | REPLAY_EMPTY | STALL_NOT_ACTIVE)
            == (PAGING | STALL | IDLE | REPLAY_EMPTY)
    })
}

/// Request both banks to stall and wait for stall, idle and replay-empty in the
/// same fresh sample. Paging must remain enabled at the expected directory.
/// Errors never resume, reset, disable paging, acknowledge faults or release RAM.
/// Keep the owner quarantined on uncertainty. No live DeviceCtx uses this yet. — KESTREL
pub fn stall(bytes: &[u8], root: u64, io: &mut impl RegisterIo) -> Result<[BankState; 2], Error> {
    if root == 0 || root & 4095 != 0 || root >= 1 << 32 {
        return Err(Error::InvalidRoot);
    }
    unit(bytes)?;
    let initial = sample(io)?;
    check_request(&initial, root)?;
    routes(io)?.check()?;
    if drained(&initial) {
        return Ok(initial);
    }
    let start = io.now_ns();
    for bank in BANKS {
        routes(io)?.check()?;
        ready(io)?;
        io.write(bank + COMMAND, ENABLE_STALL);
        io.barrier();
    }
    for _ in 0..MAX_POLLS {
        let states = sample(io)?;
        check_request(&states, root)?;
        routes(io)?.check()?;
        // A late ready sample is still a timeout. The iteration limit also bounds
        // a stopped timer; a backward timer cannot extend the deadline. — KESTREL
        if io
            .now_ns()
            .checked_sub(start)
            .is_none_or(|n| n >= TIMEOUT_NS)
        {
            return Err(Error::Timeout);
        }
        if drained(&states) {
            return Ok(states);
        }
    }
    Err(Error::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power_unit::PowerIo;
    extern crate std;
    use std::{
        collections::{BTreeMap, VecDeque},
        vec,
        vec::Vec,
    };
    const DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    const ROOT: u64 = 0x1234_5000;
    const RUN: BankState = BankState {
        directory: ROOT as u32,
        status: 0x01,
        faults: 0,
    };
    const DRAINED: BankState = BankState {
        directory: ROOT as u32,
        status: 0x1d,
        faults: 0,
    };
    #[derive(Debug, Eq, PartialEq)]
    enum Event {
        Read(u64),
        Write(u64, u32),
        Barrier,
        Clock,
    }
    struct Io {
        regs: BTreeMap<u64, u32>,
        samples: VecDeque<[BankState; 2]>,
        current: [BankState; 2],
        times: VecDeque<u64>,
        last_time: u64,
        power_loss_after_writes: Option<usize>,
        route_after_writes: Option<(usize, u64, u32)>,
        route_after_sample: Option<(usize, u64, u32)>,
        power_loss_after_route: Option<u64>,
        events: Vec<Event>,
        sample_count: usize,
    }
    impl Io {
        fn new(samples: impl IntoIterator<Item = [BankState; 2]>) -> Self {
            Self {
                regs: BTreeMap::new(),
                samples: samples.into_iter().collect(),
                current: [RUN; 2],
                times: VecDeque::new(),
                last_time: 0,
                power_loss_after_writes: None,
                route_after_writes: None,
                route_after_sample: None,
                power_loss_after_route: None,
                events: Vec::new(),
                sample_count: 0,
            }
        }
        fn writes(&self) -> Vec<(u64, u32)> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    Event::Write(p, v) => Some((*p, *v)),
                    _ => None,
                })
                .collect()
        }
        fn mmu_reads(&self) -> usize {
            self.events.iter().filter(|e| matches!(e, Event::Read(p) if BANKS.iter().any(|b| *b <= *p && *p < *b + 0x100))).count()
        }
    }
    impl PowerIo for Io {
        fn read(&mut self, p: u64) -> u32 {
            self.events.push(Event::Read(p));
            if p == 0xfd8d_8150
                && self
                    .power_loss_after_writes
                    .is_some_and(|n| self.writes().len() >= n)
            {
                return 1 << 17;
            }
            if p == BANKS[0] {
                self.sample_count += 1;
                if let Some(next) = self.samples.pop_front() {
                    self.current = next;
                }
            }
            if let Some((n, address, value)) = self.route_after_writes {
                if p == address && self.writes().len() >= n {
                    return value;
                }
            }
            if let Some((n, address, value)) = self.route_after_sample {
                if p == address && self.sample_count >= n {
                    return value;
                }
            }
            for (i, bank) in BANKS.into_iter().enumerate() {
                match p.checked_sub(bank) {
                    Some(0) => return self.current[i].directory,
                    Some(4) => return self.current[i].status,
                    Some(0x14) => return self.current[i].faults,
                    _ => {}
                }
            }
            let value = *self.regs.get(&p).unwrap_or(&0);
            if self.power_loss_after_route == Some(p) {
                self.regs.insert(0xfd8d_8150, 1 << 17);
            }
            value
        }
        fn write(&mut self, p: u64, value: u32) {
            self.events.push(Event::Write(p, value));
        }
        fn now_ns(&mut self) -> u64 {
            self.events.push(Event::Clock);
            if let Some(t) = self.times.pop_front() {
                self.last_time = t;
            }
            self.last_time
        }
    }
    impl RegisterIo for Io {
        fn barrier(&mut self) {
            self.events.push(Event::Barrier);
        }
    }
    fn commands() -> Vec<(u64, u32)> {
        vec![(0xfdd9_7e08, 2), (0xfdd9_7f08, 2)]
    }

    fn route_reads(io: &Io) -> Vec<u64> {
        io.events
            .iter()
            .filter_map(|e| match e {
                Event::Read(p)
                    if *p == GLOBAL_ROUTE
                        || CLUSTER_ROUTES.contains(p)
                        || ESMART_ROUTES.contains(p) =>
                {
                    Some(*p)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn routing_audit_reads_all_nine_literal_control_words_without_writes() {
        let mut io = Io::new([]);
        io.regs.insert(0xfdd9_0024, 0x7c00_0000);
        for (i, p) in [0xfdd9_1100, 0xfdd9_1300, 0xfdd9_1500, 0xfdd9_1700]
            .into_iter()
            .enumerate()
        {
            io.regs.insert(p, 0x1000 + i as u32);
        }
        for (p, v) in [
            (0xfdd9_1808, 1),
            (0xfdd9_1a08, 2),
            (0xfdd9_1c08, 8),
            (0xfdd9_1e08, 0x100),
        ] {
            io.regs.insert(p, v);
        }
        let observed = read_routes(DTB, &mut io).unwrap();
        assert_eq!(
            observed,
            Routes {
                global: 0x7c00_0000,
                clusters: [0x1000, 0x1001, 0x1002, 0x1003],
                esmart: [1, 2, 8, 0x100]
            }
        );
        assert_eq!(observed.check(), Ok(()));
        assert_eq!(
            route_reads(&io),
            [
                0xfdd9_0024,
                0xfdd9_1100,
                0xfdd9_1300,
                0xfdd9_1500,
                0xfdd9_1700,
                0xfdd9_1808,
                0xfdd9_1a08,
                0xfdd9_1c08,
                0xfdd9_1e08
            ]
        );
        assert!(io.writes().is_empty());
    }

    #[test]
    fn global_bypass_never_becomes_safe_by_choosing_a_rid_threshold() {
        for threshold in [0, 1, 31] {
            let mut io = Io::new([[RUN; 2]]);
            io.regs.insert(0xfdd9_0024, 0x8000_0000 | (threshold << 26));
            assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::Bypass));
            assert!(io.writes().is_empty());
        }
    }

    #[test]
    fn every_cluster_and_esmart_bypass_rejects_even_already_stalled_banks() {
        for p in [
            0xfdd9_1100,
            0xfdd9_1300,
            0xfdd9_1500,
            0xfdd9_1700,
            0xfdd9_1808,
            0xfdd9_1a08,
            0xfdd9_1c08,
            0xfdd9_1e08,
        ] {
            for initial in [RUN, DRAINED] {
                let mut io = Io::new([[initial; 2]]);
                io.regs
                    .insert(p, if p < 0xfdd9_1800 { 1 << 14 } else { 1 << 2 });
                assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::Bypass));
                assert!(io.writes().is_empty());
            }
        }
    }

    #[test]
    fn automatic_reset_requires_disable_regardless_of_selected_reset_mode() {
        for mode in 0..4 {
            let mut io = Io::new([[DRAINED; 2]]);
            io.regs.insert(0xfdd9_0024, (1 << 20) | (mode << 24));
            assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::AutomaticReset));
            assert!(io.writes().is_empty());
        }
    }

    #[test]
    fn route_changes_after_one_or_both_requests_never_resume_or_report_success() {
        for (p, value) in [
            (0xfdd9_0024, 1 << 31),
            (0xfdd9_1700, 1 << 14),
            (0xfdd9_1e08, 1 << 2),
        ] {
            for after in [1, 2] {
                let mut io = Io::new([[RUN; 2], [DRAINED; 2]]);
                io.route_after_writes = Some((after, p, value));
                assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::Bypass));
                assert_eq!(io.writes(), commands()[..after]);
            }
        }
        let mut io = Io::new([[RUN; 2], [RUN; 2], [DRAINED; 2]]);
        io.route_after_sample = Some((3, 0xfdd9_0024, 1 << 20));
        assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::AutomaticReset));
        assert_eq!(io.sample_count, 3);
        assert_eq!(io.writes(), commands());
    }

    #[test]
    fn saved_readback_cannot_substitute_for_fresh_route_preflight() {
        let mut io = Io::new([[RUN; 2]]);
        let old = read_routes(DTB, &mut io).unwrap();
        assert_eq!(old.check(), Ok(()));
        io.regs.insert(0xfdd9_1e08, 1 << 2);
        assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::Bypass));
        assert!(io.writes().is_empty());
    }

    #[test]
    fn routing_reads_stop_at_power_loss_and_unknown_topology_has_no_io() {
        let mut io = Io::new([]);
        assert_eq!(read_routes(&[], &mut io), Err(Error::NoUnit));
        assert!(io.events.is_empty());
        io.regs.insert(0xfd8d_8150, 1 << 17);
        assert_eq!(read_routes(DTB, &mut io), Err(Error::NotReady));
        assert!(route_reads(&io).is_empty());
        let mut io = Io::new([]);
        io.power_loss_after_route = Some(0xfdd9_0024);
        assert_eq!(read_routes(DTB, &mut io), Err(Error::NotReady));
        assert_eq!(route_reads(&io), [0xfdd9_0024]);
        assert!(io.writes().is_empty());
    }

    #[test]
    fn malformed_requests_and_unknown_units_perform_no_io() {
        for root in [0, 1, ROOT + 1, 1 << 32, u64::MAX] {
            let mut io = Io::new([]);
            assert_eq!(stall(DTB, root, &mut io), Err(Error::InvalidRoot));
            assert!(io.events.is_empty());
        }
        let mut io = Io::new([]);
        assert_eq!(stall(&[], ROOT, &mut io), Err(Error::NoUnit));
        assert_eq!(read_state(&[], &mut io), Err(Error::NoUnit));
        assert!(io.events.is_empty());
    }

    #[test]
    fn power_and_biu_guards_precede_all_mmu_access() {
        for (p, bit) in [
            (0xfd8d_8150, 17),
            (0xfd8d_810c, 13),
            (0xfd8d_810c, 14),
            (0xfd8d_8118, 13),
            (0xfd8d_8118, 14),
            (0xfd8d_8120, 13),
            (0xfd8d_8120, 14),
        ] {
            let mut io = Io::new([]);
            io.regs.insert(p, 1 << bit);
            assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::NotReady));
            assert_eq!(read_state(DTB, &mut io), Err(Error::NotReady));
            assert_eq!(io.mmu_reads(), 0);
            assert!(io.writes().is_empty());
        }
    }

    #[test]
    fn disabled_paging_even_with_stale_stall_idle_bits_is_not_containment() {
        for bank in 0..2 {
            for status in [0, 0x18, 0x1c] {
                let mut states = [DRAINED; 2];
                states[bank].status = status;
                let mut io = Io::new([states]);
                assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::NotTranslated));
                assert_eq!(check_request(&states, ROOT), Err(Error::NotTranslated));
                assert!(io.writes().is_empty());
            }
        }
    }

    #[test]
    fn both_root_and_fault_preflights_finish_before_either_command() {
        for bank in 0..2 {
            for (directory, status, faults, error) in [
                (ROOT as u32 + 4096, 1, 0, Error::RootMismatch),
                (ROOT as u32, 3, 0, Error::Fault),
                (ROOT as u32, 1, 1, Error::Fault),
                (ROOT as u32, 1, 2, Error::Fault),
                (ROOT as u32, 1, 0x8000_0000, Error::Fault),
            ] {
                let mut states = [RUN; 2];
                states[bank] = BankState {
                    directory,
                    status,
                    faults,
                };
                let mut io = Io::new([states]);
                assert_eq!(stall(DTB, ROOT, &mut io), Err(error));
                assert!(io.writes().is_empty());
            }
        }
    }

    #[test]
    fn successful_stall_writes_only_the_two_exact_commands_with_barriers() {
        let mut io = Io::new([[RUN; 2], [DRAINED, RUN], [DRAINED; 2]]);
        io.times = [0, 100, 200].into_iter().collect();
        assert_eq!(stall(DTB, ROOT, &mut io), Ok([DRAINED; 2]));
        assert_eq!(io.writes(), commands());
        for (i, event) in io.events.iter().enumerate() {
            if matches!(event, Event::Write(_, _)) {
                assert_eq!(io.events[i + 1], Event::Barrier);
            }
        }
        assert_eq!(io.sample_count, 3);
    }

    #[test]
    fn already_drained_translation_and_read_only_observation_issue_no_commands() {
        let mut io = Io::new([[DRAINED; 2]]);
        assert_eq!(stall(DTB, ROOT, &mut io), Ok([DRAINED; 2]));
        assert!(io.writes().is_empty());
        assert!(!io.events.contains(&Event::Clock));
        let mut io = Io::new([[RUN; 2]]);
        assert_eq!(read_state(DTB, &mut io), Ok([RUN; 2]));
        assert_eq!(io.mmu_reads(), 6);
        assert!(io.writes().is_empty());
    }

    #[test]
    fn each_bank_requires_stall_idle_empty_replay_and_no_negative_indication() {
        for bank in 0..2 {
            for status in [0x01, 0x05, 0x0d, 0x15, 0x8000_001d] {
                let mut partial = [DRAINED; 2];
                partial[bank].status = status;
                let mut io = Io::new([[RUN; 2], partial]);
                assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::Timeout));
                assert_eq!(io.writes(), commands());
                assert_eq!(io.sample_count, MAX_POLLS + 1);
            }
        }
    }

    #[test]
    fn acknowledgements_from_different_samples_cannot_be_combined() {
        let samples = core::iter::once([RUN; 2]).chain((0..MAX_POLLS).map(|i| {
            if i % 2 == 0 {
                [DRAINED, RUN]
            } else {
                [RUN, DRAINED]
            }
        }));
        let mut io = Io::new(samples);
        assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::Timeout));
        assert_eq!(io.sample_count, MAX_POLLS + 1);
        assert_eq!(io.writes(), commands());
    }

    #[test]
    fn deadline_backward_and_stopped_clocks_are_bounded_and_never_resume() {
        for times in [[0, 1_000_000], [100, 99]] {
            let mut io = Io::new([[RUN; 2], [DRAINED; 2]]);
            io.times = times.into_iter().collect();
            assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::Timeout));
            assert_eq!(io.writes(), commands());
            assert_eq!(io.sample_count, 2);
        }
        let mut io = Io::new([[RUN; 2]]);
        assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::Timeout));
        assert_eq!(io.sample_count, MAX_POLLS + 1);
        assert_eq!(io.writes(), commands());
    }

    #[test]
    fn root_paging_or_fault_change_after_requests_retains_uncertainty() {
        for (directory, status, faults, error) in [
            (ROOT as u32 + 4096, 0x1d, 0, Error::RootMismatch),
            (ROOT as u32, 0x1c, 0, Error::NotTranslated),
            (ROOT as u32, 0x1f, 0, Error::Fault),
            (ROOT as u32, 0x1d, 2, Error::Fault),
        ] {
            let mut changed = [DRAINED; 2];
            changed[1] = BankState {
                directory,
                status,
                faults,
            };
            let mut io = Io::new([[RUN; 2], changed]);
            assert_eq!(stall(DTB, ROOT, &mut io), Err(error));
            assert_eq!(io.writes(), commands());
        }
    }

    #[test]
    fn power_loss_after_one_or_both_requests_stops_access_without_cleanup() {
        for count in [1, 2] {
            let mut io = Io::new([[RUN; 2], [DRAINED; 2]]);
            io.power_loss_after_writes = Some(count);
            assert_eq!(stall(DTB, ROOT, &mut io), Err(Error::NotReady));
            assert_eq!(io.writes(), commands()[..count]);
            assert_eq!(io.mmu_reads(), 6);
        }
    }
}
