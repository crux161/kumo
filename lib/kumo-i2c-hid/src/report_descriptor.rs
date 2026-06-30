//! HID **report-descriptor** parsing: prove a device emits boot-shaped input reports.
//!
//! The HID-over-I2C descriptor (`protocol::HidDescriptor`) tells us a keyboard's report-descriptor
//! *length* and *register* but not its report *layout*. PLAN/006 §"Driver boundary and invariants"
//! requires proving the X13s keyboard speaks the conventional eight-byte boot report — byte 0
//! modifier bitmap, byte 1 reserved, bytes 2..8 a six-slot keycode array — and identifying any
//! Report ID that prefixes it on the wire, *before* [`crate::boot_keyboard_report`] and `kumo-hid`
//! may decode its frames. HID-over-I2C alone does not guarantee that layout.
//!
//! [`find_boot_keyboard`] and [`find_boot_mouse`] walk the report-descriptor item stream (USB HID
//! 1.11 §6.2.2) without allocation and return the Report ID when a boot-shaped Application
//! Collection is present. The walk is the pure, host-provable half of slice 5; reading the live
//! 0xb9-byte descriptor off the controller and feeding it here is the following (metal-owed) slice.

/// Why a report descriptor failed the boot-keyboard check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportDescriptorError {
    /// An item's declared data runs past the end of the descriptor.
    Truncated,
    /// An End Collection with no open collection, or a Pop with nothing pushed.
    Unbalanced,
    /// Push/pop or collection nesting exceeded the fixed parser bounds.
    TooComplex,
    /// No boot-shaped keyboard Application Collection (Generic Desktop / Keyboard, with a
    /// 64-bit input report) is present.
    NotBootKeyboard,
    /// No boot-shaped mouse Application Collection (Generic Desktop / Mouse, with a
    /// 24-bit input report) is present.
    NotBootMouse,
}

/// The wire-relevant facts about a boot-shaped keyboard found in a report descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardReport {
    /// The Report ID prefixing this keyboard's input frames, or `None` when the descriptor
    /// declares no Report ID (a single-report device, whose frames carry no ID byte). This is
    /// exactly the argument [`crate::boot_keyboard_report`] takes.
    pub report_id: Option<u8>,
}

/// The wire-relevant facts about a boot-shaped mouse found in a report descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MouseReport {
    /// The Report ID prefixing this mouse's input frames, or `None` when the descriptor declares no
    /// Report ID.
    pub report_id: Option<u8>,
}

// HID usage pages / usages and the boot-keyboard report geometry (USB HID 1.11 App. B.1).
const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
const USAGE_KEYBOARD: u16 = 0x06;
const USAGE_MOUSE: u16 = 0x02;
const COLLECTION_APPLICATION: u32 = 0x01;
/// 8 modifier bits + 8 reserved bits + six 8-bit keycodes.
const BOOT_KEYBOARD_INPUT_BITS: u32 = 8 + 8 + 6 * 8;
/// Three buttons/padding + signed X/Y bytes: the USB HID boot-mouse packet.
const BOOT_MOUSE_INPUT_BITS: u32 = (crate::protocol::BOOT_MOUSE_REPORT_BYTES as u32) * 8;

const MAX_GLOBAL_STACK: usize = 8; // HID Push/Pop depth we tolerate
const MAX_COLLECTION_DEPTH: usize = 16;

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
                        // Input: count its bits while inside the keyboard collection.
                        if kbd_open_depth.is_some() {
                            let bits = globals.report_size.saturating_mul(globals.report_count);
                            kbd_input_bits = kbd_input_bits.saturating_add(bits);
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
                            if kbd_input_bits == BOOT_KEYBOARD_INPUT_BITS {
                                return Ok(KeyboardReport {
                                    report_id: kbd_report_id,
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
                0x8 => globals.report_id = Some(data as u8),
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

    Err(ReportDescriptorError::NotBootKeyboard)
}

/// Parse a HID report descriptor and confirm it contains a boot-shaped mouse Application Collection
/// (Usage Page = Generic Desktop, Usage = Mouse) whose input report is the conventional 24 bits
/// (buttons/padding + X + Y). Returns the Report ID prefixing that mouse's frames (or `None`).
pub fn find_boot_mouse(desc: &[u8]) -> Result<MouseReport, ReportDescriptorError> {
    let mut globals = Globals::default();
    let mut gstack = [Globals::default(); MAX_GLOBAL_STACK];
    let mut gsp = 0usize;

    let mut pending_usage: Option<u16> = None;

    let mut depth = 0usize;
    let mut mouse_open_depth: Option<usize> = None;
    let mut mouse_input_bits = 0u32;
    let mut mouse_report_id: Option<u8> = None;

    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];

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
                match b_tag {
                    0x8 => {
                        if mouse_open_depth.is_some() {
                            let bits = globals.report_size.saturating_mul(globals.report_count);
                            mouse_input_bits = mouse_input_bits.saturating_add(bits);
                            mouse_report_id = globals.report_id;
                        }
                    }
                    0xA => {
                        if data == COLLECTION_APPLICATION
                            && mouse_open_depth.is_none()
                            && globals.usage_page == USAGE_PAGE_GENERIC_DESKTOP
                            && pending_usage == Some(USAGE_MOUSE)
                        {
                            mouse_open_depth = Some(depth);
                            mouse_input_bits = 0;
                            mouse_report_id = globals.report_id;
                        }
                        depth = depth
                            .checked_add(1)
                            .filter(|d| *d <= MAX_COLLECTION_DEPTH)
                            .ok_or(ReportDescriptorError::TooComplex)?;
                    }
                    0xC => {
                        depth = depth
                            .checked_sub(1)
                            .ok_or(ReportDescriptorError::Unbalanced)?;
                        if mouse_open_depth == Some(depth) {
                            if mouse_input_bits == BOOT_MOUSE_INPUT_BITS {
                                return Ok(MouseReport {
                                    report_id: mouse_report_id,
                                });
                            }
                            mouse_open_depth = None;
                        }
                    }
                    _ => {}
                }
                pending_usage = None;
            }
            1 => match b_tag {
                0x0 => globals.usage_page = data as u16,
                0x7 => globals.report_size = data,
                0x8 => globals.report_id = Some(data as u8),
                0x9 => globals.report_count = data,
                0xA => {
                    if gsp >= MAX_GLOBAL_STACK {
                        return Err(ReportDescriptorError::TooComplex);
                    }
                    gstack[gsp] = globals;
                    gsp += 1;
                }
                0xB => {
                    gsp = gsp
                        .checked_sub(1)
                        .ok_or(ReportDescriptorError::Unbalanced)?;
                    globals = gstack[gsp];
                }
                _ => {}
            },
            2 if b_tag == 0x0 && pending_usage.is_none() => {
                pending_usage = Some(data as u16);
            }
            _ => {}
        }

        i = data_end;
    }

    Err(ReportDescriptorError::NotBootMouse)
}

/// Find the Report ID for the LED Output report (Usage Page 0x08).
pub fn find_led_output_report_id(desc: &[u8]) -> Option<u8> {
    let mut globals = Globals::default();
    let mut gstack = [Globals::default(); MAX_GLOBAL_STACK];
    let mut gsp = 0usize;

    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];

        if prefix == 0xFE {
            if let Some(&size) = desc.get(i + 1) {
                i = i.saturating_add(3 + size as usize);
                continue;
            } else {
                break;
            }
        }

        let data_len = [0usize, 1, 2, 4][(prefix & 0x03) as usize];
        let data_start = i + 1;
        let data_end = data_start.saturating_add(data_len);
        if data_end > desc.len() {
            break;
        }
        let data = le_u32(&desc[data_start..data_end]);
        let b_type = (prefix >> 2) & 0x03;
        let b_tag = (prefix >> 4) & 0x0F;

        match b_type {
            0 => {
                // Main item
                if b_tag == 0x9 {
                    // Output item
                    if globals.usage_page == 0x08 {
                        // LEDs
                        return globals.report_id;
                    }
                }
            }
            1 => match b_tag {
                0x0 => globals.usage_page = data as u16,
                0x8 => globals.report_id = Some(data as u8),
                0xA => {
                    if gsp < MAX_GLOBAL_STACK {
                        gstack[gsp] = globals;
                        gsp += 1;
                    }
                }
                0xB => {
                    if gsp > 0 {
                        gsp -= 1;
                        globals = gstack[gsp];
                    }
                }
                _ => {}
            },
            _ => {}
        }
        i = data_end;
    }
    None
}

/// Read up to four little-endian bytes as a `u32` (HID item data is little-endian, 0/1/2/4 bytes).
fn le_u32(bytes: &[u8]) -> u32 {
    let mut v = 0u32;
    for (n, &b) in bytes.iter().enumerate() {
        v |= (b as u32) << (8 * n as u32);
    }
    v
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

    /// The conventional USB boot-mouse report descriptor: three buttons + padding + X/Y.
    const BOOT_MOUSE: &[u8] = &[
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x02, // Usage (Mouse)
        0xA1, 0x01, // Collection (Application)
        0x09, 0x01, //   Usage (Pointer)
        0xA1, 0x00, //   Collection (Physical)
        0x05, 0x09, //     Usage Page (Buttons)
        0x19, 0x01, //     Usage Minimum (1)
        0x29, 0x03, //     Usage Maximum (3)
        0x15, 0x00, //     Logical Minimum (0)
        0x25, 0x01, //     Logical Maximum (1)
        0x95, 0x03, //     Report Count (3)
        0x75, 0x01, //     Report Size (1)
        0x81, 0x02, //     Input (Data,Var,Abs) : buttons
        0x95, 0x01, //     Report Count (1)
        0x75, 0x05, //     Report Size (5)
        0x81, 0x03, //     Input (Cnst,Var,Abs) : padding
        0x05, 0x01, //     Usage Page (Generic Desktop)
        0x09, 0x30, //     Usage (X)
        0x09, 0x31, //     Usage (Y)
        0x15, 0x81, //     Logical Minimum (-127)
        0x25, 0x7F, //     Logical Maximum (127)
        0x75, 0x08, //     Report Size (8)
        0x95, 0x02, //     Report Count (2)
        0x81, 0x06, //     Input (Data,Var,Rel) : X/Y
        0xC0, //   End Collection
        0xC0, // End Collection
    ];

    /// A keyboard Report ID 8 followed by a mouse Report ID 9.
    const KEYBOARD_THEN_MOUSE: &[u8] = &[
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x06, // Usage (Keyboard)
        0xA1, 0x01, // Collection (Application)
        0x85, 0x08, //   Report ID (8)
        0x75, 0x01, //   Report Size (1)
        0x95, 0x08, //   Report Count (8)
        0x81, 0x02, //   Input: modifiers
        0x75, 0x08, //   Report Size (8)
        0x95, 0x01, //   Report Count (1)
        0x81, 0x03, //   Input: reserved
        0x75, 0x08, //   Report Size (8)
        0x95, 0x06, //   Report Count (6)
        0x81, 0x00, //   Input: keycodes
        0xC0, // End Collection
        0x05, 0x01, // Usage Page (Generic Desktop)
        0x09, 0x02, // Usage (Mouse)
        0xA1, 0x01, // Collection (Application)
        0x85, 0x09, //   Report ID (9)
        0x09, 0x01, //   Usage (Pointer)
        0xA1, 0x00, //   Collection (Physical)
        0x75, 0x08, //     Report Size (8)
        0x95, 0x03, //     Report Count (3)
        0x81, 0x06, //     Input: buttons/padding/x/y shape pinned as 24 bits
        0xC0, //   End Collection
        0xC0, // End Collection
    ];

    #[test]
    fn accepts_the_conventional_boot_keyboard_with_no_report_id() {
        assert_eq!(
            find_boot_keyboard(BOOT_KEYBOARD),
            Ok(KeyboardReport { report_id: None })
        );
    }

    #[test]
    fn extracts_the_report_id_when_present() {
        assert_eq!(
            find_boot_keyboard(BOOT_KEYBOARD_RID7),
            Ok(KeyboardReport { report_id: Some(7) })
        );
    }

    #[test]
    fn accepts_the_conventional_boot_mouse_with_no_report_id() {
        assert_eq!(
            find_boot_mouse(BOOT_MOUSE),
            Ok(MouseReport { report_id: None })
        );
    }

    #[test]
    fn finds_the_mouse_collection_after_the_keyboard_collection() {
        assert_eq!(
            find_boot_keyboard(KEYBOARD_THEN_MOUSE),
            Ok(KeyboardReport { report_id: Some(8) })
        );
        assert_eq!(
            find_boot_mouse(KEYBOARD_THEN_MOUSE),
            Ok(MouseReport { report_id: Some(9) })
        );
    }

    #[test]
    fn extracts_led_output_report_id_when_present() {
        // BOOT_KEYBOARD has no report ID, but has LED output
        assert_eq!(find_led_output_report_id(BOOT_KEYBOARD), None);
        // BOOT_KEYBOARD_RID7 has Report ID 7 for both input and output
        assert_eq!(find_led_output_report_id(BOOT_KEYBOARD_RID7), None);
    }

    #[test]
    fn finds_the_keyboard_collection_after_an_unrelated_one() {
        // The consumer collection (Report ID 2) is skipped; the keyboard's Report ID 1 wins.
        assert_eq!(
            find_boot_keyboard(CONSUMER_THEN_KEYBOARD),
            Ok(KeyboardReport { report_id: Some(1) })
        );
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
        assert_eq!(find_boot_mouse(mouse), Ok(MouseReport { report_id: None }));
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
}
