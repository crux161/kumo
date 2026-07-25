#![no_std]
//j383

//! Allocation-free USB HID boot-keyboard report decoding.
//!
//! [`Decoder`] compares each 8-byte boot report with the preceding valid report and emits key
//! release/press edges. It deliberately owns no USB transport or hardware state, so the same core
//! can be host-tested now and embedded in the eventual xHCI keyboard driver.

/// Bytes in a USB HID boot-keyboard input report.
pub const REPORT_BYTES: usize = 8;
/// Key slots in a USB HID boot-keyboard input report (6-key rollover).
pub const REPORT_KEYS: usize = 6;
/// Most edges one report transition can produce: six releases plus six presses.
pub const MAX_EVENTS: usize = REPORT_KEYS * 2;
/// Longest terminal byte sequence emitted for one decoded key.
pub const MAX_TERMINAL_BYTES: usize = 4;

/// Modifier bits from byte zero of a boot-keyboard report.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const LEFT_CTRL: u8 = 1 << 0;
    pub const LEFT_SHIFT: u8 = 1 << 1;
    pub const LEFT_ALT: u8 = 1 << 2;
    pub const LEFT_GUI: u8 = 1 << 3;
    pub const RIGHT_CTRL: u8 = 1 << 4;
    pub const RIGHT_SHIFT: u8 = 1 << 5;
    pub const RIGHT_ALT: u8 = 1 << 6;
    pub const RIGHT_GUI: u8 = 1 << 7;

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn ctrl(self) -> bool {
        self.0 & (Self::LEFT_CTRL | Self::RIGHT_CTRL) != 0
    }

    pub const fn shift(self) -> bool {
        self.0 & (Self::LEFT_SHIFT | Self::RIGHT_SHIFT) != 0
    }

    pub const fn alt(self) -> bool {
        self.0 & (Self::LEFT_ALT | Self::RIGHT_ALT) != 0
    }

    pub const fn gui(self) -> bool {
        self.0 & (Self::LEFT_GUI | Self::RIGHT_GUI) != 0
    }
}

/// A decoded key meaning. Printable and terminal control bytes use [`KeySym::Ascii`]; keys that
/// do not fit the shell's byte stream retain a named symbol for a future richer input protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeySym {
    Ascii(u8),
    CapsLock,
    Function(u8),
    PrintScreen,
    ScrollLock,
    Pause,
    Insert,
    Home,
    PageUp,
    Delete,
    End,
    PageDown,
    Right,
    Left,
    Down,
    Up,
    NumLock,
    Application,
    Unknown(u8),
}

impl KeySym {
    /// Return the single byte meaning for printable/control keys, if any.
    pub const fn ascii(self) -> Option<u8> {
        match self {
            Self::Ascii(byte) => Some(byte),
            _ => None,
        }
    }

    /// Return the terminal byte sequence for keys that fit the current tty byte stream.
    pub const fn terminal_bytes(self) -> TerminalBytes {
        match self {
            Self::Ascii(byte) => TerminalBytes::one(byte),
            Self::Home => TerminalBytes::sequence([0x1b, b'[', b'H', 0], 3),
            Self::End => TerminalBytes::sequence([0x1b, b'[', b'F', 0], 3),
            Self::Right => TerminalBytes::sequence([0x1b, b'[', b'C', 0], 3),
            Self::Left => TerminalBytes::sequence([0x1b, b'[', b'D', 0], 3),
            Self::Down => TerminalBytes::sequence([0x1b, b'[', b'B', 0], 3),
            Self::Up => TerminalBytes::sequence([0x1b, b'[', b'A', 0], 3),
            Self::Delete => TerminalBytes::sequence([0x1b, b'[', b'3', b'~'], 4),
            _ => TerminalBytes::none(),
        }
    }
}

pub const fn apply_caps_lock_to_ascii(byte: u8, caps_lock: bool) -> u8 {
    if !caps_lock {
        return byte;
    }
    match byte {
        b'a'..=b'z' => byte - (b'a' - b'A'),
        b'A'..=b'Z' => byte + (b'a' - b'A'),
        _ => byte,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalBytes {
    bytes: [u8; MAX_TERMINAL_BYTES],
    len: u8,
}

impl TerminalBytes {
    pub const fn none() -> Self {
        Self {
            bytes: [0; MAX_TERMINAL_BYTES],
            len: 0,
        }
    }

    pub const fn one(byte: u8) -> Self {
        let mut bytes = [0; MAX_TERMINAL_BYTES];
        bytes[0] = byte;
        Self { bytes, len: 1 }
    }

    const fn sequence(bytes: [u8; MAX_TERMINAL_BYTES], len: u8) -> Self {
        Self { bytes, len }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// Direction of one report-to-report key edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyState {
    Pressed,
    Released,
}

/// One key edge. `usage` is retained even when the US-layout mapping is unknown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyEvent {
    pub usage: u8,
    pub state: KeyState,
    pub modifiers: Modifiers,
    pub symbol: KeySym,
}

const EMPTY_EVENT: KeyEvent = KeyEvent {
    usage: 0,
    state: KeyState::Released,
    modifiers: Modifiers::from_bits(0),
    symbol: KeySym::Unknown(0),
};

/// Fixed-capacity output from one report transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Events {
    entries: [KeyEvent; MAX_EVENTS],
    len: u8,
}

impl Events {
    const fn new() -> Self {
        Self {
            entries: [EMPTY_EVENT; MAX_EVENTS],
            len: 0,
        }
    }

    fn push(&mut self, event: KeyEvent) {
        debug_assert!((self.len as usize) < MAX_EVENTS);
        self.entries[self.len as usize] = event;
        self.len += 1;
    }

    pub fn as_slice(&self) -> &[KeyEvent] {
        &self.entries[..self.len as usize]
    }

    pub const fn len(&self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<'a> IntoIterator for &'a Events {
    type Item = &'a KeyEvent;
    type IntoIter = core::slice::Iter<'a, KeyEvent>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

/// A malformed HID report that must not replace the decoder's last valid state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// Usage IDs 1 through 3 are the HID boot-protocol error/rollover sentinels.
    Rollover,
}

/// Stateful report decoder. It tracks all six boot-protocol key slots and emits each edge once.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Decoder {
    modifiers: Modifiers,
    keys: [u8; REPORT_KEYS],
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            modifiers: Modifiers::from_bits(0),
            keys: [0; REPORT_KEYS],
        }
    }

    /// Decode an 8-byte boot-keyboard report.
    ///
    /// Releases precede presses, matching a physical transition where the old set is retired
    /// before the new set becomes active. Duplicate slots produce only one edge. A rollover/error
    /// report is rejected and leaves the preceding valid state intact.
    pub fn decode(&mut self, report: [u8; REPORT_BYTES]) -> Result<Events, DecodeError> {
        let modifiers = Modifiers::from_bits(report[0]);
        let keys: [u8; REPORT_KEYS] = report[2..].try_into().expect("fixed boot report");

        if keys.iter().any(|usage| (1..=3).contains(usage)) {
            return Err(DecodeError::Rollover);
        }

        let mut events = Events::new();
        for usage in unique_keys(&self.keys) {
            if !contains(&keys, usage) {
                events.push(KeyEvent {
                    usage,
                    state: KeyState::Released,
                    modifiers: self.modifiers,
                    symbol: key_sym(usage, self.modifiers),
                });
            }
        }
        for usage in unique_keys(&keys) {
            if !contains(&self.keys, usage) {
                events.push(KeyEvent {
                    usage,
                    state: KeyState::Pressed,
                    modifiers,
                    symbol: key_sym(usage, modifiers),
                });
            }
        }

        self.modifiers = modifiers;
        self.keys = keys;
        Ok(events)
    }
}

fn contains(keys: &[u8; REPORT_KEYS], usage: u8) -> bool {
    usage != 0 && keys.contains(&usage)
}

fn unique_keys(keys: &[u8; REPORT_KEYS]) -> impl Iterator<Item = u8> + '_ {
    keys.iter()
        .copied()
        .enumerate()
        .filter_map(|(index, usage)| {
            (usage != 0 && !keys[..index].contains(&usage)).then_some(usage)
        })
}

/// Map a USB HID keyboard-page usage through the boot keyboard's conventional US layout.
pub fn key_sym(usage: u8, modifiers: Modifiers) -> KeySym {
    if modifiers.ctrl() {
        if let Some(byte) = ctrl_ascii(usage, modifiers.shift()) {
            return KeySym::Ascii(byte);
        }
    }

    let shifted = modifiers.shift();
    match usage {
        0x04..=0x1d => {
            let lower = b'a' + usage - 0x04;
            KeySym::Ascii(if shifted { lower - 32 } else { lower })
        }
        0x1e..=0x27 => {
            const PLAIN: &[u8; 10] = b"1234567890";
            const SHIFTED: &[u8; 10] = b"!@#$%^&*()";
            let index = (usage - 0x1e) as usize;
            KeySym::Ascii(if shifted {
                SHIFTED[index]
            } else {
                PLAIN[index]
            })
        }
        0x28 => KeySym::Ascii(b'\n'),
        0x29 => KeySym::Ascii(0x1b),
        0x2a => KeySym::Ascii(0x7f),
        0x2b => KeySym::Ascii(b'\t'),
        0x2c => KeySym::Ascii(b' '),
        0x2d => shifted_pair(b'-', b'_', shifted),
        0x2e => shifted_pair(b'=', b'+', shifted),
        0x2f => shifted_pair(b'[', b'{', shifted),
        0x30 => shifted_pair(b']', b'}', shifted),
        0x31 => shifted_pair(b'\\', b'|', shifted),
        0x32 => shifted_pair(b'#', b'~', shifted),
        0x33 => shifted_pair(b';', b':', shifted),
        0x34 => shifted_pair(b'\'', b'"', shifted),
        0x35 => shifted_pair(b'`', b'~', shifted),
        0x36 => shifted_pair(b',', b'<', shifted),
        0x37 => shifted_pair(b'.', b'>', shifted),
        0x38 => shifted_pair(b'/', b'?', shifted),
        0x39 => KeySym::CapsLock,
        0x3a..=0x45 => KeySym::Function(usage - 0x39),
        0x46 => KeySym::PrintScreen,
        0x47 => KeySym::ScrollLock,
        0x48 => KeySym::Pause,
        0x49 => KeySym::Insert,
        0x4a => KeySym::Home,
        0x4b => KeySym::PageUp,
        0x4c => KeySym::Delete,
        0x4d => KeySym::End,
        0x4e => KeySym::PageDown,
        0x4f => KeySym::Right,
        0x50 => KeySym::Left,
        0x51 => KeySym::Down,
        0x52 => KeySym::Up,
        0x53 => KeySym::NumLock,
        0x54 => KeySym::Ascii(b'/'),
        0x55 => KeySym::Ascii(b'*'),
        0x56 => KeySym::Ascii(b'-'),
        0x57 => KeySym::Ascii(b'+'),
        0x58 => KeySym::Ascii(b'\n'),
        0x59..=0x61 => KeySym::Ascii(b'1' + usage - 0x59),
        0x62 => KeySym::Ascii(b'0'),
        0x63 => KeySym::Ascii(b'.'),
        0x65 => KeySym::Application,
        _ => KeySym::Unknown(usage),
    }
}

const fn shifted_pair(plain: u8, shifted: u8, is_shifted: bool) -> KeySym {
    KeySym::Ascii(if is_shifted { shifted } else { plain })
}

fn ctrl_ascii(usage: u8, shifted: bool) -> Option<u8> {
    match usage {
        0x04..=0x1d => Some(usage - 0x03),
        0x1f if shifted => Some(0),    // Ctrl-@ (Shift-2)
        0x2c => Some(0),               // Ctrl-Space
        0x2f => Some(0x1b),            // Ctrl-[
        0x30 => Some(0x1d),            // Ctrl-]
        0x31 => Some(0x1c),            // Ctrl-\
        0x23 if shifted => Some(0x1e), // Ctrl-^ (Shift-6)
        0x2d if shifted => Some(0x1f), // Ctrl-_ (Shift--)
        _ => None,
    }
}

/// USB base class for Human Interface Devices.
pub const USB_CLASS_HID: u8 = 3;
/// USB base class for hubs (device descriptor bDeviceClass, and the hub interface class).
pub const USB_CLASS_HUB: u8 = 9;
/// HID subclass indicating the device supports the boot protocol.
pub const HID_SUBCLASS_BOOT: u8 = 1;
/// HID boot-protocol interface protocols.
pub const HID_PROTOCOL_KEYBOARD: u8 = 1;
pub const HID_PROTOCOL_MOUSE: u8 = 2;

const DESC_INTERFACE: u8 = 4;
const DESC_ENDPOINT: u8 = 5;
const ENDPOINT_XFER_INTERRUPT: u8 = 3;
const ENDPOINT_DIR_IN: u8 = 0x80;

/// A boot-protocol HID interface and its interrupt-IN endpoint, located inside a USB
/// configuration descriptor. This is the target for setting up periodic report transfers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HidBootInterface {
    pub interface_number: u8,
    pub alternate_setting: u8,
    /// [`HID_PROTOCOL_KEYBOARD`] or [`HID_PROTOCOL_MOUSE`] (or another value for a boot device
    /// that is neither).
    pub protocol: u8,
    /// bEndpointAddress of the interrupt-IN endpoint (bit 7 set = IN).
    pub in_endpoint_address: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl HidBootInterface {
    pub const fn is_keyboard(&self) -> bool {
        self.protocol == HID_PROTOCOL_KEYBOARD
    }

    pub const fn is_mouse(&self) -> bool {
        self.protocol == HID_PROTOCOL_MOUSE
    }

    /// USB endpoint number (low nibble of the address), for the Device Context Index.
    pub const fn endpoint_number(&self) -> u8 {
        self.in_endpoint_address & 0x0f
    }
}

/// Walk a USB configuration descriptor blob and return the first boot-protocol HID interface
/// (class 3, subclass 1) paired with its first interrupt-IN endpoint. Allocation-free and
/// bounds-checked: a truncated or malformed blob yields `None` rather than panicking, so it is
/// safe to run directly over a DMA buffer the device filled.
pub fn find_hid_boot_interface(config: &[u8]) -> Option<HidBootInterface> {
    let mut index = 0usize;
    // The interface whose endpoints we are currently scanning, if it is a boot HID interface.
    let mut current: Option<(u8, u8, u8)> = None; // (interface_number, alternate_setting, protocol)
    while index + 2 <= config.len() {
        let length = config[index] as usize;
        let descriptor_type = config[index + 1];
        if length < 2 || index + length > config.len() {
            break;
        }
        match descriptor_type {
            DESC_INTERFACE if length >= 9 => {
                let class = config[index + 5];
                let subclass = config[index + 6];
                let protocol = config[index + 7];
                current = if class == USB_CLASS_HID && subclass == HID_SUBCLASS_BOOT {
                    Some((config[index + 2], config[index + 3], protocol))
                } else {
                    None
                };
            }
            DESC_ENDPOINT if length >= 7 => {
                if let Some((interface_number, alternate_setting, protocol)) = current {
                    let address = config[index + 2];
                    let attributes = config[index + 3];
                    if attributes & 0x3 == ENDPOINT_XFER_INTERRUPT && address & ENDPOINT_DIR_IN != 0
                    {
                        return Some(HidBootInterface {
                            interface_number,
                            alternate_setting,
                            protocol,
                            in_endpoint_address: address,
                            max_packet_size: u16::from_le_bytes([
                                config[index + 4],
                                config[index + 5],
                            ]),
                            interval: config[index + 6],
                        });
                    }
                }
            }
            _ => {}
        }
        index += length;
    }
    None
}

/// An interrupt-IN endpoint and the interface it belongs to, located in a configuration
/// descriptor. Used for a hub's status-change endpoint or any periodic-IN endpoint whose
/// interface is not a boot-HID one (so [`find_hid_boot_interface`] would skip it).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptInEndpoint {
    pub interface_number: u8,
    pub interface_class: u8,
    pub in_endpoint_address: u8,
    pub max_packet_size: u16,
    pub interval: u8,
}

impl InterruptInEndpoint {
    pub const fn endpoint_number(&self) -> u8 {
        self.in_endpoint_address & 0x0f
    }
}

/// Find the first interrupt-IN endpoint that belongs to an interface of `interface_class`. Walks
/// the configuration descriptor allocation-free and bounds-checked, so it is safe over a raw DMA
/// buffer. (For a boot keyboard/mouse prefer [`find_hid_boot_interface`], which also checks the
/// boot subclass/protocol.)
pub fn find_interrupt_in_endpoint(config: &[u8], interface_class: u8) -> Option<InterruptInEndpoint> {
    let mut index = 0usize;
    let mut current: Option<(u8, u8)> = None; // (interface_number, class) while class matches
    while index + 2 <= config.len() {
        let length = config[index] as usize;
        let descriptor_type = config[index + 1];
        if length < 2 || index + length > config.len() {
            break;
        }
        match descriptor_type {
            DESC_INTERFACE if length >= 9 => {
                let class = config[index + 5];
                current = if class == interface_class {
                    Some((config[index + 2], class))
                } else {
                    None
                };
            }
            DESC_ENDPOINT if length >= 7 => {
                if let Some((interface_number, class)) = current {
                    let address = config[index + 2];
                    let attributes = config[index + 3];
                    if attributes & 0x3 == ENDPOINT_XFER_INTERRUPT && address & ENDPOINT_DIR_IN != 0
                    {
                        return Some(InterruptInEndpoint {
                            interface_number,
                            interface_class: class,
                            in_endpoint_address: address,
                            max_packet_size: u16::from_le_bytes([
                                config[index + 4],
                                config[index + 5],
                            ]),
                            interval: config[index + 6],
                        });
                    }
                }
            }
            _ => {}
        }
        index += length;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(modifiers: u8, keys: &[u8]) -> [u8; REPORT_BYTES] {
        let mut report = [0; REPORT_BYTES];
        report[0] = modifiers;
        report[2..2 + keys.len()].copy_from_slice(keys);
        report
    }

    #[test]
    fn emits_press_once_then_release() {
        let mut decoder = Decoder::new();
        let pressed = decoder.decode(report(0, &[0x04])).unwrap();
        assert_eq!(
            pressed.as_slice(),
            &[KeyEvent {
                usage: 0x04,
                state: KeyState::Pressed,
                modifiers: Modifiers::from_bits(0),
                symbol: KeySym::Ascii(b'a'),
            }]
        );
        assert!(decoder.decode(report(0, &[0x04])).unwrap().is_empty());

        let released = decoder.decode(report(0, &[])).unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(released.as_slice()[0].state, KeyState::Released);
        assert_eq!(released.as_slice()[0].symbol, KeySym::Ascii(b'a'));
    }

    #[test]
    fn tracks_all_six_keys_and_orders_release_before_press() {
        let mut decoder = Decoder::new();
        decoder
            .decode(report(0, &[0x04, 0x05, 0x06, 0x07, 0x08, 0x09]))
            .unwrap();
        let events = decoder
            .decode(report(0, &[0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]))
            .unwrap();
        assert_eq!(events.len(), MAX_EVENTS);
        assert!(events.as_slice()[..REPORT_KEYS]
            .iter()
            .all(|event| event.state == KeyState::Released));
        assert!(events.as_slice()[REPORT_KEYS..]
            .iter()
            .all(|event| event.state == KeyState::Pressed));
    }

    #[test]
    fn duplicate_slots_do_not_duplicate_edges() {
        let mut decoder = Decoder::new();
        let events = decoder.decode(report(0, &[0x04, 0x04])).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn shift_maps_letters_digits_and_punctuation() {
        let shift = Modifiers::from_bits(Modifiers::LEFT_SHIFT);
        assert_eq!(key_sym(0x04, shift), KeySym::Ascii(b'A'));
        assert_eq!(key_sym(0x1e, shift), KeySym::Ascii(b'!'));
        assert_eq!(key_sym(0x38, shift), KeySym::Ascii(b'?'));
        assert_eq!(key_sym(0x2a, shift), KeySym::Ascii(0x7f));
    }

    #[test]
    fn caps_lock_toggles_only_printable_letter_case() {
        assert_eq!(apply_caps_lock_to_ascii(b'a', true), b'A');
        assert_eq!(apply_caps_lock_to_ascii(b'A', true), b'a');
        assert_eq!(apply_caps_lock_to_ascii(b'1', true), b'1');
        assert_eq!(apply_caps_lock_to_ascii(b'\x1b', true), b'\x1b');
        assert_eq!(apply_caps_lock_to_ascii(b'a', false), b'a');
        assert_eq!(apply_caps_lock_to_ascii(b'A', false), b'A');
    }

    #[test]
    fn delete_usage_maps_to_the_terminal_delete_sequence() {
        assert_eq!(key_sym(0x4c, Modifiers::default()), KeySym::Delete);
    }

    #[test]
    fn terminal_byte_contract_covers_cooked_tty_goals() {
        let plain = Modifiers::default();
        let shift = Modifiers::from_bits(Modifiers::LEFT_SHIFT);
        let ctrl = Modifiers::from_bits(Modifiers::LEFT_CTRL);

        assert_eq!(key_sym(0x04, plain), KeySym::Ascii(b'a'));
        assert_eq!(key_sym(0x28, plain), KeySym::Ascii(b'\n'));
        assert_eq!(key_sym(0x2a, plain), KeySym::Ascii(0x7f));
        assert_eq!(key_sym(0x4c, plain), KeySym::Delete);
        assert_eq!(key_sym(0x33, shift), KeySym::Ascii(b':'));
        assert_eq!(key_sym(0x38, shift), KeySym::Ascii(b'?'));
        assert_eq!(key_sym(0x06, ctrl), KeySym::Ascii(0x03)); // Ctrl-C
        assert_eq!(key_sym(0x0b, ctrl), KeySym::Ascii(0x08)); // Ctrl-H
        assert_eq!(key_sym(0x10, ctrl), KeySym::Ascii(0x0d)); // Ctrl-M
    }

    #[test]
    fn navigation_keys_emit_terminal_sequences() {
        let plain = Modifiers::default();

        assert_eq!(key_sym(0x4f, plain).terminal_bytes().as_slice(), b"\x1b[C");
        assert_eq!(key_sym(0x50, plain).terminal_bytes().as_slice(), b"\x1b[D");
        assert_eq!(key_sym(0x51, plain).terminal_bytes().as_slice(), b"\x1b[B");
        assert_eq!(key_sym(0x52, plain).terminal_bytes().as_slice(), b"\x1b[A");
        assert_eq!(key_sym(0x4a, plain).terminal_bytes().as_slice(), b"\x1b[H");
        assert_eq!(key_sym(0x4d, plain).terminal_bytes().as_slice(), b"\x1b[F");
    }

    #[test]
    fn either_control_key_maps_terminal_control_bytes() {
        for bit in [Modifiers::LEFT_CTRL, Modifiers::RIGHT_CTRL] {
            let ctrl = Modifiers::from_bits(bit);
            assert_eq!(key_sym(0x06, ctrl), KeySym::Ascii(0x03)); // Ctrl-C
            assert_eq!(key_sym(0x2f, ctrl), KeySym::Ascii(0x1b)); // Ctrl-[
            assert_eq!(key_sym(0x2c, ctrl), KeySym::Ascii(0)); // Ctrl-Space
        }
    }

    #[test]
    fn rollover_is_rejected_without_losing_held_keys() {
        let mut decoder = Decoder::new();
        decoder.decode(report(0, &[0x04])).unwrap();
        assert_eq!(
            decoder.decode(report(0, &[0x01])),
            Err(DecodeError::Rollover)
        );
        assert!(decoder.decode(report(0, &[0x04])).unwrap().is_empty());
        assert_eq!(decoder.decode(report(0, &[])).unwrap().len(), 1);
    }

    #[test]
    fn unknown_usage_is_preserved_as_a_keysym() {
        assert_eq!(key_sym(0xfe, Modifiers::default()), KeySym::Unknown(0xfe));
    }

    // Config(9) + Interface(9) + HID(9) + Endpoint(7) for one boot HID interface.
    fn boot_hid_config(protocol: u8, endpoint_address: u8, attributes: u8, mps: u16) -> [u8; 34] {
        let mps = mps.to_le_bytes();
        [
            // Configuration descriptor
            0x09, 0x02, 34, 0x00, 0x01, 0x01, 0x00, 0xa0, 0x32,
            // Interface descriptor: class 3 (HID), subclass 1 (boot), given protocol
            0x09, 0x04, 0x00, 0x00, 0x01, USB_CLASS_HID, HID_SUBCLASS_BOOT, protocol, 0x00,
            // HID descriptor (skipped by the walker)
            0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x3f, 0x00,
            // Endpoint descriptor
            0x07, 0x05, endpoint_address, attributes, mps[0], mps[1], 0x0a,
        ]
    }

    #[test]
    fn finds_a_boot_keyboards_interrupt_in_endpoint() {
        let config = boot_hid_config(HID_PROTOCOL_KEYBOARD, 0x81, 0x03, 8);
        let found = find_hid_boot_interface(&config).expect("keyboard interface");
        assert_eq!(found.protocol, HID_PROTOCOL_KEYBOARD);
        assert!(found.is_keyboard());
        assert_eq!(found.in_endpoint_address, 0x81);
        assert_eq!(found.endpoint_number(), 1);
        assert_eq!(found.max_packet_size, 8);
        assert_eq!(found.interval, 0x0a);
        assert_eq!(found.interface_number, 0);
    }

    #[test]
    fn finds_a_boot_mouse_and_reports_its_protocol() {
        let config = boot_hid_config(HID_PROTOCOL_MOUSE, 0x82, 0x03, 4);
        let found = find_hid_boot_interface(&config).expect("mouse interface");
        assert!(found.is_mouse());
        assert_eq!(found.endpoint_number(), 2);
    }

    #[test]
    fn ignores_non_interrupt_or_out_endpoints() {
        // A boot HID interface whose only endpoint is bulk, or is OUT, is not a match.
        assert_eq!(
            find_hid_boot_interface(&boot_hid_config(HID_PROTOCOL_KEYBOARD, 0x81, 0x02, 8)),
            None
        );
        assert_eq!(
            find_hid_boot_interface(&boot_hid_config(HID_PROTOCOL_KEYBOARD, 0x01, 0x03, 8)),
            None
        );
    }

    #[test]
    fn mass_storage_config_has_no_boot_hid_interface() {
        // Config + Interface(class 8, mass storage) + two bulk endpoints.
        let config = [
            0x09, 0x02, 32, 0x00, 0x01, 0x01, 0x00, 0x80, 0x32, //
            0x09, 0x04, 0x00, 0x00, 0x02, 0x08, 0x06, 0x50, 0x00, // class 8 mass storage
            0x07, 0x05, 0x81, 0x02, 0x00, 0x02, 0x00, // bulk IN
            0x07, 0x05, 0x02, 0x02, 0x00, 0x02, 0x00, // bulk OUT
        ];
        assert_eq!(find_hid_boot_interface(&config), None);
    }

    #[test]
    fn truncated_descriptor_yields_none_without_panicking() {
        let full = boot_hid_config(HID_PROTOCOL_KEYBOARD, 0x81, 0x03, 8);
        // Cut off inside the endpoint descriptor: the endpoint must not be reported.
        assert_eq!(find_hid_boot_interface(&full[..full.len() - 3]), None);
        // A zero-length descriptor must not loop forever.
        assert_eq!(find_hid_boot_interface(&[0x00, 0x02, 0x00, 0x00]), None);
        assert_eq!(find_hid_boot_interface(&[]), None);
    }

    #[test]
    fn finds_a_hubs_interrupt_in_status_endpoint() {
        // Config + Interface(class 9 hub) + Endpoint(interrupt IN).
        let config = [
            0x09, 0x02, 25, 0x00, 0x01, 0x01, 0x00, 0xe0, 0x00, //
            0x09, 0x04, 0x00, 0x00, 0x01, USB_CLASS_HUB, 0x00, 0x00, 0x00, //
            0x07, 0x05, 0x81, 0x03, 0x01, 0x00, 0x0c, // interrupt IN, mps 1, interval 12
        ];
        let found = find_interrupt_in_endpoint(&config, USB_CLASS_HUB).expect("hub status endpoint");
        assert_eq!(found.interface_class, USB_CLASS_HUB);
        assert_eq!(found.in_endpoint_address, 0x81);
        assert_eq!(found.max_packet_size, 1);
        assert_eq!(found.interval, 12);
        // A boot keyboard config has no hub-class interface.
        assert_eq!(
            find_interrupt_in_endpoint(
                &boot_hid_config(HID_PROTOCOL_KEYBOARD, 0x81, 0x03, 8),
                USB_CLASS_HUB
            ),
            None
        );
        // But it does have a HID-class interrupt endpoint.
        assert!(find_interrupt_in_endpoint(
            &boot_hid_config(HID_PROTOCOL_KEYBOARD, 0x81, 0x03, 8),
            USB_CLASS_HID
        )
        .is_some());
    }
}
