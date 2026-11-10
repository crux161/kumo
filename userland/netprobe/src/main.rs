#![no_std]
#![no_main]
//j845
//j846
//j843
//j844
//j816
//j817

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
        Status::NotFound => b"NotFound",
        Status::NotConfigured => b"NotConfigured",
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

    /// NET8: an ICMP echo socket; its identifier is the port BIND receives.
    fn icmp_socket(&mut self) -> u32 {
        let reply = self.call(Op::Socket, 0, 3, &[]);
        self.expect(b"socket icmp", reply.status, Status::Ok);
        reply.socket
    }

    /// Bind any wire address; BIND's port is the identifier for ICMP, the port for UDP.
    fn bind_any(&mut self, socket: u32, port: u16) -> u16 {
        let reply = self.call(Op::Bind, socket, 0, &Addr { ip: [0; 4], port }.encode());
        self.expect(b"bind any", reply.status, Status::Ok);
        reply.result as u16
    }

    fn sendto(&mut self, socket: u32, port: u16, data: &[u8]) -> Status {
        self.sendto_addr(socket, Addr { ip: LOOPBACK, port }, data)
    }

    fn sendto_addr(&mut self, socket: u32, to: Addr, data: &[u8]) -> Status {
        let mut payload = [0u8; ADDR_LEN + UDP_PAYLOAD_MAX];
        payload[..ADDR_LEN].copy_from_slice(&to.encode());
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

/// NET7b: send datagrams to a LAN peer. The peer's capture is the proof; each payload is
/// the prefix, the index, then `index % 16` fill bytes, so a checker can match every frame.
fn wire(client: &mut Client, ip: [u8; 4], port: u16, count: u16) {
    let count = count.clamp(1, 64);
    let s = client.udp_socket();
    let local = client.bind(s, 0);
    client.out.line(&[
        b"netprobe: wire ",
        dec(local as u64).bytes(),
        b" -> ",
        dec(port as u64).bytes(),
        b" x",
        dec(count as u64).bytes(),
    ]);
    let mut sent = 0u32;
    for i in 0..count {
        let mut payload = [0u8; 96];
        let mut at = 0;
        let index = dec(i as u64);
        for part in [&b"kumo-net7b-"[..], index.bytes(), b"-"] {
            payload[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        }
        for _ in 0..(i % 16) {
            payload[at] = b'x';
            at += 1;
        }
        // The wire may not be up yet: autoexec runs before the NIC finishes enumerating,
        // so NetUnreachable is retried up to 60 s. A bound, unconnected socket's CONNECTED
        // wait can only time out, which makes the reply a 500 ms sleep through netstack.
        let mut attempts = 0;
        loop {
            let status = client.sendto_addr(s, Addr { ip, port }, &payload[..at]);
            if i == 0 && attempts == 0 && status != Status::Ok {
                let name = status_name(status);
                let mut line = [0u8; 48];
                const PREFIX: &[u8] = b"netprobe: wire first status=";
                line[..PREFIX.len()].copy_from_slice(PREFIX);
                let n = name.len().min(18);
                line[PREFIX.len()..PREFIX.len() + n].copy_from_slice(&name[..n]);
                line[PREFIX.len() + n] = b'\n';
                let _ = kumo_rt::log_write(
                    kumo_abi::sys::log_level::NOTICE,
                    &line[..PREFIX.len() + n + 1],
                );
            }
            if status == Status::Ok {
                sent += 1;
                break;
            }
            let retry = status == Status::NetUnreachable || status == Status::WouldBlock;
            if !retry || attempts >= 300 {
                client.fail(&[b"wire send -> ", status_name(status)]);
                break;
            }
            attempts += 1;
            // A bound, unconnected socket's CONNECTED wait can only time out, which makes
            // the reply a sleep through netstack: 500 ms for an unattached wire, 200 ms
            // while the wire socket's queue drains.
            let sleep_ms = if status == Status::WouldBlock {
                200u64
            } else {
                500u64
            };
            let _ = client.call(Op::Wait, s, WAIT_CONNECTED as u64 | (sleep_ms << 32), &[]);
        }
    }
    client.out.line(&[
        b"netprobe: wire sent ",
        dec(sent as u64).bytes(),
        b"/",
        dec(count as u64).bytes(),
    ]);
}

/// NET8: ping a LAN peer. The peer's capture is the proof; each data body is the prefix, the
/// index, then `index % 16` fill bytes, so a checker can match every request and reply.
/// `retries` bounds the wait for the wire: the autoexec runs a short foreground run that
/// fails fast (its exit triggers boot's late service wave, which starts the NIC) and a long
/// background run that is still retrying when the wave brings the wire up.
fn ping(client: &mut Client, ip: [u8; 4], count: u16, retries: u32) {
    let count = count.clamp(1, 64);
    let s = client.icmp_socket();
    let ident = client.bind_any(s, 0);
    let mut ip_text = [0u8; 15];
    let mut at = 0;
    for (i, octet) in ip.iter().enumerate() {
        if i > 0 {
            ip_text[at] = b'.';
            at += 1;
        }
        let digits = dec(*octet as u64);
        ip_text[at..at + digits.bytes().len()].copy_from_slice(digits.bytes());
        at += digits.bytes().len();
    }
    client.out.line(&[
        b"netprobe: ping ident ",
        dec(ident as u64).bytes(),
        b" -> ",
        &ip_text[..at],
        b" x",
        dec(count as u64).bytes(),
    ]);
    let mut exact = 0u32;
    for i in 0..count {
        let mut payload = [0u8; 96];
        let mut len = 0;
        let index = dec(i as u64);
        for part in [&b"kumo-net8-"[..], index.bytes(), b"-"] {
            payload[len..len + part.len()].copy_from_slice(part);
            len += part.len();
        }
        for _ in 0..(i % 16) {
            payload[len] = b'x';
            len += 1;
        }
        // The wire may not be up yet: autoexec runs before the NIC finishes enumerating, and
        // the boot's late service wave that starts the NIC only runs after the autoexec child
        // exits (the TOWER's supervisor restart). So the budget is the caller's: a short
        // foreground run triggers the wave, a long background run is still retrying when the
        // wire arrives. Clamped so a typo cannot spin forever.
        let retries = retries.clamp(1, 6000);
        let mut attempts = 0;
        loop {
            let status = client.sendto_addr(s, Addr { ip, port: 0 }, &payload[..len]);
            if i == 0 && attempts == 0 && status != Status::Ok {
                let name = status_name(status);
                let mut line = [0u8; 48];
                const PREFIX: &[u8] = b"netprobe: ping first status=";
                line[..PREFIX.len()].copy_from_slice(PREFIX);
                let n = name.len().min(18);
                line[PREFIX.len()..PREFIX.len() + n].copy_from_slice(&name[..n]);
                line[PREFIX.len() + n] = b'\n';
                let _ = kumo_rt::log_write(
                    kumo_abi::sys::log_level::NOTICE,
                    &line[..PREFIX.len() + n + 1],
                );
            }
            if status == Status::Ok {
                break;
            }
            let retry = status == Status::NetUnreachable || status == Status::WouldBlock;
            if !retry || attempts >= retries {
                client.fail(&[b"ping send -> ", status_name(status)]);
                break;
            }
            attempts += 1;
            let sleep_ms = if status == Status::WouldBlock {
                200u64
            } else {
                500u64
            };
            let _ = client.call(Op::Wait, s, WAIT_CONNECTED as u64 | (sleep_ms << 32), &[]);
        }
        if attempts >= retries {
            break;
        }
        let wait = client.call(Op::Wait, s, WAIT_READABLE as u64 | (2000u64 << 32), &[]);
        if wait.status != Status::Ok {
            client.fail(&[b"ping wait -> ", status_name(wait.status)]);
            continue;
        }
        let got = client.call(Op::RecvFrom, s, UDP_PAYLOAD_MAX as u64, &[]);
        let reply = client.payload(&got);
        let source = Addr { ip, port: 0 }.encode();
        let body = &reply[ADDR_LEN.min(reply.len())..];
        if got.status == Status::Ok
            && reply.len() >= ADDR_LEN
            && reply[..ADDR_LEN] == source
            && body == &payload[..len]
            && got.result == len as u64
        {
            exact += 1;
            client.out.line(&[
                b"netprobe: ping ",
                dec(i as u64).bytes(),
                b" exact fnv=",
                &hex32(fnv(body)),
            ]);
        } else {
            client.fail(&[
                b"ping ",
                dec(i as u64).bytes(),
                b" reply not exact fnv=",
                &hex32(fnv(body)),
            ]);
        }
    }
    client.out.line(&[
        b"netprobe: ping ",
        dec(exact as u64).bytes(),
        b"/",
        dec(count as u64).bytes(),
        b" exact",
    ]);
}

/// NET9's payload contract, shared with the Mac peer script and the pcap checker:
/// index 0 is the zero-length datagram, index 1 is the maximum unfragmented payload
/// (1472 bytes: the prefix then zeros), every other index is the prefix plus `i % 16`
/// fill bytes. `side` is `kumo` or `peer`.
fn net9_payload(buf: &mut [u8; UDP_PAYLOAD_MAX], side: &[u8], i: u16) -> usize {
    let index = dec(i as u64);
    let mut prefix = [0u8; 32];
    let mut at = 0;
    for part in [side, &b"-net9-"[..], index.bytes(), &b"-"[..]] {
        prefix[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    match i {
        0 => 0,
        1 => {
            buf[..at].copy_from_slice(&prefix[..at]);
            UDP_PAYLOAD_MAX
        }
        _ => {
            let fill = (i % 16) as usize;
            buf[..at].copy_from_slice(&prefix[..at]);
            for byte in &mut buf[at..at + fill] {
                *byte = b'x';
            }
            at + fill
        }
    }
}

/// NET9: exchange datagrams with a real peer in both directions. Send `count` numbered
/// datagrams to `<ip>:<port>`, then receive up to `count` from the peer and echo each one
/// back byte-exact. The Mac peer script verifies the initiations; the echoes prove the
/// received direction. Counters print to stdout; the peer script and the capture are the
/// authority on metal, where stdout is not on the serial console.
fn peer(client: &mut Client, ip: [u8; 4], port: u16, count: u16, retries: u32) {
    let count = count.clamp(2, 48);
    let retries = retries.clamp(1, 6000);
    let s = client.udp_socket();
    let local = client.bind_any(s, 0);
    client.out.line(&[
        b"netprobe: peer ",
        dec(local as u64).bytes(),
        b" <-> ",
        dec(port as u64).bytes(),
        b" x",
        dec(count as u64).bytes(),
    ]);
    let mut payload = [0u8; UDP_PAYLOAD_MAX];

    // Initiate: every datagram, retrying until the wire exists.
    let mut sent = 0u16;
    'outer: for i in 0..count {
        let len = net9_payload(&mut payload, b"kumo", i);
        let mut attempts = 0u32;
        loop {
            let status = client.sendto_addr(s, Addr { ip, port }, &payload[..len]);
            if status == Status::Ok {
                sent += 1;
                if i == 0 {
                    // The first datagram starts the ARP exchange and the neighbour's
                    // one-packet queue holds only it; give the reply a beat before the
                    // burst, or the next sends are dropped pre-resolution (smoltcp).
                    let _ = client.call(Op::Wait, s, WAIT_CONNECTED as u64 | (200u64 << 32), &[]);
                }
                break;
            }
            let retry = status == Status::NetUnreachable || status == Status::WouldBlock;
            if !retry || attempts >= retries {
                client.fail(&[b"peer send -> ", status_name(status)]);
                break 'outer;
            }
            attempts += 1;
            let sleep_ms = if status == Status::WouldBlock {
                200u64
            } else {
                500u64
            };
            let _ = client.call(Op::Wait, s, WAIT_CONNECTED as u64 | (sleep_ms << 32), &[]);
        }
    }

    // Echo: read up to `count` peer datagrams and send each one back byte-exact.
    let mut echo_buf = [0u8; UDP_PAYLOAD_MAX];
    let mut received = 0u16;
    let mut echoed = 0u16;
    let mut idle_waits = 0u8;
    while received < count && idle_waits < 10 {
        let wait = client.call(Op::Wait, s, WAIT_READABLE as u64 | (2000u64 << 32), &[]);
        if wait.status != Status::Ok {
            idle_waits += 1;
            continue;
        }
        let got = client.call(Op::RecvFrom, s, UDP_PAYLOAD_MAX as u64, &[]);
        let (source, body_len) = {
            let reply = client.payload(&got);
            if got.status != Status::Ok || reply.len() < ADDR_LEN {
                idle_waits += 1;
                continue;
            }
            let source = Addr {
                ip: [reply[4], reply[5], reply[6], reply[7]],
                port: u16::from_le_bytes([reply[2], reply[3]]),
            };
            let body_len = reply.len() - ADDR_LEN;
            echo_buf[..body_len].copy_from_slice(&reply[ADDR_LEN..]);
            (source, body_len)
        };
        received += 1;
        if client.sendto_addr(s, source, &echo_buf[..body_len]) == Status::Ok {
            echoed += 1;
        } else {
            client.fail(&[b"peer echo refused"]);
        }
        idle_waits = 0;
    }
    client.out.line(&[
        b"netprobe: peer sent ",
        dec(sent as u64).bytes(),
        b"/",
        dec(count as u64).bytes(),
        b" received ",
        dec(received as u64).bytes(),
        b" echoed ",
        dec(echoed as u64).bytes(),
    ]);
    if sent != count || received != count || echoed != count {
        client.fail(&[b"peer exchange incomplete"]);
    }
}

/// NET10: the wire-TCP campaign against the peer script, one phase per port:
/// `<port>` client exchange, `<port>+1` server exchange, `<port>+2` refusal,
/// `<port>+3` reset, `<port>+4` timeout and backpressure, `<port>+5` churn.
fn tcpwire(client: &mut Client, ip: [u8; 4], port: u16, retries: u32) {
    let retries = retries.clamp(1, 6000);
    let mut failures = 0u32;
    if !tcpwire_client_exchange(client, ip, port, retries) {
        failures += 1;
    }
    if !tcpwire_server_exchange(client, port + 1) {
        failures += 1;
    }
    if !tcpwire_refusal(client, ip, port + 2, retries) {
        failures += 1;
    }
    if !tcpwire_reset(client, ip, port + 3, retries) {
        failures += 1;
    }
    if !tcpwire_timeout(client, ip, port + 4, retries) {
        failures += 1;
    }
    if !tcpwire_churn(client, ip, port + 5, retries) {
        failures += 1;
    }
    client.out.line(&[
        b"netprobe: tcpwire phases failed=",
        dec(failures as u64).bytes(),
    ]);
    if failures != 0 {
        client.fail(&[b"tcpwire campaign incomplete"]);
    }
}

/// A NOTICE line on the serial console: the framebuffer alone would hide the campaign from
/// the journal's capture.
fn log_tcpwire(name: &[u8], ok: bool, bytes: u64, hash: u32) {
    let d = dec(bytes);
    let h = hex32(hash);
    let mut buf = [0u8; 128];
    let mut at = 0usize;
    for part in [
        &b"netprobe: tcpwire "[..],
        name,
        if ok { &b" exact "[..] } else { &b" FAIL "[..] },
        d.bytes(),
        &b" B fnv=0x"[..],
        &h[..],
        &b"\n"[..],
    ] {
        let take = part.len().min(buf.len() - at);
        buf[at..at + take].copy_from_slice(&part[..take]);
        at += take;
    }
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &buf[..at]);
}

/// A stall during the stream: say so on the serial console (the framebuffer FAIL is not
/// visible in a serial capture), naming the wait status and the interval number.
fn log_stall(stalls: u32, status: Status) {
    let d = dec(stalls as u64);
    let name = status_name(status);
    let mut buf = [0u8; 96];
    let mut at = 0usize;
    for part in [
        &b"netprobe: tcpwire stall "[..],
        d.bytes(),
        &b" "[..],
        name,
        &b"\n"[..],
    ] {
        let take = part.len().min(buf.len() - at);
        buf[at..at + take].copy_from_slice(&part[..take]);
        at += take;
    }
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &buf[..at]);
}

/// The framebuffer fail line is invisible in a serial capture; mirror a failing stream
/// status on the serial console with its tag.
fn log_status(tag: &[u8], status: Status) {
    let name = status_name(status);
    let mut buf = [0u8; 96];
    let mut at = 0usize;
    for part in [
        &b"netprobe: tcpwire "[..],
        tag,
        &b" -> "[..],
        name,
        &b"\n"[..],
    ] {
        let take = part.len().min(buf.len() - at);
        buf[at..at + take].copy_from_slice(&part[..take]);
        at += take;
    }
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &buf[..at]);
}

/// CONNECT to a wire address, retrying while the wire is absent and while the peer's
/// listener is not up yet. Returns the socket, or 0 after the budget or a hard error.
fn wire_connect_settled(client: &mut Client, ip: [u8; 4], port: u16, retries: u32) -> u32 {
    let mut attempts = 0u32;
    loop {
        let socket = tcp_socket(client);
        let status = client
            .call(Op::Connect, socket, 0, &Addr { ip, port }.encode())
            .status;
        if status != Status::InProgress {
            let _ = client.call(Op::Close, socket, 0, &[]);
            if status == Status::NetUnreachable && attempts < retries {
                attempts += 1;
                // A bound, unconnected TCP socket's CONNECTED wait can only time out:
                // a 500 ms sleep through netstack.
                let pause = tcp_socket(client);
                let _ = wait(client, pause, WAIT_CONNECTED, 500);
                let _ = client.call(Op::Close, pause, 0, &[]);
                continue;
            }
            client.fail(&[b"tcpwire connect -> ", status_name(status)]);
            return 0;
        }
        match wait(client, socket, WAIT_CONNECTED, 5000) {
            Status::Ok => return socket,
            Status::ConnRefused | Status::TimedOut if attempts < retries => {
                attempts += 1;
                let _ = client.call(Op::Close, socket, 0, &[]);
            }
            other => {
                client.fail(&[b"tcpwire wait -> ", status_name(other)]);
                return 0;
            }
        }
    }
}

/// Both directions carry STREAM_TOTAL bytes of the shared generator at once; each is
/// verified byte-exact and hashed. Returns whether everything held.
fn stream_both_ways(client: &mut Client, stream: u32) -> (bool, usize, u32) {
    let (mut sent, mut got, mut partial) = (0usize, 0usize, 0u64);
    let (mut hash, mut exact, mut shut, mut eof) = (FNV_START, true, false, false);
    let mut stalls = 0u32;
    let mut chunk = [0u8; STREAM_CHUNK];
    for _ in 0..200_000 {
        let mut progress = false;
        if sent < STREAM_TOTAL {
            let n = (STREAM_TOTAL - sent).min(STREAM_CHUNK);
            for (i, byte) in chunk[..n].iter_mut().enumerate() {
                *byte = stream_byte(sent + i);
            }
            let reply = client.call(Op::Send, stream, 0, &chunk[..n]);
            match reply.status {
                Status::Ok => {
                    let accepted = reply.result as usize;
                    if accepted < n {
                        partial += 1;
                    }
                    sent += accepted;
                    progress |= accepted > 0;
                }
                Status::WouldBlock => {}
                other => {
                    log_status(b"send", other);
                    client.fail(&[b"tcpwire send -> ", status_name(other)]);
                    return (false, got, hash);
                }
            }
        } else if !shut {
            let status = client.call(Op::Shutdown, stream, 2, &[]).status;
            client.expect(b"tcpwire shutdown write", status, Status::Ok);
            shut = true;
        }
        if !eof {
            let reply = client.call(Op::Recv, stream, chunk.len() as u64, &[]);
            match reply.status {
                Status::Ok if reply.len == 0 => eof = true,
                Status::Ok => {
                    let n = reply.len;
                    chunk[..n].copy_from_slice(client.payload(&reply));
                    exact &= chunk[..n]
                        .iter()
                        .enumerate()
                        .all(|(i, &byte)| byte == stream_byte(got + i));
                    hash = fnv_step(hash, &chunk[..n]);
                    got += n;
                    progress = true;
                }
                Status::WouldBlock => {}
                other => {
                    log_status(b"recv", other);
                    client.fail(&[b"tcpwire recv -> ", status_name(other)]);
                    return (false, got, hash);
                }
            }
        }
        if shut && eof {
            break;
        }
        if !progress {
            let interest = if sent < STREAM_TOTAL {
                WAIT_READABLE | WAIT_WRITABLE
            } else {
                WAIT_READABLE
            };
            let status = wait(client, stream, interest, 2000);
            if status == Status::Ok {
                stalls = 0;
            } else {
                // The peer's FIN trails its data by an RTO under loss; tolerate a
                // bounded number of intervals, and name each one on the serial.
                stalls += 1;
                log_stall(stalls, status);
                if status != Status::TimedOut || stalls >= 5 {
                    client.fail(&[b"tcpwire stream stalled"]);
                    return (false, got, hash);
                }
            }
        }
    }
    let ok = exact && eof && shut && sent == STREAM_TOTAL && got == STREAM_TOTAL;
    let hash_x = hex32(hash);
    client.out.line(&[
        b"netprobe: tcpwire sent ",
        dec(sent as u64).bytes(),
        b" B (",
        dec(partial).bytes(),
        b" partial); echo ",
        dec(got as u64).bytes(),
        b" B fnv=0x",
        &hash_x,
        if ok {
            b" exact, then EOF"
        } else {
            b" NOT exact"
        },
    ]);
    (ok, got, hash)
}

fn tcpwire_client_exchange(client: &mut Client, ip: [u8; 4], port: u16, retries: u32) -> bool {
    let stream = wire_connect_settled(client, ip, port, retries);
    if stream == 0 {
        return false;
    }
    let (ok, got, hash) = stream_both_ways(client, stream);
    let status = client.call(Op::Close, stream, 0, &[]).status;
    let ok = ok && status == Status::Ok;
    log_tcpwire(b"client", ok, got as u64, hash);
    ok
}

fn tcpwire_server_exchange(client: &mut Client, port: u16) -> bool {
    let listener = tcp_socket(client);
    client.bind_any(listener, port);
    let status = client.call(Op::Listen, listener, 2, &[]).status;
    client.expect(b"tcpwire listen", status, Status::Ok);
    // The peer connects after the wire is up; acceptable can take a while.
    if wait(client, listener, WAIT_CONNECTED, 60_000) != Status::Ok {
        client.fail(&[b"tcpwire no connection to accept"]);
        return false;
    }
    let accepted = client.call(Op::Accept, listener, 0, &[]);
    client.expect(b"tcpwire accept", accepted.status, Status::Ok);
    if accepted.status != Status::Ok {
        return false;
    }
    let stream = accepted.socket;
    let (mut total, mut hash) = (0usize, FNV_START);
    let mut chunk = [0u8; STREAM_CHUNK];
    loop {
        let reply = client.call(Op::Recv, stream, chunk.len() as u64, &[]);
        match reply.status {
            Status::Ok if reply.len == 0 => break,
            Status::Ok => {
                let n = reply.len;
                chunk[..n].copy_from_slice(client.payload(&reply));
                hash = fnv_step(hash, &chunk[..n]);
                total += n;
                let mut off = 0;
                while off < n {
                    let sent = client.call(Op::Send, stream, 0, &chunk[off..n]);
                    match sent.status {
                        Status::Ok => off += sent.result as usize,
                        Status::WouldBlock => {
                            if wait(client, stream, WAIT_WRITABLE, 2000) != Status::Ok {
                                client.fail(&[b"tcpwire echo stalled"]);
                                return false;
                            }
                        }
                        other => {
                            client.fail(&[b"tcpwire echo -> ", status_name(other)]);
                            return false;
                        }
                    }
                }
            }
            Status::WouldBlock => {
                if wait(client, stream, WAIT_READABLE, 2000) != Status::Ok {
                    client.fail(&[b"tcpwire echo read stalled"]);
                    return false;
                }
            }
            other => {
                client.fail(&[b"tcpwire server recv -> ", status_name(other)]);
                return false;
            }
        }
    }
    let _ = client.call(Op::Shutdown, stream, 2, &[]);
    let status = client.call(Op::Close, stream, 0, &[]).status;
    client.expect(b"tcpwire server close", status, Status::Ok);
    let ok = total == STREAM_TOTAL && status == Status::Ok;
    log_tcpwire(b"server", ok, total as u64, hash);
    ok
}

fn tcpwire_refusal(client: &mut Client, ip: [u8; 4], port: u16, retries: u32) -> bool {
    let mut attempts = 0u32;
    let socket = loop {
        let socket = tcp_socket(client);
        let status = client
            .call(Op::Connect, socket, 0, &Addr { ip, port }.encode())
            .status;
        if status == Status::InProgress {
            break socket;
        }
        let _ = client.call(Op::Close, socket, 0, &[]);
        if status != Status::NetUnreachable || attempts >= retries {
            client.fail(&[b"tcpwire refusal connect -> ", status_name(status)]);
            return false;
        }
        attempts += 1;
        let pause = tcp_socket(client);
        let _ = wait(client, pause, WAIT_CONNECTED, 500);
        let _ = client.call(Op::Close, pause, 0, &[]);
    };
    let status = wait(client, socket, WAIT_CONNECTED, 5000);
    let ok = status == Status::ConnRefused;
    client.expect(b"tcpwire refusal", status, Status::ConnRefused);
    log_tcpwire(b"refusal", ok, 0, 0);
    let _ = client.call(Op::Close, socket, 0, &[]);
    ok
}

fn tcpwire_reset(client: &mut Client, ip: [u8; 4], port: u16, retries: u32) -> bool {
    let stream = wire_connect_settled(client, ip, port, retries);
    if stream == 0 {
        return false;
    }
    // The peer accepts, then aborts: the RST must surface as ConnReset on RECV.
    let mut ok = false;
    for _ in 0..50 {
        let reply = client.call(Op::Recv, stream, 64, &[]);
        match reply.status {
            Status::WouldBlock => {
                let _ = wait(client, stream, WAIT_READABLE, 200);
            }
            Status::ConnReset => {
                ok = true;
                break;
            }
            Status::Ok => {}
            other => {
                client.fail(&[b"tcpwire reset -> ", status_name(other)]);
                break;
            }
        }
    }
    client.expect(
        b"tcpwire reset",
        if ok { Status::ConnReset } else { Status::Ok },
        Status::ConnReset,
    );
    log_tcpwire(b"reset", ok, 0, 0);
    let _ = client.call(Op::Close, stream, 0, &[]);
    ok
}

fn tcpwire_timeout(client: &mut Client, ip: [u8; 4], port: u16, retries: u32) -> bool {
    let stream = wire_connect_settled(client, ip, port, retries);
    if stream == 0 {
        return false;
    }
    let timed = wait(client, stream, WAIT_READABLE, 300) == Status::TimedOut;
    // The peer never reads: the stream must reach backpressure (WouldBlock, not an endless
    // accept of memory the peer's window cannot carry).
    let mut blocked = false;
    let block = [0x5Au8; STREAM_CHUNK];
    for _ in 0..600 {
        let status = client.call(Op::Send, stream, 0, &block).status;
        if status == Status::WouldBlock {
            blocked = true;
            break;
        }
        if status != Status::Ok {
            break;
        }
    }
    let ok = timed && blocked;
    client.expect(
        b"tcpwire timeout readable",
        if timed { Status::TimedOut } else { Status::Ok },
        Status::TimedOut,
    );
    client.expect(
        b"tcpwire backpressure",
        if blocked {
            Status::WouldBlock
        } else {
            Status::Ok
        },
        Status::WouldBlock,
    );
    log_tcpwire(b"timeout", ok, 0, 0);
    let _ = client.call(Op::Close, stream, 0, &[]);
    ok
}

fn tcpwire_churn(client: &mut Client, ip: [u8; 4], port: u16, retries: u32) -> bool {
    let mut ok = true;
    let mut cycles = 0u16;
    for cycle in 0..10u16 {
        let stream = wire_connect_settled(client, ip, port, retries);
        if stream == 0 {
            ok = false;
            break;
        }
        let mut payload = [0u8; 256];
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte = (cycle as u8).wrapping_mul(31).wrapping_add(i as u8);
        }
        if client.call(Op::Send, stream, 0, &payload).status != Status::Ok {
            ok = false;
            break;
        }
        let _ = client.call(Op::Shutdown, stream, 2, &[]);
        let (mut got, mut exact) = (0usize, true);
        let mut echo = [0u8; 256];
        for _ in 0..50 {
            let reply = client.call(Op::Recv, stream, 256, &[]);
            match reply.status {
                Status::Ok if reply.len == 0 => break,
                Status::Ok => {
                    let n = reply.len;
                    echo[..n].copy_from_slice(client.payload(&reply));
                    exact &= (got..got + n).all(|i| {
                        echo[i - got] == (cycle as u8).wrapping_mul(31).wrapping_add(i as u8)
                    });
                    got += n;
                    if got >= payload.len() {
                        break;
                    }
                }
                Status::WouldBlock => {
                    let _ = wait(client, stream, WAIT_READABLE, 500);
                }
                other => {
                    client.fail(&[b"tcpwire churn recv -> ", status_name(other)]);
                    exact = false;
                    break;
                }
            }
        }
        let _ = client.call(Op::Close, stream, 0, &[]);
        if !(exact && got == payload.len()) {
            ok = false;
            break;
        }
        cycles += 1;
    }
    let ok = ok && cycles == 10;
    client.out.line(&[
        b"netprobe: tcpwire churn ",
        dec(cycles as u64).bytes(),
        b"/10",
    ]);
    log_tcpwire(b"churn", ok, cycles as u64, 0);
    ok
}

/// NET11: watch the wire configuration the launcher's DHCP policy produces, then prove the
/// lease carries traffic. The observable is GETNAME's local address on a socket connected
/// to the peer: 0.0.0.0 until a lease, the leased address after, and 0.0.0.0 again when
/// the lease expires. `seconds` bounds the window after the wire attaches; the connect
/// phase before that is bounded separately.
fn dhcp_watch(client: &mut Client, peer: [u8; 4], echo_port: u16, seconds: u16) {
    let socket = client.udp_socket();
    client.bind_any(socket, 0);
    let pause = client.udp_socket();
    client.bind_any(pause, 0);
    let target = Addr {
        ip: peer,
        port: echo_port,
    };
    let (mut connected, mut last) = (false, [0u8; 4]);
    let (mut acquired, mut lost, mut echoes, mut refused) = (0u32, 0u32, 0u32, 0u32);
    let (mut odd, mut empty, mut rxerr, mut wto) = (0u32, 0u32, 0u32, 0u32);
    let (mut observed, mut waiting) = (0u32, 0u32);
    let mut port_logged = false;
    while observed < u32::from(seconds) * 2 {
        if !connected {
            // The wire attaches with the late service wave; connect may be refused first.
            let status = client.call(Op::Connect, socket, 0, &target.encode()).status;
            if status == Status::Ok {
                connected = true;
            } else if status != Status::NetUnreachable {
                client.fail(&[b"dhcp connect -> ", status_name(status)]);
                break;
            } else {
                waiting += 1;
                if waiting > 1200 {
                    client.fail(&[b"dhcp: the wire never attached"]);
                    break;
                }
            }
        }
        if connected {
            observed += 1;
            let names = client.call(Op::GetName, socket, 0, &[]);
            if names.status == Status::Ok && names.len >= 8 {
                let payload = client.payload(&names);
                if !port_logged {
                    port_logged = true;
                    log_dhcp_port(payload[2], payload[3]);
                }
                let ip = [payload[4], payload[5], payload[6], payload[7]];
                if ip != last {
                    if ip == [0; 4] {
                        if last != [0; 4] {
                            lost += 1;
                            log_dhcp(b"netprobe: dhcp lease lost");
                        }
                    } else {
                        acquired += 1;
                        log_dhcp_addr(ip);
                    }
                    last = ip;
                }
                if ip != [0; 4] {
                    let mut probe = [0u8; 32];
                    let n = dhcp_probe(&mut probe, observed);
                    // A late echo from a previous probe must not desynchronize the
                    // comparison: drain the socket before sending this one.
                    loop {
                        if client.call(Op::RecvFrom, socket, 64, &[]).status != Status::Ok {
                            break;
                        }
                    }
                    match client.call(Op::Send, socket, 0, &probe[..n]).status {
                        Status::Ok => {
                            if wait(client, socket, WAIT_READABLE, 300) == Status::Ok {
                                let got = client.call(Op::RecvFrom, socket, 64, &[]);
                                match got.status {
                                    Status::Ok if got.len > ADDR_LEN => {
                                        let payload = client.payload(&got);
                                        if payload[ADDR_LEN..] == probe[..n] {
                                            echoes += 1;
                                        } else {
                                            odd += 1;
                                            if odd == 1 {
                                                log_dhcp_recv(b"odd", got.len);
                                            }
                                        }
                                    }
                                    Status::Ok => {
                                        empty += 1;
                                        if empty == 1 {
                                            log_dhcp_recv(b"empty", got.len);
                                        }
                                    }
                                    other => {
                                        rxerr += 1;
                                        if rxerr == 1 {
                                            client.fail(&[b"dhcp recv -> ", status_name(other)]);
                                        }
                                    }
                                }
                            } else {
                                wto += 1;
                            }
                        }
                        Status::NetUnreachable => refused += 1,
                        _ => {}
                    }
                } else {
                    refused += 1;
                }
            }
        }
        let _ = wait(client, pause, WAIT_READABLE, 500);
    }
    let mut line = [0u8; 160];
    let mut at = 0usize;
    for part in [
        &b"netprobe: dhcp acquired="[..],
        dec(u64::from(acquired)).bytes(),
        &b" lost="[..],
        dec(u64::from(lost)).bytes(),
        &b" echoes="[..],
        dec(u64::from(echoes)).bytes(),
        &b" refused="[..],
        dec(u64::from(refused)).bytes(),
        &b" odd="[..],
        dec(u64::from(odd)).bytes(),
        &b" empty="[..],
        dec(u64::from(empty)).bytes(),
        &b" rxerr="[..],
        dec(u64::from(rxerr)).bytes(),
        &b" wto="[..],
        dec(u64::from(wto)).bytes(),
        &b"\n"[..],
    ] {
        let take = part.len().min(line.len() - at);
        line[at..at + take].copy_from_slice(&part[..take]);
        at += take;
    }
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line[..at]);
    if acquired >= 1 && echoes >= 1 {
        client.out.line(&[b"netprobe: dhcp lease lifecycle exact"]);
    } else {
        client.fail(&[b"dhcp campaign incomplete"]);
    }
}

fn log_dhcp_recv(tag: &[u8], len: usize) {
    let mut buf = [0u8; 96];
    let mut at = 0usize;
    for part in [
        &b"netprobe: dhcp first "[..],
        tag,
        &b" len="[..],
        dec(len as u64).bytes(),
        &b"\n"[..],
    ] {
        let take = part.len().min(buf.len() - at);
        buf[at..at + take].copy_from_slice(&part[..take]);
        at += take;
    }
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &buf[..at]);
}

fn log_dhcp_port(lo: u8, hi: u8) {
    let port = u16::from_le_bytes([lo, hi]);
    let mut buf = [0u8; 64];
    let head = b"netprobe: dhcp bound port=";
    buf[..head.len()].copy_from_slice(head);
    let d = dec(u64::from(port));
    let bytes = d.bytes();
    let take = bytes.len().min(buf.len() - head.len());
    buf[head.len()..head.len() + take].copy_from_slice(&bytes[..take]);
    let mut at = head.len() + take;
    buf[at] = b'\n';
    at += 1;
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &buf[..at]);
}

fn dhcp_probe(buf: &mut [u8; 32], n: u32) -> usize {
    let mut at = 0usize;
    for part in [&b"kumo-dhcp "[..], dec(u64::from(n)).bytes()] {
        let take = part.len().min(buf.len() - at);
        buf[at..at + take].copy_from_slice(&part[..take]);
        at += take;
    }
    at
}

fn log_dhcp(text: &[u8]) {
    let mut buf = [0u8; 96];
    let mut at = 0usize;
    for part in [text, &b"\n"[..]] {
        let take = part.len().min(buf.len() - at);
        buf[at..at + take].copy_from_slice(&part[..take]);
        at += take;
    }
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &buf[..at]);
}

fn log_dhcp_addr(ip: [u8; 4]) {
    let mut buf = [0u8; 96];
    let head = b"netprobe: dhcp lease ";
    buf[..head.len()].copy_from_slice(head);
    let mut at = head.len();
    for (i, byte) in ip.iter().enumerate() {
        if i > 0 {
            buf[at] = b'.';
            at += 1;
        }
        let d = dec(u64::from(*byte));
        let bytes = d.bytes();
        let take = bytes.len().min(buf.len() - at);
        buf[at..at + take].copy_from_slice(&bytes[..take]);
        at += take;
    }
    if at < buf.len() {
        buf[at] = b'\n';
        at += 1;
    }
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &buf[..at]);
}

/// NET12: the resolver campaign. Each phase is a name the isolated server answers a fixed
/// way; the `tcp`/`notcp` argument selects what a truncated answer should do, matching the
/// boot's resolver policy. Serial NOTICE lines carry every phase for the journal capture.
fn dns_probe(client: &mut Client, mode: &[u8]) {
    if mode != b"tcp" && mode != b"notcp" {
        client.out.line(&[b"netprobe: dns needs tcp|notcp"]);
        client.fail(&[b"dns mode"]);
        return;
    }
    let fallback_ok = mode == b"tcp";
    let pause = client.udp_socket();
    client.bind_any(pause, 0);
    let mut failures = 0u32;
    let mut expect = |client: &mut Client, name: &[u8], want: Status, budget: u64| -> u64 {
        let (status, result) = dns_resolve(client, pause, name, budget);
        log_dns(
            name,
            status,
            if status == Status::Ok {
                Some(result as u32)
            } else {
                None
            },
        );
        if status == want {
            result
        } else {
            client.fail(&[b"dns ", name, b" -> ", status_name(status)]);
            failures += 1;
            0
        }
    };
    // The first lookup may wait out the late service wave and the wire attach.
    let addr = expect(client, b"kumo.test", Status::Ok, 240_000);
    let _ = addr;
    let _ = expect(client, b"kumo.test", Status::Ok, 10_000);
    // The server's TTL is 3 s; the wait forces a second real query.
    let _ = wait(client, pause, WAIT_READABLE, 3_500);
    let _ = expect(client, b"kumo.test", Status::Ok, 60_000);
    let _ = expect(client, b"nope.test", Status::NotFound, 60_000);
    let _ = expect(client, b"nope.test", Status::NotFound, 10_000);
    let _ = expect(client, b"slow.test", Status::TimedOut, 60_000);
    let _ = expect(client, b"big.test", Status::MessageTooLarge, 60_000);
    let _ = expect(client, b"reject.test", Status::Ok, 60_000);
    let trunc_want = if fallback_ok {
        Status::Ok
    } else {
        Status::MessageTooLarge
    };
    let _ = expect(client, b"trunc.test", trunc_want, 120_000);
    client.out.line(&[
        b"netprobe: dns phases failed=",
        dec(u64::from(failures)).bytes(),
    ]);
    {
        let mut line = [0u8; 64];
        let head = b"netprobe: dns phases failed=";
        line[..head.len()].copy_from_slice(head);
        let d = dec(u64::from(failures));
        let bytes = d.bytes();
        let take = bytes.len().min(line.len() - head.len());
        line[head.len()..head.len() + take].copy_from_slice(&bytes[..take]);
        let mut at = head.len() + take;
        line[at] = b'\n';
        at += 1;
        let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line[..at]);
    }
    if failures != 0 {
        client.fail(&[b"dns campaign incomplete"]);
    }
}

/// Poll one lookup until a terminal status. NetUnreachable (the wire is not up yet) is
/// retried on the same budget as the boot.
fn dns_resolve(client: &mut Client, pause: u32, name: &[u8], budget_ms: u64) -> (Status, u64) {
    let steps = (budget_ms / 200).max(1);
    for _ in 0..steps {
        let reply = client.call(Op::Resolve, 0, 0, name);
        match reply.status {
            Status::InProgress | Status::NetUnreachable => {
                let _ = wait(client, pause, WAIT_READABLE, 200);
            }
            other => return (other, reply.result),
        }
    }
    (Status::TimedOut, 0)
}

fn log_dns(name: &[u8], status: Status, addr: Option<u32>) {
    let mut line = [0u8; 128];
    let mut at = 0usize;
    let put = |line: &mut [u8; 128], at: &mut usize, part: &[u8]| {
        let take = part.len().min(line.len() - *at);
        line[*at..*at + take].copy_from_slice(&part[..take]);
        *at += take;
    };
    put(&mut line, &mut at, b"netprobe: dns ");
    put(&mut line, &mut at, name);
    put(&mut line, &mut at, b" -> ");
    put(&mut line, &mut at, status_name(status));
    if let Some(a) = addr {
        let octets = a.to_be_bytes();
        put(&mut line, &mut at, b" ");
        for (i, byte) in octets.iter().enumerate() {
            if i > 0 {
                put(&mut line, &mut at, b".");
            }
            let d = dec(u64::from(*byte));
            put(&mut line, &mut at, d.bytes());
        }
    }
    put(&mut line, &mut at, b"\n");
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line[..at]);
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

/// `a.b.c.d`, four decimal octets of one to three digits.
fn parse_ipv4(text: &[u8]) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut fields = 0;
    let mut octet = 0u32;
    let mut digits = 0;
    for &byte in text {
        match byte {
            b'.' => {
                if digits == 0 || fields >= 3 || octet > 255 {
                    return None;
                }
                ip[fields] = octet as u8;
                fields += 1;
                octet = 0;
                digits = 0;
            }
            b'0'..=b'9' if digits < 3 => {
                octet = octet * 10 + u32::from(byte - b'0');
                digits += 1;
            }
            _ => return None,
        }
    }
    if digits == 0 || fields != 3 || octet > 255 {
        return None;
    }
    ip[3] = octet as u8;
    Some(ip)
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
    // `dns tcp|notcp`: the campaign's truncation expectation follows the boot's resolver
    // policy, which only the launcher knows.
    let dns_mode = if mode == b"dns" { args.next() } else { None };
    // `wire <ip> <port> [count]` and `ping <ip> [count]` take an address where the other
    // modes take a port; ping has no port, so its first number is the count.
    let wire_ip = if mode == b"wire"
        || mode == b"ping"
        || mode == b"peer"
        || mode == b"tcpwire"
        || mode == b"dhcp"
    {
        args.next().and_then(parse_ipv4)
    } else {
        None
    };
    let port = if mode == b"ping" {
        None
    } else {
        args.next().and_then(parse_u16)
    };
    let count = args.next().and_then(parse_u16);
    let retries = args.next().and_then(parse_u16).map_or(1200, u32::from);
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
        b"dns" => dns_probe(&mut client, dns_mode.unwrap_or(b"")),
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
        b"wire" if wire_ip.is_some_and(|ip| ip != [0; 4]) && port.is_some_and(|p| p != 0) => wire(
            &mut client,
            wire_ip.unwrap_or([0; 4]),
            port.unwrap_or(0),
            count.unwrap_or(4),
        ),
        b"ping" if wire_ip.is_some_and(|ip| ip != [0; 4]) => ping(
            &mut client,
            wire_ip.unwrap_or([0; 4]),
            count.unwrap_or(4),
            retries,
        ),
        b"peer" if wire_ip.is_some_and(|ip| ip != [0; 4]) && port.is_some_and(|p| p != 0) => peer(
            &mut client,
            wire_ip.unwrap_or([0; 4]),
            port.unwrap_or(0),
            count.unwrap_or(24),
            retries,
        ),
        b"tcpwire" if wire_ip.is_some_and(|ip| ip != [0; 4]) && port.is_some_and(|p| p != 0) => {
            tcpwire(
                &mut client,
                wire_ip.unwrap_or([0; 4]),
                port.unwrap_or(0),
                retries,
            )
        }
        b"dhcp" if wire_ip.is_some_and(|ip| ip != [0; 4]) && port.is_some_and(|p| p != 0) => {
            dhcp_watch(
                &mut client,
                wire_ip.unwrap_or([0; 4]),
                port.unwrap_or(0),
                count.unwrap_or(120),
            )
        }
        b"tcpserve" if port.is_some_and(|p| p != 0) => tcp_serve(&mut client, port.unwrap_or(0)),
        b"tcpclient" if port.is_some_and(|p| p != 0) => tcp_client(&mut client, port.unwrap_or(0)),
        b"tcperrors" => tcp_errors(&mut client),
        b"restartwait" => restart_wait(&mut client),
        b"restartfresh" if port.is_some() => restart_fresh(&mut client, port.unwrap_or(0)),
        _ => {
            client.out.line(&[
                b"usage: net netprobe udp|errors|tcperrors | recv|send <port> <1-4> | tcpserve|tcpclient <port> | restartwait | restartfresh <epoch> | wire <ip> <port> [count] | ping <ip> [count] [retries] | peer <ip> <port> [count] [retries] | tcpwire <ip> <port> [retries] | dhcp <ip> <echo-port> [seconds] | dns tcp|notcp",
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
