//j842
//! An Ethernet `FrameDevice`: the smoltcp device whose frames cross the KNF1 channel.
//!
//! The loopback device models a stack talking to itself. This device models one stack's
//! boundary with the outside wire (DESIGN/026 §3): the process pushes frames the provider
//! delivered (`RxFrame`) into the ingress ring, and drains frames smoltcp built from the egress
//! ring to hand them over as `TxFrame`s. Both rings are bounded and allocation-free, and every
//! refusal is counted rather than hidden: an inbound frame beyond the ingress depth is refused
//! (the process drops and counts it), an outbound frame beyond the egress depth is refused by
//! `transmit` so smoltcp keeps the packet queued and retries.

use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

use crate::budget::{FRAME_MAX, MTU};
use crate::loopback::InjectError;

/// Frames the process may queue for the stack before `push_inbound` refuses: one full RX
/// credit window, so a turn that drains the frame channel (netstack's `serve_frame`) never
/// overruns the ring before smoltcp polls. Was 8, sized for one frame per wake.
pub const INGRESS_SLOTS: usize = crate::budget::RX_CREDIT;
/// Frames smoltcp may build before `transmit` pushes back.
pub const EGRESS_SLOTS: usize = 8;

/// Cumulative frame counts, exposed as one snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FrameCounters {
    /// Inbound frames accepted from the provider.
    pub injected: u64,
    /// Inbound frames refused because the ingress ring was full.
    pub ingress_full: u64,
    /// Inbound frames smoltcp consumed.
    pub delivered: u64,
    /// Outbound frames smoltcp built into the egress ring.
    pub transmitted: u64,
    /// Outbound frames dropped because they exceeded `FRAME_MAX` (should never happen: the
    /// advertised MTU keeps smoltcp within bounds).
    pub oversized: u64,
    /// `transmit` (or `receive`, when it had no room for a reply) refusals.
    pub backpressure: u64,
    /// Outbound frames the process drained.
    pub popped: u64,
}

/// A bounded FIFO of owned frame copies.
struct Ring<const SLOTS: usize> {
    frames: [[u8; FRAME_MAX]; SLOTS],
    lengths: [usize; SLOTS],
    head: usize,
    count: usize,
}

impl<const SLOTS: usize> Ring<SLOTS> {
    const NONEMPTY: () = assert!(SLOTS > 0);

    const fn new() -> Self {
        let () = Self::NONEMPTY;
        Self {
            frames: [[0; FRAME_MAX]; SLOTS],
            lengths: [0; SLOTS],
            head: 0,
            count: 0,
        }
    }

    fn slot_for_push(&self) -> Option<usize> {
        if self.count >= SLOTS {
            return None;
        }
        Some((self.head + self.count) % SLOTS)
    }

    fn commit_push(&mut self, slot: usize, len: usize) {
        debug_assert_eq!(slot, (self.head + self.count) % SLOTS);
        self.lengths[slot] = len;
        self.count += 1;
    }

    fn pop(&mut self, out: &mut [u8; FRAME_MAX]) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let slot = self.head;
        let len = self.lengths[slot];
        out[..len].copy_from_slice(&self.frames[slot][..len]);
        self.head = (self.head + 1) % SLOTS;
        self.count -= 1;
        Some(len)
    }
}

/// The stack-side half of the KNF1 frame channel: smoltcp drives it as its device, the process
/// moves frames between its rings and the wire.
pub struct FrameDevice {
    ingress: Ring<INGRESS_SLOTS>,
    egress: Ring<EGRESS_SLOTS>,
    counters: FrameCounters,
}

impl FrameDevice {
    pub const fn new() -> Self {
        Self {
            ingress: Ring::new(),
            egress: Ring::new(),
            counters: FrameCounters {
                injected: 0,
                ingress_full: 0,
                delivered: 0,
                transmitted: 0,
                oversized: 0,
                backpressure: 0,
                popped: 0,
            },
        }
    }

    /// Queue a frame the provider delivered, for smoltcp to consume.
    pub fn push_inbound(&mut self, frame: &[u8]) -> Result<(), InjectError> {
        if frame.len() > FRAME_MAX {
            return Err(InjectError::TooLong);
        }
        let slot = self.ingress.slot_for_push().ok_or_else(|| {
            self.counters.ingress_full += 1;
            InjectError::Full
        })?;
        self.ingress.frames[slot][..frame.len()].copy_from_slice(frame);
        self.ingress.commit_push(slot, frame.len());
        self.counters.injected += 1;
        Ok(())
    }

    /// Pop the oldest frame smoltcp built, to be handed over as a `TxFrame`.
    pub fn take_outbound(&mut self, out: &mut [u8; FRAME_MAX]) -> Option<usize> {
        let len = self.egress.pop(out)?;
        self.counters.popped += 1;
        Some(len)
    }

    /// Frames waiting for smoltcp.
    pub const fn pending_inbound(&self) -> usize {
        self.ingress.count
    }

    /// Frames waiting for the process.
    pub const fn pending_outbound(&self) -> usize {
        self.egress.count
    }

    pub const fn counters(&self) -> FrameCounters {
        self.counters
    }
}

impl Default for FrameDevice {
    fn default() -> Self {
        Self::new()
    }
}

/// A received frame, copied out of the ingress ring.
pub struct RxToken {
    frame: [u8; FRAME_MAX],
    len: usize,
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.frame[..self.len])
    }
}

/// Space for one frame in the egress ring.
pub struct TxToken<'a> {
    ring: &'a mut Ring<EGRESS_SLOTS>,
    counters: &'a mut FrameCounters,
    slot: usize,
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // smoltcp never builds a frame above 14 + the advertised MTU; clamp anyway so a
        // violation cannot index past the slot, and drop rather than queue a truncated frame.
        let fits = len <= FRAME_MAX;
        let result = f(&mut self.ring.frames[self.slot][..len.min(FRAME_MAX)]);
        if fits {
            self.ring.commit_push(self.slot, len);
            self.counters.transmitted += 1;
        } else {
            self.counters.oversized += 1;
        }
        result
    }
}

impl Device for FrameDevice {
    type RxToken<'a>
        = RxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // smoltcp may answer inside the same poll, so a receive is only granted when the reply
        // has room; otherwise the ingress frame stays queued and the next poll retries.
        if self.egress.slot_for_push().is_none() {
            self.counters.backpressure += 1;
            return None;
        }
        let mut frame = [0u8; FRAME_MAX];
        let len = self.ingress.pop(&mut frame)?;
        self.counters.delivered += 1;
        let slot = self.egress.slot_for_push()?;
        Some((
            RxToken { frame, len },
            TxToken {
                ring: &mut self.egress,
                counters: &mut self.counters,
                slot,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        let Some(slot) = self.egress.slot_for_push() else {
            self.counters.backpressure += 1;
            return None;
        };
        Some(TxToken {
            ring: &mut self.egress,
            counters: &mut self.counters,
            slot,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        // smoltcp's Ethernet `ip_mtu()` subtracts the 14-byte frame header from this
        // value, so the device advertises the whole frame: the IP MTU stays MTU and the
        // largest unfragmented UDP payload is exactly 1472. Advertising the IP MTU here
        // made smoltcp silently drop every IP packet over 1486 bytes, because the
        // fragmentation feature is off (found by NET9's zero/maximum datagram test).
        caps.max_transmission_unit = FRAME_MAX;
        caps
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
    use smoltcp::phy::{RxToken as _, TxToken as _};
    use smoltcp::socket::udp;
    use smoltcp::wire::{
        EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address,
    };
    use std::vec::Vec;

    use crate::budget::{RX_CREDIT, SOCKET_BUFFER, TX_CREDIT, UDP_METADATA, UDP_PAYLOAD_MAX};
    use crate::frame::{decode, encode, Lane, TxStatus};
    use crate::provider::Provider;

    use super::*;

    #[test]
    fn capabilities_declare_an_ethernet_medium_whose_ip_mtu_is_the_mtu() {
        let device = FrameDevice::new();
        let caps = device.capabilities();
        assert_eq!(caps.medium, Medium::Ethernet);
        assert_eq!(caps.max_transmission_unit, FRAME_MAX);
        assert_eq!(caps.ip_mtu(), MTU, "smoltcp subtracts the frame header");
    }

    #[test]
    fn an_inbound_frame_reaches_the_stack_byte_exact_and_a_reply_leaves() {
        let mut device = FrameDevice::new();
        let frame = [0x5Au8; 60];
        device.push_inbound(&frame).unwrap();
        let (rx, tx) = device.receive(Instant::ZERO).unwrap();
        rx.consume(|bytes| assert_eq!(bytes, &frame));
        tx.consume(64, |buf| buf.fill(0xA5));
        let mut out = [0u8; FRAME_MAX];
        assert_eq!(device.take_outbound(&mut out), Some(64));
        assert_eq!(&out[..64], &[0xA5; 64]);
        let counters = device.counters();
        assert_eq!(counters.delivered, 1);
        assert_eq!(counters.transmitted, 1);
        assert_eq!(counters.popped, 1);
    }

    #[test]
    fn an_oversized_inbound_frame_is_refused_as_too_long() {
        let mut device = FrameDevice::new();
        assert_eq!(
            device.push_inbound(&[0u8; FRAME_MAX + 1]),
            Err(InjectError::TooLong)
        );
        assert_eq!(device.counters().ingress_full, 0);
    }

    #[test]
    fn a_full_ingress_ring_refuses_and_counts() {
        let mut device = FrameDevice::new();
        for _ in 0..INGRESS_SLOTS {
            device.push_inbound(&[1u8; 60]).unwrap();
        }
        assert_eq!(device.push_inbound(&[1u8; 60]), Err(InjectError::Full));
        assert_eq!(device.counters().ingress_full, 1);
        assert_eq!(device.pending_inbound(), INGRESS_SLOTS);
    }

    #[test]
    fn a_full_egress_ring_refuses_transmit_and_counts_backpressure() {
        let mut device = FrameDevice::new();
        for _ in 0..EGRESS_SLOTS {
            device
                .transmit(Instant::ZERO)
                .unwrap()
                .consume(60, |buf| buf.fill(0xEE));
        }
        assert!(device.transmit(Instant::ZERO).is_none());
        assert_eq!(device.counters().backpressure, 1);
        let mut out = [0u8; FRAME_MAX];
        let mut taken = 0;
        while device.take_outbound(&mut out).is_some() {
            taken += 1;
        }
        assert_eq!(taken, EGRESS_SLOTS);
        assert_eq!(device.pending_outbound(), 0);
    }

    #[test]
    fn receive_refuses_when_no_egress_slot_is_left_for_the_reply() {
        let mut device = FrameDevice::new();
        for _ in 0..EGRESS_SLOTS {
            device
                .transmit(Instant::ZERO)
                .unwrap()
                .consume(60, |buf| buf.fill(0));
        }
        device.push_inbound(&[2u8; 60]).unwrap();
        assert!(device.receive(Instant::ZERO).is_none());
        // The frame waits in the ingress ring; nothing is lost.
        assert_eq!(device.pending_inbound(), 1);
        assert_eq!(device.counters().backpressure, 1);
    }

    // --- the channel-coupled harness: side A <-> side B through the real KNF1 codec ---

    /// Move one message across the wire: encode it, decode it, feed the far side's state machine.
    macro_rules! cross {
        ($frame:expr, $sink:expr) => {{
            let mut wire = [0u8; WIRE_MAX];
            let n = encode(&$frame, &mut wire).expect("the message encodes");
            let decoded = decode(&wire[..n], 0).expect("the message decodes");
            $sink.accept(&decoded).expect("the far side accepts");
        }};
    }

    const MAC_A: [u8; 6] = [0x02, 0, 0, 0, 0, 0x0a];
    const MAC_B: [u8; 6] = [0x02, 0, 0, 0, 0, 0x0b];
    const IP_A: Ipv4Address = Ipv4Address::new(10, 0, 0, 1);
    const IP_B: Ipv4Address = Ipv4Address::new(10, 0, 0, 2);
    const PORT_A: u16 = 7777;
    const PORT_B: u16 = 8888;
    const WIRE_MAX: usize = crate::frame::HEADER_LEN + FRAME_MAX;

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

    /// One full stack: the smoltcp interface over its `FrameDevice`, plus the netstack-side
    /// channel state (`Lane`) and the provider that feeds it from the other end.
    struct Side<'a> {
        device: FrameDevice,
        iface: Interface,
        sockets: SocketSet<'a>,
        socket: SocketHandle,
        lane: Lane,
        provider: Provider,
        returned: u32,
        now: i64,
    }

    impl<'a> Side<'a> {
        fn new(
            storage: &'a mut [SocketStorage<'a>],
            udp: &'a mut Udp,
            mac: [u8; 6],
            ip: Ipv4Address,
            port: u16,
        ) -> Self {
            let mut device = FrameDevice::new();
            let mut iface = Interface::new(
                Config::new(HardwareAddress::Ethernet(EthernetAddress(mac))),
                &mut device,
                Instant::ZERO,
            );
            iface.update_ip_addrs(|addrs| {
                addrs.push(IpCidr::new(IpAddress::Ipv4(ip), 24)).unwrap();
            });
            let mut sock = udp.socket();
            sock.bind(port).unwrap();
            let mut sockets = SocketSet::new(storage);
            let socket = sockets.add(sock);
            let mut lane = Lane::new();
            let mut provider = Provider::new();
            cross!(provider.hello(mac), lane);
            Self {
                device,
                iface,
                sockets,
                socket,
                lane,
                provider,
                returned: 0,
                now: 0,
            }
        }

        fn poll(&mut self) {
            self.now += 10;
            let now = Instant::from_millis(self.now);
            self.iface.poll(now, &mut self.device, &mut self.sockets);
        }

        fn send(&mut self, payload: &[u8], to: (Ipv4Address, u16)) -> Result<(), udp::SendError> {
            let to = IpEndpoint::new(IpAddress::Ipv4(to.0), to.1);
            self.sockets
                .get_mut::<udp::Socket>(self.socket)
                .send_slice(payload, to)
        }

        fn received(&mut self) -> Vec<(Vec<u8>, u16)> {
            let socket = self.sockets.get_mut::<udp::Socket>(self.socket);
            let mut out = Vec::new();
            let mut buf = [0u8; UDP_PAYLOAD_MAX];
            while let Ok((n, meta)) = socket.recv_slice(&mut buf) {
                out.push((buf[..n].to_vec(), meta.endpoint.port));
            }
            out
        }

        /// Hand consumed frames back as one credit return (the channel's own accounting).
        fn return_credits(&mut self) {
            let delivered = self.device.counters().delivered as u32;
            let owed = delivered.saturating_sub(self.returned);
            if owed > 0 {
                let credit = self
                    .lane
                    .return_rx(owed)
                    .expect("a credit frame for consumed frames");
                cross!(credit, self.provider);
                self.returned = delivered;
            }
        }
    }

    /// Move every frame one side built across the real channel into the other side: TX_FRAME
    /// with its TX_DONE back, then RX_FRAME into the peer's lane and device.
    fn pump(from: &mut Side<'_>, to: &mut Side<'_>) {
        let mut frame = [0u8; FRAME_MAX];
        while let Some(len) = from.device.take_outbound(&mut frame) {
            let tx = from.lane.send_tx(&frame[..len]).expect("TX credit");
            cross!(tx, from.provider);
            let done = from
                .provider
                .tx_done(TxStatus::HandedToBus)
                .expect("TX_DONE");
            cross!(done, from.lane);
            let rx = to.provider.send_rx(&frame[..len]).expect("RX credit");
            cross!(rx, to.lane);
            to.device.push_inbound(&frame[..len]).expect("ingress room");
        }
    }

    fn settle(a: &mut Side<'_>, b: &mut Side<'_>) {
        for _ in 0..16 {
            pump(a, b);
            pump(b, a);
            a.return_credits();
            b.return_credits();
            a.poll();
            b.poll();
        }
        pump(a, b);
        pump(b, a);
        a.return_credits();
        b.return_credits();
    }

    #[test]
    fn a_datagram_crosses_two_stacks_through_the_real_knf1_codec() {
        let mut storage_a: [SocketStorage<'_>; 1] = Default::default();
        let mut storage_b: [SocketStorage<'_>; 1] = Default::default();
        let mut udp_a = Udp::new();
        let mut udp_b = Udp::new();
        let mut a = Side::new(&mut storage_a, &mut udp_a, MAC_A, IP_A, PORT_A);
        let mut b = Side::new(&mut storage_b, &mut udp_b, MAC_B, IP_B, PORT_B);

        let payload = b"a datagram through the frame channel";
        a.send(payload, (IP_B, PORT_B)).unwrap();
        settle(&mut a, &mut b);
        let got = b.received();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.as_slice(), payload);
        assert_eq!(got[0].1, PORT_A);

        // Full duplex: the answer crosses back over the same channel pair.
        b.send(b"the answer", (IP_A, PORT_A)).unwrap();
        settle(&mut a, &mut b);
        let got = a.received();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, b"the answer");

        // The channel stayed within every rule all the way through.
        for side in [&a, &b] {
            assert_eq!(side.lane.generation(), side.provider.generation());
            assert_eq!(side.lane.generation(), 1, "one generation: no restart");
            assert_eq!(
                side.lane.tx_credit(),
                TX_CREDIT as u32,
                "every TX_DONE came back"
            );
            assert_eq!(
                side.provider.counters(),
                crate::frame::Counters::default(),
                "no drops, no errors"
            );
            assert_eq!(side.device.counters().ingress_full, 0);
        }
        assert!(a.device.counters().transmitted >= 2, "ARP plus UDP");
        assert!(b.device.counters().delivered >= 2);
    }

    #[test]
    fn rx_credit_exhaustion_drops_and_counts_at_the_provider() {
        let mut lane = Lane::new();
        let mut provider = Provider::new();
        cross!(provider.hello(MAC_A), lane);
        let frame = [0x11u8; 60];
        for i in 0..RX_CREDIT {
            let rx = provider
                .send_rx(&frame)
                .unwrap_or_else(|| panic!("frame {i} inside the window"));
            cross!(rx, lane);
        }
        assert!(provider.send_rx(&frame).is_none());
        assert_eq!(provider.counters().rx_drop_no_credit, 1);
        // Returning the consumed window reopens it.
        let credit = lane
            .return_rx(RX_CREDIT as u32)
            .expect("the whole window returns");
        cross!(credit, provider);
        let rx = provider.send_rx(&frame).expect("credit restored");
        cross!(rx, lane);
    }

    #[test]
    fn the_ingress_ring_holds_one_whole_credit_window() {
        // A turn that drains the frame channel queues up to one RX credit window before
        // smoltcp polls; the ring must take all of it, and the window must close exactly
        // when the ring is full, so a provider inside its credit cannot overrun the ring.
        assert_eq!(INGRESS_SLOTS, RX_CREDIT);
        let mut storage: [SocketStorage<'_>; 1] = Default::default();
        let mut udp = Udp::new();
        let mut b = Side::new(&mut storage, &mut udp, MAC_B, IP_B, PORT_B);
        let frame = [0x22u8; 60];
        for i in 0..INGRESS_SLOTS {
            let rx = b
                .provider
                .send_rx(&frame)
                .expect("credit inside the window");
            cross!(rx, b.lane);
            b.device
                .push_inbound(&frame)
                .unwrap_or_else(|e| panic!("frame {i}: {e:?}"));
        }
        assert!(b.provider.send_rx(&frame).is_none(), "the window closes with the ring");
        assert_eq!(b.provider.counters().rx_drop_no_credit, 1);
        assert_eq!(b.device.push_inbound(&frame), Err(InjectError::Full));
        assert_eq!(b.device.counters().ingress_full, 1);
        // The stack drains the whole window in one poll.
        b.poll();
        assert_eq!(b.device.counters().delivered, INGRESS_SLOTS as u64);
    }
}
