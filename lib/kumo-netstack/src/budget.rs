//j813
//j844

//! DESIGN/026 §5 budgets. Every bound the kernel does not enforce (its channel inbox is
//! unbounded) is one of these constants, so a test or a server can name it.

/// IP MTU of every interface, loopback included (DESIGN/026 §9 item 4).
pub const MTU: usize = 1500;
/// Ethernet header bytes (untagged, no FCS).
pub const ETHERNET_HEADER: usize = 14;
/// Largest Ethernet frame carried on the frame channel.
pub const FRAME_MAX: usize = MTU + ETHERNET_HEADER;
/// IPv4 header without options plus the UDP header.
pub const IPV4_UDP_HEADERS: usize = 20 + 8;
/// Largest UDP payload: one unfragmented datagram, on loopback and on the wire alike.
pub const UDP_PAYLOAD_MAX: usize = MTU - IPV4_UDP_HEADERS;
/// Largest stream payload in one socket request or reply.
pub const STREAM_PAYLOAD_MAX: usize = 2048;

/// Concurrent client sessions.
pub const SESSIONS: usize = 8;
/// Sockets one session may hold.
pub const SOCKETS_PER_SESSION: usize = 16;
/// Sockets across all sessions: the size of the smoltcp `SocketStorage` array.
pub const SOCKETS_TOTAL: usize = 32;
/// Unanswered requests a session may have outstanding before it is disconnected.
pub const UNANSWERED_PER_SESSION: usize = 8;
/// Largest TCP listen backlog.
pub const LISTEN_BACKLOG_MAX: usize = 4;

/// Per-socket buffer bytes in each direction, UDP and TCP.
pub const SOCKET_BUFFER: usize = 4096;
/// UDP datagram metadata slots per direction.
pub const UDP_METADATA: usize = 4;

/// Wire ICMP (NET8): bytes and metadata slots per direction, one echo identifier per socket.
pub const ICMP_BUFFER: usize = 2048;
pub const ICMP_METADATA: usize = 4;
/// Largest echo message either way: the ICMP header plus one full UDP-sized data body.
pub const ICMP_ECHO_HEADER: usize = 8;
pub const ICMP_ECHO_MAX: usize = ICMP_ECHO_HEADER + UDP_PAYLOAD_MAX;

/// RX frames a provider may have outstanding (credit granted by netstack).
pub const RX_CREDIT: usize = 32;
/// TX frames netstack may have without a TX_DONE (credit offered by the provider).
pub const TX_CREDIT: usize = 16;
/// Frames read per event-loop turn.
pub const FRAMES_PER_TURN: usize = 16;
/// Requests read per event-loop turn, across all sessions.
pub const REQUESTS_PER_TURN: usize = 16;

/// Slots in the in-memory loopback ring.
pub const LOOPBACK_SLOTS: usize = 16;

const _: () = assert!(UDP_PAYLOAD_MAX == 1472);
const _: () = assert!(ICMP_BUFFER >= ICMP_ECHO_MAX);
const _: () = assert!(FRAME_MAX == 1514);
const _: () = assert!(SESSIONS * SOCKETS_PER_SESSION >= SOCKETS_TOTAL);
// A KNF1/KNS1 header plus the largest payload fits one channel message (lib/kumo-ipc).
const _: () = assert!(32 + FRAME_MAX <= 4096 && 32 + STREAM_PAYLOAD_MAX <= 4096);
