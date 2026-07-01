//j379
/// HID-over-I2C protocol decoding failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolError {
    Truncated,
    InvalidDescriptorLength,
    UnsupportedVersion,
    InvalidInputLength,
    UnexpectedReportId,
    NotBootKeyboardReport,
    NotBootMouseReport,
}

/// Why formatting a host-to-device output report failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputReportError {
    BufferTooSmall,
    PayloadTooLong,
}

/// Bytes in a USB HID boot-mouse input report after any Report ID prefix.
pub const BOOT_MOUSE_REPORT_BYTES: usize = 3;

/// Button bitmap from byte zero of a boot-mouse report.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MouseButtons(u8);

impl MouseButtons {
    pub const LEFT: u8 = 1 << 0;
    pub const RIGHT: u8 = 1 << 1;
    pub const MIDDLE: u8 = 1 << 2;

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & (Self::LEFT | Self::RIGHT | Self::MIDDLE))
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn left(self) -> bool {
        self.0 & Self::LEFT != 0
    }

    pub const fn right(self) -> bool {
        self.0 & Self::RIGHT != 0
    }

    pub const fn middle(self) -> bool {
        self.0 & Self::MIDDLE != 0
    }
}

/// Conventional three-byte USB HID boot-mouse payload.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BootMouseReport {
    pub buttons: MouseButtons,
    pub x_delta: i8,
    pub y_delta: i8,
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
    report_payload(frame, report_id, ProtocolError::NotBootKeyboardReport)?
        .try_into()
        .map_err(|_| ProtocolError::NotBootKeyboardReport)
}

/// Extract and decode the conventional three-byte boot-mouse payload.
pub fn boot_mouse_report(
    frame: InputFrame<'_>,
    report_id: Option<u8>,
) -> Result<BootMouseReport, ProtocolError> {
    let payload = report_payload(frame, report_id, ProtocolError::NotBootMouseReport)?;
    // Take the boot buttons+X+Y prefix; a report-protocol pointing report (the X13s Elan mouse,
    // report-id 0x01) carries trailing fields we ignore. A payload shorter than the prefix is still
    // rejected. — CORVUS
    let raw: [u8; BOOT_MOUSE_REPORT_BYTES] = payload
        .get(..BOOT_MOUSE_REPORT_BYTES)
        .ok_or(ProtocolError::NotBootMouseReport)?
        .try_into()
        .map_err(|_| ProtocolError::NotBootMouseReport)?;
    Ok(BootMouseReport {
        buttons: MouseButtons::from_bits(raw[0]),
        x_delta: raw[1] as i8,
        y_delta: raw[2] as i8,
    })
}

fn report_payload<'a>(
    frame: InputFrame<'a>,
    report_id: Option<u8>,
    wrong_shape: ProtocolError,
) -> Result<&'a [u8], ProtocolError> {
    let InputFrame::Report(mut payload) = frame else {
        return Err(wrong_shape);
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
    Ok(payload)
}

const fn word(raw: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([raw[offset], raw[offset + 1]])
}

/// HID-over-I2C power state operand for `SET_POWER` (spec 1.0 §7.2.8; Linux `I2C_HID_PWR_*`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerState {
    On = 0x00,
    Sleep = 0x01,
}

/// Builds the 4-byte HID-over-I2C commands the driver writes to the command register to bring the
/// device up. Layout matches the spec 1.0 §7.2 short form and Linux `i2c_hid_encode_command`:
/// the command-register address as two little-endian bytes, then `report_type << 4 | report_id`,
/// then the opcode. `SET_POWER` and `RESET` both use report_type=0 and a report_id below 0x0F
/// (the power state, or 0), so neither needs the extended report-id byte.
///
/// Bring-up sequence (Linux `i2c_hid_start_hwreset`): `SET_POWER(On)` then `RESET`; the device
/// answers RESET with a length-0 input report — the reset-complete sync, decoded as
/// [`InputFrame::Reset`]. — CORVUS
pub struct Command;

impl Command {
    const OPCODE_RESET: u8 = 0x01;
    const OPCODE_SET_REPORT: u8 = 0x03;
    const OPCODE_SET_POWER: u8 = 0x08;
    const REPORT_TYPE_OUTPUT: u8 = 0x02;

    /// `SET_POWER` with the given power state.
    pub fn set_power(command_register: u16, state: PowerState) -> [u8; 4] {
        Self::encode(command_register, state as u8, Self::OPCODE_SET_POWER)
    }

    /// `RESET`. The device signals completion with a length-0 input report.
    pub fn reset(command_register: u16) -> [u8; 4] {
        Self::encode(command_register, 0, Self::OPCODE_RESET)
    }

    /// `SET_REPORT` (Output).
    pub fn set_report(command_register: u16, report_id: u8) -> [u8; 4] {
        let [lo, hi] = command_register.to_le_bytes();
        [
            lo,
            hi,
            (Self::REPORT_TYPE_OUTPUT << 4) | (report_id & 0x0F),
            Self::OPCODE_SET_REPORT,
        ]
    }

    /// Linux-style plain OUTPUT report: output register, two-byte total report length, optional
    /// Report ID, then payload bytes.
    pub fn plain_output_report(
        output_register: u16,
        report_id: Option<u8>,
        payload: &[u8],
        out: &mut [u8],
    ) -> Result<usize, OutputReportError> {
        let mut len = push_word(out, 0, output_register)?;
        len = format_report(out, len, report_id, payload)?;
        Ok(len)
    }

    /// Linux-style SET_REPORT(Output): command register, encoded SET_REPORT, data register, then
    /// the formatted report payload.
    pub fn set_output_report(
        command_register: u16,
        data_register: u16,
        report_id: Option<u8>,
        payload: &[u8],
        out: &mut [u8],
    ) -> Result<usize, OutputReportError> {
        let report_id_byte = report_id.unwrap_or(0);
        let mut len = push_word(out, 0, command_register)?;
        len = encode_report_command(
            out,
            len,
            Self::OPCODE_SET_REPORT,
            Self::REPORT_TYPE_OUTPUT,
            report_id_byte,
        )?;
        len = push_word(out, len, data_register)?;
        len = format_report(out, len, report_id, payload)?;
        Ok(len)
    }

    fn encode(command_register: u16, report_id: u8, opcode: u8) -> [u8; 4] {
        let [lo, hi] = command_register.to_le_bytes();
        // report_type is always 0 here, report_id < 0x0F → short form, no extended id byte.
        [lo, hi, report_id & 0x0F, opcode]
    }
}

fn format_report(
    out: &mut [u8],
    offset: usize,
    report_id: Option<u8>,
    payload: &[u8],
) -> Result<usize, OutputReportError> {
    let report_len = 2usize
        .checked_add(if report_id.is_some() { 1 } else { 0 })
        .and_then(|len| len.checked_add(payload.len()))
        .filter(|len| *len <= u16::MAX as usize)
        .ok_or(OutputReportError::PayloadTooLong)?;
    let mut len = push_word(out, offset, report_len as u16)?;
    if let Some(id) = report_id {
        len = push_byte(out, len, id)?;
    }
    for &byte in payload {
        len = push_byte(out, len, byte)?;
    }
    Ok(len)
}

fn encode_report_command(
    out: &mut [u8],
    offset: usize,
    opcode: u8,
    report_type: u8,
    report_id: u8,
) -> Result<usize, OutputReportError> {
    let mut len = offset;
    if report_id < 0x0f {
        len = push_byte(out, len, (report_type << 4) | report_id)?;
        len = push_byte(out, len, opcode)?;
    } else {
        len = push_byte(out, len, (report_type << 4) | 0x0f)?;
        len = push_byte(out, len, opcode)?;
        len = push_byte(out, len, report_id)?;
    }
    Ok(len)
}

fn push_word(out: &mut [u8], offset: usize, value: u16) -> Result<usize, OutputReportError> {
    let [lo, hi] = value.to_le_bytes();
    let len = push_byte(out, offset, lo)?;
    push_byte(out, len, hi)
}

fn push_byte(out: &mut [u8], offset: usize, value: u8) -> Result<usize, OutputReportError> {
    if offset >= out.len() {
        return Err(OutputReportError::BufferTooSmall);
    }
    out[offset] = value;
    Ok(offset + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_power_on_matches_spec_byte_layout() {
        // cmd-reg 0x0005 LE, report_type/id byte 0x00, SET_POWER opcode 0x08.
        assert_eq!(
            Command::set_power(0x0005, PowerState::On),
            [0x05, 0x00, 0x00, 0x08]
        );
    }

    #[test]
    fn set_power_sleep_carries_state_in_low_nibble() {
        assert_eq!(
            Command::set_power(0x0005, PowerState::Sleep),
            [0x05, 0x00, 0x01, 0x08]
        );
    }

    #[test]
    fn reset_matches_spec_byte_layout() {
        assert_eq!(Command::reset(0x0005), [0x05, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn command_register_address_is_little_endian() {
        assert_eq!(Command::reset(0x1234), [0x34, 0x12, 0x00, 0x01]);
    }

    #[test]
    fn plain_output_report_matches_linux_format_report_shape() {
        let mut out = [0u8; 8];
        let len = Command::plain_output_report(0x0025, Some(8), &[0x02], &mut out).unwrap();
        assert_eq!(len, 6);
        assert_eq!(&out[..len], &[0x25, 0x00, 0x04, 0x00, 0x08, 0x02]);

        let len = Command::plain_output_report(0x0025, None, &[0x02], &mut out).unwrap();
        assert_eq!(len, 5);
        assert_eq!(&out[..len], &[0x25, 0x00, 0x03, 0x00, 0x02]);
    }

    #[test]
    fn set_output_report_matches_linux_command_envelope() {
        let mut out = [0u8; 12];
        let len = Command::set_output_report(0x0040, 0x0042, Some(8), &[0x02], &mut out).unwrap();
        assert_eq!(len, 10);
        assert_eq!(
            &out[..len],
            &[0x40, 0x00, 0x28, 0x03, 0x42, 0x00, 0x04, 0x00, 0x08, 0x02]
        );
    }

    #[test]
    fn set_output_report_uses_extended_report_id_encoding() {
        let mut out = [0u8; 12];
        let len =
            Command::set_output_report(0x0040, 0x0042, Some(0x15), &[0x02], &mut out).unwrap();
        assert_eq!(len, 11);
        assert_eq!(
            &out[..len],
            &[0x40, 0x00, 0x2f, 0x03, 0x15, 0x42, 0x00, 0x04, 0x00, 0x15, 0x02,]
        );
    }

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

    #[test]
    fn decodes_boot_mouse_report_with_or_without_report_id() {
        let raw = [0x05, 0x7f, 0x81];
        let report = BootMouseReport {
            buttons: MouseButtons::from_bits(MouseButtons::LEFT | MouseButtons::MIDDLE),
            x_delta: 127,
            y_delta: -127,
        };
        assert_eq!(
            boot_mouse_report(InputFrame::Report(&raw), None),
            Ok(report)
        );

        let identified = [9, 0x05, 0x7f, 0x81];
        assert_eq!(
            boot_mouse_report(InputFrame::Report(&identified), Some(9)),
            Ok(report)
        );
        assert!(report.buttons.left());
        assert!(!report.buttons.right());
        assert!(report.buttons.middle());
    }

    #[test]
    fn rejects_malformed_boot_mouse_payloads() {
        assert_eq!(
            boot_mouse_report(InputFrame::Reset, Some(9)),
            Err(ProtocolError::NotBootMouseReport)
        );
        assert_eq!(
            boot_mouse_report(InputFrame::Report(&[9, 0, 1]), Some(9)),
            Err(ProtocolError::NotBootMouseReport)
        );
        assert_eq!(
            boot_mouse_report(InputFrame::Report(&[8, 0, 1, 2]), Some(9)),
            Err(ProtocolError::UnexpectedReportId)
        );
    }

    #[test]
    fn decodes_a_report_protocol_mouse_ignoring_trailing_bytes() {
        // The X13s Elan mouse (report-id 0x01) sends 8 data bytes: buttons, X, Y, then 5 pad;
        // boot_mouse_report reads only the boot prefix and ignores the trailing bytes.
        let report = boot_mouse_report(
            InputFrame::Report(&[1, 0x01, 0x03, 0xfe, 0, 0, 0, 0, 0]),
            Some(1),
        )
        .unwrap();
        assert!(report.buttons.left());
        assert!(!report.buttons.right());
        assert_eq!(report.x_delta, 3);
        assert_eq!(report.y_delta, -2);
    }
}
