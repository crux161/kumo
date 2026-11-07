//j822
//j825
//! Privileged AX0 diagnostics over Sora's existing root channel, not an app syscall.
//! The capability is held only by Sora. Counters measure costs; they change no policy.

/// AX0's legacy query magic. Answered byte-identically while the true live count fits
/// [`RESIDENT_LIMIT`]; a probe must accept a 4-byte little-endian status reply instead of a
/// frame when it does not (AX4 `Errno::BadState`). — TIMBERDOODLE
pub const QUERY: [u8; 4] = *b"AXB1";
pub const SNAPSHOT_LEN: usize = 192;
/// The legacy AXR1 process-array bound. AX4 keeps this view for AX0's probes; the
/// authoritative bound is [`RESIDENT_MAX`] and the AXR2 view.
pub const RESIDENT_LIMIT: usize = 12;
/// AX4: largest resident bound a Job policy may set, and the AXR2 process-array size.
/// Metal needs 256 measured actors plus the 10 processes live at login plus two spare.
pub const RESIDENT_MAX: usize = 288;
/// AX4 query magic. Same op byte as [`QUERY`]; the reply is an AXR2 frame.
pub const QUERY2: [u8; 4] = *b"AXB2";
/// Fixed AXR2 frame: 4 magic + 7 u32 + 8 u64 = 96, then the whole capacity-bounded koid
/// array. 2400 bytes stays inside the inline-channel cap.
pub const SNAPSHOT2_LEN: usize = 96 + 8 * RESIDENT_MAX;
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

/// AX4's capacity-bounded snapshot. Field order is exactly AXR1's 7 u32 + 8 u64; the
/// process array is then the fixed [`RESIDENT_MAX`] slots, written contiguously from index
/// 0 and zero after `live`. Decode rejects any frame whose nonzero array entries do not
/// occupy exactly `[0, live)` or whose counts exceed its reported capacity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotWide {
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
    pub processes: [u64; RESIDENT_MAX],
}

impl Default for SnapshotWide {
    fn default() -> Self {
        Self {
            enabled: false,
            capacity: 0,
            live: 0,
            stored: 0,
            blocked: 0,
            ready: 0,
            running: 0,
            frames_issued: 0,
            heap_used: 0,
            roots: 0,
            root_ns: 0,
            root_min_ns: 0,
            root_max_ns: 0,
            syscalls: 0,
            switches: 0,
            processes: [0; RESIDENT_MAX],
        }
    }
}

impl SnapshotWide {
    pub fn encode(&self) -> [u8; SNAPSHOT2_LEN] {
        let mut b = [0; SNAPSHOT2_LEN];
        b[..4].copy_from_slice(b"AXR2");
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
        if b.len() != SNAPSHOT2_LEN || &b[..4] != b"AXR2" {
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
        let live = s.live as usize;
        (u32_at(4) <= 1
            && s.capacity as usize <= RESIDENT_MAX
            && s.live <= s.capacity
            && s.live <= s.stored
            && s.stored <= s.capacity
            && s.blocked
                .checked_add(s.ready)
                .and_then(|v| v.checked_add(s.running))
                == Some(s.live)
            && live <= RESIDENT_MAX
            && s.processes[..live].iter().all(|p| *p != 0)
            && s.processes[live..].iter().all(|p| *p == 0))
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

    #[test]
    fn axr1_wire_view_is_unchanged() {
        // AXR1 is a frozen ABI: AX0's recorded probes decode these exact bytes. Changing the
        // magic, length, field order or 12-slot array is a compatibility break, not a tweak.
        assert_eq!(QUERY, *b"AXB1");
        assert_eq!(SNAPSHOT_LEN, 192);
        assert_eq!(RESIDENT_LIMIT, 12);
        let mut s = Snapshot {
            capacity: 12,
            live: 1,
            stored: 1,
            ready: 1,
            frames_issued: 1,
            ..Snapshot::default()
        };
        s.processes[0] = 7;
        let b = s.encode();
        assert_eq!(b.len(), 192);
        assert_eq!(&b[..4], b"AXR1");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 12);
        assert_eq!(u64::from_le_bytes(b[32..40].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(b[96..104].try_into().unwrap()), 7);
        assert_eq!(Snapshot::decode(&b), Some(s));
    }

    #[test]
    fn axr2_roundtrips_at_capacity_boundaries_and_rejects_bad_frames() {
        assert_eq!(QUERY2, *b"AXB2");
        assert_eq!(SNAPSHOT2_LEN, 2400);
        assert_eq!(RESIDENT_MAX, 288);
        let mut s = SnapshotWide {
            capacity: RESIDENT_MAX as u32,
            live: 3,
            stored: 4,
            blocked: 1,
            ready: 1,
            running: 1,
            frames_issued: 9,
            heap_used: 10,
            roots: 11,
            root_ns: 12,
            root_min_ns: 13,
            root_max_ns: 14,
            syscalls: 15,
            switches: 16,
            ..SnapshotWide::default()
        };
        s.processes[..3].copy_from_slice(&[40, 41, 42]);
        let b = s.encode();
        assert_eq!(b.len(), 2400);
        assert_eq!(&b[..4], b"AXR2");
        assert_eq!(SnapshotWide::decode(&b), Some(s.clone()));

        // The whole 288-slot frame is valid with every slot populated.
        let mut full = s.clone();
        full.live = RESIDENT_MAX as u32;
        full.stored = RESIDENT_MAX as u32;
        full.blocked = RESIDENT_MAX as u32;
        full.ready = 0;
        full.running = 0;
        for i in 0..RESIDENT_MAX {
            full.processes[i] = 1000 + i as u64;
        }
        assert_eq!(SnapshotWide::decode(&full.encode()), Some(full));

        // capacity above the policy bound, live above capacity, a gap inside the live
        // prefix, a nonzero stale slot and a bad magic all fail closed.
        for mutate in [
            |s: &mut SnapshotWide| s.capacity = RESIDENT_MAX as u32 + 1,
            |s: &mut SnapshotWide| s.live = 4,
            |s: &mut SnapshotWide| s.processes[1] = 0,
            |s: &mut SnapshotWide| s.processes[3] = 99,
            |s: &mut SnapshotWide| s.stored = 2,
        ] {
            let mut bad = s.clone();
            mutate(&mut bad);
            assert_eq!(SnapshotWide::decode(&bad.encode()), None);
        }
        let mut bad = b;
        bad[0] = b'X';
        assert_eq!(SnapshotWide::decode(&bad), None);
        for len in [0, 191, 192, 2399] {
            assert_eq!(SnapshotWide::decode(&b[..len.min(b.len())]), None);
        }
    }
}
