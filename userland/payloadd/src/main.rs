#![no_std]
#![no_main]

//! `payloadd` (PROPOSAL/025 N2): fetch the payload archive from the boot server into the VMO
//! Sora granted as cap0, over netstack with windowed TFTP, and mark it `DONE` when the bytes
//! hash to the pin. Sora verifies again before it mounts anything; this program is only the
//! transport, and a failure here leaves the boot set fully usable.
//!
//! argv: `payloadd <server-ip> <filename> <size> <sha256-hex> [retries]`
//! cap0: a VMO of at least `size + 4` bytes; `DONE` is written at `size` on success.
//! Progress and the result go to the kernel console (HDMI + serial).

extern crate alloc;

use kumo_abi::{unpack_argv, Handle};
use kumo_netstack::budget::UDP_PAYLOAD_MAX;
use kumo_netstack::socket::{
    decode_reply, encode_request, Addr, Op, RequestHeader, Status, ADDR_LEN, HEADER_LEN,
    RECV_BATCH_MAX, RESULT_MORE, WAIT_CONNECTED, WAIT_READABLE,
};
use kumo_rt::{
    channel_read, channel_write, clock_get, debug_write, process_exit, startup, vmo_read, vmo_write,
};
use payloadd::{ack, rrq, Receiver, Rx};

kumo_rt::entry!(main);

const MESSAGE_MAX: usize = 2112;
const TFTP_PORT: u16 = 69;
const BLKSIZE: u16 = 1468;
/// Blocks per ACK. The round trip through netstack is ~20 ms on the opi5, so the window sets
/// the rate. Measured on the opi5 AX88 (USB 2 behind a hub), 2.7 MB: 4 -> 10.2 s clean;
/// 8 -> 5.7 s clean; 16 -> 28.8 s, the chip flagging drops from 14-frame bursts it could not
/// buffer. Stay at 8 unless the adapter's link changes.
const WINDOW: u16 = 8;
/// Silent seconds before an ACK is resent; transfers give up after this many in a row.
const TIMEOUT_MS: u64 = 1000;
const MAX_TIMEOUTS: u32 = 8;
/// Fresh transfers after a stall before giving up.
const MAX_RESTARTS: u64 = 5;
pub const DONE_MARK: &[u8; 4] = b"DONE";

fn say(parts: &[&[u8]]) {
    let mut buf = [0u8; 200];
    let mut at = 0;
    for part in parts.iter().chain([&b"\n"[..]].iter()) {
        let take = part.len().min(buf.len() - at);
        buf[at..at + take].copy_from_slice(&part[..take]);
        at += take;
    }
    debug_write(buf.as_ptr(), at);
}

struct Dec([u8; 20], usize);

fn dec(mut value: u64) -> Dec {
    let mut buf = [0u8; 20];
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let mut out = [0u8; 20];
    let n = buf.len() - at;
    out[..n].copy_from_slice(&buf[at..]);
    Dec(out, n)
}

impl Dec {
    fn bytes(&self) -> &[u8] {
        &self.0[..self.1]
    }
}

fn fail(parts: &[&[u8]]) -> ! {
    let mut line = [&b"payload: FAIL "[..]; 6];
    let n = parts.len().min(5);
    line[1..1 + n].copy_from_slice(&parts[..n]);
    say(&line[..1 + n]);
    process_exit(1)
}

struct Client {
    channel: Handle,
    generation: u32,
    next_id: u32,
    rx: [u8; MESSAGE_MAX],
}

struct Reply {
    status: Status,
    socket: u32,
    result: u64,
    len: usize,
}

impl Client {
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
            fail(&[b"request would not encode"]);
        };
        if channel_write(self.channel, buf.as_ptr(), len) != 0 {
            fail(&[b"netstack session closed"]);
        }
        let n = channel_read(self.channel, self.rx.as_mut_ptr(), self.rx.len()) as usize;
        if n == 0 || n > self.rx.len() {
            fail(&[b"netstack session closed"]);
        }
        let Ok(reply) = decode_reply(&self.rx[..n], 0) else {
            fail(&[b"bad reply"]);
        };
        if reply.request_id != id {
            fail(&[b"reply does not match its request"]);
        }
        if self.generation == 0 {
            self.generation = reply.generation;
        }
        Reply {
            status: reply.status,
            socket: reply.socket,
            result: reply.result,
            len: reply.payload.len(),
        }
    }

    fn sendto(&mut self, socket: u32, to: Addr, data: &[u8]) -> Status {
        let mut payload = [0u8; ADDR_LEN + UDP_PAYLOAD_MAX];
        payload[..ADDR_LEN].copy_from_slice(&to.encode());
        payload[ADDR_LEN..ADDR_LEN + data.len()].copy_from_slice(data);
        self.call(Op::SendTo, socket, 0, &payload[..ADDR_LEN + data.len()])
            .status
    }

    /// RECV_BATCH: hand each datagram (source, body) to `sink`; 0 when the socket was empty.
    fn recv_batch(&mut self, socket: u32, mut sink: impl FnMut(Addr, &[u8])) -> usize {
        let first = self.call(Op::RecvBatch, socket, RECV_BATCH_MAX, &[]);
        if first.status != Status::Ok || first.len < ADDR_LEN {
            return 0;
        }
        let id = self.next_id - 1;
        let payload = &self.rx[HEADER_LEN..HEADER_LEN + first.len];
        sink(source_of(payload), &payload[ADDR_LEN..]);
        let mut received = 1;
        let mut more = first.result & RESULT_MORE != 0;
        while more {
            let n = channel_read(self.channel, self.rx.as_mut_ptr(), self.rx.len()) as usize;
            if n == 0 || n > self.rx.len() {
                break;
            }
            let Ok(reply) = decode_reply(&self.rx[..n], 0) else {
                break;
            };
            if reply.request_id != id || reply.payload.len() < ADDR_LEN {
                break;
            }
            more = reply.result & RESULT_MORE != 0;
            sink(source_of(reply.payload), &reply.payload[ADDR_LEN..]);
            received += 1;
        }
        received
    }

    fn wait(&mut self, socket: u32, interest: u8, ms: u64) -> Status {
        self.call(Op::Wait, socket, interest as u64 | (ms.max(1) << 32), &[])
            .status
    }
}

fn source_of(payload: &[u8]) -> Addr {
    Addr {
        ip: [payload[4], payload[5], payload[6], payload[7]],
        port: u16::from_le_bytes([payload[2], payload[3]]),
    }
}

fn parse_ipv4(text: &[u8]) -> Option<[u8; 4]> {
    let mut ip = [0u8; 4];
    let mut fields = text.split(|b| *b == b'.');
    for octet in &mut ip {
        let field = fields.next()?;
        if field.is_empty() || field.len() > 3 || !field.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let v = field.iter().fold(0u32, |a, b| a * 10 + u32::from(b - b'0'));
        *octet = u8::try_from(v).ok()?;
    }
    fields.next().is_none().then_some(ip)
}

fn parse_u64(text: &[u8]) -> Option<u64> {
    (!text.is_empty() && text.len() <= 19 && text.iter().all(u8::is_ascii_digit))
        .then(|| text.iter().fold(0u64, |a, b| a * 10 + u64::from(b - b'0')))
}

fn parse_hex32(text: &[u8]) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let nib = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; 32];
    for (i, pair) in text.chunks_exact(2).enumerate() {
        out[i] = nib(pair[0])? << 4 | nib(pair[1])?;
    }
    Some(out)
}

/// `vmo_write` moves at most VMO_TRANSFER_MAX bytes per call.
fn store(vmo: Handle, offset: u64, bytes: &[u8]) -> bool {
    let mut done = 0;
    while done < bytes.len() {
        let take = (bytes.len() - done).min(kumo_abi::sys::VMO_TRANSFER_MAX);
        if vmo_write(vmo, offset + done as u64, bytes[done..].as_ptr(), take) != 0 {
            return false;
        }
        done += take;
    }
    true
}

#[no_mangle]
extern "C" fn main(
    bootstrap: u64,
    _a1: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    let startup = startup(Handle(bootstrap as u32));
    let mut argv_buf = [0u8; 256];
    let read = startup
        .argv
        .is_some_and(|argv| vmo_read(argv, 0, argv_buf.as_mut_ptr(), argv_buf.len()) == 0);
    let mut args = unpack_argv(if read { &argv_buf[..] } else { &[] }).skip(1);
    let (Some(server), Some(name), Some(size), Some(pin)) = (
        args.next().and_then(parse_ipv4),
        args.next(),
        args.next().and_then(parse_u64),
        args.next().and_then(parse_hex32),
    ) else {
        fail(&[b"usage: payloadd <server-ip> <file> <size> <sha256> [retries]"]);
    };
    let retries = args.next().and_then(parse_u64).unwrap_or(1200);
    let Some(net) = startup.net else {
        fail(&[b"no network grant"]);
    };
    let Some(vmo) = startup.cap0 else {
        fail(&[b"no payload VMO (cap0)"]);
    };

    let mut client = alloc::boxed::Box::new(Client {
        channel: net,
        generation: 0,
        next_id: 1,
        rx: [0u8; MESSAGE_MAX],
    });
    if client.call(Op::Hello, 0, 0, &[]).status != Status::Ok {
        fail(&[b"netstack hello"]);
    }
    let socket = client.call(Op::Socket, 0, 1, &[]);
    if socket.status != Status::Ok {
        fail(&[b"udp socket"]);
    }
    let s = socket.socket;
    let bind = client.call(
        Op::Bind,
        s,
        0,
        &Addr {
            ip: [0; 4],
            port: 0,
        }
        .encode(),
    );
    if bind.status != Status::Ok {
        fail(&[b"udp bind"]);
    }

    let mut request = [0u8; 160];
    let Some(rrq_len) = rrq(name, BLKSIZE, WINDOW, &mut request) else {
        fail(&[b"file name too long"]);
    };
    let server_port = Addr {
        ip: server,
        port: TFTP_PORT,
    };
    say(&[
        b"payload: fetching ",
        name,
        b" (",
        dec(size).bytes(),
        b" bytes) from the boot server",
    ]);

    // 1. RRQ until the server answers, retrying while the wire comes up.
    let mut rx = Receiver::new();
    let mut tid: Option<Addr> = None;
    let mut hasher = kumo_sha256::Sha256::new();
    let mut stored = 0u64;
    let mut tries = 0u64;
    let started = clock_get();
    let mut first_data = 0u64;
    let mut acks_sent = 0u64;
    let mut gap_acks = 0u64;
    let mut timeouts_total = 0u64;
    let mut timeouts = 0u32;
    let mut last_rrq = 0u64;
    let mut restarts = 0u64;
    while !rx.done() {
        // The RRQ goes out at most once a second until the server answers; a wake for its
        // answer must never resend it (each RRQ opens a new server session).
        if !rx.started() && clock_get().saturating_sub(last_rrq) >= 1_000_000_000 {
            last_rrq = clock_get();
            let status = client.sendto(s, server_port, &request[..rrq_len]);
            if status != Status::Ok
                && status != Status::NetUnreachable
                && status != Status::WouldBlock
            {
                fail(&[b"rrq send refused"]);
            }
            if status != Status::Ok {
                last_rrq = 0; // no wire yet: retry on the next pass, after the wait below
                let _ = client.wait(s, WAIT_CONNECTED, 500);
            }
            tries += 1;
            if tries > retries {
                fail(&[b"boot server never answered"]);
            }
            if tries % 20 == 0 {
                say(&[
                    b"payload: waiting for the wire / server, try ",
                    dec(tries).bytes(),
                ]);
            }
        }
        // 2. Drain whatever arrived, acting on each datagram.
        let mut to_send: [Option<(Addr, u16)>; 64] = [None; 64];
        let mut queued = 0usize;
        let mut error = None;
        let mut overflow = false;
        let got = client.recv_batch(s, |from, packet| {
            // The transfer is whoever answers the RRQ first, address and port (RFC 1350's
            // TID). The address may differ from the one asked: a multi-homed server (a Mac on
            // Wi-Fi and Ethernet) answers from its route's interface. The boot set's SHA-256
            // pin, not the source address, is what the bytes are trusted by.
            if let Some(t) = tid {
                if from.ip != t.ip || from.port != t.port {
                    return;
                }
            }
            let reply = match rx.on_packet(packet) {
                Rx::Oack { .. } => Some(0),
                Rx::Data {
                    offset,
                    bytes,
                    ack,
                    last: _,
                } => {
                    if offset + bytes.len() as u64 > size || !store(vmo, offset, bytes) {
                        overflow = true;
                        return;
                    }
                    hasher.update(bytes);
                    stored = offset + bytes.len() as u64;
                    if first_data == 0 {
                        first_data = clock_get();
                    }
                    ack
                }
                Rx::Skip { ack } => {
                    gap_acks += u64::from(ack.is_some());
                    ack
                }
                Rx::Error { code } => {
                    error = Some(code);
                    None
                }
                Rx::Ignore => None,
            };
            if tid.is_none() && rx.started() {
                tid = Some(from);
            }
            if let (Some(block), Some(t)) = (reply, tid) {
                if queued < to_send.len() {
                    to_send[queued] = Some((t, block));
                    queued += 1;
                }
            }
        });
        if overflow {
            fail(&[b"server sent more than the pinned size"]);
        }
        if let Some(code) = error {
            fail(&[b"server error code ", dec(u64::from(code)).bytes()]);
        }
        for (to, block) in to_send.iter().flatten() {
            let _ = client.sendto(s, *to, &ack(*block));
            acks_sent += 1;
        }
        if rx.done() {
            break;
        }
        if got == 0 {
            if client.wait(
                s,
                WAIT_READABLE,
                if tid.is_some() { TIMEOUT_MS } else { 200 },
            ) == Status::Ok
            {
                timeouts = 0;
                continue;
            }
            if let Some(t) = tid {
                timeouts += 1;
                timeouts_total += 1;
                if timeouts > MAX_TIMEOUTS {
                    // The server abandons a session after ~5 silent seconds; reliability is
                    // a fresh transfer, not a longer wait. The pin still checks every byte.
                    restarts += 1;
                    if restarts > MAX_RESTARTS {
                        fail(&[
                            b"transfer stalled at block ",
                            dec(rx.blocks()).bytes(),
                            b" (restarts exhausted)",
                        ]);
                    }
                    say(&[
                        b"payload: stalled at block ",
                        dec(rx.blocks()).bytes(),
                        b"; restarting the transfer (",
                        dec(restarts).bytes(),
                        b")",
                    ]);
                    rx = Receiver::new();
                    tid = None;
                    hasher = kumo_sha256::Sha256::new();
                    stored = 0;
                    timeouts = 0;
                    last_rrq = 0;
                    continue;
                }
                let _ = client.sendto(s, t, &ack(rx.timeout_ack()));
                acks_sent += 1;
            }
        } else {
            timeouts = 0;
        }
    }

    let ms = (clock_get().saturating_sub(started)) / 1_000_000;
    let transfer_ms = (clock_get().saturating_sub(first_data)) / 1_000_000;
    if stored != size {
        fail(&[
            b"short transfer: ",
            dec(stored).bytes(),
            b" of ",
            dec(size).bytes(),
        ]);
    }
    if hasher.finish() != pin {
        fail(&[b"SHA-256 does not match the boot set's pin"]);
    }
    if !store(vmo, size, DONE_MARK) {
        fail(&[b"could not mark the VMO"]);
    }
    let kbps = if transfer_ms > 0 {
        stored * 8 / transfer_ms
    } else {
        0
    };
    say(&[
        b"payload: fetched ",
        dec(stored).bytes(),
        b" bytes in ",
        dec(transfer_ms).bytes(),
        b" ms (",
        dec(kbps / 1000).bytes(),
        b".",
        dec(kbps % 1000 / 100).bytes(),
        b" Mbit/s), sha256 ok; total ",
        dec(ms).bytes(),
        b" ms incl. wire wait; acks ",
        dec(acks_sent).bytes(),
        b" gap ",
        dec(gap_acks).bytes(),
        b" timeouts ",
        dec(timeouts_total).bytes(),
    ]);
    process_exit(0)
}
