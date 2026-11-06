#![no_std]
//j815

//! netstack's server core (PLAN/020 NET2b, DESIGN/026): sessions over the KNS1/KNR1 socket
//! contract, UDP sockets on smoltcp, one loopback interface. No syscalls live here, so the whole
//! request path runs under `cargo test -p netstack`; `main.rs` is the Port loop around it.
//!
//! Every socket and buffer is allocated once in [`Server::new`]; nothing grows afterwards. A
//! socket slot is reused by closing its smoltcp socket, which clears its endpoint and buffers.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use kumo_netstack::budget::{
    SESSIONS, SOCKETS_PER_SESSION, SOCKETS_TOTAL, SOCKET_BUFFER, STREAM_PAYLOAD_MAX, UDP_METADATA,
    UDP_PAYLOAD_MAX,
};
use kumo_netstack::smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use kumo_netstack::smoltcp::socket::udp;
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

struct Slot {
    handle: SocketHandle,
    /// `(session index, socket id)` while a client holds it.
    owner: Option<(usize, u32)>,
    port: u16,
    peer: Option<Addr>,
    wait: Option<PendingWait>,
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
        let storage: &'static mut [SocketStorage<'static>] =
            Box::leak((0..SOCKETS_TOTAL).map(|_| SocketStorage::EMPTY).collect());
        let mut sockets = SocketSet::new(storage);
        let mut slots = Vec::with_capacity(SOCKETS_TOTAL);
        for _ in 0..SOCKETS_TOTAL {
            let socket = udp::Socket::new(
                udp::PacketBuffer::new(
                    leak(udp::PacketMetadata::EMPTY, UDP_METADATA),
                    leak(0u8, SOCKET_BUFFER),
                ),
                udp::PacketBuffer::new(
                    leak(udp::PacketMetadata::EMPTY, UDP_METADATA),
                    leak(0u8, SOCKET_BUFFER),
                ),
            );
            slots.push(Slot {
                handle: sockets.add(socket),
                owner: None,
                port: 0,
                peer: None,
                wait: None,
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

    pub fn live_sockets(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.owner.is_some())
            .count()
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

    /// Free a session and every socket it held; returns how many sockets were freed.
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
            if matches!(self.slots[index].owner, Some((owner, _)) if owner == session) {
                self.release(index);
                freed += 1;
            }
        }
        self.stats.sessions_closed += 1;
        self.stats.sockets_freed += freed as u64;
        freed
    }

    fn release(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        slot.owner = None;
        slot.port = 0;
        slot.peer = None;
        slot.wait = None;
        self.sockets.get_mut::<udp::Socket>(slot.handle).close();
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

    fn port_in_use(&self, port: u16) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.owner.is_some() && slot.port == port)
    }

    fn ephemeral(&mut self) -> Option<u16> {
        let span = (u16::MAX - EPHEMERAL_FIRST) as usize + 1;
        for _ in 0..span {
            let port = self.next_ephemeral;
            self.next_ephemeral = if port == u16::MAX {
                EPHEMERAL_FIRST
            } else {
                port + 1
            };
            if !self.port_in_use(port) {
                return Some(port);
            }
        }
        None
    }

    fn bind_slot(&mut self, index: usize, port: u16) -> Result<(), Status> {
        let port = match port {
            0 => self.ephemeral().ok_or(Status::AddrInUse)?,
            port if self.port_in_use(port) => return Err(Status::AddrInUse),
            port => port,
        };
        let handle = self.slots[index].handle;
        let endpoint = IpListenEndpoint { addr: None, port };
        self.sockets
            .get_mut::<udp::Socket>(handle)
            .bind(endpoint)
            .map_err(|_| Status::InvalidArgument)?;
        self.slots[index].port = port;
        Ok(())
    }

    fn ensure_bound(&mut self, index: usize) -> Result<(), Status> {
        if self.slots[index].port == 0 {
            self.bind_slot(index, 0)?;
        }
        Ok(())
    }

    fn send(&mut self, index: usize, to: Addr, data: &[u8]) -> Result<u64, Status> {
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
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::from(to.ip)), to.port);
        match self
            .sockets
            .get_mut::<udp::Socket>(self.slots[index].handle)
            .send_slice(data, endpoint)
        {
            Ok(()) => {
                self.stats.datagrams_sent += 1;
                Ok(data.len() as u64)
            }
            Err(udp::SendError::BufferFull) => Err(Status::WouldBlock),
            Err(_) => Err(Status::InvalidArgument),
        }
    }

    fn ready_mask(&self, index: usize) -> u8 {
        let slot = &self.slots[index];
        let socket = self.sockets.get::<udp::Socket>(slot.handle);
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
        mask
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
        match request.body {
            Body::Hello => Ok(Answer::ok(0, hello_limits())),
            Body::Socket(SocketKind::Tcp) => Err(Status::Unsupported),
            Body::Socket(SocketKind::Udp) => {
                let held = self.sessions[session].as_ref().map_or(0, |s| s.sockets);
                let free = self.slots.iter().position(|slot| slot.owner.is_none());
                let (Some(index), true) = (free, held < SOCKETS_PER_SESSION) else {
                    return Err(Status::QuotaExceeded);
                };
                let s = self.sessions[session].as_mut().ok_or(Status::BadSocket)?;
                let id = s.next_socket;
                s.next_socket += 1;
                s.sockets += 1;
                self.slots[index].owner = Some((session, id));
                Ok(Answer::ok(id, id as u64))
            }
            Body::Bind(addr) => {
                let index = self.slot_of(session, socket)?;
                if self.slots[index].port != 0 {
                    return Err(Status::InvalidArgument);
                }
                if addr.ip != LOOPBACK && addr.ip != [0; 4] {
                    return Err(Status::AddrNotAvailable);
                }
                self.bind_slot(index, addr.port)?;
                Ok(Answer::ok(socket, self.slots[index].port as u64))
            }
            Body::Connect(addr) => {
                let index = self.slot_of(session, socket)?;
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
                let index = self.slot_of(session, socket)?;
                let peer = self.slots[index].peer.ok_or(Status::NotConnected)?;
                Ok(Answer::ok(socket, self.send(index, peer, data)?))
            }
            Body::SendTo(to, data) => {
                let index = self.slot_of(session, socket)?;
                Ok(Answer::ok(socket, self.send(index, to, data)?))
            }
            Body::Recv { max } | Body::RecvFrom { max } => {
                let index = self.slot_of(session, socket)?;
                let with_addr = matches!(request.body, Body::RecvFrom { .. });
                let handle = self.slots[index].handle;
                let udp = self.sockets.get_mut::<udp::Socket>(handle);
                let (data, meta) = udp.recv().map_err(|_| Status::WouldBlock)?;
                let take = data.len().min(max as usize);
                let mut payload = Vec::with_capacity(ADDR_LEN + take);
                if with_addr {
                    let ip = match meta.endpoint.addr {
                        IpAddress::Ipv4(ip) => ip.octets(),
                    };
                    payload.extend_from_slice(
                        &Addr {
                            ip,
                            port: meta.endpoint.port,
                        }
                        .encode(),
                    );
                }
                payload.extend_from_slice(&data[..take]);
                let mut result = data.len() as u64;
                if data.len() > take {
                    result |= RESULT_TRUNCATED;
                }
                self.stats.datagrams_received += 1;
                Ok(Answer::Now {
                    status: Status::Ok,
                    socket,
                    result,
                    payload,
                })
            }
            Body::Listen { .. } | Body::Accept | Body::Shutdown { .. } => {
                self.slot_of(session, socket)?;
                Err(Status::Unsupported)
            }
            Body::Close => {
                let index = self.slot_of(session, socket)?;
                self.release(index);
                if let Some(s) = self.sessions[session].as_mut() {
                    s.sockets -= 1;
                }
                self.stats.sockets_freed += 1;
                Ok(Answer::ok(socket, 0))
            }
            Body::Wait {
                interest,
                timeout_ms,
            } => {
                let index = self.slot_of(session, socket)?;
                if self.slots[index].wait.is_some() {
                    return Err(Status::InvalidArgument);
                }
                let ready = self.ready_mask(index) & interest;
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
                let index = self.slot_of(session, socket)?;
                let slot = &self.slots[index];
                let local = Addr {
                    ip: LOOPBACK,
                    port: slot.port,
                };
                let peer = slot.peer.unwrap_or(Addr {
                    ip: [0; 4],
                    port: 0,
                });
                let mut payload = Vec::with_capacity(2 * ADDR_LEN);
                payload.extend_from_slice(&local.encode());
                payload.extend_from_slice(&peer.encode());
                Ok(Answer::Now {
                    status: Status::Ok,
                    socket,
                    result: 0,
                    payload,
                })
            }
        }
    }

    /// Drive smoltcp and settle deferred WAITs. Call after handling messages and on timer wakes.
    pub fn poll(&mut self, now_ns: u64, out: &mut Vec<Output>) {
        for _ in 0..POLLS_PER_TURN {
            let result = self
                .iface
                .poll(instant(now_ns), &mut *self.device, &mut self.sockets);
            if result == kumo_netstack::smoltcp::iface::PollResult::None {
                break;
            }
        }
        for index in 0..self.slots.len() {
            let (Some(wait), Some((session, socket))) =
                (self.slots[index].wait, self.slots[index].owner)
            else {
                continue;
            };
            let ready = self.ready_mask(index) & wait.interest;
            let status = if ready != 0 {
                Status::Ok
            } else if now_ns >= wait.deadline_ns {
                Status::TimedOut
            } else {
                continue;
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

    /// Nanoseconds until the next deferred WAIT deadline or smoltcp timer, if any.
    pub fn next_wake_ns(&mut self, now_ns: u64) -> Option<u64> {
        let waits = self
            .slots
            .iter()
            .filter_map(|slot| slot.wait.map(|w| w.deadline_ns.saturating_sub(now_ns)))
            .min();
        let stack = self
            .iface
            .poll_delay(instant(now_ns), &self.sockets)
            .map(|delay| delay.total_micros().saturating_mul(1000));
        match (waits, stack) {
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
        Self::Now {
            status: Status::Ok,
            socket,
            result,
            payload: Vec::new(),
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
