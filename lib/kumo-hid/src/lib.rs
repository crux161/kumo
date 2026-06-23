#![no_std]

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
    /// Return the byte that can be forwarded to the existing keyboard channel, if any.
    pub const fn ascii(self) -> Option<u8> {
        match self {
            Self::Ascii(byte) => Some(byte),
            _ => None,
        }
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
}
