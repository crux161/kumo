//j797
//! NV3a: inherit-only PCIe/NVMe status and mint admission. No power/reset/ATU/SMMU
//! writes exist here. Only the already-proven unmatched BAR0 prefix is writable;
//! root/client/config windows are raw-read-only. D5 is cooperative, not isolation.
//! — TIMBERDOODLE

use crate::power_unit::{DevicePowerReadout, DevicePowerUnitInfo};
use crate::smmuv3::{self, Mmu600PcieControlStatus, Mmu600PciePowerStatus, Mmu600PcieStatusReport};
use kumo_pcie::{endpoint::select_config_page, nvme::validate_memory_route, RegisterRead, Report};

pub const BAR0: u64 = 0xf000_0000;
pub const PREFIX_LEN: u64 = 4096;
const ROOT_WINDOWS: [(u64, u64); 5] = [
    (0xf000_0000, 0x100_0000),
    (0x9_0000_0000, 0x4000_0000),
    (kumo_pcie::PCIE30X4_DBI_ALIAS, kumo_pcie::PCIE30X4_DBI_LEN),
    (kumo_pcie::PCIE30X4_DBI, kumo_pcie::PCIE30X4_DBI_LEN),
    (kumo_pcie::PCIE30X4_APB, 0x1_0000),
];
const READ_WINDOWS: [(u64, u64); 3] = [
    (kumo_pcie::PCIE30X4_APB, 4096),
    (kumo_pcie::PCIE30X4_DBI_ALIAS, 4096),
    (kumo_pcie::PCIE30X4_DBI_ALIAS + 0x30_0000, 0x2000),
];

fn contains(base: u64, bytes: u64, start: u64, len: u64) -> bool {
    len != 0
        && start >= base
        && start
            .checked_add(len)
            .zip(base.checked_add(bytes))
            .is_some_and(|(end, limit)| end <= limit)
}

pub fn overlaps(base: u64, len: u64) -> bool {
    if len == 0 {
        return false;
    }
    let Some(end) = base.checked_add(len) else {
        return true;
    };
    ROOT_WINDOWS
        .iter()
        .any(|&(start, bytes)| base < start + bytes && start < end)
}

/// Reserve raw root/config writes even off-board, like the display reservation.
/// Only a VMO contained in the first BAR page may carry WRITE in this slice.
pub fn read_only(base: u64, len: u64) -> bool {
    overlaps(base, len) && !contains(BAR0, PREFIX_LEN, base, len)
}

pub fn info(dtb: &[u8], aperture: u64) -> Option<DevicePowerUnitInfo> {
    let t = kumo_pcie::discover_rk3588_pcie30x4(dtb)?;
    (aperture == BAR0 && t.config == BAR0 && t.config_len >= PREFIX_LEN).then_some(
        DevicePowerUnitInfo {
            name: "PCIE NVME INHERIT",
            aperture_base: BAR0,
            aperture_len: PREFIX_LEN,
        },
    )
}

/// Root-side CPU routing only, never endpoint identity or BAR sizing.
fn admitted(report: &Report, base: u64, len: u64) -> bool {
    contains(BAR0, PREFIX_LEN, base, len)
        || READ_WINDOWS
            .iter()
            .any(|&(start, bytes)| contains(start, bytes, base, len))
        || select_config_page(report).is_ok_and(|page| contains(page.physical, 4096, base, len))
}

fn observe_policy(
    dtb: &[u8],
    report: &Report,
    io: &mut impl RegisterRead,
) -> Option<(smmuv3::Mmu600PcieSnapshot, u32)> {
    validate_memory_route(report, BAR0).ok()?;
    let topology = smmuv3::discover_rk3588_mmu600_pcie(dtb)?;
    let power = Mmu600PciePowerStatus {
        pwr_gate_sts0: io.read32(smmuv3::RK3588_PMU_PWR_GATE_STS0),
        submem_pwr_gate_sts: io.read32(smmuv3::RK3588_PMU_SUBMEM_PWR_GATE_STS),
        qchannel_pwr_sts: io.read32(smmuv3::RK3588_PMU_QCHANNEL_PWR_STS),
    };
    let control = Mmu600PcieControlStatus {
        gate_con34: io.read32(smmuv3::RK3588_CRU_GATE_CON34),
        softrst_con34: io.read32(smmuv3::RK3588_CRU_SOFTRST_CON34),
    };
    let status = Mmu600PcieStatusReport {
        topology,
        power,
        control,
    };
    let snapshot = smmuv3::mmu600_pcie_snapshot(&status, report, &mut |offset| {
        io.read32(topology.base + u64::from(offset))
    })
    .ok()?;
    Some((snapshot, power.qchannel_pwr_sts))
}

fn readout(dtb: &[u8], report: &Report, io: &mut impl RegisterRead) -> DevicePowerReadout {
    let policy = observe_policy(dtb, report, io);
    let power = report.power;
    let c = report.clock;
    let pmu = match policy {
        Some((s, q)) => [power.pwr_gate_sts0, s.idr0, s.cr0, s.cr0ack, s.gbpa, q],
        None => [
            power.pwr_gate_sts0,
            0,
            u32::MAX,
            u32::MAX,
            u32::MAX,
            u32::MAX,
        ],
    };
    DevicePowerReadout {
        powered: power.php_up() && power.pcie_up(),
        // No BISR observation or rate selection is made by an inherit-only unit.
        repair_done: false,
        bus_idle: power.biu_idle() || power.biu_acknowledged(),
        bus_idle_requested: power.biu_requested(),
        clock_at_boot_rate: false,
        aperture_ready: policy.is_some_and(|(s, _)| s.disabled_bypass()),
        // Selected diagnostic words, not the GPU unit's register order. — TIMBERDOODLE
        pmu,
        cru: [
            c.gate32,
            c.gate33,
            c.gate34,
            c.gate39,
            c.softrst32,
            c.softrst33,
            c.softrst34,
            c.php_gate0,
        ],
    }
}

pub fn status(dtb: &[u8], aperture: u64, io: &mut impl RegisterRead) -> Option<DevicePowerReadout> {
    info(dtb, aperture)?;
    let topology = kumo_pcie::discover_rk3588_pcie30x4(dtb)?;
    let report = kumo_pcie::probe(topology, io);
    Some(readout(dtb, &report, io))
}

pub fn blocked(dtb: &[u8], base: u64, len: u64, io: &mut impl RegisterRead) -> bool {
    if !overlaps(base, len) {
        return false;
    }
    if base.checked_add(len).is_none() {
        return true;
    }
    let Some(topology) = kumo_pcie::discover_rk3588_pcie30x4(dtb) else {
        return false;
    };
    let report = kumo_pcie::probe(topology, io);
    !admitted(&report, base, len) || !readout(dtb, &report, io).aperture_ready
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use kumo_pcie::{AtuRegion, Client, Clock, Dbi, Power};
    use std::vec::Vec;
    const DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    const X13S: &[u8] = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");

    fn report() -> Report {
        let mut outbound = [AtuRegion::default(); kumo_pcie::ATU_REGIONS];
        outbound[0] = AtuRegion {
            ctrl1: 4,
            ctrl2: 0x9000_0000,
            upper_base: 9,
            lower_base: 0x10_0000,
            limit: 0x10_ffff,
            lower_target: 0x10_0000,
            ..AtuRegion::default()
        };
        Report {
            topology: kumo_pcie::discover_rk3588_pcie30x4(DTB).unwrap(),
            power: Power::default(),
            clock: Clock::default(),
            phy: None,
            client: Some(Client {
                general_con: 0x4c,
                ltssm_status: 0x23_0011,
                ..Client::default()
            }),
            dbi: Some(Dbi {
                command: 7,
                bus: 0x0001_0100,
                mem: 0xf000_f000,
                outbound,
                ..Dbi::default()
            }),
            stop: None,
        }
    }
    #[derive(Default)]
    struct Fake {
        values: Vec<(u64, u32)>,
        reads: Vec<u64>,
    }
    impl RegisterRead for Fake {
        fn read32(&mut self, p: u64) -> u32 {
            self.reads.push(p);
            self.values
                .iter()
                .find_map(|&(a, v)| (a == p).then_some(v))
                .unwrap_or(match p {
                    smmuv3::RK3588_PMU_QCHANNEL_PWR_STS => 2 << 16,
                    smmuv3::RK3588_MMU600_PCIE_BASE => 0x080f_3e0f,
                    _ => 0,
                })
        }
    }
    impl Fake {
        fn aperture_read(&self) -> bool {
            self.reads.iter().any(|&p| {
                (smmuv3::RK3588_MMU600_PCIE_BASE..smmuv3::RK3588_MMU600_PCIE_BASE + 4096)
                    .contains(&p)
            })
        }
    }
    #[test]
    fn status_only_unit_is_exact_and_off_board_is_inert() {
        assert_eq!(info(DTB, BAR0).unwrap().aperture_len, 4096);
        assert!(info(DTB, BAR0 + 4096).is_none());
        let mut io = Fake::default();
        assert!(status(X13S, BAR0, &mut io).is_none());
        assert!(status(b"bad", BAR0, &mut io).is_none());
        assert!(!blocked(X13S, BAR0, 4096, &mut io));
        assert!(!blocked(DTB, 0xfb00_0000, 4096, &mut io));
        assert!(io.reads.is_empty());
        assert!(
            crate::power_unit::live_unit(DTB, BAR0).is_none(),
            "inherit unit must have no power-up write sequence"
        );
    }
    #[test]
    fn reservations_never_allow_raw_root_or_config_writes() {
        assert!(!read_only(BAR0, 4096));
        for (b, n) in [
            (BAR0, 8192),
            (BAR0 + 4096, 4096),
            (0x9_0010_0000, 4096),
            (kumo_pcie::PCIE30X4_APB, 4096),
            (kumo_pcie::PCIE30X4_DBI_ALIAS, 4096),
            (kumo_pcie::PCIE30X4_DBI_ALIAS + 0x30_0000, 8192),
            (kumo_pcie::PCIE30X4_DBI, 4096),
        ] {
            assert!(read_only(b, n), "{b:#x}+{n}");
        }
        assert!(!read_only(0xfb00_0000, 4096));
        let mut io = Fake::default();
        assert!(blocked(DTB, u64::MAX, 4096, &mut io));
        assert!(io.reads.is_empty());
    }
    #[test]
    fn exact_prefix_and_read_windows_only() {
        let r = report();
        for (b, n) in [
            (BAR0, 4096),
            (0x9_0010_0000, 4096),
            (kumo_pcie::PCIE30X4_APB, 4096),
            (kumo_pcie::PCIE30X4_DBI_ALIAS, 4096),
            (kumo_pcie::PCIE30X4_DBI_ALIAS + 0x30_0000, 8192),
        ] {
            assert!(admitted(&r, b, n));
        }
        for (b, n) in [
            (BAR0, 8192),
            (BAR0 + 4096, 4096),
            (0x9_0020_0000, 4096),
            (kumo_pcie::PCIE30X4_DBI_ALIAS, 8192),
            (kumo_pcie::PCIE30X4_APB + 4096, 4096),
        ] {
            assert!(!admitted(&r, b, n));
        }
    }
    #[test]
    fn root_refusals_precede_every_smmu_access() {
        let mut cases = Vec::new();
        let mut r = report();
        r.stop = Some(kumo_pcie::Stop::PhyNotRunning);
        cases.push(r);
        let mut r = report();
        r.client.as_mut().unwrap().ltssm_status = 0;
        cases.push(r);
        let mut r = report();
        r.client.as_mut().unwrap().axi_slv_misc_info = 1 << 21;
        cases.push(r);
        let mut r = report();
        r.dbi.as_mut().unwrap().command = 5;
        cases.push(r);
        let mut r = report();
        r.dbi.as_mut().unwrap().outbound[1] = AtuRegion {
            ctrl2: 1 << 31,
            lower_base: BAR0 as u32,
            limit: BAR0 as u32 + 4095,
            ..AtuRegion::default()
        };
        cases.push(r);
        for r in cases {
            let mut io = Fake::default();
            assert!(!readout(DTB, &r, &mut io).aperture_ready);
            assert!(io.reads.is_empty());
        }
    }
    #[test]
    fn closed_mmu_guards_never_read_its_aperture() {
        for (a, v) in [
            (smmuv3::RK3588_PMU_PWR_GATE_STS0, 1 << 21),
            (smmuv3::RK3588_PMU_SUBMEM_PWR_GATE_STS, 1 << 7),
            (smmuv3::RK3588_PMU_QCHANNEL_PWR_STS, 0),
            (smmuv3::RK3588_PMU_QCHANNEL_PWR_STS, (2 << 16) | (1 << 23)),
            (smmuv3::RK3588_CRU_GATE_CON34, 1 << 7),
            (smmuv3::RK3588_CRU_SOFTRST_CON34, 1 << 9),
        ] {
            let mut io = Fake {
                values: std::vec![(a, v)],
                ..Fake::default()
            };
            assert!(!readout(DTB, &report(), &mut io).aperture_ready);
            assert!(!io.aperture_read());
        }
    }
    #[test]
    fn bypass_is_observed_and_every_nonbypass_or_uncertain_policy_refuses() {
        let mut io = Fake::default();
        let s = readout(DTB, &report(), &mut io);
        assert!(s.aperture_ready && s.powered);
        assert!(!s.clock_at_boot_rate && !s.repair_done);
        assert_eq!(&s.pmu[1..5], &[0x080f_3e0f, 0, 0, 0]);
        let base = smmuv3::RK3588_MMU600_PCIE_BASE;
        for pairs in [
            std::vec![(base + 0x20, 1), (base + 0x24, 1)],
            std::vec![(base + 0x24, 1)],
            std::vec![(base + 0x44, 1 << 20)],
            std::vec![(base + 0x44, 1 << 31)],
            std::vec![(base, u32::MAX)],
            std::vec![(base + 0x20, u32::MAX), (base + 0x24, u32::MAX)],
        ] {
            let mut io = Fake {
                values: pairs,
                ..Fake::default()
            };
            assert!(!readout(DTB, &report(), &mut io).aperture_ready);
        }
    }
}
