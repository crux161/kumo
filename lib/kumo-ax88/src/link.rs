//j833
//j834
//j836
//j839
//! NET5b: minimal volatile initialization and link control for the NET0-profiled
//! AX88179, as pure sequencing over an exact vendor-request port. Every value comes
//! from the supplied ax88179_178a.c (reset, link_reset, set_rx_mode, unbind), cut to
//! KUMO's initial policy: software checksums, unicast+broadcast reception, no pause,
//! EEE, LED, wake, multicast or jumbo setup. The request constructors cannot express
//! EEPROM/eFuse access and this module issues no PHY write. The NET5b cable goal keeps
//! MAC receive off (MEDIUM bit 8 clear); NET6's receive goal turns it on at each link
//! up after link_reset()'s RX control toggle, and off again before parking. Records
//! are raw little-endian register bytes. — TIMBERDOODLE
use core::fmt;

use crate::control::{Direction, MacRegister, Request};
use crate::identity::{phy_status, PhyStatus};

pub const PHYPWR_IPRL: u16 = 0x0020;
/// BZ|IPRL|AT, the only power/reset bits the reference names. Bit 15, set before
/// NET5b on metal (J832), is reported but not compared.
pub const PHYPWR_NAMED: u16 = 0x1030;
pub const CLOCK_ACS_BCS: u8 = 0x03;
pub const RX_CTL_START: u16 = 0x0080;
pub const RX_CTL_AB: u16 = 0x0008;
/// Two-byte IP alignment pseudo header, which `rx::Aggregate` strips.
pub const RX_CTL_IPE: u16 = 0x0200;
pub const MEDIUM_GIGA: u16 = 0x0001;
pub const MEDIUM_FULL_DUPLEX: u16 = 0x0002;
pub const MEDIUM_EN_125MHZ: u16 = 0x0008;
pub const MEDIUM_RECEIVE_EN: u16 = 0x0100;
pub const MEDIUM_PS: u16 = 0x0200;
const PHYSR_LINK: u16 = 0x0400;
const USB_SS: u8 = 0x04;
const USB_HS: u8 = 0x02;

/// Clause 22 registers at PHY address 3. NET5b reads these and writes none.
pub mod phy {
    pub const BMCR: u8 = 0;
    pub const BMSR: u8 = 1;
    pub const ID1: u8 = 2;
    pub const ID2: u8 = 3;
    pub const ANAR: u8 = 4;
    pub const ANLPAR: u8 = 5;
    pub const GBCR: u8 = 9;
    pub const GBSR: u8 = 10;
    /// Vendor physical status: link bit 10, duplex bit 13, speed bits 15:14.
    pub const PHYSR: u8 = 17;
    pub const PAGE: u8 = 31;
}

/// One volatile MAC register write and the bits its readback must reproduce.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Write {
    register: MacRegister,
    value: [u8; 5],
    compare: [u8; 5],
}

impl Write {
    const fn byte(register: MacRegister, value: u8) -> Self {
        Self {
            register,
            value: [value, 0, 0, 0, 0],
            compare: [0xff, 0, 0, 0, 0],
        }
    }
    const fn word(register: MacRegister, value: u16, compare: u16) -> Self {
        let v = value.to_le_bytes();
        let c = compare.to_le_bytes();
        Self {
            register,
            value: [v[0], v[1], 0, 0, 0],
            compare: [c[0], c[1], 0, 0, 0],
        }
    }
    const fn queue(value: [u8; 5]) -> Self {
        Self {
            register: MacRegister::RxBulkQueue,
            value,
            compare: [0xff; 5],
        }
    }
    pub const fn register(&self) -> MacRegister {
        self.register
    }
    pub fn bytes(&self) -> &[u8] {
        &self.value[..self.register.width()]
    }
    pub fn request(&self) -> Request {
        Request::mac_write(self.register, self.bytes()).expect("writable literal register width")
    }
    pub fn readback(&self) -> Request {
        Request::mac_read(self.register)
    }
    pub fn verify(&self, read: &[u8]) -> bool {
        read.len() == self.register.width()
            && read
                .iter()
                .zip(self.value.iter().zip(&self.compare))
                .all(|(r, (v, c))| (r ^ v) & c == 0)
    }
}

/// Read before any write, so the state firmware left behind is on record.
pub const BASELINE: [MacRegister; 7] = [
    MacRegister::PhyPowerReset,
    MacRegister::Clock,
    MacRegister::Medium,
    MacRegister::RxControl,
    MacRegister::RxChecksum,
    MacRegister::TxChecksum,
    MacRegister::RxBulkQueue,
];
/// reset(): PHY reset asserted, then released; 500 ms; clocks; 200 ms.
pub const PHY_RESET: Write = Write::word(MacRegister::PhyPowerReset, 0, PHYPWR_NAMED);
pub const PHY_RELEASE: Write = Write::word(MacRegister::PhyPowerReset, PHYPWR_IPRL, PHYPWR_NAMED);
pub const PHY_SETTLE_NS: u64 = 500_000_000;
pub const CLOCK_ON: Write = Write::byte(MacRegister::Clock, CLOCK_ACS_BCS);
pub const CLOCK_SETTLE_NS: u64 = 200_000_000;
/// Checksum offload off both ways (reset() enables it); MAC receive off with no
/// speed bits; set_rx_mode()'s START|AB|IPE with no multicast or promiscuous bit.
pub const POLICY: [(&str, Write); 4] = [
    ("rx-checksum", Write::byte(MacRegister::RxChecksum, 0)),
    ("tx-checksum", Write::byte(MacRegister::TxChecksum, 0)),
    ("medium", Write::word(MacRegister::Medium, 0, 0xffff)),
    (
        "rx-control",
        Write::word(
            MacRegister::RxControl,
            RX_CTL_START | RX_CTL_AB | RX_CTL_IPE,
            0xffff,
        ),
    ),
];
/// unbind(): receive stopped, clocks off, PHY powered down, as NET5a found it.
pub const PARK: [(&str, Write); 3] = [
    (
        "park-rx-control",
        Write::word(MacRegister::RxControl, 0, 0xffff),
    ),
    ("park-clock", Write::byte(MacRegister::Clock, 0)),
    (
        "park-phy",
        Write::word(MacRegister::PhyPowerReset, 0, PHYPWR_NAMED),
    ),
];

/// AX88179_BULKIN_SIZE: SuperSpeed giga, high-speed giga, 100 Mb/s, slow fallback.
const BULKIN: [[u8; 5]; 4] = [
    [7, 0x4f, 0, 0x12, 0xff],
    [7, 0x20, 3, 0x16, 0xff],
    [7, 0xae, 7, 0x18, 0xff],
    [7, 0xcc, 0x4c, 0x18, 8],
];

/// link_reset()'s `rx_urb_size` for a queue setting: the largest aggregate it permits.
pub const fn aggregate_limit(queue: [u8; 5]) -> usize {
    1024 * (queue[3] as usize + 2)
}

impl LinkConfig {
    /// NET6: the same medium with MAC receive enabled.
    pub fn receiving(&self) -> Write {
        let b = self.medium.bytes();
        Write::word(
            MacRegister::Medium,
            u16::from_le_bytes([b[0], b[1]]) | MEDIUM_RECEIVE_EN,
            0xffff,
        )
    }
}

/// NET6 receive goal: link_reset()'s RX control stop/start before each link-up's
/// medium, and receive off before the unbind() park.
pub const RX_RESET: (&str, Write) = ("rx-reset", Write::word(MacRegister::RxControl, 0, 0xffff));
pub const RECEIVE_OFF: (&str, Write) = ("park-medium", Write::word(MacRegister::Medium, 0, 0xffff));

/// J839 experiment: request the reference link_reset()'s TX re-arm from outside the
/// proof tick (which has no room for new frames); the next receive-goal tick runs it.
static REARM_PENDING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn request_rearm() {
    REARM_PENDING.store(true, core::sync::atomic::Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkConfig {
    pub speed_mbps: u16,
    pub full_duplex: bool,
    pub queue: Write,
    pub medium: Write,
}

/// link_reset()'s queue and medium choice for a reported link, minus RECEIVE_EN and
/// both pause enables (policy). Reserved speed bits refuse where Linux would fall
/// back to its 10 Mb/s branch.
pub fn link_config(physr: u16, usb_link: u8) -> Option<LinkConfig> {
    if physr & PHYSR_LINK == 0 {
        return None;
    }
    let fast_usb = usb_link & (USB_SS | USB_HS) != 0;
    let (speed_mbps, mut medium, queue) = match physr & 0xc000 {
        0x8000 => (
            1000,
            MEDIUM_GIGA | MEDIUM_EN_125MHZ,
            if usb_link & USB_SS != 0 {
                BULKIN[0]
            } else if usb_link & USB_HS != 0 {
                BULKIN[1]
            } else {
                BULKIN[3]
            },
        ),
        0x4000 => (100, MEDIUM_PS, BULKIN[if fast_usb { 2 } else { 3 }]),
        0 => (10, 0, BULKIN[3]),
        _ => return None,
    };
    let full_duplex = physr & 0x2000 != 0;
    if full_duplex {
        medium |= MEDIUM_FULL_DUPLEX;
    }
    Some(LinkConfig {
        speed_mbps,
        full_duplex,
        queue: Write::queue(queue),
        medium: Write::word(MacRegister::Medium, medium, 0xffff),
    })
}

/// IEEE 802.3 Annex 28B.3 priority over our advertisement and the partner's pages.
/// `None` until autonegotiation completes or without a common technology.
pub fn resolve(bmsr: u16, anar: u16, anlpar: u16, gbcr: u16, gbsr: u16) -> Option<(u16, bool)> {
    if bmsr & 0x0020 == 0 {
        return None;
    }
    // GBSR partner bits 11/10 line up with GBCR advertisement bits 9/8.
    let giga = gbcr & (gbsr >> 2);
    let common = anar & anlpar;
    [
        (giga & 0x0200, 1000, true),
        (giga & 0x0100, 1000, false),
        (common & 0x0100, 100, true),
        (common & 0x0200, 100, false),
        (common & 0x0080, 100, false),
        (common & 0x0040, 10, true),
        (common & 0x0020, 10, false),
    ]
    .into_iter()
    .find(|&(bit, _, _)| bit != 0)
    .map(|(_, speed, full)| (speed, full))
}

/// A powered PHY must report valid IDs within this window after the clock settles.
pub const IDENTIFY_NS: u64 = 2_000_000_000;
/// The interrupt endpoint must confirm each polled transition within this window.
pub const STATUS_AGREE_NS: u64 = 2_000_000_000;
/// Physical unplug/replug cycles the monitor waits for, and for how long.
pub const CABLE_CYCLES: u32 = 2;
pub const MONITOR_NS: u64 = 600_000_000_000;

/// Exact vendor transfers for the sequence. Every error is terminal: the sequence
/// stops and issues nothing further.
pub trait Port {
    type Error: Copy + fmt::Debug;
    /// OUT sends `request.outbound()`; IN fills exactly `request.transfer_len()` bytes.
    fn exchange(&mut self, request: &Request, into: &mut [u8; 6]) -> Result<(), Self::Error>;
    fn now_ns(&self) -> u64;
    fn record(&mut self, args: fmt::Arguments<'_>);
}

/// What the interrupt endpoint last reported: its link bit and when it last changed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StatusView {
    pub reports: u32,
    pub link: Option<bool>,
    pub changed_ns: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    pub reads: u32,
    pub writes: u32,
    pub ups: u32,
    pub downs: u32,
    pub cycles: u32,
    pub agreed: u32,
    pub missed: u32,
    pub disagreed: u32,
    pub max_transfer_ns: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Start,
    PhySettle { until: u64 },
    ClockSettle { until: u64 },
    Identify { deadline: u64, attempts: u32 },
    Monitor { deadline: u64 },
    Park,
    Parked,
    Done,
    Failed,
}

/// A polled transition awaiting the interrupt endpoint's confirmation.
#[derive(Clone, Copy, Debug)]
struct Awaiting {
    up: bool,
    at_ns: u64,
    previous_ns: u64,
}

struct Stop;
type Step = Result<(), Stop>;

/// What the monitor waits for: NET5b's physical cable cycles, or NET6's caller-ended
/// receive window with MAC receive on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Goal {
    CableCycles,
    Receive,
}

pub struct Link {
    goal: Goal,
    concluded: bool,
    receiving: bool,
    phase: Phase,
    started_ns: u64,
    released_ns: u64,
    clocked_ns: u64,
    link: Option<bool>,
    changed_ns: u64,
    awaiting: Option<Awaiting>,
    down_after_up: bool,
    init_ok: bool,
    parked_ok: bool,
    counts: Counts,
}

impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}

impl Link {
    pub const fn new() -> Self {
        Self::with_goal(Goal::CableCycles)
    }

    pub const fn with_goal(goal: Goal) -> Self {
        Self {
            goal,
            concluded: false,
            receiving: false,
            phase: Phase::Start,
            started_ns: 0,
            released_ns: 0,
            clocked_ns: 0,
            link: None,
            changed_ns: 0,
            awaiting: None,
            down_after_up: false,
            init_ok: false,
            parked_ok: false,
            counts: Counts {
                reads: 0,
                writes: 0,
                ups: 0,
                downs: 0,
                cycles: 0,
                agreed: 0,
                missed: 0,
                disagreed: 0,
                max_transfer_ns: 0,
            },
        }
    }

    pub const fn counts(&self) -> Counts {
        self.counts
    }

    /// Whether a link-up has turned MAC receive on (receive goal only).
    pub const fn receiving(&self) -> bool {
        self.receiving
    }

    /// End the receive window; the next tick parks once the link state is confirmed.
    pub fn conclude(&mut self) {
        self.concluded = true;
    }

    /// `Some(pass)` once the sequence has ended; a failure is `Some(false)`.
    pub fn outcome(&self) -> Option<bool> {
        match self.phase {
            Phase::Done => Some(self.passed()),
            Phase::Failed => Some(false),
            _ => None,
        }
    }

    fn passed(&self) -> bool {
        let c = self.counts;
        let goal = match self.goal {
            Goal::CableCycles => c.cycles >= CABLE_CYCLES,
            Goal::Receive => self.concluded && self.receiving && c.ups > 0,
        };
        self.init_ok
            && self.parked_ok
            && goal
            && c.missed == 0
            && c.disagreed == 0
            && c.agreed == c.ups + c.downs
    }

    /// One paced step. Returns whether another tick is wanted.
    pub fn tick<P: Port>(&mut self, port: &mut P, status: StatusView) -> bool {
        if self.rearm_if_pending(port).is_err() {
            self.phase = Phase::Failed;
            return false;
        }
        let now = port.now_ns();
        let _ = match self.phase {
            Phase::Start => self.start(port, now),
            Phase::PhySettle { until } if now >= until => self.clock(port, now),
            Phase::ClockSettle { until } if now >= until => {
                port.record(format_args!(
                    "NET5b settled step=clock ms={}\n",
                    ms(now - self.clocked_ns)
                ));
                self.identify(port, now, now + IDENTIFY_NS, 0)
            }
            Phase::Identify { deadline, attempts } => self.identify(port, now, deadline, attempts),
            Phase::Monitor { deadline } => self.monitor(port, now, deadline, status),
            Phase::Park => self.park(port),
            Phase::Parked => self.parked(port),
            _ => Ok(()),
        };
        !matches!(self.phase, Phase::Done | Phase::Failed)
    }

    fn start<P: Port>(&mut self, port: &mut P, now: u64) -> Step {
        self.started_ns = now;
        match self.goal {
            Goal::CableCycles => port.record(format_args!(
                "NET5b start policy=sw-checksum,unicast+broadcast,receive-off phy_writes=0 cycles={} window_s={}\n",
                CABLE_CYCLES,
                MONITOR_NS / 1_000_000_000
            )),
            Goal::Receive => port.record(format_args!(
                "NET5b start policy=sw-checksum,unicast+broadcast,receive-on phy_writes=0 goal=receive window_s={}\n",
                MONITOR_NS / 1_000_000_000
            )),
        }
        for register in BASELINE {
            let bytes = self.transfer(port, &Request::mac_read(register))?;
            port.record(format_args!(
                "NET5b baseline reg={:#04x} bytes={}\n",
                register.address(),
                Hex(&bytes[..register.width()])
            ));
        }
        self.write(port, "phy-reset", PHY_RESET)?;
        self.write(port, "phy-release", PHY_RELEASE)?;
        self.released_ns = port.now_ns();
        self.phase = Phase::PhySettle {
            until: self.released_ns + PHY_SETTLE_NS,
        };
        Ok(())
    }

    fn clock<P: Port>(&mut self, port: &mut P, now: u64) -> Step {
        port.record(format_args!(
            "NET5b settled step=phy ms={}\n",
            ms(now - self.released_ns)
        ));
        self.write(port, "clock", CLOCK_ON)?;
        self.clocked_ns = port.now_ns();
        self.phase = Phase::ClockSettle {
            until: self.clocked_ns + CLOCK_SETTLE_NS,
        };
        Ok(())
    }

    fn identify<P: Port>(&mut self, port: &mut P, now: u64, deadline: u64, attempts: u32) -> Step {
        let attempts = attempts + 1;
        let page = self.phy(port, phy::PAGE)?;
        let id1 = self.phy(port, phy::ID1)?;
        let id2 = self.phy(port, phy::ID2)?;
        let physr = self.phy(port, phy::PHYSR)?;
        match phy_status(page, id1, id2, physr) {
            PhyStatus::Page0 { .. } => {}
            // Selecting page zero would be a PHY write; refuse instead.
            PhyStatus::UnknownPage(page) => {
                return self.fail(port, format_args!("identify page={:#06x}", page))
            }
            PhyStatus::Unavailable if now < deadline => {
                self.phase = Phase::Identify { deadline, attempts };
                return Ok(());
            }
            PhyStatus::Unavailable => {
                return self.fail(
                    port,
                    format_args!(
                        "identify id={:04x}{:04x} physr={:#06x} attempts={}",
                        id1, id2, physr, attempts
                    ),
                )
            }
        }
        let bmcr = self.phy(port, phy::BMCR)?;
        let bmsr = self.phy(port, phy::BMSR)?;
        let anar = self.phy(port, phy::ANAR)?;
        let gbcr = self.phy(port, phy::GBCR)?;
        port.record(format_args!(
            "NET5b phy id={:04x}{:04x} page=0 bmcr={:#06x} bmsr={:#06x} anar={:#06x} gbcr={:#06x} attempts={} ready_ms={}\n",
            id1,
            id2,
            bmcr,
            bmsr,
            anar,
            gbcr,
            attempts,
            ms(port.now_ns() - self.released_ns)
        ));
        for (step, write) in POLICY {
            self.write(port, step, write)?;
        }
        self.init_ok = true;
        let at = port.now_ns();
        port.record(format_args!(
            "NET5b init PASS reads={} writes={} ms={}\n",
            self.counts.reads,
            self.counts.writes,
            ms(at - self.started_ns)
        ));
        // A link cannot predate the PHY's release: the first sample's `since_ms` is the
        // autonegotiation time, and a status report during init can confirm it.
        self.changed_ns = self.released_ns;
        self.phase = Phase::Monitor {
            deadline: at + MONITOR_NS,
        };
        Ok(())
    }

    fn monitor<P: Port>(
        &mut self,
        port: &mut P,
        now: u64,
        deadline: u64,
        status: StatusView,
    ) -> Step {
        self.confirm(port, now, status);
        let physr = self.phy(port, phy::PHYSR)?;
        let up = physr & PHYSR_LINK != 0;
        if self.link != Some(up) {
            let at = port.now_ns();
            let first = self.link.is_none();
            if let Some(stale) = self.awaiting.take() {
                self.counts.missed += 1;
                port.record(format_args!(
                    "NET5b status missed link={} superseded_ms={} reported={}\n",
                    stale.up as u8,
                    ms(at - stale.at_ns),
                    Reported(status.link)
                ));
            }
            let previous_ns = self.changed_ns;
            self.link = Some(up);
            self.changed_ns = at;
            let n = if up {
                self.counts.ups += 1;
                if self.down_after_up {
                    self.down_after_up = false;
                    self.counts.cycles += 1;
                }
                self.counts.ups
            } else if first {
                0
            } else {
                self.counts.downs += 1;
                self.down_after_up = self.counts.ups > 0;
                self.counts.downs
            };
            port.record(format_args!(
                "NET5b link {} n={} ms={} since_ms={} physr={:#06x}\n",
                match (up, first) {
                    (true, _) => "up",
                    (false, true) => "initial-down",
                    (false, false) => "down",
                },
                n,
                ms(at - self.started_ns),
                ms(at - previous_ns),
                physr
            ));
            if up {
                self.configure(port, physr)?;
            }
            // The interrupt endpoint has no transition to report for the first
            // sample of a cable that is already out.
            if up || !first {
                self.awaiting = Some(Awaiting {
                    up,
                    at_ns: at,
                    previous_ns,
                });
                let now = port.now_ns();
                self.confirm(port, now, status);
            }
            if self.goal == Goal::CableCycles && self.counts.cycles < CABLE_CYCLES {
                port.record(format_args!(
                    "NET5b cable {} the ax88 Ethernet cable (cycle {}/{})\n",
                    if up { "unplug" } else { "replug" },
                    self.counts.cycles + 1,
                    CABLE_CYCLES
                ));
            }
        }
        let goal = match self.goal {
            Goal::CableCycles => self.counts.cycles >= CABLE_CYCLES,
            Goal::Receive => self.concluded,
        };
        let complete = goal && self.awaiting.is_none() && up;
        if self.goal == Goal::Receive && (complete || now >= deadline) {
            let c = self.counts;
            port.record(format_args!(
                "NET5b monitor {} goal=receive concluded={} ups={} downs={} agreed={} missed={} disagreed={} ms={}\n",
                if complete { "complete" } else { "incomplete" },
                self.concluded as u8,
                c.ups,
                c.downs,
                c.agreed,
                c.missed,
                c.disagreed,
                ms(port.now_ns() - self.started_ns)
            ));
            self.phase = Phase::Park;
        } else if complete || now >= deadline {
            let c = self.counts;
            port.record(format_args!(
                "NET5b monitor {} cycles={}/{} ups={} downs={} agreed={} missed={} disagreed={} ms={}\n",
                if complete { "complete" } else { "incomplete" },
                c.cycles,
                CABLE_CYCLES,
                c.ups,
                c.downs,
                c.agreed,
                c.missed,
                c.disagreed,
                ms(port.now_ns() - self.started_ns)
            ));
            self.phase = Phase::Park;
        }
        Ok(())
    }

    /// A status change after the previous transition reports this one; a negative lag
    /// means the interrupt endpoint saw it before the poll did.
    fn confirm<P: Port>(&mut self, port: &mut P, now: u64, status: StatusView) {
        let Some(wait) = self.awaiting else {
            return;
        };
        if status.link == Some(wait.up) && status.changed_ns >= wait.previous_ns {
            self.counts.agreed += 1;
            self.awaiting = None;
            port.record(format_args!(
                "NET5b status agrees link={} lag_ms={} reports={}\n",
                wait.up as u8,
                (status.changed_ns as i64 - wait.at_ns as i64) / 1_000_000,
                status.reports
            ));
        } else if now.saturating_sub(wait.at_ns) > STATUS_AGREE_NS {
            self.counts.missed += 1;
            self.awaiting = None;
            port.record(format_args!(
                "NET5b status missed link={} waited_ms={} reported={}\n",
                wait.up as u8,
                ms(now - wait.at_ns),
                Reported(status.link)
            ));
        }
    }

    fn configure<P: Port>(&mut self, port: &mut P, physr: u16) -> Step {
        let bmsr = self.phy(port, phy::BMSR)?;
        let anar = self.phy(port, phy::ANAR)?;
        let anlpar = self.phy(port, phy::ANLPAR)?;
        let gbcr = self.phy(port, phy::GBCR)?;
        let gbsr = self.phy(port, phy::GBSR)?;
        let usb = self.transfer(port, &Request::mac_read(MacRegister::UsbLink))?[0];
        let resolved = resolve(bmsr, anar, anlpar, gbcr, gbsr);
        let config = link_config(physr, usb);
        let agrees = config.is_some_and(|c| resolved == Some((c.speed_mbps, c.full_duplex)));
        if !agrees {
            self.counts.disagreed += 1;
        }
        port.record(format_args!(
            "NET5b negotiated speed={} full={} hcd={} bmsr={:#06x} anar={:#06x} anlpar={:#06x} gbcr={:#06x} gbsr={:#06x} usb={:#04x} agrees={}\n",
            config.map_or(0, |c| c.speed_mbps),
            config.is_some_and(|c| c.full_duplex) as u8,
            Resolved(resolved),
            bmsr,
            anar,
            anlpar,
            gbcr,
            gbsr,
            usb,
            agrees as u8
        ));
        // Reserved speed bits leave the MAC as it was; the result already fails.
        let Some(config) = config else {
            return Ok(());
        };
        if self.goal == Goal::Receive {
            let (step, write) = RX_RESET;
            self.write(port, step, write)?;
            let (step, write) = POLICY[3];
            self.write(port, step, write)?;
        }
        self.write(port, "queue", config.queue)?;
        if self.goal == Goal::Receive {
            // Reference link_reset() polls the device TX FIFO empty (vendor 0x81/0x8c)
            // before the medium. Record one read: the 2026-11-07 wire capture showed TX
            // stops after the first padded transfer, so this word is the experiment.
            let fifo = self.transfer(port, &Request::fifo_status())?;
            port.record(format_args!(
                "NET5b fifo value={:#010x}\n",
                u32::from_le_bytes([fifo[0], fifo[1], fifo[2], fifo[3]])
            ));
            self.write(port, "medium-receive", config.receiving())?;
            self.receiving = true;
            return Ok(());
        }
        self.write(port, "medium-link", config.medium)
    }

    /// Runs a pending TX re-arm on the tick's own frame: RX_CTL 0000 then
    /// START|AB|IPE and one 0x81/0x8c FIFO read, recorded for the peer capture.
    fn rearm_if_pending<P: Port>(&mut self, port: &mut P) -> Step {
        if !self.receiving || !REARM_PENDING.swap(false, core::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        let on = Request::mac_write(
            MacRegister::RxControl,
            &(RX_CTL_START | RX_CTL_AB | RX_CTL_IPE).to_le_bytes(),
        )
        .map_err(|_| Stop)?;
        let off = Request::mac_write(MacRegister::RxControl, &[0x00, 0x00]).map_err(|_| Stop)?;
        self.transfer(port, &off)?;
        self.transfer(port, &on)?;
        let fifo = self.transfer(port, &Request::fifo_status())?;
        port.record(format_args!(
            "NET7 tx rearm fifo={:#010x}\n",
            u32::from_le_bytes([fifo[0], fifo[1], fifo[2], fifo[3]])
        ));
        Ok(())
    }

    fn park<P: Port>(&mut self, port: &mut P) -> Step {
        if self.goal == Goal::Receive {
            let (step, write) = RECEIVE_OFF;
            self.write(port, step, write)?;
        }
        for (step, write) in PARK {
            self.write(port, step, write)?;
        }
        self.phase = Phase::Parked;
        Ok(())
    }

    /// One tick after parking, the PHY must read back unavailable as NET5a found it.
    fn parked<P: Port>(&mut self, port: &mut P) -> Step {
        let power = self.transfer(port, &Request::mac_read(MacRegister::PhyPowerReset))?;
        let clock = self.transfer(port, &Request::mac_read(MacRegister::Clock))?[0];
        let rx = self.transfer(port, &Request::mac_read(MacRegister::RxControl))?;
        let page = self.phy(port, phy::PAGE)?;
        let id1 = self.phy(port, phy::ID1)?;
        let id2 = self.phy(port, phy::ID2)?;
        let physr = self.phy(port, phy::PHYSR)?;
        let phy = phy_status(page, id1, id2, physr);
        self.parked_ok = phy == PhyStatus::Unavailable;
        port.record(format_args!(
            "NET5b parked power={} clock={:02x} rx_control={} phy={:?}\n",
            Hex(&power[..2]),
            clock,
            Hex(&rx[..2]),
            phy
        ));
        self.phase = Phase::Done;
        let c = self.counts;
        if self.goal == Goal::Receive {
            port.record(format_args!(
                "NET5b result {} goal=receive reads={} writes={} ups={} downs={} agreed={} missed={} disagreed={} max_transfer_us={} init={} receive={} concluded={} parked={}\n",
                if self.passed() { "PASS" } else { "FAIL" },
                c.reads,
                c.writes,
                c.ups,
                c.downs,
                c.agreed,
                c.missed,
                c.disagreed,
                c.max_transfer_ns / 1_000,
                self.init_ok as u8,
                self.receiving as u8,
                self.concluded as u8,
                self.parked_ok as u8
            ));
            return Ok(());
        }
        port.record(format_args!(
            "NET5b result {} reads={} writes={} ups={} downs={} cycles={}/{} agreed={} missed={} disagreed={} max_transfer_us={} init={} parked={}\n",
            if self.passed() { "PASS" } else { "FAIL" },
            c.reads,
            c.writes,
            c.ups,
            c.downs,
            c.cycles,
            CABLE_CYCLES,
            c.agreed,
            c.missed,
            c.disagreed,
            c.max_transfer_ns / 1_000,
            self.init_ok as u8,
            self.parked_ok as u8
        ));
        Ok(())
    }

    fn write<P: Port>(&mut self, port: &mut P, step: &str, write: Write) -> Step {
        let start = port.now_ns();
        self.transfer(port, &write.request())?;
        let back = self.transfer(port, &write.readback())?;
        let back = &back[..write.register().width()];
        let ok = write.verify(back);
        port.record(format_args!(
            "NET5b write step={} reg={:#04x} value={} readback={} us={} {}\n",
            step,
            write.register().address(),
            Hex(write.bytes()),
            Hex(back),
            (port.now_ns() - start) / 1_000,
            if ok { "PASS" } else { "MISMATCH" }
        ));
        if !ok {
            return self.fail(port, format_args!("readback step={}", step));
        }
        Ok(())
    }

    fn phy<P: Port>(&mut self, port: &mut P, register: u8) -> Result<u16, Stop> {
        let request = Request::phy_read(register).expect("literal PHY register");
        let bytes = self.transfer(port, &request)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn transfer<P: Port>(&mut self, port: &mut P, request: &Request) -> Result<[u8; 6], Stop> {
        let mut bytes = [0; 6];
        let start = port.now_ns();
        if let Err(error) = port.exchange(request, &mut bytes) {
            let setup = request.setup();
            self.fail(
                port,
                format_args!("transfer setup={} error={:?}", Hex(&setup), error),
            )?;
        }
        let took = port.now_ns().saturating_sub(start);
        self.counts.max_transfer_ns = self.counts.max_transfer_ns.max(took);
        match request.direction() {
            Direction::In => self.counts.reads += 1,
            Direction::Out => self.counts.writes += 1,
        }
        Ok(bytes)
    }

    fn fail<P: Port>(&mut self, port: &mut P, why: fmt::Arguments<'_>) -> Step {
        self.phase = Phase::Failed;
        port.record(format_args!(
            "NET5b FAIL {}; EP0 and lanes retained, no further request\n",
            why
        ));
        Err(Stop)
    }
}

fn ms(ns: u64) -> u64 {
    ns / 1_000_000
}

struct Hex<'a>(&'a [u8]);
impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

struct Resolved(Option<(u16, bool)>);
impl fmt::Display for Resolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some((speed, full)) => write!(f, "{}/{}", speed, if full { "full" } else { "half" }),
            None => f.write_str("none"),
        }
    }
}

struct Reported(Option<bool>);
impl fmt::Display for Reported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(link) => write!(f, "{}", link as u8),
            None => f.write_str("none"),
        }
    }
}

#[cfg(test)]
mod tests;
