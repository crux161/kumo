//j533

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

/// What the driver should do after one round of port-status polls.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HubConnectVerdict {
    /// This 1-based port has held a connection for [`HUB_DEBOUNCE_STABLE_NS`]: reset it.
    Stable(u8),
    /// Poll again after [`HUB_DEBOUNCE_STEP_NS`].
    Waiting,
    /// No port holds a stable connection, and waiting longer will not change that.
    Empty,
}

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

    pub fn verdict(&self, now_ns: u64) -> HubConnectVerdict {
        let ports = &self.connected_since[..self.port_count as usize];
        if let Some(index) = ports.iter().position(|since| {
            since.is_some_and(|since| now_ns.saturating_sub(since) >= HUB_DEBOUNCE_STABLE_NS)
        }) {
            return HubConnectVerdict::Stable(index as u8 + 1);
        }
        let elapsed = now_ns.saturating_sub(self.started_ns);
        let connecting = ports.iter().any(Option::is_some);
        let still_worth_waiting = if connecting {
            elapsed < HUB_DEBOUNCE_TIMEOUT_NS
        } else {
            elapsed < HUB_CONNECT_WINDOW_NS
        };
        if still_worth_waiting {
            HubConnectVerdict::Waiting
        } else {
            HubConnectVerdict::Empty
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
        assert_eq!(hub.verdict(160 * MS), HubConnectVerdict::Stable(3));
    }

    #[test]
    fn an_already_attached_device_still_debounces_once() {
        // A warm reboot keeps the keyboard powered: connected on the first poll, chosen after
        // T_ATTDB rather than immediately.
        let mut hub = HubConnectDebounce::new(4, 0);
        hub.observe(3, true, 0);
        assert_eq!(hub.verdict(25 * MS), HubConnectVerdict::Waiting);
        assert_eq!(hub.verdict(100 * MS), HubConnectVerdict::Stable(3));
    }

    #[test]
    fn a_bounce_restarts_the_debounce() {
        let mut hub = HubConnectDebounce::new(4, 0);
        hub.observe(2, true, 0);
        hub.observe(2, false, 50 * MS);
        hub.observe(2, true, 75 * MS);
        assert_eq!(hub.verdict(150 * MS), HubConnectVerdict::Waiting);
        assert_eq!(hub.verdict(175 * MS), HubConnectVerdict::Stable(2));
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
    fn the_lowest_stable_port_wins_and_out_of_range_ports_are_ignored() {
        let mut hub = HubConnectDebounce::new(4, 0);
        hub.observe(4, true, 0);
        hub.observe(2, true, 0);
        hub.observe(9, true, 0);
        hub.observe(0, true, 0);
        assert_eq!(hub.verdict(100 * MS), HubConnectVerdict::Stable(2));
    }
}
