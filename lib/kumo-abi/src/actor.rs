//j822
//! Privileged AX0 diagnostics over Sora's existing root channel, not an app syscall.
//! The capability is held only by Sora. Counters measure costs; they change no policy.

pub const QUERY: [u8; 4] = *b"AXB1";
pub const SNAPSHOT_LEN: usize = 192;
pub const RESIDENT_LIMIT: usize = 12;
pub const ROUNDS: u32 = 1024;
pub const PAYLOAD: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Snapshot {
    pub enabled: bool,
    pub capacity: u32,
    pub live: u32,
    pub stored: u32,
    pub blocked: u32,
    pub ready: u32,
    pub running: u32,
    pub frames_issued: u64,
    pub heap_used: u64,
    pub roots: u64,
    pub root_ns: u64,
    pub root_min_ns: u64,
    pub root_max_ns: u64,
    pub syscalls: u64,
    pub switches: u64,
    pub processes: [u64; RESIDENT_LIMIT],
}

impl Snapshot {
    pub fn encode(self) -> [u8; SNAPSHOT_LEN] {
        let mut b = [0; SNAPSHOT_LEN];
        b[..4].copy_from_slice(b"AXR1");
        for (i, v) in [
            self.enabled as u32,
            self.capacity,
            self.live,
            self.stored,
            self.blocked,
            self.ready,
            self.running,
        ]
        .into_iter()
        .enumerate()
        {
            b[4 + i * 4..8 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        for (i, v) in [
            self.frames_issued,
            self.heap_used,
            self.roots,
            self.root_ns,
            self.root_min_ns,
            self.root_max_ns,
            self.syscalls,
            self.switches,
        ]
        .into_iter()
        .chain(self.processes)
        .enumerate()
        {
            b[32 + i * 8..40 + i * 8].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != SNAPSHOT_LEN || &b[..4] != b"AXR1" {
            return None;
        }
        let u32_at = |i| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let u64_at = |i| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let s = Self {
            enabled: u32_at(4) == 1,
            capacity: u32_at(8),
            live: u32_at(12),
            stored: u32_at(16),
            blocked: u32_at(20),
            ready: u32_at(24),
            running: u32_at(28),
            frames_issued: u64_at(32),
            heap_used: u64_at(40),
            roots: u64_at(48),
            root_ns: u64_at(56),
            root_min_ns: u64_at(64),
            root_max_ns: u64_at(72),
            syscalls: u64_at(80),
            switches: u64_at(88),
            processes: core::array::from_fn(|i| u64_at(96 + i * 8)),
        };
        (u32_at(4) <= 1
            && s.capacity as usize <= RESIDENT_LIMIT
            && s.live <= s.capacity
            && s.live <= s.stored
            && s.stored <= s.capacity
            && s.blocked
                .checked_add(s.ready)
                .and_then(|v| v.checked_add(s.running))
                == Some(s.live)
            && s.processes.iter().filter(|p| **p != 0).count() == s.live as usize)
            .then_some(s)
    }
}

pub fn fill_ping(sequence: u32) -> [u8; PAYLOAD] {
    let mut b = [0; PAYLOAD];
    b[..4].copy_from_slice(&sequence.to_le_bytes());
    for (i, v) in b.iter_mut().enumerate().skip(4) {
        *v = (i as u8).wrapping_mul(31).wrapping_add(sequence as u8);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_roundtrip_and_invalid_frames() {
        let mut s = Snapshot {
            capacity: 12,
            live: 2,
            stored: 3,
            blocked: 1,
            ready: 1,
            frames_issued: 123,
            syscalls: 456,
            switches: 789,
            roots: 12,
            root_ns: 999,
            root_min_ns: 1,
            root_max_ns: 100,
            ..Snapshot::default()
        };
        s.processes[..2].copy_from_slice(&[15, 99]);
        let b = s.encode();
        assert_eq!(Snapshot::decode(&b), Some(s));
        for len in 0..SNAPSHOT_LEN {
            assert_eq!(Snapshot::decode(&b[..len]), None);
        }
        s.live = 13;
        assert_eq!(Snapshot::decode(&s.encode()), None);
        s.live = 2;
        s.capacity = 99;
        assert_eq!(Snapshot::decode(&s.encode()), None);
        s.capacity = 12;
        s.blocked = u32::MAX;
        assert_eq!(Snapshot::decode(&s.encode()), None);
        s.blocked = 2;
        assert_eq!(Snapshot::decode(&s.encode()), None);
    }
    #[test]
    fn every_ping_is_exact_and_ordered() {
        for n in 1..=ROUNDS {
            let b = fill_ping(n);
            assert_eq!(u32::from_le_bytes(b[..4].try_into().unwrap()), n);
            assert_ne!(b, fill_ping(n + 1));
        }
    }
}
