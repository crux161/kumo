//j809
//j826
//! ax88 (AX88179/AX88178A) descriptor-only inventory. No device commands or DMA.
//! A match records topology; it does not license configuring endpoints. — TIMBERDOODLE

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Family {
    Ax88179,
    Ax88178a,
}

pub fn family(device: &[u8]) -> Option<Family> {
    if device.len() != 18 || device[0..2] != [18, 1] || device[8..10] != [0x95, 0x0b] {
        return None;
    }
    match u16::from_le_bytes([device[10], device[11]]) {
        0x1790 => Some(Family::Ax88179),
        0x178a => Some(Family::Ax88178a),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Companion {
    pub max_burst: u8,
    pub attributes: u8,
    pub bytes_per_interval: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub address: u8,
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
    pub companion: Option<Companion>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Inventory {
    pub family: Family,
    pub usb_revision: u16,
    pub device_revision: u16,
    pub ep0_encoding: u8,
    pub configuration: u8,
    pub interface: u8,
    pub alternate: u8,
    pub endpoints: [Endpoint; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Device,
    Length,
    Descriptor,
    Configuration,
    Interface,
    Endpoint,
    Companion,
}

/// Only the two explicit ASIX IDs and one vendor ff/ff/00 alt0 interface. Unknown
/// IDs are ignored; unsupported layouts refuse rather than guessing a protocol.
/// Full-buffer length is mandatory: never parse zero-filled DMA tail as a read.
/// — TIMBERDOODLE
pub fn inspect(device: &[u8], config: &[u8], superspeed: bool) -> Result<Option<Inventory>, Error> {
    let Some(family) = family(device) else {
        return Ok(None);
    };
    if device[17] != 1 {
        return Err(Error::Device);
    }
    if config.len() < 9 || config.len() > 256 || config[0..2] != [9, 2] {
        return Err(Error::Length);
    }
    if u16::from_le_bytes([config[2], config[3]]) as usize != config.len() {
        return Err(Error::Length);
    }
    if config[4] != 1 || config[5] == 0 || config[7] & 0x80 == 0 {
        return Err(Error::Configuration);
    }
    let mut at = 9;
    let mut interface = None;
    let mut endpoints = [None; 3];
    let mut count = 0;
    let mut pending_companion = None;
    while at < config.len() {
        if config.len() - at < 2 {
            return Err(Error::Descriptor);
        }
        let len = config[at] as usize;
        if len < 2 || len > config.len() - at {
            return Err(Error::Descriptor);
        }
        let d = &config[at..at + len];
        if superspeed && pending_companion.is_some() && d[1] != 0x30 {
            return Err(Error::Companion);
        }
        match d[1] {
            2 => return Err(Error::Configuration),
            4 => {
                if len != 9 || interface.is_some() || d[3] != 0 || d[4..8] != [3, 0xff, 0xff, 0] {
                    return Err(Error::Interface);
                }
                interface = Some(d[2]);
            }
            5 => {
                if len != 7 || interface.is_none() || count == endpoints.len() {
                    return Err(Error::Endpoint);
                }
                let address = d[2];
                let attributes = d[3];
                let mps = u16::from_le_bytes([d[4], d[5]]);
                if address & 0x0f == 0
                    || address & 0x70 != 0
                    || !matches!(attributes & 3, 2 | 3)
                    || (attributes & 3 == 3 && address & 0x80 == 0)
                    || mps & 0xe000 != 0
                    || mps & 0x7ff == 0
                    || mps & 0x7ff > 1024
                    || endpoints[..count]
                        .iter()
                        .flatten()
                        .any(|e: &Endpoint| e.address == address)
                {
                    return Err(Error::Endpoint);
                }
                endpoints[count] = Some(Endpoint {
                    address,
                    attributes,
                    max_packet_size: mps,
                    interval: d[6],
                    companion: None,
                });
                pending_companion = Some(count);
                count += 1;
            }
            0x30 => {
                let Some(index) = pending_companion.take() else {
                    return Err(Error::Companion);
                };
                if !superspeed || len != 6 || d[2] > 15 {
                    return Err(Error::Companion);
                }
                let ep = endpoints[index].as_mut().ok_or(Error::Companion)?;
                let bytes_per_interval = u16::from_le_bytes([d[4], d[5]]);
                if ep.attributes & 3 == 2 && bytes_per_interval != 0 {
                    return Err(Error::Companion);
                }
                ep.companion = Some(Companion {
                    max_burst: d[2],
                    attributes: d[3],
                    bytes_per_interval,
                });
            }
            _ => {
                pending_companion = None;
            }
        }
        at += len;
    }
    if superspeed && pending_companion.is_some() {
        return Err(Error::Companion);
    }
    let interface = interface.ok_or(Error::Interface)?;
    if count != 3 {
        return Err(Error::Endpoint);
    }
    let endpoints = endpoints.map(|e| e.unwrap());
    let bulk_in = endpoints
        .iter()
        .filter(|e| e.attributes & 3 == 2 && e.address & 0x80 != 0)
        .count();
    let bulk_out = endpoints
        .iter()
        .filter(|e| e.attributes & 3 == 2 && e.address & 0x80 == 0)
        .count();
    let status = endpoints.iter().filter(|e| e.attributes & 3 == 3).count();
    if (bulk_in, bulk_out, status) != (1, 1, 1) {
        return Err(Error::Endpoint);
    }
    Ok(Some(Inventory {
        family,
        usb_revision: u16::from_le_bytes([device[2], device[3]]),
        device_revision: u16::from_le_bytes([device[12], device[13]]),
        ep0_encoding: device[7],
        configuration: config[5],
        interface,
        alternate: 0,
        endpoints,
    }))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn device(pid: u16) -> [u8; 18] {
        let [lo, hi] = pid.to_le_bytes();
        [
            18, 1, 0, 3, 0, 0, 0, 9, 0x95, 0x0b, lo, hi, 0, 1, 1, 2, 3, 1,
        ]
    }
    fn config(ss: bool) -> Vec<u8> {
        let mut b = std::vec![9, 2, 0, 0, 1, 1, 0, 0x80, 125, 9, 4, 0, 0, 3, 0xff, 0xff, 0, 0];
        for (addr, kind, mps, interval) in [
            (0x81, 3, 8u16, 11),
            (0x82, 2, if ss { 1024 } else { 512 }, 0),
            (3, 2, if ss { 1024 } else { 512 }, 0),
        ] {
            let [lo, hi] = mps.to_le_bytes();
            b.extend([7, 5, addr, kind, lo, hi, interval]);
            if ss {
                b.extend([
                    6,
                    0x30,
                    if kind == 2 { 3 } else { 0 },
                    0,
                    if kind == 3 { 8 } else { 0 },
                    0,
                ]);
            }
        }
        let len = (b.len() as u16).to_le_bytes();
        b[2..4].copy_from_slice(&len);
        b
    }
    #[test]
    fn explicit_family_ids_and_unknown_ids() {
        assert_eq!(family(&device(0x1790)), Some(Family::Ax88179));
        assert_eq!(family(&device(0x178a)), Some(Family::Ax88178a));
        assert_eq!(inspect(&device(0x1791), &[], true), Ok(None));
        let mut d = device(0x1790);
        d[8] = 0x96;
        assert_eq!(family(&d), None);
        for n in 0..18 {
            assert_eq!(family(&device(0x1790)[..n]), None);
        }
    }
    #[test]
    fn both_families_inventory_usb2_and_superspeed() {
        for pid in [0x1790, 0x178a] {
            for ss in [false, true] {
                let i = inspect(&device(pid), &config(ss), ss).unwrap().unwrap();
                assert_eq!(i.interface, 0);
                assert_eq!(i.configuration, 1);
                assert_eq!(i.device_revision, 0x100);
                assert_eq!(i.endpoints[1].address, 0x82);
                assert_eq!(i.endpoints[1].max_packet_size, if ss { 1024 } else { 512 });
                assert_eq!(
                    i.endpoints[1].companion.map(|c| c.max_burst),
                    if ss { Some(3) } else { None }
                );
            }
        }
    }
    #[test]
    fn every_truncation_and_wrong_total_refuses() {
        for ss in [false, true] {
            let c = config(ss);
            for n in 0..c.len() {
                assert!(inspect(&device(0x1790), &c[..n], ss).is_err(), "{ss}:{n}");
            }
            for len in [0, 8, 256, 65535] {
                let mut b = c.clone();
                b[2..4].copy_from_slice(&(len as u16).to_le_bytes());
                assert_eq!(inspect(&device(0x1790), &b, ss), Err(Error::Length));
            }
        }
    }
    #[test]
    fn malformed_descriptors_and_unsupported_layouts_refuse() {
        let d = device(0x1790);
        for (at, value) in [
            (4, 2),
            (5, 0),
            (7, 0),
            (9, 0),
            (9, 1),
            (9, 255),
            (12, 1),
            (13, 2),
            (14, 2),
            (18, 6),
            (20, 0),
            (20, 0x91),
            (21, 0),
            (22, 0),
            (23, 0xe0),
            (27, 0x81),
            (34, 0x84),
        ] {
            let mut c = config(false);
            c[at] = value;
            assert!(inspect(&d, &c, false).is_err(), "byte{at}={value}");
        }
        let mut d = d;
        d[17] = 2;
        assert_eq!(inspect(&d, &config(false), false), Err(Error::Device));
    }
    #[test]
    fn superspeed_companions_must_be_adjacent_unique_and_bounded() {
        let d = device(0x1790);
        assert!(inspect(&d, &config(false), true).is_err());
        assert!(inspect(&d, &config(true), false).is_err());
        for (at, value) in [(25, 0x31), (26, 16), (48, 1)] {
            let mut c = config(true);
            c[at] = value;
            assert!(inspect(&d, &c, true).is_err(), "byte{at}");
        }
    }
    #[test]
    fn arbitrary_single_byte_corruption_cannot_panic() {
        for ss in [false, true] {
            let source = config(ss);
            for at in 0..source.len() {
                for value in 0..=255 {
                    let mut c = source.clone();
                    c[at] = value;
                    let _ = inspect(&device(0x1790), &c, ss);
                }
            }
        }
    }
}

/// Data and status must BOTH complete for a descriptor read. A successful status
/// stage alone cannot prove that the requested data bytes arrived. — TIMBERDOODLE
#[derive(Default)]
pub struct ReadCompletion {
    data: bool,
    status: bool,
}
impl ReadCompletion {
    pub fn observe(
        &mut self,
        data: bool,
        status: bool,
        cc: u8,
        remaining: u32,
    ) -> Result<bool, Error> {
        if data == status
            || cc != 1
            || remaining != 0
            || (data && self.data)
            || (status && (self.status || !self.data))
        {
            return Err(Error::Descriptor);
        }
        if data {
            self.data = true;
        } else {
            self.status = true;
        }
        Ok(self.data && self.status)
    }
}
#[cfg(test)]
mod completion_tests {
    use super::*;
    #[test]
    fn data_then_status_proves_exact_read() {
        let mut c = ReadCompletion::default();
        assert_eq!(c.observe(true, false, 1, 0), Ok(false));
        assert_eq!(c.observe(false, true, 1, 0), Ok(true));
    }
    #[test]
    fn short_error_duplicate_or_status_without_data_never_proves_read() {
        for (d, s, cc, rem) in [
            (true, false, 13, 1),
            (true, false, 1, 1),
            (true, false, 6, 0),
            (false, true, 1, 0),
            (false, false, 1, 0),
            (true, true, 1, 0),
        ] {
            assert!(ReadCompletion::default().observe(d, s, cc, rem).is_err());
        }
        let mut c = ReadCompletion::default();
        c.observe(true, false, 1, 0).unwrap();
        assert!(c.observe(true, false, 1, 0).is_err());
        assert!(c.observe(false, true, 6, 0).is_err());
    }
}
