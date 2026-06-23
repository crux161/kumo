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
    PortUnbind = 36,
    /// Transfer framebuffer text-console ownership from the Stage-A HAL to a
    /// userspace driver holding the matching Resource capability.
    FramebufferClaim = 37,
}

/// Options for [`Syscall::ProcessRun`].
///
/// Unmarked arguments are copied into the child when they name handles. A
/// transfer-marked argument is instead moved: it is removed from the caller
/// only after the child has been admitted successfully.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessRunFlags(pub u64);

impl ProcessRunFlags {
    /// Admit the child without blocking the caller.
    pub const ASYNC: Self = Self(1 << 0);
    /// Move `arg` (x0) from the caller's handle table into the child.
    pub const TRANSFER_ARG: Self = Self(1 << 1);
    /// Move `arg2` (x1) from the caller's handle table into the child.
    pub const TRANSFER_ARG2: Self = Self(1 << 2);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn contains(self, needed: Self) -> bool {
        self.0 & needed.0 == needed.0
    }
}

impl core::ops::BitOr for ProcessRunFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_run_flags_are_composable() {
        let flags = ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG;
        assert!(flags.contains(ProcessRunFlags::ASYNC));
        assert!(flags.contains(ProcessRunFlags::TRANSFER_ARG));
        assert!(!flags.contains(ProcessRunFlags::TRANSFER_ARG2));
        assert_eq!(flags.bits(), 0b011);
    }

    #[test]
    fn framebuffer_mapping_policy_is_distinct_from_device_mmio() {
        let framebuffer = VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED;
        assert!(framebuffer.contains(VmarFlags::UNCACHED));
        assert!(!framebuffer.contains(VmarFlags::DEVICE));
        assert_ne!(VmarFlags::UNCACHED.0, VmarFlags::DEVICE.0);
    }
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
    /// Map MMIO-backed memory as Normal-NC. This is for linear framebuffers whose
    /// bytes are memory-like scanout storage, not device registers.
    pub const UNCACHED: Self = Self(1 << 5);

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
