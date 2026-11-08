//j833
//j834
extern crate std;
use super::*;
use std::string::{String, ToString};
use std::vec::Vec;

const TICK: u64 = 100_000_000;
const TRANSFER: u64 = 500_000;

#[derive(Clone, Copy)]
struct Partner {
    physr: u16,
    anlpar: u16,
    gbsr: u16,
}

const GIGA: Partner = Partner {
    physr: 0xa400,
    anlpar: 0xc5e1,
    gbsr: 0x3c00,
};
const FAST: Partner = Partner {
    physr: 0x6400,
    anlpar: 0x45e1,
    gbsr: 0,
};

/// Register model: MAC bytes by address; PHY reads zero while IPRL is clear (NET5a);
/// PHY power bit 15 always reads set; 500 us per transfer. Records every request.
struct Chip {
    mac: [[u8; 6]; 0x40],
    page: u16,
    ids_ready_ns: u64,
    cable: Option<Partner>,
    now: u64,
    trace: Vec<([u8; 8], Vec<u8>, u64)>,
    lines: Vec<String>,
    fail_at: Option<usize>,
    stuck: Option<u8>,
}

impl Chip {
    fn new(cable: Option<Partner>) -> Self {
        let mut mac = [[0; 6]; 0x40];
        mac[0x02][0] = 0x02;
        mac[0x26][..2].copy_from_slice(&[0x00, 0x80]);
        mac[0x33][0] = 0x03;
        Self {
            mac,
            page: 0,
            ids_ready_ns: 0,
            cable,
            now: 1_000_000_000,
            trace: Vec::new(),
            lines: Vec::new(),
            fail_at: None,
            stuck: None,
        }
    }
    fn powered(&self) -> bool {
        self.mac[0x26][0] & 0x20 != 0
    }
    fn phy_read(&self, register: u8) -> u16 {
        if !self.powered() {
            return 0;
        }
        let partner = self.cable;
        match register {
            0 => 0x1140,
            1 => 0x7949 | if partner.is_some() { 0x0024 } else { 0 },
            2 if self.now >= self.ids_ready_ns => 0x001c,
            3 if self.now >= self.ids_ready_ns => 0xc916,
            4 => 0x01e1,
            5 => partner.map_or(0, |p| p.anlpar),
            9 => 0x0300,
            10 => partner.map_or(0, |p| p.gbsr),
            17 => partner.map_or(0, |p| p.physr),
            31 => self.page,
            _ => 0,
        }
    }
    fn has(&self, text: &str) -> bool {
        self.lines.iter().any(|l| l.contains(text))
    }
    fn count(&self, text: &str) -> usize {
        self.lines.iter().filter(|l| l.contains(text)).count()
    }
}

impl Port for Chip {
    type Error = &'static str;
    fn exchange(&mut self, request: &Request, into: &mut [u8; 6]) -> Result<(), Self::Error> {
        let setup = request.setup();
        let (value, index) = (setup[2] as usize, setup[4]);
        let len = request.transfer_len();
        if self.fail_at == Some(self.trace.len()) {
            return Err("injected");
        }
        self.trace
            .push((setup, request.outbound().to_vec(), self.now));
        self.now += TRANSFER;
        match (setup[0], setup[1]) {
            (0xc0, 1) => {
                into[..len].copy_from_slice(&self.mac[value][..len]);
                if value == 0x26 {
                    into[1] |= 0x80;
                }
            }
            (0xc0, 2) => {
                assert_eq!(value, 3, "PHY address");
                into[..2].copy_from_slice(&self.phy_read(index).to_le_bytes());
            }
            (0x40, 1) if self.stuck != Some(value as u8) => {
                self.mac[value][..len].copy_from_slice(request.outbound());
            }
            (0x40, 1) => {}
            other => panic!("unrepresentable request {:?}", other),
        }
        Ok(())
    }
    fn now_ns(&self) -> u64 {
        self.now
    }
    fn record(&mut self, args: fmt::Arguments<'_>) {
        self.lines.push(args.to_string());
    }
}

/// The interrupt endpoint reports each cable change after `delay` (None: silent).
struct Bench {
    chip: Chip,
    link: Link,
    status: StatusView,
    due: Option<(bool, u64)>,
    delay: Option<u64>,
}

impl Bench {
    fn new(cable: Option<Partner>, delay: Option<u64>) -> Self {
        let mut bench = Self {
            chip: Chip::new(cable),
            link: Link::new(),
            status: StatusView::default(),
            due: None,
            delay,
        };
        bench.report(cable.is_some());
        bench
    }
    fn report(&mut self, up: bool) {
        self.due = self.delay.map(|d| (up, self.chip.now + d));
    }
    fn tick(&mut self) -> bool {
        self.chip.now += TICK;
        if let Some((up, at)) = self.due {
            // A powered PHY is what the endpoint reports on.
            if self.chip.now >= at && self.chip.powered() {
                self.status.reports += 1;
                if self.status.link != Some(up) {
                    self.status.link = Some(up);
                    self.status.changed_ns = self.chip.now;
                }
                self.due = None;
            }
        }
        self.link.tick(&mut self.chip, self.status)
    }
    fn until(&mut self, done: impl Fn(&Bench) -> bool) {
        for _ in 0..20_000 {
            if done(self) || !self.tick() {
                return;
            }
        }
        panic!("bench never settled");
    }
    fn cable(&mut self, partner: Option<Partner>) {
        self.chip.cable = partner;
        self.report(partner.is_some());
    }
    /// Wait for a polled state the interrupt endpoint has confirmed (or missed).
    fn settle(&mut self, up: bool) {
        self.until(|b| b.link.link == Some(up) && b.link.awaiting.is_none());
    }
    /// Unplug and replug `cycles` times, each after the current state is confirmed.
    fn cycle(&mut self, cycles: u32, partner: Partner) {
        for _ in 0..cycles {
            self.settle(true);
            self.cable(None);
            self.settle(false);
            self.cable(Some(partner));
            self.settle(true);
        }
    }
    fn receive(cable: Option<Partner>, delay: Option<u64>) -> Self {
        let mut bench = Self::new(cable, delay);
        bench.link = Link::with_goal(Goal::Receive);
        bench
    }
    fn steps(&self) -> Vec<&str> {
        self.chip
            .lines
            .iter()
            .filter_map(|l| l.strip_prefix("NET5b write step="))
            .map(|l| l.split(' ').next().unwrap())
            .collect()
    }
    fn finish(&mut self) -> Option<bool> {
        self.until(|b| b.link.outcome().is_some());
        self.link.outcome()
    }
}

fn setup(direction: u8, command: u8, value: u8, index: u8, len: u8) -> [u8; 8] {
    [direction, command, value, 0, index, 0, len, 0]
}

#[test]
fn initialization_is_this_literal_request_trace() {
    let mut bench = Bench::new(None, Some(TICK / 2));
    bench.until(|b| b.chip.has("NET5b init PASS"));
    let mac_in = |reg: u8, len: u8| (setup(0xc0, 1, reg, len, len), Vec::new());
    let mac_out = |reg: u8, bytes: &[u8]| {
        let len = bytes.len() as u8;
        (setup(0x40, 1, reg, len, len), bytes.to_vec())
    };
    let phy_in = |reg: u8| (setup(0xc0, 2, 3, reg, 2), Vec::new());
    let mut expected = std::vec![
        mac_in(0x26, 2),
        mac_in(0x33, 1),
        mac_in(0x22, 2),
        mac_in(0x0b, 2),
        mac_in(0x34, 1),
        mac_in(0x35, 1),
        mac_in(0x2e, 5),
        mac_out(0x26, &[0, 0]),
        mac_in(0x26, 2),
        mac_out(0x26, &[0x20, 0]),
        mac_in(0x26, 2),
        mac_out(0x33, &[3]),
        mac_in(0x33, 1),
    ];
    for reg in [31, 2, 3, 17, 0, 1, 4, 9] {
        expected.push(phy_in(reg));
    }
    for (reg, bytes) in [
        (0x34, &[0u8][..]),
        (0x35, &[0]),
        (0x22, &[0, 0]),
        (0x0b, &[0x88, 0x02]),
    ] {
        expected.push(mac_out(reg, bytes));
        expected.push(mac_in(reg, bytes.len() as u8));
    }
    let actual: Vec<_> = bench
        .chip
        .trace
        .iter()
        .map(|(s, o, _)| (*s, o.clone()))
        .collect();
    assert_eq!(actual, expected);
    assert!(bench
        .chip
        .has("NET5b write step=phy-release reg=0x26 value=2000 readback=2080"));
    assert!(bench.chip.has("NET5b init PASS reads=22 writes=7"));
}

#[test]
fn settle_waits_follow_the_reference_and_never_block_a_tick() {
    let mut bench = Bench::new(Some(GIGA), Some(TICK));
    bench.until(|b| b.chip.has("NET5b init PASS"));
    let at = |s: [u8; 8]| {
        bench
            .chip
            .trace
            .iter()
            .find(|(x, o, _)| *x == s && o != &[0, 0])
            .map(|t| t.2)
            .unwrap()
    };
    let release = bench
        .chip
        .trace
        .iter()
        .find(|(_, o, _)| o == &[0x20, 0])
        .unwrap()
        .2;
    let clock = at(setup(0x40, 1, 0x33, 1, 1));
    let identify = at(setup(0xc0, 2, 3, 31, 2));
    assert!(clock - release >= PHY_SETTLE_NS);
    assert!(identify - clock >= CLOCK_SETTLE_NS);
    // No tick issues more than its own short burst of transfers.
    let mut per_tick = std::collections::BTreeMap::new();
    for (_, _, t) in &bench.chip.trace {
        *per_tick.entry(t / TICK).or_insert(0) += 1;
    }
    assert!(per_tick.values().all(|&n| n <= 24));
}

#[test]
fn full_cable_proof_passes_with_two_cycles_and_parks_the_chip() {
    let mut bench = Bench::new(Some(GIGA), Some(TICK / 2));
    bench.cycle(CABLE_CYCLES, GIGA);
    assert_eq!(bench.finish(), Some(true), "{:#?}", bench.chip.lines);
    let c = bench.link.counts();
    assert_eq!((c.ups, c.downs, c.cycles, c.agreed), (3, 2, 2, 5));
    assert_eq!((c.missed, c.disagreed), (0, 0));
    assert_eq!(
        bench
            .chip
            .count("NET5b write step=queue reg=0x2e value=07200316ff readback=07200316ff"),
        3
    );
    assert_eq!(
        bench
            .chip
            .count("NET5b write step=medium-link reg=0x22 value=0b00 readback=0b00"),
        3
    );
    assert!(bench
        .chip
        .has("NET5b negotiated speed=1000 full=1 hcd=1000/full"));
    assert!(bench
        .chip
        .has("NET5b parked power=0080 clock=00 rx_control=0000 phy=Unavailable"));
    assert!(bench.chip.has("NET5b result PASS"));
    // Every request is a named MAC access or a PHY read; never a PHY write.
    for (s, _, _) in &bench.chip.trace {
        assert!(
            matches!((s[0], s[1]), (0xc0, 1) | (0x40, 1) | (0xc0, 2)),
            "{:x?}",
            s
        );
    }
    // Parked as NET5a found it, and nothing after the result.
    assert_eq!(&bench.chip.mac[0x26][..2], &[0, 0]);
    assert_eq!(
        (bench.chip.mac[0x33][0], &bench.chip.mac[0x0b][..2]),
        (0, &[0u8, 0][..])
    );
    let n = bench.chip.trace.len();
    assert!(!bench.tick());
    assert_eq!(bench.chip.trace.len(), n);
}

#[test]
fn a_cable_absent_at_boot_starts_with_an_unawaited_initial_down() {
    let mut bench = Bench::new(None, Some(TICK / 2));
    bench.until(|b| b.chip.has("NET5b link initial-down n=0"));
    bench.cable(Some(FAST));
    bench.until(|b| b.link.counts().ups == 1);
    bench.cycle(CABLE_CYCLES, FAST);
    assert_eq!(bench.finish(), Some(true), "{:#?}", bench.chip.lines);
    assert_eq!(bench.link.counts().downs, 2);
    assert!(bench
        .chip
        .has("NET5b negotiated speed=100 full=1 hcd=100/full"));
    assert!(bench.chip.has("value=07ae0718ff"));
    assert!(bench
        .chip
        .has("NET5b write step=medium-link reg=0x22 value=0202"));
}

#[test]
fn a_status_report_ahead_of_the_poll_agrees_with_a_negative_lag() {
    let mut bench = Bench::new(Some(GIGA), Some(0));
    bench.until(|b| b.chip.has("NET5b status agrees"));
    let line = bench
        .chip
        .lines
        .iter()
        .find(|l| l.contains("status agrees"))
        .unwrap();
    assert!(line.contains("lag_ms=-"), "{line}");
}

#[test]
fn a_silent_interrupt_endpoint_or_a_disagreeing_phy_fails_the_result() {
    let mut silent = Bench::new(Some(GIGA), None);
    silent.cycle(CABLE_CYCLES, GIGA);
    assert_eq!(silent.finish(), Some(false));
    assert_eq!(silent.link.counts().missed, 5);
    assert!(silent.chip.has("NET5b result FAIL"));

    // PHYSR reports 100 Mb/s while both pages advertise gigabit.
    let liar = Partner {
        physr: 0x6400,
        ..GIGA
    };
    let mut bench = Bench::new(Some(liar), Some(TICK / 2));
    bench.cycle(CABLE_CYCLES, liar);
    assert_eq!(bench.finish(), Some(false));
    assert_eq!(bench.link.counts().disagreed, 3);
    assert!(bench.chip.has("hcd=1000/full"));
    assert!(bench.chip.has("agrees=0"));
}

#[test]
fn reserved_speed_bits_refuse_without_touching_the_mac() {
    let reserved = Partner {
        physr: 0xe400,
        ..GIGA
    };
    let mut bench = Bench::new(Some(reserved), Some(TICK / 2));
    bench.until(|b| b.chip.has("NET5b negotiated"));
    assert!(bench
        .chip
        .has("NET5b negotiated speed=0 full=0 hcd=1000/full"));
    assert!(!bench.chip.has("step=queue"));
    assert_eq!(bench.finish(), Some(false));
}

#[test]
fn the_monitor_window_ends_incomplete_and_still_parks() {
    let mut bench = Bench::new(Some(GIGA), Some(TICK / 2));
    assert_eq!(bench.finish(), Some(false));
    assert!(bench
        .chip
        .has("NET5b monitor incomplete cycles=0/2 ups=1 downs=0"));
    assert!(bench.chip.has("NET5b parked"));
    assert!(bench.chip.has("NET5b result FAIL"));
    assert!(bench.chip.has("parked=1"));
}

#[test]
fn identify_waits_for_ids_but_refuses_a_foreign_page_or_a_dead_phy() {
    let mut slow = Bench::new(Some(GIGA), Some(TICK / 2));
    slow.chip.ids_ready_ns = slow.chip.now + 1_200_000_000;
    slow.until(|b| b.chip.has("NET5b init PASS"));
    assert!(slow
        .chip
        .lines
        .iter()
        .any(|l| l.contains("NET5b phy id=001cc916") && !l.contains("attempts=1 ")));

    for (page, ready) in [(7, 0), (0, u64::MAX)] {
        let mut bench = Bench::new(Some(GIGA), Some(TICK / 2));
        bench.chip.page = page;
        bench.chip.ids_ready_ns = ready;
        assert_eq!(bench.finish(), Some(false));
        assert!(bench.chip.has("NET5b FAIL identify"));
        // No policy write follows a refused identity.
        assert!(!bench
            .chip
            .trace
            .iter()
            .any(|(s, _, _)| s[0] == 0x40 && s[2] == 0x34));
    }
}

#[test]
fn a_readback_mismatch_or_transport_error_stops_every_later_request() {
    let mut stuck = Bench::new(Some(GIGA), Some(TICK / 2));
    stuck.chip.stuck = Some(0x0b);
    assert_eq!(stuck.finish(), Some(false));
    assert!(stuck
        .chip
        .has("step=rx-control reg=0x0b value=8802 readback=0000 us="));
    assert!(stuck.chip.has("NET5b FAIL readback step=rx-control"));

    for at in [0, 8, 30] {
        let mut bench = Bench::new(Some(GIGA), Some(TICK / 2));
        bench.chip.fail_at = Some(at);
        assert_eq!(bench.finish(), Some(false));
        assert_eq!(bench.chip.trace.len(), at);
        assert!(bench.chip.has("NET5b FAIL transfer setup="));
        for _ in 0..50 {
            assert!(!bench.tick());
        }
        assert_eq!(bench.chip.trace.len(), at);
    }
}

#[test]
fn speed_selects_the_reference_queue_and_medium_without_policy_bits() {
    let q = |b: [u8; 5]| Write::queue(b);
    let m = |v: u16| Write::word(MacRegister::Medium, v, 0xffff);
    for (physr, usb, expected) in [
        (
            0xa400,
            0x04,
            Some((1000, true, q([7, 0x4f, 0, 0x12, 0xff]), m(0x000b))),
        ),
        (
            0xa400,
            0x02,
            Some((1000, true, q([7, 0x20, 3, 0x16, 0xff]), m(0x000b))),
        ),
        (
            0x8400,
            0x00,
            Some((1000, false, q([7, 0xcc, 0x4c, 0x18, 8]), m(0x0009))),
        ),
        (
            0x6400,
            0x02,
            Some((100, true, q([7, 0xae, 7, 0x18, 0xff]), m(0x0202))),
        ),
        (
            0x4400,
            0x00,
            Some((100, false, q([7, 0xcc, 0x4c, 0x18, 8]), m(0x0200))),
        ),
        (
            0x2400,
            0x04,
            Some((10, true, q([7, 0xcc, 0x4c, 0x18, 8]), m(0x0002))),
        ),
        (
            0x0400,
            0x02,
            Some((10, false, q([7, 0xcc, 0x4c, 0x18, 8]), m(0x0000))),
        ),
        (0xe400, 0x02, None),
        (0xa000, 0x02, None),
    ] {
        let got = link_config(physr, usb).map(|c| (c.speed_mbps, c.full_duplex, c.queue, c.medium));
        assert_eq!(got, expected, "physr={physr:#06x} usb={usb:#04x}");
    }
    for physr in 0..=u16::MAX {
        for usb in [0, 2, 4, 6] {
            if let Some(c) = link_config(physr, usb) {
                let medium = u16::from_le_bytes([c.medium.bytes()[0], c.medium.bytes()[1]]);
                assert_eq!(medium & (MEDIUM_RECEIVE_EN | 0x0030 | 0x8040), 0);
                let queue: [u8; 5] = c.queue.bytes().try_into().unwrap();
                assert!(aggregate_limit(queue) <= crate::AGGREGATE_MAX);
            }
        }
    }
}

#[test]
fn policy_disables_offload_receive_and_every_broad_filter() {
    let [rx_sum, tx_sum, medium, rx_ctl] = POLICY.map(|(_, w)| w);
    assert_eq!((rx_sum.bytes(), tx_sum.bytes()), (&[0u8][..], &[0u8][..]));
    assert_eq!(medium.bytes(), &[0, 0]);
    // START|AB|IPE: no PRO(1), AMALL(2), AM(0x10), AP(0x20) or DROPCRCERR(0x100).
    assert_eq!(rx_ctl.bytes(), &[0x88, 0x02]);
    assert_eq!(
        PARK.map(|(_, w)| (w.register(), w.bytes().to_vec())),
        [
            (MacRegister::RxControl, std::vec![0, 0]),
            (MacRegister::Clock, std::vec![0]),
            (MacRegister::PhyPowerReset, std::vec![0, 0]),
        ]
    );
}

#[test]
fn readback_compares_only_the_named_bits() {
    assert!(PHY_RELEASE.verify(&[0x20, 0x80]));
    assert!(!PHY_RELEASE.verify(&[0x00, 0x80]));
    assert!(!PHY_RELEASE.verify(&[0x20, 0x90]));
    assert!(PHY_RESET.verify(&[0x00, 0x80]));
    assert!(!PHY_RESET.verify(&[0x00, 0x80, 0]));
    assert!(CLOCK_ON.verify(&[3]) && !CLOCK_ON.verify(&[0x0b]));
    let rx_ctl = POLICY[3].1;
    assert!(rx_ctl.verify(&[0x88, 0x02]) && !rx_ctl.verify(&[0x88, 0x03]));
}

#[test]
fn resolution_follows_annex_28b_priority() {
    let bmsr = 0x0020;
    assert_eq!(resolve(0, 0x01e1, 0xc5e1, 0x0300, 0x3c00), None);
    assert_eq!(
        resolve(bmsr, 0x01e1, 0xc5e1, 0x0300, 0x0c00),
        Some((1000, true))
    );
    assert_eq!(
        resolve(bmsr, 0x01e1, 0xc5e1, 0x0100, 0x0c00),
        Some((1000, false))
    );
    assert_eq!(
        resolve(bmsr, 0x01e1, 0xc5e1, 0x0200, 0x0400),
        Some((100, true))
    );
    assert_eq!(resolve(bmsr, 0x01e1, 0x0081, 0, 0), Some((100, false)));
    assert_eq!(resolve(bmsr, 0x0261, 0x0201, 0, 0), Some((100, false)));
    assert_eq!(resolve(bmsr, 0x0061, 0x0061, 0, 0), Some((10, true)));
    assert_eq!(resolve(bmsr, 0x0021, 0x0061, 0, 0), Some((10, false)));
    assert_eq!(resolve(bmsr, 0x0001, 0x01e1, 0x0300, 0), None);
}

#[test]
fn every_record_fits_one_log_line() {
    let mut bench = Bench::new(Some(GIGA), Some(TICK / 2));
    bench.cycle(CABLE_CYCLES, GIGA);
    bench.finish();
    let mut slow = Bench::new(None, None);
    slow.finish();
    let mut receive = Bench::receive(Some(GIGA), Some(TICK / 2));
    receive.settle(true);
    receive.link.conclude();
    receive.finish();
    for line in bench
        .chip
        .lines
        .iter()
        .chain(&slow.chip.lines)
        .chain(&receive.chip.lines)
    {
        assert!(line.starts_with("NET5b ") && line.ends_with('\n'), "{line}");
        assert!(line.len() <= 200, "{} bytes: {line}", line.len());
    }
}

#[test]
fn the_receive_goal_toggles_rx_control_enables_receive_and_parks_it_off() {
    let mut bench = Bench::receive(Some(GIGA), Some(TICK / 2));
    bench.settle(true);
    assert!(bench.link.receiving());
    assert!(!bench.chip.has("NET5b cable"));
    assert!(bench.chip.has("NET5b start policy=sw-checksum,unicast+broadcast,receive-on phy_writes=0 goal=receive window_s=600"));
    assert_eq!(&bench.chip.mac[0x22][..2], &[0x0b, 0x01]);
    // Nothing ends the window until the caller concludes it.
    for _ in 0..50 {
        assert!(bench.tick());
    }
    bench.link.conclude();
    assert_eq!(bench.finish(), Some(true), "{:#?}", bench.chip.lines);
    assert_eq!(
        bench.steps(),
        [
            "phy-reset",
            "phy-release",
            "clock",
            "rx-checksum",
            "tx-checksum",
            "medium",
            "rx-control",
            "rx-reset",
            "rx-control",
            "queue",
            "medium-receive",
            "park-medium",
            "park-rx-control",
            "park-clock",
            "park-phy",
        ]
    );
    assert!(bench
        .chip
        .has("NET5b write step=medium-receive reg=0x22 value=0b01 readback=0b01"));
    assert!(bench
        .chip
        .has("NET5b monitor complete goal=receive concluded=1 ups=1 downs=0 agreed=1"));
    assert!(bench.chip.has("NET5b result PASS goal=receive"));
    assert!(bench.chip.has("receive=1 concluded=1 parked=1"));
    assert_eq!(&bench.chip.mac[0x22][..2], &[0, 0]);
}

#[test]
fn an_unconcluded_receive_window_fails_but_still_turns_receive_off() {
    let mut bench = Bench::receive(Some(FAST), Some(TICK / 2));
    assert_eq!(bench.finish(), Some(false));
    assert!(bench
        .chip
        .has("NET5b monitor incomplete goal=receive concluded=0 ups=1"));
    assert!(bench
        .chip
        .has("NET5b write step=park-medium reg=0x22 value=0000 readback=0000"));
    assert!(bench.chip.has("NET5b result FAIL goal=receive"));
    assert!(bench.chip.has("value=0203 readback=0203"));
}

#[test]
fn a_link_flap_during_receive_reapplies_the_toggle_and_receive() {
    let mut bench = Bench::receive(Some(GIGA), Some(TICK / 2));
    bench.cycle(1, GIGA);
    bench.link.conclude();
    assert_eq!(bench.finish(), Some(true), "{:#?}", bench.chip.lines);
    let steps = bench.steps();
    assert_eq!(steps.iter().filter(|s| **s == "rx-reset").count(), 2);
    assert_eq!(steps.iter().filter(|s| **s == "medium-receive").count(), 2);
    let c = bench.link.counts();
    assert_eq!((c.ups, c.downs, c.agreed), (2, 1, 3));
}

#[test]
fn receiving_adds_only_the_receive_bit_to_every_medium() {
    for physr in [0xa400u16, 0x8400, 0x6400, 0x4400, 0x2400, 0x0400] {
        let c = link_config(physr, 2).unwrap();
        let (a, b) = (c.medium.bytes(), c.receiving());
        let (a, b) = (
            u16::from_le_bytes([a[0], a[1]]),
            u16::from_le_bytes([b.bytes()[0], b.bytes()[1]]),
        );
        assert_eq!(b, a | MEDIUM_RECEIVE_EN);
        assert_eq!(b & 0x8070, 0, "no jumbo or pause bits");
    }
}
