//j834
//j835
//! NET6 passive-RX test frames: a peer's numbered IPv4/UDP broadcasts to `PORT`, whose
//! size and every payload byte follow from the sequence number alone. Each frame is
//! verified in software (IPv4 header checksum, UDP checksum, payload) and summarized
//! by digests a host checker recomputes. Classification and counting only: no
//! allocation, queue or socket. — TIMBERDOODLE

pub const PORT: u16 = 50606;
pub const MAGIC: [u8; 8] = *b"KUMONET6";
/// Sequences below this are the slow exact phase, logged one frame per line.
pub const EXACT: u32 = 64;
/// The burst phase follows the exact phase; its arrivals are kept in a bitmap.
pub const BURST: u32 = 4096;
/// Sent after both phases; it concludes the receive window.
pub const END: u32 = u32::MAX;
const ETH: usize = 14;
const IP: usize = 20;
const UDP: usize = 8;
const PAYLOAD_HEADER: usize = 16;

/// Payload layout: `MAGIC`, sequence (u32 LE), payload length (u16 LE), format version
/// 1, a zero byte, then `pattern` bytes.
///
/// Exact phase payload sizes: the 60-byte minimum frame and its neighbours, frames
/// 496..511 and 1008..1023 around USB packet multiples, and the 1514-byte maximum.
pub fn payload_len(seq: u32) -> usize {
    match seq {
        END => PAYLOAD_HEADER + 2,
        0..=15 => 18 + seq as usize,
        16..=31 => 454 + (seq - 16) as usize,
        32..=47 => 966 + (seq - 32) as usize,
        48..=63 => 1457 + (seq - 48) as usize,
        _ => 18 + (seq as usize * 389) % 1455,
    }
}

/// Byte `i` of a payload after its 16-byte header.
pub fn pattern(seq: u32, i: usize) -> u8 {
    (seq as u8).wrapping_add((i as u8).wrapping_mul(13)) ^ (i >> 8) as u8
}

pub fn fnv64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// IEEE 802.3 CRC-32, as the frame check sequence carries it (little-endian).
pub fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |mut c, &b| {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = (c >> 1) ^ (0xedb8_8320 & (c & 1).wrapping_neg());
        }
        c
    })
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn ones_sum(bytes: &[u8], mut sum: u32) -> u32 {
    let mut chunks = bytes.chunks_exact(2);
    for c in &mut chunks {
        sum += u32::from(u16::from_be_bytes([c[0], c[1]]));
    }
    if let [last] = chunks.remainder() {
        sum += u32::from(*last) << 8;
    }
    sum
}

fn fold(mut sum: u32) -> u16 {
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    Valid,
    IpChecksum,
    UdpChecksum,
    Length,
    Payload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TestFrame {
    pub seq: u32,
    pub verdict: Verdict,
    /// FNV-1a 64 of the UDP payload alone, which the peer fully determines.
    pub payload_fnv: u64,
    /// Bytes after the IPv4 datagram: Ethernet padding and/or a passed-through FCS.
    pub trailing: usize,
    /// The last four bytes are the CRC-32 of everything before them.
    pub fcs: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Class {
    Test(TestFrame),
    Arp,
    Ipv4,
    Ipv6,
    Other(u16),
}

/// Classify one received frame (Ethernet header onward, alignment bytes removed).
pub fn classify(frame: &[u8]) -> Class {
    if frame.len() < ETH {
        return Class::Other(0);
    }
    match be16(frame, 12) {
        0x0806 => return Class::Arp,
        0x86dd => return Class::Ipv6,
        0x0800 => {}
        other => return Class::Other(other),
    }
    let ip = &frame[ETH..];
    if ip.len() < IP + UDP || ip[0] != 0x45 || ip[9] != 17 || be16(ip, 22) != PORT {
        return Class::Ipv4;
    }
    let total = usize::from(be16(ip, 2));
    let udp_len = usize::from(be16(ip, 24));
    let fits = total <= ip.len() && total >= IP + UDP && udp_len == total - IP;
    let payload = if fits { &ip[IP + UDP..total] } else { &[][..] };
    if payload.len() < PAYLOAD_HEADER || !payload.starts_with(&MAGIC) {
        return Class::Ipv4;
    }
    let seq = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
    let trailing = frame.len() - ETH - total;
    let fcs = frame.len() >= 4 + ETH && {
        let (body, tail) = frame.split_at(frame.len() - 4);
        crc32(body).to_le_bytes() == tail
    };
    let verdict = if fold(ones_sum(&ip[..IP], 0)) != 0xffff {
        Verdict::IpChecksum
    } else if be16(ip, 26) != 0 && {
        let pseudo = ones_sum(&ip[12..20], 17 + udp_len as u32);
        fold(ones_sum(&ip[IP..total], pseudo)) != 0xffff
    } {
        Verdict::UdpChecksum
    } else if payload.len() != payload_len(seq)
        || usize::from(u16::from_le_bytes([payload[12], payload[13]])) != payload.len()
    {
        Verdict::Length
    } else if payload[PAYLOAD_HEADER..]
        .iter()
        .enumerate()
        .any(|(i, &b)| b != pattern(seq, i))
    {
        Verdict::Payload
    } else {
        Verdict::Valid
    };
    Class::Test(TestFrame {
        seq,
        verdict,
        payload_fnv: fnv64(payload),
        trailing,
        fcs,
    })
}

/// The order-independent burst digest term for one received sequence.
pub fn burst_term(seq: u32, payload_fnv: u64) -> u64 {
    let mut bytes = [0u8; 12];
    bytes[..4].copy_from_slice(&seq.to_le_bytes());
    bytes[4..].copy_from_slice(&payload_fnv.to_le_bytes());
    fnv64(&bytes)
}

/// Every receive-window count NET6 reports. Exact-phase frames are logged one by one
/// by the caller; burst arrivals land in `burst_bits` and the wrapping digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tally {
    pub aggregates: u32,
    pub parse_errors: u32,
    pub frames: u32,
    pub dropped: u32,
    pub dummies: u32,
    pub max_frames_per_aggregate: u32,
    pub max_aggregate_bytes: u32,
    pub arp: u32,
    pub ipv4: u32,
    pub ipv6: u32,
    pub other: u32,
    pub exact_bits: u64,
    pub burst_bits: [u64; (BURST / 64) as usize],
    pub burst_received: u32,
    pub burst_digest: u64,
    pub duplicates: u32,
    pub invalid: u32,
    pub unexpected_seq: u32,
    pub fcs: u32,
    pub no_fcs: u32,
    pub end: u32,
}

impl Default for Tally {
    fn default() -> Self {
        Self::new()
    }
}

impl Tally {
    pub const fn new() -> Self {
        Self {
            aggregates: 0,
            parse_errors: 0,
            frames: 0,
            dropped: 0,
            dummies: 0,
            max_frames_per_aggregate: 0,
            max_aggregate_bytes: 0,
            arp: 0,
            ipv4: 0,
            ipv6: 0,
            other: 0,
            exact_bits: 0,
            burst_bits: [0; (BURST / 64) as usize],
            burst_received: 0,
            burst_digest: 0,
            duplicates: 0,
            invalid: 0,
            unexpected_seq: 0,
            fcs: 0,
            no_fcs: 0,
            end: 0,
        }
    }

    /// Count one completed transfer; `frames` is how many it carried.
    pub fn aggregate(&mut self, bytes: usize, parsed: Result<crate::rx::Counts, crate::rx::Error>) {
        self.aggregates += 1;
        self.max_aggregate_bytes = self.max_aggregate_bytes.max(bytes as u32);
        match parsed {
            Ok(c) => {
                self.frames += c.frames as u32;
                self.dropped += c.dropped as u32;
                self.dummies += c.dummy as u32;
                self.max_frames_per_aggregate = self.max_frames_per_aggregate.max(c.frames as u32);
            }
            Err(_) => self.parse_errors += 1,
        }
    }

    /// Classify and count one accepted frame. Returns the test frame, if it is one.
    pub fn frame(&mut self, frame: &[u8]) -> Option<TestFrame> {
        let test = match classify(frame) {
            Class::Test(t) => t,
            Class::Arp => {
                self.arp += 1;
                return None;
            }
            Class::Ipv4 => {
                self.ipv4 += 1;
                return None;
            }
            Class::Ipv6 => {
                self.ipv6 += 1;
                return None;
            }
            Class::Other(_) => {
                self.other += 1;
                return None;
            }
        };
        if test.verdict != Verdict::Valid {
            self.invalid += 1;
            return Some(test);
        }
        if test.fcs {
            self.fcs += 1;
        } else {
            self.no_fcs += 1;
        }
        let seq = test.seq;
        let fresh = if seq == END {
            self.end += 1;
            true
        } else if seq < EXACT {
            let bit = 1u64 << seq;
            let fresh = self.exact_bits & bit == 0;
            self.exact_bits |= bit;
            fresh
        } else if seq - EXACT < BURST {
            let i = (seq - EXACT) as usize;
            let (word, bit) = (i / 64, 1u64 << (i % 64));
            let fresh = self.burst_bits[word] & bit == 0;
            if fresh {
                self.burst_bits[word] |= bit;
                self.burst_received += 1;
                self.burst_digest = self
                    .burst_digest
                    .wrapping_add(burst_term(seq, test.payload_fnv));
            }
            fresh
        } else {
            self.unexpected_seq += 1;
            true
        };
        if !fresh {
            self.duplicates += 1;
        }
        Some(test)
    }

    pub fn exact_received(&self) -> u32 {
        self.exact_bits.count_ones()
    }
}

/// NET7a transmit test frames: broadcast on the IEEE local-experimental EtherType, so
/// no host on the LAN interprets them. Payload: `TX_MAGIC`, sequence (u32 LE), frame
/// length (u16 LE), version 1, a zero byte, then `pattern` bytes.
pub const TX_ETHERTYPE: u16 = 0x88b5;
pub const TX_MAGIC: [u8; 8] = *b"KUMOTX07";
pub const TX_EXACT: u32 = 32;
pub const TX_BURST: u32 = 2048;
/// Exact-phase frame lengths: below the 60-byte minimum (the chip must pad), around the
/// 512/1024 transfer boundary (504 and 1016 bytes + 8-byte prefix land exactly on it),
/// and at the 1514-byte maximum.
const TX_SIZES: [u16; TX_EXACT as usize] = [
    30, 42, 59, 60, 61, 64, 100, 200, 400, 503, 504, 505, 511, 512, 513, 600, 800, 1000, 1015,
    1016, 1017, 1023, 1024, 1025, 1200, 1400, 1500, 1510, 1511, 1512, 1513, 1514,
];

pub fn tx_frame_len(seq: u32) -> usize {
    match TX_SIZES.get(seq as usize) {
        Some(&n) => usize::from(n),
        None => 60 + (seq as usize * 389) % 1455,
    }
}

/// Write the frame for `seq` from `source` into `out`; `None` if `out` is too short.
pub fn tx_frame(seq: u32, source: [u8; 6], out: &mut [u8]) -> Option<usize> {
    let len = tx_frame_len(seq);
    let frame = out.get_mut(..len)?;
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&source);
    frame[12..14].copy_from_slice(&TX_ETHERTYPE.to_be_bytes());
    frame[14..22].copy_from_slice(&TX_MAGIC);
    frame[22..26].copy_from_slice(&seq.to_le_bytes());
    frame[26..28].copy_from_slice(&(len as u16).to_le_bytes());
    frame[28..30].copy_from_slice(&[1, 0]);
    for (i, b) in frame[30..].iter_mut().enumerate() {
        *b = pattern(seq, i);
    }
    Some(len)
}

#[cfg(test)]
mod tests;
