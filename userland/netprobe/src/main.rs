#![no_std]
#![no_main]
//j815

//! `netprobe` — drive netstack's socket contract over a real granted channel (PLAN/020 NET2b).
//!
//! `net netprobe udp`: two UDP sockets in one session exchange exact datagrams on 127.0.0.1.
//! `net netprobe errors`: deterministic refusals, the 16-socket quota, then a malformed header
//! that must close the session. Run without `net`, it reports that it holds no network grant —
//! the grant is a launcher decision, not something a program can ask for.

extern crate alloc;

use kumo_abi::{unpack_argv, Handle};
use kumo_netstack::budget::{SOCKETS_PER_SESSION, UDP_PAYLOAD_MAX};
use kumo_netstack::socket::{
    decode_reply, encode_request, Addr, Op, RequestHeader, Status, ADDR_LEN, HEADER_LEN,
    WAIT_READABLE,
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
    let got = client.call(Op::Socket, 0, 2, &[]).status;
    client.expect(b"socket tcp (NET2c)", got, Status::Unsupported);
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
    let mode: &[u8] = match startup.argv {
        Some(argv) if vmo_read(argv, 0, argv_buf.as_mut_ptr(), argv_buf.len()) == 0 => {
            unpack_argv(&argv_buf).nth(1).unwrap_or(b"")
        }
        _ => b"",
    };
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
        b"errors" => errors(&mut client),
        _ => {
            client.out.line(&[b"usage: net netprobe udp|errors"]);
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
