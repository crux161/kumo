#![no_std]

use kumo_geni_i2c::SourceClock;
use kumo_hid::{DecodeError, Decoder, KeyState};
use kumo_i2c_hid::{boot_keyboard_report, InputFrame, KeyboardTopology, ProtocolError};

const MAGIC: [u8; 4] = *b"I2H1";
pub const KEYBOARD_BOOTSTRAP_TAG: u8 = b'K';
pub const INPUT_POLL_FRAMES: usize = 32;
pub const MAX_INPUT_FRAME_BYTES: usize = 64;
pub const MAX_REPORT_DESCRIPTOR_BYTES: usize = 256;
pub const ELAN_VENDOR_ID: u16 = 0x04f3;
pub const HANTICK_VENDOR_ID: u16 = 0x0911;
pub const HANTICK_5288_PRODUCT_ID: u16 = 0x5288;
pub const HP_ITE_VENDOR_ID: u16 = 0x103c;
pub const ITE_VOYO_WINPAD_A15_PRODUCT_ID: u16 = 0x184f;
pub const RAYDIUM_VENDOR_ID: u16 = 0x2386;
pub const RAYDIUM_3118_PRODUCT_ID: u16 = 0x3118;
pub const USB_ITE_VENDOR_ID: u16 = 0x048d;
pub const ITE_LENOVO_LEGION_Y720_PRODUCT_ID: u16 = 0x837a;
pub const QTEC_VENDOR_ID: u16 = 0x6243;
pub const BLTP_VENDOR_ID: u16 = 0x36b6;
pub const BLTP_7853_PRODUCT_ID: u16 = 0xc001;
pub const SOFT_FAILURE_LOG_LIMIT: u32 = 4;

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
}

/// The outcome of decoding one HID-over-I2C input frame, routed by report ID the way Linux's
/// `hid_input_report` dispatches a numbered report to its collection. The X13s `keyboard@68`
/// device emits more than one input report (the boot keyboard plus consumer / system-control
/// reports); KUMO owns only the keyboard, so a frame for another report ID is a benign skip,
/// not a decode failure. — CORVUS
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodedReport {
    /// A keyboard input report we decoded (it may carry zero key edges, e.g. an all-released
    /// poll).
    Keyboard(InputProbe),
    /// A valid input frame whose report ID is not the keyboard's. Carries the foreign report ID
    /// so the driver can log a bounded diagnostic sample without treating it as an error.
    NonKeyboard { report_id: u8 },
    /// A length-0 reset/empty notification, or a quirk-swallowed bogus IRQ. Nothing to forward.
    Empty,
}

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
    pub no_irq_after_reset: bool,
    pub no_wakeup_after_reset: bool,
    pub bogus_irq: bool,
    pub bad_input_size: bool,
    pub re_power_on: bool,
}

impl DeviceQuirks {
    pub const fn for_vendor_product(vendor_id: u16, product_id: u16) -> Self {
        if vendor_id == ELAN_VENDOR_ID {
            // ELAN's real Linux quirks (i2c-hid-core.c table): NO_WAKEUP_AFTER_RESET | BOGUS_IRQ.
            // The attention line is driven with the trigger the DT declares — level-low for
            // `keyboard@68` — exactly as Linux requests it (`IRQF_TRIGGER_LOW | IRQF_ONESHOT`).
            // KUMO previously forced falling-edge (J289/J290); that quirk does not exist in Linux
            // and edge triggering drops a report whenever the line is still asserted at service
            // time, so it is gone. — CORVUS
            Self {
                no_irq_after_reset: false,
                no_wakeup_after_reset: true,
                bogus_irq: true,
                bad_input_size: false,
                re_power_on: false,
            }
        } else if (vendor_id == HANTICK_VENDOR_ID && product_id == HANTICK_5288_PRODUCT_ID)
            || (vendor_id == HP_ITE_VENDOR_ID && product_id == ITE_VOYO_WINPAD_A15_PRODUCT_ID)
            || (vendor_id == RAYDIUM_VENDOR_ID && product_id == RAYDIUM_3118_PRODUCT_ID)
            || (vendor_id == BLTP_VENDOR_ID && product_id == BLTP_7853_PRODUCT_ID)
        {
            Self {
                no_irq_after_reset: true,
                no_wakeup_after_reset: false,
                bogus_irq: false,
                bad_input_size: false,
                re_power_on: false,
            }
        } else if vendor_id == USB_ITE_VENDOR_ID && product_id == ITE_LENOVO_LEGION_Y720_PRODUCT_ID
        {
            Self {
                no_irq_after_reset: false,
                no_wakeup_after_reset: false,
                bogus_irq: false,
                bad_input_size: true,
                re_power_on: false,
            }
        } else if vendor_id == QTEC_VENDOR_ID {
            Self {
                no_irq_after_reset: false,
                no_wakeup_after_reset: false,
                bogus_irq: false,
                bad_input_size: false,
                re_power_on: true,
            }
        } else {
            Self {
                no_irq_after_reset: false,
                no_wakeup_after_reset: false,
                bogus_irq: false,
                bad_input_size: false,
                re_power_on: false,
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

    /// Back-compat shape used by the host tests and any single-report caller: a frame for another
    /// report ID still surfaces as `UnexpectedReportId` here. New code that wants Linux-style
    /// routing (skip non-keyboard reports rather than error on them) should call
    /// [`Self::decode_report_with_quirks`]. — CORVUS
    pub fn decode_with_quirks(
        &mut self,
        raw: &[u8],
        report_id: Option<u8>,
        quirks: DeviceQuirks,
    ) -> Result<InputProbe, InputProbeError> {
        match self.decode_report_with_quirks(raw, report_id, quirks)? {
            DecodedReport::Keyboard(probe) => Ok(probe),
            DecodedReport::Empty => Ok(no_input_probe()),
            DecodedReport::NonKeyboard { .. } => {
                Err(InputProbeError::Protocol(ProtocolError::UnexpectedReportId))
            }
        }
    }

    /// Decode one input frame, routing by report ID like Linux's `hid_input_report`: the keyboard
    /// report decodes to key edges, any other report ID is a benign [`DecodedReport::NonKeyboard`]
    /// skip, and a reset/empty/bogus frame is [`DecodedReport::Empty`]. Only a genuinely malformed
    /// *keyboard* report (truncated, or a rollover/decode error on our own report ID) is an `Err`.
    /// — CORVUS
    pub fn decode_report_with_quirks(
        &mut self,
        raw: &[u8],
        report_id: Option<u8>,
        quirks: DeviceQuirks,
    ) -> Result<DecodedReport, InputProbeError> {
        if quirks.bogus_irq && raw.len() >= 2 && raw[0] == 0xff && raw[1] == 0xff {
            return Ok(DecodedReport::Empty);
        }
        let frame = if quirks.bad_input_size {
            InputFrame::parse_with_bad_size_quirk(raw)
        } else {
            InputFrame::parse(raw)
        }
        .map_err(InputProbeError::Protocol)?;
        // A length-0 input frame is the HID-over-I2C reset-complete / empty notification, not a key
        // report. Treat it as benign only because the driver completes the GPIO attention IRQ after
        // the plain I2C read has drained the level-low source. — KESTREL
        let frame = match frame {
            InputFrame::Reset => return Ok(DecodedReport::Empty),
            frame @ InputFrame::Report(payload) => {
                // Demux numbered reports by their leading report-ID byte. A frame for a report ID
                // other than the keyboard's belongs to another collection (the Elan keyboard@68
                // also speaks consumer / system-control reports); skip it the way the HID core
                // routes an unclaimed report, instead of failing the whole driver. — CORVUS
                if let Some(expected) = report_id {
                    match payload.first() {
                        None => return Err(InputProbeError::Protocol(ProtocolError::Truncated)),
                        Some(&actual) if actual != expected => {
                            return Ok(DecodedReport::NonKeyboard { report_id: actual });
                        }
                        _ => {}
                    }
                }
                frame
            }
        };
        let report = boot_keyboard_report(frame, report_id).map_err(InputProbeError::Protocol)?;
        let events = self
            .decoder
            .decode(report)
            .map_err(InputProbeError::Decode)?;
        Ok(DecodedReport::Keyboard(input_probe_from_events(&events)))
    }
}

/// Capability-adjacent bootstrap data. Authority remains in the separately transferred Resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeConfig {
    pub i2c_address: u8,
    pub hid_descriptor_register: u16,
    pub attention_irq: u32,
}

impl ProbeConfig {
    pub const BYTES: usize = 12;

    pub fn for_x13s(topology: KeyboardTopology) -> Result<Self, ConfigError> {
        let config = Self {
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
        raw[4] = self.i2c_address;
        raw[5..7].copy_from_slice(&self.hid_descriptor_register.to_le_bytes());
        raw[8..12].copy_from_slice(&self.attention_irq.to_le_bytes());
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
            i2c_address: raw[4],
            hid_descriptor_register: u16::from_le_bytes(raw[5..7].try_into().unwrap()),
            attention_irq: u32::from_le_bytes(raw[8..12].try_into().unwrap()),
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(self) -> Result<(), ConfigError> {
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
    input_read_len(length, 2)
}

pub fn input_read_len(
    advertised_length: u16,
    report_frame_length: usize,
) -> Result<usize, InputProbeError> {
    if report_frame_length > MAX_INPUT_FRAME_BYTES {
        return Err(InputProbeError::InvalidLength);
    }
    let advertised_length = advertised_length as usize;
    if advertised_length < 2 {
        return Err(InputProbeError::InvalidLength);
    }
    Ok(advertised_length
        .max(report_frame_length)
        .min(MAX_INPUT_FRAME_BYTES))
}

pub fn strict_input_frame_len(length: u16) -> Result<usize, InputProbeError> {
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

const fn no_input_probe() -> InputProbe {
    InputProbe {
        event_count: 0,
        first_pressed_usage: None,
        first_pressed_ascii: None,
    }
}

fn input_probe_from_events(events: &kumo_hid::Events) -> InputProbe {
    let mut first_pressed_usage = None;
    let mut first_pressed_ascii = None;
    for event in events.as_slice() {
        if event.state == KeyState::Pressed {
            first_pressed_usage = Some(event.usage);
            first_pressed_ascii = event.symbol.ascii();
            break;
        }
    }

    InputProbe {
        event_count: events.len(),
        first_pressed_usage,
        first_pressed_ascii,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_i2c_hid::{GicInterrupt, GpioInterrupt};

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
            Ok(MAX_INPUT_FRAME_BYTES)
        );
    }

    #[test]
    fn elan_vendor_enables_linux_i2c_hid_quirks() {
        // ELAN's real Linux quirk set is NO_WAKEUP_AFTER_RESET | BOGUS_IRQ — no falling-edge
        // override; the attention line uses the DT's level-low trigger like Linux. — CORVUS
        assert_eq!(
            DeviceQuirks::for_vendor_product(ELAN_VENDOR_ID, 0x1234),
            DeviceQuirks {
                no_irq_after_reset: false,
                no_wakeup_after_reset: true,
                bogus_irq: true,
                bad_input_size: false,
                re_power_on: false,
            }
        );
        assert_eq!(
            DeviceQuirks::for_vendor_product(0x17ef, 0x1234),
            DeviceQuirks::default()
        );
    }

    #[test]
    fn linux_i2c_hid_quirks_are_preserved_when_kumo_can_act_on_them() {
        assert!(
            DeviceQuirks::for_vendor_product(HANTICK_VENDOR_ID, HANTICK_5288_PRODUCT_ID)
                .no_irq_after_reset
        );
        assert!(
            DeviceQuirks::for_vendor_product(USB_ITE_VENDOR_ID, ITE_LENOVO_LEGION_Y720_PRODUCT_ID)
                .bad_input_size
        );
        assert!(DeviceQuirks::for_vendor_product(QTEC_VENDOR_ID, 0x0001).re_power_on);
        assert!(
            DeviceQuirks::for_vendor_product(BLTP_VENDOR_ID, BLTP_7853_PRODUCT_ID)
                .no_irq_after_reset
        );
    }

    #[test]
    fn elan_bogus_irq_frame_is_a_decoded_no_event_frame() {
        let quirks = DeviceQuirks::for_vendor_product(ELAN_VENDOR_ID, 0);
        let mut decoder = InputProbeDecoder::new();
        assert_eq!(
            decoder.decode_with_quirks(&[0xff, 0xff], Some(7), quirks),
            Ok(InputProbe {
                event_count: 0,
                first_pressed_usage: None,
                first_pressed_ascii: None,
            })
        );
    }

    #[test]
    fn decodes_one_boot_keyboard_input_frame() {
        let raw = [10, 0, 0, 0, 0x04, 0, 0, 0, 0, 0];
        assert_eq!(
            decode_input_probe(&raw, None),
            Ok(InputProbe {
                event_count: 1,
                first_pressed_usage: Some(0x04),
                first_pressed_ascii: Some(b'a'),
            })
        );
    }

    #[test]
    fn decodes_bad_input_size_frames_only_behind_the_linux_quirk() {
        let raw = [0x20, 0, 0, 0, 0x04, 0, 0, 0, 0, 0];
        let quirks =
            DeviceQuirks::for_vendor_product(USB_ITE_VENDOR_ID, ITE_LENOVO_LEGION_Y720_PRODUCT_ID);
        let mut decoder = InputProbeDecoder::new();
        assert_eq!(
            decoder.decode_with_quirks(&raw, None, quirks),
            Ok(InputProbe {
                event_count: 1,
                first_pressed_usage: Some(0x04),
                first_pressed_ascii: Some(b'a'),
            })
        );
        assert_eq!(
            InputProbeDecoder::new().decode(&raw, None),
            Err(InputProbeError::Protocol(ProtocolError::InvalidInputLength))
        );
    }

    #[test]
    fn decodes_an_identified_boot_keyboard_input_frame() {
        let raw = [11, 0, 7, 0, 0, 0x05, 0, 0, 0, 0, 0];
        assert_eq!(
            decode_input_probe(&raw, Some(7)),
            Ok(InputProbe {
                event_count: 1,
                first_pressed_usage: Some(0x05),
                first_pressed_ascii: Some(b'b'),
            })
        );
    }

    #[test]
    fn empty_keyboard_report_is_a_decoded_no_event_frame() {
        let raw = [10, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            decode_input_probe(&raw, None),
            Ok(InputProbe {
                event_count: 0,
                first_pressed_usage: None,
                first_pressed_ascii: None,
            })
        );
    }

    #[test]
    fn length_zero_reset_frame_is_a_benign_no_event_frame() {
        // The RESET-complete sync and any empty poll both arrive as a length-0 frame; they must
        // decode to a no-event frame, never NotBootKeyboardReport. — CORVUS
        let reset = [0u8, 0u8];
        assert_eq!(
            decode_input_probe(&reset, Some(7)),
            Ok(InputProbe {
                event_count: 0,
                first_pressed_usage: None,
                first_pressed_ascii: None,
            })
        );
    }

    #[test]
    fn soft_failures_are_bounded_diagnostics() {
        let mut failures = BoundedFailureLog::new();
        for i in 0..SOFT_FAILURE_LOG_LIMIT {
            assert!(failures.record(), "failure {i} should still log");
        }
        assert!(!failures.record());
        assert_eq!(failures.count(), SOFT_FAILURE_LOG_LIMIT + 1);
    }

    #[test]
    fn input_read_len_clamps_like_linux_min_of_descriptor_and_buffer() {
        assert_eq!(input_read_len(0x22, 11), Ok(0x22));
        assert_eq!(input_read_len(0x200, 11), Ok(MAX_INPUT_FRAME_BYTES));
        assert_eq!(input_read_len(4, 11), Ok(11));
        assert_eq!(
            input_read_len(0x22, MAX_INPUT_FRAME_BYTES + 1),
            Err(InputProbeError::InvalidLength)
        );
        assert_eq!(
            strict_input_frame_len(0x200),
            Err(InputProbeError::InvalidLength)
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
    fn routing_skips_a_non_keyboard_report_id_as_benign() {
        // A consumer/system-control report (id 2) interleaved with the keyboard's (id 7) must be a
        // benign NonKeyboard skip carrying the actual id, not a decode failure — Linux routes it to
        // another collection. — CORVUS
        let consumer = [11, 0, 2, 0, 0, 0x05, 0, 0, 0, 0, 0];
        let mut decoder = InputProbeDecoder::new();
        assert_eq!(
            decoder.decode_report_with_quirks(&consumer, Some(7), DeviceQuirks::default()),
            Ok(DecodedReport::NonKeyboard { report_id: 2 })
        );
    }

    #[test]
    fn routing_decodes_the_matching_keyboard_report_id() {
        let keyboard = [11, 0, 7, 0, 0, 0x05, 0, 0, 0, 0, 0];
        let mut decoder = InputProbeDecoder::new();
        assert_eq!(
            decoder.decode_report_with_quirks(&keyboard, Some(7), DeviceQuirks::default()),
            Ok(DecodedReport::Keyboard(InputProbe {
                event_count: 1,
                first_pressed_usage: Some(0x05),
                first_pressed_ascii: Some(b'b'),
            }))
        );
    }

    #[test]
    fn routing_reports_reset_and_bogus_irq_as_empty() {
        let mut decoder = InputProbeDecoder::new();
        assert_eq!(
            decoder.decode_report_with_quirks(&[0, 0], Some(7), DeviceQuirks::default()),
            Ok(DecodedReport::Empty)
        );
        let elan = DeviceQuirks::for_vendor_product(ELAN_VENDOR_ID, 0);
        assert_eq!(
            decoder.decode_report_with_quirks(&[0xff, 0xff], Some(7), elan),
            Ok(DecodedReport::Empty)
        );
    }

    #[test]
    fn stateful_decoder_does_not_repeat_a_held_key() {
        let mut decoder = InputProbeDecoder::new();
        let pressed = [10, 0, 0, 0, 0x04, 0, 0, 0, 0, 0];
        let released = [10, 0, 0, 0, 0, 0, 0, 0, 0, 0];

        assert_eq!(
            decoder.decode(&pressed, None),
            Ok(InputProbe {
                event_count: 1,
                first_pressed_usage: Some(0x04),
                first_pressed_ascii: Some(b'a'),
            })
        );
        assert_eq!(
            decoder.decode(&pressed, None),
            Ok(InputProbe {
                event_count: 0,
                first_pressed_usage: None,
                first_pressed_ascii: None,
            })
        );
        assert_eq!(
            decoder.decode(&released, None),
            Ok(InputProbe {
                event_count: 1,
                first_pressed_usage: None,
                first_pressed_ascii: None,
            })
        );
    }
}
