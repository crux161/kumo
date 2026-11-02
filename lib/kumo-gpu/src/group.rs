//j752
//! Driver-owned group facts, readable after the command channel has closed. The VMO is
//! CPU metadata only: no GPU mapping, DMA pin or synthetic fence completion. — MILVUS

pub const REPORT_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum GroupState {
    Usable = 0,
    Fatal = 1,
    TimedOut = 2,
    Unknown = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupReport {
    pub state: GroupState,
    pub gpu_status: u32,
}
impl GroupReport {
    pub const USABLE: Self = Self {
        state: GroupState::Usable,
        gpu_status: 0,
    };
    pub const UNKNOWN: Self = Self {
        state: GroupState::Unknown,
        gpu_status: 0,
    };
    pub const TIMED_OUT: Self = Self {
        state: GroupState::TimedOut,
        gpu_status: 0,
    };
    pub const fn valid(self) -> bool {
        matches!(self.state, GroupState::Fatal) || self.gpu_status == 0
    }
    pub const fn from_words(state: u32, gpu_status: u32) -> Option<Self> {
        let state = match state {
            0 => GroupState::Usable,
            1 => GroupState::Fatal,
            2 => GroupState::TimedOut,
            3 => GroupState::Unknown,
            _ => return None,
        };
        let result = Self { state, gpu_status };
        if result.valid() {
            Some(result)
        } else {
            None
        }
    }
    pub const fn encode(self, group: u32) -> [u8; REPORT_BYTES] {
        let mut bytes = [0; REPORT_BYTES];
        let words = [
            u32::from_le_bytes(*b"KGS1"),
            group,
            self.state as u32,
            self.gpu_status,
        ];
        let mut n = 0;
        while n < 4 {
            let word = words[n].to_le_bytes();
            let mut j = 0;
            while j < 4 {
                bytes[4 * n + j] = word[j];
                j += 1;
            }
            n += 1;
        }
        bytes
    }
    pub fn decode(bytes: &[u8], group: u32) -> Option<Self> {
        if bytes.len() != REPORT_BYTES
            || group == 0
            || &bytes[..4] != b"KGS1"
            || u32::from_le_bytes(bytes[4..8].try_into().ok()?) != group
        {
            return None;
        }
        Self::from_words(
            u32::from_le_bytes(bytes[8..12].try_into().ok()?),
            u32::from_le_bytes(bytes[12..16].try_into().ok()?),
        )
    }
    /// Positive fault/timeout evidence survives loss. An old usable sample cannot prove
    /// health once the session or logical VM is gone. — MILVUS
    pub fn observed(self, live: bool) -> Self {
        if !live && self.state == GroupState::Usable {
            Self::UNKNOWN
        } else {
            self
        }
    }
}

/// One READ-only status VMO grant. The caller owns it and uses VmoRead, not a writable map.
pub struct GroupWatch<H> {
    pub group: u32,
    pub report: GroupReport,
    pub handle: H,
}
