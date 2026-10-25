//j556
//j559
//j565
//j571
//j601

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
    InterruptComplete = 25,
    ClockGet = 26,
    TimerCreate = 27,
    FutexWait = 28,
    FutexWake = 29,
    DebugWrite = 30,
    AddressSpaceCreate = 31,
    ProcessRun = 32,
    PortBind = 33,
    HandleKoid = 34,
    ProcessWait = 35,
    ResourceCreateChild = 36,
    PortUnbind = 37,
    /// Transfer framebuffer text-console ownership from the Stage-A HAL to a
    /// userspace driver holding the matching Resource capability.
    FramebufferClaim = 38,
    IoMmuFromResource = 39,
    DeviceCtxCreate = 40,
    DeviceVmarMap = 41,
    DeviceVmarUnmap = 42,
    DeviceCtxWaitFault = 43,
    DeviceCtxInfo = 44,
    /// Allocate a physically-contiguous, zeroed VMO and report its physical base, so a
    /// userspace driver can hand device-visible addresses to a DMA engine (xHCI rings,
    /// DCBAA, transfer buffers). Distinct from [`Syscall::VmoCreate`], whose anonymous
    /// backing is per-page and non-contiguous. Returns the handle in the primary result
    /// and the physical base in the secondary register (like `ChannelRead`'s handle).
    VmoCreateContiguous = 45,
    /// End a process its caller manages: take its threads off the CPU and out of every wait,
    /// then close its handles as an exit would, so peers see `PEER_CLOSED`. Requires
    /// `Rights::MANAGE` on the process handle, which `ProcessCreate` gives the creator. A
    /// process cannot kill itself (that is [`Syscall::ProcessExit`]).
    ProcessKill = 46,
    /// Keep a diagnostic in the kernel log (`dmesg`) at a [`log_level`]: `(level, ptr, len)`.
    /// The console also shows it while its level is at or above the console's. Same authority
    /// as `DebugWrite`, which stays the call for terminal output (J559).
    LogWrite = 47,
    /// Read the kernel log: `(resource, offset, ptr, len)` returns bytes copied in `x0` and the
    /// next offset in `x1`. Needs a Resource handle with `MANAGE` (the root resource).
    LogRead = 48,
    /// Set the console log level: `(resource, level)` returns the previous level;
    /// [`LOG_LEVEL_QUERY`] only reports it. Needs a Resource handle with `MANAGE`.
    LogLevel = 49,
    /// Raise the power domain of the device whose register aperture starts at `aperture_base`:
    /// `(resource, aperture_base)` returns a status. The kernel runs the device's whole power-up
    /// sequence (clock first, then power, memory repair, bus out of idle) from its own table;
    /// the caller passes no registers, bits or rates. Needs a Resource with `WRITE` whose window
    /// contains the aperture. Idempotent (DESIGN/020).
    DevicePowerUp = 50,
    /// Report a device's power, bus and clock state: `(resource, aperture_base, ptr, len)`
    /// copies a [`DevicePowerStatus`]. Needs a Resource with `READ` whose window contains the
    /// aperture (DESIGN/020).
    DevicePowerStatus = 51,
    /// Set one approved reset line: `(resource, aperture_base, line_index, asserted)`.
    /// Needs Resource WRITE covering the live DTB-gated device aperture. `asserted` is 0/1;
    /// no register, bit mask or sequence is supplied by the caller (DESIGN/020 D0).
    DeviceResetSet = 52,
}

/// Kernel log levels, syslog's numbering: lower is more urgent (J559).
pub mod log_level {
    pub const EMERG: u8 = 0;
    pub const ALERT: u8 = 1;
    pub const CRIT: u8 = 2;
    pub const ERR: u8 = 3;
    pub const WARNING: u8 = 4;
    pub const NOTICE: u8 = 5;
    pub const INFO: u8 = 6;
    pub const DEBUG: u8 = 7;
}

/// The commands the kernel's console shell answers (`kernel/src/shell.rs`, with `power`'s verbs)
/// when Sora forwards a line it does not handle itself. Sora reports any other name as not found,
/// status 127 (J565); the kernel's tests keep this list honest.
pub const KERNEL_SHELL_COMMANDS: &[&str] = &[
    "help", "ver", "banner", "mem", "ps", "ticks", "uptime", "echo", "clear", "ipcstat",
    "shutdown", "poweroff", "halt", "reboot", "reset",
];

/// `LogLevel`'s level argument that reports the console level without changing it.
pub const LOG_LEVEL_QUERY: u64 = u64::MAX;

pub const IRQ_KIND_TLMM_GPIO: u32 = 0x8000_0000;
pub const TLMM_GPIO_PIN_MASK: u32 = 0x0000_0fff;
pub const TLMM_GPIO_FLAGS_SHIFT: u32 = 12;
pub const TLMM_GPIO_FLAGS_MASK: u32 = 0x000f_f000;

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoMmuKind {
    Virtio = 1,
    SmmuV3 = 2,
    /// ARM MMU-500 (SMMUv2) — the SC8280XP `apps_smmu` the X13s USB/display/etc. ride. A wholly
    /// different programming model from SmmuV3 (Stream Match Registers → context banks).
    SmmuV2 = 3,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceCtxInfo {
    pub fault_record: u64,
    pub fault_addr: u64,
}

/// What [`Syscall::DevicePowerStatus`] copies out (DESIGN/020 §6). The flags let a driver act
/// without knowing its SoC's register layout; the raw words keep the evidence, in the order the
/// kernel's boot report reads them.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DevicePowerStatus {
    /// [`DevicePowerStatus::VERSION`].
    pub version: u32,
    /// `DevicePowerStatus::*` flag bits.
    pub flags: u32,
    /// The power-controller words the kernel read.
    pub pmu: [u32; 6],
    /// The clock-and-reset-controller words the kernel read.
    pub cru: [u32; 8],
}

impl DevicePowerStatus {
    pub const VERSION: u32 = 1;
    /// The domain is powered.
    pub const POWERED: u32 = 1 << 0;
    /// The domain's memory repair has completed since it was powered.
    pub const REPAIR_DONE: u32 = 1 << 1;
    /// The device's bus interface is idle (passing no traffic).
    pub const BUS_IDLE: u32 = 1 << 2;
    /// Software is asking the bus interface to go idle.
    pub const BUS_IDLE_REQUESTED: u32 = 1 << 3;
    /// The device clock is at the kernel's safe boot rate.
    pub const CLOCK_AT_BOOT_RATE: u32 = 1 << 4;
    /// Every precondition for touching the aperture holds, so `ResourceMintMmio` will mint it.
    pub const APERTURE_MINTABLE: u32 = 1 << 5;

    pub const fn contains(self, flag: u32) -> bool {
        self.flags & flag == flag
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TlmmGpioIrq {
    pub pin: u32,
    pub flags: u32,
}

pub const fn tlmm_gpio_irq_window_base(pin: u32) -> u32 {
    IRQ_KIND_TLMM_GPIO | (pin & TLMM_GPIO_PIN_MASK)
}

pub const fn tlmm_gpio_irq(pin: u32, flags: u32) -> u32 {
    tlmm_gpio_irq_window_base(pin) | ((flags << TLMM_GPIO_FLAGS_SHIFT) & TLMM_GPIO_FLAGS_MASK)
}

pub const fn decode_tlmm_gpio_irq(irq: u32) -> Option<TlmmGpioIrq> {
    if irq & IRQ_KIND_TLMM_GPIO == 0 {
        None
    } else {
        Some(TlmmGpioIrq {
            pin: irq & TLMM_GPIO_PIN_MASK,
            flags: (irq & TLMM_GPIO_FLAGS_MASK) >> TLMM_GPIO_FLAGS_SHIFT,
        })
    }
}

pub const fn interrupt_authority_key(irq: u32) -> u32 {
    match decode_tlmm_gpio_irq(irq) {
        Some(gpio) => tlmm_gpio_irq_window_base(gpio.pin),
        None => irq,
    }
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

    #[test]
    fn tlmm_gpio_irq_carries_pin_and_flags_but_authorizes_by_line() {
        let irq = tlmm_gpio_irq(104, 8);
        assert_eq!(
            decode_tlmm_gpio_irq(irq),
            Some(TlmmGpioIrq { pin: 104, flags: 8 })
        );
        assert_eq!(interrupt_authority_key(irq), tlmm_gpio_irq_window_base(104));

        // Trigger flags ride with the synthetic IRQ for HAL configuration, while authority stays
        // keyed by TLMM pin. A Resource minted for one trigger encoding therefore still covers the
        // same line if a future board legitimately requests another trigger type. — KESTREL
        let falling = tlmm_gpio_irq(104, 2);
        assert_eq!(
            decode_tlmm_gpio_irq(falling),
            Some(TlmmGpioIrq { pin: 104, flags: 2 })
        );
        assert_eq!(
            interrupt_authority_key(falling),
            interrupt_authority_key(irq)
        );
    }

    #[test]
    fn interrupt_complete_occupies_the_designated_syscall_slot() {
        assert_eq!(Syscall::InterruptComplete as usize, 25);
        assert_eq!(Syscall::TimerCreate as usize, 27);
    }

    #[test]
    fn contiguous_dma_vmo_occupies_the_next_syscall_slot() {
        assert_eq!(Syscall::DeviceCtxInfo as usize, 44);
        assert_eq!(Syscall::VmoCreateContiguous as usize, 45);
        // D1: the futex pair had been reserved numbers with no implementation since the ABI was
        // laid out; pinning them here keeps the now-live syscalls where userland expects them.
        assert_eq!(Syscall::FutexWait as usize, 28);
        assert_eq!(Syscall::FutexWake as usize, 29);
    }

    #[test]
    fn process_kill_follows_the_contiguous_dma_vmo() {
        assert_eq!(Syscall::ProcessKill as usize, 46);
    }

    #[test]
    fn device_power_calls_follow_the_kernel_log_calls() {
        assert_eq!(Syscall::DevicePowerUp as usize, 50);
        assert_eq!(Syscall::DevicePowerStatus as usize, 51);
        assert_eq!(Syscall::DeviceResetSet as usize, 52);
        assert_eq!(Errno::TimedOut.status(), -10);
        assert_eq!(Errno::BadState.status(), -11);
    }

    #[test]
    fn device_power_status_is_a_fixed_64_byte_record() {
        assert_eq!(core::mem::size_of::<DevicePowerStatus>(), 64);
        assert_eq!(core::mem::align_of::<DevicePowerStatus>(), 4);
        let flags = [
            DevicePowerStatus::POWERED,
            DevicePowerStatus::REPAIR_DONE,
            DevicePowerStatus::BUS_IDLE,
            DevicePowerStatus::BUS_IDLE_REQUESTED,
            DevicePowerStatus::CLOCK_AT_BOOT_RATE,
            DevicePowerStatus::APERTURE_MINTABLE,
        ];
        let mut seen = 0;
        for flag in flags {
            assert_eq!(flag.count_ones(), 1);
            assert_eq!(seen & flag, 0, "flag bits overlap");
            seen |= flag;
        }
        let status = DevicePowerStatus {
            flags: DevicePowerStatus::POWERED | DevicePowerStatus::REPAIR_DONE,
            ..DevicePowerStatus::default()
        };
        assert!(status.contains(DevicePowerStatus::POWERED));
        assert!(!status.contains(DevicePowerStatus::APERTURE_MINTABLE));
    }

    #[test]
    fn the_kernel_log_calls_follow_process_kill() {
        assert_eq!(Syscall::LogWrite as usize, 47);
        assert_eq!(Syscall::LogRead as usize, 48);
        assert_eq!(Syscall::LogLevel as usize, 49);
        assert_eq!(log_level::EMERG, 0);
        assert_eq!(log_level::NOTICE, 5);
        assert_eq!(log_level::DEBUG, 7);
    }
}

/// The most bytes a single `VmoRead` or `VmoWrite` call moves. Callers chunk at this cap;
/// the kernel refuses an over-cap `VmoWrite` with `InvalidArgs` instead of silently
/// truncating it (J601).
pub const VMO_TRANSFER_MAX: usize = 256;

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
    /// The hardware did not reach the requested state in time (DESIGN/020).
    TimedOut = -10,
    /// The object is not in a state that allows the operation, e.g. minting the registers of a
    /// powered-down device (DESIGN/020).
    BadState = -11,
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
