//j813

//! A bounded in-memory loopback `Device` for smoltcp (DESIGN/026 §9 item 1).
//!
//! smoltcp's own `phy::Loopback` needs `alloc`; this one is a fixed ring of `SLOTS` IP packets
//! of at most `MTU` bytes. A full ring refuses `transmit`, which smoltcp treats as "try again on
//! the next poll" — backpressure, not loss. Received packets are copied out into an owned token,
//! so the receive and transmit tokens never borrow the ring at the same time.

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

use crate::budget::{LOOPBACK_SLOTS, MTU};

/// Why [`Loopback::inject`] refused a packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InjectError {
    /// Longer than the MTU.
    TooLong,
    /// Every slot is occupied.
    Full,
}

/// Counters a test or a status report reads; none of them ever resets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoopbackCounters {
    /// Packets smoltcp transmitted into the ring.
    pub transmitted: u64,
    /// Packets handed back to smoltcp as received.
    pub received: u64,
    /// `transmit` calls refused because the ring was full.
    pub backpressure: u64,
    /// Packets a test or fixture placed into the ring directly.
    pub injected: u64,
}

/// The ring itself; split from [`Loopback`] so a transmit token can borrow only this.
struct Ring<const SLOTS: usize> {
    packets: [[u8; MTU]; SLOTS],
    lengths: [u16; SLOTS],
    head: usize,
    count: usize,
    counters: LoopbackCounters,
}

impl<const SLOTS: usize> Ring<SLOTS> {
    fn slot_for_push(&mut self) -> Option<usize> {
        (self.count < SLOTS).then_some((self.head + self.count) % SLOTS)
    }

    fn commit_push(&mut self, slot: usize, len: usize) {
        self.lengths[slot] = len as u16;
        self.count += 1;
    }

    fn pop(&mut self, into: &mut [u8; MTU]) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let slot = self.head;
        let len = self.lengths[slot] as usize;
        into[..len].copy_from_slice(&self.packets[slot][..len]);
        self.head = (self.head + 1) % SLOTS;
        self.count -= 1;
        Some(len)
    }
}

/// A loopback interface's device: IP medium, MTU [`MTU`], `SLOTS` packets in flight.
pub struct Loopback<const SLOTS: usize = LOOPBACK_SLOTS> {
    ring: Ring<SLOTS>,
}

impl<const SLOTS: usize> Default for Loopback<SLOTS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const SLOTS: usize> Loopback<SLOTS> {
    const NONEMPTY: () = assert!(SLOTS > 0);

    pub const fn new() -> Self {
        let () = Self::NONEMPTY;
        Self {
            ring: Ring {
                packets: [[0; MTU]; SLOTS],
                lengths: [0; SLOTS],
                head: 0,
                count: 0,
                counters: LoopbackCounters {
                    transmitted: 0,
                    received: 0,
                    backpressure: 0,
                    injected: 0,
                },
            },
        }
    }

    /// Packets waiting to be received.
    pub const fn pending(&self) -> usize {
        self.ring.count
    }

    pub const fn counters(&self) -> LoopbackCounters {
        self.ring.counters
    }

    /// Place raw bytes into the ring as if they had been transmitted: the way fixtures feed
    /// malformed packets to the stack.
    pub fn inject(&mut self, packet: &[u8]) -> Result<(), InjectError> {
        if packet.len() > MTU {
            return Err(InjectError::TooLong);
        }
        let slot = self.ring.slot_for_push().ok_or(InjectError::Full)?;
        self.ring.packets[slot][..packet.len()].copy_from_slice(packet);
        self.ring.commit_push(slot, packet.len());
        self.ring.counters.injected += 1;
        Ok(())
    }
}

/// A received packet, copied out of the ring.
pub struct RxToken {
    packet: [u8; MTU],
    len: usize,
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.packet[..self.len])
    }
}

/// Space for one packet in the ring.
pub struct TxToken<'a, const SLOTS: usize> {
    ring: &'a mut Ring<SLOTS>,
    slot: usize,
}

impl<const SLOTS: usize> phy::TxToken for TxToken<'_, SLOTS> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // smoltcp never builds a packet above the advertised MTU; clamp anyway so a violation
        // cannot index past the slot, and drop rather than queue a truncated packet.
        let fits = len <= MTU;
        let result = f(&mut self.ring.packets[self.slot][..len.min(MTU)]);
        if fits {
            self.ring.commit_push(self.slot, len);
            self.ring.counters.transmitted += 1;
        }
        result
    }
}

impl<const SLOTS: usize> Device for Loopback<SLOTS> {
    type RxToken<'a>
        = RxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a, SLOTS>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut packet = [0u8; MTU];
        let len = self.ring.pop(&mut packet)?;
        self.ring.counters.received += 1;
        // The pop just freed a slot, so a reply always has room.
        let slot = self.ring.slot_for_push()?;
        Some((
            RxToken { packet, len },
            TxToken {
                ring: &mut self.ring,
                slot,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        let Some(slot) = self.ring.slot_for_push() else {
            self.ring.counters.backpressure += 1;
            return None;
        };
        Some(TxToken {
            ring: &mut self.ring,
            slot,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = MTU;
        caps
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::socket::udp;
    use smoltcp::wire::{
        HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpProtocol, Ipv4Address, Ipv4Packet,
        Ipv4Repr, UdpPacket, UdpRepr,
    };

    use super::*;
    use crate::budget::{SOCKET_BUFFER, UDP_METADATA, UDP_PAYLOAD_MAX};

    const HOME: Ipv4Address = Ipv4Address::new(127, 0, 0, 1);
    const SERVER: u16 = 7000;
    const CLIENT: u16 = 7001;

    struct Udp {
        rx_meta: [udp::PacketMetadata; UDP_METADATA],
        rx: [u8; SOCKET_BUFFER],
        tx_meta: [udp::PacketMetadata; UDP_METADATA],
        tx: [u8; SOCKET_BUFFER],
    }

    impl Udp {
        fn new() -> Self {
            Self {
                rx_meta: [udp::PacketMetadata::EMPTY; UDP_METADATA],
                rx: [0; SOCKET_BUFFER],
                tx_meta: [udp::PacketMetadata::EMPTY; UDP_METADATA],
                tx: [0; SOCKET_BUFFER],
            }
        }

        fn socket(&mut self) -> udp::Socket<'_> {
            udp::Socket::new(
                udp::PacketBuffer::new(&mut self.rx_meta[..], &mut self.rx[..]),
                udp::PacketBuffer::new(&mut self.tx_meta[..], &mut self.tx[..]),
            )
        }
    }

    /// One loopback interface with a bound server socket and a bound client socket.
    struct Net<'a> {
        device: Loopback,
        iface: Interface,
        sockets: SocketSet<'a>,
        server: SocketHandle,
        client: SocketHandle,
        now: i64,
    }

    impl<'a> Net<'a> {
        fn new(storage: &'a mut [SocketStorage<'a>], a: &'a mut Udp, b: &'a mut Udp) -> Self {
            let mut device = Loopback::new();
            let mut iface =
                Interface::new(Config::new(HardwareAddress::Ip), &mut device, Instant::ZERO);
            iface.update_ip_addrs(|addrs| {
                addrs.push(IpCidr::new(IpAddress::Ipv4(HOME), 8)).unwrap();
            });
            let mut sockets = SocketSet::new(storage);
            let mut server = a.socket();
            server.bind(SERVER).unwrap();
            let mut client = b.socket();
            client.bind(CLIENT).unwrap();
            let server = sockets.add(server);
            let client = sockets.add(client);
            Self {
                device,
                iface,
                sockets,
                server,
                client,
                now: 0,
            }
        }

        /// Poll until the ring is empty; the bound proves every packet costs finite work.
        fn settle(&mut self) {
            for _ in 0..64 {
                self.now += 1;
                let now = Instant::from_millis(self.now);
                self.iface.poll(now, &mut self.device, &mut self.sockets);
                if self.device.pending() == 0 {
                    return;
                }
            }
            panic!("loopback did not drain");
        }

        fn send(&mut self, payload: &[u8]) -> Result<(), udp::SendError> {
            let to = IpEndpoint::new(IpAddress::Ipv4(HOME), SERVER);
            self.sockets
                .get_mut::<udp::Socket>(self.client)
                .send_slice(payload, to)
        }

        fn received(&mut self) -> Vec<(Vec<u8>, u16)> {
            let socket = self.sockets.get_mut::<udp::Socket>(self.server);
            let mut out = Vec::new();
            let mut buf = [0u8; UDP_PAYLOAD_MAX];
            while let Ok((n, meta)) = socket.recv_slice(&mut buf) {
                out.push((buf[..n].to_vec(), meta.endpoint.port));
            }
            out
        }
    }

    fn net_parts() -> ([SocketStorage<'static>; 2], Udp, Udp) {
        (Default::default(), Udp::new(), Udp::new())
    }

    /// A correct IPv4/UDP packet from the client port to the server port.
    fn udp_packet(payload: &[u8]) -> Vec<u8> {
        let udp = UdpRepr {
            src_port: CLIENT,
            dst_port: SERVER,
        };
        let ip = Ipv4Repr {
            src_addr: HOME,
            dst_addr: HOME,
            next_header: IpProtocol::Udp,
            payload_len: udp.header_len() + payload.len(),
            hop_limit: 64,
        };
        let mut bytes = std::vec![0u8; ip.buffer_len() + ip.payload_len];
        let checksums = ChecksumCapabilities::default();
        let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
        ip.emit(&mut packet, &checksums);
        let mut datagram = UdpPacket::new_unchecked(packet.payload_mut());
        udp.emit(
            &mut datagram,
            &HOME.into(),
            &HOME.into(),
            payload.len(),
            |buf| buf.copy_from_slice(payload),
            &checksums,
        );
        bytes
    }

    #[test]
    fn exact_datagrams_cross_loopback_in_order() {
        let (mut storage, mut a, mut b) = net_parts();
        let mut net = Net::new(&mut storage, &mut a, &mut b);
        let max = [0xA5u8; UDP_PAYLOAD_MAX];
        let cases: [&[u8]; 3] = [b"", b"hello, netstack", &max];
        for payload in cases {
            net.send(payload).unwrap();
            net.settle();
        }
        let got = net.received();
        assert_eq!(got.len(), 3);
        for (payload, (bytes, from)) in cases.iter().zip(&got) {
            assert_eq!(bytes.as_slice(), *payload);
            assert_eq!(*from, CLIENT);
        }
        let counters = net.device.counters();
        assert_eq!((counters.transmitted, counters.received), (3, 3));
    }

    #[test]
    fn oversized_datagram_is_accepted_by_smoltcp_then_silently_dropped() {
        let (mut storage, mut a, mut b) = net_parts();
        let mut net = Net::new(&mut storage, &mut a, &mut b);
        // One byte past the unfragmented maximum. Without a fragmentation feature smoltcp says
        // Ok, then drops it at dispatch: nothing is transmitted and the queue empties. A caller
        // told "sent" would be misled, so netstack must refuse above UDP_PAYLOAD_MAX itself
        // (DESIGN/026 §4, MessageTooLarge) before smoltcp sees it. — TIMBERDOODLE
        assert_eq!(net.send(&[0u8; UDP_PAYLOAD_MAX + 1]), Ok(()));
        net.settle();
        assert_eq!(net.sockets.get::<udp::Socket>(net.client).send_queue(), 0);
        assert_eq!(net.device.counters().transmitted, 0);
        // The drop blocks nothing behind it.
        net.send(b"after").unwrap();
        net.settle();
        let got = net.received();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, b"after");
    }

    #[test]
    fn a_full_ring_pushes_back_and_the_datagram_waits_then_arrives() {
        let (mut storage, mut a, mut b) = net_parts();
        let mut net = Net::new(&mut storage, &mut a, &mut b);
        // Fill the ring with packets the stack discards silently (bad UDP checksum), so the
        // only thing that can reach the server afterwards is the queued datagram.
        let mut filler = udp_packet(b"filler");
        let last = filler.len() - 1;
        filler[last] ^= 0xFF;
        for _ in 0..LOOPBACK_SLOTS {
            net.device.inject(&filler).unwrap();
        }
        assert_eq!(net.device.inject(&filler), Err(InjectError::Full));

        net.send(b"queued behind a full ring").unwrap();
        net.iface
            .poll_egress(Instant::from_millis(1), &mut net.device, &mut net.sockets);
        assert!(net.device.counters().backpressure > 0);
        let queued = net.sockets.get::<udp::Socket>(net.client).send_queue();
        assert!(
            queued > 0,
            "refused transmit keeps the datagram in the socket"
        );

        net.settle();
        let got = net.received();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, b"queued behind a full ring");
        assert_eq!(net.sockets.get::<udp::Socket>(net.client).send_queue(), 0);
    }

    #[test]
    fn inject_refuses_packets_longer_than_the_mtu() {
        let mut device: Loopback = Loopback::new();
        assert_eq!(device.inject(&[0u8; MTU + 1]), Err(InjectError::TooLong));
        assert_eq!(device.inject(&[0u8; MTU]), Ok(()));
        assert_eq!(device.pending(), 1);
    }

    #[test]
    fn malformed_packets_are_dropped_with_bounded_work() {
        let good = udp_packet(b"survivor");
        let mut fixtures: Vec<(&str, Vec<u8>)> = Vec::new();
        fixtures.push(("empty", Vec::new()));
        fixtures.push(("truncated ipv4 header", good[..12].to_vec()));
        let mut v6 = good.clone();
        v6[0] = 0x65;
        fixtures.push(("version 6 in a v4 packet", v6));
        let mut ihl = good.clone();
        ihl[0] = 0x44;
        fixtures.push(("header length below 20", ihl));
        let mut ip_sum = good.clone();
        ip_sum[10] ^= 0xFF;
        fixtures.push(("bad ipv4 header checksum", ip_sum));
        let mut udp_sum = good.clone();
        let last = udp_sum.len() - 1;
        udp_sum[last] ^= 0xFF;
        fixtures.push(("bad udp checksum", udp_sum));
        let mut total = good.clone();
        total[2..4].copy_from_slice(&(good.len() as u16 + 100).to_be_bytes());
        fixtures.push(("total length past the packet", total));
        let mut udp_len = good.clone();
        udp_len[24..26].copy_from_slice(&900u16.to_be_bytes());
        fixtures.push(("udp length past the packet", udp_len));
        let mut proto = good.clone();
        proto[9] = 0xFD;
        fixtures.push(("unknown ip protocol", proto));

        for (name, bytes) in &fixtures {
            let (mut storage, mut a, mut b) = net_parts();
            let mut net = Net::new(&mut storage, &mut a, &mut b);
            net.device.inject(bytes).unwrap();
            net.device.inject(&good).unwrap();
            net.settle();
            let got = net.received();
            assert_eq!(got.len(), 1, "{name}: exactly the valid datagram survives");
            assert_eq!(got[0].0, b"survivor", "{name}");
            assert_eq!(net.device.pending(), 0, "{name}");
        }
    }
}
