//j896
//! Raw report collection for CRUX's 1a34:0203 arcade stick; no guessed control map. — KESTREL
use core::fmt;

pub const VID: u16 = 0x1a34;
pub const PID: u16 = 0x0203;
pub const MAX_REPORT_BYTES: usize = 64;
pub const MAX_CAPTURE_REPORTS: u16 = 128;

/// Captured from the tested 1a34:0203. First 56 input bits are 12 buttons + padding,
/// hat + padding, and four axes; the following 20 bytes are vendor-defined. Byte 19
/// alternates while idle. Filter control changes only for this exact descriptor,
/// while printing every byte of each selected raw report. — KESTREL
pub const REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x15, 0x00, 0x25, 0x01, 0x35, 0x00, 0x45, 0x01, 0x75, 0x01,
    0x95, 0x0c, 0x05, 0x09, 0x19, 0x01, 0x29, 0x0c, 0x81, 0x02, 0x95, 0x04, 0x81, 0x01, 0x05, 0x01,
    0x25, 0x07, 0x46, 0x3b, 0x01, 0x75, 0x04, 0x95, 0x01, 0x65, 0x14, 0x09, 0x39, 0x81, 0x42, 0x65,
    0x00, 0x95, 0x01, 0x81, 0x01, 0x26, 0xff, 0x00, 0x46, 0xff, 0x00, 0x09, 0x30, 0x09, 0x31, 0x09,
    0x32, 0x09, 0x35, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02, 0x06, 0x00, 0xff, 0x09, 0x20, 0x09, 0x21,
    0x09, 0x22, 0x09, 0x23, 0x09, 0x24, 0x09, 0x25, 0x09, 0x26, 0x09, 0x27, 0x09, 0x28, 0x09, 0x29,
    0x09, 0x2a, 0x09, 0x2b, 0x95, 0x0c, 0x81, 0x02, 0x26, 0xff, 0x03, 0x46, 0xff, 0x03, 0x09, 0x2c,
    0x09, 0x2d, 0x09, 0x2e, 0x09, 0x2f, 0x75, 0x10, 0x95, 0x04, 0x81, 0x02, 0x05, 0x08, 0x09, 0x43,
    0x15, 0x00, 0x26, 0xff, 0x00, 0x35, 0x00, 0x46, 0xff, 0x00, 0x75, 0x08, 0x95, 0x01, 0x91, 0x82,
    0x09, 0x44, 0x91, 0x82, 0x09, 0x45, 0x91, 0x82, 0x09, 0x46, 0x91, 0x82, 0xc0,
];

pub fn matches(device: &[u8]) -> bool {
    device.len() == 18
        && device[0] == 18
        && device[1] == 1
        && u16::from_le_bytes([device[8], device[9]]) == VID
        && u16::from_le_bytes([device[10], device[11]]) == PID
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interface {
    pub interface_number: u8,
    pub in_endpoint_address: u8,
    pub max_packet_size: u16,
    pub interval: u8,
    pub report_descriptor_bytes: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Malformed,
    Unsupported,
    Ambiguous,
}

struct Candidate {
    number: u8,
    endpoints: u8,
    seen: u8,
    report_bytes: Option<u16>,
    endpoint: Option<(u8, u16, u8)>,
}
impl Candidate {
    fn finish(self) -> Result<Interface, Error> {
        if self.endpoints != self.seen {
            return Err(Error::Malformed);
        }
        let (in_endpoint_address, max_packet_size, interval) =
            self.endpoint.ok_or(Error::Unsupported)?;
        Ok(Interface {
            interface_number: self.number,
            in_endpoint_address,
            max_packet_size,
            interval,
            report_descriptor_bytes: self.report_bytes.ok_or(Error::Unsupported)?,
        })
    }
}

/// Only a complete configuration's alternate zero, HID subclass/protocol zero. Reject rather
/// than guess between interfaces/endpoints or silently truncate the descriptor. — KESTREL
pub fn find(device: &[u8], config: &[u8]) -> Result<Option<Interface>, Error> {
    if !matches(device) {
        return Ok(None);
    }
    if config.len() < 9
        || config[0] != 9
        || config[1] != 2
        || usize::from(u16::from_le_bytes([config[2], config[3]])) != config.len()
        || config[5] == 0
    {
        return Err(Error::Malformed);
    }
    let mut offset = 9;
    let mut current: Option<Candidate> = None;
    let mut found = None;
    while offset < config.len() {
        if offset + 2 > config.len() {
            return Err(Error::Malformed);
        }
        let len = usize::from(config[offset]);
        if len < 2 || offset + len > config.len() {
            return Err(Error::Malformed);
        }
        let b = &config[offset..offset + len];
        match b[1] {
            4 => {
                if len != 9 {
                    return Err(Error::Malformed);
                }
                if let Some(c) = current.take() {
                    if found.replace(c.finish()?).is_some() {
                        return Err(Error::Ambiguous);
                    }
                }
                if b[3] == 0 && b[5..8] == [3, 0, 0] {
                    current = Some(Candidate {
                        number: b[2],
                        endpoints: b[4],
                        seen: 0,
                        report_bytes: None,
                        endpoint: None,
                    });
                }
            }
            0x21 => {
                if let Some(c) = current.as_mut() {
                    if len < 6 || len != 6 + usize::from(b[5]) * 3 {
                        return Err(Error::Malformed);
                    }
                    for descriptor in b[6..].chunks_exact(3) {
                        if descriptor[0] == 0x22 {
                            let bytes = u16::from_le_bytes([descriptor[1], descriptor[2]]);
                            if bytes == 0 || bytes > 256 {
                                return Err(Error::Unsupported);
                            }
                            if c.report_bytes.replace(bytes).is_some() {
                                return Err(Error::Ambiguous);
                            }
                        }
                    }
                }
            }
            5 => {
                if len < 7 {
                    return Err(Error::Malformed);
                }
                if let Some(c) = current.as_mut() {
                    c.seen = c.seen.checked_add(1).ok_or(Error::Malformed)?;
                    if b[2] & 0x80 != 0 && b[3] & 3 == 3 {
                        let mps = u16::from_le_bytes([b[4], b[5]]);
                        if b[2] & 0x70 != 0
                            || b[2] & 15 == 0
                            || mps == 0
                            || usize::from(mps) > MAX_REPORT_BYTES
                            || b[6] == 0
                        {
                            return Err(Error::Unsupported);
                        }
                        if c.endpoint.replace((b[2], mps, b[6])).is_some() {
                            return Err(Error::Ambiguous);
                        }
                    }
                }
            }
            _ => {}
        }
        offset += len;
    }
    if let Some(c) = current {
        if found.replace(c.finish()?).is_some() {
            return Err(Error::Ambiguous);
        }
    }
    Ok(found)
}

/// Bound diagnostic output even if the device streams noise; valid duplicates cost no output.
/// No release/press semantics are inferred and the Report ID stays in byte zero. — KESTREL
pub struct Capture {
    last: [u8; MAX_REPORT_BYTES],
    len: usize,
    count: u16,
    control_prefix: Option<usize>,
}
impl Default for Capture {
    fn default() -> Self {
        Self {
            last: [0; MAX_REPORT_BYTES],
            len: 0,
            count: 0,
            control_prefix: None,
        }
    }
}
impl Capture {
    pub fn for_descriptor(descriptor: &[u8]) -> Self {
        Self {
            control_prefix: (descriptor == REPORT_DESCRIPTOR).then_some(7),
            ..Self::default()
        }
    }
    pub fn control_prefix_bytes(&self) -> Option<usize> {
        self.control_prefix
    }
    pub fn changed(&mut self, report: &[u8]) -> bool {
        let compare = self
            .control_prefix
            .unwrap_or(report.len())
            .min(report.len());
        if report.is_empty()
            || report.len() > MAX_REPORT_BYTES
            || self.count == MAX_CAPTURE_REPORTS
            || (self.len == report.len() && self.last[..compare] == report[..compare])
        {
            return false;
        }
        self.last[..report.len()].copy_from_slice(report);
        self.len = report.len();
        self.count += 1;
        true
    }
    pub fn count(&self) -> u16 {
        self.count
    }
}

pub struct Hex<'a>(pub &'a [u8]);
impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if i != 0 {
                f.write_str(" ")?;
            }
            write!(f, "{b:02X}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn device() -> [u8; 18] {
        let mut d = [0; 18];
        d[..2].copy_from_slice(&[18, 1]);
        d[8..12].copy_from_slice(&[0x34, 0x1a, 3, 2]);
        d
    }
    // Synthetic descriptors; the real device's descriptor is a separate metal gate.
    fn config() -> [u8; 34] {
        [
            9, 2, 34, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 1, 3, 0, 0, 0, 9, 0x21, 0x11, 1, 0, 1,
            0x22, 84, 0, 7, 5, 0x81, 3, 64, 0, 10,
        ]
    }
    #[test]
    fn exact_identity_and_nonboot_interrupt_interface() {
        let i = find(&device(), &config()).unwrap().unwrap();
        assert_eq!(
            i,
            Interface {
                interface_number: 0,
                in_endpoint_address: 0x81,
                max_packet_size: 64,
                interval: 10,
                report_descriptor_bytes: 84
            }
        );
        let mut other = device();
        other[10] ^= 1;
        assert_eq!(find(&other, &config()), Ok(None));
        for end in 0..18 {
            assert!(!matches(&device()[..end]));
        }
    }
    #[test]
    fn truncation_and_bad_descriptor_lengths_never_match() {
        let good = config();
        for end in 0..good.len() {
            assert!(find(&device(), &good[..end]).is_err());
        }
        for index in [9, 18, 27] {
            for len in [0, 1, 255] {
                let mut bad = good;
                bad[index] = len;
                assert!(find(&device(), &bad).is_err());
            }
        }
        let mut bad = good;
        bad[13] = 2;
        assert_eq!(find(&device(), &bad), Err(Error::Malformed));
    }
    #[test]
    fn boot_alternates_and_cross_interface_endpoints_are_not_arcade() {
        for (index, value) in [(12, 1), (15, 1), (16, 1)] {
            let mut c = config();
            c[index] = value;
            assert_eq!(find(&device(), &c), Ok(None));
        }
        let mut c = config().to_vec();
        c.splice(27..27, [9, 4, 1, 0, 1, 0xff, 0, 0, 0]);
        let n = c.len() as u16;
        c[2..4].copy_from_slice(&n.to_le_bytes());
        assert!(find(&device(), &c).is_err());
    }
    #[test]
    fn refuse_missing_or_ambiguous_reports_and_endpoints() {
        let mut c = config();
        c[24] = 0x23;
        assert_eq!(find(&device(), &c), Err(Error::Unsupported));
        for index in [29, 30, 31, 33] {
            let mut c = config();
            c[index] = 0;
            assert!(find(&device(), &c).is_err());
        }
        let mut c = config();
        c[31] = 65;
        assert_eq!(find(&device(), &c), Err(Error::Unsupported));
        let mut c = config();
        c[26] = 2;
        assert_eq!(find(&device(), &c), Err(Error::Unsupported));
        let mut c = config().to_vec();
        c.extend_from_slice(&config()[27..]);
        c[13] = 2;
        let n = c.len() as u16;
        c[2..4].copy_from_slice(&n.to_le_bytes());
        assert_eq!(find(&device(), &c), Err(Error::Ambiguous));
        let mut c = config().to_vec();
        c.extend_from_slice(&config()[9..]);
        let n = c.len() as u16;
        c[2..4].copy_from_slice(&n.to_le_bytes());
        assert_eq!(find(&device(), &c), Err(Error::Ambiguous));
    }
    #[test]
    fn verified_descriptor_ignores_vendor_churn_but_preserves_control_and_length_changes() {
        let mut capture = Capture::for_descriptor(REPORT_DESCRIPTOR);
        assert_eq!(capture.control_prefix_bytes(), Some(7));
        let mut raw = [0; 27];
        assert!(capture.changed(&raw));
        raw[19] = 1;
        assert!(!capture.changed(&raw));
        raw[0] = 1; // Button field.
        assert!(capture.changed(&raw));
        raw[19] = 0;
        assert!(!capture.changed(&raw));
        raw[3] = 0x80; // Axis field.
        assert!(capture.changed(&raw));
        raw[..7].fill(0); // Release/neutral.
        assert!(capture.changed(&raw));
        assert!(capture.changed(&raw[..7])); // Never discard a changed wire length.
        let mut unknown = REPORT_DESCRIPTOR.to_vec();
        unknown[0] ^= 1;
        let mut capture = Capture::for_descriptor(&unknown);
        assert_eq!(capture.control_prefix_bytes(), None);
        assert!(capture.changed(&raw));
        raw[19] ^= 1;
        assert!(capture.changed(&raw));
    }

    #[test]
    fn raw_reports_keep_id_length_release_and_zeros_with_a_hard_output_bound() {
        let mut c = Capture::default();
        assert!(!c.changed(&[]));
        assert!(!c.changed(&[0; 65]));
        assert!(c.changed(&[0; 64]));
        assert!(!c.changed(&[0; 64]));
        assert!(c.changed(&[1, 0, 0x80]));
        assert!(!c.changed(&[1, 0, 0x80]));
        assert!(c.changed(&[1, 0, 0]));
        assert!(c.changed(&[1, 0, 0, 0]));
        for i in c.count()..MAX_CAPTURE_REPORTS {
            assert!(c.changed(&i.to_le_bytes()));
        }
        assert!(!c.changed(&[0xff]));
        assert_eq!(c.count(), MAX_CAPTURE_REPORTS);
    }
}
