//j815

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
        let mut payload = Addr { ip: LOOPBACK, port }.encode().to_vec();
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
    assert_eq!(
        c.call(&mut server, Op::Socket, 0, 2, &[]).status,
        Status::Unsupported
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
