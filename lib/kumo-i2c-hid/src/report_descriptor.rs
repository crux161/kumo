//! HID **report-descriptor** parsing: prove a device emits a boot-shaped keyboard report.
//!
//! The HID-over-I2C descriptor (`protocol::HidDescriptor`) tells us a keyboard's report-descriptor
//! *length* and *register* but not its report *layout*. PLAN/006 §"Driver boundary and invariants"
//! requires proving the X13s keyboard speaks the conventional eight-byte boot report — byte 0
//! modifier bitmap, byte 1 reserved, bytes 2..8 a six-slot keycode array — and identifying any
//! Report ID that prefixes it on the wire, *before* [`crate::boot_keyboard_report`] and `kumo-hid`
//! may decode its frames. HID-over-I2C alone does not guarantee that layout.
//!
//! [`find_boot_keyboard`] walks the report-descriptor item stream (USB HID 1.11 §6.2.2) without
//! allocation and returns that Report ID when a boot-shaped keyboard Application Collection is
//! present. The walk is the pure, host-provable half of slice 5; reading the live 0xb9-byte
//! descriptor off the controller and feeding it here is the following (metal-owed) slice.

/// Why a report descriptor failed the boot-keyboard check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportDescriptorError {
    /// An item's declared data runs past the end of the descriptor.
    Truncated,
    /// An End Collection with no open collection, or a Pop with nothing pushed.
    Unbalanced,
    /// Push/pop or collection nesting exceeded the fixed parser bounds.
    TooComplex,
    /// A Report ID item used the forbidden ID 0 or overflowed the 8-bit report-id space.
    InvalidReportId,
    /// No boot-shaped keyboard Application Collection (Generic Desktop / Keyboard, with a
    /// 64-bit input report) is present.
    NotBootKeyboard,
}

/// The wire-relevant facts about a boot-shaped keyboard found in a report descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardReport {
    /// The Report ID prefixing this keyboard's input frames, or `None` when the descriptor
    /// declares no Report ID (a single-report device, whose frames carry no ID byte). This is
    /// exactly the argument [`crate::boot_keyboard_report`] takes.
    pub report_id: Option<u8>,
    /// Input payload bytes for the boot keyboard report, excluding HID-over-I2C's 2-byte length
    /// field and excluding any Report ID prefix.
    pub input_payload_bytes: usize,
    /// Full HID-over-I2C input frame bytes for the boot keyboard report: 2-byte length, optional
    /// Report ID, and payload.
    pub input_frame_bytes: usize,
}

/// Linux-like report-descriptor summary: the keyboard report KUMO can decode today, plus the
/// largest input frame implied by the parsed HID reports. — KESTREL
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReportDescriptorInfo {
    pub keyboard: KeyboardReport,
    pub max_input_frame_bytes: usize,
}

// HID usage pages / usages and the boot-keyboard report geometry (USB HID 1.11 App. B.1).
const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const USAGE_KEYBOARD: u16 = 0x06;
const COLLECTION_APPLICATION: u32 = 0x01;
/// 8 modifier bits + 8 reserved bits + six 8-bit keycodes.
const BOOT_KEYBOARD_INPUT_BITS: u32 = 8 + 8 + 6 * 8;

const MAX_GLOBAL_STACK: usize = 8; // HID Push/Pop depth we tolerate
const MAX_COLLECTION_DEPTH: usize = 16;
const HID_MAX_IDS: usize = 256;
const INPUT_FRAME_LENGTH_BYTES: usize = 2;

/// Global-item state, saved/restored by Push (0xA4) / Pop (0xB4).
#[derive(Clone, Copy, Default)]
struct Globals {
    usage_page: u16,
    report_size: u32,
    report_count: u32,
    report_id: Option<u8>,
}

/// Parse a HID report descriptor and confirm it contains a boot-shaped keyboard Application
/// Collection (Usage Page = Generic Desktop, Usage = Keyboard) whose **input** report is exactly
/// the conventional 64 bits. Returns the Report ID prefixing that keyboard's frames (or `None`).
///
/// Output reports (the LED block of a boot keyboard) are correctly *not* counted toward the input
/// length. The first boot-shaped keyboard collection wins; other collections (consumer control,
/// vendor) are skipped.
pub fn find_boot_keyboard(desc: &[u8]) -> Result<KeyboardReport, ReportDescriptorError> {
    Ok(inspect_report_descriptor(desc)?.keyboard)
}

/// Parse the report descriptor far enough to mirror Linux's i2c-hid sizing discipline:
/// report sizes come from HID report fields, not just from the firmware's wMaxInputLength. — KESTREL
pub fn inspect_report_descriptor(
    desc: &[u8],
) -> Result<ReportDescriptorInfo, ReportDescriptorError> {
    let mut globals = Globals::default();
    let mut gstack = [Globals::default(); MAX_GLOBAL_STACK];
    let mut gsp = 0usize;

    // Local state: the first Usage since the last Main item identifies a collection's usage.
    let mut pending_usage: Option<u16> = None;

    let mut depth = 0usize;
    // Depth at which the keyboard Application Collection opened, plus its accumulators.
    let mut kbd_open_depth: Option<usize> = None;
    let mut kbd_input_bits = 0u32;
    let mut kbd_report_id: Option<u8> = None;
    let mut keyboard = None;

    let mut input_bits = [0u32; HID_MAX_IDS];
    let mut input_seen = [false; HID_MAX_IDS];
    let mut input_numbered = false;

    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];

        // Long item (prefix 0xFE): bDataSize, bLongItemTag, then data. We never need a long
        // item's payload, but must skip it precisely.
        if prefix == 0xFE {
            let size = *desc.get(i + 1).ok_or(ReportDescriptorError::Truncated)? as usize;
            let end = i
                .checked_add(3 + size)
                .ok_or(ReportDescriptorError::Truncated)?;
            if end > desc.len() {
                return Err(ReportDescriptorError::Truncated);
            }
            i = end;
            continue;
        }

        // Short item: prefix bits [1:0] = size selector, [3:2] = type, [7:4] = tag.
        let data_len = [0usize, 1, 2, 4][(prefix & 0x03) as usize];
        let data_start = i + 1;
        let data_end = data_start
            .checked_add(data_len)
            .ok_or(ReportDescriptorError::Truncated)?;
        if data_end > desc.len() {
            return Err(ReportDescriptorError::Truncated);
        }
        let data = le_u32(&desc[data_start..data_end]);
        let b_type = (prefix >> 2) & 0x03;
        let b_tag = (prefix >> 4) & 0x0F;

        match b_type {
            0 => {
                // Main item. Clears local state afterward (HID 1.11 §6.2.2.8).
                match b_tag {
                    0x8 => {
                        // Input: account per-report size like Linux hid_add_field(), and count
                        // keyboard bits while inside the boot-keyboard collection. — KESTREL
                        let bits = report_bits(globals)?;
                        let report_index = report_index(globals.report_id)?;
                        input_seen[report_index] = true;
                        input_bits[report_index] = input_bits[report_index]
                            .checked_add(bits)
                            .ok_or(ReportDescriptorError::TooComplex)?;
                        if globals.report_id.is_some() {
                            input_numbered = true;
                        }
                        if kbd_open_depth.is_some() {
                            kbd_input_bits = kbd_input_bits
                                .checked_add(bits)
                                .ok_or(ReportDescriptorError::TooComplex)?;
                            kbd_report_id = globals.report_id;
                        }
                    }
                    0xA => {
                        // Collection: detect the boot-keyboard Application collection on open.
                        if data == COLLECTION_APPLICATION
                            && kbd_open_depth.is_none()
                            && globals.usage_page == USAGE_PAGE_GENERIC_DESKTOP
                            && pending_usage == Some(USAGE_KEYBOARD)
                        {
                            kbd_open_depth = Some(depth);
                            kbd_input_bits = 0;
                            kbd_report_id = globals.report_id;
                        }
                        depth = depth
                            .checked_add(1)
                            .filter(|d| *d <= MAX_COLLECTION_DEPTH)
                            .ok_or(ReportDescriptorError::TooComplex)?;
                    }
                    0xC => {
                        // End Collection.
                        depth = depth
                            .checked_sub(1)
                            .ok_or(ReportDescriptorError::Unbalanced)?;
                        if kbd_open_depth == Some(depth) {
                            // Closing the keyboard collection: accept iff it is boot-shaped.
                            if keyboard.is_none() && kbd_input_bits == BOOT_KEYBOARD_INPUT_BITS {
                                keyboard = Some(KeyboardReport {
                                    report_id: kbd_report_id,
                                    input_payload_bytes: bits_to_bytes(kbd_input_bits)?,
                                    input_frame_bytes: frame_bytes(
                                        kbd_input_bits,
                                        kbd_report_id.is_some(),
                                    )?,
                                });
                            }
                            // Not boot-shaped: keep scanning for another keyboard collection.
                            kbd_open_depth = None;
                        }
                    }
                    _ => {} // Output / Feature: do not affect the input-report length.
                }
                pending_usage = None;
            }
            1 => match b_tag {
                0x0 => globals.usage_page = data as u16,
                0x7 => globals.report_size = data,
                0x8 => {
                    if data == 0 || data >= HID_MAX_IDS as u32 {
                        return Err(ReportDescriptorError::InvalidReportId);
                    }
                    globals.report_id = Some(data as u8);
                }
                0x9 => globals.report_count = data,
                0xA => {
                    // Push
                    if gsp >= MAX_GLOBAL_STACK {
                        return Err(ReportDescriptorError::TooComplex);
                    }
                    gstack[gsp] = globals;
                    gsp += 1;
                }
                0xB => {
                    // Pop
                    gsp = gsp
                        .checked_sub(1)
                        .ok_or(ReportDescriptorError::Unbalanced)?;
                    globals = gstack[gsp];
                }
                _ => {} // logical/physical min/max, unit, unit exponent: not needed
            },
            // Local item: capture the first Usage (tag 0x0) since the last Main item; it names
            // the collection that an immediately-following Collection opens. Other local items
            // (Usage Min/Max, …) are not needed for the boot-shape gate and fall through.
            2 if b_tag == 0x0 && pending_usage.is_none() => {
                pending_usage = Some(data as u16);
            }
            _ => {} // other local items, and reserved type 3
        }

        i = data_end;
    }

    if depth != 0 {
        return Err(ReportDescriptorError::Unbalanced);
    }

    let keyboard = keyboard.ok_or(ReportDescriptorError::NotBootKeyboard)?;
    let max_input_frame_bytes = max_input_frame_bytes(&input_bits, &input_seen, input_numbered)?
        .max(keyboard.input_frame_bytes);
    Ok(ReportDescriptorInfo {
        keyboard,
        max_input_frame_bytes,
    })
}

/// Read up to four little-endian bytes as a `u32` (HID item data is little-endian, 0/1/2/4 bytes).
fn le_u32(bytes: &[u8]) -> u32 {
    let mut v = 0u32;
    for (n, &b) in bytes.iter().enumerate() {
        v |= (b as u32) << (8 * n as u32);
    }
    v
}

fn report_bits(globals: Globals) -> Result<u32, ReportDescriptorError> {
    globals
        .report_size
        .checked_mul(globals.report_count)
        .ok_or(ReportDescriptorError::TooComplex)
}

fn report_index(report_id: Option<u8>) -> Result<usize, ReportDescriptorError> {
    Ok(report_id.unwrap_or(0) as usize)
}

fn max_input_frame_bytes(
    input_bits: &[u32; HID_MAX_IDS],
    input_seen: &[bool; HID_MAX_IDS],
    input_numbered: bool,
) -> Result<usize, ReportDescriptorError> {
    let mut max = 0usize;
    for (&bits, &seen) in input_bits.iter().zip(input_seen.iter()) {
        if seen {
            max = max.max(frame_bytes(bits, input_numbered)?);
        }
    }
    Ok(max)
}

fn frame_bytes(bits: u32, numbered: bool) -> Result<usize, ReportDescriptorError> {
    bits_to_bytes(bits)?
        .checked_add(INPUT_FRAME_LENGTH_BYTES + usize::from(numbered))
        .ok_or(ReportDescriptorError::TooComplex)
}

fn bits_to_bytes(bits: u32) -> Result<usize, ReportDescriptorError> {
    Ok(bits
        .checked_add(7)
        .ok_or(ReportDescriptorError::TooComplex)? as usize
        / 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conventional USB boot-keyboard report descriptor (USB HID 1.11 Appendix B.1): an
    /// 8-bit modifier byte, an 8-bit reserved byte, a 5-bit LED **output** block (+3 padding),
    /// and a six-byte keycode array — 64 input bits total.
    const BOOT_KEYBOARD: &[u8] = &[
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x06, // Usage (Keyboard)
        0xA1, 0x01, // Collection (Application)
        0x05, 0x07, //   Usage Page (Keyboard/Keypad)
        0x19, 0xE0, //   Usage Minimum (LeftControl)
        0x29, 0xE7, //   Usage Maximum (Right GUI)
        0x15, 0x00, //   Logical Minimum (0)
        0x25, 0x01, //   Logical Maximum (1)
        0x75, 0x01, //   Report Size (1)
        0x95, 0x08, //   Report Count (8)
        0x81, 0x02, //   Input (Data,Var,Abs)  : modifiers (8 bits)
        0x95, 0x01, //   Report Count (1)
        0x75, 0x08, //   Report Size (8)
        0x81, 0x03, //   Input (Cnst,Var,Abs)  : reserved (8 bits)
        0x95, 0x05, //   Report Count (5)
        0x75, 0x01, //   Report Size (1)
        0x05, 0x08, //   Usage Page (LEDs)
        0x19, 0x01, //   Usage Minimum (Num Lock)
        0x29, 0x05, //   Usage Maximum (Kana)
        0x91, 0x02, //   Output (Data,Var,Abs) : LEDs (not an input)
        0x95, 0x01, //   Report Count (1)
        0x75, 0x03, //   Report Size (3)
        0x91, 0x03, //   Output (Cnst,Var,Abs) : LED padding (not an input)
        0x95, 0x06, //   Report Count (6)
        0x75, 0x08, //   Report Size (8)
        0x15, 0x00, //   Logical Minimum (0)
        0x25, 0x65, //   Logical Maximum (101)
        0x05, 0x07, //   Usage Page (Keyboard/Keypad)
        0x19, 0x00, //   Usage Minimum (0)
        0x29, 0x65, //   Usage Maximum (101)
        0x81, 0x00, //   Input (Data,Array)    : 6 keycodes (48 bits)
        0xC0, // End Collection
    ];

    /// A structurally boot-shaped keyboard collection carrying `Report ID (7)` — modifiers (8) +
    /// reserved (8) + six keycodes (48) = 64 input bits, the minimum the detector accepts.
    const BOOT_KEYBOARD_RID7: &[u8] = &[
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x06, // Usage (Keyboard)
        0xA1, 0x01, // Collection (Application)
        0x85, 0x07, //   Report ID (7)
        0x75, 0x01, //   Report Size (1)
        0x95, 0x08, //   Report Count (8)
        0x81, 0x02, //   Input (Data,Var,Abs)  : modifiers
        0x75, 0x08, //   Report Size (8)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x03, //   Input (Cnst,Var,Abs)  : reserved
        0x75, 0x08, //   Report Size (8)
        0x95, 0x06, //   Report Count (6)
        0x81, 0x00, //   Input (Data,Array)    : 6 keycodes
        0xC0, // End Collection
    ];

    /// A consumer-control collection (Report ID 2) followed by the keyboard collection
    /// (Report ID 1) — the real multi-report shape an Elan keyboard presents.
    const CONSUMER_THEN_KEYBOARD: &[u8] = &[
        // Consumer control — must be skipped.
        0x05, 0x0C, // Usage Page (Consumer)
        0x09, 0x01, // Usage (Consumer Control)
        0xA1, 0x01, // Collection (Application)
        0x85, 0x02, //   Report ID (2)
        0x75, 0x10, //   Report Size (16)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x00, //   Input (Array)
        0xC0, // End Collection
        // Keyboard — Report ID 1, 64 input bits.
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x06, // Usage (Keyboard)
        0xA1, 0x01, // Collection (Application)
        0x85, 0x01, //   Report ID (1)
        0x75, 0x01, //   Report Size (1)
        0x95, 0x08, //   Report Count (8)
        0x81, 0x02, //   Input  : modifiers
        0x75, 0x08, //   Report Size (8)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x03, //   Input  : reserved
        0x75, 0x08, //   Report Size (8)
        0x95, 0x06, //   Report Count (6)
        0x81, 0x00, //   Input  : 6 keycodes
        0xC0, // End Collection
    ];

    #[test]
    fn accepts_the_conventional_boot_keyboard_with_no_report_id() {
        assert_eq!(
            find_boot_keyboard(BOOT_KEYBOARD),
            Ok(KeyboardReport {
                report_id: None,
                input_payload_bytes: 8,
                input_frame_bytes: 10,
            })
        );
    }

    #[test]
    fn extracts_the_report_id_when_present() {
        assert_eq!(
            find_boot_keyboard(BOOT_KEYBOARD_RID7),
            Ok(KeyboardReport {
                report_id: Some(7),
                input_payload_bytes: 8,
                input_frame_bytes: 11,
            })
        );
    }

    #[test]
    fn finds_the_keyboard_collection_after_an_unrelated_one() {
        // The consumer collection (Report ID 2) is skipped; the keyboard's Report ID 1 wins.
        assert_eq!(
            find_boot_keyboard(CONSUMER_THEN_KEYBOARD),
            Ok(KeyboardReport {
                report_id: Some(1),
                input_payload_bytes: 8,
                input_frame_bytes: 11,
            })
        );
    }

    #[test]
    fn summarizes_the_largest_input_frame_like_linux_i2c_hid() {
        let info = inspect_report_descriptor(CONSUMER_THEN_KEYBOARD).unwrap();
        assert_eq!(info.keyboard.report_id, Some(1));
        // The consumer report is 16 bits and the keyboard report is 64 bits; numbered input reports
        // carry a report-id byte after HID-over-I2C's 2-byte length field.
        assert_eq!(info.max_input_frame_bytes, 11);
    }

    #[test]
    fn rejects_a_mouse() {
        let mouse = &[
            0x05, 0x01, // Usage Page (Generic Desktop)
            0x09, 0x02, // Usage (Mouse)
            0xA1, 0x01, // Collection (Application)
            0x75, 0x08, //   Report Size (8)
            0x95, 0x03, //   Report Count (3)
            0x81, 0x02, //   Input (Data,Var,Abs)
            0xC0, // End Collection
        ];
        assert_eq!(
            find_boot_keyboard(mouse),
            Err(ReportDescriptorError::NotBootKeyboard)
        );
    }

    #[test]
    fn rejects_a_keyboard_shaped_collection_with_the_wrong_input_width() {
        // Generic Desktop / Keyboard, but only 8 input bits — not the boot 64.
        let stunted = &[
            0x05, 0x01, // Usage Page (Generic Desktop)
            0x09, 0x06, // Usage (Keyboard)
            0xA1, 0x01, // Collection (Application)
            0x75, 0x08, //   Report Size (8)
            0x95, 0x01, //   Report Count (1)
            0x81, 0x02, //   Input (Data,Var,Abs)
            0xC0, // End Collection
        ];
        assert_eq!(
            find_boot_keyboard(stunted),
            Err(ReportDescriptorError::NotBootKeyboard)
        );
    }

    #[test]
    fn detects_a_truncated_item() {
        // `0x05` (Usage Page, size 1) with no following data byte.
        assert_eq!(
            find_boot_keyboard(&[0x05]),
            Err(ReportDescriptorError::Truncated)
        );
    }

    #[test]
    fn detects_an_unbalanced_end_collection() {
        assert_eq!(
            find_boot_keyboard(&[0xC0]),
            Err(ReportDescriptorError::Unbalanced)
        );
    }

    #[test]
    fn rejects_invalid_report_ids() {
        assert_eq!(
            find_boot_keyboard(&[
                0x05, 0x01, // Usage Page (Generic Desktop)
                0x09, 0x06, // Usage (Keyboard)
                0xA1, 0x01, // Collection (Application)
                0x85, 0x00, //   Report ID (0) is invalid in Linux hid-core too.
                0xC0, // End Collection
            ]),
            Err(ReportDescriptorError::InvalidReportId)
        );
    }
}
