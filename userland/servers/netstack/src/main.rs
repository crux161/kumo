#![no_std]
#![no_main]
//j815
//j820
//j843
//j844

//! The resident netstack: one Port, one loopback interface, sessions over granted channels
//! (DESIGN/026). Sora transfers a control channel in `x1`; each control message is the tag `S`
//! carrying the server end of one new session channel. Everything protocol-shaped lives in the
//! host-tested library; this file is the syscall loop around it.
//!
//! Recovery class: stateless (discardable socket soft state); fresh Sora-granted channels reconnect.
//!
//! A resident child's `ChannelRead` parks on an empty channel and returns 0 only once the peer
//! has closed. So every wake reads exactly one message from the channel that woke (the kernel
//! re-arms the port while more is queued), and a 0-byte read is a closed session.

extern crate alloc;

use alloc::vec::Vec;

use kumo_abi::sys::log_level;
use kumo_abi::Handle;
use kumo_ipc::MAX_INLINE_BYTES;
use kumo_netstack::budget::{FRAMES_PER_TURN, FRAME_MAX, RX_CREDIT, SESSIONS};
use kumo_netstack::frame::{decode, encode, Body, Lane, HEADER_LEN};
use kumo_rt::{
    channel_read_with_handle, channel_write, clock_get, handle_close, handle_koid, port_bind,
    port_create, port_unbind, port_wait, timer_create,
};
use netstack::{After, IngressError, Output, ResolverConfig, Server, WireConfig, WireEvent};

kumo_rt::entry!(main);

/// A control message: one tag byte, with the session channel attached.
const CONTROL_SESSION: u8 = b'S';
/// A frame-channel grant: tag, IPv4 address (4 bytes), prefix (1), channel attached.
const CONTROL_FRAME: u8 = b'F';
/// NET11: a DHCP grant: tag, server port u16 LE, client port u16 LE.
const CONTROL_DHCP: u8 = b'D';
/// NET12: a resolver grant: tag, server IPv4 (4), UDP port u16 LE, TCP port u16 LE, flags.
const CONTROL_RESOLVER: u8 = b'R';
/// Timers shorter than this are rounded up, so a busy stack cannot spin the loop.
const MIN_TIMER_NS: u64 = 1_000_000;

fn log(level: u8, message: &[u8]) {
    let _ = kumo_rt::log_write(level, message);
}

fn fail(message: &[u8]) -> ! {
    log(log_level::ERR, message);
    kumo_rt::process_exit(1)
}

/// NET11: lease events reach the serial console here (dmesg and the journal capture them).
fn log_wire_events(server: &mut Server) {
    while let Some(event) = server.take_wire_event() {
        let mut line = Vec::with_capacity(96);
        match event {
            WireEvent::DhcpLease {
                addr,
                prefix,
                router,
            } => {
                line.extend_from_slice(b"netstack: dhcp lease ");
                push_ipv4(&mut line, addr);
                line.push(b'/');
                push_dec(&mut line, u64::from(prefix));
                if let Some(router) = router {
                    line.extend_from_slice(b" router ");
                    push_ipv4(&mut line, router);
                }
                line.push(b'\n');
            }
            WireEvent::DhcpRenewed { addr, prefix } => {
                line.extend_from_slice(b"netstack: dhcp renewed ");
                push_ipv4(&mut line, addr);
                line.push(b'/');
                push_dec(&mut line, u64::from(prefix));
                line.push(b'\n');
            }
            WireEvent::DhcpExpired => {
                line.extend_from_slice(b"netstack: dhcp lease expired\n");
            }
            WireEvent::DhcpDiscardedByGeneration => {
                line.extend_from_slice(b"netstack: dhcp lease discarded by interface generation\n");
            }
            WireEvent::DnsQuerySent => {
                line.extend_from_slice(b"netstack: dns query sent\n");
            }
            WireEvent::DnsAnswer { addr } => {
                line.extend_from_slice(b"netstack: dns answer ");
                push_ipv4(&mut line, addr);
                line.push(b'\n');
            }
            WireEvent::DnsNotFound => {
                line.extend_from_slice(b"netstack: dns not found\n");
            }
            WireEvent::DnsMalformed => {
                line.extend_from_slice(b"netstack: dns reply rejected (malformed)\n");
            }
            WireEvent::DnsRejected => {
                line.extend_from_slice(b"netstack: dns reply rejected (id/source)\n");
            }
            WireEvent::DnsOversized => {
                line.extend_from_slice(b"netstack: dns reply refused (oversized)\n");
            }
            WireEvent::DnsTruncated => {
                line.extend_from_slice(
                    b"netstack: dns reply refused (truncated; no TCP fallback)\n",
                );
            }
            WireEvent::DnsFallback => {
                line.extend_from_slice(b"netstack: dns truncated; TCP fallback\n");
            }
            WireEvent::DnsTimedOut => {
                line.extend_from_slice(b"netstack: dns lookup timed out\n");
            }
        }
        log(log_level::NOTICE, &line);
    }
}

fn push_ipv4(line: &mut Vec<u8>, addr: [u8; 4]) {
    for (i, byte) in addr.iter().enumerate() {
        if i > 0 {
            line.push(b'.');
        }
        push_dec(line, u64::from(*byte));
    }
}

fn valid(raw: u64) -> Option<Handle> {
    (raw != 0 && raw != u64::MAX).then_some(Handle(raw as u32))
}

fn push_dec(line: &mut Vec<u8>, mut value: u64) {
    let mut digits = [0u8; 20];
    let mut len = 0;
    loop {
        digits[len] = b'0' + (value % 10) as u8;
        value /= 10;
        len += 1;
        if value == 0 {
            break;
        }
    }
    line.extend(digits[..len].iter().rev());
}

struct Channels {
    port: Handle,
    /// `(channel, koid)` per server session index.
    sessions: [Option<(Handle, u64)>; SESSIONS],
    timer: Option<(Handle, u64, u64)>,
}

impl Channels {
    fn session_of(&self, koid: u64) -> Option<usize> {
        self.sessions
            .iter()
            .position(|entry| matches!(entry, Some((_, k)) if *k == koid))
    }

    fn write(&self, out: &mut Vec<Output>) {
        for output in out.drain(..) {
            if let Some((channel, _)) = self.sessions[output.session] {
                let _ = channel_write(channel, output.bytes.as_ptr(), output.bytes.len());
            }
        }
    }

    fn close(&mut self, server: &mut Server, session: usize, why: &[u8]) {
        if let Some((channel, _)) = self.sessions[session].take() {
            let _ = port_unbind(self.port, channel);
            let _ = handle_close(channel);
        }
        let freed = server.close_session(session);
        let mut line = Vec::with_capacity(96);
        line.extend_from_slice(b"netstack: session ");
        push_dec(&mut line, session as u64);
        line.extend_from_slice(b" closed (");
        line.extend_from_slice(why);
        line.extend_from_slice(b") sockets freed=");
        push_dec(&mut line, freed as u64);
        line.extend_from_slice(b" live sessions=");
        push_dec(&mut line, server.live_sessions() as u64);
        line.extend_from_slice(b" sockets=");
        push_dec(&mut line, server.live_sockets() as u64);
        line.push(b'\n');
        log(log_level::NOTICE, &line);
    }

    /// Keep exactly one timer armed for the server's next deadline.
    fn arm(&mut self, server: &mut Server, now: u64) {
        let Some(delay) = server.next_wake_ns(now) else {
            return;
        };
        let due = now.saturating_add(delay.max(MIN_TIMER_NS));
        if matches!(self.timer, Some((_, _, armed)) if armed <= due) {
            return;
        }
        self.disarm();
        let Some(timer) = valid(timer_create(due - now)) else {
            return;
        };
        let koid = handle_koid(timer);
        if koid == u64::MAX || port_bind(self.port, timer) != 0 {
            let _ = handle_close(timer);
            return;
        }
        self.timer = Some((timer, koid, due));
    }

    fn disarm(&mut self) {
        if let Some((timer, _, _)) = self.timer.take() {
            let _ = port_unbind(self.port, timer);
            let _ = handle_close(timer);
        }
    }
}

/// The one frame channel (DESIGN/026 §3): Sora grants it with the wire address, the provider
/// opens each generation with HELLO. A new grant replaces the old channel.
struct Frame {
    channel: Handle,
    koid: u64,
    lane: Lane,
    /// NET11: the launcher's configuration mode, granted with the channel.
    config: WireConfig,
    /// `wire_delivered()` at the last CREDIT return.
    returned: u64,
    /// RX_FRAMEs the wire device or the ingress policy refused.
    device_drops: u64,
    /// First-refusal-of-each-kind bits, so a malformed-frame storm cannot flood the log.
    refused: u8,
    /// Set once when the TX credit first runs out with egress still queued.
    backpressure_logged: bool,
}

/// Replace the frame channel with a fresh Sora grant. The wire stack attaches when the
/// provider's HELLO supplies its MAC.
fn install_frame(
    frame: &mut Option<Frame>,
    server: &mut Server,
    port: Handle,
    channel: Handle,
    config: WireConfig,
) {
    close_frame(frame, server, port);
    let koid = handle_koid(channel);
    if koid == u64::MAX || port_bind(port, channel) != 0 {
        let _ = handle_close(channel);
        log(log_level::WARNING, b"netstack: frame channel bind failed\n");
        return;
    }
    *frame = Some(Frame {
        channel,
        koid,
        lane: Lane::new(),
        config,
        returned: 0,
        device_drops: 0,
        refused: 0,
        backpressure_logged: false,
    });
    log(log_level::NOTICE, b"netstack: frame channel granted\n");
}

fn close_frame(frame: &mut Option<Frame>, server: &mut Server, port: Handle) {
    if let Some(old) = frame.take() {
        let _ = port_unbind(port, old.channel);
        let _ = handle_close(old.channel);
    }
    server.detach_wire();
}

/// One message from the frame channel: decode, enforce the Lane, apply to the wire stack.
/// Drain the frame channel: every pending message, at most one RX credit window per wake.
/// Port wakes coalesce per source, so a reader that takes one message per wake paces the
/// whole wire to its wake rate. On the opi5 that was ~325-420 frames/s, which emptied the
/// provider's 32-frame credit window and dropped ~75% of a 15 Mbit/s download in drv-xhci
/// (`no_credit`, 2026-11-13 speed test). Draining lets the end-of-turn `pump_frame` return
/// the whole batch's credits at once. Reads are non-blocking, so a coalesced wake that finds
/// the channel already drained returns instead of parking netstack on an empty channel.
fn serve_frame(frame: &mut Option<Frame>, server: &mut Server, port: Handle, now: u64) {
    let mut bytes = [0u8; FRAME_MAX + HEADER_LEN];
    for turn in 0..RX_CREDIT {
        let Some(f) = frame.as_ref() else {
            return;
        };
        let Some((n, raw)) = read_frame_message(f.channel, &mut bytes, turn == 0) else {
            return;
        };
        if n > bytes.len() {
            log(log_level::WARNING, b"netstack: frame channel read failed\n");
            return;
        }
        serve_frame_message(frame, server, port, now, &mut bytes, n, raw);
    }
}

/// One frame-channel message: `Some((len, handle))`, or `None` when nothing is pending.
/// aarch64 reads with `CHANNEL_READ_NONBLOCK`; elsewhere the first read of a wake blocks as
/// before and the drain stops there.
fn read_frame_message(channel: Handle, bytes: &mut [u8], first: bool) -> Option<(usize, u64)> {
    #[cfg(target_arch = "aarch64")]
    {
        let _ = first;
        kumo_rt::channel_try_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len())
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        first.then(|| channel_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len()))
    }
}

fn serve_frame_message(
    frame: &mut Option<Frame>,
    server: &mut Server,
    port: Handle,
    now: u64,
    bytes: &mut [u8; FRAME_MAX + HEADER_LEN],
    n: usize,
    raw: u64,
) {
    let Some(f) = frame.as_mut() else {
        return;
    };
    if n == 0 {
        log(log_level::NOTICE, b"netstack: frame channel closed\n");
        close_frame(frame, server, port);
        return;
    }
    let handles = usize::from(valid(raw).is_some());
    if let Some(extra) = valid(raw) {
        let _ = handle_close(extra);
    }
    let Ok(message) = decode(&bytes[..n], handles) else {
        log(
            log_level::WARNING,
            b"netstack: frame message dropped (decode)\n",
        );
        return;
    };
    if let Err(violation) = f.lane.accept(&message) {
        // A violation closes the channel (DESIGN/026 §3); a fresh channel re-HELLOs.
        let name: &[u8] = match violation {
            kumo_netstack::frame::Violation::WrongDirection => b"WrongDirection",
            kumo_netstack::frame::Violation::NoHello => b"NoHello",
            kumo_netstack::frame::Violation::ReusedGeneration => b"ReusedGeneration",
            kumo_netstack::frame::Violation::HelloSequence => b"HelloSequence",
            kumo_netstack::frame::Violation::StaleGeneration => b"StaleGeneration",
            kumo_netstack::frame::Violation::SequenceGap => b"SequenceGap",
            kumo_netstack::frame::Violation::CreditOverrun => b"CreditOverrun",
            kumo_netstack::frame::Violation::UnsolicitedTxDone => b"UnsolicitedTxDone",
        };
        let answers = match message.body {
            Body::TxDone { answers, .. } => answers,
            _ => 0,
        };
        let mut line = Vec::with_capacity(128);
        line.extend_from_slice(b"netstack: frame violation ");
        line.extend_from_slice(name);
        line.extend_from_slice(b" answers=");
        push_dec(&mut line, u64::from(answers));
        line.extend_from_slice(b" oldest=");
        push_dec(&mut line, u64::from(f.lane.oldest_tx()));
        line.extend_from_slice(b" in_flight=");
        push_dec(&mut line, u64::from(f.lane.tx_in_flight()));
        line.extend_from_slice(b"; channel closed\n");
        log(log_level::WARNING, &line);
        close_frame(frame, server, port);
        return;
    }
    match message.body {
        Body::Hello(hello) => {
            server.attach_wire_config(now, hello.mac, f.config);
            log(log_level::NOTICE, b"netstack: wire interface up\n");
            if matches!(f.config, WireConfig::Dhcp { .. }) {
                log(log_level::NOTICE, b"netstack: dhcp discovering\n");
            }
        }
        Body::Link { up, mbps, .. } => {
            let mut line = Vec::with_capacity(64);
            line.extend_from_slice(b"netstack: wire link ");
            line.extend_from_slice(if up { b"up" } else { b"down" });
            line.push(b' ');
            push_dec(&mut line, mbps as u64);
            line.extend_from_slice(b" Mb/s\n");
            log(log_level::NOTICE, &line);
        }
        Body::RxFrame(bytes) => {
            if let Err(refusal) = server.push_wire_frame(bytes) {
                f.device_drops += 1;
                let (bit, name): (u8, &[u8]) = match refusal {
                    IngressError::Detached => (0, b"detached"),
                    IngressError::Device(_) => (1, b"ingress ring full"),
                    IngressError::Truncated => (2, b"truncated"),
                    IngressError::UnknownEthertype(_) => (3, b"unknown ethertype"),
                    IngressError::Fragment => (4, b"fragment"),
                    IngressError::BadIpv4Checksum => (5, b"bad ipv4 checksum"),
                };
                // First refusal of each kind per generation: a storm cannot flood the log.
                if f.refused & (1 << bit) == 0 {
                    f.refused |= 1 << bit;
                    let mut line = Vec::with_capacity(48);
                    line.extend_from_slice(b"netstack: wire ingress refused ");
                    line.extend_from_slice(name);
                    line.push(b'\n');
                    log(log_level::NOTICE, &line);
                }
            }
        }
        Body::Counters(counters) => {
            let mut line = Vec::with_capacity(128);
            line.extend_from_slice(b"netstack: provider counters drop_no_credit=");
            push_dec(&mut line, counters.rx_drop_no_credit);
            line.extend_from_slice(b" bad=");
            push_dec(&mut line, counters.rx_bad);
            line.extend_from_slice(b" tx_drop=");
            push_dec(&mut line, counters.tx_drop);
            line.extend_from_slice(b" tx_err=");
            push_dec(&mut line, counters.tx_err);
            line.push(b'\n');
            log(log_level::NOTICE, &line);
        }
        _ => {}
    }
}

/// Hand wire egress over as TX_FRAMEs within the TX credit, and return consumed RX credits.
fn pump_frame(frame: &mut Option<Frame>, server: &mut Server) {
    let Some(f) = frame.as_mut() else {
        return;
    };
    let mut bytes = [0u8; FRAME_MAX];
    for _ in 0..FRAMES_PER_TURN {
        if f.lane.tx_credit() == 0 {
            if !f.backpressure_logged && server.wire_pending_outbound() > 0 {
                f.backpressure_logged = true;
                log(
                    log_level::NOTICE,
                    b"netstack: frame tx backpressure (credit exhausted)\n",
                );
            }
            break;
        }
        let Some(len) = server.take_wire_frame(&mut bytes) else {
            break;
        };
        let Some(out) = f.lane.send_tx(&bytes[..len]) else {
            break;
        };
        let mut wire = [0u8; FRAME_MAX + HEADER_LEN];
        let Some(n) = encode(&out, &mut wire) else {
            f.lane.abandon_tx();
            break;
        };
        // A refused write means the frame never reached the provider; the lane must not
        // keep counting it or every later TX_DONE answer misfires. J843 metal finding.
        if channel_write(f.channel, wire.as_ptr(), n) != 0 {
            f.lane.abandon_tx();
            log(
                log_level::NOTICE,
                b"netstack: frame tx write refused; lane rolled back\n",
            );
            break;
        }
        // Per frame, so DEBUG: at NOTICE a proof image's console spent the upload logging it
        // (0.7 Mbit/s instead of ~45 in the 2026-11-13 speed test).
        kumo_rt::log_write(
            log_level::DEBUG,
            b"netstack: frame tx handed to provider\n",
        );
    }
    let delivered = server.wire_delivered();
    let owed = delivered.saturating_sub(f.returned);
    if owed > 0 {
        if let Some(credit) = f.lane.return_rx(owed as u32) {
            let mut wire = [0u8; FRAME_MAX + HEADER_LEN];
            if let Some(n) = encode(&credit, &mut wire) {
                if channel_write(f.channel, wire.as_ptr(), n) == 0 {
                    f.returned = delivered;
                }
            }
        }
    }
}

#[no_mangle]
extern "C" fn main(
    _arg0: u64,
    control_raw: u64,
    _arg2: u64,
    _arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
    _arg7: u64,
) -> ! {
    let control = valid(control_raw).unwrap_or_else(|| fail(b"netstack: no control channel\n"));
    // The supervisor queues the instance epoch before admission. Read it before binding,
    // so its READABLE packet cannot survive as a stale wake in the ordinary receive loop.
    let mut bootstrap = [0u8; 3];
    let (n, raw) = channel_read_with_handle(control, bootstrap.as_mut_ptr(), bootstrap.len());
    if valid(raw).is_some() || n != 3 || bootstrap[0] != b'I' {
        if let Some(handle) = valid(raw) {
            let _ = handle_close(handle);
        }
        fail(b"netstack: invalid instance bootstrap\n");
    }
    let epoch = u16::from_le_bytes([bootstrap[1], bootstrap[2]]);
    let port = valid(port_create()).unwrap_or_else(|| fail(b"netstack: no port\n"));
    let control_koid = handle_koid(control);
    if control_koid == u64::MAX || port_bind(port, control) != 0 {
        fail(b"netstack: control bind failed\n");
    }
    // Heap, not stack: a native child's default stack is 16 KiB.
    let mut server = alloc::boxed::Box::new(
        Server::for_instance(clock_get(), epoch)
            .unwrap_or_else(|| fail(b"netstack: invalid instance epoch\n")),
    );
    let mut channels = Channels {
        port,
        sessions: [None; SESSIONS],
        timer: None,
    };
    let mut frame: Option<Frame> = None;
    let mut out: Vec<Output> = Vec::new();
    let mut message = alloc::vec![0u8; MAX_INLINE_BYTES];
    log(
        log_level::NOTICE,
        b"netstack: online, loopback 127.0.0.1/8, UDP/TCP; sessions granted by Sora\n",
    );

    loop {
        let source = port_wait(port);
        let now = clock_get();
        if source == control_koid {
            let mut grant = [0u8; 12];
            let (n, raw) = channel_read_with_handle(control, grant.as_mut_ptr(), grant.len());
            if n == 0 {
                // Sora is gone; serve the sessions that remain.
                let _ = port_unbind(port, control);
            } else if n == 6 && grant[0] == CONTROL_FRAME {
                match valid(raw) {
                    Some(channel) => install_frame(
                        &mut frame,
                        &mut server,
                        port,
                        channel,
                        WireConfig::Static {
                            addr: [grant[1], grant[2], grant[3], grant[4]],
                            prefix: grant[5],
                        },
                    ),
                    None => log(
                        log_level::WARNING,
                        b"netstack: frame grant without a channel\n",
                    ),
                }
            } else if n == 5 && grant[0] == CONTROL_DHCP {
                let server_port = u16::from_le_bytes([grant[1], grant[2]]);
                let client_port = u16::from_le_bytes([grant[3], grant[4]]);
                match valid(raw) {
                    Some(channel) => install_frame(
                        &mut frame,
                        &mut server,
                        port,
                        channel,
                        WireConfig::Dhcp {
                            server_port,
                            client_port,
                        },
                    ),
                    None => log(
                        log_level::WARNING,
                        b"netstack: dhcp grant without a channel\n",
                    ),
                }
            } else if n == 10 && grant[0] == CONTROL_RESOLVER {
                if let Some(extra) = valid(raw) {
                    let _ = handle_close(extra);
                }
                let config = ResolverConfig {
                    server: [grant[1], grant[2], grant[3], grant[4]],
                    udp_port: u16::from_le_bytes([grant[5], grant[6]]),
                    tcp_port: u16::from_le_bytes([grant[7], grant[8]]),
                    fallback: grant[9] & 1 != 0,
                };
                server.set_resolver(config);
                let mut line = Vec::with_capacity(96);
                line.extend_from_slice(b"netstack: resolver granted udp=");
                push_dec(&mut line, u64::from(config.udp_port));
                line.extend_from_slice(b" tcp=");
                push_dec(&mut line, u64::from(config.tcp_port));
                line.extend_from_slice(b" fallback=");
                push_dec(&mut line, u64::from(config.fallback as u64));
                line.push(b'\n');
                log(log_level::NOTICE, &line);
            } else if let Some(channel) = valid(raw) {
                let koid = handle_koid(channel);
                let admitted = (grant[0] == CONTROL_SESSION && koid != u64::MAX)
                    .then(|| server.open_session())
                    .flatten();
                match admitted {
                    Some(session) if port_bind(port, channel) == 0 => {
                        channels.sessions[session] = Some((channel, koid));
                        let mut line = Vec::with_capacity(64);
                        line.extend_from_slice(b"netstack: session ");
                        push_dec(&mut line, session as u64);
                        line.extend_from_slice(b" granted, live sessions=");
                        push_dec(&mut line, server.live_sessions() as u64);
                        line.push(b'\n');
                        log(log_level::NOTICE, &line);
                    }
                    Some(session) => {
                        server.close_session(session);
                        let _ = handle_close(channel);
                    }
                    None => {
                        log(
                            log_level::WARNING,
                            b"netstack: session refused (full or bad tag)\n",
                        );
                        let _ = handle_close(channel);
                    }
                }
            }
        } else if frame.as_ref().is_some_and(|f| f.koid == source) {
            serve_frame(&mut frame, &mut server, port, clock_get());
        } else if let Some(session) = channels.session_of(source) {
            let (channel, _) = channels.sessions[session].unwrap();
            let (n, raw) = channel_read_with_handle(channel, message.as_mut_ptr(), message.len());
            if n == 0 {
                channels.close(&mut server, session, b"peer closed");
            } else {
                let handles = usize::from(valid(raw).is_some());
                if let Some(extra) = valid(raw) {
                    let _ = handle_close(extra);
                }
                // The buffer holds the largest message the kernel carries, so `n` is the
                // whole message and the codec's exact-length check stays meaningful.
                let after = server.handle(session, &message[..n], handles, now, &mut out);
                server.poll(now, &mut out);
                channels.write(&mut out);
                if after == After::Close {
                    channels.close(&mut server, session, b"protocol");
                }
            }
        } else if matches!(channels.timer, Some((_, koid, _)) if koid == source) {
            channels.disarm();
        }
        server.poll(clock_get(), &mut out);
        channels.write(&mut out);
        channels.arm(&mut server, clock_get());
        pump_frame(&mut frame, &mut server);
        log_wire_events(&mut server);
    }
}
