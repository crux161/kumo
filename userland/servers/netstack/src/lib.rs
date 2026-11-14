#![no_std]
//j815
//j817
//j820
//j843
//j844

//! netstack's server core (PLAN/020 NET2b/NET2c, DESIGN/026): sessions over the KNS1/KNR1
//! socket contract, UDP and TCP sockets on smoltcp, one loopback interface. No syscalls live
//! here, so the whole request path runs under `cargo test -p netstack`; `main.rs` is the Port
//! loop around it.
//!
//! Recovery class: stateless (discardable socket soft state); process death destroys sessions and connections.
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
    FRAME_MAX, ICMP_BUFFER, ICMP_ECHO_MAX, ICMP_METADATA, SESSIONS, SOCKETS_PER_SESSION,
    SOCKETS_TOTAL, SOCKET_BUFFER, STREAM_PAYLOAD_MAX, UDP_METADATA, UDP_PAYLOAD_MAX,
    WIRE_UDP_RX_BUFFER, WIRE_UDP_RX_METADATA,
};
use kumo_netstack::ingress::{self, Refusals};
use kumo_netstack::smoltcp::iface::{
    Config, Interface, PollResult, SocketHandle, SocketSet, SocketStorage,
};
use kumo_netstack::smoltcp::phy::ChecksumCapabilities;
use kumo_netstack::smoltcp::socket::{dhcpv4, icmp, tcp, udp};
use kumo_netstack::smoltcp::time::Instant;
use kumo_netstack::smoltcp::wire::{
    EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, IpEndpoint,
    IpListenEndpoint, Ipv4Address,
};
use kumo_netstack::socket::{
    decode_request, encode_reply, hello_limits, Addr, Body, Disposition, Op, Rejected, Reply,
    Request, Session, SocketKind, Status, ADDR_LEN, HEADER_LEN, RESULT_MORE, RESULT_TRUNCATED,
    WAIT_CONNECTED, WAIT_READABLE, WAIT_WRITABLE,
};
use kumo_netstack::{FrameDevice, InjectError, Loopback};

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
    /// The same slot's UDP socket on the wire stack; `None` until a frame interface attaches.
    wire_udp: Option<SocketHandle>,
    /// The same slot's ICMP socket on the wire stack (kind ICMP only): created when the slot
    /// binds while a wire is attached, dropped with the generation. `None` otherwise.
    wire_icmp: Option<SocketHandle>,
    /// The next echo-request sequence number this slot sends (NET8).
    icmp_seq: u16,
    tcp: SocketHandle,
    /// The same slot's TCP socket on the wire stack (NET10): created on demand at CONNECT,
    /// LISTEN arming or ACCEPT handoff; dropped with the generation or when the stream ends.
    wire_tcp: Option<SocketHandle>,
    /// Whether this slot's TCP connection or listener lives on the wire stack (NET10).
    tcp_on_wire: bool,
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
    pub echo_requests_sent: u64,
    pub echo_replies_received: u64,
}

/// Why [`Server::push_wire_frame`] refused a frame (NET8's ingress policy, DESIGN/026 §5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngressError {
    /// No wire stack is attached; the frame channel is closed.
    Detached,
    /// The wire device's ingress ring is full.
    Device(InjectError),
    /// Shorter than the Ethernet or IPv4 header it claims.
    Truncated,
    /// Not ARP and not IPv4 (VLAN tags, IPv6, LLDP, anything else).
    UnknownEthertype(u16),
    /// An IPv4 fragment; no reassembly exists by policy.
    Fragment,
    /// The IPv4 header checksum fails.
    BadIpv4Checksum,
}

/// The wire stack: its own smoltcp interface and socket table over the frame channel's
/// device. A shared table would let whichever interface polls first dispatch a socket's
/// egress with its own addresses and medium (smoltcp's `poll` contract), so the wire gets
/// its own `SocketSet` and the Server routes each operation to the right stack.
/// NET11: the DHCP client's receive packet buffer (a DHCP message is far under 576 B).
const DHCP_PACKET_BUFFER: usize = 576;

/// NET12 resolver policy. The UDP receive buffer is deliberately larger than the 512-byte
/// policy cap, so an oversized answer is received and refused by name rather than dropped
/// into a timeout.
const DNS_UDP_BUFFER: usize = 640;
const DNS_TCP_BUFFER: usize = 640;
const DNS_POLICY_MAX: usize = 512;
/// The resolver's own local port on the wire stack (both transports).
const DNS_INTERNAL_PORT: u16 = 47653;
const DNS_UDP_DEADLINE_NS: u64 = 4_000_000_000;
const DNS_TCP_DEADLINE_NS: u64 = 8_000_000_000;
const DNS_TTL_CAP_NS: u64 = 300_000_000_000;
const DNS_NEGATIVE_TTL_NS: u64 = 10_000_000_000;
const DNS_CACHE_MAX: usize = 8;
const DNS_RETRANSMIT_NS: [u64; 3] = [0, 1_000_000_000, 3_000_000_000];

/// NET12: the launcher-granted resolver. Configuration requires an explicit grant; until
/// one arrives every lookup answers NotConfigured, never a guessed server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolverConfig {
    pub server: [u8; 4],
    pub udp_port: u16,
    pub tcp_port: u16,
    /// Whether a truncated UDP answer may fall back to one bounded TCP query.
    pub fallback: bool,
}

/// One in-flight lookup (one at a time keeps the retry budget bounded and auditable).
struct DnsQuery {
    /// Dotted, lowercased name as granted by the client.
    name: Vec<u8>,
    /// Wire-format name (length-prefixed labels plus a zero terminator).
    wire: Vec<u8>,
    txid: u16,
    started_ns: u64,
    next_tx_ns: u64,
    transmissions: u8,
    tcp_attempted: bool,
}

/// The TCP fallback's small state machine.
enum DnsTcp {
    Connecting {
        at_ns: u64,
    },
    Querying {
        at_ns: u64,
        framed: Vec<u8>,
        sent: usize,
        buf: Vec<u8>,
    },
}

/// A cached lookup outcome; failures are cached too, so a polling client cannot drive
/// unbounded retries within the negative TTL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CachedResult {
    Found([u8; 4]),
    NotFound,
    Failed(Status),
}

struct DnsCacheEntry {
    name: Vec<u8>,
    result: CachedResult,
    expires_ns: u64,
}

/// What one DNS response means to the resolver.
enum DnsReply {
    Answer([u8; 4], u32),
    NotFound,
    Truncated,
    Oversized,
    Malformed,
    Rejected,
}

fn dns_query_repr<'a>(txid: u16, wire_name: &'a [u8]) -> kumo_netstack::smoltcp::wire::DnsRepr<'a> {
    use kumo_netstack::smoltcp::wire::{DnsFlags, DnsOpcode, DnsQueryType, DnsQuestion, DnsRepr};
    DnsRepr {
        transaction_id: txid,
        opcode: DnsOpcode::Query,
        flags: DnsFlags::RECURSION_DESIRED,
        question: DnsQuestion {
            name: wire_name,
            type_: DnsQueryType::A,
        },
    }
}

fn dns_packet(buffer: &mut [u8]) -> kumo_netstack::smoltcp::wire::DnsPacket<&mut [u8]> {
    kumo_netstack::smoltcp::wire::DnsPacket::new_unchecked(buffer)
}

/// One response, validated for transaction id, shape, and question name, with answers
/// compared compression-safely. A CNAME rewrites the expected name for the records after
/// it, mirroring the crate's own client.
fn dns_parse_response(data: &[u8], txid: u16, name_wire: &[u8]) -> DnsReply {
    use kumo_netstack::smoltcp::wire::{
        DnsFlags, DnsOpcode, DnsPacket, DnsQueryType, DnsQuestion, DnsRcode, DnsRecord,
        DnsRecordData,
    };
    let Ok(packet) = DnsPacket::new_checked(data) else {
        return DnsReply::Malformed;
    };
    if packet.transaction_id() != txid {
        return DnsReply::Rejected;
    }
    if packet.opcode() != DnsOpcode::Query || !packet.flags().contains(DnsFlags::RESPONSE) {
        return DnsReply::Malformed;
    }
    match packet.rcode() {
        DnsRcode::NoError => {}
        DnsRcode::NXDomain => return DnsReply::NotFound,
        _ => return DnsReply::NotFound,
    }
    if packet.flags().contains(DnsFlags::TRUNCATED) {
        return DnsReply::Truncated;
    }
    if packet.question_count() != 1 {
        return DnsReply::Malformed;
    }
    let Ok((payload, question)) = DnsQuestion::parse(packet.payload()) else {
        return DnsReply::Malformed;
    };
    if question.type_ != DnsQueryType::A || !dns_names_eq(&packet, question.name, name_wire) {
        return DnsReply::Rejected;
    }
    let mut current = name_wire.to_vec();
    let mut addr = None;
    let mut ttl = u32::MAX;
    let mut rest = payload;
    for _ in 0..packet.answer_record_count() {
        let Ok((next, record)) = DnsRecord::parse(rest) else {
            return DnsReply::Malformed;
        };
        rest = next;
        if !dns_names_eq(&packet, record.name, &current) {
            continue;
        }
        match record.data {
            DnsRecordData::A(a) => {
                if addr.is_none() {
                    addr = Some(a.octets());
                }
                ttl = ttl.min(record.ttl);
            }
            DnsRecordData::Cname(name) => match dns_wire_from(&packet, name) {
                Some(wire) => current = wire,
                None => return DnsReply::Malformed,
            },
            _ => {}
        }
    }
    match addr {
        Some(addr) => DnsReply::Answer(addr, ttl),
        None => DnsReply::NotFound,
    }
}

fn dns_names_eq(
    packet: &kumo_netstack::smoltcp::wire::DnsPacket<&[u8]>,
    a: &[u8],
    b: &[u8],
) -> bool {
    let mut ia = packet.parse_name(a);
    let mut ib = packet.parse_name(b);
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return true,
            (Some(Ok(x)), Some(Ok(y))) => {
                if !x.eq_ignore_ascii_case(y) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

fn dns_wire_from(
    packet: &kumo_netstack::smoltcp::wire::DnsPacket<&[u8]>,
    name: &[u8],
) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for label in packet.parse_name(name) {
        let label = label.ok()?;
        if label.is_empty() || label.len() > 63 || out.len() + label.len() + 2 > 255 {
            return None;
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label);
    }
    out.push(0);
    Some(out)
}

/// Validate a dotted name (labels 1..=63, wire form at most 255 bytes) and normalize it to
/// its lowercased cache key.
fn normalize_dns_name(name: &[u8]) -> Result<Vec<u8>, Status> {
    if name.is_empty() || name.len() > 253 {
        return Err(Status::InvalidArgument);
    }
    for label in name.split(|&c| c == b'.') {
        if label.is_empty() || label.len() > 63 {
            return Err(Status::InvalidArgument);
        }
    }
    Ok(name.iter().map(|c| c.to_ascii_lowercase()).collect())
}

fn dotted_to_wire(dotted: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(dotted.len() + 2);
    for label in dotted.split(|&c| c == b'.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label);
    }
    out.push(0);
    (out.len() <= 255).then_some(out)
}

/// NET11: how the wire interface is configured, chosen by the launcher's frame grant.
#[derive(Clone, Copy)]
pub enum WireConfig {
    /// The fixed LAN address (the pre-NET11 behavior, unchanged).
    Static { addr: [u8; 4], prefix: u8 },
    /// DHCP with the granted port pair: 67/68 in production, the isolated test server's
    /// ports in the proof. The client socket lives in this generation's wire stack.
    Dhcp { server_port: u16, client_port: u16 },
}

/// NET11: lease lifecycle events, drained by the server loop and logged as NOTICE lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireEvent {
    DhcpLease {
        addr: [u8; 4],
        prefix: u8,
        router: Option<[u8; 4]>,
    },
    DhcpRenewed {
        addr: [u8; 4],
        prefix: u8,
    },
    DhcpExpired,
    DhcpDiscardedByGeneration,
    DnsQuerySent,
    DnsAnswer {
        addr: [u8; 4],
    },
    DnsNotFound,
    DnsMalformed,
    DnsRejected,
    DnsOversized,
    DnsTruncated,
    DnsFallback,
    DnsTimedOut,
}

struct Wire {
    device: Box<FrameDevice>,
    iface: Interface,
    sockets: SocketSet<'static>,
    mac: [u8; 6],
    config: WireConfig,
    /// The address the stack currently answers on: the grant's, or the DHCP lease's.
    addr: [u8; 4],
    prefix: u8,
    /// NET11: the DHCP socket in this generation's wire set (DHCP mode only).
    dhcp: Option<SocketHandle>,
    /// NET12: the resolver's stack-internal sockets in this generation's wire set.
    dns_udp: Option<SocketHandle>,
    dns_tcp: Option<SocketHandle>,
    /// NET8's ingress refusals: frames the policy kept out of the stack, by kind.
    refusals: Refusals,
}

pub struct Server {
    /// NET11 lease events awaiting the loop's NOTICE logging.
    wire_events: Vec<WireEvent>,
    /// NET12 resolver state: the granted configuration, the one in-flight lookup, its TCP
    /// fallback, and a small TTL-bounded cache.
    resolver: Option<ResolverConfig>,
    dns_query: Option<DnsQuery>,
    dns_tcp_state: Option<DnsTcp>,
    dns_cache: Vec<DnsCacheEntry>,
    dns_tx_counter: u32,
    iface: Interface,
    /// Boxed: the 16-slot ring is ~24 KB, more than a native child's 16 KiB default stack.
    device: Box<Loopback>,
    sockets: SocketSet<'static>,
    wire: Option<Wire>,
    slots: Vec<Slot>,
    sessions: Vec<Option<SessionSlot>>,
    next_generation: u32,
    generation_end: u32,
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

/// The wire device, built directly on the heap: its rings are ~24 KiB, larger than a native
/// child's 16 KiB default stack. All-zero is exactly `FrameDevice::new()` (empty rings).
fn boxed_frame_device() -> Box<FrameDevice> {
    // SAFETY: every field of `FrameDevice` is an integer or an array of integers, for which
    // zero is a valid value, and zero is the empty rings `FrameDevice::new()` builds.
    unsafe { Box::<FrameDevice>::new_zeroed().assume_init() }
}

/// One wire ICMP socket (NET8): the slot's echo identifier, buffers from the ICMP budget.
fn wire_icmp_socket(ident: u16) -> icmp::Socket<'static> {
    let mut socket = icmp::Socket::new(
        icmp::PacketBuffer::new(
            leak(icmp::PacketMetadata::EMPTY, ICMP_METADATA),
            leak(0u8, ICMP_BUFFER),
        ),
        icmp::PacketBuffer::new(
            leak(icmp::PacketMetadata::EMPTY, ICMP_METADATA),
            leak(0u8, ICMP_BUFFER),
        ),
    );
    let _ = socket.bind(icmp::Endpoint::Ident(ident));
    socket
}

/// One wire TCP socket, created on demand (NET10). Buffers come from the TCP budget.
fn wire_tcp_socket() -> tcp::Socket<'static> {
    tcp::Socket::new(
        tcp::SocketBuffer::new(leak(0u8, SOCKET_BUFFER)),
        tcp::SocketBuffer::new(leak(0u8, SOCKET_BUFFER)),
    )
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
                wire_udp: None,
                wire_icmp: None,
                icmp_seq: 0,
                tcp: sockets.add(tcp),
                wire_tcp: None,
                tcp_on_wire: false,
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
            wire: None,
            wire_events: Vec::new(),
            resolver: None,
            dns_query: None,
            dns_tcp_state: None,
            dns_cache: Vec::new(),
            dns_tx_counter: 0,
            slots,
            sessions: (0..SESSIONS).map(|_| None).collect(),
            next_generation: 1,
            generation_end: u32::MAX,
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

    /// Sora assigns a nonzero instance epoch. Each epoch owns one disjoint
    /// 65535-session range; exhaustion refuses rather than reusing a token.
    /// Old channel capabilities are never attached to a replacement. — TIMBERDOODLE
    pub fn for_instance(now_ns: u64, epoch: u16) -> Option<Self> {
        if epoch == 0 {
            return None;
        }
        let mut server = Self::new(now_ns);
        server.next_generation = (u32::from(epoch) << 16) | 1;
        server.generation_end = (u32::from(epoch) << 16) | 0xffff;
        Some(server)
    }

    /// Attach (or replace) the wire stack from a frame-provider HELLO: the interface takes the
    /// provider's MAC and the Sora-granted address (DESIGN/026 §3; DHCP is NET11). Every slot
    /// gains a wire UDP twin bound to the same port, so a bound port is one namespace reachable
    /// from both stacks.
    /// The pre-NET11 static configuration, kept as the launcher's default.
    pub fn attach_wire(&mut self, now_ns: u64, mac: [u8; 6], addr: [u8; 4], prefix: u8) {
        self.attach_wire_config(now_ns, mac, WireConfig::Static { addr, prefix });
    }

    /// NET11: DHCP configuration; the port pair is the granted one.
    pub fn attach_wire_dhcp(
        &mut self,
        now_ns: u64,
        mac: [u8; 6],
        server_port: u16,
        client_port: u16,
    ) {
        self.attach_wire_config(
            now_ns,
            mac,
            WireConfig::Dhcp {
                server_port,
                client_port,
            },
        );
    }

    pub fn attach_wire_config(&mut self, now_ns: u64, mac: [u8; 6], config: WireConfig) {
        // No resolver state survives a different interface generation either (NET12).
        self.dns_reset();
        // A lease never survives a different interface generation silently (NET11).
        if let Some(old) = self.wire.as_ref() {
            if matches!(old.config, WireConfig::Dhcp { .. }) && old.addr != [0; 4] {
                self.wire_events.push(WireEvent::DhcpDiscardedByGeneration);
            }
        }
        let mut device = boxed_frame_device();
        let mut iface = Interface::new(
            Config::new(HardwareAddress::Ethernet(EthernetAddress(mac))),
            &mut *device,
            instant(now_ns),
        );
        if let WireConfig::Static { addr, prefix } = config {
            iface.update_ip_addrs(|addrs| {
                let _ = addrs.push(IpCidr::new(
                    IpAddress::Ipv4(Ipv4Address::from(addr)),
                    prefix,
                ));
            });
        }
        // Half the table is the per-slot UDP twins; the rest serves the ICMP (NET8) and TCP
        // twin sockets created on demand (NET10): a slot is one kind at a time, so one
        // on-demand socket per slot fits the reserve exactly. NET11 adds the DHCP socket.
        let storage: &'static mut [SocketStorage<'static>] = Box::leak(
            (0..2 * SOCKETS_TOTAL + 3)
                .map(|_| SocketStorage::EMPTY)
                .collect(),
        );
        let mut sockets = SocketSet::new(storage);
        let mut handles = Vec::with_capacity(SOCKETS_TOTAL);
        // One slab per generation for every twin's receive queue: 32 separate ~96 KiB
        // allocations would each claim a heap region, and the growable heap has 32.
        let mut rx_meta = leak(
            udp::PacketMetadata::EMPTY,
            SOCKETS_TOTAL * WIRE_UDP_RX_METADATA,
        );
        let mut rx_bytes = leak(0u8, SOCKETS_TOTAL * WIRE_UDP_RX_BUFFER);
        for _ in 0..SOCKETS_TOTAL {
            let (meta, meta_rest) =
                core::mem::take(&mut rx_meta).split_at_mut(WIRE_UDP_RX_METADATA);
            rx_meta = meta_rest;
            let (bytes, bytes_rest) =
                core::mem::take(&mut rx_bytes).split_at_mut(WIRE_UDP_RX_BUFFER);
            rx_bytes = bytes_rest;
            let udp = udp::Socket::new(
                udp::PacketBuffer::new(meta, bytes),
                udp::PacketBuffer::new(
                    leak(udp::PacketMetadata::EMPTY, UDP_METADATA),
                    leak(0u8, SOCKET_BUFFER),
                ),
            );
            handles.push(sockets.add(udp));
        }
        for (slot, handle) in self.slots.iter_mut().zip(handles) {
            slot.wire_udp = Some(handle);
            slot.wire_icmp = None;
            if slot.port != 0 && slot.kind == SocketKind::Udp && slot.owner.is_some() {
                let endpoint = IpListenEndpoint {
                    addr: None,
                    port: slot.port,
                };
                let _ = sockets.get_mut::<udp::Socket>(handle).bind(endpoint);
            }
            // A bound ICMP client socket gets a fresh wire socket per generation, the same
            // rule as the UDP twin above (NET8).
            if slot.port != 0 && slot.kind == SocketKind::Icmp && slot.owner.is_some() {
                slot.wire_icmp = Some(sockets.add(wire_icmp_socket(slot.port)));
            }
        }
        let dns_udp = {
            let socket = udp::Socket::new(
                udp::PacketBuffer::new(
                    leak(udp::PacketMetadata::EMPTY, UDP_METADATA),
                    leak(0u8, DNS_UDP_BUFFER),
                ),
                udp::PacketBuffer::new(
                    leak(udp::PacketMetadata::EMPTY, UDP_METADATA),
                    leak(0u8, DNS_UDP_BUFFER),
                ),
            );
            let handle = sockets.add(socket);
            let _ = sockets
                .get_mut::<udp::Socket>(handle)
                .bind(IpListenEndpoint {
                    addr: None,
                    port: DNS_INTERNAL_PORT,
                });
            Some(handle)
        };
        let dns_tcp = Some(sockets.add(tcp::Socket::new(
            tcp::SocketBuffer::new(leak(0u8, DNS_TCP_BUFFER)),
            tcp::SocketBuffer::new(leak(0u8, DNS_TCP_BUFFER)),
        )));
        let dhcp = match config {
            WireConfig::Static { .. } => None,
            WireConfig::Dhcp {
                server_port,
                client_port,
            } => {
                let mut socket = dhcpv4::Socket::new();
                socket.set_ports(server_port, client_port);
                socket.set_receive_packet_buffer(leak(0u8, DHCP_PACKET_BUFFER));
                Some(sockets.add(socket))
            }
        };
        let (lease_addr, lease_prefix) = match config {
            WireConfig::Static { addr, prefix } => (addr, prefix),
            WireConfig::Dhcp { .. } => ([0; 4], 0),
        };
        self.wire = Some(Wire {
            device,
            iface,
            sockets,
            mac,
            config,
            addr: lease_addr,
            prefix: lease_prefix,
            dhcp,
            dns_udp,
            dns_tcp,
            refusals: Refusals::default(),
        });
        // NET10: a listener is reachable from both stacks. Every armed backlog slot gains a
        // wire listener twin for this generation, and wire-bound streams from the previous
        // generation are gone: their slots read as reset through the untouched loopback
        // socket (Closed without a FIN), never as a live connection.
        for index in 0..self.slots.len() {
            let hidden = self.slots[index].listener.is_some();
            let kind = self.slots[index].kind;
            let port = self.slots[index].port;
            self.slots[index].wire_tcp = None;
            self.slots[index].tcp_on_wire = false;
            if hidden && kind == SocketKind::Tcp {
                if let Some(handle) = self.ensure_wire_tcp(index) {
                    let wire = self.wire.as_mut().expect("a fresh generation");
                    let _ = wire.sockets.get_mut::<tcp::Socket>(handle).listen(port);
                }
            }
        }
    }

    /// NET12: the launcher's resolver grant. Until it arrives every lookup answers
    /// NotConfigured; a new grant discards any in-flight lookup and the cache.
    pub fn set_resolver(&mut self, config: ResolverConfig) {
        self.resolver = Some(config);
        self.dns_reset();
    }

    fn dns_reset(&mut self) {
        self.dns_query = None;
        self.dns_tcp_state = None;
        self.dns_cache.clear();
    }

    /// NET11: drain one lease event for the server loop's NOTICE logging.
    pub fn take_wire_event(&mut self) -> Option<WireEvent> {
        if self.wire_events.is_empty() {
            None
        } else {
            Some(self.wire_events.remove(0))
        }
    }

    /// A new provider generation: discard wire-bound state (DESIGN/026 §3) and rebuild the
    /// stack over the same grant. Loopback sockets are untouched.
    pub fn reset_wire(&mut self, now_ns: u64) {
        let Some(wire) = self.wire.take() else {
            return;
        };
        if matches!(wire.config, WireConfig::Dhcp { .. }) && wire.addr != [0; 4] {
            self.wire_events.push(WireEvent::DhcpDiscardedByGeneration);
        }
        self.attach_wire_config(now_ns, wire.mac, wire.config);
    }

    /// The frame channel closed: the stack and every wire twin go with it.
    pub fn detach_wire(&mut self) {
        self.dns_reset();
        if let Some(wire) = self.wire.as_ref() {
            if matches!(wire.config, WireConfig::Dhcp { .. }) && wire.addr != [0; 4] {
                self.wire_events.push(WireEvent::DhcpDiscardedByGeneration);
            }
        }
        if self.wire.take().is_some() {
            for slot in &mut self.slots {
                slot.wire_udp = None;
                slot.wire_icmp = None;
                slot.wire_tcp = None;
                slot.tcp_on_wire = false;
            }
        }
    }

    pub fn wire_attached(&self) -> bool {
        self.wire.is_some()
    }

    /// Frames smoltcp built on the wire stack, waiting to be handed over as TX_FRAMEs.
    pub fn take_wire_frame(&mut self, out: &mut [u8; FRAME_MAX]) -> Option<usize> {
        self.wire.as_mut()?.device.take_outbound(out)
    }

    /// Queue a frame the provider delivered as an RX_FRAME, after NET8's ingress policy:
    /// unknown EtherTypes, IPv4 fragments and bad IPv4 header checksums are refused and
    /// counted here, so smoltcp only ever sees bounded, well-formed input.
    pub fn push_wire_frame(&mut self, frame: &[u8]) -> Result<(), IngressError> {
        let Some(wire) = self.wire.as_mut() else {
            return Err(IngressError::Detached);
        };
        match ingress::classify(frame) {
            ingress::Verdict::Pass => wire
                .device
                .push_inbound(frame)
                .map_err(IngressError::Device),
            verdict => {
                wire.refusals.count(verdict);
                Err(match verdict {
                    ingress::Verdict::Truncated => IngressError::Truncated,
                    ingress::Verdict::UnknownEthertype(raw) => IngressError::UnknownEthertype(raw),
                    ingress::Verdict::Fragment => IngressError::Fragment,
                    ingress::Verdict::BadIpv4Checksum => IngressError::BadIpv4Checksum,
                    ingress::Verdict::Pass => unreachable!(),
                })
            }
        }
    }

    /// NET8 wire ingress refusals; `None` while no frame interface is attached.
    pub fn wire_refusals(&self) -> Option<Refusals> {
        self.wire.as_ref().map(|wire| wire.refusals)
    }

    /// Frames the wire stack has consumed; the process returns credits from its delta.
    pub fn wire_delivered(&self) -> u64 {
        self.wire
            .as_ref()
            .map_or(0, |wire| wire.device.counters().delivered)
    }

    /// Frames smoltcp has built but the channel has not taken yet.
    pub fn wire_pending_outbound(&self) -> usize {
        self.wire
            .as_ref()
            .map_or(0, |wire| wire.device.pending_outbound())
    }

    /// Inbound frames the wire stack refused because its ingress ring was full.
    pub fn wire_ingress_full(&self) -> u64 {
        self.wire
            .as_ref()
            .map_or(0, |wire| wire.device.counters().ingress_full)
    }

    /// Admit a new session; `None` when all [`SESSIONS`] are in use (the caller closes the
    /// channel unanswered). Generations are never reused by this instance.
    pub fn open_session(&mut self) -> Option<usize> {
        let index = self.sessions.iter().position(Option::is_none)?;
        let generation = self.next_generation;
        if generation == 0 {
            return None;
        }
        self.next_generation = if generation == self.generation_end {
            0
        } else {
            generation + 1
        };
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
            && self.tcp_socket(index).state() != tcp::State::Closed;
        self.tcp_socket_mut(index).abort();
        self.forget(index);
        if tcp_live {
            // Zero deadline: free on the next poll, after the RST has been dispatched.
            self.slots[index].draining = Some(0);
        } else {
            self.release_wire_tcp(index);
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
        if let (Some(wire), Some(handle)) = (self.wire.as_mut(), slot.wire_udp) {
            wire.sockets.get_mut::<udp::Socket>(handle).close();
        }
        if let (Some(wire), Some(handle)) = (self.wire.as_mut(), slot.wire_icmp.take()) {
            wire.sockets.remove(handle);
        }
        slot.icmp_seq = 0;
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
            Answer::Batch { socket, replies } => {
                let last = replies.len().saturating_sub(1);
                for (i, (result, payload)) in replies.iter().enumerate() {
                    let more = if i < last { RESULT_MORE } else { 0 };
                    self.push_reply(
                        session,
                        op as u16,
                        request.request_id,
                        socket,
                        Status::Ok,
                        result | more,
                        payload,
                        out,
                    );
                }
                // One admitted request, one answer, however many replies carried it.
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
            if let (Some(wire), Some(handle)) = (self.wire.as_mut(), self.slots[index].wire_udp) {
                wire.sockets
                    .get_mut::<udp::Socket>(handle)
                    .bind(endpoint)
                    .map_err(|_| Status::InvalidArgument)?;
            }
        }
        if kind == SocketKind::Icmp {
            // No wire yet records the identifier only; attach_wire creates the socket.
            if let Some(wire) = self.wire.as_mut() {
                let handle = wire.sockets.add(wire_icmp_socket(port));
                self.slots[index].wire_icmp = Some(handle);
            }
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

    /// Bind the slot's wire TCP twin, creating it on demand (NET10); `None` without a wire.
    /// The twin is reset so a previous connection or listener never leaks into the next one.
    fn ensure_wire_tcp(&mut self, index: usize) -> Option<SocketHandle> {
        let wire = self.wire.as_mut()?;
        let handle = match self.slots[index].wire_tcp {
            Some(handle) => handle,
            None => {
                let handle = wire.sockets.add(wire_tcp_socket());
                self.slots[index].wire_tcp = Some(handle);
                handle
            }
        };
        wire.sockets.get_mut::<tcp::Socket>(handle).abort();
        Some(handle)
    }

    /// Drop the slot's wire TCP twin; a generation change takes the whole socket set with it.
    fn release_wire_tcp(&mut self, index: usize) {
        if let (Some(wire), Some(handle)) = (self.wire.as_mut(), self.slots[index].wire_tcp.take())
        {
            wire.sockets.remove(handle);
        }
        self.slots[index].tcp_on_wire = false;
    }

    /// The slot's routed TCP socket: its wire twin while the slot lives on the wire stack,
    /// the loopback socket otherwise (NET10).
    fn tcp_socket(&self, index: usize) -> &tcp::Socket<'static> {
        let slot = &self.slots[index];
        if slot.tcp_on_wire {
            let handle = slot.wire_tcp.expect("a wire TCP slot owns its twin");
            self.wire
                .as_ref()
                .expect("a wire TCP slot has a wire stack")
                .sockets
                .get::<tcp::Socket>(handle)
        } else {
            self.sockets.get::<tcp::Socket>(slot.tcp)
        }
    }

    fn tcp_socket_mut(&mut self, index: usize) -> &mut tcp::Socket<'static> {
        let on_wire = self.slots[index].tcp_on_wire;
        if on_wire {
            let handle = self.slots[index]
                .wire_tcp
                .expect("a wire TCP slot owns its twin");
            self.wire
                .as_mut()
                .expect("a wire TCP slot has a wire stack")
                .sockets
                .get_mut::<tcp::Socket>(handle)
        } else {
            let handle = self.slots[index].tcp;
            self.sockets.get_mut::<tcp::Socket>(handle)
        }
    }

    fn send_datagram(&mut self, index: usize, to: Addr, data: &[u8]) -> Result<u64, Status> {
        let to_wire = !is_loopback(to.ip);
        if to_wire && self.wire.is_none() {
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
        let sent = if to_wire {
            let Some(wire) = self.wire.as_mut() else {
                return Err(Status::NetUnreachable);
            };
            let handle = self.slots[index].wire_udp.ok_or(Status::NetUnreachable)?;
            wire.sockets
                .get_mut::<udp::Socket>(handle)
                .send_slice(data, endpoint_of(to))
        } else {
            self.sockets
                .get_mut::<udp::Socket>(self.slots[index].udp)
                .send_slice(data, endpoint_of(to))
        };
        match sent {
            Ok(()) => {
                self.stats.datagrams_sent += 1;
                Ok(data.len() as u64)
            }
            Err(udp::SendError::BufferFull) => Err(Status::WouldBlock),
            // NET11: with no lease the interface has no address to send from.
            Err(udp::SendError::Unaddressable) => Err(Status::NetUnreachable),
        }
    }

    /// Promote a CONNECT that completed, and remember a FIN once seen, so a later Closed state
    /// can be told apart from a reset.
    fn observe(&mut self, index: usize) {
        if self.slots[index].kind != SocketKind::Tcp {
            return;
        }
        let (state, finished) = {
            let socket = self.tcp_socket(index);
            let state = socket.state();
            (
                state,
                !socket.may_recv()
                    && !matches!(
                        state,
                        tcp::State::Closed | tcp::State::SynSent | tcp::State::Listen
                    ),
            )
        };
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
            let wire_socket = slot.wire_udp.and_then(|handle| {
                self.wire
                    .as_ref()
                    .map(|wire| wire.sockets.get::<udp::Socket>(handle))
            });
            let mut mask = 0;
            if socket.can_recv() || wire_socket.is_some_and(|wire| wire.can_recv()) {
                mask |= WAIT_READABLE;
            }
            if slot.port == 0
                || socket.can_send()
                || wire_socket.is_some_and(|wire| wire.can_send())
            {
                mask |= WAIT_WRITABLE;
            }
            if slot.peer.is_some() {
                mask |= WAIT_CONNECTED;
            }
            return Ok(mask);
        }
        if slot.kind == SocketKind::Icmp {
            let wire_socket = slot.wire_icmp.and_then(|handle| {
                self.wire
                    .as_ref()
                    .map(|wire| wire.sockets.get::<icmp::Socket>(handle))
            });
            let mut mask = 0;
            if wire_socket.is_some_and(|socket| socket.can_recv()) {
                mask |= WAIT_READABLE;
            }
            if wire_socket.is_some_and(|socket| socket.can_send()) {
                mask |= WAIT_WRITABLE;
            }
            if slot.peer.is_some() {
                mask |= WAIT_CONNECTED;
            }
            return Ok(mask);
        }
        let socket = self.tcp_socket(index);
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

    /// A backlog slot of `listener` whose connection is established and can be accepted,
    /// with whether it arrived on the wire stack (NET10).
    fn acceptable(&self, listener: usize) -> Option<(usize, bool)> {
        fn established(socket: &tcp::Socket<'_>) -> bool {
            matches!(
                socket.state(),
                tcp::State::Established | tcp::State::CloseWait
            )
        }
        for (index, slot) in self.slots.iter().enumerate() {
            if !matches!(slot.listener, Some((l, _)) if l == listener) {
                continue;
            }
            if established(self.sockets.get::<tcp::Socket>(slot.tcp)) {
                return Some((index, false));
            }
            if let (Some(wire), Some(handle)) = (self.wire.as_ref(), slot.wire_tcp) {
                if established(wire.sockets.get::<tcp::Socket>(handle)) {
                    return Some((index, true));
                }
            }
        }
        None
    }

    /// Arm one more hidden listener for `listener`; false when no slot is free. With a wire
    /// attached the hidden slot listens on both stacks: a bound port is one namespace.
    fn arm_backlog(&mut self, listener: usize, session: usize) -> bool {
        let port = self.slots[listener].port;
        let Some(index) = self.slots.iter().position(Slot::is_free) else {
            return false;
        };
        {
            let slot = &mut self.slots[index];
            slot.kind = SocketKind::Tcp;
            slot.port = port;
            slot.listener = Some((listener, session));
            slot.tcp_on_wire = false;
        }
        let armed = self
            .sockets
            .get_mut::<tcp::Socket>(self.slots[index].tcp)
            .listen(port)
            .is_ok();
        if !armed {
            self.forget(index);
            return false;
        }
        if self.wire.is_some() {
            let Some(handle) = self.ensure_wire_tcp(index) else {
                self.forget(index);
                return false;
            };
            let armed = self
                .wire
                .as_mut()
                .expect("a wire was just checked")
                .sockets
                .get_mut::<tcp::Socket>(handle)
                .listen(port)
                .is_ok();
            if !armed {
                self.release_wire_tcp(index);
                self.forget(index);
                return false;
            }
        }
        true
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
        if let Body::Resolve(name) = request.body {
            return self.dispatch_resolve(name, now_ns);
        }
        let index = self.slot_of(session, socket)?;
        match self.slots[index].kind {
            SocketKind::Udp => self.dispatch_udp(session, index, request, now_ns),
            SocketKind::Tcp => self.dispatch_tcp(session, index, request, now_ns),
            SocketKind::Icmp => self.dispatch_icmp(session, index, request, now_ns),
        }
    }

    /// NET12: resolve a dotted name through the launcher-granted resolver. Every path is
    /// bounded: three UDP transmissions over four seconds, at most one TCP fallback under
    /// the policy, and failures are negative-cached so polling cannot retry without bound.
    fn dispatch_resolve(&mut self, name: &[u8], now_ns: u64) -> Result<Answer, Status> {
        let Some(config) = self.resolver else {
            return Err(Status::NotConfigured);
        };
        if self.wire.is_none() {
            return Err(Status::NetUnreachable);
        }
        let name = normalize_dns_name(name)?;
        if let Some(cached) = self.dns_cache_lookup(&name, now_ns) {
            return match cached {
                CachedResult::Found(addr) => Ok(Answer::ok(0, u64::from(u32::from_be_bytes(addr)))),
                CachedResult::NotFound => Err(Status::NotFound),
                CachedResult::Failed(status) => Err(status),
            };
        }
        if let Some(query) = self.dns_query.as_ref() {
            return Err(if query.name == name {
                Status::InProgress
            } else {
                Status::WouldBlock
            });
        }
        let Some(wire_name) = dotted_to_wire(&name) else {
            return Err(Status::InvalidArgument);
        };
        self.dns_tx_counter = self.dns_tx_counter.wrapping_add(1);
        let txid = (self.dns_tx_counter.wrapping_mul(2_654_435_761) ^ (now_ns as u32)) as u16;
        self.dns_query = Some(DnsQuery {
            name,
            wire: wire_name,
            txid,
            started_ns: now_ns,
            next_tx_ns: now_ns,
            transmissions: 0,
            tcp_attempted: !config.fallback,
        });
        self.dns_transmit(now_ns)?;
        self.wire_events.push(WireEvent::DnsQuerySent);
        Err(Status::InProgress)
    }

    fn dns_transmit(&mut self, now_ns: u64) -> Result<(), Status> {
        let Some(config) = self.resolver else {
            return Err(Status::NotConfigured);
        };
        let Some(wire) = self.wire.as_mut() else {
            return Err(Status::NetUnreachable);
        };
        let Some(handle) = wire.dns_udp else {
            return Err(Status::NetUnreachable);
        };
        let Some(query) = self.dns_query.as_mut() else {
            return Err(Status::WouldBlock);
        };
        let repr = dns_query_repr(query.txid, &query.wire);
        let mut payload = [0u8; DNS_POLICY_MAX];
        let len = repr.buffer_len();
        if len > payload.len() {
            return Err(Status::MessageTooLarge);
        }
        let mut packet = dns_packet(&mut payload[..len]);
        repr.emit(&mut packet);
        let endpoint = IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Address::from(config.server)),
            config.udp_port,
        );
        wire.sockets
            .get_mut::<udp::Socket>(handle)
            .send_slice(&payload[..len], endpoint)
            .map_err(|_| Status::NetUnreachable)?;
        query.transmissions = query.transmissions.saturating_add(1);
        // The schedule is absolute offsets from the query's start: 0, 1 s, 3 s.
        let step = usize::from(query.transmissions.min(2));
        query.next_tx_ns = query.started_ns.saturating_add(DNS_RETRANSMIT_NS[step]);
        Ok(())
    }

    /// NET12: drive the in-flight lookup. Called once per stack turn.
    fn dns_poll(&mut self, now_ns: u64) {
        let Some(config) = self.resolver else {
            return;
        };
        self.dns_purge_cache(now_ns);
        loop {
            let reply = {
                let Some(wire) = self.wire.as_mut() else {
                    return;
                };
                let Some(handle) = wire.dns_udp else {
                    return;
                };
                match wire.sockets.get_mut::<udp::Socket>(handle).recv() {
                    Ok((data, meta)) => Some((data.to_vec(), meta.endpoint)),
                    Err(_) => None,
                }
            };
            let Some((data, from)) = reply else {
                break;
            };
            let expect = IpEndpoint::new(
                IpAddress::Ipv4(Ipv4Address::from(config.server)),
                config.udp_port,
            );
            if from != expect {
                self.wire_events.push(WireEvent::DnsRejected);
                continue;
            }
            let Some((txid, wire_name, name_key)) = self
                .dns_query
                .as_ref()
                .map(|q| (q.txid, q.wire.clone(), q.name.clone()))
            else {
                continue;
            };
            let verdict = if data.len() > DNS_POLICY_MAX {
                DnsReply::Oversized
            } else {
                dns_parse_response(&data, txid, &wire_name)
            };
            if self.dns_settle(verdict, now_ns, config, &name_key, true) {
                continue;
            }
        }
        if self.dns_query.is_none() {
            return;
        }
        if self.dns_tcp_state.is_some() {
            self.dns_poll_tcp(now_ns, config);
            return;
        }
        let (started, attempts, next_tx) = {
            let Some(query) = self.dns_query.as_ref() else {
                return;
            };
            (query.started_ns, query.transmissions, query.next_tx_ns)
        };
        if now_ns >= started.saturating_add(DNS_UDP_DEADLINE_NS) {
            self.dns_fail(now_ns, Status::TimedOut, WireEvent::DnsTimedOut);
            return;
        }
        if attempts < 3 && now_ns >= next_tx {
            let _ = self.dns_transmit(now_ns);
        }
    }

    /// Apply a response verdict. Returns false when the query slot was freed, so the caller
    /// stops draining stale replies for it.
    fn dns_settle(
        &mut self,
        verdict: DnsReply,
        now_ns: u64,
        config: ResolverConfig,
        name_key: &[u8],
        udp: bool,
    ) -> bool {
        match verdict {
            DnsReply::Answer(addr, ttl_s) => {
                if ttl_s > 0 {
                    let ttl = u64::from(ttl_s)
                        .saturating_mul(1_000_000_000)
                        .min(DNS_TTL_CAP_NS);
                    self.dns_cache_insert(
                        name_key.to_vec(),
                        CachedResult::Found(addr),
                        now_ns.saturating_add(ttl),
                    );
                }
                self.dns_query = None;
                self.dns_tcp_state = None;
                self.wire_events.push(WireEvent::DnsAnswer { addr });
                false
            }
            DnsReply::NotFound => {
                self.dns_cache_insert(
                    name_key.to_vec(),
                    CachedResult::NotFound,
                    now_ns.saturating_add(DNS_NEGATIVE_TTL_NS),
                );
                self.dns_query = None;
                self.dns_tcp_state = None;
                self.wire_events.push(WireEvent::DnsNotFound);
                false
            }
            DnsReply::Truncated if udp && config.fallback => {
                let attempted = self.dns_query.as_ref().is_some_and(|q| q.tcp_attempted);
                if !attempted {
                    if let Some(query) = self.dns_query.as_mut() {
                        query.tcp_attempted = true;
                    }
                    if self.dns_start_tcp(now_ns, config) {
                        self.wire_events.push(WireEvent::DnsFallback);
                        return true;
                    }
                    self.dns_fail(now_ns, Status::TimedOut, WireEvent::DnsTimedOut);
                    return false;
                }
                true
            }
            DnsReply::Truncated => {
                self.dns_fail(now_ns, Status::MessageTooLarge, WireEvent::DnsTruncated);
                false
            }
            DnsReply::Oversized => {
                self.dns_fail(now_ns, Status::MessageTooLarge, WireEvent::DnsOversized);
                false
            }
            DnsReply::Malformed => {
                self.wire_events.push(WireEvent::DnsMalformed);
                true
            }
            DnsReply::Rejected => {
                self.wire_events.push(WireEvent::DnsRejected);
                true
            }
        }
    }

    fn dns_fail(&mut self, now_ns: u64, status: Status, event: WireEvent) {
        if let Some(query) = self.dns_query.take() {
            self.dns_cache_insert(
                query.name,
                CachedResult::Failed(status),
                now_ns.saturating_add(DNS_NEGATIVE_TTL_NS),
            );
        }
        self.dns_tcp_state = None;
        self.wire_events.push(event);
    }

    fn dns_start_tcp(&mut self, now_ns: u64, config: ResolverConfig) -> bool {
        let Some(wire) = self.wire.as_mut() else {
            return false;
        };
        let Some(handle) = wire.dns_tcp else {
            return false;
        };
        let remote = IpEndpoint::new(
            IpAddress::Ipv4(Ipv4Address::from(config.server)),
            config.tcp_port,
        );
        if wire
            .sockets
            .get_mut::<tcp::Socket>(handle)
            .connect(wire.iface.context(), remote, DNS_INTERNAL_PORT)
            .is_err()
        {
            return false;
        }
        self.dns_tcp_state = Some(DnsTcp::Connecting { at_ns: now_ns });
        true
    }

    fn dns_poll_tcp(&mut self, now_ns: u64, config: ResolverConfig) {
        let Some(state) = self.dns_tcp_state.take() else {
            return;
        };
        match state {
            DnsTcp::Connecting { at_ns } => {
                let state_now = {
                    let Some(wire) = self.wire.as_mut() else {
                        return;
                    };
                    let Some(handle) = wire.dns_tcp else {
                        return;
                    };
                    wire.sockets.get_mut::<tcp::Socket>(handle).state()
                };
                match state_now {
                    tcp::State::Established => {
                        let Some((txid, wire_name, _)) = self
                            .dns_query
                            .as_ref()
                            .map(|q| (q.txid, q.wire.clone(), q.name.clone()))
                        else {
                            return;
                        };
                        let repr = dns_query_repr(txid, &wire_name);
                        let mut body = alloc::vec![0u8; repr.buffer_len()];
                        let mut packet = dns_packet(&mut body[..]);
                        repr.emit(&mut packet);
                        let mut framed = Vec::with_capacity(2 + body.len());
                        framed.extend_from_slice(&(body.len() as u16).to_be_bytes());
                        framed.extend_from_slice(&body);
                        let sent = {
                            let Some(wire) = self.wire.as_mut() else {
                                return;
                            };
                            let Some(handle) = wire.dns_tcp else {
                                return;
                            };
                            wire.sockets
                                .get_mut::<tcp::Socket>(handle)
                                .send_slice(&framed)
                                .unwrap_or(0)
                        };
                        self.dns_tcp_state = Some(DnsTcp::Querying {
                            at_ns,
                            framed,
                            sent,
                            buf: Vec::new(),
                        });
                    }
                    tcp::State::Closed | tcp::State::TimeWait => {
                        self.dns_fail(now_ns, Status::TimedOut, WireEvent::DnsTimedOut);
                    }
                    _ => {
                        if now_ns >= at_ns.saturating_add(DNS_TCP_DEADLINE_NS) {
                            self.dns_fail(now_ns, Status::TimedOut, WireEvent::DnsTimedOut);
                        } else {
                            self.dns_tcp_state = Some(DnsTcp::Connecting { at_ns });
                        }
                    }
                }
            }
            DnsTcp::Querying {
                at_ns,
                mut framed,
                mut sent,
                mut buf,
            } => {
                {
                    let Some(wire) = self.wire.as_mut() else {
                        return;
                    };
                    let Some(handle) = wire.dns_tcp else {
                        return;
                    };
                    let socket = wire.sockets.get_mut::<tcp::Socket>(handle);
                    if sent < framed.len() && socket.can_send() {
                        if let Ok(n) = socket.send_slice(&framed[sent..]) {
                            sent += n;
                        }
                    }
                    if socket.can_recv() {
                        let mut chunk = [0u8; DNS_TCP_BUFFER];
                        if let Ok(n) = socket.recv_slice(&mut chunk) {
                            buf.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
                if buf.len() >= 2 {
                    let want = 2 + usize::from(u16::from_be_bytes([buf[0], buf[1]]));
                    if buf.len() >= want {
                        let Some((txid, wire_name, name_key)) = self
                            .dns_query
                            .as_ref()
                            .map(|q| (q.txid, q.wire.clone(), q.name.clone()))
                        else {
                            return;
                        };
                        let verdict = if want - 2 > DNS_POLICY_MAX {
                            DnsReply::Oversized
                        } else {
                            dns_parse_response(&buf[2..want], txid, &wire_name)
                        };
                        let _ = self.dns_settle(verdict, now_ns, config, &name_key, false);
                        return;
                    }
                }
                if now_ns >= at_ns.saturating_add(DNS_TCP_DEADLINE_NS) {
                    self.dns_fail(now_ns, Status::TimedOut, WireEvent::DnsTimedOut);
                } else {
                    self.dns_tcp_state = Some(DnsTcp::Querying {
                        at_ns,
                        framed,
                        sent,
                        buf,
                    });
                }
            }
        }
    }

    fn dns_purge_cache(&mut self, now_ns: u64) {
        self.dns_cache.retain(|e| e.expires_ns > now_ns);
    }

    fn dns_cache_lookup(&mut self, name: &[u8], now_ns: u64) -> Option<CachedResult> {
        self.dns_purge_cache(now_ns);
        self.dns_cache
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.result)
    }

    fn dns_cache_insert(&mut self, name: Vec<u8>, result: CachedResult, expires_ns: u64) {
        self.dns_cache.retain(|e| e.name != name);
        if self.dns_cache.len() >= DNS_CACHE_MAX {
            self.dns_cache.remove(0);
        }
        self.dns_cache.push(DnsCacheEntry {
            name,
            result,
            expires_ns,
        });
    }

    /// NET8: ICMP echo on the wire. BIND's port is the echo identifier, SEND/SENDTO emit one
    /// echo request (the sequence is managed here), RECV/RECVFROM return a matching echo
    /// reply's data. Loopback has no ICMP stack, so every destination must be on the wire.
    fn dispatch_icmp(
        &mut self,
        session: usize,
        index: usize,
        request: &Request<'_>,
        now_ns: u64,
    ) -> Result<Answer, Status> {
        let socket = request.socket;
        match request.body {
            Body::Connect(addr) => {
                if is_loopback(addr.ip) {
                    return Err(Status::NetUnreachable);
                }
                if addr.port != 0 {
                    return Err(Status::InvalidArgument);
                }
                if self.wire.is_none() {
                    return Err(Status::NetUnreachable);
                }
                self.ensure_bound(index)?;
                self.slots[index].peer = Some(addr);
                Ok(Answer::ok(socket, 0))
            }
            Body::Send(data) => {
                let peer = self.slots[index].peer.ok_or(Status::NotConnected)?;
                let sent = self.send_echo(index, peer.ip, data)?;
                Ok(Answer::ok(socket, sent))
            }
            Body::SendTo(to, data) => {
                if is_loopback(to.ip) {
                    return Err(Status::NetUnreachable);
                }
                if to.port != 0 {
                    return Err(Status::InvalidArgument);
                }
                let sent = self.send_echo(index, to.ip, data)?;
                Ok(Answer::ok(socket, sent))
            }
            Body::Recv { max } | Body::RecvFrom { max } => {
                let with_addr = matches!(request.body, Body::RecvFrom { .. });
                let from_wire = if let (Some(wire), Some(handle)) =
                    (self.wire.as_mut(), self.slots[index].wire_icmp)
                {
                    wire.sockets.get_mut::<icmp::Socket>(handle).recv().ok()
                } else {
                    None
                };
                let (bytes, from) = from_wire.ok_or(Status::WouldBlock)?;
                let packet = Icmpv4Packet::new_checked(bytes).map_err(|_| Status::WouldBlock)?;
                let repr = Icmpv4Repr::parse(&packet, &ChecksumCapabilities::default())
                    .map_err(|_| Status::WouldBlock)?;
                let Icmpv4Repr::EchoReply { data, .. } = repr else {
                    return Err(Status::WouldBlock);
                };
                let take = data.len().min(max as usize);
                let mut payload = Vec::with_capacity(ADDR_LEN + take);
                if with_addr {
                    let ip = match from {
                        IpAddress::Ipv4(v4) => v4.octets(),
                        #[allow(unreachable_patterns)]
                        _ => [0; 4],
                    };
                    payload.extend_from_slice(&Addr { ip, port: 0 }.encode());
                }
                payload.extend_from_slice(&data[..take]);
                let mut result = data.len() as u64;
                if data.len() > take {
                    result |= RESULT_TRUNCATED;
                }
                self.stats.echo_replies_received += 1;
                Ok(Answer::payload(socket, result, payload))
            }
            Body::Listen { .. } | Body::Accept | Body::Shutdown { .. } => Err(Status::Unsupported),
            _ => self.dispatch_common(session, index, request, now_ns),
        }
    }

    /// Emit one echo request for `index`'s identifier; returns the data bytes accepted.
    fn send_echo(&mut self, index: usize, to: [u8; 4], data: &[u8]) -> Result<u64, Status> {
        if data.len() > UDP_PAYLOAD_MAX {
            return Err(Status::MessageTooLarge);
        }
        if self.wire.is_none() {
            return Err(Status::NetUnreachable);
        }
        self.ensure_bound(index)?;
        let ident = self.slots[index].port;
        let seq_no = self.slots[index].icmp_seq;
        let repr = Icmpv4Repr::EchoRequest {
            ident,
            seq_no,
            data,
        };
        let len = repr.buffer_len();
        let mut bytes = [0u8; ICMP_ECHO_MAX];
        let mut packet = Icmpv4Packet::new_unchecked(&mut bytes[..len]);
        repr.emit(&mut packet, &ChecksumCapabilities::default());
        {
            let wire = self.wire.as_mut().ok_or(Status::NetUnreachable)?;
            let handle = self.slots[index].wire_icmp.ok_or(Status::NetUnreachable)?;
            wire.sockets
                .get_mut::<icmp::Socket>(handle)
                .send_slice(&bytes[..len], IpAddress::Ipv4(Ipv4Address::from(to)))
                .map_err(|error| match error {
                    icmp::SendError::BufferFull => Status::WouldBlock,
                    icmp::SendError::Unaddressable => Status::NetUnreachable,
                })?;
        }
        self.slots[index].icmp_seq = seq_no.wrapping_add(1);
        self.stats.echo_requests_sent += 1;
        Ok(data.len() as u64)
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
                if self.slots[index].kind == SocketKind::Icmp && addr.ip == LOOPBACK {
                    return Err(Status::AddrNotAvailable);
                }
                let wire_addr = self.wire.as_ref().map(|wire| wire.addr);
                if addr.ip != LOOPBACK && addr.ip != [0; 4] && Some(addr.ip) != wire_addr {
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
                let wire_addr = self.wire.as_ref().map_or(LOOPBACK, |wire| wire.addr);
                let local_ip = match slot.kind {
                    SocketKind::Icmp => self.wire.as_ref().map_or([0; 4], |wire| wire.addr),
                    SocketKind::Tcp if slot.tcp_on_wire => wire_addr,
                    _ => match slot.peer {
                        Some(peer) if !is_loopback(peer.ip) => wire_addr,
                        _ => LOOPBACK,
                    },
                };
                let local = Addr {
                    ip: local_ip,
                    port: slot.port,
                };
                let peer = if slot.kind == SocketKind::Tcp {
                    self.tcp_socket(index).remote_endpoint().map(addr_of)
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
            self.release_wire_tcp(index);
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
        let kind = slot.kind;
        let port = slot.port;
        self.tcp_socket_mut(index).close();
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
                if !is_loopback(addr.ip) && self.wire.is_none() {
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
                // A bound slot exists on both stacks; a wire reply must not wait behind an
                // empty loopback queue, so the wire stack is drained first.
                let from_wire = if let (Some(wire), Some(handle)) =
                    (self.wire.as_mut(), self.slots[index].wire_udp)
                {
                    wire.sockets.get_mut::<udp::Socket>(handle).recv().ok()
                } else {
                    None
                };
                let (data, meta) = match from_wire {
                    Some(pair) => pair,
                    None => self
                        .sockets
                        .get_mut::<udp::Socket>(self.slots[index].udp)
                        .recv()
                        .map_err(|_| Status::WouldBlock)?,
                };
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
            Body::RecvBatch { count } => {
                let mut replies = Vec::with_capacity(count as usize);
                while replies.len() < count as usize {
                    // Wire first, as for RECVFROM: a wire datagram never waits behind loopback.
                    let from_wire = if let (Some(wire), Some(handle)) =
                        (self.wire.as_mut(), self.slots[index].wire_udp)
                    {
                        wire.sockets.get_mut::<udp::Socket>(handle).recv().ok()
                    } else {
                        None
                    };
                    let (data, meta) = match from_wire {
                        Some(pair) => pair,
                        None => match self
                            .sockets
                            .get_mut::<udp::Socket>(self.slots[index].udp)
                            .recv()
                        {
                            Ok(pair) => pair,
                            Err(_) => break,
                        },
                    };
                    let mut payload = Vec::with_capacity(ADDR_LEN + data.len());
                    payload.extend_from_slice(&addr_of(meta.endpoint).encode());
                    payload.extend_from_slice(data);
                    replies.push((data.len() as u64, payload));
                }
                if replies.is_empty() {
                    return Err(Status::WouldBlock);
                }
                self.stats.datagrams_received += replies.len() as u64;
                Ok(Answer::Batch { socket, replies })
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
                if !is_loopback(addr.ip) && self.wire.is_none() {
                    return Err(Status::NetUnreachable);
                }
                if addr.port == 0 {
                    return Err(Status::InvalidArgument);
                }
                self.ensure_bound(index)?;
                let local = self.slots[index].port;
                if is_loopback(addr.ip) {
                    self.release_wire_tcp(index);
                    self.sockets
                        .get_mut::<tcp::Socket>(handle)
                        .connect(self.iface.context(), endpoint_of(addr), local)
                        .map_err(|_| Status::InvalidArgument)?;
                } else {
                    let Some(wire_handle) = self.ensure_wire_tcp(index) else {
                        return Err(Status::NetUnreachable);
                    };
                    let wire = self.wire.as_mut().expect("checked above");
                    wire.sockets
                        .get_mut::<tcp::Socket>(wire_handle)
                        .connect(wire.iface.context(), endpoint_of(addr), local)
                        .map_err(|e| match e {
                            tcp::ConnectError::Unaddressable => Status::NetUnreachable,
                            _ => Status::InvalidArgument,
                        })?;
                    self.slots[index].tcp_on_wire = true;
                }
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
                let (ready, on_wire) = self.acceptable(index).ok_or(Status::WouldBlock)?;
                let s = self.sessions[session].as_mut().ok_or(Status::BadSocket)?;
                if s.sockets >= SOCKETS_PER_SESSION {
                    return Err(Status::QuotaExceeded);
                }
                let id = s.next_socket;
                s.next_socket += 1;
                s.sockets += 1;
                {
                    let slot = &mut self.slots[ready];
                    slot.listener = None;
                    slot.owner = Some((session, id));
                    slot.state = Tcp::Stream;
                    slot.tcp_on_wire = on_wire;
                }
                let peer = if on_wire {
                    // The loopback twin was listening on the same port; it is not this stream.
                    self.sockets
                        .get_mut::<tcp::Socket>(self.slots[ready].tcp)
                        .abort();
                    let handle = self.slots[ready].wire_tcp.expect("a wire twin");
                    self.wire
                        .as_ref()
                        .expect("a wire ACCEPT has its stack")
                        .sockets
                        .get::<tcp::Socket>(handle)
                        .remote_endpoint()
                        .map(addr_of)
                } else {
                    self.release_wire_tcp(ready);
                    self.sockets
                        .get::<tcp::Socket>(self.slots[ready].tcp)
                        .remote_endpoint()
                        .map(addr_of)
                };
                let peer = peer.unwrap_or(Addr {
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
                let sent = {
                    let tcp = self.tcp_socket_mut(index);
                    if tcp.state() == tcp::State::Closed {
                        return Err(Status::ConnReset);
                    }
                    if !tcp.may_send() {
                        return Err(Status::ShutDown);
                    }
                    tcp.send_slice(data)
                };
                match sent {
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
                let read = {
                    let tcp = self.tcp_socket_mut(index);
                    if tcp.can_recv() {
                        let mut data = vec![0u8; max as usize];
                        let n = tcp.recv_slice(&mut data).map_err(|_| Status::ConnReset)?;
                        data.truncate(n);
                        Some((n, data))
                    } else {
                        None
                    }
                };
                if let Some((n, data)) = read {
                    self.stats.stream_bytes_received += n as u64;
                    return Ok(Answer::payload(socket, n as u64, data));
                }
                let tcp = self.tcp_socket(index);
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
                    // Entering CloseWait means the peer's FIN was already received, so the
                    // stream ends orderly even if the socket reaches Closed before another
                    // dispatch samples the state (the FIN-behind-payload race: the peer's
                    // ACK would otherwise make RECV answer ConnReset).
                    if self.tcp_socket(index).state() == tcp::State::CloseWait {
                        self.slots[index].peer_finished = true;
                    }
                    self.tcp_socket_mut(index).close();
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
        if let Some(wire) = self.wire.as_mut() {
            for _ in 0..POLLS_PER_TURN {
                let result = wire
                    .iface
                    .poll(instant(now_ns), &mut *wire.device, &mut wire.sockets);
                if result == PollResult::None {
                    break;
                }
            }
        }
        // NET11: apply DHCP lease events to this generation's interface.
        let mut lease_events: Vec<WireEvent> = Vec::new();
        if let Some(wire) = self.wire.as_mut() {
            if let Some(handle) = wire.dhcp {
                for _ in 0..4 {
                    let event = {
                        let socket = wire.sockets.get_mut::<dhcpv4::Socket>(handle);
                        match socket.poll() {
                            Some(dhcpv4::Event::Configured(config)) => Some((
                                config.address.address().octets(),
                                config.address.prefix_len(),
                                config.router.map(|r| r.octets()),
                            )),
                            Some(dhcpv4::Event::Deconfigured) => Some(([0; 4], 0, None)),
                            None => None,
                        }
                    };
                    let Some((addr, prefix, router)) = event else {
                        break;
                    };
                    if addr == [0; 4] {
                        // The pre-lease state reports Deconfigured once; a lease drop is
                        // an event only when an address was actually held.
                        if wire.addr != [0; 4] {
                            wire.iface.update_ip_addrs(|addrs| addrs.clear());
                            let _ = wire.iface.routes_mut().remove_default_ipv4_route();
                            wire.addr = [0; 4];
                            wire.prefix = 0;
                            lease_events.push(WireEvent::DhcpExpired);
                        }
                    } else {
                        wire.iface.update_ip_addrs(|addrs| {
                            addrs.clear();
                            let _ = addrs.push(IpCidr::new(
                                IpAddress::Ipv4(Ipv4Address::from(addr)),
                                prefix,
                            ));
                        });
                        if let Some(router) = router {
                            let _ = wire
                                .iface
                                .routes_mut()
                                .add_default_ipv4_route(Ipv4Address::from(router));
                        }
                        let renewed = wire.addr == addr && wire.prefix == prefix;
                        wire.addr = addr;
                        wire.prefix = prefix;
                        lease_events.push(if renewed {
                            WireEvent::DhcpRenewed { addr, prefix }
                        } else {
                            WireEvent::DhcpLease {
                                addr,
                                prefix,
                                router,
                            }
                        });
                    }
                }
            }
        }
        self.wire_events.extend(lease_events);
        // NET12: drive the in-flight DNS lookup (retransmissions, replies, fallback).
        self.dns_poll(now_ns);
        // Sampling after the polls latches state windows (CloseWait, LastAck, TimeWait)
        // that a dispatch-only observe would miss.
        for index in 0..self.slots.len() {
            if self.slots[index].kind == SocketKind::Tcp {
                self.observe(index);
            }
        }
        for index in 0..self.slots.len() {
            let Some(deadline) = self.slots[index].draining else {
                continue;
            };
            let state = self.tcp_socket(index).state();
            let done = matches!(state, tcp::State::Closed | tcp::State::TimeWait);
            if done || now_ns >= deadline {
                // The FIN exchange (or the RST abort queued) has gone out; reset and reuse.
                self.tcp_socket_mut(index).abort();
                if !done {
                    // A deadline reset still owes the peer its RST: one more poll.
                    self.slots[index].draining = Some(0);
                    continue;
                }
                self.release_wire_tcp(index);
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
        let wire_stack = self.wire.as_mut().and_then(|wire| {
            wire.iface
                .poll_delay(instant(now_ns), &wire.sockets)
                .map(|delay| delay.total_micros().saturating_mul(1000))
        });
        let stack = match (stack, wire_stack) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
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
    /// RECV_BATCH: one reply per datagram, `(result, payload)`, sent in order.
    Batch {
        socket: u32,
        replies: Vec<(u64, Vec<u8>)>,
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
