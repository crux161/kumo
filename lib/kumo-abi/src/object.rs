use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign};

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Handle(pub u32);

pub const INVALID_HANDLE: Handle = Handle(0);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct KoId(pub u64);

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
    Job = 1,
    Process = 2,
    Thread = 3,
    Channel = 4,
    Port = 5,
    Vmo = 6,
    Vmar = 7,
    Event = 8,
    Timer = 9,
    Resource = 10,
    Interrupt = 11,
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Rights(pub u32);

impl Rights {
    pub const DUPLICATE: Self = Self(1 << 0);
    pub const TRANSFER: Self = Self(1 << 1);
    pub const READ: Self = Self(1 << 2);
    pub const WRITE: Self = Self(1 << 3);
    pub const MAP: Self = Self(1 << 4);
    pub const WAIT: Self = Self(1 << 5);
    pub const SIGNAL: Self = Self(1 << 6);
    pub const EXECUTE: Self = Self(1 << 7);
    pub const MANAGE: Self = Self(1 << 8);

    pub const ALL: Self = Self(
        Self::DUPLICATE.0
            | Self::TRANSFER.0
            | Self::READ.0
            | Self::WRITE.0
            | Self::MAP.0
            | Self::WAIT.0
            | Self::SIGNAL.0
            | Self::EXECUTE.0
            | Self::MANAGE.0,
    );

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, needed: Self) -> bool {
        self.0 & needed.0 == needed.0
    }
}

impl BitOr for Rights {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Rights {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Rights {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for Rights {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Signals(pub u32);

impl Signals {
    pub const READABLE: Self = Self(1 << 0);
    pub const WRITABLE: Self = Self(1 << 1);
    pub const PEER_CLOSED: Self = Self(1 << 2);
    pub const TERMINATED: Self = Self(1 << 3);
    pub const IRQ: Self = Self(1 << 4);
    pub const TIMER: Self = Self(1 << 5);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, needed: Self) -> bool {
        self.0 & needed.0 == needed.0
    }
}

impl BitOr for Signals {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Signals {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights_masks_are_composable() {
        let rw = Rights::READ | Rights::WRITE;
        assert!(rw.contains(Rights::READ));
        assert!(rw.contains(Rights::WRITE));
        assert!(!rw.contains(Rights::MAP));
    }
}
