#![no_std]
#![no_main]
//j815
//j816
//j817
//j820
//j822

//! `netprobe` — drive netstack's socket contract over a real granted channel (PLAN/020 NET2b).
//!
//! `net netprobe udp`: two UDP sockets in one session exchange exact datagrams on 127.0.0.1.
//! `net netprobe errors`: deterministic refusals, the 16-socket quota, then a malformed header
//! that must close the session.
//! `bg net netprobe recv <port> <n>` + `net netprobe send <port> <n>`: two separately launched
//! processes, each with its own session, exchange `n` datagrams; the receiver echoes each one
//! and the sender checks the echo is exact (NET2b-2).
//! `bg net netprobe tcpserve <port>` + `net netprobe tcpclient <port>`: a 12000-byte stream,
//! echoed back exact across partial writes, then half-close and EOF (NET2c).
//! `net netprobe restartwait` / `restartfresh <epoch>`: old TCP session death and fresh state.
//! `net netprobe tcperrors`: refusal, would-block, deadline, a full send buffer and EOF. Run without `net`, it reports that it holds no network grant —
//! the grant is a launcher decision, not something a program can ask for.

extern crate alloc;

use kumo_abi::{unpack_argv, Errno, Handle};
use kumo_netstack::budget::{SOCKETS_PER_SESSION, UDP_PAYLOAD_MAX};
use kumo_netstack::socket::{
    decode_reply, encode_request, Addr, Op, RequestHeader, Status, ADDR_LEN, HEADER_LEN,
    WAIT_CONNECTED, WAIT_READABLE, WAIT_WRITABLE,
};
use kumo_rt::{channel_read, channel_write, debug_write, process_exit, startup, vmo_read};

kumo_rt::entry!(main);

const LOOPBACK: [u8; 4] = [127, 0, 0, 1];
/// The largest legal request or reply (DESIGN/026: header + address + 2048 B stream payload,
/// rounded up). Buffers stay this size because a native child's stack is 16 KiB.
const MESSAGE_MAX: usize = 2112;

struct Out(Handle);

impl Out {
    fn write(&self, bytes: &[u8]) {
        if self.0 .0 == 0 {
            debug_write(bytes.as_ptr(), bytes.len());
        } else {
            let _ = channel_write(self.0, bytes.as_ptr(), bytes.len());
        }
    }

    fn line(&self, parts: &[&[u8]]) {
        let mut buf = [0u8; 160];
        let mut at = 0;
        for part in parts.iter().chain([&b"\n"[..]].iter()) {
            let take = part.len().min(buf.len() - at);
            buf[at..at + take].copy_from_slice(&part[..take]);
            at += take;
        }
        self.write(&buf[..at]);
    }
}

struct Dec([u8; 20], usize);

fn dec(mut value: u64) -> Dec {
    let mut digits = [0u8; 20];
    let mut len = 0;
    loop {
        digits[19 - len] = b'0' + (value % 10) as u8;
        value /= 10;
        len += 1;
        if value == 0 {
            break;
        }
    }
    Dec(digits, len)
}

impl Dec {
    fn bytes(&self) -> &[u8] {
        &self.0[20 - self.1..]
    }
}

fn hex32(value: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    for (i, byte) in out.iter_mut().enumerate() {
        let nibble = (value >> (28 - 4 * i)) & 0xF;
        *byte = b"0123456789abcdef"[nibble as usize];
    }
    out
}

/// FNV-1a, so a reader can compare a long payload by one word on each side.
fn fnv(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, &byte| {
        (hash ^ byte as u32).wrapping_mul(0x0100_0193)
    })
}

fn status_name(status: Status) -> &'static [u8] {
    match status {
        Status::Ok => b"Ok",
        Status::BadSocket => b"BadSocket",
        Status::QuotaExceeded => b"QuotaExceeded",
        Status::WouldBlock => b"WouldBlock",
        Status::TimedOut => b"TimedOut",
        Status::MessageTooLarge => b"MessageTooLarge",
        Status::Unsupported => b"Unsupported",
        Status::NotConnected => b"NotConnected",
        Status::AddrInUse => b"AddrInUse",
        Status::NetUnreachable => b"NetUnreachable",
        Status::InProgress => b"InProgress",
        Status::ConnRefused => b"ConnRefused",
        Status::ConnReset => b"ConnReset",
        Status::ShutDown => b"ShutDown",
        _ => b"(other)",
    }
}

/// A decoded reply; its payload stays in the client's one receive buffer (`Client::payload`),
/// so no reply-sized copy travels up the stack.
#[derive(Clone, Copy)]
struct Reply {
    status: Status,
    socket: u32,
    result: u64,
    len: usize,
}

struct Client {
    channel: Handle,
    generation: u32,
    next_id: u32,
    out: Out,
    failures: u32,
    /// The single receive buffer; `Client` lives on the heap because the 16 KiB default stack
    /// cannot also hold reply-sized buffers in every frame (J815 QEMU stack fault).
    rx: [u8; MESSAGE_MAX],
}

impl Client {
    /// Send raw bytes and read the one reply; `None` when netstack closed the session.
    fn exchange(&mut self, request: &[u8], id: u32) -> Option<Reply> {
        if channel_write(self.channel, request.as_ptr(), request.len()) != 0 {
            return None;
        }
        let n = channel_read(self.channel, self.rx.as_mut_ptr(), self.rx.len()) as usize;
        if n == 0 || n > self.rx.len() {
            return None;
        }
        let reply = decode_reply(&self.rx[..n], 0).ok()?;
        let (status, socket, result, len) = (
            reply.status,
            reply.socket,
            reply.result,
            reply.payload.len(),
        );
        let (request_id, generation) = (reply.request_id, reply.generation);
        if request_id != id || (self.generation != 0 && generation != self.generation) {
            self.fail(&[b"reply does not match its request"]);
            return None;
        }
        if self.generation == 0 {
            self.generation = generation;
        }
        Some(Reply {
            status,
            socket,
            result,
            len,
        })
    }

    /// The payload of the reply most recently returned.
    fn payload(&self, reply: &Reply) -> &[u8] {
        &self.rx[HEADER_LEN..HEADER_LEN + reply.len]
    }

    fn call(&mut self, op: Op, socket: u32, arg: u64, payload: &[u8]) -> Reply {
        let id = self.next_id;
        self.next_id += 1;
        let header = RequestHeader {
            op,
            request_id: id,
            socket,
            generation: self.generation,
            flags: 0,
            arg,
        };
        let mut buf = [0u8; MESSAGE_MAX];
        let Some(len) = encode_request(&header, payload, &mut buf) else {
            self.out.line(&[b"netprobe: FAIL request would not encode"]);
            process_exit(1);
        };
        self.exchange(&buf[..len], id).unwrap_or_else(|| {
            self.out
                .line(&[b"netprobe: FAIL session closed unexpectedly"]);
            process_exit(1)
        })
    }

    fn fail(&mut self, parts: &[&[u8]]) {
        self.failures += 1;
        let mut line = [&b"netprobe: FAIL "[..]; 6];
        let n = parts.len().min(5);
        line[1..1 + n].copy_from_slice(&parts[..n]);
        self.out.line(&line[..1 + n]);
    }

    fn expect(&mut self, what: &[u8], got: Status, want: Status) {
        if got == want {
            self.out
                .line(&[b"netprobe: ", what, b" -> ", status_name(got), b" ok"]);
        } else {
            self.fail(&[
                what,
                b" -> ",
                status_name(got),
                b" want ",
                status_name(want),
            ]);
        }
    }

    fn udp_socket(&mut self) -> u32 {
        let reply = self.call(Op::Socket, 0, 1, &[]);
        self.expect(b"socket udp", reply.status, Status::Ok);
        reply.socket
    }

    fn bind(&mut self, socket: u32, port: u16) -> u16 {
        let reply = self.call(Op::Bind, socket, 0, &Addr { ip: LOOPBACK, port }.encode());
        self.expect(b"bind", reply.status, Status::Ok);
        reply.result as u16
    }

    fn sendto(&mut self, socket: u32, port: u16, data: &[u8]) -> Status {
        let mut payload = [0u8; ADDR_LEN + UDP_PAYLOAD_MAX];
        payload[..ADDR_LEN].copy_from_slice(&Addr { ip: LOOPBACK, port }.encode());
        payload[ADDR_LEN..ADDR_LEN + data.len()].copy_from_slice(data);
        self.call(Op::SendTo, socket, 0, &payload[..ADDR_LEN + data.len()])
            .status
    }

    /// WAIT for a datagram (2 s), then RECVFROM it and check bytes and source.
    fn receive_exact(&mut self, socket: u32, data: &[u8], from_port: u16) {
        let wait = self.call(
            Op::Wait,
            socket,
            WAIT_READABLE as u64 | (2000u64 << 32),
            &[],
        );
        self.expect(b"wait readable", wait.status, Status::Ok);
        let got = self.call(Op::RecvFrom, socket, UDP_PAYLOAD_MAX as u64, &[]);
        let source = Addr {
            ip: LOOPBACK,
            port: from_port,
        }
        .encode();
        let payload = self.payload(&got);
        let body = &payload[ADDR_LEN.min(payload.len())..];
        let exact = got.status == Status::Ok
            && payload.len() >= ADDR_LEN
            && payload[..ADDR_LEN] == source
            && body == data
            && got.result == data.len() as u64;
        let len = dec(data.len() as u64);
        let hash = hex32(fnv(body));
        if exact {
            self.out.line(&[
                b"netprobe: datagram ",
                len.bytes(),
                b" B fnv=",
                &hash,
                b" from 127.0.0.1:",
                dec(from_port as u64).bytes(),
                b" exact",
            ]);
        } else {
            self.fail(&[b"datagram ", len.bytes(), b" B not exact fnv=", &hash]);
        }
    }
}

fn pattern(buf: &mut [u8; UDP_PAYLOAD_MAX], seed: u8) {
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(31).wrapping_add(seed);
    }
}

fn udp(client: &mut Client) {
    let a = client.udp_socket();
    let b = client.udp_socket();
    let port_a = client.bind(a, 7000);
    let port_b = client.bind(b, 0);
    client.out.line(&[
        b"netprobe: udp sockets bound 127.0.0.1:",
        dec(port_a as u64).bytes(),
        b" and 127.0.0.1:",
        dec(port_b as u64).bytes(),
    ]);
    let mut big = [0u8; UDP_PAYLOAD_MAX];
    pattern(&mut big, 7);
    let cases: [&[u8]; 3] = [b"", b"KUMO NET2b loopback datagram", &big];
    for data in cases {
        let status = client.sendto(b, port_a, data);
        client.expect(b"sendto", status, Status::Ok);
        client.receive_exact(a, data, port_b);
    }
    let status = client.sendto(a, port_b, b"and back again");
    client.expect(b"sendto reverse", status, Status::Ok);
    client.receive_exact(b, b"and back again", port_a);
}

fn parse_u16(text: &[u8]) -> Option<u16> {
    if text.is_empty() || text.len() > 5 {
        return None;
    }
    let mut value = 0u32;
    for &digit in text {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value * 10 + (digit - b'0') as u32;
    }
    u16::try_from(value).ok()
}

/// Datagram sizes the `send`/`recv` pair exchanges, in order.
const PEER_SIZES: [usize; 4] = [13, 0, UDP_PAYLOAD_MAX, 512];

fn peer_payload(index: usize, buf: &mut [u8; UDP_PAYLOAD_MAX]) -> usize {
    let len = PEER_SIZES[index];
    if index == 0 {
        buf[..len].copy_from_slice(b"NET2b-2 hello");
    } else {
        pattern(buf, index as u8);
    }
    len
}

/// Receive `count` datagrams on `port` and echo each to its sender.
fn recv(client: &mut Client, port: u16, count: usize) {
    let s = client.udp_socket();
    client.bind(s, port);
    client
        .out
        .line(&[b"netprobe: recv bound 127.0.0.1:", dec(port as u64).bytes()]);
    for index in 0..count {
        let wait = client.call(Op::Wait, s, WAIT_READABLE as u64 | (10_000u64 << 32), &[]);
        if wait.status != Status::Ok {
            client.fail(&[b"recv wait -> ", status_name(wait.status)]);
            return;
        }
        let got = client.call(Op::RecvFrom, s, UDP_PAYLOAD_MAX as u64, &[]);
        let payload = client.payload(&got);
        if got.status != Status::Ok || payload.len() < ADDR_LEN {
            client.fail(&[b"recvfrom -> ", status_name(got.status)]);
            return;
        }
        let mut echo = [0u8; ADDR_LEN + UDP_PAYLOAD_MAX];
        echo[..payload.len()].copy_from_slice(payload);
        let echo = &echo[..payload.len()];
        let from = u16::from_le_bytes([echo[2], echo[3]]);
        let data = &echo[ADDR_LEN..];
        let hash = hex32(fnv(data));
        client.out.line(&[
            b"netprobe: recv ",
            dec(index as u64).bytes(),
            b": ",
            dec(data.len() as u64).bytes(),
            b" B fnv=",
            &hash,
            b" from 127.0.0.1:",
            dec(from as u64).bytes(),
        ]);
        // Echo to exactly the address the datagram came from.
        let status = client.call(Op::SendTo, s, 0, echo).status;
        client.expect(b"recv echo", status, Status::Ok);
    }
}

/// Send `count` datagrams to `port` and require each echo back, exact, from that port. The
/// first datagram is retransmitted until echoed, because the receiver is a separate process
/// that may not have bound yet; after that the peer is known live and nothing is retried.
fn send(client: &mut Client, port: u16, count: usize) {
    const FIRST_TRIES: u32 = 40;
    let s = client.udp_socket();
    let local = client.bind(s, 0);
    client.out.line(&[
        b"netprobe: send from 127.0.0.1:",
        dec(local as u64).bytes(),
        b" to 127.0.0.1:",
        dec(port as u64).bytes(),
    ]);
    let mut data = [0u8; UDP_PAYLOAD_MAX];
    for index in 0..count {
        let len = peer_payload(index, &mut data);
        let tries = if index == 0 { FIRST_TRIES } else { 1 };
        let mut attempts = 0;
        let mut echoed = false;
        while attempts < tries && !echoed {
            attempts += 1;
            let status = client.sendto(s, port, &data[..len]);
            client.expect(b"send", status, Status::Ok);
            let timeout: u64 = if index == 0 { 250 } else { 2000 };
            let wait = client.call(Op::Wait, s, WAIT_READABLE as u64 | (timeout << 32), &[]);
            echoed = wait.status == Status::Ok;
        }
        if !echoed {
            client.fail(&[b"no echo for datagram ", dec(index as u64).bytes()]);
            return;
        }
        let got = client.call(Op::RecvFrom, s, UDP_PAYLOAD_MAX as u64, &[]);
        let payload = client.payload(&got);
        let source = Addr { ip: LOOPBACK, port }.encode();
        let body = &payload[ADDR_LEN.min(payload.len())..];
        let hash = hex32(fnv(body));
        let exact = got.status == Status::Ok
            && payload.len() >= ADDR_LEN
            && payload[..ADDR_LEN] == source
            && body == &data[..len];
        if exact {
            client.out.line(&[
                b"netprobe: send ",
                dec(index as u64).bytes(),
                b": ",
                dec(len as u64).bytes(),
                b" B fnv=",
                &hash,
                b" echoed exact by 127.0.0.1:",
                dec(port as u64).bytes(),
                b" after ",
                dec(attempts as u64).bytes(),
                b" send(s)",
            ]);
        } else {
            client.fail(&[
                b"echo ",
                dec(index as u64).bytes(),
                b" not exact fnv=",
                &hash,
            ]);
        }
    }
}

/// The NET2c stream: 12000 bytes generated from their index, so neither side holds it whole.
const STREAM_TOTAL: usize = 12_000;
const STREAM_CHUNK: usize = 2048;
const FNV_START: u32 = 0x811c_9dc5;

fn stream_byte(index: usize) -> u8 {
    (index as u8)
        .wrapping_mul(13)
        .wrapping_add((index >> 8) as u8)
}

fn fnv_step(hash: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(hash, |hash, &byte| {
        (hash ^ byte as u32).wrapping_mul(0x0100_0193)
    })
}

fn wait(client: &mut Client, socket: u32, interest: u8, ms: u64) -> Status {
    client
        .call(Op::Wait, socket, interest as u64 | (ms << 32), &[])
        .status
}

fn tcp_socket(client: &mut Client) -> u32 {
    let reply = client.call(Op::Socket, 0, 2, &[]);
    client.expect(b"socket tcp", reply.status, Status::Ok);
    reply.socket
}

fn connect(client: &mut Client, socket: u32, port: u16) -> Status {
    client
        .call(
            Op::Connect,
            socket,
            0,
            &Addr { ip: LOOPBACK, port }.encode(),
        )
        .status
}

/// Accept one connection on `port`, echo everything until the client half-closes, then close.
fn tcp_serve(client: &mut Client, port: u16) {
    let listener = tcp_socket(client);
    client.bind(listener, port);
    let got = client.call(Op::Listen, listener, 2, &[]).status;
    client.expect(b"listen", got, Status::Ok);
    client.out.line(&[
        b"netprobe: tcp listening on 127.0.0.1:",
        dec(port as u64).bytes(),
    ]);
    if wait(client, listener, WAIT_CONNECTED, 15_000) != Status::Ok {
        client.fail(&[b"no connection to accept"]);
        return;
    }
    let accepted = client.call(Op::Accept, listener, 0, &[]);
    client.expect(b"accept", accepted.status, Status::Ok);
    let stream = accepted.socket;
    let peer = client.payload(&accepted);
    let peer_port = if peer.len() >= ADDR_LEN {
        u16::from_le_bytes([peer[2], peer[3]])
    } else {
        0
    };
    client.out.line(&[
        b"netprobe: tcp accepted 127.0.0.1:",
        dec(peer_port as u64).bytes(),
    ]);
    let (mut total, mut hash) = (0usize, FNV_START);
    let mut chunk = [0u8; 700];
    loop {
        let got = client.call(Op::Recv, stream, chunk.len() as u64, &[]);
        match got.status {
            Status::Ok if got.len == 0 => break,
            Status::Ok => {
                let n = got.len;
                chunk[..n].copy_from_slice(client.payload(&got));
                hash = fnv_step(hash, &chunk[..n]);
                total += n;
                let mut off = 0;
                while off < n {
                    let sent = client.call(Op::Send, stream, 0, &chunk[off..n]);
                    match sent.status {
                        Status::Ok => off += sent.result as usize,
                        Status::WouldBlock => {
                            if wait(client, stream, WAIT_WRITABLE, 2000) != Status::Ok {
                                client.fail(&[b"echo stalled"]);
                                return;
                            }
                        }
                        other => {
                            client.fail(&[b"echo send -> ", status_name(other)]);
                            return;
                        }
                    }
                }
            }
            Status::WouldBlock => {
                if wait(client, stream, WAIT_READABLE, 5000) != Status::Ok {
                    client.fail(&[b"stream went quiet before EOF"]);
                    return;
                }
            }
            other => {
                client.fail(&[b"recv -> ", status_name(other)]);
                return;
            }
        }
    }
    client.out.line(&[
        b"netprobe: tcp echoed ",
        dec(total as u64).bytes(),
        b" B fnv=",
        &hex32(hash),
        b", EOF after the client's half-close",
    ]);
    let closed = client.call(Op::Close, stream, 0, &[]).status;
    client.expect(b"close stream", closed, Status::Ok);
    let closed = client.call(Op::Close, listener, 0, &[]).status;
    client.expect(b"close listener", closed, Status::Ok);
}

/// Connect (retrying while the server starts), stream STREAM_TOTAL bytes while reading the echo,
/// half-close, read to EOF, and require the echo byte-for-byte.
fn tcp_client(client: &mut Client, port: u16) {
    let pause = client.udp_socket();
    client.bind(pause, 0);
    let mut stream = 0;
    let mut tries = 0;
    for attempt in 1..=50u64 {
        tries = attempt;
        let socket = tcp_socket(client);
        let status = connect(client, socket, port);
        if status == Status::InProgress && wait(client, socket, WAIT_CONNECTED, 1000) == Status::Ok
        {
            stream = socket;
            break;
        }
        // Refused (the server is not listening yet): drop the socket, pause, try again.
        let _ = client.call(Op::Close, socket, 0, &[]);
        let _ = wait(client, pause, WAIT_READABLE, 100);
    }
    if stream == 0 {
        client.fail(&[b"never connected"]);
        return;
    }
    client.out.line(&[
        b"netprobe: tcp connected to 127.0.0.1:",
        dec(port as u64).bytes(),
        b" after ",
        dec(tries).bytes(),
        b" attempt(s)",
    ]);
    let (mut sent, mut got, mut sends, mut partial) = (0usize, 0usize, 0u64, 0u64);
    let (mut hash, mut exact, mut shut, mut eof) = (FNV_START, true, false, false);
    let mut chunk = [0u8; STREAM_CHUNK];
    for _ in 0..20_000 {
        let mut progress = false;
        if sent < STREAM_TOTAL {
            let n = (STREAM_TOTAL - sent).min(STREAM_CHUNK);
            for (i, byte) in chunk[..n].iter_mut().enumerate() {
                *byte = stream_byte(sent + i);
            }
            let reply = client.call(Op::Send, stream, 0, &chunk[..n]);
            match reply.status {
                Status::Ok => {
                    sends += 1;
                    let accepted = reply.result as usize;
                    if accepted < n {
                        partial += 1;
                    }
                    sent += accepted;
                    progress |= accepted > 0;
                }
                Status::WouldBlock => {}
                other => {
                    client.fail(&[b"send -> ", status_name(other)]);
                    return;
                }
            }
        } else if !shut {
            let status = client.call(Op::Shutdown, stream, 2, &[]).status;
            client.expect(b"shutdown write (half-close)", status, Status::Ok);
            shut = true;
        }
        let reply = client.call(Op::Recv, stream, STREAM_CHUNK as u64, &[]);
        match reply.status {
            Status::Ok if reply.len == 0 => {
                eof = true;
                break;
            }
            Status::Ok => {
                let data = client.payload(&reply);
                exact &= data
                    .iter()
                    .enumerate()
                    .all(|(i, &byte)| byte == stream_byte(got + i));
                hash = fnv_step(hash, data);
                got += data.len();
                progress = true;
            }
            Status::WouldBlock => {}
            other => {
                client.fail(&[b"recv -> ", status_name(other)]);
                return;
            }
        }
        if !progress {
            let interest = if sent < STREAM_TOTAL {
                WAIT_READABLE | WAIT_WRITABLE
            } else {
                WAIT_READABLE
            };
            if wait(client, stream, interest, 2000) != Status::Ok {
                client.fail(&[b"stream stalled"]);
                return;
            }
        }
    }
    let (sent_d, sends_d, partial_d, got_d, hash_x) = (
        dec(sent as u64),
        dec(sends),
        dec(partial),
        dec(got as u64),
        hex32(hash),
    );
    let line: [&[u8]; 11] = [
        b"netprobe: tcp sent ",
        sent_d.bytes(),
        b" B in ",
        sends_d.bytes(),
        b" SENDs (",
        partial_d.bytes(),
        b" partial); echo ",
        got_d.bytes(),
        b" B fnv=",
        &hash_x,
        if exact && eof {
            b" exact, then EOF"
        } else {
            b" NOT exact"
        },
    ];
    client.out.line(&line);
    if !(exact && eof && got == STREAM_TOTAL && partial > 0) {
        client.fail(&[b"stream not exact, no EOF, or no partial write"]);
    }
    let status = client.call(Op::Close, stream, 0, &[]).status;
    client.expect(b"close", status, Status::Ok);
}

/// TCP refusals and edges inside one session.
fn tcp_errors(client: &mut Client) {
    let lonely = tcp_socket(client);
    let status = connect(client, lonely, 8999);
    client.expect(b"connect to 8999 (no listener)", status, Status::InProgress);
    let status = wait(client, lonely, WAIT_CONNECTED, 2000);
    client.expect(b"wait connected", status, Status::ConnRefused);

    let listener = tcp_socket(client);
    client.bind(listener, 8998);
    let status = client.call(Op::Listen, listener, 1, &[]).status;
    client.expect(b"listen backlog 1", status, Status::Ok);
    let status = client.call(Op::Accept, listener, 0, &[]).status;
    client.expect(b"accept with none pending", status, Status::WouldBlock);
    let near = tcp_socket(client);
    let status = connect(client, near, 8998);
    client.expect(b"connect to 8998", status, Status::InProgress);
    let status = wait(client, near, WAIT_CONNECTED, 2000);
    client.expect(b"wait connected", status, Status::Ok);
    let status = wait(client, listener, WAIT_CONNECTED, 2000);
    client.expect(b"wait acceptable", status, Status::Ok);
    let far = client.call(Op::Accept, listener, 0, &[]);
    client.expect(b"accept", far.status, Status::Ok);
    let far = far.socket;

    let status = client.call(Op::Recv, far, 64, &[]).status;
    client.expect(b"recv with nothing sent", status, Status::WouldBlock);
    let status = wait(client, far, WAIT_READABLE, 50);
    client.expect(b"wait readable 50ms", status, Status::TimedOut);

    let block = [0x5Au8; STREAM_CHUNK];
    let mut filled = Status::Ok;
    for _ in 0..20 {
        filled = client.call(Op::Send, near, 0, &block).status;
        if filled == Status::WouldBlock {
            break;
        }
        let _ = wait(client, far, WAIT_READABLE, 20);
    }
    client.expect(b"send into an unread stream", filled, Status::WouldBlock);

    let status = client.call(Op::Close, near, 0, &[]).status;
    client.expect(b"close the sender", status, Status::Ok);
    let mut drained = 0usize;
    let mut end = Status::TimedOut;
    for _ in 0..200 {
        let reply = client.call(Op::Recv, far, STREAM_CHUNK as u64, &[]);
        match reply.status {
            Status::Ok if reply.len == 0 => {
                end = Status::Ok;
                break;
            }
            Status::Ok => drained += reply.len,
            Status::WouldBlock => {
                let _ = wait(client, far, WAIT_READABLE, 1000);
            }
            other => {
                end = other;
                break;
            }
        }
    }
    client.out.line(&[
        b"netprobe: drained ",
        dec(drained as u64).bytes(),
        b" B after the peer closed",
    ]);
    client.expect(b"recv after peer close (EOF)", end, Status::Ok);
}

/// Keep a live TCP connection and an unanswered WAIT across a server death. Closure is a
/// transport fact, not a synthetic success from the codec. No application request is replayed.
/// — TIMBERDOODLE
fn restart_wait(client: &mut Client) {
    let listener = tcp_socket(client);
    client.bind(listener, 8997);
    let status = client.call(Op::Listen, listener, 1, &[]).status;
    client.expect(b"restart listen", status, Status::Ok);
    let near = tcp_socket(client);
    let status = connect(client, near, 8997);
    client.expect(b"restart connect", status, Status::InProgress);
    let status = wait(client, near, WAIT_CONNECTED, 2000);
    client.expect(b"restart connected", status, Status::Ok);
    let status = wait(client, listener, WAIT_CONNECTED, 2000);
    client.expect(b"restart acceptable", status, Status::Ok);
    let far = client.call(Op::Accept, listener, 0, &[]);
    client.expect(b"restart accept", far.status, Status::Ok);
    let id = client.next_id;
    client.next_id += 1;
    let mut request = [0u8; HEADER_LEN];
    let header = RequestHeader {
        op: Op::Wait,
        request_id: id,
        socket: far.socket,
        generation: client.generation,
        flags: 0,
        arg: WAIT_READABLE as u64 | (60_000u64 << 32),
    };
    let len = encode_request(&header, &[], &mut request).unwrap_or(0);
    if len == 0 || channel_write(client.channel, request.as_ptr(), len) != 0 {
        client.fail(&[b"restart WAIT did not send"]);
        return;
    }
    client
        .out
        .line(&[b"netprobe: restartwait armed (live TCP, pending WAIT)"]);
    let read = channel_read(client.channel, client.rx.as_mut_ptr(), client.rx.len());
    let write = channel_write(client.channel, request.as_ptr(), len);
    if read == 0 && write == Errno::PeerClosed.status() {
        client
            .out
            .line(&[b"netprobe: old session -> PeerClosed; no replay ok"]);
    } else {
        client.fail(&[b"old session was not PeerClosed"]);
    }
}

/// A new launch must have a new instance generation, empty socket state and reusable binds.
fn restart_fresh(client: &mut Client, epoch: u16) {
    if client.generation >> 16 != u32::from(epoch) || epoch < 2 {
        client.fail(&[b"replacement reused the old instance generation"]);
    }
    let status = client.call(Op::Close, 1, 0, &[]).status;
    client.expect(b"old socket id on fresh session", status, Status::BadSocket);
    let listener = tcp_socket(client);
    client.bind(listener, 8997);
    let status = client.call(Op::Listen, listener, 1, &[]).status;
    client.expect(b"old TCP bind reused", status, Status::Ok);
    udp(client);
    let id = client.next_id;
    client.next_id += 1;
    let header = RequestHeader {
        op: Op::Close,
        request_id: id,
        socket: listener,
        generation: ((u32::from(epoch) - 1) << 16) | 1,
        flags: 0,
        arg: 0,
    };
    let mut request = [0u8; HEADER_LEN];
    let len = encode_request(&header, &[], &mut request).unwrap_or(0);
    let status = client.exchange(&request[..len], id).map(|r| r.status);
    if status == Some(Status::StaleSession) {
        client
            .out
            .line(&[b"netprobe: old generation -> StaleSession ok"]);
    } else {
        client.fail(&[b"old generation was accepted"]);
    }
}

fn errors(client: &mut Client) {
    let s = client.udp_socket();
    let port = client.bind(s, 0);
    let got = client.call(Op::RecvFrom, s, 64, &[]).status;
    client.expect(b"recvfrom empty", got, Status::WouldBlock);
    let got = client
        .call(Op::Wait, s, WAIT_READABLE as u64 | (50u64 << 32), &[])
        .status;
    client.expect(b"wait 50ms", got, Status::TimedOut);
    let got = client.call(Op::Close, 999, 0, &[]).status;
    client.expect(b"close unknown socket", got, Status::BadSocket);
    let got = client.call(Op::Listen, s, 1, &[]).status;
    client.expect(b"listen on udp", got, Status::Unsupported);
    let elsewhere = Addr {
        ip: [10, 0, 0, 1],
        port: 9,
    };
    let got = client.call(Op::SendTo, s, 0, &elsewhere.encode()).status;
    client.expect(b"sendto 10.0.0.1", got, Status::NetUnreachable);

    // The client encoder refuses an oversize datagram, so build it by hand: netstack must
    // refuse it too, rather than let smoltcp accept and silently drop it (J813).
    let id = client.next_id;
    client.next_id += 1;
    let header = RequestHeader {
        op: Op::SendTo,
        request_id: id,
        socket: s,
        generation: client.generation,
        flags: 0,
        arg: 0,
    };
    let mut raw = [0u8; HEADER_LEN + ADDR_LEN + UDP_PAYLOAD_MAX + 1];
    let addr = Addr { ip: LOOPBACK, port }.encode();
    let len = encode_request(&header, &addr, &mut raw).unwrap_or(0);
    let total = raw.len();
    raw[22..24].copy_from_slice(&((total - HEADER_LEN) as u16).to_le_bytes());
    let got = match (len, client.exchange(&raw, id)) {
        (0, _) | (_, None) => Status::ProtocolViolation,
        (_, Some(reply)) => reply.status,
    };
    client.expect(b"sendto 1473 bytes", got, Status::MessageTooLarge);

    // Quota: this session holds one socket; fifteen more fill it, the next is refused.
    for _ in 1..SOCKETS_PER_SESSION {
        let reply = client.call(Op::Socket, 0, 1, &[]);
        if reply.status != Status::Ok {
            client.fail(&[b"socket under quota -> ", status_name(reply.status)]);
        }
    }
    let got = client.call(Op::Socket, 0, 1, &[]).status;
    client.expect(b"socket 17", got, Status::QuotaExceeded);

    // An untrusted header: no reply, and netstack closes the session.
    let bad = [0u8; HEADER_LEN];
    if client.exchange(&bad, 0).is_none() {
        client
            .out
            .line(&[b"netprobe: malformed header -> session closed by netstack ok"]);
    } else {
        client.fail(&[b"malformed header was answered"]);
    }
}

#[no_mangle]
extern "C" fn main(
    bootstrap: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    let startup = startup(Handle(bootstrap as u32));
    let out = Out(startup.stdout.unwrap_or(Handle(0)));
    let mut argv_buf = [0u8; 256];
    let read = startup
        .argv
        .is_some_and(|argv| vmo_read(argv, 0, argv_buf.as_mut_ptr(), argv_buf.len()) == 0);
    let mut args = unpack_argv(if read { &argv_buf[..] } else { &[] }).skip(1);
    let mode: &[u8] = args.next().unwrap_or(b"");
    let port = args.next().and_then(parse_u16);
    let count = args.next().and_then(parse_u16);
    let Some(channel) = startup.net else {
        out.line(&[b"netprobe: no network grant (launch it as `net netprobe ...`)"]);
        process_exit(2);
    };
    let mut client = alloc::boxed::Box::new(Client {
        channel,
        generation: 0,
        next_id: 1,
        out,
        failures: 0,
        rx: [0u8; MESSAGE_MAX],
    });
    let hello = client.call(Op::Hello, 0, 0, &[]);
    client.expect(b"hello", hello.status, Status::Ok);
    client.out.line(&[
        b"netprobe: session generation ",
        dec(client.generation as u64).bytes(),
        b", udp max ",
        dec(hello.result >> 48).bytes(),
        b" B, sockets ",
        dec(hello.result & 0xFFFF).bytes(),
    ]);
    match mode {
        b"udp" => udp(&mut client),
        b"hold" => {
            let s = client.udp_socket();
            client.bind(s, 0);
            let got = client
                .call(Op::Wait, s, WAIT_READABLE as u64 | (2000u64 << 32), &[])
                .status;
            client.expect(b"census hold 2s", got, Status::TimedOut);
        }
        b"errors" => errors(&mut client),
        b"recv" | b"send"
            if port.is_some_and(|p| p != 0)
                && count.is_some_and(|n| (1..=PEER_SIZES.len() as u16).contains(&n)) =>
        {
            let (port, count) = (port.unwrap_or(0), count.unwrap_or(0) as usize);
            if mode == b"recv" {
                recv(&mut client, port, count)
            } else {
                send(&mut client, port, count)
            }
        }
        b"tcpserve" if port.is_some_and(|p| p != 0) => tcp_serve(&mut client, port.unwrap_or(0)),
        b"tcpclient" if port.is_some_and(|p| p != 0) => tcp_client(&mut client, port.unwrap_or(0)),
        b"tcperrors" => tcp_errors(&mut client),
        b"restartwait" => restart_wait(&mut client),
        b"restartfresh" if port.is_some() => restart_fresh(&mut client, port.unwrap_or(0)),
        _ => {
            client.out.line(&[
                b"usage: net netprobe udp|errors|tcperrors | recv|send <port> <1-4> | tcpserve|tcpclient <port> | restartwait | restartfresh <epoch>",
            ]);
            process_exit(2);
        }
    }
    if client.failures == 0 {
        client.out.line(&[b"netprobe: ", mode, b" PASS"]);
        process_exit(0)
    } else {
        client.out.line(&[b"netprobe: ", mode, b" FAIL"]);
        process_exit(1)
    }
}
