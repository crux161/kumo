//j814

//! KNF1, the frame channel between netstack and one frame provider (DESIGN/026 §3).
//!
//! [`decode`] checks one message in isolation; [`Lane`] is netstack's view of the channel and
//! checks what a single message cannot show: interface generation, sequence and credit. Any
//! error from either is a violation: netstack drops the message, counts it and closes the
//! channel, and the provider comes back with a new generation.

use crate::budget::{FRAME_MAX, MTU, RX_CREDIT, TX_CREDIT};

pub const MAGIC: [u8; 4] = *b"KNF1";
pub const VERSION: u16 = 1;
pub const HEADER_LEN: usize = 32;
/// Smallest Ethernet frame carried: a bare header.
pub const FRAME_MIN: usize = 14;
pub const HELLO_LEN: usize = 16;
pub const LINK_LEN: usize = 8;
pub const TX_DONE_LEN: usize = 4;
pub const COUNTERS_LEN: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Op {
    Hello = 1,
    Link = 2,
    RxFrame = 3,
    TxFrame = 4,
    TxDone = 5,
    Credit = 6,
    Counters = 7,
    QueryCounters = 8,
}

impl Op {
    pub const fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::Link,
            3 => Self::RxFrame,
            4 => Self::TxFrame,
            5 => Self::TxDone,
            6 => Self::Credit,
            7 => Self::Counters,
            8 => Self::QueryCounters,
            _ => return None,
        })
    }

    /// Whether the provider sends this op (otherwise netstack does).
    pub const fn from_provider(self) -> bool {
        !matches!(self, Self::TxFrame | Self::Credit | Self::QueryCounters)
    }
}

/// What the provider did with a TX_FRAME. None of these means the peer received it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum TxStatus {
    HandedToBus = 0,
    Dropped = 1,
    DeviceError = 2,
}

impl TxStatus {
    pub const fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            0 => Self::HandedToBus,
            1 => Self::Dropped,
            2 => Self::DeviceError,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hello {
    pub mac: [u8; 6],
    pub mtu: u16,
    pub max_frame: u16,
    pub rx_credit_wanted: u16,
    pub tx_credit_offered: u16,
    pub flags: u16,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counters {
    pub rx_drop_no_credit: u64,
    pub rx_bad: u64,
    pub tx_drop: u64,
    pub tx_err: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Body<'a> {
    Hello(Hello),
    Link {
        up: bool,
        full_duplex: bool,
        mbps: u32,
    },
    RxFrame(&'a [u8]),
    TxFrame(&'a [u8]),
    TxDone {
        answers: u32,
        status: TxStatus,
    },
    Credit(u32),
    Counters(Counters),
    QueryCounters,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Frame<'a> {
    pub generation: u32,
    pub sequence: u32,
    pub body: Body<'a>,
}

impl Frame<'_> {
    pub const fn op(&self) -> Op {
        match self.body {
            Body::Hello(_) => Op::Hello,
            Body::Link { .. } => Op::Link,
            Body::RxFrame(_) => Op::RxFrame,
            Body::TxFrame(_) => Op::TxFrame,
            Body::TxDone { .. } => Op::TxDone,
            Body::Credit(_) => Op::Credit,
            Body::Counters(_) => Op::Counters,
            Body::QueryCounters => Op::QueryCounters,
        }
    }
}

/// Why one message was rejected on its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    Short,
    Magic,
    Version,
    Op,
    Reserved,
    /// Header payload length disagrees with the message length.
    Length,
    Handles,
    ZeroGeneration,
    ZeroSequence,
    /// A status on an op other than TX_DONE, or an unknown TX status.
    Status,
    /// A credit on an op other than CREDIT, or a CREDIT outside 1..=RX_CREDIT.
    Credit,
    /// Payload size wrong for the op.
    Payload,
    /// HELLO or LINK fields outside what this netstack can run.
    Field,
}

fn le16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn le64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

/// Decode one channel message that carried `handles` handles.
pub fn decode(message: &[u8], handles: usize) -> Result<Frame<'_>, DecodeError> {
    if handles != 0 {
        return Err(DecodeError::Handles);
    }
    let Some((header, payload)) = message.split_first_chunk::<HEADER_LEN>() else {
        return Err(DecodeError::Short);
    };
    if header[0..4] != MAGIC {
        return Err(DecodeError::Magic);
    }
    if le16(header, 4) != VERSION {
        return Err(DecodeError::Version);
    }
    let op = Op::from_u16(le16(header, 6)).ok_or(DecodeError::Op)?;
    if header[24..32].iter().any(|&b| b != 0) {
        return Err(DecodeError::Reserved);
    }
    if le16(header, 18) as usize != payload.len() {
        return Err(DecodeError::Length);
    }
    let generation = le32(header, 8);
    if generation == 0 {
        return Err(DecodeError::ZeroGeneration);
    }
    let sequence = le32(header, 12);
    if sequence == 0 {
        return Err(DecodeError::ZeroSequence);
    }
    let status = le16(header, 16);
    if status != 0 && op != Op::TxDone {
        return Err(DecodeError::Status);
    }
    let credit = le32(header, 20);
    if credit != 0 && op != Op::Credit {
        return Err(DecodeError::Credit);
    }
    let exact = |len: usize| {
        if payload.len() == len {
            Ok(())
        } else {
            Err(DecodeError::Payload)
        }
    };
    let body = match op {
        Op::Hello => {
            exact(HELLO_LEN)?;
            let hello = Hello {
                mac: payload[0..6].try_into().unwrap(),
                mtu: le16(payload, 6),
                max_frame: le16(payload, 8),
                rx_credit_wanted: le16(payload, 10),
                tx_credit_offered: le16(payload, 12),
                flags: le16(payload, 14),
            };
            let unicast = hello.mac[0] & 1 == 0 && hello.mac != [0; 6];
            if !unicast
                || hello.mtu as usize != MTU
                || hello.max_frame as usize != FRAME_MAX
                || hello.rx_credit_wanted == 0
                || hello.tx_credit_offered == 0
                || hello.flags != 0
            {
                return Err(DecodeError::Field);
            }
            Body::Hello(hello)
        }
        Op::Link => {
            exact(LINK_LEN)?;
            if payload[0] > 1 || payload[1] > 1 || le16(payload, 6) != 0 {
                return Err(DecodeError::Field);
            }
            Body::Link {
                up: payload[0] == 1,
                full_duplex: payload[1] == 1,
                mbps: le32(payload, 2),
            }
        }
        Op::RxFrame | Op::TxFrame => {
            if !(FRAME_MIN..=FRAME_MAX).contains(&payload.len()) {
                return Err(DecodeError::Payload);
            }
            if op == Op::RxFrame {
                Body::RxFrame(payload)
            } else {
                Body::TxFrame(payload)
            }
        }
        Op::TxDone => {
            exact(TX_DONE_LEN)?;
            let answers = le32(payload, 0);
            if answers == 0 {
                return Err(DecodeError::Field);
            }
            Body::TxDone {
                answers,
                status: TxStatus::from_u16(status).ok_or(DecodeError::Status)?,
            }
        }
        Op::Credit => {
            exact(0)?;
            if credit == 0 || credit as usize > RX_CREDIT {
                return Err(DecodeError::Credit);
            }
            Body::Credit(credit)
        }
        Op::Counters => {
            exact(COUNTERS_LEN)?;
            Body::Counters(Counters {
                rx_drop_no_credit: le64(payload, 0),
                rx_bad: le64(payload, 8),
                tx_drop: le64(payload, 16),
                tx_err: le64(payload, 24),
            })
        }
        Op::QueryCounters => {
            exact(0)?;
            Body::QueryCounters
        }
    };
    Ok(Frame {
        generation,
        sequence,
        body,
    })
}

/// Encode `frame` into `out`; returns the message length, or `None` if `out` is too small or the
/// frame would not decode (so this side can never emit what the other side must reject).
pub fn encode(frame: &Frame<'_>, out: &mut [u8]) -> Option<usize> {
    let mut payload = [0u8; COUNTERS_LEN];
    let (status, credit, body): (u16, u32, &[u8]) = match frame.body {
        Body::Hello(h) => {
            payload[0..6].copy_from_slice(&h.mac);
            payload[6..8].copy_from_slice(&h.mtu.to_le_bytes());
            payload[8..10].copy_from_slice(&h.max_frame.to_le_bytes());
            payload[10..12].copy_from_slice(&h.rx_credit_wanted.to_le_bytes());
            payload[12..14].copy_from_slice(&h.tx_credit_offered.to_le_bytes());
            payload[14..16].copy_from_slice(&h.flags.to_le_bytes());
            (0, 0, &payload[..HELLO_LEN])
        }
        Body::Link {
            up,
            full_duplex,
            mbps,
        } => {
            payload[0] = up as u8;
            payload[1] = full_duplex as u8;
            payload[2..6].copy_from_slice(&mbps.to_le_bytes());
            (0, 0, &payload[..LINK_LEN])
        }
        Body::RxFrame(bytes) | Body::TxFrame(bytes) => (0, 0, bytes),
        Body::TxDone { answers, status } => {
            payload[0..4].copy_from_slice(&answers.to_le_bytes());
            (status as u16, 0, &payload[..TX_DONE_LEN])
        }
        Body::Credit(credit) => (0, credit, &[][..]),
        Body::Counters(c) => {
            payload[0..8].copy_from_slice(&c.rx_drop_no_credit.to_le_bytes());
            payload[8..16].copy_from_slice(&c.rx_bad.to_le_bytes());
            payload[16..24].copy_from_slice(&c.tx_drop.to_le_bytes());
            payload[24..32].copy_from_slice(&c.tx_err.to_le_bytes());
            (0, 0, &payload[..COUNTERS_LEN])
        }
        Body::QueryCounters => (0, 0, &[][..]),
    };
    let len = HEADER_LEN.checked_add(body.len())?;
    if out.len() < len || body.len() > u16::MAX as usize {
        return None;
    }
    let header = &mut out[..HEADER_LEN];
    header.fill(0);
    header[0..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&(frame.op() as u16).to_le_bytes());
    header[8..12].copy_from_slice(&frame.generation.to_le_bytes());
    header[12..16].copy_from_slice(&frame.sequence.to_le_bytes());
    header[16..18].copy_from_slice(&status.to_le_bytes());
    header[18..20].copy_from_slice(&(body.len() as u16).to_le_bytes());
    header[20..24].copy_from_slice(&credit.to_le_bytes());
    out[HEADER_LEN..len].copy_from_slice(body);
    decode(&out[..len], 0).ok().map(|_| len)
}

/// Why a well-formed message still violated the channel's state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Violation {
    /// netstack-only op arriving from the provider.
    WrongDirection,
    /// Anything before the first HELLO.
    NoHello,
    /// HELLO reusing the current generation.
    ReusedGeneration,
    /// HELLO not starting its generation at sequence 1.
    HelloSequence,
    /// A generation other than the current one.
    StaleGeneration,
    /// A sequence other than the next expected one.
    SequenceGap,
    /// An RX_FRAME beyond the credit netstack granted.
    CreditOverrun,
    /// A TX_DONE with no TX_FRAME outstanding, or answering one that was not sent.
    UnsolicitedTxDone,
}

/// netstack's side of one frame channel: checks every inbound message against the channel
/// state and accounts both credits (DESIGN/026 §3).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Lane {
    generation: u32,
    next_in: u32,
    next_out: u32,
    rx_granted: u32,
    rx_outstanding: u32,
    tx_credit: u32,
    /// Sequence numbers of TX_FRAMEs awaiting TX_DONE are the contiguous range
    /// `oldest_tx .. oldest_tx + tx_in_flight`: TX_DONEs answer in order.
    oldest_tx: u32,
    tx_in_flight: u32,
}

impl Lane {
    pub const fn new() -> Self {
        Self {
            generation: 0,
            next_in: 0,
            next_out: 0,
            rx_granted: 0,
            rx_outstanding: 0,
            tx_credit: 0,
            oldest_tx: 0,
            tx_in_flight: 0,
        }
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }

    pub const fn tx_credit(&self) -> u32 {
        self.tx_credit
    }

    pub const fn rx_outstanding(&self) -> u32 {
        self.rx_outstanding
    }

    /// Check an inbound frame and apply its effect. On `Err` the channel must be closed.
    pub fn accept(&mut self, frame: &Frame<'_>) -> Result<(), Violation> {
        let op = frame.op();
        if !op.from_provider() {
            return Err(Violation::WrongDirection);
        }
        if let Body::Hello(hello) = frame.body {
            if frame.generation == self.generation {
                return Err(Violation::ReusedGeneration);
            }
            if frame.sequence != 1 {
                return Err(Violation::HelloSequence);
            }
            *self = Self {
                generation: frame.generation,
                next_in: 2,
                next_out: 1,
                rx_granted: (hello.rx_credit_wanted as u32).min(RX_CREDIT as u32),
                rx_outstanding: 0,
                tx_credit: (hello.tx_credit_offered as u32).min(TX_CREDIT as u32),
                oldest_tx: 1,
                tx_in_flight: 0,
            };
            return Ok(());
        }
        if self.generation == 0 {
            return Err(Violation::NoHello);
        }
        if frame.generation != self.generation {
            return Err(Violation::StaleGeneration);
        }
        if frame.sequence != self.next_in {
            return Err(Violation::SequenceGap);
        }
        match frame.body {
            Body::RxFrame(_) => {
                if self.rx_outstanding >= self.rx_granted {
                    return Err(Violation::CreditOverrun);
                }
                self.rx_outstanding += 1;
            }
            Body::TxDone { answers, .. } => {
                if self.tx_in_flight == 0 || answers != self.oldest_tx {
                    return Err(Violation::UnsolicitedTxDone);
                }
                self.oldest_tx = self.oldest_tx.wrapping_add(1);
                self.tx_in_flight -= 1;
                self.tx_credit += 1;
            }
            _ => {}
        }
        self.next_in = self.next_in.wrapping_add(1);
        Ok(())
    }

    /// The header for the next netstack → provider message, or `None` before HELLO.
    fn outbound<'a>(&mut self, body: Body<'a>) -> Option<Frame<'a>> {
        if self.generation == 0 {
            return None;
        }
        let sequence = self.next_out;
        self.next_out = self.next_out.wrapping_add(1);
        Some(Frame {
            generation: self.generation,
            sequence,
            body,
        })
    }

    /// Spend one TX credit on `frame`; `None` when no credit is left (backpressure).
    pub fn send_tx<'a>(&mut self, frame: &'a [u8]) -> Option<Frame<'a>> {
        if self.tx_credit == 0 {
            return None;
        }
        let out = self.outbound(Body::TxFrame(frame))?;
        self.tx_credit -= 1;
        if self.tx_in_flight == 0 {
            self.oldest_tx = out.sequence;
        }
        self.tx_in_flight += 1;
        Some(out)
    }

    /// Return `count` consumed RX credits to the provider.
    pub fn return_rx(&mut self, count: u32) -> Option<Frame<'static>> {
        if count == 0 || count > self.rx_outstanding {
            return None;
        }
        let out = self.outbound(Body::Credit(count))?;
        self.rx_outstanding -= count;
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    const MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 1];

    fn hello(generation: u32) -> Frame<'static> {
        Frame {
            generation,
            sequence: 1,
            body: Body::Hello(Hello {
                mac: MAC,
                mtu: MTU as u16,
                max_frame: FRAME_MAX as u16,
                rx_credit_wanted: 4,
                tx_credit_offered: 2,
                flags: 0,
            }),
        }
    }

    fn bytes(frame: &Frame<'_>) -> Vec<u8> {
        let mut out = [0u8; HEADER_LEN + FRAME_MAX];
        let len = encode(frame, &mut out).expect("encodes");
        out[..len].to_vec()
    }

    fn rx(generation: u32, sequence: u32) -> Frame<'static> {
        Frame {
            generation,
            sequence,
            body: Body::RxFrame(&[0xAB; 60]),
        }
    }

    #[test]
    fn every_op_round_trips() {
        let frames = [
            hello(7),
            Frame {
                generation: 7,
                sequence: 2,
                body: Body::Link {
                    up: true,
                    full_duplex: true,
                    mbps: 1000,
                },
            },
            rx(7, 3),
            Frame {
                generation: 7,
                sequence: 1,
                body: Body::TxFrame(&[0x11; FRAME_MAX]),
            },
            Frame {
                generation: 7,
                sequence: 4,
                body: Body::TxDone {
                    answers: 1,
                    status: TxStatus::Dropped,
                },
            },
            Frame {
                generation: 7,
                sequence: 2,
                body: Body::Credit(3),
            },
            Frame {
                generation: 7,
                sequence: 5,
                body: Body::Counters(Counters {
                    rx_drop_no_credit: 1,
                    rx_bad: 2,
                    tx_drop: 3,
                    tx_err: u64::MAX,
                }),
            },
            Frame {
                generation: 7,
                sequence: 3,
                body: Body::QueryCounters,
            },
        ];
        for frame in frames {
            let encoded = bytes(&frame);
            assert_eq!(decode(&encoded, 0), Ok(frame));
        }
    }

    #[test]
    fn header_rejections() {
        let good = bytes(&rx(7, 2));
        assert_eq!(decode(&good, 1), Err(DecodeError::Handles));
        assert_eq!(decode(&good[..31], 0), Err(DecodeError::Short));
        let mutate = |at: usize, value: u8| {
            let mut b = good.clone();
            b[at] = value;
            decode(&b, 0).map(|_| ())
        };
        assert_eq!(mutate(0, b'X'), Err(DecodeError::Magic));
        assert_eq!(mutate(4, 2), Err(DecodeError::Version));
        assert_eq!(mutate(6, 9), Err(DecodeError::Op));
        assert_eq!(mutate(6, 0), Err(DecodeError::Op));
        assert_eq!(mutate(31, 1), Err(DecodeError::Reserved));
        assert_eq!(mutate(16, 1), Err(DecodeError::Status));
        assert_eq!(mutate(20, 1), Err(DecodeError::Credit));
        let mut zero_gen = good.clone();
        zero_gen[8..12].fill(0);
        assert_eq!(decode(&zero_gen, 0), Err(DecodeError::ZeroGeneration));
        let mut zero_seq = good.clone();
        zero_seq[12..16].fill(0);
        assert_eq!(decode(&zero_seq, 0), Err(DecodeError::ZeroSequence));
        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(decode(&trailing, 0), Err(DecodeError::Length));
        assert_eq!(decode(&good[..good.len() - 1], 0), Err(DecodeError::Length));
    }

    #[test]
    fn payload_and_field_rejections() {
        let mut out = [0u8; HEADER_LEN + FRAME_MAX + 1];
        // encode refuses what decode would reject, so build bad messages by hand.
        let raw = |op: u16, status: u16, credit: u32, payload: &[u8], out: &mut [u8]| {
            out[..HEADER_LEN].fill(0);
            out[0..4].copy_from_slice(&MAGIC);
            out[4..6].copy_from_slice(&VERSION.to_le_bytes());
            out[6..8].copy_from_slice(&op.to_le_bytes());
            out[8..12].copy_from_slice(&7u32.to_le_bytes());
            out[12..16].copy_from_slice(&1u32.to_le_bytes());
            out[16..18].copy_from_slice(&status.to_le_bytes());
            out[18..20].copy_from_slice(&(payload.len() as u16).to_le_bytes());
            out[20..24].copy_from_slice(&credit.to_le_bytes());
            out[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
            HEADER_LEN + payload.len()
        };
        let mut check = |op: Op, status: u16, credit: u32, payload: &[u8], want: DecodeError| {
            let n = raw(op as u16, status, credit, payload, &mut out);
            assert_eq!(decode(&out[..n], 0), Err(want), "{op:?} {payload:?}");
        };
        check(Op::RxFrame, 0, 0, &[0; FRAME_MIN - 1], DecodeError::Payload);
        check(Op::TxFrame, 0, 0, &[0; FRAME_MAX + 1], DecodeError::Payload);
        check(Op::Hello, 0, 0, &[0; HELLO_LEN - 1], DecodeError::Payload);
        check(
            Op::Link,
            0,
            0,
            &[1, 1, 0, 0, 0, 0, 0, 0, 0],
            DecodeError::Payload,
        );
        check(
            Op::Link,
            0,
            0,
            &[2, 1, 0, 0, 0, 0, 0, 0],
            DecodeError::Field,
        );
        check(
            Op::Link,
            0,
            0,
            &[1, 1, 0, 0, 0, 0, 1, 0],
            DecodeError::Field,
        );
        check(Op::TxDone, 3, 0, &1u32.to_le_bytes(), DecodeError::Status);
        check(Op::TxDone, 0, 0, &0u32.to_le_bytes(), DecodeError::Field);
        check(Op::Credit, 0, 0, &[], DecodeError::Credit);
        check(
            Op::Credit,
            0,
            RX_CREDIT as u32 + 1,
            &[],
            DecodeError::Credit,
        );
        check(Op::Credit, 0, 1, &[0], DecodeError::Payload);
        check(Op::QueryCounters, 0, 0, &[0], DecodeError::Payload);
        check(
            Op::Counters,
            0,
            0,
            &[0; COUNTERS_LEN - 8],
            DecodeError::Payload,
        );

        let hello_payload = |edit: &dyn Fn(&mut [u8; HELLO_LEN])| {
            let mut p = [0u8; HELLO_LEN];
            p[0..6].copy_from_slice(&MAC);
            p[6..8].copy_from_slice(&(MTU as u16).to_le_bytes());
            p[8..10].copy_from_slice(&(FRAME_MAX as u16).to_le_bytes());
            p[10..12].copy_from_slice(&4u16.to_le_bytes());
            p[12..14].copy_from_slice(&2u16.to_le_bytes());
            edit(&mut p);
            p
        };
        type Edit<'a> = &'a dyn Fn(&mut [u8; HELLO_LEN]);
        let bad_hellos: [Edit<'_>; 7] = [
            &|p| p[0] |= 1,         // multicast MAC
            &|p| p[0..6].fill(0),   // all-zero MAC
            &|p| p[6] = 0,          // MTU 1280, not ours
            &|p| p[8] = 0,          // max frame not 1514
            &|p| p[10..12].fill(0), // no RX credit wanted
            &|p| p[12..14].fill(0), // no TX credit offered
            &|p| p[14] = 1,         // unknown flag
        ];
        for edit in bad_hellos {
            check(Op::Hello, 0, 0, &hello_payload(edit), DecodeError::Field);
        }
    }

    #[test]
    fn encode_refuses_what_decode_rejects() {
        let mut out = [0u8; HEADER_LEN + FRAME_MAX];
        let bad = Frame {
            generation: 0,
            sequence: 1,
            body: Body::QueryCounters,
        };
        assert_eq!(encode(&bad, &mut out), None);
        let short = Frame {
            generation: 1,
            sequence: 1,
            body: Body::RxFrame(&[0; 4]),
        };
        assert_eq!(encode(&short, &mut out), None);
        let fits = rx(1, 1);
        assert_eq!(encode(&fits, &mut out[..HEADER_LEN + 59]), None);
    }

    #[test]
    fn lane_tracks_generation_sequence_and_credit() {
        let mut lane = Lane::new();
        assert_eq!(lane.accept(&rx(7, 2)), Err(Violation::NoHello));
        assert_eq!(lane.send_tx(&[0; 60]), None, "no TX before HELLO");
        lane.accept(&hello(7)).unwrap();
        assert_eq!(lane.tx_credit(), 2);

        // RX: four granted, the fifth overruns.
        for seq in 2..6 {
            lane.accept(&rx(7, seq)).unwrap();
        }
        assert_eq!(lane.accept(&rx(7, 6)), Err(Violation::CreditOverrun));
        let credit = lane.return_rx(2).unwrap();
        assert_eq!(credit.body, Body::Credit(2));
        assert_eq!(lane.rx_outstanding(), 2);
        assert_eq!(lane.return_rx(3), None, "cannot return more than consumed");
        lane.accept(&rx(7, 6)).unwrap();

        // Sequence and generation.
        assert_eq!(lane.accept(&rx(7, 8)), Err(Violation::SequenceGap));
        assert_eq!(lane.accept(&rx(7, 6)), Err(Violation::SequenceGap));
        assert_eq!(lane.accept(&rx(6, 7)), Err(Violation::StaleGeneration));

        // TX: two credits, answered in order.
        let first = lane.send_tx(&[0; 60]).unwrap().sequence;
        let second = lane.send_tx(&[0; 60]).unwrap().sequence;
        assert_eq!(
            lane.send_tx(&[0; 60]),
            None,
            "credit exhausted: backpressure"
        );
        let done = |seq: u32, answers: u32| Frame {
            generation: 7,
            sequence: seq,
            body: Body::TxDone {
                answers,
                status: TxStatus::HandedToBus,
            },
        };
        assert_eq!(
            lane.accept(&done(7, second)),
            Err(Violation::UnsolicitedTxDone)
        );
        lane.accept(&done(7, first)).unwrap();
        lane.accept(&done(8, second)).unwrap();
        assert_eq!(lane.tx_credit(), 2);
        assert_eq!(
            lane.accept(&done(9, second + 1)),
            Err(Violation::UnsolicitedTxDone)
        );

        // Direction, and a new HELLO resets everything.
        let credit_in = Frame {
            generation: 7,
            sequence: 9,
            body: Body::Credit(1),
        };
        assert_eq!(lane.accept(&credit_in), Err(Violation::WrongDirection));
        assert_eq!(lane.accept(&hello(7)), Err(Violation::ReusedGeneration));
        let mut late = hello(8);
        late.sequence = 2;
        assert_eq!(lane.accept(&late), Err(Violation::HelloSequence));
        lane.accept(&hello(8)).unwrap();
        assert_eq!(lane.generation(), 8);
        assert_eq!(lane.rx_outstanding(), 0);
        assert_eq!(lane.accept(&rx(7, 2)), Err(Violation::StaleGeneration));
        lane.accept(&rx(8, 2)).unwrap();
    }

    #[test]
    fn credits_are_capped_at_the_budget() {
        let mut lane = Lane::new();
        let mut greedy = hello(3);
        if let Body::Hello(ref mut h) = greedy.body {
            h.rx_credit_wanted = u16::MAX;
            h.tx_credit_offered = u16::MAX;
        }
        lane.accept(&greedy).unwrap();
        assert_eq!(lane.tx_credit(), TX_CREDIT as u32);
        for seq in 0..RX_CREDIT as u32 {
            lane.accept(&rx(3, seq + 2)).unwrap();
        }
        assert_eq!(
            lane.accept(&rx(3, RX_CREDIT as u32 + 2)),
            Err(Violation::CreditOverrun)
        );
    }
}
