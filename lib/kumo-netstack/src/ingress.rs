//j844

//! NET8's wire ingress gate (PLAN/020 NET8, DESIGN/026 §5): the one policy point between the
//! frame channel and smoltcp, so every refusal is a named counter rather than a silent drop.
//!
//! The stack has no reassembly buffer (`proto-ipv4-fragmentation` stays off) and no handler for
//! anything but ARP and IPv4, so a fragment or an unknown EtherType must never reach the socket
//! layer as if it were a whole packet. Bad IPv4 header checksums are refused here too: smoltcp
//! would drop them uncounted. The check is a fixed handful of header reads, so a malformed frame
//! costs the same bounded work however it is shaped.

use smoltcp::wire::{EthernetFrame, EthernetProtocol, Ipv4Packet};

/// What [`classify`] decided about one Ethernet frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// ARP or a whole, checksum-clean IPv4 packet: the stack may have it.
    Pass,
    /// Shorter than the Ethernet or IPv4 header it claims.
    Truncated,
    /// Not ARP and not IPv4 (VLAN tags, IPv6, LLDP, anything else).
    UnknownEthertype(u16),
    /// An IPv4 fragment: more-fragments set or a nonzero offset.
    Fragment,
    /// The IPv4 header checksum fails.
    BadIpv4Checksum,
}

/// NET8's ingress policy in one pass over the frame's fixed headers.
pub fn classify(frame: &[u8]) -> Verdict {
    let Ok(eth) = EthernetFrame::new_checked(frame) else {
        return Verdict::Truncated;
    };
    match eth.ethertype() {
        EthernetProtocol::Arp => Verdict::Pass,
        EthernetProtocol::Ipv4 => {
            let Ok(packet) = Ipv4Packet::new_checked(eth.payload()) else {
                return Verdict::Truncated;
            };
            if packet.more_frags() || packet.frag_offset() != 0 {
                return Verdict::Fragment;
            }
            if !packet.verify_checksum() {
                return Verdict::BadIpv4Checksum;
            }
            Verdict::Pass
        }
        EthernetProtocol::Ipv6 => Verdict::UnknownEthertype(0x86DD),
        EthernetProtocol::Unknown(raw) => Verdict::UnknownEthertype(raw),
    }
}

/// One counter per refusal the policy can produce; never resets within a wire generation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Refusals {
    pub truncated: u64,
    pub unknown_ethertype: u64,
    pub fragments: u64,
    pub bad_ipv4_checksum: u64,
}

impl Refusals {
    /// Count one [`Verdict`]; `Pass` counts nothing.
    pub fn count(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Pass => {}
            Verdict::Truncated => self.truncated += 1,
            Verdict::UnknownEthertype(_) => self.unknown_ethertype += 1,
            Verdict::Fragment => self.fragments += 1,
            Verdict::BadIpv4Checksum => self.bad_ipv4_checksum += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::wire::{
        ArpOperation, ArpPacket, ArpRepr, EthernetAddress, IpProtocol, Ipv4Address, Ipv4Repr,
    };

    use super::*;

    const MAC_A: [u8; 6] = [0x02, 0, 0, 0, 0, 0x0a];
    const MAC_B: [u8; 6] = [0x02, 0, 0, 0, 0, 0x0b];
    const IP_A: [u8; 4] = [192, 168, 0, 50];
    const IP_B: [u8; 4] = [192, 168, 0, 106];

    fn ethernet(
        src: [u8; 6],
        dst: [u8; 6],
        ethertype: EthernetProtocol,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut bytes = std::vec![0u8; 14 + payload.len()];
        {
            let mut frame = EthernetFrame::new_unchecked(&mut bytes[..]);
            frame.set_src_addr(EthernetAddress(src));
            frame.set_dst_addr(EthernetAddress(dst));
            frame.set_ethertype(ethertype);
            frame.payload_mut().copy_from_slice(payload);
        }
        bytes
    }

    fn ipv4(next_header: IpProtocol, payload: &[u8]) -> Vec<u8> {
        let checksums = ChecksumCapabilities::default();
        let repr = Ipv4Repr {
            src_addr: Ipv4Address::from(IP_B),
            dst_addr: Ipv4Address::from(IP_A),
            next_header,
            payload_len: payload.len(),
            hop_limit: 64,
        };
        let mut bytes = std::vec![0u8; repr.buffer_len() + payload.len()];
        {
            let mut packet = Ipv4Packet::new_unchecked(&mut bytes[..]);
            repr.emit(&mut packet, &checksums);
            packet.payload_mut().copy_from_slice(payload);
        }
        bytes
    }

    #[test]
    fn arp_and_whole_ipv4_pass() {
        let mut arp = std::vec![0u8; 28];
        {
            let mut packet = ArpPacket::new_unchecked(&mut arp[..]);
            ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Request,
                source_hardware_addr: EthernetAddress(MAC_B),
                source_protocol_addr: Ipv4Address::from(IP_B),
                target_hardware_addr: EthernetAddress([0; 6]),
                target_protocol_addr: Ipv4Address::from(IP_A),
            }
            .emit(&mut packet);
        }
        assert_eq!(
            classify(&ethernet(MAC_B, [0xFF; 6], EthernetProtocol::Arp, &arp)),
            Verdict::Pass
        );
        let packet = ipv4(IpProtocol::Icmp, &[0u8; 8]);
        assert_eq!(
            classify(&ethernet(MAC_B, MAC_A, EthernetProtocol::Ipv4, &packet)),
            Verdict::Pass
        );
    }

    #[test]
    fn an_unknown_ethertype_is_refused_and_named() {
        let ipv6 = ethernet(MAC_B, MAC_A, EthernetProtocol::Ipv6, &[0u8; 40]);
        assert_eq!(classify(&ipv6), Verdict::UnknownEthertype(0x86DD));
        let vlan = ethernet(MAC_B, MAC_A, EthernetProtocol::Unknown(0x8100), &[0u8; 40]);
        assert_eq!(classify(&vlan), Verdict::UnknownEthertype(0x8100));
    }

    #[test]
    fn a_fragment_is_refused_before_it_can_masquerade_as_a_packet() {
        // First fragment: more-fragments set, offset zero.
        let mut first = ipv4(IpProtocol::Icmp, &[0u8; 8]);
        {
            let mut packet = Ipv4Packet::new_unchecked(&mut first[..]);
            packet.set_more_frags(true);
            packet.fill_checksum();
        }
        assert_eq!(
            classify(&ethernet(MAC_B, MAC_A, EthernetProtocol::Ipv4, &first)),
            Verdict::Fragment
        );
        // Later fragment: nonzero offset, more-fragments clear.
        let mut later = ipv4(IpProtocol::Icmp, &[0u8; 8]);
        {
            let mut packet = Ipv4Packet::new_unchecked(&mut later[..]);
            packet.set_frag_offset(8);
            packet.fill_checksum();
        }
        assert_eq!(
            classify(&ethernet(MAC_B, MAC_A, EthernetProtocol::Ipv4, &later)),
            Verdict::Fragment
        );
    }

    #[test]
    fn a_bad_ipv4_header_checksum_is_refused() {
        let mut packet = ipv4(IpProtocol::Icmp, &[0u8; 8]);
        packet[10] ^= 0xFF;
        assert_eq!(
            classify(&ethernet(MAC_B, MAC_A, EthernetProtocol::Ipv4, &packet)),
            Verdict::BadIpv4Checksum
        );
    }

    #[test]
    fn short_frames_are_refused_as_truncated() {
        assert_eq!(classify(&[0u8; 13]), Verdict::Truncated);
        let frame = ethernet(MAC_B, MAC_A, EthernetProtocol::Ipv4, &[0u8; 10]);
        assert_eq!(classify(&frame), Verdict::Truncated);
    }

    #[test]
    fn refusals_count_one_per_verdict_and_ignore_passes() {
        let mut refusals = Refusals::default();
        for verdict in [
            Verdict::Truncated,
            Verdict::UnknownEthertype(0x8100),
            Verdict::Fragment,
            Verdict::BadIpv4Checksum,
            Verdict::Pass,
        ] {
            refusals.count(verdict);
        }
        assert_eq!(
            refusals,
            Refusals {
                truncated: 1,
                unknown_ethertype: 1,
                fragments: 1,
                bad_ipv4_checksum: 1,
            }
        );
    }

    #[test]
    fn the_net8_neighbor_cache_is_eight_entries() {
        assert_eq!(
            smoltcp::config::IFACE_NEIGHBOR_CACHE_COUNT,
            8,
            "DESIGN/026 §5 fixes the cache at NET8 via SMOLTCP_IFACE_NEIGHBOR_CACHE_COUNT"
        );
    }
}
