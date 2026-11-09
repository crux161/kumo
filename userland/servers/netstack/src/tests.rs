//j820
//j843
//j844
//j846
//j848

extern crate std;

use std::vec::Vec;

use kumo_netstack::budget::{SESSIONS, SOCKETS_PER_SESSION, SOCKETS_TOTAL, UDP_PAYLOAD_MAX};
use kumo_netstack::socket::{
    decode_reply, encode_request, Addr, Op, RequestHeader, Status, ADDR_LEN, HEADER_LEN,
    RESULT_TRUNCATED, WAIT_READABLE,
};

use super::*;

/// A test client speaking the real codec to one session of a shared server.
struct Client {
    session: usize,
    generation: u32,
    next_id: u32,
}

struct Answer {
    generation: u32,
    status: Status,
    socket: u32,
    result: u64,
    payload: Vec<u8>,
}

const MS: u64 = 1_000_000;

fn header(op: Op, socket: u32, generation: u32, arg: u64, id: u32) -> RequestHeader {
    RequestHeader {
        op,
        request_id: id,
        socket,
        generation,
        flags: 0,
        arg,
    }
}

impl Client {
    fn open(server: &mut Server) -> Self {
        let session = server.open_session().expect("session slot");
        let mut client = Self {
            session,
            generation: 0,
            next_id: 1,
        };
        let hello = client.call(server, Op::Hello, 0, 0, &[]);
        assert_eq!(hello.status, Status::Ok);
        assert_eq!(hello.result, hello_limits());
        client
    }

    /// Send one request and return its single immediate reply.
    fn call(
        &mut self,
        server: &mut Server,
        op: Op,
        socket: u32,
        arg: u64,
        payload: &[u8],
    ) -> Answer {
        let mut out = self.send(server, op, socket, arg, payload, 0);
        assert_eq!(out.len(), 1, "{op:?} answers immediately");
        let answer = Self::read(&out.remove(0));
        if op == Op::Hello {
            self.generation = answer.generation;
        }
        answer
    }

    fn send(
        &mut self,
        server: &mut Server,
        op: Op,
        socket: u32,
        arg: u64,
        payload: &[u8],
        now_ns: u64,
    ) -> Vec<Output> {
        let mut bytes = [0u8; 4096];
        let id = self.next_id;
        self.next_id += 1;
        let len = encode_request(
            &header(op, socket, self.generation, arg, id),
            payload,
            &mut bytes,
        )
        .expect("valid request");
        let mut out = Vec::new();
        assert_eq!(
            server.handle(self.session, &bytes[..len], 0, now_ns, &mut out),
            After::Keep
        );
        server.poll(now_ns, &mut out);
        for output in &out {
            assert_eq!(output.session, self.session);
            let reply = decode_reply(&output.bytes, 0).unwrap();
            assert_eq!(
                reply.generation,
                if op == Op::Hello {
                    reply.generation
                } else {
                    self.generation
                }
            );
        }
        out
    }

    fn read(output: &Output) -> Answer {
        let reply = decode_reply(&output.bytes, 0).unwrap();
        Answer {
            generation: reply.generation,
            status: reply.status,
            socket: reply.socket,
            result: reply.result,
            payload: reply.payload.to_vec(),
        }
    }

    fn udp(&mut self, server: &mut Server) -> u32 {
        let answer = self.call(server, Op::Socket, 0, 1, &[]);
        assert_eq!(answer.status, Status::Ok);
        answer.socket
    }

    fn bind(&mut self, server: &mut Server, socket: u32, port: u16) -> u16 {
        let addr = Addr { ip: LOOPBACK, port };
        let answer = self.call(server, Op::Bind, socket, 0, &addr.encode());
        assert_eq!(answer.status, Status::Ok);
        answer.result as u16
    }

    fn sendto(&mut self, server: &mut Server, socket: u32, port: u16, data: &[u8]) -> Answer {
        self.sendto_to(server, socket, Addr { ip: LOOPBACK, port }, data)
    }

    fn sendto_to(&mut self, server: &mut Server, socket: u32, to: Addr, data: &[u8]) -> Answer {
        let mut payload = to.encode().to_vec();
        payload.extend_from_slice(data);
        self.call(server, Op::SendTo, socket, 0, &payload)
    }

    fn recvfrom(&mut self, server: &mut Server, socket: u32, max: u16) -> Answer {
        self.call(server, Op::RecvFrom, socket, max as u64, &[])
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[test]
fn the_zeroed_heap_ring_is_the_empty_ring() {
    let ring = boxed_loopback();
    let fresh: kumo_netstack::Loopback = kumo_netstack::Loopback::new();
    assert_eq!(ring.pending(), fresh.pending());
    assert_eq!(ring.counters(), fresh.counters());
}

#[test]
fn two_sessions_exchange_exact_datagrams_over_loopback() {
    let mut server = Server::new(0);
    let mut a = Client::open(&mut server);
    let mut b = Client::open(&mut server);
    assert_ne!(
        a.generation, b.generation,
        "each session has its own generation"
    );

    let sa = a.udp(&mut server);
    let sb = b.udp(&mut server);
    let port_a = a.bind(&mut server, sa, 7000);
    let port_b = b.bind(&mut server, sb, 0);
    assert_eq!(port_a, 7000);
    assert!(port_b >= 49152, "port 0 picks an ephemeral port");

    for data in [
        Vec::new(),
        b"hello from b".to_vec(),
        pattern(UDP_PAYLOAD_MAX, 7),
    ] {
        let sent = b.sendto(&mut server, sb, port_a, &data);
        assert_eq!((sent.status, sent.result), (Status::Ok, data.len() as u64));
        let got = a.recvfrom(&mut server, sa, UDP_PAYLOAD_MAX as u16);
        assert_eq!(got.status, Status::Ok);
        let from = &got.payload[..ADDR_LEN];
        assert_eq!(
            from,
            Addr {
                ip: LOOPBACK,
                port: port_b
            }
            .encode()
        );
        assert_eq!(&got.payload[ADDR_LEN..], data.as_slice());
        assert_eq!(got.result, data.len() as u64);
    }
    // And back the other way.
    a.sendto(&mut server, sa, port_b, b"reply from a");
    let back = b.recvfrom(&mut server, sb, 64);
    assert_eq!(&back.payload[ADDR_LEN..], b"reply from a");
    assert_eq!(server.stats().datagrams_received, 4);
}

#[test]
fn recvfrom_truncates_and_flags_a_long_datagram() {
    let mut server = Server::new(0);
    let mut c = Client::open(&mut server);
    let (s1, s2) = (c.udp(&mut server), c.udp(&mut server));
    let p1 = c.bind(&mut server, s1, 0);
    c.sendto(&mut server, s2, p1, b"0123456789");
    let got = c.recvfrom(&mut server, s1, 4);
    assert_eq!(&got.payload[ADDR_LEN..], b"0123");
    assert_eq!(got.result, 10 | RESULT_TRUNCATED);
}

#[test]
fn errors_fail_deterministically() {
    let mut server = Server::new(0);
    let mut c = Client::open(&mut server);
    let s = c.udp(&mut server);
    let port = c.bind(&mut server, s, 0);

    assert_eq!(c.recvfrom(&mut server, s, 64).status, Status::WouldBlock);
    assert_eq!(
        c.call(&mut server, Op::Close, 99, 0, &[]).status,
        Status::BadSocket
    );
    // TCP is served since NET2c (J817); this session's TCP socket is simply created.
    assert_eq!(
        c.call(&mut server, Op::Socket, 0, 2, &[]).status,
        Status::Ok
    );
    assert_eq!(
        c.call(&mut server, Op::Send, s, 0, b"x").status,
        Status::NotConnected
    );
    assert_eq!(
        c.call(&mut server, Op::Listen, s, 1, &[]).status,
        Status::Unsupported
    );
    let elsewhere = Addr {
        ip: [10, 0, 0, 1],
        port: 9,
    };
    assert_eq!(
        c.call(&mut server, Op::Connect, s, 0, &elsewhere.encode())
            .status,
        Status::NetUnreachable
    );
    let s2 = c.udp(&mut server);
    let taken = Addr { ip: LOOPBACK, port };
    assert_eq!(
        c.call(&mut server, Op::Bind, s2, 0, &taken.encode()).status,
        Status::AddrInUse
    );
    let foreign = Addr {
        ip: [192, 168, 0, 9],
        port: 0,
    };
    assert_eq!(
        c.call(&mut server, Op::Bind, s2, 0, &foreign.encode())
            .status,
        Status::AddrNotAvailable
    );

    // The codec refuses an oversize SENDTO on the client side; send raw bytes to prove netstack
    // answers MessageTooLarge too.
    let mut raw = std::vec![0u8; HEADER_LEN + ADDR_LEN + UDP_PAYLOAD_MAX + 1];
    let len = encode_request(
        &header(Op::SendTo, s, c.generation, 0, 100),
        &Addr { ip: LOOPBACK, port }.encode(),
        &mut raw,
    )
    .unwrap();
    raw.truncate(len);
    raw.extend_from_slice(&std::vec![0u8; UDP_PAYLOAD_MAX + 1]);
    let total = (raw.len() - HEADER_LEN) as u16;
    raw[22..24].copy_from_slice(&total.to_le_bytes());
    let mut out = Vec::new();
    server.handle(c.session, &raw, 0, 0, &mut out);
    assert_eq!(Client::read(&out[0]).status, Status::MessageTooLarge);
}

#[test]
fn quotas_hold_per_session_and_in_total_and_close_frees_them() {
    let mut server = Server::new(0);
    let mut first = Client::open(&mut server);
    for _ in 0..SOCKETS_PER_SESSION {
        first.udp(&mut server);
    }
    assert_eq!(
        first.call(&mut server, Op::Socket, 0, 1, &[]).status,
        Status::QuotaExceeded
    );
    let mut second = Client::open(&mut server);
    for _ in 0..SOCKETS_TOTAL - SOCKETS_PER_SESSION {
        second.udp(&mut server);
    }
    let mut third = Client::open(&mut server);
    assert_eq!(
        third.call(&mut server, Op::Socket, 0, 1, &[]).status,
        Status::QuotaExceeded,
        "total cap"
    );

    assert_eq!(server.close_session(first.session), SOCKETS_PER_SESSION);
    assert_eq!(server.live_sockets(), SOCKETS_TOTAL - SOCKETS_PER_SESSION);
    third.udp(&mut server);

    // Sessions themselves are capped too.
    let mut extra = Vec::new();
    while let Some(index) = server.open_session() {
        extra.push(index);
    }
    assert_eq!(server.live_sessions(), SESSIONS);
    for index in extra {
        server.close_session(index);
    }
}

#[test]
fn a_closed_port_is_rebindable_and_its_datagrams_are_gone() {
    let mut server = Server::new(0);
    let mut c = Client::open(&mut server);
    let (rx, tx) = (c.udp(&mut server), c.udp(&mut server));
    c.bind(&mut server, rx, 7100);
    c.sendto(&mut server, tx, 7100, b"stale");
    assert_eq!(
        c.call(&mut server, Op::Close, rx, 0, &[]).status,
        Status::Ok
    );
    let again = c.udp(&mut server);
    assert_ne!(again, rx, "socket ids are never reused in a session");
    c.bind(&mut server, again, 7100);
    assert_eq!(
        c.recvfrom(&mut server, again, 64).status,
        Status::WouldBlock
    );
}

#[test]
fn wait_defers_until_ready_or_deadline() {
    let mut server = Server::new(0);
    let mut c = Client::open(&mut server);
    let (rx, tx) = (c.udp(&mut server), c.udp(&mut server));
    let port = c.bind(&mut server, rx, 0);

    // Nothing queued: the WAIT is held, then times out at its deadline.
    let wait = WAIT_READABLE as u64 | (50u64 << 32);
    assert!(c.send(&mut server, Op::Wait, rx, wait, &[], 0).is_empty());
    assert_eq!(server.next_wake_ns(0), Some(50 * MS));
    let mut out = Vec::new();
    server.poll(49 * MS, &mut out);
    assert!(out.is_empty());
    server.poll(50 * MS, &mut out);
    assert_eq!(Client::read(&out[0]).status, Status::TimedOut);

    // Held again, then released by a datagram before its deadline.
    assert!(c
        .send(&mut server, Op::Wait, rx, wait, &[], 60 * MS)
        .is_empty());
    let replies = {
        let mut payload = Addr { ip: LOOPBACK, port }.encode().to_vec();
        payload.extend_from_slice(b"wake");
        c.send(&mut server, Op::SendTo, tx, 0, &payload, 61 * MS)
    };
    let statuses: Vec<_> = replies.iter().map(|o| Client::read(o).status).collect();
    assert_eq!(
        statuses,
        [Status::Ok, Status::Ok],
        "SENDTO reply, then the released WAIT"
    );
    assert_eq!(Client::read(&replies[1]).result, WAIT_READABLE as u64);

    // A WAIT parked on a socket being closed is answered ShutDown before the close.
    assert!(c
        .send(
            &mut server,
            Op::Wait,
            tx,
            WAIT_READABLE as u64 | (1000u64 << 32),
            &[],
            70 * MS
        )
        .is_empty());
    let closing = c.send(&mut server, Op::Close, tx, 0, &[], 71 * MS);
    let statuses: Vec<_> = closing.iter().map(|o| Client::read(o).status).collect();
    assert_eq!(statuses, [Status::ShutDown, Status::Ok]);
}

#[test]
fn untrusted_and_violating_requests_close_the_session() {
    let mut server = Server::new(0);
    let mut c = Client::open(&mut server);
    let mut out = Vec::new();
    assert_eq!(
        server.handle(c.session, b"KNX1 not a request", 0, 0, &mut out),
        After::Close
    );
    assert!(out.is_empty(), "an untrusted header gets no reply");

    let mut replayed = [0u8; 64];
    let len = encode_request(&header(Op::Hello, 0, 0, 0, 1), &[], &mut replayed).unwrap();
    assert_eq!(
        server.handle(c.session, &replayed[..len], 0, 0, &mut out),
        After::Close
    );
    assert_eq!(Client::read(&out[0]).status, Status::ProtocolViolation);

    assert_eq!(server.close_session(c.session), 0);
    c.next_id = 1;
    let mut dead = Vec::new();
    assert_eq!(
        server.handle(c.session, &replayed[..len], 0, 0, &mut dead),
        After::Close,
        "closed session"
    );
}

// ---- TCP (NET2c, J817) ----------------------------------------------------------------------

use kumo_netstack::socket::{WAIT_CONNECTED, WAIT_WRITABLE};

/// A test clock: TCP needs time to move (ACK delay, FIN exchange, deadlines). Each test thread
/// has its own, so parallel tests never share time.
mod clock {
    use super::std::cell::Cell;
    super::std::thread_local!(static NOW: Cell<u64> = const { Cell::new(1_000_000) });
    pub fn now() -> u64 {
        NOW.with(Cell::get)
    }
    pub fn advance(ns: u64) {
        NOW.with(|now| now.set(now.get() + ns));
    }
}

/// Run the stack for `ms` milliseconds in 1 ms steps; returns any deferred replies.
fn run(server: &mut Server, ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    for _ in 0..ms {
        clock::advance(MS);
        server.poll(clock::now(), &mut out);
    }
    out
}

impl Client {
    /// `call`, at the test clock's current time.
    fn at(&mut self, server: &mut Server, op: Op, socket: u32, arg: u64, payload: &[u8]) -> Answer {
        let mut out = self.send(server, op, socket, arg, payload, clock::now());
        assert_eq!(out.len(), 1, "{op:?} answers immediately");
        Self::read(&out.remove(0))
    }

    fn tcp(&mut self, server: &mut Server) -> u32 {
        let answer = self.at(server, Op::Socket, 0, 2, &[]);
        assert_eq!(answer.status, Status::Ok);
        answer.socket
    }

    fn tcp_listener(&mut self, server: &mut Server, port: u16, backlog: u64) -> u32 {
        let l = self.tcp(server);
        let bind = self.at(
            server,
            Op::Bind,
            l,
            0,
            &Addr { ip: LOOPBACK, port }.encode(),
        );
        assert_eq!(bind.status, Status::Ok);
        assert_eq!(
            self.at(server, Op::Listen, l, backlog, &[]).status,
            Status::Ok
        );
        l
    }

    fn connect(&mut self, server: &mut Server, socket: u32, port: u16) -> Status {
        self.at(
            server,
            Op::Connect,
            socket,
            0,
            &Addr { ip: LOOPBACK, port }.encode(),
        )
        .status
    }

    fn recv_tcp(&mut self, server: &mut Server, socket: u32, max: u64) -> Answer {
        self.at(server, Op::Recv, socket, max, &[])
    }
}

/// Listener on `port` in `a`, a connected stream from `b`, and the accepted id in `a`.
fn connected(server: &mut Server, a: &mut Client, b: &mut Client, port: u16) -> (u32, u32, u32) {
    let l = a.tcp_listener(server, port, 2);
    let c = b.tcp(server);
    assert_eq!(b.connect(server, c, port), Status::InProgress);
    run(server, 20);
    let ready = b.at(server, Op::Wait, c, WAIT_CONNECTED as u64, &[]);
    assert_eq!(ready.status, Status::Ok);
    assert_ne!(ready.result & WAIT_CONNECTED as u64, 0);
    let accepted = a.at(server, Op::Accept, l, 0, &[]);
    assert_eq!(accepted.status, Status::Ok);
    assert_eq!(accepted.socket as u64, accepted.result);
    // The accepted connection's peer is the connecting socket's local port.
    let names = b.at(server, Op::GetName, c, 0, &[]);
    assert_eq!(accepted.payload, names.payload[..ADDR_LEN]);
    (l, c, accepted.socket)
}

#[test]
fn tcp_connect_accept_and_exact_stream_across_partial_writes() {
    let mut server = Server::new(clock::now());
    let mut a = Client::open(&mut server);
    let mut b = Client::open(&mut server);
    let (_, c, s) = connected(&mut server, &mut a, &mut b, 8000);

    let data = pattern(20_000, 3);
    let (mut sent, mut received) = (0usize, Vec::new());
    let (mut partial, mut would_block) = (false, false);
    for _ in 0..10_000 {
        if sent < data.len() {
            let chunk = (data.len() - sent).min(STREAM_PAYLOAD_MAX);
            let answer = b.at(&mut server, Op::Send, c, 0, &data[sent..sent + chunk]);
            match answer.status {
                Status::Ok => {
                    partial |= (answer.result as usize) < chunk;
                    sent += answer.result as usize;
                }
                Status::WouldBlock => would_block = true,
                other => panic!("send -> {other:?}"),
            }
        }
        run(&mut server, 2);
        // A slower reader than writer, so the buffers fill and writes go partial.
        let got = b_recv(&mut a, &mut server, s, 700);
        received.extend_from_slice(&got);
        if received.len() == data.len() {
            break;
        }
    }
    assert_eq!(received, data, "stream read back exact");
    // Backpressure shows here as partially accepted SENDs; a fully refused SEND (WouldBlock) is
    // proven against an unread stream in the refusal test.
    assert!(partial, "partial writes were exercised");
    let _ = would_block;
    assert_eq!(server.stats().stream_bytes_received, data.len() as u64);
}

fn b_recv(client: &mut Client, server: &mut Server, socket: u32, max: u64) -> Vec<u8> {
    let answer = client.recv_tcp(server, socket, max);
    match answer.status {
        Status::Ok => answer.payload,
        Status::WouldBlock => Vec::new(),
        other => panic!("recv -> {other:?}"),
    }
}

#[test]
fn tcp_half_close_eof_and_orderly_close() {
    let mut server = Server::new(clock::now());
    let mut a = Client::open(&mut server);
    let mut b = Client::open(&mut server);
    let (_, c, s) = connected(&mut server, &mut a, &mut b, 8001);

    assert_eq!(
        b.at(&mut server, Op::Send, c, 0, b"last words").status,
        Status::Ok
    );
    assert_eq!(
        b.at(&mut server, Op::Shutdown, c, 2, &[]).status,
        Status::Ok
    );
    assert_eq!(
        b.at(&mut server, Op::Send, c, 0, b"too late").status,
        Status::ShutDown
    );
    run(&mut server, 30);
    assert_eq!(b_recv(&mut a, &mut server, s, 64), b"last words");
    let eof = a.recv_tcp(&mut server, s, 64);
    assert_eq!(
        (eof.status, eof.payload.len()),
        (Status::Ok, 0),
        "peer FIN reads as EOF"
    );

    // Half-closed: the other direction still carries data.
    assert_eq!(
        a.at(&mut server, Op::Send, s, 0, b"still open").status,
        Status::Ok
    );
    run(&mut server, 30);
    assert_eq!(b_recv(&mut b, &mut server, c, 64), b"still open");

    assert_eq!(a.at(&mut server, Op::Close, s, 0, &[]).status, Status::Ok);
    run(&mut server, 30);
    let end = b.recv_tcp(&mut server, c, 64);
    assert_eq!((end.status, end.payload.len()), (Status::Ok, 0));
    assert_eq!(b.at(&mut server, Op::Close, c, 0, &[]).status, Status::Ok);
    run(&mut server, 100);
    // Only the listener and its two backlog slots remain busy.
    assert_eq!(server.busy_slots(), 3);
}

#[test]
fn tcp_refusal_backlog_would_block_and_deadlines() {
    let mut server = Server::new(clock::now());
    let mut a = Client::open(&mut server);
    let mut b = Client::open(&mut server);

    // No listener: refused.
    let c = b.tcp(&mut server);
    assert_eq!(b.connect(&mut server, c, 8100), Status::InProgress);
    run(&mut server, 20);
    let refused = b.at(
        &mut server,
        Op::Wait,
        c,
        WAIT_CONNECTED as u64 | (1000u64 << 32),
        &[],
    );
    assert_eq!(refused.status, Status::ConnRefused);

    // Backlog 1: one connection waits for ACCEPT, the next is refused.
    let l = a.tcp_listener(&mut server, 8101, 1);
    assert_eq!(
        a.at(&mut server, Op::Accept, l, 0, &[]).status,
        Status::WouldBlock
    );
    let (first, second) = (b.tcp(&mut server), b.tcp(&mut server));
    assert_eq!(b.connect(&mut server, first, 8101), Status::InProgress);
    run(&mut server, 20);
    assert_eq!(b.connect(&mut server, second, 8101), Status::InProgress);
    run(&mut server, 20);
    let w = |client: &mut Client, server: &mut Server, s| {
        client
            .at(
                server,
                Op::Wait,
                s,
                WAIT_CONNECTED as u64 | (500u64 << 32),
                &[],
            )
            .status
    };
    assert_eq!(w(&mut b, &mut server, first), Status::Ok);
    assert_eq!(w(&mut b, &mut server, second), Status::ConnRefused);
    let s = a.at(&mut server, Op::Accept, l, 0, &[]).socket;

    // Nothing to read: would-block now, TimedOut at the WAIT deadline.
    assert_eq!(a.recv_tcp(&mut server, s, 64).status, Status::WouldBlock);
    let held = a.send(
        &mut server,
        Op::Wait,
        s,
        WAIT_READABLE as u64 | (50u64 << 32),
        &[],
        clock::now(),
    );
    assert!(held.is_empty(), "the WAIT is deferred");
    let early = run(&mut server, 49);
    assert!(early.is_empty());
    let late = run(&mut server, 2);
    assert_eq!(Client::read(&late[0]).status, Status::TimedOut);

    // Writable until the buffers fill, then would-block.
    assert_ne!(
        b.at(&mut server, Op::Wait, first, WAIT_WRITABLE as u64, &[])
            .result
            & WAIT_WRITABLE as u64,
        0
    );
    let block = [7u8; STREAM_PAYLOAD_MAX];
    let mut blocked = false;
    for _ in 0..20 {
        if b.at(&mut server, Op::Send, first, 0, &block).status == Status::WouldBlock {
            blocked = true;
            break;
        }
        run(&mut server, 5);
    }
    assert!(blocked, "an unread stream pushes back");
}

#[test]
fn tcp_peer_session_close_resets_and_every_slot_comes_back() {
    let mut server = Server::new(clock::now());
    let mut a = Client::open(&mut server);
    let mut b = Client::open(&mut server);
    let (_, c, _) = connected(&mut server, &mut a, &mut b, 8200);
    assert_eq!(
        server.close_session(a.session),
        2,
        "the listener and the accepted stream"
    );
    run(&mut server, 20);
    assert_eq!(b.recv_tcp(&mut server, c, 64).status, Status::ConnReset);
    assert_eq!(
        b.at(&mut server, Op::Send, c, 0, b"x").status,
        Status::ConnReset
    );
    assert_eq!(b.at(&mut server, Op::Close, c, 0, &[]).status, Status::Ok);
    run(&mut server, 20);
    assert_eq!(
        server.busy_slots(),
        0,
        "listener, backlog, stream and drains all freed"
    );
    assert_eq!(server.live_sockets(), 0);
}

#[test]
fn an_aborted_slot_is_not_reused_before_its_rst_leaves() {
    let mut server = Server::new(clock::now());
    let mut a = Client::open(&mut server);
    let mut b = Client::open(&mut server);
    let (_, c, _) = connected(&mut server, &mut a, &mut b, 8300);
    // Close a's session and, with NO poll in between, let a new listener arm backlog slots. If
    // the aborted slots were free at once, `listen()` would reset them and swallow their RST.
    // (netstack's loop polls after every message; this holds the invariant for any caller.)
    server.close_session(a.session);
    let d = server.open_session().unwrap();
    let now = clock::now();
    let mut out = Vec::new();
    let raw = |server: &mut Server, out: &mut Vec<Output>, h: RequestHeader, payload: &[u8]| {
        let mut bytes = [0u8; 256];
        let len = encode_request(&h, payload, &mut bytes).unwrap();
        server.handle(d, &bytes[..len], 0, now, out);
    };
    raw(&mut server, &mut out, header(Op::Hello, 0, 0, 0, 1), &[]);
    let generation = decode_reply(&out[0].bytes, 0).unwrap().generation;
    let addr = Addr {
        ip: LOOPBACK,
        port: 8301,
    }
    .encode();
    raw(
        &mut server,
        &mut out,
        header(Op::Socket, 0, generation, 2, 2),
        &[],
    );
    raw(
        &mut server,
        &mut out,
        header(Op::Bind, 1, generation, 0, 3),
        &addr,
    );
    raw(
        &mut server,
        &mut out,
        header(Op::Listen, 1, generation, 4, 4),
        &[],
    );
    let statuses: Vec<_> = out
        .iter()
        .map(|o| decode_reply(&o.bytes, 0).unwrap().status)
        .collect();
    assert_eq!(statuses, [Status::Ok; 4]);
    run(&mut server, 20);
    assert_eq!(b.recv_tcp(&mut server, c, 64).status, Status::ConnReset);
}

#[test]
fn replacement_instances_have_disjoint_generations_and_never_wrap() {
    assert!(Server::for_instance(0, 0).is_none());
    let mut first = Server::for_instance(0, 1).unwrap();
    let mut second = Server::for_instance(0, 2).unwrap();
    let a = first.open_session().unwrap();
    let b = second.open_session().unwrap();
    assert_eq!(
        first.sessions[a].as_ref().unwrap().state.generation(),
        0x10001
    );
    assert_eq!(
        second.sessions[b].as_ref().unwrap().state.generation(),
        0x20001
    );
    first.close_session(a);
    first.next_generation = first.generation_end;
    let last = first.open_session().unwrap();
    first.close_session(last);
    assert!(first.open_session().is_none());
}

// --- the wire stack: frames as the peer capture sees them ---

use kumo_netstack::budget::FRAME_MAX;
use kumo_netstack::smoltcp::phy::ChecksumCapabilities;
use kumo_netstack::smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetFrame, EthernetProtocol, Icmpv4Packet, Icmpv4Repr,
    IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr,
};

const WIRE_IP: [u8; 4] = [192, 168, 0, 50];
const WIRE_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x50];
const PEER_IP: [u8; 4] = [192, 168, 0, 106];
const PEER_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x66];
const WIRE_PORT: u16 = 9000;
const PEER_PORT: u16 = 7777;

fn ethernet_wrap(
    src: [u8; 6],
    dst: [u8; 6],
    ethertype: EthernetProtocol,
    payload: &[u8],
) -> Vec<u8> {
    let mut bytes = std::vec![0u8; 14 + payload.len()];
    let mut frame = EthernetFrame::new_unchecked(&mut bytes[..]);
    frame.set_src_addr(kumo_netstack::smoltcp::wire::EthernetAddress(src));
    frame.set_dst_addr(kumo_netstack::smoltcp::wire::EthernetAddress(dst));
    frame.set_ethertype(ethertype);
    frame.payload_mut().copy_from_slice(payload);
    bytes
}

/// The ARP reply the peer answers our request with.
fn arp_reply() -> Vec<u8> {
    let mut payload = std::vec![0u8; 28];
    let mut packet = ArpPacket::new_unchecked(&mut payload[..]);
    ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Reply,
        source_hardware_addr: kumo_netstack::smoltcp::wire::EthernetAddress(PEER_MAC),
        source_protocol_addr: Ipv4Address::from(PEER_IP),
        target_hardware_addr: kumo_netstack::smoltcp::wire::EthernetAddress(WIRE_MAC),
        target_protocol_addr: Ipv4Address::from(WIRE_IP),
    }
    .emit(&mut packet);
    ethernet_wrap(PEER_MAC, WIRE_MAC, EthernetProtocol::Arp, &payload)
}

/// The peer's ARP request for the wire address (what a host sends before its first ping).
fn arp_request() -> Vec<u8> {
    let mut payload = std::vec![0u8; 28];
    let mut packet = ArpPacket::new_unchecked(&mut payload[..]);
    ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: kumo_netstack::smoltcp::wire::EthernetAddress(PEER_MAC),
        source_protocol_addr: Ipv4Address::from(PEER_IP),
        target_hardware_addr: kumo_netstack::smoltcp::wire::EthernetAddress([0; 6]),
        target_protocol_addr: Ipv4Address::from(WIRE_IP),
    }
    .emit(&mut packet);
    ethernet_wrap(PEER_MAC, [0xFF; 6], EthernetProtocol::Arp, &payload)
}

/// An ICMP echo frame from the peer: `request` false builds the reply.
fn echo_from_peer(request: bool, ident: u16, seq_no: u16, data: &[u8]) -> Vec<u8> {
    let checksums = ChecksumCapabilities::default();
    let repr = if request {
        Icmpv4Repr::EchoRequest {
            ident,
            seq_no,
            data,
        }
    } else {
        Icmpv4Repr::EchoReply {
            ident,
            seq_no,
            data,
        }
    };
    let mut icmp = std::vec![0u8; repr.buffer_len()];
    {
        let mut packet = Icmpv4Packet::new_unchecked(&mut icmp[..]);
        repr.emit(&mut packet, &checksums);
    }
    let ip = Ipv4Repr {
        src_addr: Ipv4Address::from(PEER_IP),
        dst_addr: Ipv4Address::from(WIRE_IP),
        next_header: IpProtocol::Icmp,
        payload_len: icmp.len(),
        hop_limit: 64,
    };
    let mut bytes = std::vec![0u8; ip.buffer_len() + ip.payload_len];
    {
        let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
        ip.emit(&mut packet, &checksums);
        packet.payload_mut().copy_from_slice(&icmp);
    }
    ethernet_wrap(PEER_MAC, WIRE_MAC, EthernetProtocol::Ipv4, &bytes)
}

/// Parse an ICMPv4 echo out of a captured Ethernet frame:
/// `(src, dst, is_request, ident, seq_no, data)`.
fn parse_echo(frame: &[u8]) -> (Ipv4Address, Ipv4Address, bool, u16, u16, Vec<u8>) {
    let eth = EthernetFrame::new_checked(frame).expect("ethernet");
    assert_eq!(eth.ethertype(), EthernetProtocol::Ipv4);
    let ip = Ipv4Packet::new_checked(eth.payload()).expect("ipv4");
    assert!(ip.verify_checksum());
    assert_eq!(ip.next_header(), IpProtocol::Icmp);
    let packet = Icmpv4Packet::new_checked(ip.payload()).expect("icmp");
    let repr = Icmpv4Repr::parse(&packet, &ChecksumCapabilities::default()).expect("icmp repr");
    match repr {
        Icmpv4Repr::EchoRequest {
            ident,
            seq_no,
            data,
        } => (
            ip.src_addr(),
            ip.dst_addr(),
            true,
            ident,
            seq_no,
            data.to_vec(),
        ),
        Icmpv4Repr::EchoReply {
            ident,
            seq_no,
            data,
        } => (
            ip.src_addr(),
            ip.dst_addr(),
            false,
            ident,
            seq_no,
            data.to_vec(),
        ),
        _ => panic!("not an echo"),
    }
}

/// A UDP datagram from the peer to the wire address, exactly as it arrives from the LAN.
fn udp_frame(src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let checksums = ChecksumCapabilities::default();
    let udp = UdpRepr { src_port, dst_port };
    let ip = Ipv4Repr {
        src_addr: Ipv4Address::from(PEER_IP),
        dst_addr: Ipv4Address::from(WIRE_IP),
        next_header: IpProtocol::Udp,
        payload_len: udp.header_len() + payload.len(),
        hop_limit: 64,
    };
    let mut bytes = std::vec![0u8; ip.buffer_len() + ip.payload_len];
    let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
    ip.emit(&mut packet, &checksums);
    let mut datagram = UdpPacket::new_unchecked(packet.payload_mut());
    let src: IpAddress = Ipv4Address::from(PEER_IP).into();
    let dst: IpAddress = Ipv4Address::from(WIRE_IP).into();
    udp.emit(
        &mut datagram,
        &src,
        &dst,
        payload.len(),
        |buf| buf.copy_from_slice(payload),
        &checksums,
    );
    ethernet_wrap(PEER_MAC, WIRE_MAC, EthernetProtocol::Ipv4, &bytes)
}

#[test]
fn a_wire_send_resolves_arp_and_the_datagram_leaves_byte_exact() {
    let mut server = Server::new(0);
    server.attach_wire(0, WIRE_MAC, WIRE_IP, 24);
    let mut c = Client::open(&mut server);
    let s = c.udp(&mut server);
    let port = c.bind(&mut server, s, 0);
    let to = Addr {
        ip: PEER_IP,
        port: PEER_PORT,
    };
    let sent = c.sendto_to(&mut server, s, to, b"over the wire");
    assert_eq!((sent.status, sent.result), (Status::Ok, 13));

    // First the ARP request for the peer, broadcast from the wire address.
    let mut frame = [0u8; FRAME_MAX];
    let n = server.take_wire_frame(&mut frame).expect("an ARP request");
    assert_eq!(&frame[0..6], &[0xFF; 6]);
    assert_eq!(&frame[6..12], &WIRE_MAC);
    assert_eq!(&frame[12..14], &[0x08, 0x06]);
    let arp = ArpPacket::new_checked(&frame[14..n]).expect("an ARP packet");
    match ArpRepr::parse(&arp).expect("an ARP repr") {
        ArpRepr::EthernetIpv4 {
            operation,
            source_protocol_addr,
            target_protocol_addr,
            ..
        } => {
            assert_eq!(operation, ArpOperation::Request);
            assert_eq!(source_protocol_addr, Ipv4Address::from(WIRE_IP));
            assert_eq!(target_protocol_addr, Ipv4Address::from(PEER_IP));
        }
        // ArpRepr is non-exhaustive; Ethernet/IPv4 is the only repr this stack speaks.
        _ => panic!("unexpected ARP repr"),
    }
    assert!(
        server.take_wire_frame(&mut frame).is_none(),
        "nothing else leaves before the neighbour answers"
    );

    // The peer answers; the queued datagram must now leave byte-exact.
    server.push_wire_frame(&arp_reply()).unwrap();
    let mut out = Vec::new();
    server.poll(MS, &mut out);
    let n = server
        .take_wire_frame(&mut frame)
        .expect("the datagram after ARP");
    assert_eq!(&frame[0..6], &PEER_MAC);
    assert_eq!(&frame[6..12], &WIRE_MAC);
    assert_eq!(&frame[12..14], &[0x08, 0x00]);
    let ip = Ipv4Packet::new_checked(&frame[14..n]).expect("an IPv4 packet");
    assert_eq!(ip.src_addr(), Ipv4Address::from(WIRE_IP));
    assert_eq!(ip.dst_addr(), Ipv4Address::from(PEER_IP));
    let udp = UdpPacket::new_checked(ip.payload()).expect("a UDP packet");
    assert_eq!(udp.src_port(), port);
    assert_eq!(udp.dst_port(), PEER_PORT);
    assert_eq!(udp.payload(), b"over the wire");
}

#[test]
fn a_wire_datagram_is_received_and_counted_for_credits() {
    let mut server = Server::new(0);
    server.attach_wire(0, WIRE_MAC, WIRE_IP, 24);
    let mut c = Client::open(&mut server);
    let s = c.udp(&mut server);
    assert_eq!(c.bind(&mut server, s, WIRE_PORT), WIRE_PORT);
    server
        .push_wire_frame(&udp_frame(PEER_PORT, WIRE_PORT, b"from the wire"))
        .unwrap();
    let mut out = Vec::new();
    server.poll(MS, &mut out);
    assert_eq!(
        server.wire_delivered(),
        1,
        "a consumed frame owes one credit"
    );
    let got = c.recvfrom(&mut server, s, 64);
    assert_eq!(got.status, Status::Ok);
    assert_eq!(
        &got.payload[..ADDR_LEN],
        Addr {
            ip: PEER_IP,
            port: PEER_PORT
        }
        .encode()
    );
    assert_eq!(&got.payload[ADDR_LEN..], b"from the wire");
}

#[test]
fn wire_datagrams_of_zero_and_maximum_size_cross_both_ways() {
    let mut server = Server::new(0);
    server.attach_wire(0, WIRE_MAC, WIRE_IP, 24);
    let mut c = Client::open(&mut server);
    let s = c.udp(&mut server);
    assert_eq!(c.bind(&mut server, s, WIRE_PORT), WIRE_PORT);

    // Inbound: the zero-length and the maximum unfragmented datagram from the peer.
    server
        .push_wire_frame(&udp_frame(PEER_PORT, WIRE_PORT, b""))
        .unwrap();
    let big = pattern(UDP_PAYLOAD_MAX, 9);
    server
        .push_wire_frame(&udp_frame(PEER_PORT, WIRE_PORT, &big))
        .unwrap();
    let mut out = Vec::new();
    server.poll(MS, &mut out);
    let zero = c.recvfrom(&mut server, s, UDP_PAYLOAD_MAX as u16);
    assert_eq!((zero.status, zero.result), (Status::Ok, 0));
    assert_eq!(
        &zero.payload[..ADDR_LEN],
        Addr {
            ip: PEER_IP,
            port: PEER_PORT
        }
        .encode()
    );
    assert_eq!(
        zero.payload.len(),
        ADDR_LEN,
        "an empty datagram is an empty body"
    );
    let max = c.recvfrom(&mut server, s, UDP_PAYLOAD_MAX as u16);
    assert_eq!(
        (max.status, max.result),
        (Status::Ok, UDP_PAYLOAD_MAX as u64)
    );
    assert_eq!(&max.payload[ADDR_LEN..], big.as_slice());

    // Outbound: one datagram at a time. The first send starts the ARP exchange, and the
    // neighbour's one-packet queue holds only that first datagram; a send queued before
    // the reply would be dropped by smoltcp pre-resolution (netprobe's peer mode gives
    // the first send the same 200 ms beat before its burst).
    let to = Addr {
        ip: PEER_IP,
        port: PEER_PORT,
    };
    let mut frame = [0u8; FRAME_MAX];
    assert_eq!(c.sendto_to(&mut server, s, to, b"").status, Status::Ok);
    server
        .take_wire_frame(&mut frame)
        .expect("an ARP request for the peer");
    server.push_wire_frame(&arp_reply()).unwrap();
    server.poll(MS, &mut out);
    let n = server
        .take_wire_frame(&mut frame)
        .expect("the zero-length datagram");
    let ip = Ipv4Packet::new_checked(&frame[14..n]).expect("ipv4");
    let udp = UdpPacket::new_checked(ip.payload()).expect("udp");
    assert_eq!(udp.payload().len(), 0);
    assert_eq!(c.sendto_to(&mut server, s, to, &big).status, Status::Ok);
    server.poll(MS, &mut out);
    let n = server
        .take_wire_frame(&mut frame)
        .expect("the maximum datagram");
    let ip = Ipv4Packet::new_checked(&frame[14..n]).expect("ipv4");
    let udp = UdpPacket::new_checked(ip.payload()).expect("udp");
    assert_eq!(udp.payload(), big.as_slice());
    assert_eq!(n, FRAME_MAX, "the maximum datagram is one whole frame");
}

#[test]
fn connects_and_names_follow_the_wire_once_attached() {
    let mut server = Server::new(0);
    server.attach_wire(0, WIRE_MAC, WIRE_IP, 24);
    let mut c = Client::open(&mut server);
    let s = c.udp(&mut server);
    let port = c.bind(&mut server, s, 0);
    let peer = Addr {
        ip: PEER_IP,
        port: PEER_PORT,
    };
    assert_eq!(
        c.call(&mut server, Op::Connect, s, 0, &peer.encode())
            .status,
        Status::Ok
    );
    let name = c.call(&mut server, Op::GetName, s, 0, &[]);
    assert_eq!(name.status, Status::Ok);
    assert_eq!(
        &name.payload[..ADDR_LEN],
        Addr { ip: WIRE_IP, port }.encode()
    );
    assert_eq!(&name.payload[ADDR_LEN..], peer.encode());
}

#[test]
fn the_interface_answers_a_peer_ping_after_its_arp_request() {
    let mut server = Server::new(0);
    server.attach_wire(0, WIRE_MAC, WIRE_IP, 24);
    let mut out = Vec::new();
    let mut frame = [0u8; FRAME_MAX];

    // The peer resolves us first, as a host does; the interface answers and learns the peer.
    server.push_wire_frame(&arp_request()).unwrap();
    server.poll(MS, &mut out);
    let n = server.take_wire_frame(&mut frame).expect("the ARP reply");
    assert_eq!(&frame[0..6], &PEER_MAC);
    assert_eq!(&frame[6..12], &WIRE_MAC);
    assert_eq!(&frame[12..14], &[0x08, 0x06]);
    match ArpRepr::parse(&ArpPacket::new_checked(&frame[14..n]).unwrap()).unwrap() {
        ArpRepr::EthernetIpv4 {
            operation,
            source_protocol_addr,
            target_protocol_addr,
            ..
        } => {
            assert_eq!(operation, ArpOperation::Reply);
            assert_eq!(source_protocol_addr, Ipv4Address::from(WIRE_IP));
            assert_eq!(target_protocol_addr, Ipv4Address::from(PEER_IP));
        }
        _ => panic!("unexpected ARP repr"),
    }

    // The peer's ping is answered by the interface itself (auto-icmp-echo-reply).
    server
        .push_wire_frame(&echo_from_peer(true, 0x1234, 7, b"kumo net8 ping"))
        .unwrap();
    server.poll(MS, &mut out);
    let n = server.take_wire_frame(&mut frame).expect("the echo reply");
    let (src, dst, request, ident, seq_no, data) = parse_echo(&frame[..n]);
    assert_eq!(
        (src, dst),
        (Ipv4Address::from(WIRE_IP), Ipv4Address::from(PEER_IP))
    );
    assert!(!request, "the answer is an echo reply");
    assert_eq!((ident, seq_no), (0x1234, 7));
    assert_eq!(data, b"kumo net8 ping");
    assert!(server.take_wire_frame(&mut frame).is_none());
}

#[test]
fn a_kumo_ping_crosses_to_the_peer_and_the_reply_comes_back() {
    let mut server = Server::new(0);
    server.attach_wire(0, WIRE_MAC, WIRE_IP, 24);
    let mut c = Client::open(&mut server);
    let s = c.call(&mut server, Op::Socket, 0, 3, &[]).socket;
    let bound = c.call(
        &mut server,
        Op::Bind,
        s,
        0,
        &Addr {
            ip: [0; 4],
            port: 0,
        }
        .encode(),
    );
    assert_eq!(bound.status, Status::Ok);
    let ident = bound.result as u16;
    assert!(ident >= 49152, "an ephemeral identifier");

    let payload = b"kumo-net8-0-";
    let sent = c.sendto_to(
        &mut server,
        s,
        Addr {
            ip: PEER_IP,
            port: 0,
        },
        payload,
    );
    assert_eq!(
        (sent.status, sent.result),
        (Status::Ok, payload.len() as u64)
    );

    // Cold neighbour cache: the echo request waits behind one ARP request for the peer.
    let mut frame = [0u8; FRAME_MAX];
    server.take_wire_frame(&mut frame).expect("an ARP request");
    assert_eq!(&frame[0..6], &[0xFF; 6]);
    assert!(server.take_wire_frame(&mut frame).is_none());
    server.push_wire_frame(&arp_reply()).unwrap();
    let mut out = Vec::new();
    server.poll(MS, &mut out);
    let n = server
        .take_wire_frame(&mut frame)
        .expect("the echo request");
    let (src, dst, request, got_ident, seq_no, data) = parse_echo(&frame[..n]);
    assert_eq!(
        (src, dst),
        (Ipv4Address::from(WIRE_IP), Ipv4Address::from(PEER_IP))
    );
    assert!(request, "an echo request");
    assert_eq!((got_ident, seq_no), (ident, 0));
    assert_eq!(data, payload);

    // The peer answers; WAIT is ready and RECVFROM reports the data and the source.
    server
        .push_wire_frame(&echo_from_peer(false, ident, 0, payload))
        .unwrap();
    server.poll(MS, &mut out);
    let wait = c.call(
        &mut server,
        Op::Wait,
        s,
        WAIT_READABLE as u64 | (1000u64 << 32),
        &[],
    );
    assert_eq!(
        (wait.status, wait.result),
        (Status::Ok, WAIT_READABLE as u64)
    );
    let got = c.recvfrom(&mut server, s, 1472);
    assert_eq!(got.status, Status::Ok);
    assert_eq!(
        &got.payload[..ADDR_LEN],
        Addr {
            ip: PEER_IP,
            port: 0
        }
        .encode()
    );
    assert_eq!(&got.payload[ADDR_LEN..], payload);
    assert_eq!(got.result, payload.len() as u64);
    assert_eq!(server.stats().echo_requests_sent, 1);
    assert_eq!(server.stats().echo_replies_received, 1);
}

#[test]
fn malformed_frames_are_refused_with_bounded_work() {
    let mut server = Server::new(0);
    server.attach_wire(0, WIRE_MAC, WIRE_IP, 24);
    let mut out = Vec::new();
    let mut frame = [0u8; FRAME_MAX];

    let ipv6 = ethernet_wrap(PEER_MAC, WIRE_MAC, EthernetProtocol::Ipv6, &[0u8; 40]);
    assert_eq!(
        server.push_wire_frame(&ipv6),
        Err(IngressError::UnknownEthertype(0x86DD))
    );
    let vlan = ethernet_wrap(
        PEER_MAC,
        WIRE_MAC,
        EthernetProtocol::Unknown(0x8100),
        &[0u8; 40],
    );
    assert_eq!(
        server.push_wire_frame(&vlan),
        Err(IngressError::UnknownEthertype(0x8100))
    );

    // A first fragment of a valid datagram never reaches the stack as a whole packet.
    let mut first = udp_frame(PEER_PORT, WIRE_PORT, b"fragment");
    {
        let mut packet = Ipv4Packet::new_unchecked(&mut first[14..]);
        packet.set_more_frags(true);
        packet.fill_checksum();
    }
    assert_eq!(server.push_wire_frame(&first), Err(IngressError::Fragment));

    let mut bad = udp_frame(PEER_PORT, WIRE_PORT, b"checksum");
    bad[14 + 10] ^= 0xFF;
    assert_eq!(
        server.push_wire_frame(&bad),
        Err(IngressError::BadIpv4Checksum)
    );

    let short = ethernet_wrap(PEER_MAC, WIRE_MAC, EthernetProtocol::Ipv4, &[0u8; 10]);
    assert_eq!(server.push_wire_frame(&short), Err(IngressError::Truncated));

    let refusals = server.wire_refusals().expect("a wire is attached");
    assert_eq!((refusals.unknown_ethertype, refusals.fragments), (2, 1));
    assert_eq!((refusals.bad_ipv4_checksum, refusals.truncated), (1, 1));
    server.poll(MS, &mut out);
    assert!(
        server.take_wire_frame(&mut frame).is_none(),
        "refusals build nothing"
    );

    // An echo with a bad ICMP checksum passes the IPv4 gate and is dropped by smoltcp's own
    // parse: still no reply, still one fixed parse's worth of work.
    let mut corrupt = echo_from_peer(true, 0x2222, 1, b"bad icmp checksum");
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    assert_eq!(server.push_wire_frame(&corrupt), Ok(()));
    server.poll(MS, &mut out);
    assert!(server.take_wire_frame(&mut frame).is_none());
}

#[test]
fn icmp_sockets_follow_the_wire_generation() {
    let mut server = Server::new(0);
    server.attach_wire(0, WIRE_MAC, WIRE_IP, 24);
    let mut c = Client::open(&mut server);
    let s = c.call(&mut server, Op::Socket, 0, 3, &[]).socket;
    let bound = c.call(
        &mut server,
        Op::Bind,
        s,
        0,
        &Addr {
            ip: [0; 4],
            port: 5000,
        }
        .encode(),
    );
    assert_eq!((bound.status, bound.result), (Status::Ok, 5000));
    let to = Addr {
        ip: PEER_IP,
        port: 0,
    };

    // Detach: the client socket survives, its wire socket does not.
    assert_eq!(c.sendto_to(&mut server, s, to, b"one").status, Status::Ok);
    server.detach_wire();
    assert_eq!(
        c.sendto_to(&mut server, s, to, b"two").status,
        Status::NetUnreachable
    );

    // A new generation re-creates the wire socket for the same identifier and it sends again.
    server.attach_wire(0, WIRE_MAC, WIRE_IP, 24);
    assert_eq!(c.sendto_to(&mut server, s, to, b"three").status, Status::Ok);
    // Closing frees the identifier for a fresh socket.
    assert_eq!(c.call(&mut server, Op::Close, s, 0, &[]).status, Status::Ok);
    let s2 = c.call(&mut server, Op::Socket, 0, 3, &[]).socket;
    let again = c.call(
        &mut server,
        Op::Bind,
        s2,
        0,
        &Addr {
            ip: [0; 4],
            port: 5000,
        }
        .encode(),
    );
    assert_eq!((again.status, again.result), (Status::Ok, 5000));
}

#[test]
fn detaching_the_wire_stack_drops_wire_routing() {
    let mut server = Server::new(0);
    server.attach_wire(0, WIRE_MAC, WIRE_IP, 24);
    let mut c = Client::open(&mut server);
    let s = c.udp(&mut server);
    c.bind(&mut server, s, 0);
    server.detach_wire();
    assert!(!server.wire_attached());
    assert_eq!(server.wire_delivered(), 0);
    let to = Addr {
        ip: PEER_IP,
        port: PEER_PORT,
    };
    assert_eq!(
        c.sendto_to(&mut server, s, to, b"x").status,
        Status::NetUnreachable
    );
}

// ---- NET10: TCP on the wire stack, against a real smoltcp peer ------------------------------

/// The peer host: its own smoltcp interface and socket table over its own `FrameDevice`.
/// Frames move between it and the server's wire stack through the same rings smoltcp drives,
/// so the handshake, ACKs, windows, RTOs and FINs are all real TCP.
struct WirePeer {
    device: FrameDevice,
    iface: Interface,
    sockets: SocketSet<'static>,
}

impl WirePeer {
    fn new(now_ns: u64) -> Self {
        let mut device = FrameDevice::new();
        let mut iface = Interface::new(
            Config::new(HardwareAddress::Ethernet(EthernetAddress(PEER_MAC))),
            &mut device,
            instant(now_ns),
        );
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(Ipv4Address::from(PEER_IP)), 24))
                .unwrap();
        });
        let storage: &'static mut [SocketStorage<'static>] =
            Box::leak((0..4).map(|_| SocketStorage::EMPTY).collect());
        Self {
            device,
            iface,
            sockets: SocketSet::new(storage),
        }
    }

    fn tcp(&mut self) -> SocketHandle {
        let socket = tcp::Socket::new(
            tcp::SocketBuffer::new(leak(0u8, SOCKET_BUFFER)),
            tcp::SocketBuffer::new(leak(0u8, SOCKET_BUFFER)),
        );
        self.sockets.add(socket)
    }

    fn listen(&mut self, handle: SocketHandle) {
        self.sockets
            .get_mut::<tcp::Socket>(handle)
            .listen(PEER_PORT)
            .expect("the peer listens");
    }

    fn poll(&mut self, now_ns: u64) {
        self.iface
            .poll(instant(now_ns), &mut self.device, &mut self.sockets);
    }

    /// Drain everything the peer's socket has received so far. `recv_slice` answers
    /// `Ok(0)` for an open-but-empty socket, so the guard is `can_recv`.
    fn received(&mut self, handle: SocketHandle) -> Vec<u8> {
        let socket = self.sockets.get_mut::<tcp::Socket>(handle);
        let mut out = Vec::new();
        let mut buf = [0u8; 700];
        while socket.can_recv() {
            match socket.recv_slice(&mut buf) {
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        out
    }
}

/// Move every frame between the two stacks: server egress into the peer's ingress ring and
/// the peer's egress into the server's (through the real NET8 ingress policy).
fn pump(server: &mut Server, peer: &mut WirePeer) -> usize {
    let mut frame = [0u8; FRAME_MAX];
    let mut moved = 0;
    while let Some(n) = server.take_wire_frame(&mut frame) {
        peer.device
            .push_inbound(&frame[..n])
            .expect("the peer's ingress ring has room");
        moved += 1;
    }
    while let Some(n) = peer.device.take_outbound(&mut frame) {
        server
            .push_wire_frame(&frame[..n])
            .expect("the server's ingress policy admits it");
        moved += 1;
    }
    moved
}

fn run_wire(server: &mut Server, peer: &mut WirePeer, ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    for _ in 0..ms {
        clock::advance(MS);
        let now = clock::now();
        peer.poll(now);
        server.poll(now, &mut out);
        pump(server, peer);
    }
    out
}

/// Is this frame a TCP segment carrying payload? The induced-loss test drops the first one.
fn tcp_data(frame: &[u8]) -> bool {
    if frame.len() < 34 || frame[12..14] != [0x08, 0x00] || frame[23] != 6 {
        return false;
    }
    let ihl = usize::from(frame[14] & 0x0F) * 4;
    if frame.len() < 14 + ihl + 20 {
        return false;
    }
    let total = usize::from(u16::from_be_bytes([frame[16], frame[17]]));
    total > ihl + 20
}

fn run_wire_dropping(server: &mut Server, peer: &mut WirePeer, drop: &mut usize, ms: u64) {
    let mut out = Vec::new();
    for _ in 0..ms {
        clock::advance(MS);
        let now = clock::now();
        peer.poll(now);
        server.poll(now, &mut out);
        let mut frame = [0u8; FRAME_MAX];
        while let Some(n) = server.take_wire_frame(&mut frame) {
            if *drop > 0 && tcp_data(&frame[..n]) {
                *drop -= 1;
                continue;
            }
            peer.device
                .push_inbound(&frame[..n])
                .expect("the peer's ingress ring has room");
        }
        while let Some(n) = peer.device.take_outbound(&mut frame) {
            server
                .push_wire_frame(&frame[..n])
                .expect("the server's ingress policy admits it");
        }
    }
}

fn wire_bind(client: &mut Client, server: &mut Server, socket: u32, port: u16) -> u16 {
    let answer = client.at(
        server,
        Op::Bind,
        socket,
        0,
        &Addr { ip: [0; 4], port }.encode(),
    );
    assert_eq!(answer.status, Status::Ok);
    answer.result as u16
}

fn wire_connect(client: &mut Client, server: &mut Server, socket: u32) -> Status {
    client
        .at(
            server,
            Op::Connect,
            socket,
            0,
            &Addr {
                ip: PEER_IP,
                port: PEER_PORT,
            }
            .encode(),
        )
        .status
}

fn wire_wait(
    client: &mut Client,
    server: &mut Server,
    peer: &mut WirePeer,
    socket: u32,
    interest: u8,
    timeout_ms: u64,
) -> Answer {
    let id = client.next_id;
    client.next_id += 1;
    let mut bytes = [0u8; 4096];
    let len = encode_request(
        &header(
            Op::Wait,
            socket,
            client.generation,
            interest as u64 | (timeout_ms << 32),
            id,
        ),
        &[],
        &mut bytes,
    )
    .expect("a WAIT request encodes");
    let mut out = Vec::new();
    assert_eq!(
        server.handle(client.session, &bytes[..len], 0, clock::now(), &mut out),
        After::Keep
    );
    for _ in 0..(timeout_ms + 100) {
        if !out.is_empty() {
            break;
        }
        out.extend(run_wire(server, peer, 1));
    }
    assert!(!out.is_empty(), "the WAIT settled before the deadline");
    Client::read(&out.remove(0))
}

/// CONNECT from kumo to the peer's listener, WAIT for the handshake, and return the socket.
fn wire_connected(
    server: &mut Server,
    peer: &mut WirePeer,
    client: &mut Client,
    listening: SocketHandle,
) -> u32 {
    let socket = client.tcp(server);
    assert_eq!(wire_connect(client, server, socket), Status::InProgress);
    let ready = wire_wait(client, server, peer, socket, WAIT_CONNECTED, 5000);
    assert_eq!(ready.status, Status::Ok);
    assert_ne!(ready.result & WAIT_CONNECTED as u64, 0);
    // The peer's third ACK may still be in flight when kumo sees Established.
    run_wire(server, peer, 5);
    assert_eq!(
        peer.sockets.get::<tcp::Socket>(listening).state(),
        tcp::State::Established
    );
    socket
}

#[test]
fn a_wire_tcp_stream_crosses_both_stacks_exactly() {
    let mut server = Server::new(clock::now());
    server.attach_wire(clock::now(), WIRE_MAC, WIRE_IP, 24);
    let mut peer = WirePeer::new(clock::now());
    let listening = peer.tcp();
    peer.listen(listening);
    let mut c = Client::open(&mut server);
    let s = wire_connected(&mut server, &mut peer, &mut c, listening);

    // kumo -> peer: a multi-buffer payload, sent in STREAM chunks while the peer drains.
    let data = pattern(12_000, 3);
    let (mut sent, mut got) = (0usize, Vec::new());
    let mut partial = false;
    let mut would_block = false;
    for _ in 0..20_000 {
        // Fill the transmit buffer before pumping: the tail chunk then only partially
        // fits, which is the wire face of backpressure (the loopback test's slow reader
        // produces the same partial SENDs).
        while sent < data.len() {
            let chunk = (data.len() - sent).min(700);
            let answer = c.at(&mut server, Op::Send, s, 0, &data[sent..sent + chunk]);
            match answer.status {
                Status::Ok => {
                    partial |= (answer.result as usize) < chunk;
                    sent += answer.result as usize;
                }
                Status::WouldBlock => {
                    would_block = true;
                    break;
                }
                other => panic!("wire send -> {other:?}"),
            }
        }
        got.extend(peer.received(listening));
        if sent == data.len() && got.len() == data.len() {
            break;
        }
        run_wire(&mut server, &mut peer, 2);
    }
    assert_eq!(got, data, "the kumo->peer stream is byte-exact");
    assert!(partial, "partial SENDs were exercised on the wire");
    assert!(would_block, "the full transmit buffer refused a SEND");

    // peer -> kumo.
    let back = pattern(12_000, 7);
    let (mut pushed, mut echoed) = (0usize, Vec::new());
    for _ in 0..20_000 {
        if pushed < back.len() {
            let socket = peer.sockets.get_mut::<tcp::Socket>(listening);
            if let Ok(n) = socket.send_slice(&back[pushed..]) {
                pushed += n;
            }
        }
        let answer = c.at(&mut server, Op::Recv, s, 700, &[]);
        match answer.status {
            Status::Ok if answer.payload.is_empty() => {}
            Status::Ok => echoed.extend_from_slice(&answer.payload),
            Status::WouldBlock => {}
            other => panic!("wire recv -> {other:?}"),
        }
        if pushed == back.len() && echoed.len() == back.len() {
            break;
        }
        run_wire(&mut server, &mut peer, 2);
    }
    assert_eq!(echoed, back, "the peer->kumo stream is byte-exact");

    // Orderly close: kumo's FIN reads as EOF on the peer; the peer's close as EOF on kumo.
    assert_eq!(
        c.at(&mut server, Op::Shutdown, s, 2, &[]).status,
        Status::Ok
    );
    run_wire(&mut server, &mut peer, 100);
    {
        let mut buf = [0u8; 16];
        let socket = peer.sockets.get_mut::<tcp::Socket>(listening);
        assert!(
            matches!(socket.recv_slice(&mut buf), Err(tcp::RecvError::Finished)),
            "kumo's FIN is an orderly end at the peer"
        );
    }
    peer.sockets.get_mut::<tcp::Socket>(listening).close();
    let mut eof = false;
    for _ in 0..2000 {
        let answer = c.at(&mut server, Op::Recv, s, 64, &[]);
        match answer.status {
            Status::Ok if answer.payload.is_empty() => {
                eof = true;
                break;
            }
            Status::Ok => {}
            Status::WouldBlock => {}
            other => panic!("wire recv at close -> {other:?}"),
        }
        run_wire(&mut server, &mut peer, 2);
    }
    assert!(eof, "the peer's FIN reads as EOF on kumo");
    assert_eq!(server.stats().stream_bytes_sent, data.len() as u64);
    assert_eq!(server.stats().stream_bytes_received, back.len() as u64);
}

#[test]
fn a_wire_listener_accepts_a_peer_connection() {
    let mut server = Server::new(clock::now());
    server.attach_wire(clock::now(), WIRE_MAC, WIRE_IP, 24);
    let mut peer = WirePeer::new(clock::now());
    let mut c = Client::open(&mut server);
    let l = c.tcp(&mut server);
    assert_eq!(wire_bind(&mut c, &mut server, l, WIRE_PORT), WIRE_PORT);
    assert_eq!(c.at(&mut server, Op::Listen, l, 2, &[]).status, Status::Ok);

    let remote = peer.tcp();
    peer.sockets
        .get_mut::<tcp::Socket>(remote)
        .connect(
            peer.iface.context(),
            IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::from(WIRE_IP)), WIRE_PORT),
            PEER_PORT,
        )
        .expect("the peer connects");
    run_wire(&mut server, &mut peer, 50);

    let ready = wire_wait(&mut c, &mut server, &mut peer, l, WAIT_CONNECTED, 5000);
    assert_eq!(ready.status, Status::Ok);
    let accepted = c.at(&mut server, Op::Accept, l, 0, &[]);
    assert_eq!(accepted.status, Status::Ok);
    assert_eq!(accepted.socket as u64, accepted.result);
    assert_eq!(
        accepted.payload,
        Addr {
            ip: PEER_IP,
            port: PEER_PORT
        }
        .encode()
    );

    // One message each way, byte-exact.
    let stream = accepted.socket;
    assert_eq!(
        c.at(&mut server, Op::Send, stream, 0, b"from kumo").status,
        Status::Ok
    );
    run_wire(&mut server, &mut peer, 50);
    assert_eq!(peer.received(remote), b"from kumo".to_vec());

    let socket = peer.sockets.get_mut::<tcp::Socket>(remote);
    socket.send_slice(b"from peer").expect("the peer sends");
    run_wire(&mut server, &mut peer, 50);
    let answer = c.at(&mut server, Op::Recv, stream, 64, &[]);
    assert_eq!(
        (answer.status, answer.payload.as_slice()),
        (Status::Ok, b"from peer".as_slice())
    );

    // Close both ends and check the names: kumo reports the wire address locally.
    let names = c.at(&mut server, Op::GetName, stream, 0, &[]);
    assert_eq!(
        u16::from_le_bytes([names.payload[2], names.payload[3]]),
        WIRE_PORT,
        "the local port is the listener's"
    );
    assert_eq!(
        &names.payload[4..8],
        &WIRE_IP,
        "local address is the wire address"
    );
    peer.sockets.get_mut::<tcp::Socket>(remote).close();
    assert_eq!(
        c.at(&mut server, Op::Close, stream, 0, &[]).status,
        Status::Ok
    );
    assert_eq!(c.at(&mut server, Op::Close, l, 0, &[]).status, Status::Ok);
    run_wire(&mut server, &mut peer, 300);
    assert_eq!(server.busy_slots(), 0, "every slot came back");
}

#[test]
fn a_wire_refusal_and_a_peer_reset_surface_as_statuses() {
    let mut server = Server::new(clock::now());
    server.attach_wire(clock::now(), WIRE_MAC, WIRE_IP, 24);
    let mut peer = WirePeer::new(clock::now());
    let mut c = Client::open(&mut server);

    // No listener at the peer: its stack answers the SYN with RST.
    let refused = c.tcp(&mut server);
    assert_eq!(
        wire_connect(&mut c, &mut server, refused),
        Status::InProgress
    );
    let answer = wire_wait(
        &mut c,
        &mut server,
        &mut peer,
        refused,
        WAIT_CONNECTED,
        5000,
    );
    assert_eq!(answer.status, Status::ConnRefused);
    let _ = c.at(&mut server, Op::Close, refused, 0, &[]);

    // An accept-then-abort at the peer is a reset at kumo, not an orderly end.
    let listening = peer.tcp();
    peer.listen(listening);
    let s = wire_connected(&mut server, &mut peer, &mut c, listening);
    peer.sockets.get_mut::<tcp::Socket>(listening).abort();
    let mut reset = false;
    for _ in 0..2000 {
        let answer = c.at(&mut server, Op::Recv, s, 64, &[]);
        match answer.status {
            Status::ConnReset => {
                reset = true;
                break;
            }
            Status::WouldBlock => {}
            Status::Ok if answer.payload.is_empty() => {}
            other => panic!("wire recv after peer abort -> {other:?}"),
        }
        run_wire(&mut server, &mut peer, 2);
    }
    assert!(reset, "the peer's RST surfaces as ConnReset");
    let _ = c.at(&mut server, Op::Close, s, 0, &[]);
    run_wire(&mut server, &mut peer, 200);
}

#[test]
fn a_wire_tcp_retransmits_under_induced_loss() {
    let mut server = Server::new(clock::now());
    server.attach_wire(clock::now(), WIRE_MAC, WIRE_IP, 24);
    let mut peer = WirePeer::new(clock::now());
    let listening = peer.tcp();
    peer.listen(listening);
    let mut c = Client::open(&mut server);
    let s = wire_connected(&mut server, &mut peer, &mut c, listening);

    let message = pattern(512, 5);
    assert_eq!(
        c.at(&mut server, Op::Send, s, 0, &message).status,
        Status::Ok
    );
    // Lose the segment carrying the message, then let the initial RTO (1000 ms) elapse.
    let mut drops = 1;
    run_wire_dropping(&mut server, &mut peer, &mut drops, 20);
    assert_eq!(drops, 0, "the data segment was dropped");
    assert_eq!(
        peer.received(listening),
        Vec::new(),
        "nothing arrived while the segment was lost"
    );
    run_wire(&mut server, &mut peer, 2500);
    assert_eq!(
        peer.received(listening),
        message,
        "the retransmission delivered the message"
    );
}

#[test]
fn a_wire_stream_reaches_backpressure_and_deadlines() {
    let mut server = Server::new(clock::now());
    server.attach_wire(clock::now(), WIRE_MAC, WIRE_IP, 24);
    let mut peer = WirePeer::new(clock::now());
    let listening = peer.tcp();
    peer.listen(listening);
    let mut c = Client::open(&mut server);
    let s = wire_connected(&mut server, &mut peer, &mut c, listening);

    // The peer never reads: SEND eventually refuses rather than growing memory.
    let block = [0x5Au8; STREAM_PAYLOAD_MAX];
    let mut blocked = false;
    for _ in 0..40 {
        match c.at(&mut server, Op::Send, s, 0, &block).status {
            Status::Ok => {}
            Status::WouldBlock => {
                blocked = true;
                break;
            }
            other => panic!("wire send -> {other:?}"),
        }
    }
    assert!(blocked, "the unread wire stream reaches backpressure");

    // READABLE with no data answers the deadline, and the stack keeps running.
    let answer = wire_wait(&mut c, &mut server, &mut peer, s, WAIT_READABLE, 120);
    assert_eq!(answer.status, Status::TimedOut);
}

#[test]
fn repeated_wire_connections_return_the_slot_census_flat() {
    let mut server = Server::new(clock::now());
    server.attach_wire(clock::now(), WIRE_MAC, WIRE_IP, 24);
    let mut peer = WirePeer::new(clock::now());
    let mut c = Client::open(&mut server);
    for cycle in 0..6u8 {
        let listening = peer.tcp();
        peer.listen(listening);
        let s = wire_connected(&mut server, &mut peer, &mut c, listening);
        let msg = [cycle; 16];
        assert_eq!(c.at(&mut server, Op::Send, s, 0, &msg).status, Status::Ok);
        run_wire(&mut server, &mut peer, 50);
        assert_eq!(peer.received(listening), msg.to_vec());
        peer.sockets.get_mut::<tcp::Socket>(listening).close();
        run_wire(&mut server, &mut peer, 50);
        assert_eq!(c.at(&mut server, Op::Close, s, 0, &[]).status, Status::Ok);
        run_wire(&mut server, &mut peer, 300);
        peer.sockets.remove(listening);
    }
    assert_eq!(server.busy_slots(), 0, "every slot came back");
    assert_eq!(server.live_sockets(), 0);
}

#[test]
fn a_generation_change_resets_a_wire_stream() {
    let mut server = Server::new(clock::now());
    server.attach_wire(clock::now(), WIRE_MAC, WIRE_IP, 24);
    let mut peer = WirePeer::new(clock::now());
    let listening = peer.tcp();
    peer.listen(listening);
    let mut c = Client::open(&mut server);
    let s = wire_connected(&mut server, &mut peer, &mut c, listening);

    // The provider generation changes under a live stream: the stack is rebuilt and the
    // stream is gone without a FIN, which is a reset, never a live connection.
    server.reset_wire(clock::now());
    assert_eq!(
        c.at(&mut server, Op::Send, s, 0, b"gone").status,
        Status::ConnReset
    );
    assert_eq!(
        c.at(&mut server, Op::Recv, s, 64, &[]).status,
        Status::ConnReset
    );
    assert_eq!(c.at(&mut server, Op::Close, s, 0, &[]).status, Status::Ok);
    run_wire(&mut server, &mut peer, 100);
    assert_eq!(server.busy_slots(), 0);
}

/// The metal peer's ordering: it sends its whole payload, half-closes at once (FIN races
/// the stream), and only then reads. The stream test above lets the peer FIN arrive late;
/// this one reproduces the client phase that printed FAIL on the board.
#[test]
fn a_wire_tcp_reads_eof_when_the_peer_fin_races_the_stream() {
    let mut server = Server::new(clock::now());
    server.attach_wire(clock::now(), WIRE_MAC, WIRE_IP, 24);
    let mut peer = WirePeer::new(clock::now());
    let listening = peer.tcp();
    peer.listen(listening);
    let mut c = Client::open(&mut server);
    let s = wire_connected(&mut server, &mut peer, &mut c, listening);

    let back = pattern(12_000, 7);
    let data = pattern(12_000, 3);
    let (mut pushed, mut peer_shut) = (0usize, false);
    let (mut sent, mut got, mut peer_data) = (0usize, Vec::new(), Vec::new());
    let (mut shut, mut eof) = (false, false);
    for _ in 0..20_000 {
        {
            let socket = peer.sockets.get_mut::<tcp::Socket>(listening);
            if pushed < back.len() {
                if let Ok(n) = socket.send_slice(&back[pushed..]) {
                    pushed += n;
                }
            } else if !peer_shut {
                // The peer's half-close: FIN right after its payload.
                socket.close();
                peer_shut = true;
            }
        }
        // The peer's application reads continuously, like the real peer's read_all.
        peer_data.extend(peer.received(listening));
        if sent < data.len() {
            let chunk = (data.len() - sent).min(2048);
            let answer = c.at(&mut server, Op::Send, s, 0, &data[sent..sent + chunk]);
            match answer.status {
                Status::Ok => sent += answer.result as usize,
                Status::WouldBlock => {}
                other => panic!("send -> {other:?}"),
            }
        } else if !shut {
            assert_eq!(
                c.at(&mut server, Op::Shutdown, s, 2, &[]).status,
                Status::Ok
            );
            shut = true;
        }
        if !eof {
            let answer = c.at(&mut server, Op::Recv, s, 2048, &[]);
            match answer.status {
                Status::Ok if answer.payload.is_empty() => eof = true,
                Status::Ok => got.extend_from_slice(&answer.payload),
                Status::WouldBlock => {}
                other => panic!("recv -> {other:?}"),
            }
        }
        if shut
            && eof
            && sent == data.len()
            && pushed == back.len()
            && peer_data.len() == data.len()
        {
            break;
        }
        run_wire(&mut server, &mut peer, 1);
    }
    assert_eq!(got, back, "the peer->kumo stream is byte-exact");
    assert_eq!(
        peer_data, data,
        "the peer drained kumo's stream after half-closing"
    );
    assert!(eof, "kumo saw EOF after the racing FIN (eof={eof})");
    assert!(shut, "kumo half-closed (shut={shut})");
    assert_eq!(sent, data.len(), "kumo sent its whole stream (sent={sent})");
    assert_eq!(pushed, back.len(), "the peer queued its whole stream");
}

/// The race the metal client phase hit: the peer's FIN arrives while its payload still
/// sits unread in kumo's receive buffer. Kumo half-closes behind that payload, drains it,
/// and one poll later the peer's ACK retires the socket to Closed. The orderly path must
/// read as EOF, never as a reset, even though no dispatch observed CloseWait with an
/// empty buffer to latch `peer_finished`.
#[test]
fn wire_shutdown_behind_the_peer_fin_reads_eof_not_reset() {
    let mut server = Server::new(clock::now());
    server.attach_wire(clock::now(), WIRE_MAC, WIRE_IP, 24);
    let mut peer = WirePeer::new(clock::now());
    let listening = peer.tcp();
    peer.listen(listening);
    let mut c = Client::open(&mut server);
    let s = wire_connected(&mut server, &mut peer, &mut c, listening);

    // The peer fills kumo's receive buffer and half-closes at once: its FIN waits behind
    // data kumo has not read yet.
    let back = pattern(SOCKET_BUFFER, 7);
    {
        let socket = peer.sockets.get_mut::<tcp::Socket>(listening);
        assert!(matches!(socket.send_slice(&back), Ok(n) if n == back.len()));
        socket.close();
    }

    // Kumo sends its own stream first (the peer drains it on its side), then half-closes
    // while its receive buffer still holds the peer's payload.
    let data = pattern(6_000, 3);
    let (mut sent, mut peer_got) = (0usize, Vec::new());
    for _ in 0..20_000 {
        if sent < data.len() {
            let chunk = (data.len() - sent).min(2048);
            let answer = c.at(&mut server, Op::Send, s, 0, &data[sent..sent + chunk]);
            match answer.status {
                Status::Ok => sent += answer.result as usize,
                Status::WouldBlock => {}
                other => panic!("send -> {other:?}"),
            }
        }
        peer_got.extend(peer.received(listening));
        if sent == data.len() && peer_got.len() == data.len() {
            break;
        }
        run_wire(&mut server, &mut peer, 2);
    }
    assert_eq!(sent, data.len(), "kumo sent its whole stream");
    assert_eq!(peer_got, data, "the peer drained kumo's stream");
    // Let the peer's payload and FIN settle into kumo's stack.
    run_wire(&mut server, &mut peer, 20);

    assert_eq!(
        c.at(&mut server, Op::Shutdown, s, 2, &[]).status,
        Status::Ok
    );

    // Drain the peer's payload; the socket is LastAck behind it.
    let mut got = Vec::new();
    for _ in 0..2_000 {
        let answer = c.at(&mut server, Op::Recv, s, 2048, &[]);
        match answer.status {
            Status::Ok if answer.payload.is_empty() => {
                panic!("EOF before the payload behind the FIN drained")
            }
            Status::Ok => got.extend_from_slice(&answer.payload),
            Status::WouldBlock => {}
            other => panic!("recv -> {other:?}"),
        }
        if got.len() == back.len() {
            break;
        }
        run_wire(&mut server, &mut peer, 2);
    }
    assert_eq!(got, back, "the peer's payload behind its FIN is intact");

    // One poll lets the peer's ACK retire kumo to Closed; the orderly close must still
    // read as EOF, never as a reset.
    run_wire(&mut server, &mut peer, 50);
    let eof = c.at(&mut server, Op::Recv, s, 64, &[]);
    assert_eq!(
        (eof.status, eof.payload.is_empty()),
        (Status::Ok, true),
        "an orderly close behind the peer's FIN is EOF, not ConnReset"
    );
}
// ---- NET11: DHCP configuration (host half against a fake isolated server) ------------------

const DHCP_SERVER_PORT: u16 = 6767;
const DHCP_CLIENT_PORT: u16 = 6868;
const LEASE_IP: [u8; 4] = [192, 168, 0, 150];

/// One step of the netstack clock. The wire interface's DHCP client dispatches into the
/// device and consumes what the fake server pushed; no peer stack is involved.
fn run_dhcp(server: &mut Server, ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    for _ in 0..ms {
        clock::advance(MS);
        server.poll(clock::now(), &mut out);
    }
    out
}

/// The fake server's DHCP reply, emitted with the crate's own repr so the packet shape is
/// the wire's, not a hand-rolled approximation.
fn dhcp_reply(ack: bool, xid: u32, chaddr: [u8; 6], include_server_id: bool) -> Vec<u8> {
    use kumo_netstack::smoltcp::wire::{
        DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress, Ipv4Address,
    };
    let server = Ipv4Address::from(PEER_IP);
    let repr = DhcpRepr {
        message_type: if ack {
            DhcpMessageType::Ack
        } else {
            DhcpMessageType::Offer
        },
        transaction_id: xid,
        secs: 0,
        client_hardware_address: EthernetAddress(chaddr),
        client_ip: Ipv4Address::UNSPECIFIED,
        your_ip: Ipv4Address::from(LEASE_IP),
        server_ip: server,
        router: Some(server),
        subnet_mask: Some(Ipv4Address::from([255, 255, 255, 0])),
        relay_agent_ip: Ipv4Address::UNSPECIFIED,
        broadcast: false,
        requested_ip: None,
        client_identifier: Some(EthernetAddress(chaddr)),
        server_identifier: include_server_id.then_some(server),
        parameter_request_list: None,
        dns_servers: None,
        max_size: None,
        renew_duration: Some(4),
        rebind_duration: None,
        lease_duration: Some(8),
        additional_options: &[],
    };
    let mut dhcp = std::vec![0u8; repr.buffer_len()];
    repr.emit(&mut DhcpPacket::new_unchecked(&mut dhcp[..]))
        .expect("the reply is well-formed");
    udp_frame_exact(
        PEER_IP,
        [255, 255, 255, 255],
        DHCP_SERVER_PORT,
        DHCP_CLIENT_PORT,
        &dhcp,
    )
}

/// One UDP frame between arbitrary addresses; the fixed helper peers are not enough once
/// the destination is a leased address.
fn udp_frame_exact(
    src: [u8; 4],
    dst: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let checksums = ChecksumCapabilities::default();
    let udp = UdpRepr { src_port, dst_port };
    let ip = Ipv4Repr {
        src_addr: Ipv4Address::from(src),
        dst_addr: Ipv4Address::from(dst),
        next_header: IpProtocol::Udp,
        payload_len: udp.header_len() + payload.len(),
        hop_limit: 64,
    };
    let mut bytes = std::vec![0u8; ip.buffer_len() + ip.payload_len];
    let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
    ip.emit(&mut packet, &checksums);
    let mut datagram = UdpPacket::new_unchecked(packet.payload_mut());
    let s: IpAddress = Ipv4Address::from(src).into();
    let d: IpAddress = Ipv4Address::from(dst).into();
    udp.emit(
        &mut datagram,
        &s,
        &d,
        payload.len(),
        |out| out.copy_from_slice(payload),
        &checksums,
    );
    ethernet_wrap(PEER_MAC, WIRE_MAC, EthernetProtocol::Ipv4, &bytes)
}

/// An ARP reply for a (source, target) pair; the client ARPs for the peer before its
/// unicast renewals can leave.
fn arp_reply_to(src: [u8; 4], dst: [u8; 4]) -> Vec<u8> {
    use kumo_netstack::smoltcp::wire::{ArpOperation, ArpPacket, ArpRepr, EthernetAddress};
    let mut payload = std::vec![0u8; 28];
    let mut packet = ArpPacket::new_unchecked(&mut payload[..]);
    ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Reply,
        source_hardware_addr: EthernetAddress(PEER_MAC),
        source_protocol_addr: Ipv4Address::from(src),
        target_hardware_addr: EthernetAddress(WIRE_MAC),
        target_protocol_addr: Ipv4Address::from(dst),
    }
    .emit(&mut packet);
    ethernet_wrap(PEER_MAC, WIRE_MAC, EthernetProtocol::Arp, &payload)
}

/// Advance the clock, answer any ARP request with the peer's reply, and report the first
/// DHCP frame the client emitted in the window: (xid, chaddr, 1 DISCOVER / 3 REQUEST).
fn dhcp_step(server: &mut Server, ms: u64) -> Option<(u32, [u8; 6], u8)> {
    use kumo_netstack::smoltcp::wire::{DhcpMessageType, DhcpPacket, DhcpRepr};
    let mut found = None;
    for _ in 0..ms {
        run_dhcp(server, 1);
        let mut frame = [0u8; FRAME_MAX];
        while let Some(n) = server.take_wire_frame(&mut frame) {
            let f = &frame[..n];
            if f.len() >= 42 && f[12..14] == [0x08, 0x06] {
                server
                    .push_wire_frame(&arp_reply_to(PEER_IP, LEASE_IP))
                    .expect("the ARP reply is admitted");
                continue;
            }
            if found.is_some() || f.len() < 42 || f[12..14] != [0x08, 0x00] || f[23] != 17 {
                continue;
            }
            let ihl = usize::from(f[14] & 0x0F) * 4;
            let udp = 14 + ihl;
            if f.len() < udp + 8 {
                continue;
            }
            let src = u16::from_be_bytes([f[udp], f[udp + 1]]);
            let dst = u16::from_be_bytes([f[udp + 2], f[udp + 3]]);
            if src != DHCP_CLIENT_PORT || dst != DHCP_SERVER_PORT {
                continue;
            }
            let Ok(packet) = DhcpPacket::new_checked(&f[udp + 8..n]) else {
                continue;
            };
            let Ok(repr) = DhcpRepr::parse(&packet) else {
                continue;
            };
            let kind = if repr.message_type == DhcpMessageType::Discover {
                1
            } else if repr.message_type == DhcpMessageType::Request {
                3
            } else {
                0
            };
            found = Some((repr.transaction_id, repr.client_hardware_address.0, kind));
        }
    }
    found
}

fn expect_dhcp(server: &mut Server, kind: u8) -> (u32, [u8; 6]) {
    for _ in 0..6_000 {
        if let Some((xid, chaddr, k)) = dhcp_step(server, 1) {
            if k == kind {
                return (xid, chaddr);
            }
        }
    }
    panic!("no DHCP kind {kind} from the client");
}

/// Acquire a lease through the fake server; returns the wire-bound UDP socket.
fn acquire_lease(server: &mut Server, c: &mut Client) -> (u32, u16) {
    let s = c.udp(server);
    let port = c.bind(server, s, 0);
    let peer = Addr {
        ip: PEER_IP,
        port: PEER_PORT,
    };
    assert_eq!(
        c.at(server, Op::Connect, s, 0, &peer.encode()).status,
        Status::Ok
    );
    let (xid, chaddr) = expect_dhcp(server, 1);
    server
        .push_wire_frame(&dhcp_reply(false, xid, chaddr, true))
        .unwrap();
    let (xid, chaddr) = expect_dhcp(server, 3);
    server
        .push_wire_frame(&dhcp_reply(true, xid, chaddr, true))
        .unwrap();
    run_dhcp(server, 5);
    (s, port)
}

#[test]
fn a_dhcp_lease_configures_the_wire_and_carries_traffic() {
    let mut server = Server::new(clock::now());
    server.attach_wire_dhcp(clock::now(), WIRE_MAC, DHCP_SERVER_PORT, DHCP_CLIENT_PORT);
    let mut c = Client::open(&mut server);

    // Before a lease: GETNAME says 0.0.0.0, TCP CONNECT to the peer is NetUnreachable,
    // and nothing leaves the wire. The initial Deconfigured the socket reports is not a
    // lease loss and must not surface as an event.
    let s = c.udp(&mut server);
    let port = c.bind(&mut server, s, 0);
    let peer = Addr {
        ip: PEER_IP,
        port: PEER_PORT,
    };
    assert_eq!(
        c.at(&mut server, Op::Connect, s, 0, &peer.encode()).status,
        Status::Ok
    );
    let t = c.tcp(&mut server);
    assert_eq!(
        c.at(&mut server, Op::Connect, t, 0, &peer.encode()).status,
        Status::NetUnreachable
    );
    run_dhcp(&mut server, 5);
    assert!(
        server.take_wire_event().is_none(),
        "no lease events before a lease"
    );
    let names = c.at(&mut server, Op::GetName, s, 0, &[]);
    assert_eq!(&names.payload[4..8], &[0, 0, 0, 0]);

    let (s, port) = acquire_lease(&mut server, &mut c);
    let names = c.at(&mut server, Op::GetName, s, 0, &[]);
    assert_eq!(&names.payload[4..8], &LEASE_IP, "GETNAME reports the lease");
    match server.take_wire_event() {
        Some(WireEvent::DhcpLease {
            addr,
            prefix,
            router,
        }) => assert_eq!((addr, prefix, router), (LEASE_IP, 24, Some(PEER_IP))),
        other => panic!("expected one DhcpLease, got {other:?}"),
    }

    // Traffic through the leased address: ARP, the datagram, the echo back.
    let sent = c.sendto_to(&mut server, s, peer, b"dhcp says hello");
    assert_eq!((sent.status, sent.result), (Status::Ok, 15));
    let mut datagram: Option<Vec<u8>> = None;
    for _ in 0..40 {
        run_dhcp(&mut server, 1);
        let mut frame = [0u8; FRAME_MAX];
        while let Some(n) = server.take_wire_frame(&mut frame) {
            let f = &frame[..n];
            if f.len() >= 42 && f[12..14] == [0x08, 0x06] {
                server
                    .push_wire_frame(&arp_reply_to(PEER_IP, LEASE_IP))
                    .expect("the ARP reply is admitted");
                continue;
            }
            if f.len() >= 42 && f[12..14] == [0x08, 0x00] && f[23] == 17 {
                datagram = Some(f.to_vec());
                break;
            }
        }
        if datagram.is_some() {
            break;
        }
    }
    let frame = datagram.expect("the datagram leaves after ARP resolves");
    assert_eq!(&frame[0..6], &PEER_MAC);
    server
        .push_wire_frame(&udp_frame_exact(
            PEER_IP,
            LEASE_IP,
            PEER_PORT,
            port,
            b"echo back",
        ))
        .unwrap();
    let mut echoed = false;
    for _ in 0..5 {
        run_dhcp(&mut server, 1);
        let got = c.recvfrom(&mut server, s, UDP_PAYLOAD_MAX as u16);
        if got.status == Status::Ok {
            if got.payload.len() > ADDR_LEN && &got.payload[ADDR_LEN..] == b"echo back" {
                echoed = true;
                break;
            }
        }
    }
    assert!(echoed, "the leased address carries a real datagram");
}

#[test]
fn dhcp_ignores_mismatched_replies_and_answers_one_valid_offer() {
    let mut server = Server::new(clock::now());
    server.attach_wire_dhcp(clock::now(), WIRE_MAC, DHCP_SERVER_PORT, DHCP_CLIENT_PORT);
    let mut c = Client::open(&mut server);
    let s = c.udp(&mut server);
    c.bind(&mut server, s, 0);
    let (xid, chaddr) = expect_dhcp(&mut server, 1);

    // A wrong xid, a wrong chaddr and a missing server identifier are all ignored: the
    // client stays in Discovering and never REQUESTs.
    server
        .push_wire_frame(&dhcp_reply(false, xid ^ 0xFFFF_FFFF, chaddr, true))
        .unwrap();
    assert!(
        dhcp_step(&mut server, 1_000).is_none(),
        "a wrong-xid OFFER must not be answered"
    );
    let mut bad_mac = chaddr;
    bad_mac[5] ^= 0x5A;
    server
        .push_wire_frame(&dhcp_reply(false, xid, bad_mac, true))
        .unwrap();
    assert!(
        dhcp_step(&mut server, 1_000).is_none(),
        "a wrong-chaddr OFFER must not be answered"
    );
    server
        .push_wire_frame(&dhcp_reply(false, xid, chaddr, false))
        .unwrap();
    assert!(
        dhcp_step(&mut server, 1_000).is_none(),
        "an OFFER without a server identifier must not be answered"
    );
    assert!(
        server.take_wire_event().is_none(),
        "no lease from mismatched replies"
    );

    // The valid OFFER is the one that is answered.
    server
        .push_wire_frame(&dhcp_reply(false, xid, chaddr, true))
        .unwrap();
    let (rx, rchaddr) = expect_dhcp(&mut server, 3);
    assert_eq!((rx, rchaddr), (xid, chaddr));
    server
        .push_wire_frame(&dhcp_reply(true, rx, rchaddr, true))
        .unwrap();
    run_dhcp(&mut server, 5);
    assert!(matches!(
        server.take_wire_event(),
        Some(WireEvent::DhcpLease { .. })
    ));
}

#[test]
fn dhcp_renews_then_expires_and_deconfigures() {
    let mut server = Server::new(clock::now());
    server.attach_wire_dhcp(clock::now(), WIRE_MAC, DHCP_SERVER_PORT, DHCP_CLIENT_PORT);
    let mut c = Client::open(&mut server);
    let (s, _) = acquire_lease(&mut server, &mut c);
    assert!(matches!(
        server.take_wire_event(),
        Some(WireEvent::DhcpLease { .. })
    ));

    // T1: the client renews (unicast; dhcp_step answers the ARP the unicast needs).
    let (xid, chaddr) = expect_dhcp(&mut server, 3);
    server
        .push_wire_frame(&dhcp_reply(true, xid, chaddr, true))
        .unwrap();
    run_dhcp(&mut server, 5);
    match server.take_wire_event() {
        Some(WireEvent::DhcpRenewed { addr, .. }) => assert_eq!(addr, LEASE_IP),
        other => panic!("expected a renewal, got {other:?}"),
    }

    // Silence: the lease lapses, the address clears, and the client re-DISCOVERs. The
    // window drains the wire ring, so the client's own post-expiry DISCOVER is observed.
    let (mut expired, mut rediscovered) = (false, false);
    for _ in 0..12_000 {
        if let Some((_, _, 1)) = dhcp_step(&mut server, 1) {
            rediscovered = true;
        }
        if let Some(WireEvent::DhcpExpired) = server.take_wire_event() {
            expired = true;
        }
        if expired && rediscovered {
            break;
        }
    }
    assert!(expired, "the lease expired without a server");
    assert!(rediscovered, "a fresh DISCOVER follows the expiry");
    let names = c.at(&mut server, Op::GetName, s, 0, &[]);
    assert_eq!(
        &names.payload[4..8],
        &[0, 0, 0, 0],
        "the address is cleared"
    );
    let peer = Addr {
        ip: PEER_IP,
        port: PEER_PORT,
    };
    let t = c.tcp(&mut server);
    assert_eq!(
        c.at(&mut server, Op::Connect, t, 0, &peer.encode()).status,
        Status::NetUnreachable
    );
}

#[test]
fn no_dhcp_server_is_bounded_and_safe() {
    let mut server = Server::new(clock::now());
    server.attach_wire_dhcp(clock::now(), WIRE_MAC, DHCP_SERVER_PORT, DHCP_CLIENT_PORT);
    let mut c = Client::open(&mut server);
    let s = c.udp(&mut server);
    c.bind(&mut server, s, 0);

    // No replies at all for 25 s: the client retries on its own bounded schedule.
    let mut discovers = 0u32;
    for _ in 0..25_000 {
        if let Some((_, _, 1)) = dhcp_step(&mut server, 1) {
            discovers += 1;
        }
    }
    assert!(
        (2..=4).contains(&discovers),
        "bounded DISCOVER retries, got {discovers}"
    );
    assert!(
        server.take_wire_event().is_none(),
        "no lease without a server"
    );

    // The stack still serves: loopback answers and names itself; the wire refuses.
    let l = c.udp(&mut server);
    let lport = c.bind(&mut server, l, 0);
    let lo = Addr {
        ip: [127, 0, 0, 1],
        port: lport,
    };
    assert_eq!(
        c.at(&mut server, Op::Connect, l, 0, &lo.encode()).status,
        Status::Ok
    );
    let names = c.at(&mut server, Op::GetName, l, 0, &[]);
    assert_eq!(&names.payload[4..8], &[127, 0, 0, 1]);
    let peer = Addr {
        ip: PEER_IP,
        port: PEER_PORT,
    };
    let t = c.tcp(&mut server);
    assert_eq!(
        c.at(&mut server, Op::Connect, t, 0, &peer.encode()).status,
        Status::NetUnreachable
    );
}

#[test]
fn a_generation_change_discards_the_lease() {
    let mut server = Server::new(clock::now());
    server.attach_wire_dhcp(clock::now(), WIRE_MAC, DHCP_SERVER_PORT, DHCP_CLIENT_PORT);
    let mut c = Client::open(&mut server);
    let (s, _) = acquire_lease(&mut server, &mut c);
    assert!(matches!(
        server.take_wire_event(),
        Some(WireEvent::DhcpLease { .. })
    ));

    // A new provider generation never silently keeps the lease (NET11).
    server.attach_wire_dhcp(clock::now(), WIRE_MAC, DHCP_SERVER_PORT, DHCP_CLIENT_PORT);
    assert!(matches!(
        server.take_wire_event(),
        Some(WireEvent::DhcpDiscardedByGeneration)
    ));
    let names = c.at(&mut server, Op::GetName, s, 0, &[]);
    assert_eq!(
        &names.payload[4..8],
        &[0, 0, 0, 0],
        "the leased address is gone"
    );

    // The new generation starts from Discovering, exactly like a cold attach.
}
