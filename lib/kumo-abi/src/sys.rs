#[repr(usize)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Syscall {
    HandleClose = 0,
    HandleDuplicate = 1,
    HandleReplace = 2,
    ChannelCreate = 3,
    ChannelWrite = 4,
    ChannelRead = 5,
    ChannelCall = 6,
    PortCreate = 7,
    PortWait = 8,
    ObjectWaitOne = 9,
    ObjectWaitMany = 10,
    ObjectSignal = 11,
    VmoCreate = 12,
    VmoRead = 13,
    VmoWrite = 14,
    VmarMap = 15,
    VmarProtect = 16,
    VmarUnmap = 17,
    ProcessCreate = 18,
    ThreadCreate = 19,
    ThreadStart = 20,
    ProcessExit = 21,
    ResourceMintMmio = 22,
    InterruptCreate = 23,
    InterruptWait = 24,
    ClockGet = 25,
    TimerCreate = 26,
    FutexWait = 27,
    FutexWake = 28,
    DebugWrite = 29,
    AddressSpaceCreate = 30,
    ProcessRun = 31,
    PortBind = 32,
    HandleKoid = 33,
    ProcessWait = 34,
    ResourceCreateChild = 35,
}

pub type Status = i32;

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Errno {
    Ok = 0,
    InvalidArgs = -1,
    BadHandle = -2,
    WrongType = -3,
    AccessDenied = -4,
    ShouldWait = -5,
    PeerClosed = -6,
    NoMemory = -7,
    NotSupported = -8,
    Internal = -9,
}

impl Errno {
    pub const fn status(self) -> Status {
        self as Status
    }

    pub const fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Page flags for `vmar_map`. Bit-identical to `kumo_hal::PageFlags` (the
/// arch-neutral HAL owns the canonical definition; these constants let
/// userspace programs construct map flags without depending on the HAL).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VmarFlags(pub u64);

impl VmarFlags {
    pub const READ: Self = Self(1 << 0);
    pub const WRITE: Self = Self(1 << 1);
    pub const EXECUTE: Self = Self(1 << 2);
    /// EL0-accessible page.
    pub const USER: Self = Self(1 << 3);
    /// Map as Device-nGnRnE memory (MMIO registers), not Normal cacheable.
    pub const DEVICE: Self = Self(1 << 4);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, needed: Self) -> bool {
        self.0 & needed.0 == needed.0
    }
}

impl core::ops::BitOr for VmarFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}
