//j814
//j844

//! KNS1 requests and KNR1 replies, the session channel between a client and netstack
//! (DESIGN/026 §4, §6).
//!
//! [`decode_request`] checks one request in isolation; [`Session`] adds what a single message
//! cannot show (request order, session generation, the unanswered cap). Every rejection says
//! what netstack does about it ([`Disposition`]): answer with a status, answer and close, or
//! close without answering when the header itself cannot be trusted.

use crate::budget::{
    LISTEN_BACKLOG_MAX, SOCKETS_PER_SESSION, STREAM_PAYLOAD_MAX, UDP_PAYLOAD_MAX,
    UNANSWERED_PER_SESSION,
};

pub const REQUEST_MAGIC: [u8; 4] = *b"KNS1";
pub const REPLY_MAGIC: [u8; 4] = *b"KNR1";
pub const VERSION: u16 = 1;
pub const HEADER_LEN: usize = 32;
pub const ADDR_LEN: usize = 8;
pub const FAMILY_IPV4: u16 = 2;
pub const FLAG_NONBLOCK: u16 = 1;
pub const FLAG_PEEK: u16 = 2;
pub const WAIT_READABLE: u8 = 1;
pub const WAIT_WRITABLE: u8 = 2;
pub const WAIT_CONNECTED: u8 = 4;
pub const WAIT_TIMEOUT_MAX_MS: u32 = 60_000;
/// RECVFROM result bit: the datagram was longer than the requested maximum and was cut.
pub const RESULT_TRUNCATED: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Op {
    Hello = 1,
    Socket = 2,
    Bind = 3,
    Connect = 4,
    Listen = 5,
    Accept = 6,
    Send = 7,
    SendTo = 8,
    Recv = 9,
    RecvFrom = 10,
    Shutdown = 11,
    Close = 12,
    Wait = 13,
    GetName = 14,
}

impl Op {
    pub const fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::Socket,
            3 => Self::Bind,
            4 => Self::Connect,
            5 => Self::Listen,
            6 => Self::Accept,
            7 => Self::Send,
            8 => Self::SendTo,
            9 => Self::Recv,
            10 => Self::RecvFrom,
            11 => Self::Shutdown,
            12 => Self::Close,
            13 => Self::Wait,
            14 => Self::GetName,
            _ => return None,
        })
    }

    /// Ops addressed to the session, which carry socket id 0.
    pub const fn is_session_op(self) -> bool {
        matches!(self, Self::Hello | Self::Socket)
    }
}

/// DESIGN/026 §6. Unknown values never decode as `Ok`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Status {
    Ok = 0,
    BadVersion = 1,
    BadOp = 2,
    BadLength = 3,
    BadSocket = 4,
    StaleSession = 5,
    QuotaExceeded = 6,
    AddrInUse = 7,
    AddrNotAvailable = 8,
    ConnRefused = 9,
    ConnReset = 10,
    NotConnected = 11,
    AlreadyConnected = 12,
    WouldBlock = 13,
    InProgress = 14,
    TimedOut = 15,
    MessageTooLarge = 16,
    Unsupported = 17,
    InvalidArgument = 18,
    ShutDown = 19,
    NetUnreachable = 20,
    ProtocolViolation = 21,
}

impl Status {
    pub const fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            0 => Self::Ok,
            1 => Self::BadVersion,
            2 => Self::BadOp,
            3 => Self::BadLength,
            4 => Self::BadSocket,
            5 => Self::StaleSession,
            6 => Self::QuotaExceeded,
            7 => Self::AddrInUse,
            8 => Self::AddrNotAvailable,
            9 => Self::ConnRefused,
            10 => Self::ConnReset,
            11 => Self::NotConnected,
            12 => Self::AlreadyConnected,
            13 => Self::WouldBlock,
            14 => Self::InProgress,
            15 => Self::TimedOut,
            16 => Self::MessageTooLarge,
            17 => Self::Unsupported,
            18 => Self::InvalidArgument,
            19 => Self::ShutDown,
            20 => Self::NetUnreachable,
            21 => Self::ProtocolViolation,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketKind {
    Udp = 1,
    Tcp = 2,
    /// A wire-only ICMP echo socket (NET8); BIND's port is the echo identifier.
    Icmp = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Addr {
    pub ip: [u8; 4],
    pub port: u16,
}

impl Addr {
    pub fn encode(&self) -> [u8; ADDR_LEN] {
        let mut out = [0u8; ADDR_LEN];
        out[0..2].copy_from_slice(&FAMILY_IPV4.to_le_bytes());
        out[2..4].copy_from_slice(&self.port.to_le_bytes());
        out[4..8].copy_from_slice(&self.ip);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, Status> {
        if bytes.len() < ADDR_LEN {
            return Err(Status::BadLength);
        }
        if le16(bytes, 0) != FAMILY_IPV4 {
            return Err(Status::Unsupported);
        }
        Ok(Self {
            port: le16(bytes, 2),
            ip: [bytes[4], bytes[5], bytes[6], bytes[7]],
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Body<'a> {
    Hello,
    Socket(SocketKind),
    Bind(Addr),
    Connect(Addr),
    Listen { backlog: u8 },
    Accept,
    Send(&'a [u8]),
    SendTo(Addr, &'a [u8]),
    Recv { max: u16 },
    RecvFrom { max: u16 },
    Shutdown { read: bool, write: bool },
    Close,
    Wait { interest: u8, timeout_ms: u32 },
    GetName,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request<'a> {
    pub request_id: u32,
    pub socket: u32,
    pub generation: u32,
    pub flags: u16,
    pub body: Body<'a>,
}

/// What netstack does with a rejected request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disposition {
    /// The header cannot be trusted: no reply, close the session.
    Close,
    /// Answer with this status and keep serving.
    Reply(Status),
    /// Answer with this status, then close the session.
    ReplyThenClose(Status),
}

/// A rejected request: enough of the header to answer it, when it can be answered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rejected {
    pub request_id: u32,
    pub op: u16,
    pub socket: u32,
    pub disposition: Disposition,
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

/// Decode one request message that carried `handles` handles.
pub fn decode_request(message: &[u8], handles: usize) -> Result<Request<'_>, Rejected> {
    let close = Rejected {
        request_id: 0,
        op: 0,
        socket: 0,
        disposition: Disposition::Close,
    };
    let Some((header, payload)) = message.split_first_chunk::<HEADER_LEN>() else {
        return Err(close);
    };
    if handles != 0 || header[0..4] != REQUEST_MAGIC || le16(header, 22) as usize != payload.len() {
        return Err(close);
    }
    let request_id = le32(header, 8);
    if request_id == 0 {
        return Err(close);
    }
    let raw_op = le16(header, 6);
    let socket = le32(header, 12);
    let reject = |status: Status| Rejected {
        request_id,
        op: raw_op,
        socket,
        disposition: Disposition::Reply(status),
    };
    if le16(header, 4) != VERSION {
        return Err(reject(Status::BadVersion));
    }
    let op = Op::from_u16(raw_op).ok_or(reject(Status::BadOp))?;
    let generation = le32(header, 16);
    if (generation == 0) != (op == Op::Hello) {
        return Err(reject(Status::StaleSession));
    }
    if (socket == 0) != op.is_session_op() {
        return Err(reject(Status::BadSocket));
    }
    let flags = le16(header, 20);
    if flags & !(FLAG_NONBLOCK | FLAG_PEEK) != 0 {
        return Err(reject(Status::InvalidArgument));
    }
    let arg = le64(header, 24);
    let no_payload = || {
        if payload.is_empty() {
            Ok(())
        } else {
            Err(reject(Status::BadLength))
        }
    };
    let no_arg = || {
        if arg == 0 {
            Ok(())
        } else {
            Err(reject(Status::InvalidArgument))
        }
    };
    let addr_only = || {
        if payload.len() != ADDR_LEN {
            return Err(reject(Status::BadLength));
        }
        Addr::decode(payload).map_err(reject)
    };
    let body = match op {
        Op::Hello => {
            no_payload()?;
            no_arg()?;
            Body::Hello
        }
        Op::Socket => {
            no_payload()?;
            Body::Socket(match arg {
                1 => SocketKind::Udp,
                2 => SocketKind::Tcp,
                3 => SocketKind::Icmp,
                _ => return Err(reject(Status::Unsupported)),
            })
        }
        Op::Bind | Op::Connect => {
            no_arg()?;
            let addr = addr_only()?;
            if op == Op::Bind {
                Body::Bind(addr)
            } else {
                Body::Connect(addr)
            }
        }
        Op::Listen => {
            no_payload()?;
            if !(1..=LISTEN_BACKLOG_MAX as u64).contains(&arg) {
                return Err(reject(Status::InvalidArgument));
            }
            Body::Listen { backlog: arg as u8 }
        }
        Op::Send => {
            no_arg()?;
            if payload.len() > STREAM_PAYLOAD_MAX {
                return Err(reject(Status::MessageTooLarge));
            }
            Body::Send(payload)
        }
        Op::SendTo => {
            no_arg()?;
            let addr = Addr::decode(payload).map_err(reject)?;
            let data = &payload[ADDR_LEN..];
            if data.len() > UDP_PAYLOAD_MAX {
                return Err(reject(Status::MessageTooLarge));
            }
            Body::SendTo(addr, data)
        }
        Op::Recv | Op::RecvFrom => {
            no_payload()?;
            let cap = if op == Op::Recv {
                STREAM_PAYLOAD_MAX
            } else {
                UDP_PAYLOAD_MAX
            };
            if !(1..=cap as u64).contains(&arg) {
                return Err(reject(Status::InvalidArgument));
            }
            let max = arg as u16;
            if op == Op::Recv {
                Body::Recv { max }
            } else {
                Body::RecvFrom { max }
            }
        }
        Op::Shutdown => {
            no_payload()?;
            if !(1..=3).contains(&arg) {
                return Err(reject(Status::InvalidArgument));
            }
            Body::Shutdown {
                read: arg & 1 != 0,
                write: arg & 2 != 0,
            }
        }
        Op::Wait => {
            no_payload()?;
            let interest = (arg & 0xFF) as u8;
            let reserved = (arg >> 8) & 0xFF_FFFF;
            let timeout_ms = (arg >> 32) as u32;
            let known = WAIT_READABLE | WAIT_WRITABLE | WAIT_CONNECTED;
            if interest == 0
                || interest & !known != 0
                || reserved != 0
                || timeout_ms > WAIT_TIMEOUT_MAX_MS
            {
                return Err(reject(Status::InvalidArgument));
            }
            Body::Wait {
                interest,
                timeout_ms,
            }
        }
        Op::Accept | Op::Close | Op::GetName => {
            no_payload()?;
            no_arg()?;
            match op {
                Op::Accept => Body::Accept,
                Op::Close => Body::Close,
                _ => Body::GetName,
            }
        }
    };
    Ok(Request {
        request_id,
        socket,
        generation,
        flags,
        body,
    })
}

/// The fixed fields of a request, as a client fills them in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestHeader {
    pub op: Op,
    pub request_id: u32,
    pub socket: u32,
    pub generation: u32,
    pub flags: u16,
    pub arg: u64,
}

/// Encode a request into `out`; returns the length, or `None` if it would not decode or does
/// not fit. Clients use this, so a client can never send what netstack must reject.
pub fn encode_request(header: &RequestHeader, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let len = write_header(
        REQUEST_MAGIC,
        header.op as u16,
        header.request_id,
        header.socket,
        header.generation,
        header.flags,
        header.arg,
        payload,
        out,
    )?;
    decode_request(&out[..len], 0).ok().map(|_| len)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reply<'a> {
    pub op: u16,
    pub request_id: u32,
    pub socket: u32,
    pub generation: u32,
    pub status: Status,
    pub result: u64,
    pub payload: &'a [u8],
}

/// Encode a reply. Replies echo the request's raw op, so a `BadOp` answer can name it.
pub fn encode_reply(reply: &Reply<'_>, out: &mut [u8]) -> Option<usize> {
    if reply.request_id == 0 || reply.payload.len() > STREAM_PAYLOAD_MAX + 2 * ADDR_LEN {
        return None;
    }
    write_header(
        REPLY_MAGIC,
        reply.op,
        reply.request_id,
        reply.socket,
        reply.generation,
        reply.status as u16,
        reply.result,
        reply.payload,
        out,
    )
}

/// Why a client rejects a reply. Any of these means the session is unusable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyError {
    Short,
    Magic,
    Version,
    Length,
    Handles,
    ZeroRequestId,
    UnknownStatus,
}

pub fn decode_reply(message: &[u8], handles: usize) -> Result<Reply<'_>, ReplyError> {
    if handles != 0 {
        return Err(ReplyError::Handles);
    }
    let Some((header, payload)) = message.split_first_chunk::<HEADER_LEN>() else {
        return Err(ReplyError::Short);
    };
    if header[0..4] != REPLY_MAGIC {
        return Err(ReplyError::Magic);
    }
    if le16(header, 4) != VERSION {
        return Err(ReplyError::Version);
    }
    if le16(header, 22) as usize != payload.len() {
        return Err(ReplyError::Length);
    }
    let request_id = le32(header, 8);
    if request_id == 0 {
        return Err(ReplyError::ZeroRequestId);
    }
    Ok(Reply {
        op: le16(header, 6),
        request_id,
        socket: le32(header, 12),
        generation: le32(header, 16),
        status: Status::from_u16(le16(header, 20)).ok_or(ReplyError::UnknownStatus)?,
        result: le64(header, 24),
        payload,
    })
}

#[allow(clippy::too_many_arguments)]
fn write_header(
    magic: [u8; 4],
    op: u16,
    request_id: u32,
    socket: u32,
    generation: u32,
    word: u16,
    wide: u64,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let len = HEADER_LEN.checked_add(payload.len())?;
    if out.len() < len || payload.len() > u16::MAX as usize {
        return None;
    }
    out[0..4].copy_from_slice(&magic);
    out[4..6].copy_from_slice(&VERSION.to_le_bytes());
    out[6..8].copy_from_slice(&op.to_le_bytes());
    out[8..12].copy_from_slice(&request_id.to_le_bytes());
    out[12..16].copy_from_slice(&socket.to_le_bytes());
    out[16..20].copy_from_slice(&generation.to_le_bytes());
    out[20..22].copy_from_slice(&word.to_le_bytes());
    out[22..24].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    out[24..32].copy_from_slice(&wide.to_le_bytes());
    out[HEADER_LEN..len].copy_from_slice(payload);
    Some(len)
}

/// The HELLO result word: the session's limits, as DESIGN/026 §4 packs them.
pub const fn hello_limits() -> u64 {
    SOCKETS_PER_SESSION as u64
        | (UNANSWERED_PER_SESSION as u64) << 16
        | (STREAM_PAYLOAD_MAX as u64) << 32
        | (UDP_PAYLOAD_MAX as u64) << 48
}

/// netstack's per-session checks across requests: order, generation and the unanswered cap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Session {
    generation: u32,
    last_request: u32,
    unanswered: usize,
}

impl Session {
    /// A session whose HELLO reply will carry `generation` (nonzero, never reused by this
    /// netstack instance).
    pub const fn new(generation: u32) -> Self {
        Self {
            generation,
            last_request: 0,
            unanswered: 0,
        }
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }

    pub const fn unanswered(&self) -> usize {
        self.unanswered
    }

    /// Admit a decoded request. On `Ok` the caller owes exactly one reply and calls
    /// [`Session::answered`] when it sends it.
    pub fn admit(&mut self, request: &Request<'_>) -> Result<(), Rejected> {
        let reject = |disposition| Rejected {
            request_id: request.request_id,
            op: op_of(&request.body) as u16,
            socket: request.socket,
            disposition,
        };
        if request.request_id <= self.last_request {
            return Err(reject(Disposition::ReplyThenClose(
                Status::ProtocolViolation,
            )));
        }
        if self.unanswered >= UNANSWERED_PER_SESSION {
            return Err(reject(Disposition::ReplyThenClose(
                Status::ProtocolViolation,
            )));
        }
        self.last_request = request.request_id;
        if request.body != Body::Hello && request.generation != self.generation {
            return Err(reject(Disposition::Reply(Status::StaleSession)));
        }
        self.unanswered += 1;
        Ok(())
    }

    pub fn answered(&mut self) {
        self.unanswered = self.unanswered.saturating_sub(1);
    }
}

pub const fn op_of(body: &Body<'_>) -> Op {
    match body {
        Body::Hello => Op::Hello,
        Body::Socket(_) => Op::Socket,
        Body::Bind(_) => Op::Bind,
        Body::Connect(_) => Op::Connect,
        Body::Listen { .. } => Op::Listen,
        Body::Accept => Op::Accept,
        Body::Send(_) => Op::Send,
        Body::SendTo(..) => Op::SendTo,
        Body::Recv { .. } => Op::Recv,
        Body::RecvFrom { .. } => Op::RecvFrom,
        Body::Shutdown { .. } => Op::Shutdown,
        Body::Close => Op::Close,
        Body::Wait { .. } => Op::Wait,
        Body::GetName => Op::GetName,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    const ADDR: Addr = Addr {
        ip: [127, 0, 0, 1],
        port: 7000,
    };

    fn req(op: Op, socket: u32, generation: u32, arg: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = [0u8; HEADER_LEN + STREAM_PAYLOAD_MAX + ADDR_LEN];
        let len = write_header(
            REQUEST_MAGIC,
            op as u16,
            5,
            socket,
            generation,
            0,
            arg,
            payload,
            &mut out,
        )
        .unwrap();
        out[..len].to_vec()
    }

    fn reply_status(bytes: &[u8]) -> Disposition {
        decode_request(bytes, 0).unwrap_err().disposition
    }

    #[test]
    fn every_op_round_trips_through_encode_request() {
        let mut sendto = ADDR.encode().to_vec();
        sendto.extend_from_slice(b"datagram");
        let wait = WAIT_READABLE as u64 | (250u64 << 32);
        type Case<'a> = (Op, u32, u32, u64, &'a [u8], Body<'a>);
        let cases: [Case<'_>; 15] = [
            (Op::Hello, 0, 0, 0, &[], Body::Hello),
            (Op::Socket, 0, 9, 2, &[], Body::Socket(SocketKind::Tcp)),
            (Op::Socket, 0, 9, 3, &[], Body::Socket(SocketKind::Icmp)),
            (Op::Bind, 1, 9, 0, &ADDR.encode(), Body::Bind(ADDR)),
            (Op::Connect, 1, 9, 0, &ADDR.encode(), Body::Connect(ADDR)),
            (Op::Listen, 1, 9, 4, &[], Body::Listen { backlog: 4 }),
            (Op::Accept, 1, 9, 0, &[], Body::Accept),
            (Op::Send, 1, 9, 0, b"stream", Body::Send(b"stream")),
            (
                Op::SendTo,
                1,
                9,
                0,
                &sendto,
                Body::SendTo(ADDR, b"datagram"),
            ),
            (Op::Recv, 1, 9, 2048, &[], Body::Recv { max: 2048 }),
            (Op::RecvFrom, 1, 9, 1472, &[], Body::RecvFrom { max: 1472 }),
            (
                Op::Shutdown,
                1,
                9,
                3,
                &[],
                Body::Shutdown {
                    read: true,
                    write: true,
                },
            ),
            (Op::Close, 1, 9, 0, &[], Body::Close),
            (
                Op::Wait,
                1,
                9,
                wait,
                &[],
                Body::Wait {
                    interest: WAIT_READABLE,
                    timeout_ms: 250,
                },
            ),
            (Op::GetName, 1, 9, 0, &[], Body::GetName),
        ];
        let mut out = [0u8; 4096];
        for (op, socket, generation, arg, payload, body) in cases {
            let header = RequestHeader {
                op,
                request_id: 7,
                socket,
                generation,
                flags: FLAG_NONBLOCK,
                arg,
            };
            let len = encode_request(&header, payload, &mut out)
                .unwrap_or_else(|| panic!("{op:?} encodes"));
            let got = decode_request(&out[..len], 0).unwrap();
            assert_eq!(got.body, body, "{op:?}");
            assert_eq!(
                (got.request_id, got.socket, got.flags),
                (7, socket, FLAG_NONBLOCK)
            );
            assert_eq!(op_of(&got.body), op);
        }
    }

    #[test]
    fn untrusted_headers_close_without_a_reply() {
        let good = req(Op::Close, 1, 9, 0, &[]);
        assert!(decode_request(&good, 0).is_ok());
        let close = |bytes: &[u8], handles| {
            assert_eq!(
                decode_request(bytes, handles).unwrap_err().disposition,
                Disposition::Close
            )
        };
        close(&good, 1);
        close(&good[..HEADER_LEN - 1], 0);
        let mut magic = good.clone();
        magic[3] = b'2';
        close(&magic, 0);
        let mut trailing = good.clone();
        trailing.push(0);
        close(&trailing, 0);
        let mut zero_id = good.clone();
        zero_id[8..12].fill(0);
        close(&zero_id, 0);
    }

    #[test]
    fn answerable_rejections_name_their_status() {
        use Disposition::Reply;
        let mut version = req(Op::Close, 1, 9, 0, &[]);
        version[4] = 2;
        assert_eq!(reply_status(&version), Reply(Status::BadVersion));
        let mut op = req(Op::Close, 1, 9, 0, &[]);
        op[6] = 15;
        let rejected = decode_request(&op, 0).unwrap_err();
        assert_eq!(
            (rejected.op, rejected.request_id),
            (15, 5),
            "reply can echo the bad op"
        );
        assert_eq!(rejected.disposition, Reply(Status::BadOp));

        let mut huge_udp = ADDR.encode().to_vec();
        huge_udp.extend_from_slice(&[0; UDP_PAYLOAD_MAX + 1]);
        let mut bad_family = ADDR.encode();
        bad_family[0] = 10;
        let mut flags = req(Op::Close, 1, 9, 0, &[]);
        flags[20] = 4;
        let cases: [(Vec<u8>, Status); 22] = [
            (req(Op::Hello, 0, 3, 0, &[]), Status::StaleSession),
            (req(Op::Close, 1, 0, 0, &[]), Status::StaleSession),
            (req(Op::Socket, 1, 9, 1, &[]), Status::BadSocket),
            (req(Op::Close, 0, 9, 0, &[]), Status::BadSocket),
            (flags, Status::InvalidArgument),
            (req(Op::Hello, 0, 0, 1, &[]), Status::InvalidArgument),
            (req(Op::Hello, 0, 0, 0, &[0]), Status::BadLength),
            (req(Op::Socket, 0, 9, 4, &[]), Status::Unsupported),
            (
                req(Op::Bind, 1, 9, 0, &ADDR.encode()[..7]),
                Status::BadLength,
            ),
            (req(Op::Bind, 1, 9, 0, &bad_family), Status::Unsupported),
            (
                req(Op::Connect, 1, 9, 1, &ADDR.encode()),
                Status::InvalidArgument,
            ),
            (req(Op::Listen, 1, 9, 0, &[]), Status::InvalidArgument),
            (req(Op::Listen, 1, 9, 5, &[]), Status::InvalidArgument),
            (
                req(Op::Send, 1, 9, 0, &[0; STREAM_PAYLOAD_MAX + 1]),
                Status::MessageTooLarge,
            ),
            (req(Op::SendTo, 1, 9, 0, &huge_udp), Status::MessageTooLarge),
            (req(Op::SendTo, 1, 9, 0, &[2, 0]), Status::BadLength),
            (req(Op::Recv, 1, 9, 0, &[]), Status::InvalidArgument),
            (req(Op::RecvFrom, 1, 9, 1473, &[]), Status::InvalidArgument),
            (req(Op::Shutdown, 1, 9, 4, &[]), Status::InvalidArgument),
            (req(Op::Wait, 1, 9, 8, &[]), Status::InvalidArgument),
            (
                req(Op::Wait, 1, 9, 1 | (60_001u64 << 32), &[]),
                Status::InvalidArgument,
            ),
            (
                req(Op::Wait, 1, 9, 1 | (1 << 8), &[]),
                Status::InvalidArgument,
            ),
        ];
        for (bytes, status) in cases {
            assert_eq!(reply_status(&bytes), Reply(status), "{:?}", &bytes[..8]);
        }
    }

    #[test]
    fn encode_request_refuses_what_netstack_rejects() {
        let mut out = [0u8; 4096];
        let close = RequestHeader {
            op: Op::Close,
            request_id: 1,
            socket: 1,
            generation: 9,
            flags: 0,
            arg: 0,
        };
        let sendto = RequestHeader {
            op: Op::SendTo,
            ..close
        };
        assert_eq!(encode_request(&sendto, &[0; 4], &mut out), None);
        let zero_id = RequestHeader {
            request_id: 0,
            ..close
        };
        assert_eq!(encode_request(&zero_id, &[], &mut out), None);
        assert_eq!(encode_request(&close, &[], &mut out[..31]), None);
        assert!(encode_request(&close, &[], &mut out).is_some());
    }

    #[test]
    fn replies_round_trip_and_unknown_status_is_never_ok() {
        let reply = Reply {
            op: Op::RecvFrom as u16,
            request_id: 12,
            socket: 3,
            generation: 9,
            status: Status::Ok,
            result: 5 | RESULT_TRUNCATED,
            payload: b"addr+data",
        };
        let mut out = [0u8; 128];
        let len = encode_reply(&reply, &mut out).unwrap();
        assert_eq!(decode_reply(&out[..len], 0), Ok(reply));
        for value in 0..=21u16 {
            assert_eq!(Status::from_u16(value).map(|s| s as u16), Some(value));
        }
        out[20..22].copy_from_slice(&22u16.to_le_bytes());
        assert_eq!(decode_reply(&out[..len], 0), Err(ReplyError::UnknownStatus));
        assert_eq!(decode_reply(&out[..len], 1), Err(ReplyError::Handles));
        assert_eq!(decode_reply(&out[..31], 0), Err(ReplyError::Short));
        assert_eq!(decode_reply(&out[..len - 1], 0), Err(ReplyError::Length));
        let mut magic = out;
        magic[0] = b'X';
        assert_eq!(decode_reply(&magic[..len], 0), Err(ReplyError::Magic));
        assert_eq!(
            encode_reply(
                &Reply {
                    request_id: 0,
                    ..reply
                },
                &mut out
            ),
            None
        );
    }

    #[test]
    fn hello_limits_pack_the_budgets() {
        let word = hello_limits();
        assert_eq!(word & 0xFFFF, 16);
        assert_eq!((word >> 16) & 0xFFFF, 8);
        assert_eq!((word >> 32) & 0xFFFF, 2048);
        assert_eq!(word >> 48, 1472);
    }

    #[test]
    fn session_orders_requests_and_caps_unanswered() {
        let mut session = Session::new(9);
        fn with_id(op: Op, socket: u32, generation: u32, id: u32) -> Request<'static> {
            let mut b = req(op, socket, generation, 0, &[]);
            b[8..12].copy_from_slice(&id.to_le_bytes());
            let leaked: &'static [u8] = std::boxed::Box::leak(b.into_boxed_slice());
            decode_request(leaked, 0).unwrap()
        }
        session.admit(&with_id(Op::Hello, 0, 0, 1)).unwrap();
        session.answered();
        let stale = session.admit(&with_id(Op::Close, 1, 8, 2)).unwrap_err();
        assert_eq!(stale.disposition, Disposition::Reply(Status::StaleSession));
        let replay = session.admit(&with_id(Op::Close, 1, 9, 2)).unwrap_err();
        assert_eq!(
            replay.disposition,
            Disposition::ReplyThenClose(Status::ProtocolViolation),
            "request ids strictly increase, even after a rejection"
        );
        for id in 3..3 + UNANSWERED_PER_SESSION as u32 {
            session.admit(&with_id(Op::Close, 1, 9, id)).unwrap();
        }
        assert_eq!(session.unanswered(), UNANSWERED_PER_SESSION);
        let ninth = session
            .admit(&with_id(Op::Close, 1, 9, 3 + UNANSWERED_PER_SESSION as u32))
            .unwrap_err();
        assert_eq!(
            ninth.disposition,
            Disposition::ReplyThenClose(Status::ProtocolViolation)
        );
        session.answered();
        session
            .admit(&with_id(Op::Close, 1, 9, 20))
            .expect("a reply frees one slot");
    }
}
