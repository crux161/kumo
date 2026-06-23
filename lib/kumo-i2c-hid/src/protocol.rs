/// HID-over-I2C protocol decoding failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    Truncated,
    InvalidDescriptorLength,
    UnsupportedVersion,
    InvalidInputLength,
    UnexpectedReportId,
    NotBootKeyboardReport,
}

/// The fixed 30-byte HID-over-I2C descriptor (protocol 1.0, section 5.2.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HidDescriptor {
    pub report_descriptor_length: u16,
    pub report_descriptor_register: u16,
    pub input_register: u16,
    pub max_input_length: u16,
    pub output_register: u16,
    pub max_output_length: u16,
    pub command_register: u16,
    pub data_register: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub version_id: u16,
}

impl HidDescriptor {
    pub const BYTES: usize = 30;

    pub fn parse(raw: &[u8]) -> Result<Self, ProtocolError> {
        if raw.len() < Self::BYTES {
            return Err(ProtocolError::Truncated);
        }
        if word(raw, 0) != Self::BYTES as u16 {
            return Err(ProtocolError::InvalidDescriptorLength);
        }
        if word(raw, 2) != 0x0100 {
            return Err(ProtocolError::UnsupportedVersion);
        }
        let descriptor = Self {
            report_descriptor_length: word(raw, 4),
            report_descriptor_register: word(raw, 6),
            input_register: word(raw, 8),
            max_input_length: word(raw, 10),
            output_register: word(raw, 12),
            max_output_length: word(raw, 14),
            command_register: word(raw, 16),
            data_register: word(raw, 18),
            vendor_id: word(raw, 20),
            product_id: word(raw, 22),
            version_id: word(raw, 24),
        };
        if descriptor.max_input_length < 2 || descriptor.report_descriptor_length == 0 {
            return Err(ProtocolError::InvalidInputLength);
        }
        Ok(descriptor)
    }
}

/// One HID-over-I2C input frame. The little-endian length includes its own two-byte field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputFrame<'a> {
    Reset,
    Report(&'a [u8]),
}

impl<'a> InputFrame<'a> {
    pub fn parse(raw: &'a [u8]) -> Result<Self, ProtocolError> {
        if raw.len() < 2 {
            return Err(ProtocolError::Truncated);
        }
        let length = word(raw, 0) as usize;
        if length == 0 {
            return Ok(Self::Reset);
        }
        if length < 2 || length > raw.len() {
            return Err(ProtocolError::InvalidInputLength);
        }
        Ok(Self::Report(&raw[2..length]))
    }
}

/// Extract the conventional eight-byte boot-keyboard payload, optionally consuming a report ID.
pub fn boot_keyboard_report(
    frame: InputFrame<'_>,
    report_id: Option<u8>,
) -> Result<[u8; 8], ProtocolError> {
    let InputFrame::Report(mut payload) = frame else {
        return Err(ProtocolError::NotBootKeyboardReport);
    };
    if let Some(expected) = report_id {
        let Some((&actual, rest)) = payload.split_first() else {
            return Err(ProtocolError::Truncated);
        };
        if actual != expected {
            return Err(ProtocolError::UnexpectedReportId);
        }
        payload = rest;
    }
    payload
        .try_into()
        .map_err(|_| ProtocolError::NotBootKeyboardReport)
}

const fn word(raw: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([raw[offset], raw[offset + 1]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_fixed_hid_descriptor() {
        let mut raw = [0u8; HidDescriptor::BYTES];
        for (offset, value) in [
            (0, 30),
            (2, 0x0100),
            (4, 63),
            (6, 0x20),
            (8, 0x30),
            (10, 11),
            (16, 0x40),
            (18, 0x42),
            (20, 0x17ef),
            (22, 0x60a3),
            (24, 0x0101),
        ] {
            raw[offset..offset + 2].copy_from_slice(&u16::to_le_bytes(value));
        }
        let descriptor = HidDescriptor::parse(&raw).unwrap();
        assert_eq!(descriptor.report_descriptor_length, 63);
        assert_eq!(descriptor.input_register, 0x30);
        assert_eq!(descriptor.max_input_length, 11);
        assert_eq!(descriptor.vendor_id, 0x17ef);
    }

    #[test]
    fn input_length_bounds_the_report_and_zero_means_reset() {
        assert_eq!(InputFrame::parse(&[0, 0]), Ok(InputFrame::Reset));
        assert_eq!(
            InputFrame::parse(&[4, 0, 0xaa, 0xbb, 0xcc]),
            Ok(InputFrame::Report(&[0xaa, 0xbb]))
        );
        assert_eq!(
            InputFrame::parse(&[8, 0, 1]),
            Err(ProtocolError::InvalidInputLength)
        );
    }

    #[test]
    fn extracts_boot_report_with_or_without_report_id() {
        let report = [2, 0, 4, 0, 0, 0, 0, 0];
        assert_eq!(
            boot_keyboard_report(InputFrame::Report(&report), None),
            Ok(report)
        );
        let mut identified = [0u8; 9];
        identified[0] = 7;
        identified[1..].copy_from_slice(&report);
        assert_eq!(
            boot_keyboard_report(InputFrame::Report(&identified), Some(7)),
            Ok(report)
        );
    }
}
