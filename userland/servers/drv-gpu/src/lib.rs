#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j572
//j574

//! G1b/G2a: power, identity and reset for the RK3588 Mali-G610. Recovery class: soft-state; all
//! state is rediscovered on launch and DevicePowerUp is idempotent. This slice probes identity,
//! then soft-resets the GPU and requires its completion interrupt; L2, address spaces, command
//! serving and supervised recovery follow with the later GPU execution slices.

use kumo_abi::{DevicePowerStatus, Errno, Status};

/// DESIGN/020's device grant. The kernel's live DTB gate decides whether this unit exists.
pub const APERTURE_BASE: u64 = 0xfb00_0000;
pub const APERTURE_LEN: u64 = 0x20_0000;
pub const IRQ_BASE: u32 = 124;
pub const IRQ_COUNT: u32 = 3;
/// The GPU block's interrupt (INTID 126 = SPI 94; RESEARCH/009 §4.1/§4.5). The job and MMU
/// blocks are `IRQ_BASE` and `IRQ_BASE + 1`.
pub const GPU_IRQ: u32 = IRQ_BASE + 2;

// RESEARCH/009 §1.2 and §4.2: read-only GPU_CONTROL identity registers.
const GPU_ID: u32 = 0x000;
const SHADER_PRESENT_LO: u32 = 0x100;
const SHADER_PRESENT_HI: u32 = 0x104;

// RESEARCH/009 §4.2, §4.4 and §4.5: the GPU_CONTROL interrupt and command words.
const GPU_INT_RAWSTAT: u32 = 0x020;
const GPU_INT_CLEAR: u32 = 0x024;
const GPU_INT_MASK: u32 = 0x028;
const GPU_CMD: u32 = 0x030;
const GPU_IRQ_RESET_COMPLETED: u32 = 1 << 8;
const GPU_CMD_SOFT_RESET: u32 = 0x0000_0101;
/// panthor's reset timeout (RESEARCH/009 §4.4).
pub const RESET_TIMEOUT_NS: u64 = 100_000_000;

pub trait ProbeIo {
    fn power_up(&mut self) -> Status;
    fn power_status(&mut self) -> Result<DevicePowerStatus, Status>;
    fn map_aperture(&mut self) -> Result<(), ProbeError>;
    fn read32(&mut self, offset: u32) -> u32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeError {
    PowerUp(Status),
    PowerStatus(Status),
    NotReady(DevicePowerStatus),
    Mint,
    Map(Status),
    Identity { gpu_id: u32, shader_present: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identity {
    pub gpu_id: u32,
    pub shader_present: u64,
    pub power: DevicePowerStatus,
}

impl Identity {
    pub fn cores(self) -> u32 {
        self.shader_present.count_ones()
    }
}

/// No aperture access is possible through this path until power-up and the complete status
/// check succeed. The kernel independently guards the MMIO mint. — KESTREL
pub fn probe(io: &mut impl ProbeIo) -> Result<Identity, ProbeError> {
    let status = io.power_up();
    if status != Errno::Ok.status() {
        return Err(ProbeError::PowerUp(status));
    }
    let power = io.power_status().map_err(ProbeError::PowerStatus)?;
    let required = DevicePowerStatus::POWERED
        | DevicePowerStatus::REPAIR_DONE
        | DevicePowerStatus::CLOCK_AT_BOOT_RATE
        | DevicePowerStatus::APERTURE_MINTABLE;
    let idle = DevicePowerStatus::BUS_IDLE | DevicePowerStatus::BUS_IDLE_REQUESTED;
    if power.version != DevicePowerStatus::VERSION
        || !power.contains(required)
        || power.flags & idle != 0
    {
        return Err(ProbeError::NotReady(power));
    }
    io.map_aperture()?;
    let gpu_id = io.read32(GPU_ID);
    // Identify the documented model before reading any further GPU register. — KESTREL
    if gpu_id & 0xffff_0000 != 0xa867_0000 {
        return Err(ProbeError::Identity {
            gpu_id,
            shader_present: 0,
        });
    }
    let shader_present =
        u64::from(io.read32(SHADER_PRESENT_LO)) | (u64::from(io.read32(SHADER_PRESENT_HI)) << 32);
    if shader_present == 0 {
        return Err(ProbeError::Identity {
            gpu_id,
            shader_present,
        });
    }
    Ok(Identity {
        gpu_id,
        shader_present,
        power,
    })
}

/// The outcome of the bound completion-interrupt wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion {
    Fired,
    TimedOut,
}

/// How the reset completion was observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetPath {
    Interrupt,
    /// The wait expired, but panthor's RAWSTAT recheck showed completion anyway.
    PolledAfterTimeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetError {
    /// The completion wait could not be armed; no GPU register was touched.
    Arm(Status),
    /// No completion interrupt, and RAWSTAT does not show the reset bit.
    Timeout { rawstat: u32 },
    /// The wait fired, but RAWSTAT does not show the reset bit.
    UnexpectedCompletion { rawstat: u32 },
}

/// The device-side half of reset, kept behind a trait so the sequence is host-testable.
pub trait ResetIo {
    fn read32(&mut self, offset: u32) -> u32;
    fn write32(&mut self, offset: u32, value: u32);
    /// Create the completion interrupt and bind its wait **before** the command is issued.
    /// The wait must expire after `timeout_ns`.
    fn arm_completion(&mut self, timeout_ns: u64) -> Result<(), Status>;
    fn wait_completion(&mut self) -> Completion;
    /// Release the wait objects and acknowledge the interrupt.
    fn complete(&mut self);
}

/// G2a: ask the GPU for a soft reset and require its completion interrupt. Clears any stale
/// completion, unmasks only `RESET_COMPLETED`, and always restores a quiet mask before
/// acknowledging, so a failed or late reset cannot storm a line nobody serves. — TIMBERDOODLE
pub fn soft_reset(io: &mut impl ResetIo) -> Result<ResetPath, ResetError> {
    io.arm_completion(RESET_TIMEOUT_NS)
        .map_err(ResetError::Arm)?;
    io.write32(GPU_INT_CLEAR, GPU_IRQ_RESET_COMPLETED);
    io.write32(GPU_INT_MASK, GPU_IRQ_RESET_COMPLETED);
    io.write32(GPU_CMD, GPU_CMD_SOFT_RESET);
    let completion = io.wait_completion();
    // panthor rechecks RAWSTAT after its timeout before declaring failure (RESEARCH/009 §4.4).
    let rawstat = io.read32(GPU_INT_RAWSTAT);
    io.write32(GPU_INT_CLEAR, GPU_IRQ_RESET_COMPLETED);
    io.write32(GPU_INT_MASK, 0);
    io.complete();
    if rawstat & GPU_IRQ_RESET_COMPLETED == 0 {
        return Err(match completion {
            Completion::Fired => ResetError::UnexpectedCompletion { rawstat },
            Completion::TimedOut => ResetError::Timeout { rawstat },
        });
    }
    Ok(match completion {
        Completion::Fired => ResetPath::Interrupt,
        Completion::TimedOut => ResetPath::PolledAfterTimeout,
    })
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    struct Fake {
        power: Status,
        status: Result<DevicePowerStatus, Status>,
        mapping: Result<(), ProbeError>,
        gpu_id: u32,
        shaders: u64,
        calls: Vec<u32>,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                power: 0,
                status: Ok(DevicePowerStatus {
                    version: 1,
                    flags: 0x33,
                    ..DevicePowerStatus::default()
                }),
                mapping: Ok(()),
                gpu_id: 0xa867_0005,
                shaders: 0x50005,
                calls: Vec::new(),
            }
        }
    }

    impl ProbeIo for Fake {
        fn power_up(&mut self) -> Status {
            self.calls.push(1);
            self.power
        }
        fn power_status(&mut self) -> Result<DevicePowerStatus, Status> {
            self.calls.push(2);
            self.status
        }
        fn map_aperture(&mut self) -> Result<(), ProbeError> {
            self.calls.push(3);
            self.mapping
        }
        fn read32(&mut self, offset: u32) -> u32 {
            self.calls.push(0x1000 + offset);
            match offset {
                GPU_ID => self.gpu_id,
                SHADER_PRESENT_LO => self.shaders as u32,
                SHADER_PRESENT_HI => (self.shaders >> 32) as u32,
                _ => panic!("unexpected register"),
            }
        }
    }

    #[test]
    fn powers_checks_maps_then_reads_identity_and_both_shader_words() {
        let mut io = Fake {
            shaders: 0x1_0005_0001,
            ..Fake::default()
        };
        let identity = probe(&mut io).unwrap();
        assert_eq!(identity.gpu_id, 0xa867_0005);
        assert_eq!(identity.cores(), 4);
        assert_eq!(io.calls, [1, 2, 3, 0x1000, 0x1100, 0x1104]);
    }

    #[test]
    fn power_failure_never_requests_status_or_mmio() {
        for error in [Errno::NotSupported, Errno::TimedOut, Errno::AccessDenied] {
            let mut io = Fake {
                power: error.status(),
                ..Fake::default()
            };
            assert_eq!(probe(&mut io), Err(ProbeError::PowerUp(error.status())));
            assert_eq!(io.calls, [1]);
        }
    }

    #[test]
    fn incomplete_or_unknown_status_never_mints() {
        for flags in [0, 0x32, 0x31, 0x23, 0x13, 0x37, 0x3b] {
            let mut io = Fake::default();
            io.status.as_mut().unwrap().flags = flags;
            assert!(matches!(probe(&mut io), Err(ProbeError::NotReady(_))));
            assert_eq!(io.calls, [1, 2]);
        }
        let mut io = Fake::default();
        io.status.as_mut().unwrap().version = 2;
        assert!(matches!(probe(&mut io), Err(ProbeError::NotReady(_))));
        assert_eq!(io.calls, [1, 2]);
        io.status = Err(Errno::BadHandle.status());
        io.calls.clear();
        assert_eq!(probe(&mut io), Err(ProbeError::PowerStatus(-2)));
        assert_eq!(io.calls, [1, 2]);
    }

    #[test]
    fn mint_or_map_failure_never_reads() {
        for error in [ProbeError::Mint, ProbeError::Map(-7)] {
            let mut io = Fake {
                mapping: Err(error),
                ..Fake::default()
            };
            assert_eq!(probe(&mut io), Err(error));
            assert_eq!(io.calls, [1, 2, 3]);
        }
    }

    #[test]
    fn unexpected_identity_stops_at_id_and_zero_cores_is_not_online() {
        for gpu_id in [0, u32::MAX, 0xa862_0005] {
            let mut io = Fake {
                gpu_id,
                ..Fake::default()
            };
            assert!(matches!(probe(&mut io), Err(ProbeError::Identity { .. })));
            assert_eq!(io.calls, [1, 2, 3, 0x1000]);
        }
        let mut io = Fake {
            shaders: 0,
            ..Fake::default()
        };
        assert!(matches!(probe(&mut io), Err(ProbeError::Identity { .. })));
    }

    const ARM: u32 = 1;
    const WAIT: u32 = 2;
    const COMPLETE: u32 = 3;
    const WRITE: u32 = 0x1000;
    const READ: u32 = 0x2000;

    struct FakeReset {
        rawstat: u32,
        completion: Completion,
        arm: Result<(), Status>,
        calls: Vec<u32>,
        writes: Vec<(u32, u32)>,
    }

    impl FakeReset {
        fn new(rawstat: u32, completion: Completion) -> Self {
            Self {
                rawstat,
                completion,
                arm: Ok(()),
                calls: Vec::new(),
                writes: Vec::new(),
            }
        }
    }

    impl ResetIo for FakeReset {
        fn read32(&mut self, offset: u32) -> u32 {
            self.calls.push(READ + offset);
            self.rawstat
        }
        fn write32(&mut self, offset: u32, value: u32) {
            self.calls.push(WRITE + offset);
            self.writes.push((offset, value));
        }
        fn arm_completion(&mut self, _timeout_ns: u64) -> Result<(), Status> {
            self.calls.push(ARM);
            self.arm
        }
        fn wait_completion(&mut self) -> Completion {
            self.calls.push(WAIT);
            self.completion
        }
        fn complete(&mut self) {
            self.calls.push(COMPLETE);
        }
    }

    fn success_sequence() -> Vec<u32> {
        std::vec![
            ARM,
            WRITE + GPU_INT_CLEAR,
            WRITE + GPU_INT_MASK,
            WRITE + GPU_CMD,
            WAIT,
            READ + GPU_INT_RAWSTAT,
            WRITE + GPU_INT_CLEAR,
            WRITE + GPU_INT_MASK,
            COMPLETE,
        ]
    }

    fn quiet_mask_writes() -> Vec<(u32, u32)> {
        std::vec![
            (GPU_INT_CLEAR, GPU_IRQ_RESET_COMPLETED),
            (GPU_INT_MASK, GPU_IRQ_RESET_COMPLETED),
            (GPU_CMD, GPU_CMD_SOFT_RESET),
            (GPU_INT_CLEAR, GPU_IRQ_RESET_COMPLETED),
            (GPU_INT_MASK, 0),
        ]
    }

    #[test]
    fn soft_reset_arms_before_any_register_and_restores_a_quiet_mask() {
        let mut io = FakeReset::new(GPU_IRQ_RESET_COMPLETED, Completion::Fired);
        assert_eq!(soft_reset(&mut io), Ok(ResetPath::Interrupt));
        assert_eq!(io.calls, success_sequence());
        assert_eq!(io.writes, quiet_mask_writes());
    }

    #[test]
    fn timeout_rechecks_rawstat_like_panthor() {
        let mut io = FakeReset::new(GPU_IRQ_RESET_COMPLETED, Completion::TimedOut);
        assert_eq!(soft_reset(&mut io), Ok(ResetPath::PolledAfterTimeout));
        assert_eq!(io.calls, success_sequence());
        assert_eq!(io.writes, quiet_mask_writes());
    }

    #[test]
    fn a_timeout_without_completion_errors_but_still_quiets_the_line() {
        let mut io = FakeReset::new(0, Completion::TimedOut);
        assert_eq!(soft_reset(&mut io), Err(ResetError::Timeout { rawstat: 0 }));
        assert_eq!(io.calls, success_sequence());
        assert_eq!(io.writes, quiet_mask_writes());
    }

    #[test]
    fn an_irq_without_the_reset_bit_is_unexpected() {
        let mut io = FakeReset::new(0, Completion::Fired);
        assert_eq!(
            soft_reset(&mut io),
            Err(ResetError::UnexpectedCompletion { rawstat: 0 })
        );
        assert_eq!(io.calls, success_sequence());
        assert_eq!(io.writes, quiet_mask_writes());
    }

    #[test]
    fn an_unarmable_wait_touches_no_register() {
        let mut io = FakeReset::new(0, Completion::Fired);
        io.arm = Err(Errno::NotSupported.status());
        assert_eq!(
            soft_reset(&mut io),
            Err(ResetError::Arm(Errno::NotSupported.status()))
        );
        assert_eq!(io.calls, [ARM]);
        assert!(io.writes.is_empty());
    }

    #[test]
    fn register_offsets_and_timeout_match_research_009() {
        assert_eq!(GPU_INT_RAWSTAT, 0x020);
        assert_eq!(GPU_INT_CLEAR, 0x024);
        assert_eq!(GPU_INT_MASK, 0x028);
        assert_eq!(GPU_CMD, 0x030);
        assert_eq!(GPU_IRQ_RESET_COMPLETED, 1 << 8);
        assert_eq!(GPU_CMD_SOFT_RESET, 0x0000_0101);
        assert_eq!(GPU_IRQ, 126);
        assert_eq!(RESET_TIMEOUT_NS, 100_000_000);
    }
}
