//j895
//! Temperature-focused NVMe SMART/Health log and architecture-neutral status. — KESTREL
use crate::{
    admin::{self, AdminIo, Completion, Queue},
    Error, Memory, Startup, PAGE,
};
use core::fmt::{self, Write};
pub const LOG_BYTES: usize = 512;
pub const STATUS_BYTES: usize = 64;
/// Supervisor-only request, carrying a new reply-channel endpoint for this one read.
pub const REQUEST: u8 = b'H';
pub const MAX_AGE_NS: u64 = 10_000_000_000;
pub const REQUEST_WAIT_NS: u64 = 2_000_000_000;

pub fn millicelsius(kelvin: u16) -> Option<i32> {
    (!matches!(kelvin, 0 | u16::MAX)).then_some(i32::from(kelvin) * 1000 - 273_150)
}
fn at(b: &[u8], n: usize) -> u16 {
    u16::from_le_bytes([b[n], b[n + 1]])
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub warning: u16,
    pub critical: u16,
}
impl Limits {
    /// Identify Controller WCTEMP/CCTEMP, not a guessed SoC curve or a Set Features change.
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        if b.len() != PAGE as usize {
            return Err(Error::Data);
        }
        Ok(Self {
            warning: at(b, 266),
            critical: at(b, 268),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Log {
    pub critical_warning: u8,
    pub composite: u16,
    pub sensors: [u16; 8],
}
impl Log {
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        if b.len() != LOG_BYTES {
            return Err(Error::Data);
        }
        let mut sensors = [0; 8];
        for (i, out) in sensors.iter_mut().enumerate() {
            *out = at(b, 200 + 2 * i);
        }
        Ok(Self {
            critical_warning: b[0],
            composite: at(b, 1),
            sensors,
        })
    }
    pub fn thermal_warning(self) -> bool {
        self.critical_warning & 2 != 0
    }
}

/// NVMe 1.x Get Log Page: controller-global SMART, 128 DWORDs (zero-based NUMD 127).
/// RAE=0 is the normal read. The tested Samsung 970 EVO Plus returns a zero page with
/// success for RAE=1. This owner submits no Async Event Requests. — KESTREL
pub fn command(address: u64) -> Result<[u32; 16], Error> {
    if address == 0 || address & (PAGE - 1) != 0 {
        return Err(Error::Memory);
    }
    let mut w = [0; 16];
    w[0] = 2;
    w[1] = u32::MAX;
    w[6] = address as u32;
    w[7] = (address >> 32) as u32;
    w[10] = (127 << 16) | 2;
    Ok(w)
}

pub fn read(
    io: &mut impl AdminIo,
    queue: &mut Queue,
    s: Startup,
    memory: Memory,
    limits: Limits,
) -> Result<(Health, Completion), Error> {
    let words = command(
        memory
            .physical
            .checked_add(admin::DATA)
            .ok_or(Error::Memory)?,
    )?;
    let mut data = [0; LOG_BYTES];
    let c = queue.submit_admin_status(io, s, memory, words, &mut data)?;
    let status = c.status >> 1;
    let mut health = Health {
        session: s.session,
        limits,
        command_status: status,
        ..Health::default()
    };
    if status == 0 {
        health = Health::from_log(s.session, limits, &data, io.now_ns())?;
    } else {
        health.state = State::ControllerError;
    }
    Ok((health, c))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Ok = 0,
    ControllerError = 1,
    TransportError = 2,
    #[default]
    Unavailable = 3,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Health {
    pub session: u32,
    pub state: State,
    pub sampled_ns: u64,
    pub limits: Limits,
    pub log: Log,
    pub command_status: u16,
    pub transport_error: u32,
}
impl Health {
    /// A completed read with an unwritten/empty page is unavailable, never a cold drive.
    pub fn from_log(
        session: u32,
        limits: Limits,
        data: &[u8],
        sampled_ns: u64,
    ) -> Result<Self, Error> {
        let log = Log::decode(data)?;
        if data.iter().all(|b| *b == 0) {
            return Ok(Self {
                session,
                limits,
                ..Self::default()
            });
        }
        Ok(Self {
            session,
            limits,
            log,
            sampled_ns,
            state: State::Ok,
            ..Self::default()
        })
    }
    pub fn failure(session: u32, limits: Limits, error: Error) -> Self {
        Self {
            session,
            limits,
            state: State::TransportError,
            transport_error: error as u32,
            ..Self::default()
        }
    }
    pub fn age_ns(&self, now: u64) -> Option<u64> {
        if self.state != State::Ok || self.sampled_ns == 0 {
            return None;
        }
        now.checked_sub(self.sampled_ns)
    }
    pub fn fresh(&self, now: u64) -> bool {
        millicelsius(self.log.composite).is_some()
            && self.age_ns(now).is_some_and(|age| age <= MAX_AGE_NS)
    }
    pub fn encode(&self) -> [u8; STATUS_BYTES] {
        let mut b = [0; STATUS_BYTES];
        b[..4].copy_from_slice(b"NVH1");
        b[4] = 1;
        b[5] = self.state as u8;
        b[6] = self.log.critical_warning;
        b[8..12].copy_from_slice(&self.session.to_le_bytes());
        b[16..24].copy_from_slice(&self.sampled_ns.to_le_bytes());
        for (off, value) in [
            (24, self.log.composite),
            (26, self.limits.warning),
            (28, self.limits.critical),
            (30, self.command_status),
        ] {
            b[off..off + 2].copy_from_slice(&value.to_le_bytes());
        }
        for (i, value) in self.log.sensors.iter().enumerate() {
            b[32 + i * 2..34 + i * 2].copy_from_slice(&value.to_le_bytes());
        }
        b[48..52].copy_from_slice(&self.transport_error.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != STATUS_BYTES
            || &b[..4] != b"NVH1"
            || b[4] != 1
            || b[7] != 0
            || b[12..16].iter().chain(b[52..].iter()).any(|v| *v != 0)
        {
            return None;
        }
        let state = match b[5] {
            0 => State::Ok,
            1 => State::ControllerError,
            2 => State::TransportError,
            3 => State::Unavailable,
            _ => return None,
        };
        let mut sensors = [0; 8];
        for (i, out) in sensors.iter_mut().enumerate() {
            *out = at(b, 32 + 2 * i);
        }
        let h = Self {
            session: u32::from_le_bytes(b[8..12].try_into().ok()?),
            state,
            sampled_ns: u64::from_le_bytes(b[16..24].try_into().ok()?),
            limits: Limits {
                warning: at(b, 26),
                critical: at(b, 28),
            },
            log: Log {
                critical_warning: b[6],
                composite: at(b, 24),
                sensors,
            },
            command_status: at(b, 30),
            transport_error: u32::from_le_bytes(b[48..52].try_into().ok()?),
        };
        if h.session == 0
            || (h.state == State::Ok
                && (h.sampled_ns == 0 || h.command_status != 0 || h.transport_error != 0))
        {
            return None;
        }
        if h.state != State::Ok && (h.sampled_ns != 0 || h.log != Log::default()) {
            return None;
        }
        Some(h)
    }
    pub fn write_status(&self, out: &mut impl Write, now: u64) -> fmt::Result {
        if self.state != State::Ok {
            return write!(
                out,
                "smart: nvme unavailable ({:?}, status=0x{:x}, error={})",
                self.state, self.command_status, self.transport_error
            );
        }
        write!(out,"smart: nvme composite {} warning {} critical {} (Identify) flags=0x{:02x} thermal-warning={} ",Temperature(self.log.composite),Temperature(self.limits.warning),Temperature(self.limits.critical),self.log.critical_warning,self.log.thermal_warning())?;
        for (i, k) in self.log.sensors.iter().enumerate() {
            if millicelsius(*k).is_some() {
                write!(out, "sensor{}={} ", i + 1, Temperature(*k))?;
            }
        }
        match self.age_ns(now) {
            Some(age) => write!(out, "age={}ms fresh={}", age / 1_000_000, self.fresh(now)),
            None => write!(out, "age=unknown fresh=false"),
        }
    }
}

pub struct Temperature(pub u16);
impl fmt::Display for Temperature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match millicelsius(self.0) {
            Some(mc) => {
                let tenth = (mc.unsigned_abs() + 50) / 100;
                write!(
                    f,
                    "{}{}.{}C",
                    if mc < 0 { "-" } else { "" },
                    tenth / 10,
                    tenth % 10
                )
            }
            None => f.write_str("unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    #[test]
    fn command_is_read_only_bounded_global_with_64_bit_dma() {
        let c = command(0x60002000).unwrap();
        assert_eq!(c[0], 2);
        assert_eq!(c[1], u32::MAX);
        assert_eq!(c[6], 0x60002000);
        assert_eq!(c[10], 0x007f0002);
        for i in [2, 3, 4, 5, 7, 8, 9, 11, 12, 13, 14, 15] {
            assert_eq!(c[i], 0);
        }
        let high = command(0x1_2345_6000).unwrap();
        assert_eq!((high[6], high[7]), (0x2345_6000, 1));
        for a in [0, 1, 0x60002001, u64::MAX] {
            assert_eq!(command(a), Err(Error::Memory));
        }
    }
    #[test]
    fn unaligned_kelvin_and_optional_sensors_do_not_invent_zero_celsius() {
        let mut b = [0; 512];
        b[0] = 2;
        b[1..3].copy_from_slice(&304u16.to_le_bytes());
        b[200..202].copy_from_slice(&310u16.to_le_bytes());
        b[202..204].copy_from_slice(&u16::MAX.to_le_bytes());
        let l = Log::decode(&b).unwrap();
        assert_eq!(millicelsius(l.composite), Some(30850));
        assert!(l.thermal_warning());
        assert_eq!(millicelsius(l.sensors[0]), Some(36850));
        assert_eq!(millicelsius(l.sensors[1]), None);
        assert_eq!(millicelsius(0), None);
        assert_eq!(Log::decode(&b[..511]), Err(Error::Data));
        assert_eq!(std::format!("{}", Temperature(304)), "30.9C");
        assert_eq!(std::format!("{}", Temperature(273)), "-0.2C");
        assert_eq!(std::format!("{}", Temperature(0)), "unknown");
    }
    #[test]
    fn empty_successful_page_is_unavailable_without_poisoning_transport() {
        let health = Health::from_log(1, Limits::default(), &[0; 512], 100).unwrap();
        assert_eq!(health.state, State::Unavailable);
        assert_eq!(health.sampled_ns, 0);
        assert!(!health.fresh(101));
        assert_eq!(Health::decode(&health.encode()), Some(health));
    }
    #[test]
    fn limits_preserve_unreported_values_and_decode_little_endian() {
        let mut b = [0; 4096];
        b[266..268].copy_from_slice(&343u16.to_le_bytes());
        b[268..270].copy_from_slice(&353u16.to_le_bytes());
        assert_eq!(
            Limits::decode(&b).unwrap(),
            Limits {
                warning: 343,
                critical: 353
            }
        );
        assert_eq!(Limits::decode(&b[..512]), Err(Error::Data));
    }
    #[test]
    fn versioned_status_roundtrip_freshness_and_failed_read_replaces_old_data() {
        let h = Health {
            session: 1,
            state: State::Ok,
            sampled_ns: 100,
            log: Log {
                composite: 304,
                ..Log::default()
            },
            ..Health::default()
        };
        assert_eq!(Health::decode(&h.encode()), Some(h));
        assert!(h.fresh(101));
        assert!(!h.fresh(99));
        assert!(!h.fresh(101 + MAX_AGE_NS));
        let failed = Health::failure(1, Limits::default(), Error::Timeout);
        assert!(!failed.fresh(200));
        assert_eq!(failed.log.composite, 0);
        assert_eq!(Health::decode(&failed.encode()), Some(failed));
        let mut b = h.encode();
        b[4] = 2;
        assert!(Health::decode(&b).is_none());
        let mut b = h.encode();
        b[63] = 1;
        assert!(Health::decode(&b).is_none());
        let mut b = failed.encode();
        b[24] = 1;
        assert!(Health::decode(&b).is_none());
    }
}
