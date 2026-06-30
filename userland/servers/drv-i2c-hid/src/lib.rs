#![no_std]

use kumo_hid::{DecodeError, Decoder, KeyState, MAX_TERMINAL_BYTES, REPORT_KEYS};
use kumo_i2c_hid::{
    boot_keyboard_report, boot_mouse_report, BootMouseReport, InputFrame, KeyboardTopology,
    MouseButtons, MouseReport, ProtocolError, SourceClock, BOOT_MOUSE_REPORT_BYTES,
};

const MAGIC: [u8; 4] = *b"I2H1";
pub const KEYBOARD_BOOTSTRAP_TAG: u8 = b'K';
pub const MOUSE_BOOTSTRAP_TAG: u8 = b'M';
pub const MOUSE_EVENT_BYTES: usize = BOOT_MOUSE_REPORT_BYTES;
pub const INPUT_POLL_FRAMES: usize = 32;
pub const MAX_INPUT_FRAME_BYTES: usize = 64;
pub const MAX_REPORT_DESCRIPTOR_BYTES: usize = 256;
pub const MAX_PRESSED_ASCII_BYTES: usize = REPORT_KEYS;
pub const MAX_PRESSED_TERMINAL_BYTES: usize = REPORT_KEYS * MAX_TERMINAL_BYTES;
pub const ELAN_VENDOR_ID: u16 = 0x04f3;
pub const SOFT_FAILURE_LOG_LIMIT: u32 = 4;
pub const INPUT_REPORT_STATS_LOG_LIMIT: u32 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    Truncated,
    BadMagic,
    InvalidMmio,
    UnsupportedBusFrequency,
    UnsupportedSourceClock,
    InvalidAddress,
    InvalidInterrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportProbeError {
    Empty,
    TooLong,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputProbeError {
    InvalidLength,
    Protocol(ProtocolError),
    Decode(DecodeError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputProbe {
    pub event_count: usize,
    pub first_pressed_usage: Option<u8>,
    pub first_pressed_ascii: Option<u8>,
    pub caps_lock_toggle: bool,
    pub pressed_ascii: [u8; MAX_PRESSED_ASCII_BYTES],
    pub pressed_ascii_len: usize,
    pub pressed_terminal: [u8; MAX_PRESSED_TERMINAL_BYTES],
    pub pressed_terminal_len: usize,
}

impl InputProbe {
    pub fn pressed_ascii(&self) -> &[u8] {
        &self.pressed_ascii[..self.pressed_ascii_len.min(MAX_PRESSED_ASCII_BYTES)]
    }

    pub fn pressed_terminal_bytes(&self) -> &[u8] {
        &self.pressed_terminal[..self.pressed_terminal_len.min(MAX_PRESSED_TERMINAL_BYTES)]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputReportClass {
    Reset,
    BogusIrq,
    KeyboardReport,
    MouseReport,
    ForeignReportId { report_id: u8 },
    ProtocolError(ProtocolError),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputReportStats {
    pub frames: u32,
    pub reset_frames: u32,
    pub bogus_irq_frames: u32,
    pub keyboard_reports: u32,
    pub mouse_reports: u32,
    pub foreign_report_ids: u32,
    pub protocol_errors: u32,
    pub decode_errors: u32,
    pub forwarded_ascii: u32,
    pub forwarded_mouse: u32,
    pub keyboard_write_drops: u32,
    pub mouse_write_drops: u32,
    pub last_report_id: Option<u8>,
    pub last_protocol_error: Option<ProtocolError>,
    pub last_decode_error: Option<DecodeError>,
}

impl InputReportStats {
    pub const fn new() -> Self {
        Self {
            frames: 0,
            reset_frames: 0,
            bogus_irq_frames: 0,
            keyboard_reports: 0,
            mouse_reports: 0,
            foreign_report_ids: 0,
            protocol_errors: 0,
            decode_errors: 0,
            forwarded_ascii: 0,
            forwarded_mouse: 0,
            keyboard_write_drops: 0,
            mouse_write_drops: 0,
            last_report_id: None,
            last_protocol_error: None,
            last_decode_error: None,
        }
    }

    pub fn record_class(&mut self, class: InputReportClass) {
        bump(&mut self.frames);
        match class {
            InputReportClass::Reset => bump(&mut self.reset_frames),
            InputReportClass::BogusIrq => bump(&mut self.bogus_irq_frames),
            InputReportClass::KeyboardReport => bump(&mut self.keyboard_reports),
            InputReportClass::MouseReport => bump(&mut self.mouse_reports),
            InputReportClass::ForeignReportId { report_id } => {
                bump(&mut self.foreign_report_ids);
                self.last_report_id = Some(report_id);
            }
            InputReportClass::ProtocolError(error) => {
                bump(&mut self.protocol_errors);
                self.last_protocol_error = Some(error);
            }
        }
    }

    pub fn record_decode_error(&mut self, error: DecodeError) {
        bump(&mut self.decode_errors);
        self.last_decode_error = Some(error);
    }

    pub fn record_forwarded_ascii(&mut self) {
        bump(&mut self.forwarded_ascii);
    }

    pub fn record_forwarded_mouse(&mut self) {
        bump(&mut self.forwarded_mouse);
    }

    pub fn record_keyboard_write_drop(&mut self) {
        bump(&mut self.keyboard_write_drops);
    }

    pub fn record_mouse_write_drop(&mut self) {
        bump(&mut self.mouse_write_drops);
    }
}

fn bump(counter: &mut u32) {
    *counter = counter.saturating_add(1);
}

pub fn should_log_input_report_stats(frames: u32, already_logged: u32) -> bool {
    already_logged < INPUT_REPORT_STATS_LOG_LIMIT && frames != 0 && frames.is_power_of_two()
}

/// A bounded soft-failure diagnostic: it lets the first `SOFT_FAILURE_LOG_LIMIT` recoverable
/// failures log, then stays silent so a persistent error cannot flood the framebuffer console. Used
/// for any post-first-light per-event soft-state loss — keyboard-forward drops AND input-report
/// decode drops — so the resident driver records the loss without dying (DESIGN/002). — CORVUS
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundedFailureLog {
    count: u32,
}

impl BoundedFailureLog {
    pub const fn new() -> Self {
        Self { count: 0 }
    }

    pub const fn count(self) -> u32 {
        self.count
    }

    pub fn record(&mut self) -> bool {
        let should_log = self.count < SOFT_FAILURE_LOG_LIMIT;
        self.count = self.count.saturating_add(1);
        should_log
    }
}

impl Default for BoundedFailureLog {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceQuirks {
    pub no_wakeup_after_reset: bool,
    pub bogus_irq: bool,
    /// ELAN i2c-hid devices need the attention line treated as **falling-edge**, not the level-low
    /// the DT declares (Linux `I2C_HID_QUIRK_FORCE_TRIGGER_FALLING`). Level-low re-fires while the
    /// device holds the line asserted — the 30–40 empty boot-burst IRQs we saw on metal. Wiring this
    /// to the attention IRQ request needs HAL TLMM edge-detection support (see J289 plan). — CORVUS
    pub force_trigger_falling: bool,
}

impl DeviceQuirks {
    pub const fn for_vendor_product(vendor_id: u16, _product_id: u16) -> Self {
        if vendor_id == ELAN_VENDOR_ID {
            Self {
                no_wakeup_after_reset: true,
                bogus_irq: true,
                force_trigger_falling: true,
            }
        } else {
            Self {
                no_wakeup_after_reset: false,
                bogus_irq: false,
                force_trigger_falling: false,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputProbeDecoder {
    decoder: Decoder,
}

impl InputProbeDecoder {
    pub const fn new() -> Self {
        Self {
            decoder: Decoder::new(),
        }
    }

    pub fn decode(
        &mut self,
        raw: &[u8],
        report_id: Option<u8>,
    ) -> Result<InputProbe, InputProbeError> {
        self.decode_with_quirks(raw, report_id, DeviceQuirks::default())
    }

    pub fn decode_with_quirks(
        &mut self,
        raw: &[u8],
        report_id: Option<u8>,
        quirks: DeviceQuirks,
    ) -> Result<InputProbe, InputProbeError> {
        if quirks.bogus_irq && raw.len() >= 2 && raw[0] == 0xff && raw[1] == 0xff {
            return Ok(no_input_probe());
        }
        let frame = InputFrame::parse(raw).map_err(InputProbeError::Protocol)?;
        // A length-0 input frame is the HID-over-I2C reset-complete / empty notification, not a key
        // report. Treat it as benign only because the driver completes the GPIO attention IRQ after
        // the plain I2C read has drained the level-low source. — KESTREL
        let frame = match frame {
            InputFrame::Reset => return Ok(no_input_probe()),
            frame @ InputFrame::Report(_) => frame,
        };
        let report = boot_keyboard_report(frame, report_id).map_err(InputProbeError::Protocol)?;
        let events = self
            .decoder
            .decode(report)
            .map_err(InputProbeError::Decode)?;
        Ok(input_probe_from_events(&events))
    }
}

/// Capability-adjacent bootstrap data. Authority remains in the separately transferred Resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeConfig {
    pub mmio_base: u64,
    pub mmio_length: u64,
    pub bus_frequency_hz: u32,
    pub source_clock: SourceClock,
    pub i2c_address: u8,
    pub hid_descriptor_register: u16,
    pub attention_irq: u32,
}

impl ProbeConfig {
    pub const BYTES: usize = 36;

    pub fn for_x13s(topology: KeyboardTopology) -> Result<Self, ConfigError> {
        let config = Self {
            mmio_base: topology.controller_mmio_base,
            mmio_length: topology.controller_mmio_length,
            bus_frequency_hz: topology.bus_frequency_hz,
            source_clock: SourceClock::Mhz19_2,
            i2c_address: topology.i2c_address,
            hid_descriptor_register: topology.hid_descriptor_register,
            attention_irq: kumo_abi::tlmm_gpio_irq(
                topology.keyboard_interrupt.pin,
                topology.keyboard_interrupt.flags,
            ),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn encode(self) -> [u8; Self::BYTES] {
        let mut raw = [0u8; Self::BYTES];
        raw[..4].copy_from_slice(&MAGIC);
        raw[4..12].copy_from_slice(&self.mmio_base.to_le_bytes());
        raw[12..20].copy_from_slice(&self.mmio_length.to_le_bytes());
        raw[20..24].copy_from_slice(&self.bus_frequency_hz.to_le_bytes());
        raw[24..28].copy_from_slice(&source_clock_hz(self.source_clock).to_le_bytes());
        raw[28] = self.i2c_address;
        raw[29..31].copy_from_slice(&self.hid_descriptor_register.to_le_bytes());
        raw[32..36].copy_from_slice(&self.attention_irq.to_le_bytes());
        raw
    }

    pub fn decode(raw: &[u8]) -> Result<Self, ConfigError> {
        if raw.len() < Self::BYTES {
            return Err(ConfigError::Truncated);
        }
        if raw[..4] != MAGIC {
            return Err(ConfigError::BadMagic);
        }
        let config = Self {
            mmio_base: u64::from_le_bytes(raw[4..12].try_into().unwrap()),
            mmio_length: u64::from_le_bytes(raw[12..20].try_into().unwrap()),
            bus_frequency_hz: u32::from_le_bytes(raw[20..24].try_into().unwrap()),
            source_clock: match u32::from_le_bytes(raw[24..28].try_into().unwrap()) {
                19_200_000 => SourceClock::Mhz19_2,
                32_000_000 => SourceClock::Mhz32,
                _ => return Err(ConfigError::UnsupportedSourceClock),
            },
            i2c_address: raw[28],
            hid_descriptor_register: u16::from_le_bytes(raw[29..31].try_into().unwrap()),
            attention_irq: u32::from_le_bytes(raw[32..36].try_into().unwrap()),
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(self) -> Result<(), ConfigError> {
        if self.mmio_base == 0
            || self.mmio_base & 0xfff != 0
            || self.mmio_length < 0x1000
            || self.mmio_length & 0xfff != 0
        {
            return Err(ConfigError::InvalidMmio);
        }
        if self.bus_frequency_hz != 400_000 {
            return Err(ConfigError::UnsupportedBusFrequency);
        }
        if self.i2c_address > 0x7f {
            return Err(ConfigError::InvalidAddress);
        }
        if kumo_abi::decode_tlmm_gpio_irq(self.attention_irq).is_none() {
            return Err(ConfigError::InvalidInterrupt);
        }
        Ok(())
    }
}

const fn source_clock_hz(clock: SourceClock) -> u32 {
    match clock {
        SourceClock::Mhz19_2 => 19_200_000,
        SourceClock::Mhz32 => 32_000_000,
    }
}

pub fn bounded_report_descriptor_len(length: u16) -> Result<usize, ReportProbeError> {
    let length = length as usize;
    if length == 0 {
        Err(ReportProbeError::Empty)
    } else if length > MAX_REPORT_DESCRIPTOR_BYTES {
        Err(ReportProbeError::TooLong)
    } else {
        Ok(length)
    }
}

pub fn bounded_input_frame_len(length: u16) -> Result<usize, InputProbeError> {
    let length = length as usize;
    if !(2..=MAX_INPUT_FRAME_BYTES).contains(&length) {
        Err(InputProbeError::InvalidLength)
    } else {
        Ok(length)
    }
}

pub fn decode_input_probe(
    raw: &[u8],
    report_id: Option<u8>,
) -> Result<InputProbe, InputProbeError> {
    InputProbeDecoder::new().decode(raw, report_id)
}

pub fn classify_input_report(
    raw: &[u8],
    report_id: Option<u8>,
    quirks: DeviceQuirks,
) -> InputReportClass {
    classify_input_report_with_mouse(raw, report_id, None, quirks)
}

pub fn classify_input_report_with_mouse(
    raw: &[u8],
    keyboard_report_id: Option<u8>,
    mouse_report: Option<MouseReport>,
    quirks: DeviceQuirks,
) -> InputReportClass {
    if quirks.bogus_irq && raw.len() >= 2 && raw[0] == 0xff && raw[1] == 0xff {
        return InputReportClass::BogusIrq;
    }
    let frame = match InputFrame::parse(raw) {
        Ok(frame) => frame,
        Err(error) => return InputReportClass::ProtocolError(error),
    };
    let InputFrame::Report(payload) = frame else {
        return InputReportClass::Reset;
    };

    if let Some(mouse_report) = mouse_report {
        if payload_could_be_report(payload, mouse_report.report_id) {
            return match boot_mouse_report(frame, mouse_report.report_id) {
                Ok(_) => InputReportClass::MouseReport,
                Err(error) => InputReportClass::ProtocolError(error),
            };
        }
    }

    match keyboard_report_id {
        Some(expected) => {
            let Some((&actual, rest)) = payload.split_first() else {
                return InputReportClass::ProtocolError(ProtocolError::Truncated);
            };
            if actual != expected {
                return InputReportClass::ForeignReportId { report_id: actual };
            }
            if rest.len() == kumo_hid::REPORT_BYTES {
                InputReportClass::KeyboardReport
            } else {
                InputReportClass::ProtocolError(ProtocolError::NotBootKeyboardReport)
            }
        }
        None if payload.len() == kumo_hid::REPORT_BYTES => InputReportClass::KeyboardReport,
        None => InputReportClass::ProtocolError(ProtocolError::NotBootKeyboardReport),
    }
}

pub fn decode_mouse_probe(
    raw: &[u8],
    mouse_report: MouseReport,
    quirks: DeviceQuirks,
) -> Result<Option<BootMouseReport>, ProtocolError> {
    if quirks.bogus_irq && raw.len() >= 2 && raw[0] == 0xff && raw[1] == 0xff {
        return Ok(None);
    }
    let frame = InputFrame::parse(raw)?;
    match frame {
        InputFrame::Reset => Ok(None),
        frame @ InputFrame::Report(_) => boot_mouse_report(frame, mouse_report.report_id).map(Some),
    }
}

pub fn encode_mouse_event(report: BootMouseReport) -> [u8; MOUSE_EVENT_BYTES] {
    [
        report.buttons.bits(),
        report.x_delta as u8,
        report.y_delta as u8,
    ]
}

pub fn decode_mouse_event(raw: &[u8]) -> Option<BootMouseReport> {
    if raw.len() == MOUSE_EVENT_BYTES {
        Some(BootMouseReport {
            buttons: MouseButtons::from_bits(raw[0]),
            x_delta: raw[1] as i8,
            y_delta: raw[2] as i8,
        })
    } else {
        None
    }
}

fn payload_could_be_report(payload: &[u8], report_id: Option<u8>) -> bool {
    match report_id {
        Some(expected) => payload.first().copied() == Some(expected),
        None => payload.len() == BOOT_MOUSE_REPORT_BYTES,
    }
}

const fn no_input_probe() -> InputProbe {
    InputProbe {
        event_count: 0,
        first_pressed_usage: None,
        first_pressed_ascii: None,
        caps_lock_toggle: false,
        pressed_ascii: [0; MAX_PRESSED_ASCII_BYTES],
        pressed_ascii_len: 0,
        pressed_terminal: [0; MAX_PRESSED_TERMINAL_BYTES],
        pressed_terminal_len: 0,
    }
}

fn input_probe_from_events(events: &kumo_hid::Events) -> InputProbe {
    let mut first_pressed_usage = None;
    let mut first_pressed_ascii = None;
    let mut caps_lock_toggle = false;
    let mut pressed_ascii = [0u8; MAX_PRESSED_ASCII_BYTES];
    let mut pressed_ascii_len = 0;
    let mut pressed_terminal = [0u8; MAX_PRESSED_TERMINAL_BYTES];
    let mut pressed_terminal_len = 0;
    for event in events.as_slice() {
        if event.state == KeyState::Pressed {
            if event.usage == 0x39 {
                caps_lock_toggle = true;
            }
            let ascii = event.symbol.ascii();
            if first_pressed_usage.is_none() {
                first_pressed_usage = Some(event.usage);
                first_pressed_ascii = ascii;
            }
            if let Some(byte) = ascii {
                if pressed_ascii_len < MAX_PRESSED_ASCII_BYTES {
                    pressed_ascii[pressed_ascii_len] = byte;
                    pressed_ascii_len += 1;
                }
            }
            for &byte in event.symbol.terminal_bytes().as_slice() {
                if pressed_terminal_len < MAX_PRESSED_TERMINAL_BYTES {
                    pressed_terminal[pressed_terminal_len] = byte;
                    pressed_terminal_len += 1;
                }
            }
        }
    }

    InputProbe {
        event_count: events.len(),
        first_pressed_usage,
        first_pressed_ascii,
        caps_lock_toggle,
        pressed_ascii,
        pressed_ascii_len,
        pressed_terminal,
        pressed_terminal_len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_i2c_hid::{GicInterrupt, GpioInterrupt, MouseButtons, MouseReport};

    fn probe(
        event_count: usize,
        first_pressed_usage: Option<u8>,
        first_pressed_ascii: Option<u8>,
        ascii: &[u8],
    ) -> InputProbe {
        probe_with_terminal(
            event_count,
            first_pressed_usage,
            first_pressed_ascii,
            ascii,
            ascii,
        )
    }

    fn probe_with_terminal(
        event_count: usize,
        first_pressed_usage: Option<u8>,
        first_pressed_ascii: Option<u8>,
        ascii: &[u8],
        terminal: &[u8],
    ) -> InputProbe {
        let mut pressed_ascii = [0; MAX_PRESSED_ASCII_BYTES];
        pressed_ascii[..ascii.len()].copy_from_slice(ascii);
        let mut pressed_terminal = [0; MAX_PRESSED_TERMINAL_BYTES];
        pressed_terminal[..terminal.len()].copy_from_slice(terminal);
        InputProbe {
            event_count,
            first_pressed_usage,
            first_pressed_ascii,
            caps_lock_toggle: false,
            pressed_ascii,
            pressed_ascii_len: ascii.len(),
            pressed_terminal,
            pressed_terminal_len: terminal.len(),
        }
    }

    fn topology() -> KeyboardTopology {
        KeyboardTopology {
            controller_mmio_base: 0x0089_4000,
            controller_mmio_length: 0x4000,
            controller_interrupt: GicInterrupt {
                kind: 0,
                number: 0x24b,
                flags: 4,
            },
            bus_frequency_hz: 400_000,
            i2c_address: 0x68,
            hid_descriptor_register: 1,
            keyboard_interrupt: GpioInterrupt {
                controller_phandle: 1,
                pin: 104,
                flags: 8,
            },
        }
    }

    #[test]
    fn x13s_config_round_trips_without_carrying_authority() {
        let dtb = include_bytes!("../../../../sc8280xp-lenovo-thinkpad-x13s.dtb");
        let discovered = kumo_i2c_hid::discover_keyboard(dtb).unwrap();
        let config = ProbeConfig::for_x13s(discovered).unwrap();
        assert_eq!(ProbeConfig::decode(&config.encode()), Ok(config));
        assert_eq!(config.source_clock, SourceClock::Mhz19_2);
        assert_eq!(config.mmio_base, 0x0089_4000);
        assert_eq!(config.i2c_address, 0x68);
        assert_eq!(config.attention_irq, kumo_abi::tlmm_gpio_irq(104, 8));
    }

    #[test]
    fn rejects_malformed_or_dangerous_bootstrap_data() {
        assert_eq!(ProbeConfig::decode(b"short"), Err(ConfigError::Truncated));
        let mut raw = ProbeConfig::for_x13s(topology()).unwrap().encode();
        raw[4] = 1;
        assert_eq!(ProbeConfig::decode(&raw), Err(ConfigError::InvalidMmio));
    }

    #[test]
    fn bounds_report_descriptor_reads_to_the_probe_buffer() {
        assert_eq!(
            bounded_report_descriptor_len(0),
            Err(ReportProbeError::Empty)
        );
        assert_eq!(bounded_report_descriptor_len(0xb9), Ok(0xb9));
        assert_eq!(
            bounded_report_descriptor_len((MAX_REPORT_DESCRIPTOR_BYTES + 1) as u16),
            Err(ReportProbeError::TooLong)
        );
    }

    #[test]
    fn bounds_input_frame_reads_to_the_probe_buffer() {
        assert_eq!(
            bounded_input_frame_len(1),
            Err(InputProbeError::InvalidLength)
        );
        assert_eq!(bounded_input_frame_len(0x22), Ok(0x22));
        assert_eq!(
            bounded_input_frame_len((MAX_INPUT_FRAME_BYTES + 1) as u16),
            Err(InputProbeError::InvalidLength)
        );
    }

    #[test]
    fn elan_vendor_enables_linux_i2c_hid_quirks() {
        assert_eq!(
            DeviceQuirks::for_vendor_product(ELAN_VENDOR_ID, 0x1234),
            DeviceQuirks {
                no_wakeup_after_reset: true,
                bogus_irq: true,
                force_trigger_falling: true,
            }
        );
        assert_eq!(
            DeviceQuirks::for_vendor_product(0x17ef, 0x1234),
            DeviceQuirks::default()
        );
    }

    #[test]
    fn elan_bogus_irq_frame_is_a_decoded_no_event_frame() {
        let quirks = DeviceQuirks::for_vendor_product(ELAN_VENDOR_ID, 0);
        let mut decoder = InputProbeDecoder::new();
        assert_eq!(
            decoder.decode_with_quirks(&[0xff, 0xff], Some(7), quirks),
            Ok(probe(0, None, None, b""))
        );
    }

    #[test]
    fn classifies_input_reports_without_changing_decode_policy() {
        let quirks = DeviceQuirks::for_vendor_product(ELAN_VENDOR_ID, 0);
        assert_eq!(
            classify_input_report(&[0xff, 0xff], Some(7), quirks),
            InputReportClass::BogusIrq
        );
        assert_eq!(
            classify_input_report(&[0, 0], Some(7), quirks),
            InputReportClass::Reset
        );
        assert_eq!(
            classify_input_report(&[11, 0, 7, 0, 0, 0x05, 0, 0, 0, 0, 0], Some(7), quirks),
            InputReportClass::KeyboardReport
        );
        assert_eq!(
            classify_input_report(&[11, 0, 2, 0, 0, 0x05, 0, 0, 0, 0, 0], Some(7), quirks),
            InputReportClass::ForeignReportId { report_id: 2 }
        );
        assert_eq!(
            classify_input_report(&[1, 0], Some(7), quirks),
            InputReportClass::ProtocolError(ProtocolError::InvalidInputLength)
        );
    }

    #[test]
    fn classifies_known_mouse_reports_without_treating_them_as_keyboard_errors() {
        let quirks = DeviceQuirks::default();
        assert_eq!(
            classify_input_report_with_mouse(
                &[5, 0, 0x01, 0x05, 0xfb],
                None,
                Some(MouseReport { report_id: None }),
                quirks
            ),
            InputReportClass::MouseReport
        );
        let mouse = MouseReport { report_id: Some(9) };
        assert_eq!(
            classify_input_report_with_mouse(
                &[6, 0, 9, 0x01, 0x05, 0xfb],
                Some(8),
                Some(mouse),
                quirks
            ),
            InputReportClass::MouseReport
        );
        assert_eq!(
            classify_input_report_with_mouse(
                &[6, 0, 7, 0x01, 0x05, 0xfb],
                Some(8),
                Some(mouse),
                quirks
            ),
            InputReportClass::ForeignReportId { report_id: 7 }
        );
        assert_eq!(
            classify_input_report_with_mouse(&[5, 0, 9, 0x01, 0x05], Some(8), Some(mouse), quirks),
            InputReportClass::ProtocolError(ProtocolError::NotBootMouseReport)
        );
    }

    #[test]
    fn decodes_mouse_probe_without_touching_the_channel() {
        let quirks = DeviceQuirks::default();
        let report = BootMouseReport {
            buttons: MouseButtons::from_bits(MouseButtons::LEFT | MouseButtons::MIDDLE),
            x_delta: 5,
            y_delta: -5,
        };
        assert_eq!(
            decode_mouse_probe(
                &[6, 0, 9, MouseButtons::LEFT | MouseButtons::MIDDLE, 5, 0xfb],
                MouseReport { report_id: Some(9) },
                quirks
            ),
            Ok(Some(report))
        );
        assert_eq!(
            decode_mouse_probe(
                &[5, 0, MouseButtons::RIGHT, 0xff, 1],
                MouseReport { report_id: None },
                quirks
            ),
            Ok(Some(BootMouseReport {
                buttons: MouseButtons::from_bits(MouseButtons::RIGHT),
                x_delta: -1,
                y_delta: 1,
            }))
        );
        assert_eq!(
            decode_mouse_probe(&[0, 0], MouseReport { report_id: Some(9) }, quirks),
            Ok(None)
        );
        assert_eq!(
            decode_mouse_probe(
                &[0xff, 0xff],
                MouseReport { report_id: Some(9) },
                DeviceQuirks::for_vendor_product(ELAN_VENDOR_ID, 0)
            ),
            Ok(None)
        );
    }

    #[test]
    fn mouse_events_encode_as_fixed_three_byte_ipc_records() {
        let report = BootMouseReport {
            buttons: MouseButtons::from_bits(MouseButtons::LEFT | MouseButtons::RIGHT | 0x80),
            x_delta: -2,
            y_delta: 127,
        };
        let encoded = encode_mouse_event(report);
        assert_eq!(
            encoded,
            [MouseButtons::LEFT | MouseButtons::RIGHT, 0xfe, 0x7f]
        );
        assert_eq!(decode_mouse_event(&encoded), Some(report));
        assert_eq!(decode_mouse_event(&encoded[..2]), None);
        assert_eq!(decode_mouse_event(&[0, 0, 0, 0]), None);
    }

    #[test]
    fn input_report_stats_count_classes_and_bound_summary_cadence() {
        let mut stats = InputReportStats::new();
        stats.record_class(InputReportClass::KeyboardReport);
        stats.record_class(InputReportClass::MouseReport);
        stats.record_class(InputReportClass::ForeignReportId { report_id: 2 });
        stats.record_class(InputReportClass::ProtocolError(
            ProtocolError::InvalidInputLength,
        ));
        stats.record_decode_error(DecodeError::Rollover);
        stats.record_forwarded_ascii();
        stats.record_forwarded_ascii();
        stats.record_forwarded_mouse();
        stats.record_keyboard_write_drop();
        stats.record_mouse_write_drop();

        assert_eq!(stats.frames, 4);
        assert_eq!(stats.keyboard_reports, 1);
        assert_eq!(stats.mouse_reports, 1);
        assert_eq!(stats.foreign_report_ids, 1);
        assert_eq!(stats.protocol_errors, 1);
        assert_eq!(stats.decode_errors, 1);
        assert_eq!(stats.forwarded_ascii, 2);
        assert_eq!(stats.forwarded_mouse, 1);
        assert_eq!(stats.keyboard_write_drops, 1);
        assert_eq!(stats.mouse_write_drops, 1);
        assert_eq!(stats.last_report_id, Some(2));
        assert_eq!(
            stats.last_protocol_error,
            Some(ProtocolError::InvalidInputLength)
        );
        assert_eq!(stats.last_decode_error, Some(DecodeError::Rollover));

        assert!(should_log_input_report_stats(1, 0));
        assert!(should_log_input_report_stats(128, 7));
        assert!(!should_log_input_report_stats(3, 0));
        assert!(!should_log_input_report_stats(
            256,
            INPUT_REPORT_STATS_LOG_LIMIT
        ));
    }

    #[test]
    fn decodes_one_boot_keyboard_input_frame() {
        let raw = [10, 0, 0, 0, 0x04, 0, 0, 0, 0, 0];
        assert_eq!(
            decode_input_probe(&raw, None),
            Ok(probe(1, Some(0x04), Some(b'a'), b"a"))
        );
    }

    #[test]
    fn decodes_an_identified_boot_keyboard_input_frame() {
        let raw = [11, 0, 7, 0, 0, 0x05, 0, 0, 0, 0, 0];
        assert_eq!(
            decode_input_probe(&raw, Some(7)),
            Ok(probe(1, Some(0x05), Some(b'b'), b"b"))
        );
    }

    #[test]
    fn decodes_the_x13s_first_light_keyboard_frame_shape() {
        // Metal first-light captured `0b 00 08 00 00 0b ...` for `h`; keep that wire shape pinned
        // before changing the restored driver path again. — KESTREL
        let raw = [0x0b, 0x00, 0x08, 0x00, 0x00, 0x0b, 0, 0, 0, 0, 0];
        assert_eq!(
            decode_input_probe(&raw, Some(0x08)),
            Ok(probe(1, Some(0x0b), Some(b'h'), b"h"))
        );
    }

    #[test]
    fn decodes_x13s_arrow_frames_as_terminal_sequences() {
        for (usage, sequence) in [
            (0x52, b"\x1b[A" as &[u8]),
            (0x51, b"\x1b[B"),
            (0x4f, b"\x1b[C"),
            (0x50, b"\x1b[D"),
        ] {
            let raw = [0x0b, 0x00, 0x08, 0x00, 0x00, usage, 0, 0, 0, 0, 0];
            let input = decode_input_probe(&raw, Some(0x08)).unwrap();
            assert_eq!(
                input,
                probe_with_terminal(1, Some(usage), None, b"", sequence)
            );
            assert_eq!(input.pressed_ascii(), b"");
            assert_eq!(input.pressed_terminal_bytes(), sequence);
        }
    }

    #[test]
    fn decodes_x13s_home_end_frames_as_terminal_sequences() {
        for (usage, sequence) in [(0x4a, b"\x1b[H" as &[u8]), (0x4d, b"\x1b[F")] {
            let raw = [0x0b, 0x00, 0x08, 0x00, 0x00, usage, 0, 0, 0, 0, 0];
            let input = decode_input_probe(&raw, Some(0x08)).unwrap();
            assert_eq!(
                input,
                probe_with_terminal(1, Some(usage), None, b"", sequence)
            );
            assert_eq!(input.pressed_terminal_bytes(), sequence);
        }
    }

    #[test]
    fn decodes_all_ascii_press_edges_from_one_keyboard_report() {
        let raw = [10, 0, 0, 0, 0x04, 0x05, 0x06, 0, 0, 0];
        let input = decode_input_probe(&raw, None).unwrap();
        assert_eq!(input, probe(3, Some(0x04), Some(b'a'), b"abc"));
        assert_eq!(input.pressed_ascii(), b"abc");
        assert_eq!(input.pressed_terminal_bytes(), b"abc");
    }

    #[test]
    fn keeps_first_pressed_probe_fields_while_collecting_later_ascii() {
        let raw = [10, 0, 0, 0, 0x39, 0x04, 0, 0, 0, 0];
        let input = decode_input_probe(&raw, None).unwrap();
        let mut expected = probe(2, Some(0x39), None, b"a");
        expected.caps_lock_toggle = true;
        assert_eq!(input, expected);
        assert_eq!(input.pressed_ascii(), b"a");
        assert_eq!(input.pressed_terminal_bytes(), b"a");
    }

    #[test]
    fn decodes_hid_delete_usage_as_terminal_delete_sequence() {
        let raw = [10, 0, 0, 0, 0x4c, 0, 0, 0, 0, 0];
        let input = decode_input_probe(&raw, None).unwrap();
        assert_eq!(
            input,
            probe_with_terminal(1, Some(0x4c), None, &[], b"\x1b[3~")
        );
        assert_eq!(input.pressed_ascii(), &[]);
        assert_eq!(input.pressed_terminal_bytes(), b"\x1b[3~");
    }

    #[test]
    fn empty_keyboard_report_is_a_decoded_no_event_frame() {
        let raw = [10, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            decode_input_probe(&raw, None),
            Ok(probe(0, None, None, b""))
        );
    }

    #[test]
    fn length_zero_reset_frame_is_a_benign_no_event_frame() {
        // The RESET-complete sync and any empty poll both arrive as a length-0 frame; they must
        // decode to a no-event frame, never NotBootKeyboardReport. — CORVUS
        let reset = [0u8, 0u8];
        assert_eq!(
            decode_input_probe(&reset, Some(7)),
            Ok(probe(0, None, None, b""))
        );
    }

    #[test]
    fn bounded_failure_log_is_a_bounded_diagnostic() {
        let mut failures = BoundedFailureLog::new();
        for i in 0..SOFT_FAILURE_LOG_LIMIT {
            assert!(failures.record(), "failure {i} should still log");
        }
        assert!(!failures.record());
        assert_eq!(failures.count(), SOFT_FAILURE_LOG_LIMIT + 1);
    }

    #[test]
    fn decoder_recovers_after_a_dropped_input_error() {
        // The steady-state IRQ loop now logs+continues on a recoverable InputProbeError instead of
        // process_exit(1). The host-side guarantee behind that "drop + continue" is that one bad
        // report does not poison the shared decoder: a foreign report-id frame errors, and the very
        // next real keyboard frame still decodes to a key. — CORVUS
        let mut decoder = InputProbeDecoder::new();
        assert_eq!(
            decoder.decode(&[11, 0, 2, 0, 0, 0x05, 0, 0, 0, 0, 0], Some(7)),
            Err(InputProbeError::Protocol(ProtocolError::UnexpectedReportId))
        );
        assert_eq!(
            decoder.decode(&[10, 0, 0, 0, 0x04, 0, 0, 0, 0, 0], None),
            Ok(probe(1, Some(0x04), Some(b'a'), b"a"))
        );
    }

    #[test]
    fn wrong_report_id_stays_a_protocol_error() {
        let raw = [11, 0, 2, 0, 0, 0x05, 0, 0, 0, 0, 0];
        assert_eq!(
            decode_input_probe(&raw, Some(7)),
            Err(InputProbeError::Protocol(ProtocolError::UnexpectedReportId))
        );
    }

    #[test]
    fn stateful_decoder_does_not_repeat_a_held_key() {
        let mut decoder = InputProbeDecoder::new();
        let pressed = [10, 0, 0, 0, 0x04, 0, 0, 0, 0, 0];
        let released = [10, 0, 0, 0, 0, 0, 0, 0, 0, 0];

        assert_eq!(
            decoder.decode(&pressed, None),
            Ok(probe(1, Some(0x04), Some(b'a'), b"a"))
        );
        assert_eq!(
            decoder.decode(&pressed, None),
            Ok(probe(0, None, None, b""))
        );
        assert_eq!(
            decoder.decode(&released, None),
            Ok(probe(1, None, None, b""))
        );
    }
}
