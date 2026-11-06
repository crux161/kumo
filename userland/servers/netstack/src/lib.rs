#![no_std]
//j815
//j817

//! netstack's server core (PLAN/020 NET2b/NET2c, DESIGN/026): sessions over the KNS1/KNR1
//! socket contract, UDP and TCP sockets on smoltcp, one loopback interface. No syscalls live
//! here, so the whole request path runs under `cargo test -p netstack`; `main.rs` is the Port
//! loop around it.
//!
//! Every socket and buffer is allocated once in [`Server::new`]; nothing grows afterwards. Each
//! of the [`SOCKETS_TOTAL`] slots owns one UDP and one TCP smoltcp socket and uses whichever kind
//! the client asked for. A slot is reused only once its sockets are quiet: UDP by `close()`, TCP
//! once its FIN exchange or RST has actually left (see [`Slot::draining`]).

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use kumo_netstack::budget::{
    SESSIONS, SOCKETS_PER_SESSION, SOCKETS_TOTAL, SOCKET_BUFFER, STREAM_PAYLOAD_MAX, UDP_METADATA,
    UDP_PAYLOAD_MAX,
};
use kumo_netstack::smoltcp::iface::{
    Config, Interface, PollResult, SocketHandle, SocketSet, SocketStorage,
};
use kumo_netstack::smoltcp::socket::{tcp, udp};
use kumo_netstack::smoltcp::time::Instant;
use kumo_netstack::smoltcp::wire::{
    HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address,
};
use kumo_netstack::socket::{
    decode_request, encode_reply, hello_limits, Addr, Body, Disposition, Op, Rejected, Reply,
    Request, Session, SocketKind, Status, ADDR_LEN, HEADER_LEN, RESULT_TRUNCATED, WAIT_CONNECTED,
    WAIT_READABLE, WAIT_WRITABLE,
};
use kumo_netstack::Loopback;

/// The loopback interface's only address.
pub const LOOPBACK: [u8; 4] = [127, 0, 0, 1];
const EPHEMERAL_FIRST: u16 = 49152;
/// smoltcp polls per turn: each moves at least one packet, so this bounds a turn's work.
const POLLS_PER_TURN: usize = 16;
/// The largest reply: header + two addresses (GETNAME) or address + a full stream payload.
pub const REPLY_MAX: usize = HEADER_LEN + ADDR_LEN + STREAM_PAYLOAD_MAX;
/// How long a gracefully closed TCP socket may take to finish its FIN exchange before netstack
/// resets it and reuses the slot.
const DRAIN_NS: u64 = 5_000_000_000;

/// One message netstack must write to a session channel.
#[derive(Debug, Eq, PartialEq)]
pub struct Output {
    pub session: usize,
    pub bytes: Vec<u8>,
}

/// What the caller does with a session after handling one message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum After {
    Keep,
    /// Write the outputs, then close the channel and call [`Server::close_session`].
    Close,
}

#[derive(Clone, Copy)]
struct PendingWait {
    request_id: u32,
    interest: u8,
    deadline_ns: u64,
}

/// What a TCP slot is doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tcp {
    /// Created or bound; neither connecting nor listening.
    Fresh,
    /// CONNECT sent its SYN; WAIT reports connected or ConnRefused.
    Connecting,
    /// A connection (connected, or handed out by ACCEPT).
    Stream,
    /// A logical listener; its backlog lives in hidden slots whose `listener` names this one.
    Listener,
}

struct Slot {
    udp: SocketHandle,
    tcp: SocketHandle,
    kind: SocketKind,
    /// `(session index, socket id)` while a client holds it.
    owner: Option<(usize, u32)>,
    /// For a hidden backlog slot: the listener slot it serves and that listener's session.
    listener: Option<(usize, usize)>,
    port: u16,
    peer: Option<Addr>,
    wait: Option<PendingWait>,
    state: Tcp,
    /// SHUTDOWN read: RECV reports end of stream from now on.
    read_shut: bool,
    /// The peer's FIN was seen, so a later Closed state is an orderly end, not a reset.
    peer_finished: bool,
    /// Released by its client but still finishing a FIN exchange or sending an RST: the slot is
    /// reused only once smoltcp is done with it, or at this deadline.
    draining: Option<u64>,
}

impl Slot {
    fn is_free(&self) -> bool {
        self.owner.is_none() && self.listener.is_none() && self.draining.is_none()
    }
}

struct SessionSlot {
    state: Session,
    next_socket: u32,
    sockets: usize,
}

/// Counters for the status lines a proof reads; none of them ever resets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    pub sessions_opened: u64,
    pub sessions_closed: u64,
    pub sockets_freed: u64,
    pub datagrams_sent: u64,
    pub datagrams_received: u64,
    pub connections_accepted: u64,
    pub stream_bytes_sent: u64,
    pub stream_bytes_received: u64,
    pub rejected: u64,
}

pub struct Server {
    iface: Interface,
    /// Boxed: the 16-slot ring is ~24 KB, more than a native child's 16 KiB default stack.
    device: Box<Loopback>,
    sockets: SocketSet<'static>,
    slots: Vec<Slot>,
    sessions: Vec<Option<SessionSlot>>,
    next_generation: u32,
    next_ephemeral: u16,
    stats: Stats,
}

fn instant(now_ns: u64) -> Instant {
    Instant::from_micros((now_ns / 1000) as i64)
}

fn leak<T: Clone>(value: T, len: usize) -> &'static mut [T] {
    Box::leak(vec![value; len].into_boxed_slice())
}

/// The loopback ring, built directly on the heap. Its all-zero state is exactly
/// `Loopback::new()` (byte arrays, lengths, indices and counters, no references), so a zeroed
/// allocation never passes through the stack.
fn boxed_loopback() -> Box<Loopback> {
    // SAFETY: every field of `Loopback` is an integer or an array of integers, for which zero is
    // a valid value, and zero is the empty ring `Loopback::new()` builds.
    unsafe { Box::<Loopback>::new_zeroed().assume_init() }
}

fn is_loopback(ip: [u8; 4]) -> bool {
    ip[0] == 127
}

fn addr_of(endpoint: IpEndpoint) -> Addr {
    let IpAddress::Ipv4(ip) = endpoint.addr;
    Addr {
        ip: ip.octets(),
        port: endpoint.port,
    }
}

fn endpoint_of(addr: Addr) -> IpEndpoint {
    IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::from(addr.ip)), addr.port)
}

impl Server {
    pub fn new(now_ns: u64) -> Self {
        let mut device = boxed_loopback();
        let mut iface = Interface::new(
            Config::new(HardwareAddress::Ip),
            &mut *device,
            instant(now_ns),
        );
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(Ipv4Address::from(LOOPBACK)), 8));
        });
        let storage: &'static mut [SocketStorage<'static>] = Box::leak(
            (0..2 * SOCKETS_TOTAL)
                .map(|_| SocketStorage::EMPTY)
                .collect(),
        );
        let mut sockets = SocketSet::new(storage);
        let mut slots = Vec::with_capacity(SOCKETS_TOTAL);
        for _ in 0..SOCKETS_TOTAL {
            let udp = udp::Socket::new(
                udp::PacketBuffer::new(
                    leak(udp::PacketMetadata::EMPTY, UDP_METADATA),
                    leak(0u8, SOCKET_BUFFER),
                ),
                udp::PacketBuffer::new(
                    leak(udp::PacketMetadata::EMPTY, UDP_METADATA),
                    leak(0u8, SOCKET_BUFFER),
                ),
            );
            let tcp = tcp::Socket::new(
                tcp::SocketBuffer::new(leak(0u8, SOCKET_BUFFER)),
                tcp::SocketBuffer::new(leak(0u8, SOCKET_BUFFER)),
            );
            slots.push(Slot {
                udp: sockets.add(udp),
                tcp: sockets.add(tcp),
                kind: SocketKind::Udp,
                owner: None,
                listener: None,
                port: 0,
                peer: None,
                wait: None,
                state: Tcp::Fresh,
                read_shut: false,
                peer_finished: false,
                draining: None,
            });
        }
        Self {
            iface,
            device,
            sockets,
            slots,
            sessions: (0..SESSIONS).map(|_| None).collect(),
            next_generation: 1,
            next_ephemeral: EPHEMERAL_FIRST,
            stats: Stats::default(),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn live_sessions(&self) -> usize {
        self.sessions.iter().flatten().count()
    }

    /// Slots a client holds (visible sockets).
    pub fn live_sockets(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.owner.is_some())
            .count()
    }

    /// Slots not free for reuse: client sockets, hidden backlog listeners and draining TCP.
    pub fn busy_slots(&self) -> usize {
        self.slots.iter().filter(|slot| !slot.is_free()).count()
    }

    /// Admit a new session; `None` when all [`SESSIONS`] are in use (the caller closes the
    /// channel unanswered). Generations are never reused by this instance.
    pub fn open_session(&mut self) -> Option<usize> {
        let index = self.sessions.iter().position(Option::is_none)?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.checked_add(1).unwrap_or(1);
        self.sessions[index] = Some(SessionSlot {
            state: Session::new(generation),
            next_socket: 1,
            sockets: 0,
        });
        self.stats.sessions_opened += 1;
        Some(index)
    }

    /// Free a session and every socket it held; returns how many client sockets were freed.
    /// TCP connections are reset, never left half-open (DESIGN/026 §7).
    pub fn close_session(&mut self, session: usize) -> usize {
        if self
            .sessions
            .get_mut(session)
            .and_then(Option::take)
            .is_none()
        {
            return 0;
        }
        let mut freed = 0;
        for index in 0..self.slots.len() {
            let owned = matches!(self.slots[index].owner, Some((owner, _)) if owner == session);
            let backlog =
                matches!(self.slots[index].listener, Some((_, owner)) if owner == session);
            if owned {
                freed += 1;
            }
            if owned || backlog {
                self.abort(index);
            }
        }
        self.stats.sessions_closed += 1;
        self.stats.sockets_freed += freed as u64;
        freed
    }

    /// Detach a slot from its client and reset it. A TCP socket that may still owe its peer an
    /// RST is held as draining until the next poll has sent it.
    fn abort(&mut self, index: usize) {
        let tcp_live = self.slots[index].kind == SocketKind::Tcp
            && self
                .sockets
                .get::<tcp::Socket>(self.slots[index].tcp)
                .state()
                != tcp::State::Closed;
        self.sockets
            .get_mut::<tcp::Socket>(self.slots[index].tcp)
            .abort();
        self.forget(index);
        if tcp_live {
            // Zero deadline: free on the next poll, after the RST has been dispatched.
            self.slots[index].draining = Some(0);
        }
    }

    /// Clear every client-facing field; the smoltcp sockets are reset separately.
    fn forget(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.owner = None;
        slot.listener = None;
        slot.port = 0;
        slot.peer = None;
        slot.wait = None;
        slot.state = Tcp::Fresh;
        slot.read_shut = false;
        slot.peer_finished = false;
        slot.draining = None;
        self.sockets.get_mut::<udp::Socket>(slot.udp).close();
    }

    /// Handle one message read from `session`'s channel, which carried `handles` handles.
    pub fn handle(
        &mut self,
        session: usize,
        message: &[u8],
        handles: usize,
        now_ns: u64,
        out: &mut Vec<Output>,
    ) -> After {
        if self
            .sessions
            .get(session)
            .and_then(Option::as_ref)
            .is_none()
        {
            return After::Close;
        }
        let request = match decode_request(message, handles) {
            Ok(request) => request,
            Err(rejected) => return self.reject(session, rejected, out),
        };
        let admitted = self.sessions[session]
            .as_mut()
            .map(|s| s.state.admit(&request));
        if let Some(Err(rejected)) = admitted {
            return self.reject(session, rejected, out);
        }
        let op = kumo_netstack::socket::op_of(&request.body);
        // A WAIT still parked on a socket being closed is answered first, so every admitted
        // request gets exactly one reply and the unanswered count stays exact.
        if request.body == Body::Close {
            if let Ok(index) = self.slot_of(session, request.socket) {
                if let Some(wait) = self.slots[index].wait.take() {
                    self.push_reply(
                        session,
                        Op::Wait as u16,
                        wait.request_id,
                        request.socket,
                        Status::ShutDown,
                        0,
                        &[],
                        out,
                    );
                    self.answered(session);
                }
            }
        }
        match self.dispatch(session, &request, now_ns) {
            Answer::Now {
                status,
                socket,
                result,
                payload,
            } => {
                self.push_reply(
                    session,
                    op as u16,
                    request.request_id,
                    socket,
                    status,
                    result,
                    &payload,
                    out,
                );
                self.answered(session);
            }
            Answer::Deferred => {}
        }
        After::Keep
    }

    fn reject(&mut self, session: usize, rejected: Rejected, out: &mut Vec<Output>) -> After {
        self.stats.rejected += 1;
        let (status, after) = match rejected.disposition {
            Disposition::Close => return After::Close,
            Disposition::Reply(status) => (status, After::Keep),
            Disposition::ReplyThenClose(status) => (status, After::Close),
        };
        self.push_reply(
            session,
            rejected.op,
            rejected.request_id,
            rejected.socket,
            status,
            0,
            &[],
            out,
        );
        after
    }

    fn answered(&mut self, session: usize) {
        if let Some(s) = self.sessions[session].as_mut() {
            s.state.answered();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_reply(
        &self,
        session: usize,
        op: u16,
        request_id: u32,
        socket: u32,
        status: Status,
        result: u64,
        payload: &[u8],
        out: &mut Vec<Output>,
    ) {
        let generation = self.sessions[session]
            .as_ref()
            .map_or(0, |s| s.state.generation());
        let reply = Reply {
            op,
            request_id,
            socket,
            generation,
            status,
            result,
            payload,
        };
        let mut bytes = vec![0u8; REPLY_MAX];
        if let Some(len) = encode_reply(&reply, &mut bytes) {
            bytes.truncate(len);
            out.push(Output { session, bytes });
        }
    }

    fn slot_of(&self, session: usize, socket: u32) -> Result<usize, Status> {
        self.slots
            .iter()
            .position(|slot| slot.owner == Some((session, socket)))
            .ok_or(Status::BadSocket)
    }

    /// A port is taken while any client socket, backlog listener or draining TCP socket uses
    /// it with the same kind.
    fn port_in_use(&self, kind: SocketKind, port: u16) -> bool {
        self.slots
            .iter()
            .any(|slot| !slot.is_free() && slot.kind == kind && slot.port == port)
    }

    fn ephemeral(&mut self, kind: SocketKind) -> Option<u16> {
        let span = (u16::MAX - EPHEMERAL_FIRST) as usize + 1;
        for _ in 0..span {
            let port = self.next_ephemeral;
            self.next_ephemeral = if port == u16::MAX {
                EPHEMERAL_FIRST
            } else {
                port + 1
            };
            if !self.port_in_use(kind, port) {
                return Some(port);
            }
        }
        None
    }

    fn bind_slot(&mut self, index: usize, port: u16) -> Result<(), Status> {
        let kind = self.slots[index].kind;
        let port = match port {
            0 => self.ephemeral(kind).ok_or(Status::AddrInUse)?,
            port if self.port_in_use(kind, port) => return Err(Status::AddrInUse),
            port => port,
        };
        if kind == SocketKind::Udp {
            let endpoint = IpListenEndpoint { addr: None, port };
            self.sockets
                .get_mut::<udp::Socket>(self.slots[index].udp)
                .bind(endpoint)
                .map_err(|_| Status::InvalidArgument)?;
        }
        // A TCP port is only recorded here; smoltcp takes it at LISTEN or CONNECT.
        self.slots[index].port = port;
        Ok(())
    }

    fn ensure_bound(&mut self, index: usize) -> Result<(), Status> {
        if self.slots[index].port == 0 {
            self.bind_slot(index, 0)?;
        }
        Ok(())
    }

    fn send_datagram(&mut self, index: usize, to: Addr, data: &[u8]) -> Result<u64, Status> {
        if !is_loopback(to.ip) {
            return Err(Status::NetUnreachable);
        }
        if to.port == 0 {
            return Err(Status::InvalidArgument);
        }
        // smoltcp says Ok to an unfragmentable datagram and then drops it (J813), so the
        // limit is enforced here, before smoltcp sees the bytes. — TIMBERDOODLE
        if data.len() > UDP_PAYLOAD_MAX {
            return Err(Status::MessageTooLarge);
        }
        self.ensure_bound(index)?;
        match self
            .sockets
            .get_mut::<udp::Socket>(self.slots[index].udp)
            .send_slice(data, endpoint_of(to))
        {
            Ok(()) => {
                self.stats.datagrams_sent += 1;
                Ok(data.len() as u64)
            }
            Err(udp::SendError::BufferFull) => Err(Status::WouldBlock),
            Err(_) => Err(Status::InvalidArgument),
        }
    }

    /// Promote a CONNECT that completed, and remember a FIN once seen, so a later Closed state
    /// can be told apart from a reset.
    fn observe(&mut self, index: usize) {
        let slot = &self.slots[index];
        if slot.kind != SocketKind::Tcp {
            return;
        }
        let socket = self.sockets.get::<tcp::Socket>(slot.tcp);
        let state = socket.state();
        let finished = !socket.may_recv()
            && !matches!(
                state,
                tcp::State::Closed | tcp::State::SynSent | tcp::State::Listen
            );
        let slot = &mut self.slots[index];
        if slot.state == Tcp::Connecting && state == tcp::State::Established {
            slot.state = Tcp::Stream;
        }
        if slot.state == Tcp::Stream && finished {
            slot.peer_finished = true;
        }
    }

    /// The slot's readiness mask, or the error a WAIT must report instead.
    fn readiness(&mut self, index: usize) -> Result<u8, Status> {
        self.observe(index);
        let slot = &self.slots[index];
        if slot.kind == SocketKind::Udp {
            let socket = self.sockets.get::<udp::Socket>(slot.udp);
            let mut mask = 0;
            if socket.can_recv() {
                mask |= WAIT_READABLE;
            }
            if slot.port == 0 || socket.can_send() {
                mask |= WAIT_WRITABLE;
            }
            if slot.peer.is_some() {
                mask |= WAIT_CONNECTED;
            }
            return Ok(mask);
        }
        let socket = self.sockets.get::<tcp::Socket>(slot.tcp);
        match slot.state {
            Tcp::Fresh => Ok(0),
            Tcp::Connecting => {
                if socket.state() == tcp::State::Closed {
                    Err(Status::ConnRefused)
                } else {
                    Ok(0)
                }
            }
            Tcp::Listener => Ok(if self.acceptable(index).is_some() {
                WAIT_CONNECTED | WAIT_READABLE
            } else {
                0
            }),
            Tcp::Stream => {
                let mut mask = WAIT_CONNECTED;
                // End of stream and a reset both make a RECV answer at once.
                if socket.can_recv() || !socket.may_recv() || slot.read_shut {
                    mask |= WAIT_READABLE;
                }
                if socket.can_send() || !socket.may_send() {
                    mask |= WAIT_WRITABLE;
                }
                Ok(mask)
            }
        }
    }

    /// A backlog slot of `listener` whose connection is established and can be accepted.
    fn acceptable(&self, listener: usize) -> Option<usize> {
        self.slots.iter().position(|slot| {
            matches!(slot.listener, Some((l, _)) if l == listener)
                && matches!(
                    self.sockets.get::<tcp::Socket>(slot.tcp).state(),
                    tcp::State::Established | tcp::State::CloseWait
                )
        })
    }

    /// Arm one more hidden listener for `listener`; false when no slot is free.
    fn arm_backlog(&mut self, listener: usize, session: usize) -> bool {
        let port = self.slots[listener].port;
        let Some(index) = self.slots.iter().position(Slot::is_free) else {
            return false;
        };
        let slot = &mut self.slots[index];
        slot.kind = SocketKind::Tcp;
        slot.port = port;
        slot.listener = Some((listener, session));
        let armed = self
            .sockets
            .get_mut::<tcp::Socket>(self.slots[index].tcp)
            .listen(port)
            .is_ok();
        if !armed {
            self.forget(index);
        }
        armed
    }

    fn dispatch(&mut self, session: usize, request: &Request<'_>, now_ns: u64) -> Answer {
        match self.dispatch_inner(session, request, now_ns) {
            Ok(answer) => answer,
            Err(status) => Answer::status(status, request.socket),
        }
    }

    fn dispatch_inner(
        &mut self,
        session: usize,
        request: &Request<'_>,
        now_ns: u64,
    ) -> Result<Answer, Status> {
        let socket = request.socket;
        if let Body::Hello = request.body {
            return Ok(Answer::ok(0, hello_limits()));
        }
        if let Body::Socket(kind) = request.body {
            let held = self.sessions[session].as_ref().map_or(0, |s| s.sockets);
            let free = self.slots.iter().position(Slot::is_free);
            let (Some(index), true) = (free, held < SOCKETS_PER_SESSION) else {
                return Err(Status::QuotaExceeded);
            };
            let s = self.sessions[session].as_mut().ok_or(Status::BadSocket)?;
            let id = s.next_socket;
            s.next_socket += 1;
            s.sockets += 1;
            self.slots[index].kind = kind;
            self.slots[index].owner = Some((session, id));
            return Ok(Answer::ok(id, id as u64));
        }
        let index = self.slot_of(session, socket)?;
        match self.slots[index].kind {
            SocketKind::Udp => self.dispatch_udp(session, index, request, now_ns),
            SocketKind::Tcp => self.dispatch_tcp(session, index, request, now_ns),
        }
    }

    fn dispatch_common(
        &mut self,
        session: usize,
        index: usize,
        request: &Request<'_>,
        now_ns: u64,
    ) -> Result<Answer, Status> {
        let socket = request.socket;
        match request.body {
            Body::Bind(addr) => {
                if self.slots[index].port != 0 {
                    return Err(Status::InvalidArgument);
                }
                if addr.ip != LOOPBACK && addr.ip != [0; 4] {
                    return Err(Status::AddrNotAvailable);
                }
                self.bind_slot(index, addr.port)?;
                Ok(Answer::ok(socket, self.slots[index].port as u64))
            }
            Body::Wait {
                interest,
                timeout_ms,
            } => {
                if self.slots[index].wait.is_some() {
                    return Err(Status::InvalidArgument);
                }
                let ready = self.readiness(index)? & interest;
                if ready != 0 || timeout_ms == 0 {
                    return Ok(Answer::ok(socket, ready as u64));
                }
                self.slots[index].wait = Some(PendingWait {
                    request_id: request.request_id,
                    interest,
                    deadline_ns: now_ns.saturating_add(timeout_ms as u64 * 1_000_000),
                });
                Ok(Answer::Deferred)
            }
            Body::GetName => {
                let slot = &self.slots[index];
                let local = Addr {
                    ip: LOOPBACK,
                    port: slot.port,
                };
                let peer = if slot.kind == SocketKind::Tcp {
                    self.sockets
                        .get::<tcp::Socket>(slot.tcp)
                        .remote_endpoint()
                        .map(addr_of)
                } else {
                    slot.peer
                };
                let peer = peer.unwrap_or(Addr {
                    ip: [0; 4],
                    port: 0,
                });
                let mut payload = Vec::with_capacity(2 * ADDR_LEN);
                payload.extend_from_slice(&local.encode());
                payload.extend_from_slice(&peer.encode());
                Ok(Answer::payload(socket, 0, payload))
            }
            Body::Close => {
                self.close_socket(session, index, now_ns);
                Ok(Answer::ok(socket, 0))
            }
            _ => Err(Status::Unsupported),
        }
    }

    fn close_socket(&mut self, session: usize, index: usize, now_ns: u64) {
        if let Some(s) = self.sessions[session].as_mut() {
            s.sockets -= 1;
        }
        self.stats.sockets_freed += 1;
        let slot = &self.slots[index];
        if slot.kind == SocketKind::Udp || slot.state == Tcp::Fresh {
            self.forget(index);
            return;
        }
        if slot.state == Tcp::Listener {
            for backlog in 0..self.slots.len() {
                if matches!(self.slots[backlog].listener, Some((l, _)) if l == index) {
                    self.abort(backlog);
                }
            }
            self.forget(index);
            return;
        }
        // Connecting or Stream: FIN now, free once the exchange finishes or at the deadline.
        let handle = slot.tcp;
        let kind = slot.kind;
        let port = slot.port;
        self.sockets.get_mut::<tcp::Socket>(handle).close();
        self.forget(index);
        let slot = &mut self.slots[index];
        slot.kind = kind;
        slot.port = port;
        slot.draining = Some(now_ns.saturating_add(DRAIN_NS));
    }

    fn dispatch_udp(
        &mut self,
        session: usize,
        index: usize,
        request: &Request<'_>,
        now_ns: u64,
    ) -> Result<Answer, Status> {
        let socket = request.socket;
        match request.body {
            Body::Connect(addr) => {
                if !is_loopback(addr.ip) {
                    return Err(Status::NetUnreachable);
                }
                if addr.port == 0 {
                    return Err(Status::InvalidArgument);
                }
                self.ensure_bound(index)?;
                self.slots[index].peer = Some(addr);
                Ok(Answer::ok(socket, 0))
            }
            Body::Send(data) => {
                let peer = self.slots[index].peer.ok_or(Status::NotConnected)?;
                Ok(Answer::ok(socket, self.send_datagram(index, peer, data)?))
            }
            Body::SendTo(to, data) => Ok(Answer::ok(socket, self.send_datagram(index, to, data)?)),
            Body::Recv { max } | Body::RecvFrom { max } => {
                let with_addr = matches!(request.body, Body::RecvFrom { .. });
                let udp = self.sockets.get_mut::<udp::Socket>(self.slots[index].udp);
                let (data, meta) = udp.recv().map_err(|_| Status::WouldBlock)?;
                let take = data.len().min(max as usize);
                let mut payload = Vec::with_capacity(ADDR_LEN + take);
                if with_addr {
                    payload.extend_from_slice(&addr_of(meta.endpoint).encode());
                }
                payload.extend_from_slice(&data[..take]);
                let mut result = data.len() as u64;
                if data.len() > take {
                    result |= RESULT_TRUNCATED;
                }
                self.stats.datagrams_received += 1;
                Ok(Answer::payload(socket, result, payload))
            }
            Body::Listen { .. } | Body::Accept | Body::Shutdown { .. } => Err(Status::Unsupported),
            _ => self.dispatch_common(session, index, request, now_ns),
        }
    }

    fn dispatch_tcp(
        &mut self,
        session: usize,
        index: usize,
        request: &Request<'_>,
        now_ns: u64,
    ) -> Result<Answer, Status> {
        let socket = request.socket;
        self.observe(index);
        let handle = self.slots[index].tcp;
        match request.body {
            Body::Connect(addr) => {
                match self.slots[index].state {
                    Tcp::Fresh => {}
                    Tcp::Connecting => return Err(Status::InProgress),
                    _ => return Err(Status::AlreadyConnected),
                }
                if !is_loopback(addr.ip) {
                    return Err(Status::NetUnreachable);
                }
                if addr.port == 0 {
                    return Err(Status::InvalidArgument);
                }
                self.ensure_bound(index)?;
                let local = self.slots[index].port;
                self.sockets
                    .get_mut::<tcp::Socket>(handle)
                    .connect(self.iface.context(), endpoint_of(addr), local)
                    .map_err(|_| Status::InvalidArgument)?;
                self.slots[index].state = Tcp::Connecting;
                Ok(Answer::status(Status::InProgress, socket))
            }
            Body::Listen { backlog } => {
                if self.slots[index].state != Tcp::Fresh {
                    return Err(Status::InvalidArgument);
                }
                self.ensure_bound(index)?;
                self.slots[index].state = Tcp::Listener;
                for _ in 0..backlog {
                    if !self.arm_backlog(index, session) {
                        // Not enough free slots for the backlog: undo it all.
                        for slot in 0..self.slots.len() {
                            if matches!(self.slots[slot].listener, Some((l, _)) if l == index) {
                                self.abort(slot);
                            }
                        }
                        self.slots[index].state = Tcp::Fresh;
                        return Err(Status::QuotaExceeded);
                    }
                }
                Ok(Answer::ok(socket, 0))
            }
            Body::Accept => {
                if self.slots[index].state != Tcp::Listener {
                    return Err(Status::InvalidArgument);
                }
                let ready = self.acceptable(index).ok_or(Status::WouldBlock)?;
                let s = self.sessions[session].as_mut().ok_or(Status::BadSocket)?;
                if s.sockets >= SOCKETS_PER_SESSION {
                    return Err(Status::QuotaExceeded);
                }
                let id = s.next_socket;
                s.next_socket += 1;
                s.sockets += 1;
                let slot = &mut self.slots[ready];
                slot.listener = None;
                slot.owner = Some((session, id));
                slot.state = Tcp::Stream;
                let peer = self
                    .sockets
                    .get::<tcp::Socket>(slot.tcp)
                    .remote_endpoint()
                    .map(addr_of)
                    .unwrap_or(Addr {
                        ip: [0; 4],
                        port: 0,
                    });
                self.stats.connections_accepted += 1;
                // Keep the backlog full; if no slot is free the listener runs one short.
                let _ = self.arm_backlog(index, session);
                Ok(Answer::payload(id, id as u64, peer.encode().to_vec()))
            }
            Body::Send(data) => {
                let slot = &self.slots[index];
                match slot.state {
                    Tcp::Stream => {}
                    Tcp::Connecting => return Err(Status::WouldBlock),
                    _ => return Err(Status::NotConnected),
                }
                let tcp = self.sockets.get_mut::<tcp::Socket>(handle);
                if tcp.state() == tcp::State::Closed {
                    return Err(Status::ConnReset);
                }
                if !tcp.may_send() {
                    return Err(Status::ShutDown);
                }
                match tcp.send_slice(data) {
                    Ok(0) if !data.is_empty() => Err(Status::WouldBlock),
                    Ok(sent) => {
                        self.stats.stream_bytes_sent += sent as u64;
                        Ok(Answer::ok(socket, sent as u64))
                    }
                    Err(_) => Err(Status::ConnReset),
                }
            }
            Body::Recv { max } => {
                let slot = &self.slots[index];
                match slot.state {
                    Tcp::Stream => {}
                    Tcp::Connecting => return Err(Status::WouldBlock),
                    _ => return Err(Status::NotConnected),
                }
                if slot.read_shut {
                    return Ok(Answer::ok(socket, 0));
                }
                let finished = slot.peer_finished;
                let tcp = self.sockets.get_mut::<tcp::Socket>(handle);
                if tcp.can_recv() {
                    let mut data = vec![0u8; max as usize];
                    let n = tcp.recv_slice(&mut data).map_err(|_| Status::ConnReset)?;
                    data.truncate(n);
                    self.stats.stream_bytes_received += n as u64;
                    return Ok(Answer::payload(socket, n as u64, data));
                }
                if tcp.may_recv() {
                    return Err(Status::WouldBlock);
                }
                // No more data will come: an orderly end (the peer's FIN) reads as zero bytes; a
                // connection that dropped straight to Closed without one was reset.
                if tcp.state() == tcp::State::Closed && !finished {
                    return Err(Status::ConnReset);
                }
                Ok(Answer::ok(socket, 0))
            }
            Body::Shutdown { read, write } => {
                if self.slots[index].state != Tcp::Stream {
                    return Err(Status::NotConnected);
                }
                if read {
                    self.slots[index].read_shut = true;
                }
                if write {
                    self.sockets.get_mut::<tcp::Socket>(handle).close();
                }
                Ok(Answer::ok(socket, 0))
            }
            Body::SendTo(..) | Body::RecvFrom { .. } => Err(Status::InvalidArgument),
            _ => self.dispatch_common(session, index, request, now_ns),
        }
    }

    /// Drive smoltcp, retire drained TCP slots and settle deferred WAITs. Call after handling
    /// messages and on timer wakes.
    pub fn poll(&mut self, now_ns: u64, out: &mut Vec<Output>) {
        for _ in 0..POLLS_PER_TURN {
            let result = self
                .iface
                .poll(instant(now_ns), &mut *self.device, &mut self.sockets);
            if result == PollResult::None {
                break;
            }
        }
        for index in 0..self.slots.len() {
            let Some(deadline) = self.slots[index].draining else {
                continue;
            };
            let handle = self.slots[index].tcp;
            let state = self.sockets.get::<tcp::Socket>(handle).state();
            let done = matches!(state, tcp::State::Closed | tcp::State::TimeWait);
            if done || now_ns >= deadline {
                // The FIN exchange (or the RST abort queued) has gone out; reset and reuse.
                self.sockets.get_mut::<tcp::Socket>(handle).abort();
                if !done {
                    // A deadline reset still owes the peer its RST: one more poll.
                    self.slots[index].draining = Some(0);
                    continue;
                }
                self.forget(index);
            }
        }
        for index in 0..self.slots.len() {
            let (Some(wait), Some((session, socket))) =
                (self.slots[index].wait, self.slots[index].owner)
            else {
                continue;
            };
            let (status, ready) = match self.readiness(index) {
                Err(status) => (status, 0),
                Ok(mask) if mask & wait.interest != 0 => (Status::Ok, mask & wait.interest),
                Ok(_) if now_ns >= wait.deadline_ns => (Status::TimedOut, 0),
                Ok(_) => continue,
            };
            self.slots[index].wait = None;
            self.push_reply(
                session,
                Op::Wait as u16,
                wait.request_id,
                socket,
                status,
                ready as u64,
                &[],
                out,
            );
            self.answered(session);
        }
    }

    /// Nanoseconds until the next deferred WAIT deadline, drain deadline or smoltcp timer.
    pub fn next_wake_ns(&mut self, now_ns: u64) -> Option<u64> {
        let slots = self
            .slots
            .iter()
            .flat_map(|slot| [slot.wait.map(|w| w.deadline_ns), slot.draining])
            .flatten()
            .map(|deadline| deadline.saturating_sub(now_ns))
            .min();
        let stack = self
            .iface
            .poll_delay(instant(now_ns), &self.sockets)
            .map(|delay| delay.total_micros().saturating_mul(1000));
        match (slots, stack) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

enum Answer {
    Now {
        status: Status,
        socket: u32,
        result: u64,
        payload: Vec<u8>,
    },
    Deferred,
}

impl Answer {
    fn ok(socket: u32, result: u64) -> Self {
        Self::payload(socket, result, Vec::new())
    }

    fn payload(socket: u32, result: u64, payload: Vec<u8>) -> Self {
        Self::Now {
            status: Status::Ok,
            socket,
            result,
            payload,
        }
    }

    fn status(status: Status, socket: u32) -> Self {
        Self::Now {
            status,
            socket,
            result: 0,
            payload: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests;
