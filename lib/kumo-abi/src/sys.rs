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
