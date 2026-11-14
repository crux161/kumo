//! The client half of windowed TFTP (RFC 1350 + 2347 options + 2348 blksize + 7440
//! windowsize) as a pure state machine: it never touches a socket, so the window, gap and
//! wrap rules are host-tested. `payloadd`'s bin feeds it datagrams and does what it says.
//!
//! The shape mirrors the EDK2 Mtftp4 client the pxehost-kumo server was tuned against: ACK
//! every `window` in-order blocks and the final short block; an out-of-order block is dropped
//! and answered once with ACK(last in-order), which restarts the server's window there.

#![no_std]

pub const OP_RRQ: u16 = 1;
pub const OP_DATA: u16 = 3;
pub const OP_ACK: u16 = 4;
pub const OP_ERROR: u16 = 5;
pub const OP_OACK: u16 = 6;

/// What the caller must do with one received datagram.
#[derive(Debug, PartialEq, Eq)]
pub enum Rx<'a> {
    /// Options accepted: send ACK(0) and keep going.
    Oack { blksize: usize, window: u16 },
    /// An in-order block: store `bytes` at `offset`, then send `ack` if any. `last` ends it.
    Data {
        offset: u64,
        bytes: &'a [u8],
        ack: Option<u16>,
        last: bool,
    },
    /// A duplicate or out-of-order block: send `ack` if any (the gap ACK), store nothing.
    Skip { ack: Option<u16> },
    /// The server refused or ended the transfer.
    Error { code: u16 },
    /// Not a TFTP packet this transfer cares about.
    Ignore,
}

pub struct Receiver {
    blksize: usize,
    window: u16,
    /// Wire number of the next block wanted (wraps 65535 -> 0).
    expect: u16,
    /// In-order blocks stored so far.
    blocks: u64,
    /// In-order blocks since the last ACK we sent.
    since_ack: u16,
    /// A gap ACK is outstanding for this `expect`; do not repeat it per stray block.
    gap_acked: bool,
    started: bool,
    done: bool,
}

impl Receiver {
    /// A receiver that asked for `blksize` and `window`; until an OACK says otherwise the
    /// transfer runs at RFC 1350's 512 bytes, one block per ACK.
    pub const fn new() -> Self {
        Self {
            blksize: 512,
            window: 1,
            expect: 1,
            blocks: 0,
            since_ack: 0,
            gap_acked: false,
            started: false,
            done: false,
        }
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    pub fn blksize(&self) -> usize {
        self.blksize
    }

    pub fn window(&self) -> u16 {
        self.window
    }

    pub fn done(&self) -> bool {
        self.done
    }

    /// True once the server answered at all (OACK or DATA).
    pub fn started(&self) -> bool {
        self.started
    }

    /// The ACK to resend after a timeout: the last in-order block (0 before any data).
    pub fn timeout_ack(&mut self) -> u16 {
        self.since_ack = 0;
        self.gap_acked = false;
        self.expect.wrapping_sub(1)
    }

    pub fn on_packet<'a>(&mut self, packet: &'a [u8]) -> Rx<'a> {
        if packet.len() < 4 || self.done {
            return Rx::Ignore;
        }
        let op = u16::from_be_bytes([packet[0], packet[1]]);
        match op {
            OP_OACK if !self.started => {
                let (mut blksize, mut window) = (512usize, 1u16);
                let mut fields = packet[2..].split(|b| *b == 0);
                while let (Some(key), Some(value)) = (fields.next(), fields.next()) {
                    let n = parse_dec(value);
                    if key.eq_ignore_ascii_case(b"blksize") {
                        if let Some(n) = n.filter(|n| (8..=65464).contains(n)) {
                            blksize = n as usize;
                        }
                    } else if key.eq_ignore_ascii_case(b"windowsize") {
                        if let Some(n) = n.filter(|n| (1..=65535).contains(n)) {
                            window = n as u16;
                        }
                    }
                }
                self.blksize = blksize;
                self.window = window;
                self.started = true;
                Rx::Oack { blksize, window }
            }
            OP_DATA => {
                self.started = true;
                let block = u16::from_be_bytes([packet[2], packet[3]]);
                let bytes = &packet[4..];
                if block != self.expect || bytes.len() > self.blksize {
                    // One gap ACK per stall: the server restarts its window from it.
                    let ack = (!self.gap_acked).then(|| self.expect.wrapping_sub(1));
                    if ack.is_some() {
                        self.gap_acked = true;
                        self.since_ack = 0;
                    }
                    return Rx::Skip { ack };
                }
                let offset = self.blocks * self.blksize as u64;
                self.blocks += 1;
                self.expect = self.expect.wrapping_add(1);
                self.since_ack += 1;
                self.gap_acked = false;
                let last = bytes.len() < self.blksize;
                let ack = (last || self.since_ack >= self.window).then(|| {
                    self.since_ack = 0;
                    block
                });
                self.done = last;
                Rx::Data {
                    offset,
                    bytes,
                    ack,
                    last,
                }
            }
            OP_ERROR => Rx::Error {
                code: u16::from_be_bytes([packet[2], packet[3]]),
            },
            _ => Rx::Ignore,
        }
    }
}

impl Default for Receiver {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_dec(text: &[u8]) -> Option<u64> {
    if text.is_empty() || text.len() > 10 {
        return None;
    }
    text.iter().try_fold(0u64, |acc, b| {
        b.is_ascii_digit().then(|| acc * 10 + u64::from(b - b'0'))
    })
}

/// Encode an RRQ for `filename` in octet mode asking for `blksize` and `window`.
pub fn rrq(filename: &[u8], blksize: u16, window: u16, out: &mut [u8]) -> Option<usize> {
    let mut at = 0;
    let mut put = |bytes: &[u8]| -> Option<()> {
        out.get_mut(at..at + bytes.len())?.copy_from_slice(bytes);
        at += bytes.len();
        Some(())
    };
    put(&OP_RRQ.to_be_bytes())?;
    put(filename)?;
    put(b"\0octet\0blksize\0")?;
    let mut num = [0u8; 5];
    put(dec(blksize, &mut num))?;
    put(b"\0windowsize\0")?;
    put(dec(window, &mut num))?;
    put(b"\0")?;
    Some(at)
}

pub fn ack(block: u16) -> [u8; 4] {
    let [hi, lo] = block.to_be_bytes();
    let [ohi, olo] = OP_ACK.to_be_bytes();
    [ohi, olo, hi, lo]
}

fn dec(mut n: u16, buf: &mut [u8; 5]) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[at..];
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn data(block: u16, bytes: &[u8]) -> Vec<u8> {
        let mut p = Vec::from(OP_DATA.to_be_bytes());
        p.extend_from_slice(&block.to_be_bytes());
        p.extend_from_slice(bytes);
        p
    }

    fn oack(blk: &str, win: &str) -> Vec<u8> {
        let mut p = Vec::from(OP_OACK.to_be_bytes());
        for f in ["blksize", blk, "windowsize", win] {
            p.extend_from_slice(f.as_bytes());
            p.push(0);
        }
        p
    }

    #[test]
    fn rrq_asks_for_both_options() {
        let mut out = [0u8; 64];
        let n = rrq(b"EFI/KUMO/payload.img", 1468, 4, &mut out).unwrap();
        assert_eq!(
            &out[..n],
            b"\0\x01EFI/KUMO/payload.img\0octet\0blksize\01468\0windowsize\04\0"
        );
        assert_eq!(ack(0x1234), [0, 4, 0x12, 0x34]);
    }

    #[test]
    fn windowed_transfer_acks_every_window_and_the_last_block() {
        let mut rx = Receiver::new();
        assert_eq!(
            rx.on_packet(&oack("8", "4")),
            Rx::Oack {
                blksize: 8,
                window: 4
            }
        );
        let mut acks = Vec::new();
        let mut stored = Vec::new();
        for i in 1..=10u16 {
            let len = if i == 10 { 3 } else { 8 };
            let p = data(i, &std::vec![i as u8; len]);
            match rx.on_packet(&p) {
                Rx::Data {
                    offset,
                    bytes,
                    ack,
                    last,
                } => {
                    assert_eq!(offset, (i as u64 - 1) * 8);
                    stored.extend_from_slice(bytes);
                    acks.extend(ack);
                    assert_eq!(last, i == 10);
                }
                other => panic!("block {i}: {other:?}"),
            }
        }
        assert_eq!(acks, [4, 8, 10]);
        assert_eq!(stored.len(), 9 * 8 + 3);
        assert!(rx.done());
        assert_eq!(rx.on_packet(&data(11, b"x")), Rx::Ignore);
    }

    #[test]
    fn a_gap_is_acked_once_then_the_restarted_window_is_taken() {
        let mut rx = Receiver::new();
        rx.on_packet(&oack("8", "4"));
        let block = |i: u16| data(i, &[i as u8; 8]);
        assert!(matches!(
            rx.on_packet(&block(1)),
            Rx::Data { ack: None, .. }
        ));
        // 2 lost: 3 and 4 arrive. One gap ACK(1), then silence.
        assert_eq!(rx.on_packet(&block(3)), Rx::Skip { ack: Some(1) });
        assert_eq!(rx.on_packet(&block(4)), Rx::Skip { ack: None });
        // The server restarts at 2: 2..5 is a full window, acked at 5.
        for i in 2..=4 {
            assert!(
                matches!(rx.on_packet(&block(i)), Rx::Data { ack: None, .. }),
                "{i}"
            );
        }
        assert!(matches!(
            rx.on_packet(&block(5)),
            Rx::Data { ack: Some(5), .. }
        ));
        assert_eq!(rx.blocks(), 5);
    }

    #[test]
    fn a_timeout_re_acks_the_last_in_order_block() {
        let mut rx = Receiver::new();
        assert_eq!(
            rx.timeout_ack(),
            0,
            "before any data: ACK(0) re-arms an OACK'd transfer"
        );
        rx.on_packet(&oack("8", "4"));
        rx.on_packet(&data(1, &[1; 8]));
        rx.on_packet(&data(2, &[2; 8]));
        assert_eq!(rx.timeout_ack(), 2);
        // After the timeout ACK the window count restarts: 3..6 acks at 6.
        for i in 3..=5 {
            assert!(matches!(
                rx.on_packet(&data(i, &[0; 8])),
                Rx::Data { ack: None, .. }
            ));
        }
        assert!(matches!(
            rx.on_packet(&data(6, &[0; 8])),
            Rx::Data { ack: Some(6), .. }
        ));
    }

    #[test]
    fn without_an_oack_it_is_rfc_1350() {
        let mut rx = Receiver::new();
        assert!(matches!(
            rx.on_packet(&data(1, &[7; 512])),
            Rx::Data {
                ack: Some(1),
                last: false,
                ..
            }
        ));
        assert!(matches!(
            rx.on_packet(&data(2, &[])),
            Rx::Data {
                ack: Some(2),
                last: true,
                ..
            }
        ));
    }

    #[test]
    fn block_numbers_wrap() {
        let mut rx = Receiver::new();
        rx.on_packet(&oack("8", "1"));
        for i in 1..=70_000u32 {
            let block = (i % 65536) as u16;
            let p = data(block, &[0; 8]);
            let r = rx.on_packet(&p);
            assert!(matches!(r, Rx::Data { .. }), "{i}: {r:?}");
        }
        assert_eq!(rx.blocks(), 70_000);
    }

    #[test]
    fn errors_and_junk() {
        let mut rx = Receiver::new();
        assert_eq!(rx.on_packet(&[0, 5, 0, 1, b'x', 0]), Rx::Error { code: 1 });
        assert_eq!(rx.on_packet(&[0, 9, 0, 0]), Rx::Ignore);
        assert_eq!(rx.on_packet(&[0]), Rx::Ignore);
    }
}
