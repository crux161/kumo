//j794
//! NV2: the firmware-assigned BAR0 prefix, read-only. No queues, DMA or register writes.
//! This first route supports DWC's unmatched-address MEM path only. A translated MEM
//! route is deliberately refused until separately proven. — TIMBERDOODLE

use crate::endpoint::{decode_bar, permitted_page, select_config_page, Bar, Endpoint};
use crate::Report;
use core::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Preconditions,
    Endpoint,
    MemoryDisabled,
    Bar,
    RootWindow,
    Sideband,
    Route,
    Mapping,
    Cap,
    Version,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Preconditions => "NV0/NV1 route precondition unmet",
            Self::Endpoint => "endpoint is not the current NVMe function in D0",
            Self::MemoryDisabled => "root or endpoint memory decode disabled",
            Self::Bar => "BAR0 is not an assigned, aligned, non-prefetchable 64-bit memory BAR",
            Self::RootWindow => "BAR0 prefix outside root memory window or DTB/SoC aperture",
            Self::Sideband => "read sideband is not ordinary MEM or ATU bypass is selected",
            Self::Route => "BAR0 CPU page has an ATU match or an unsupported outbound region",
            Self::Mapping => "read-only BAR0 page mapping refused",
            Self::Cap => "absent or inconsistent NVMe CAP",
            Self::Version => "absent or unsupported NVMe register version",
        })
    }
}

/// Prove the identity/no-ATU-match route before the first BAR read. RK3588 TRM
/// 11.6.3.2 describes matching; the DWC 4.10a databook's No Address Match Result
/// describes untranslated addresses with slave sideband headers. The RK3588 read
/// sideband is observed, not assumed from reset. Actual CAP/VS validate it on metal.
/// This is not a claim to a 5.60a databook or a BAR-size probe. — TIMBERDOODLE
pub fn select_register_page(report: &Report, endpoint: &Endpoint) -> Result<u64, Error> {
    let config = select_config_page(report).map_err(|_| Error::Preconditions)?;
    if endpoint.page != config
        || endpoint.class_revision >> 8 != 0x010802
        || endpoint.header != 0
        || matches!(endpoint.id as u16, 0 | 0xffff)
        || endpoint.pm.is_some_and(|pm| pm.control & 3 != 0)
    {
        return Err(Error::Endpoint);
    }
    let dbi = report.dbi.as_ref().ok_or(Error::Preconditions)?;
    if dbi.command & 2 == 0 || endpoint.command & 2 == 0 {
        return Err(Error::MemoryDisabled);
    }
    let address = match decode_bar(&endpoint.bars, 0).map_err(|_| Error::Bar)? {
        Bar::Memory {
            address,
            wide: true,
            prefetch: false,
        } if address != 0 && address & 4095 == 0 => address,
        _ => return Err(Error::Bar),
    };
    let end = address.checked_add(4095).ok_or(Error::Bar)?;
    let (base, limit) = dbi.memory_window();
    if address < base || end > limit || !permitted_page(&report.topology, address) {
        return Err(Error::RootWindow);
    }
    let client = report.client.ok_or(Error::Preconditions)?;
    if client.axi_slv_atu_bypass != 0 || client.axi_slv_misc_info != 0 {
        return Err(Error::Sideband);
    }
    for r in dbi.outbound.iter().filter(|r| r.enabled()) {
        // Even an inverted region or an inverse-match mode far from BAR0 can destroy
        // the no-match proof. Refuse every unmodelled header/match control.
        if r.ctrl1 & !(0x1f | (1 << 13)) != 0
            || r.ctrl2 & !((1 << 31) | (1 << 28)) != 0
            || !matches!(r.tlp_type(), 0 | 2 | 4 | 5)
            || (r.ctrl2 & (1 << 28) != 0 && !matches!(r.tlp_type(), 4 | 5))
            || r.limit() < r.base()
            || (r.base() <= end && r.limit() >= address)
        {
            return Err(Error::Route);
        }
    }
    Ok(address)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub physical: u64,
    pub cap: u64,
    pub vs: u32,
    pub cc: u32,
    pub csts: u32,
    pub aqa: u32,
    pub asq: u64,
    pub acq: u64,
    pub cmbsz: u32,
}

impl Snapshot {
    pub const fn max_queue_entries(&self) -> u32 {
        (self.cap as u32 & 0xffff) + 1
    }
    pub const fn timeout_field(&self) -> u32 {
        (self.cap >> 24) as u32 & 0xff
    }
    /// Conservative Linux-compatible RDY polling budget, not a write/ownership grant.
    pub const fn timeout_budget_ms(&self) -> u32 {
        (self.timeout_field() + 1) * 500
    }
    pub const fn doorbell_stride(&self) -> u32 {
        4 << ((self.cap >> 32) as u32 & 0xf)
    }
    pub const fn min_page_shift(&self) -> u32 {
        12 + ((self.cap >> 48) as u32 & 0xf)
    }
    pub const fn max_page_shift(&self) -> u32 {
        12 + ((self.cap >> 52) as u32 & 0xf)
    }
}

fn read64(read: &mut impl FnMut(u16) -> u32, offset: u16) -> u64 {
    // CAP is constant; firmware's queue bases are not modified by this observer.
    u64::from(read(offset)) | (u64::from(read(offset + 4)) << 32)
}

/// Only fixed, aligned offsets in BAR0[0..0x40). CAP rejection stops before VS;
/// version rejection stops before controller/queue state. No doorbell/table reads.
pub fn probe(
    report: &Report,
    endpoint: &Endpoint,
    read: &mut impl FnMut(u16) -> u32,
) -> Result<Snapshot, Error> {
    let physical = select_register_page(report, endpoint)?;
    let cap = read64(read, 0);
    if cap == 0 || cap == u64::MAX || (cap >> 48) & 0xf > (cap >> 52) & 0xf {
        return Err(Error::Cap);
    }
    let vs = read(8);
    if !matches!(vs >> 16, 1 | 2) {
        return Err(Error::Version);
    }
    Ok(Snapshot {
        physical,
        cap,
        vs,
        cc: read(0x14),
        csts: read(0x1c),
        aqa: read(0x24),
        asq: read64(read, 0x28),
        acq: read64(read, 0x30),
        cmbsz: read(0x3c),
    })
}

impl fmt::Display for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "NV2 ROUTE        Check     BAR0={:#x} CPU={:#x} unmatched identity MEM; read bypass=0 sideband=0   OK", self.physical, self.physical)?;
        writeln!(f, "NV2 CAP          Check     raw={:#x} mqes={} cqr={} to={} budget={}ms dstrd={} stride={} css={:#x} mpsmin/max={}/{}   OK",
            self.cap, self.max_queue_entries(), (self.cap >> 16) & 1,
            self.timeout_field(), self.timeout_budget_ms(), (self.cap >> 32) & 0xf,
            self.doorbell_stride(), (self.cap >> 37) & 0xff,
            self.min_page_shift(), self.max_page_shift())?;
        writeln!(f, "NV2 CONTROL      Check     vs={:#x} v{}.{}.{} cc={:#x} en={} csts={:#x} rdy={} cfs={} shst={}   OK",
            self.vs, self.vs >> 16, (self.vs >> 8) & 0xff, self.vs & 0xff,
            self.cc, self.cc & 1, self.csts, self.csts & 1, (self.csts >> 1) & 1, (self.csts >> 2) & 3)?;
        writeln!(f, "NV2 ADMIN        Check     aqa={:#x} sq/cq={}/{} asq={:#x} acq={:#x} cmbsz={:#x}; inherited, untouched   OK",
            self.aqa, (self.aqa & 0xfff) + 1, ((self.aqa >> 16) & 0xfff) + 1, self.asq, self.acq, self.cmbsz)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{AtuRegion, Client, Clock, Dbi, Power};
    use std::{format, vec::Vec};

    fn firmware() -> (Report, Endpoint) {
        let mut outbound = [AtuRegion::default(); crate::ATU_REGIONS];
        outbound[0] = AtuRegion {
            ctrl1: 4,
            ctrl2: 0x9000_0000,
            upper_base: 9,
            lower_base: 0x10_0000,
            limit: 0x10_ffff,
            lower_target: 0x10_0000,
            ..AtuRegion::default()
        };
        let r = Report {
            topology: crate::discover_rk3588_pcie30x4(include_bytes!(
                "../../../rk3588-orangepi-5-plus.dtb"
            ))
            .unwrap(),
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
        };
        let e = Endpoint {
            page: select_config_page(&r).unwrap(),
            id: 0xa808_144d,
            class_revision: 0x0108_0200,
            header: 0,
            command: 6,
            status: 0x10,
            bars: [0xf000_0004, 0, 0, 0, 0, 0],
            subsystem: 0xa801_144d,
            cap_count: 0,
            pcie: None,
            msix: None,
            pm: None,
        };
        (r, e)
    }

    #[test]
    fn closed_routes_never_read_bar0() {
        let (r, e) = firmware();
        let mut cases = Vec::new();
        let mut bad = r;
        bad.stop = Some(crate::Stop::PhyNotRunning);
        cases.push((bad, e));
        let mut bad = r;
        bad.client.as_mut().unwrap().axi_slv_misc_info = 4;
        cases.push((bad, e));
        let mut bad = r;
        bad.client.as_mut().unwrap().axi_slv_misc_info = 1 << 21;
        cases.push((bad, e));
        let mut bad = r;
        bad.client.as_mut().unwrap().axi_slv_atu_bypass = 2;
        cases.push((bad, e));
        let mut bad = r;
        bad.dbi.as_mut().unwrap().command = 5;
        cases.push((bad, e));
        let mut bad = r;
        bad.dbi.as_mut().unwrap().mem = 0xf020_f020;
        cases.push((bad, e));
        let mut bad = r;
        bad.topology.config_len = 0;
        cases.push((bad, e));
        for (base, limit, ctrl2) in [
            (0xf000_0000, 0xf000_0fff, 1 << 31),
            (0xf000_0fff, 0xf000_ffff, 1 << 31),
            (0xf000_0000, 0xf000_0000, 1 << 31),
            (0x1000, 0, 1 << 31),
            (0x1000, 0x2000, 0xa000_0000),
        ] {
            let mut bad = r;
            bad.dbi.as_mut().unwrap().outbound[1] = AtuRegion {
                lower_base: base,
                limit,
                ctrl2,
                ..AtuRegion::default()
            };
            cases.push((bad, e));
        }
        for bar in [0, 0xf000_0000, 0xf000_000c, 0xf000_0014, 0x8000_0004] {
            let mut bad = e;
            bad.bars[0] = bar;
            cases.push((r, bad));
        }
        let mut bad = e;
        bad.command = 4;
        cases.push((r, bad));
        let mut bad = e;
        bad.page.physical += 4096;
        cases.push((r, bad));
        let mut bad = e;
        bad.pm = Some(crate::endpoint::Pm {
            offset: 0x40,
            capability: 3,
            control: 3,
        });
        cases.push((r, bad));
        for (r, e) in cases {
            let mut reads = 0;
            assert!(probe(&r, &e, &mut |_| {
                reads += 1;
                0
            })
            .is_err());
            assert_eq!(reads, 0);
        }
    }

    #[test]
    fn prefix_reads_and_decodes_inherited_controller_without_doorbells() {
        let (r, e) = firmware();
        let mut reads = Vec::new();
        let s = probe(&r, &e, &mut |offset| {
            reads.push(offset);
            match offset {
                0 => 0x0501_03ff,
                4 => 0x0020_0020,
                8 => 0x0001_0300,
                0x14 => 0x0046_0001,
                0x1c => 1,
                0x24 => 0x001f_001f,
                0x28 => 0xab00_0000,
                0x30 => 0xab01_0000,
                _ => 0,
            }
        })
        .unwrap();
        assert_eq!(
            reads,
            [0, 4, 8, 0x14, 0x1c, 0x24, 0x28, 0x2c, 0x30, 0x34, 0x3c]
        );
        assert_eq!(
            (
                s.max_queue_entries(),
                s.timeout_budget_ms(),
                s.doorbell_stride()
            ),
            (1024, 3000, 4)
        );
        assert_eq!((s.min_page_shift(), s.max_page_shift()), (12, 14));
        assert_eq!(s.asq, 0xab00_0000);
        let text = format!("{s}");
        assert!(text.contains("en=1"));
        assert!(text.lines().all(|l| l.len() < 240));
    }

    #[test]
    fn absent_or_inconsistent_cap_and_bad_version_stop_before_state_reads() {
        let (r, e) = firmware();
        for (cap, vs, expected) in [
            (0, 0, 2),
            (u64::MAX, 0, 2),
            (1 | (1 << 48), 0, 2),
            (1, 0, 3),
            (1, u32::MAX, 3),
            (1, 0x0003_0000, 3),
        ] {
            let mut reads = Vec::new();
            assert!(probe(&r, &e, &mut |o| {
                reads.push(o);
                match o {
                    0 => cap as u32,
                    4 => (cap >> 32) as u32,
                    8 => vs,
                    _ => panic!(),
                }
            })
            .is_err());
            assert_eq!(reads.len(), expected);
        }
    }
}
