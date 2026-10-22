//j533
//j541

//! Downstream USB hub bring-up timing: the hub descriptor fields a driver must honour before it
//! reads port status, and the connect-debounce rule for deciding a device is really attached.

use crate::Error;

const HUB_DESCRIPTOR_USB2: u8 = 0x29;
const HUB_DESCRIPTOR_SUPERSPEED: u8 = 0x2a;
/// Bytes up to and including `bPwrOn2PwrGood` (USB 2.0 §11.23.2.1, USB 3.2 §10.15.2.1).
const HUB_DESCRIPTOR_MIN_LEN: usize = 6;

/// Downstream ports a driver tracks. Enough for every SuperSpeed hub (at most 15) and the USB 2
/// hubs seen in practice; a port past this is never chosen.
pub const MAX_HUB_PORTS: usize = 15;

/// A device must signal attach within 100 ms of VBUS becoming valid (USB 2.0 §7.1.7.3,
/// T_SIGATT). Doubled, so a slow device is not mistaken for an empty port.
pub const HUB_CONNECT_WINDOW_NS: u64 = 200_000_000;
/// A connection must hold this long before the port is reset (USB 2.0 §7.1.7.3, T_ATTDB).
pub const HUB_DEBOUNCE_STABLE_NS: u64 = 100_000_000;
/// Interval between port-status polls while debouncing.
pub const HUB_DEBOUNCE_STEP_NS: u64 = 25_000_000;
/// Give up on a connection that never holds still.
pub const HUB_DEBOUNCE_TIMEOUT_NS: u64 = 1_000_000_000;

/// The fields of a hub class descriptor (`0x29` USB 2, `0x2a` SuperSpeed) that gate port use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HubDescriptor {
    port_count: u8,
    power_on_to_power_good: u8,
}

impl HubDescriptor {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < HUB_DESCRIPTOR_MIN_LEN || (bytes[0] as usize) < HUB_DESCRIPTOR_MIN_LEN {
            return Err(Error::InvalidField);
        }
        if bytes[1] != HUB_DESCRIPTOR_USB2 && bytes[1] != HUB_DESCRIPTOR_SUPERSPEED {
            return Err(Error::InvalidField);
        }
        Ok(Self {
            port_count: bytes[2],
            power_on_to_power_good: bytes[5],
        })
    }

    pub const fn port_count(self) -> u8 {
        self.port_count
    }

    /// Time from SetPortFeature(PORT_POWER) until VBUS is good on that port:
    /// `bPwrOn2PwrGood` is in 2 ms units.
    pub const fn power_good_delay_ns(self) -> u64 {
        self.power_on_to_power_good as u64 * 2_000_000
    }
}

/// A set of 1-based downstream port numbers, iterated in ascending order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HubPorts(u16);

impl HubPorts {
    pub const fn empty() -> Self {
        Self(0)
    }

    /// The set with `port` added. Ports outside `1..=MAX_HUB_PORTS` are not representable and
    /// leave the set unchanged.
    pub const fn with(self, port: u8) -> Self {
        if port == 0 || port as usize > MAX_HUB_PORTS {
            self
        } else {
            Self(self.0 | 1 << port)
        }
    }

    pub const fn contains(self, port: u8) -> bool {
        port != 0 && (port as usize) <= MAX_HUB_PORTS && self.0 & (1 << port) != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    pub fn iter(self) -> impl Iterator<Item = u8> {
        (1..=MAX_HUB_PORTS as u8).filter(move |port| self.contains(*port))
    }
}

/// What the driver should do after one round of port-status polls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HubConnectVerdict {
    /// The hub has settled, and these 1-based ports have each held a connection for
    /// [`HUB_DEBOUNCE_STABLE_NS`]: reset and enumerate them, one at a time, in ascending order.
    Stable(HubPorts),
    /// Poll again after [`HUB_DEBOUNCE_STEP_NS`].
    Waiting,
    /// No port holds a stable connection, and waiting longer will not change that.
    Empty,
}

// J541: the verdict used to name the first port that held still, so a keyboard on port 3 hid the
// stick on port 4 for good. It now waits for the whole hub, then names every stable port: until the
// connect window closes (a device may still attach), and while any connection is still bouncing
// (until the timeout, after which that port is left out). — TIMBERDOODLE

/// Connect debounce across one hub's downstream ports, fed by GET_STATUS polls that start once
/// the ports' power is good.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HubConnectDebounce {
    started_ns: u64,
    port_count: u8,
    connected_since: [Option<u64>; MAX_HUB_PORTS],
}

impl HubConnectDebounce {
    pub const fn new(port_count: u8, now_ns: u64) -> Self {
        Self {
            started_ns: now_ns,
            port_count: if port_count as usize > MAX_HUB_PORTS {
                MAX_HUB_PORTS as u8
            } else {
                port_count
            },
            connected_since: [None; MAX_HUB_PORTS],
        }
    }

    /// Record one port's connection bit, read at `now_ns`. A disconnect restarts that port's
    /// debounce. Ports outside `1..=port_count` are ignored.
    pub fn observe(&mut self, port: u8, connected: bool, now_ns: u64) {
        if port == 0 || port > self.port_count {
            return;
        }
        let slot = &mut self.connected_since[port as usize - 1];
        *slot = match (connected, *slot) {
            (false, _) => None,
            (true, None) => Some(now_ns),
            (true, since) => since,
        };
    }

    /// The ports that have held a connection for [`HUB_DEBOUNCE_STABLE_NS`] at `now_ns`.
    pub fn stable_ports(&self, now_ns: u64) -> HubPorts {
        self.connected_since[..self.port_count as usize]
            .iter()
            .enumerate()
            .filter(|(_, since)| {
                since.is_some_and(|since| now_ns.saturating_sub(since) >= HUB_DEBOUNCE_STABLE_NS)
            })
            .fold(HubPorts::empty(), |ports, (index, _)| {
                ports.with(index as u8 + 1)
            })
    }

    pub fn verdict(&self, now_ns: u64) -> HubConnectVerdict {
        let stable = self.stable_ports(now_ns);
        let settling = self.connected_since[..self.port_count as usize]
            .iter()
            .enumerate()
            .any(|(index, since)| since.is_some() && !stable.contains(index as u8 + 1));
        let elapsed = now_ns.saturating_sub(self.started_ns);
        let still_worth_waiting =
            elapsed < HUB_CONNECT_WINDOW_NS || (settling && elapsed < HUB_DEBOUNCE_TIMEOUT_NS);
        if still_worth_waiting {
            HubConnectVerdict::Waiting
        } else if stable.is_empty() {
            HubConnectVerdict::Empty
        } else {
            HubConnectVerdict::Stable(stable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    // A 4-port USB 2 hub with bPwrOn2PwrGood = 50, as the Orange Pi 5 Plus front-panel
    // 05e3:0610 reports (`hub ports=0x4 power-good ms=0x64`); the other bytes are illustrative.
    const GENESYS_HUB: [u8; 9] = [0x09, 0x29, 0x04, 0xe0, 0x00, 0x32, 0x64, 0x00, 0xff];

    #[test]
    fn parses_port_count_and_power_good_delay() {
        let hub = HubDescriptor::parse(&GENESYS_HUB).unwrap();
        assert_eq!(hub.port_count(), 4);
        assert_eq!(hub.power_good_delay_ns(), 100 * MS);
    }

    #[test]
    fn superspeed_hub_descriptor_uses_the_same_offsets() {
        let ss = [
            0x0c, 0x2a, 0x04, 0x09, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let hub = HubDescriptor::parse(&ss).unwrap();
        assert_eq!(hub.port_count(), 4);
        assert_eq!(hub.power_good_delay_ns(), 20 * MS);
    }

    #[test]
    fn rejects_short_or_foreign_descriptors() {
        assert_eq!(
            HubDescriptor::parse(&GENESYS_HUB[..5]),
            Err(Error::InvalidField)
        );
        let mut device = GENESYS_HUB;
        device[1] = 0x01;
        assert_eq!(HubDescriptor::parse(&device), Err(Error::InvalidField));
        let mut lying_length = GENESYS_HUB;
        lying_length[0] = 3;
        assert_eq!(
            HubDescriptor::parse(&lying_length),
            Err(Error::InvalidField)
        );
    }

    #[test]
    fn a_port_scanned_before_its_device_attaches_keeps_waiting() {
        // The 2026-10-21 cold boot: every port read `status=0x100` (powered, nothing attached)
        // 4 ms after power-on, and the scan gave up. The device attaches inside T_SIGATT.
        let mut hub = HubConnectDebounce::new(4, 0);
        for port in 1..=4 {
            hub.observe(port, false, 4 * MS);
        }
        assert_eq!(hub.verdict(4 * MS), HubConnectVerdict::Waiting);
        hub.observe(3, true, 60 * MS);
        assert_eq!(hub.verdict(60 * MS), HubConnectVerdict::Waiting);
        hub.observe(3, true, 160 * MS);
        // Stable, but the connect window is still open for the other ports.
        assert_eq!(hub.verdict(160 * MS), HubConnectVerdict::Waiting);
        assert_eq!(
            hub.verdict(200 * MS),
            HubConnectVerdict::Stable(HubPorts::empty().with(3))
        );
    }

    #[test]
    fn an_already_attached_device_still_debounces_once() {
        // A warm reboot keeps the keyboard powered: connected on the first poll, chosen after
        // T_ATTDB rather than immediately.
        let mut hub = HubConnectDebounce::new(4, 0);
        hub.observe(3, true, 0);
        assert_eq!(hub.verdict(25 * MS), HubConnectVerdict::Waiting);
        assert_eq!(hub.stable_ports(100 * MS), HubPorts::empty().with(3));
        assert_eq!(
            hub.verdict(200 * MS),
            HubConnectVerdict::Stable(HubPorts::empty().with(3))
        );
    }

    #[test]
    fn a_bounce_restarts_the_debounce() {
        let mut hub = HubConnectDebounce::new(4, 0);
        hub.observe(2, true, 0);
        hub.observe(2, false, 50 * MS);
        hub.observe(2, true, 75 * MS);
        assert!(hub.stable_ports(150 * MS).is_empty());
        assert_eq!(hub.stable_ports(175 * MS), HubPorts::empty().with(2));
    }

    #[test]
    fn an_empty_hub_gives_up_after_the_connect_window() {
        let mut hub = HubConnectDebounce::new(4, 0);
        for port in 1..=4 {
            hub.observe(port, false, 0);
        }
        assert_eq!(hub.verdict(199 * MS), HubConnectVerdict::Waiting);
        assert_eq!(hub.verdict(200 * MS), HubConnectVerdict::Empty);
    }

    #[test]
    fn a_connection_that_never_settles_gives_up_at_the_timeout() {
        let mut hub = HubConnectDebounce::new(4, 0);
        let mut now = 0;
        while now < HUB_DEBOUNCE_TIMEOUT_NS {
            hub.observe(1, (now / (50 * MS)) % 2 == 0, now);
            now += 25 * MS;
        }
        assert_eq!(hub.verdict(now), HubConnectVerdict::Empty);
    }

    #[test]
    fn every_stable_port_is_named_and_out_of_range_ports_are_ignored() {
        let mut hub = HubConnectDebounce::new(4, 0);
        hub.observe(4, true, 0);
        hub.observe(2, true, 0);
        hub.observe(9, true, 0);
        hub.observe(0, true, 0);
        let HubConnectVerdict::Stable(ports) = hub.verdict(200 * MS) else {
            panic!("expected a settled hub");
        };
        assert!(ports.iter().eq([2, 4]));
    }

    #[test]
    fn the_keyboard_no_longer_hides_the_stick_beside_it() {
        // J539/J540 on metal: the front USB2 hub reported port 3 `status=0x301` (keyboard) and
        // port 4 `status=0x101` (CRUX's stick) on the same scan. Only port 3 was ever enumerated.
        let mut hub = HubConnectDebounce::new(4, 0);
        let mut now = 0;
        while now <= 200 * MS {
            hub.observe(1, false, now);
            hub.observe(2, false, now);
            hub.observe(3, true, now);
            hub.observe(4, true, now);
            if now < 200 * MS {
                assert_eq!(hub.verdict(now), HubConnectVerdict::Waiting);
            }
            now += 25 * MS;
        }
        let HubConnectVerdict::Stable(ports) = hub.verdict(200 * MS) else {
            panic!("expected a settled hub");
        };
        assert_eq!(ports.len(), 2);
        assert!(ports.iter().eq([3, 4]));
    }

    #[test]
    fn a_bouncing_port_delays_the_stable_ones_only_until_the_timeout() {
        // Port 2 is connected at every poll but drops out between polls every 75 ms, so its
        // debounce keeps restarting and never reaches T_ATTDB.
        let mut hub = HubConnectDebounce::new(4, 0);
        let mut now = 0;
        while now < HUB_DEBOUNCE_TIMEOUT_NS {
            hub.observe(1, true, now);
            if (now / (25 * MS)).is_multiple_of(3) {
                hub.observe(2, false, now);
            }
            hub.observe(2, true, now);
            assert_eq!(hub.verdict(now), HubConnectVerdict::Waiting);
            now += 25 * MS;
        }
        assert_eq!(
            hub.verdict(now),
            HubConnectVerdict::Stable(HubPorts::empty().with(1))
        );
    }

    #[test]
    fn a_device_attaching_late_in_the_window_is_waited_for() {
        let mut hub = HubConnectDebounce::new(4, 0);
        hub.observe(3, true, 0);
        hub.observe(1, true, 190 * MS);
        assert_eq!(hub.verdict(200 * MS), HubConnectVerdict::Waiting);
        let HubConnectVerdict::Stable(ports) = hub.verdict(290 * MS) else {
            panic!("expected a settled hub");
        };
        assert!(ports.iter().eq([1, 3]));
    }

    #[test]
    fn port_sets_hold_exactly_the_representable_ports() {
        let ports = HubPorts::empty().with(0).with(1).with(15).with(16).with(1);
        assert_eq!(ports.len(), 2);
        assert!(ports.contains(1) && ports.contains(15));
        assert!(!ports.contains(0) && !ports.contains(16));
        assert!(ports.iter().eq([1, 15]));
        assert!(HubPorts::empty().is_empty());
    }
}
