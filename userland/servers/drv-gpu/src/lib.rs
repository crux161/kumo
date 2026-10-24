#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j572

//! G1b: power and identity for the RK3588 Mali-G610. Recovery class: soft-state; all state is
//! rediscovered on launch and DevicePowerUp is idempotent. This first slice probes once and exits;
//! command serving, reset and supervised recovery follow with the GPU execution slices.

use kumo_abi::{DevicePowerStatus, Errno, Status};

/// DESIGN/020's device grant. The kernel's live DTB gate decides whether this unit exists.
pub const APERTURE_BASE: u64 = 0xfb00_0000;
pub const APERTURE_LEN: u64 = 0x20_0000;
pub const IRQ_BASE: u32 = 124;
pub const IRQ_COUNT: u32 = 3;

// RESEARCH/009 §1.2 and §4.2: read-only GPU_CONTROL identity registers.
const GPU_ID: u32 = 0x000;
const SHADER_PRESENT_LO: u32 = 0x100;
const SHADER_PRESENT_HI: u32 = 0x104;

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
}
