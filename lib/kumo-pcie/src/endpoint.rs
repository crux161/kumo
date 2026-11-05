//j793
//j794
//! NV1a: read one firmware-enumerated NVMe function. No config-space writes or BAR sizing.
//! Layouts are cross-checked against the local PCIe TRM and Linux's pci_regs.h; no Linux code
//! is copied. Unknown routing, unsafe error response and malformed capabilities stay inert.
//! — TIMBERDOODLE

use crate::{AtuRegion, Report, Topology};
use core::fmt;

const PAGE: u64 = 4096;
const ENABLE: u32 = 1 << 31;
const SHIFT: u32 = 1 << 28;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Nv0Stopped,
    LinkDown,
    ErrorResponse,
    BusNumbers,
    NoConfigWindow,
    UnsupportedWindow,
    OverlappingWindows,
    Mapping,
    Absent,
    Retry,
    Header,
    Class,
    Bar,
    Capabilities,
    LinkMismatch,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Nv0Stopped => "NV0 precondition unmet",
            Self::LinkDown => "root link not stable in L0",
            Self::ErrorResponse => "config error response may raise an AXI error",
            Self::BusNumbers => "firmware bus numbers disagree with DTB",
            Self::NoConfigWindow => {
                "no firmware CFG0 window for the secondary bus; no iATU write attempted"
            }
            Self::UnsupportedWindow => "unsupported or out-of-aperture firmware config window",
            Self::OverlappingWindows => "ambiguous/overlapping outbound windows",
            Self::Mapping => "read-only config page mapping refused",
            Self::Absent => "no endpoint (invalid/absent vendor ID)",
            Self::Retry => "endpoint returned Configuration Request Retry",
            Self::Header => "endpoint header is not Type 0",
            Self::Class => "endpoint is not NVMe class 01:08:02",
            Self::Bar => "malformed or unsupported BAR reference",
            Self::Capabilities => "malformed, cyclic or duplicate standard capabilities",
            Self::LinkMismatch => "endpoint link disagrees with the root port",
        })
    }
}

/// One function's 4 KiB config page, not a grant to the whole translated aperture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigPage {
    pub physical: u64,
    pub bus: u8,
    pub region: u8,
    pub shifted: bool,
}

fn contains(base: u64, bytes: u64, page: u64) -> bool {
    bytes != 0
        && page >= base
        && page
            .checked_add(PAGE)
            .zip(base.checked_add(bytes))
            .is_some_and(|(end, limit)| end <= limit)
}

pub(super) fn permitted_page(t: &Topology, page: u64) -> bool {
    // Both SoC routing and DTB ownership must agree. Never turn firmware's iATU data into
    // a Device alias of RAM or of some other SoC block. — TIMBERDOODLE
    let soc = contains(0xf000_0000, 0x100_0000, page) || contains(0x9_0000_0000, 0x4000_0000, page);
    soc && (contains(t.config, t.config_len, page)
        || t.windows[1..]
            .iter()
            .flatten()
            .any(|w| contains(w.cpu, w.bytes, page)))
}

fn region_page(r: &AtuRegion, bus: u8) -> Result<Option<u64>, Error> {
    if !r.enabled() || r.tlp_type() != 4 {
        return Ok(None);
    }
    if r.ctrl1 & !(0x1f | (1 << 13)) != 0
        || r.ctrl2 & !(ENABLE | SHIFT) != 0
        || r.base() & (PAGE - 1) != 0
        || r.limit() < r.base()
    {
        return Err(Error::UnsupportedWindow);
    }
    let page = if r.ctrl2 & SHIFT != 0 {
        // ECAM consumes untranslated bits [27:12], independently of TARGET.
        let mut p = (r.base() & !0x0fff_ffff) | (u64::from(bus) << 20);
        if p < r.base() {
            p = p.checked_add(1 << 28).ok_or(Error::UnsupportedWindow)?;
        }
        p
    } else {
        if r.upper_target != 0 || r.lower_target != u32::from(bus) << 24 {
            return Ok(None);
        }
        r.base()
    };
    Ok((page
        .checked_add(PAGE - 1)
        .is_some_and(|end| end <= r.limit()))
    .then_some(page))
}

/// Refuse before any config read unless the current snapshot proves a stable route.
pub fn select_config_page(report: &Report) -> Result<ConfigPage, Error> {
    if report.stop.is_some() || !report.topology.valid_for_probe() {
        return Err(Error::Nv0Stopped);
    }
    let client = report.client.ok_or(Error::Nv0Stopped)?;
    let dbi = report.dbi.as_ref().ok_or(Error::Nv0Stopped)?;
    if client.device_type() != 4
        || !client.link_up()
        || client.ltssm_state() != 0x11
        || dbi.training()
    {
        return Err(Error::LinkDown);
    }
    if !dbi.failed_reads_return_ones() || dbi.crs_response() == "axi-error" {
        return Err(Error::ErrorResponse);
    }
    let primary = dbi.bus as u8;
    let secondary = (dbi.bus >> 8) as u8;
    let subordinate = (dbi.bus >> 16) as u8;
    let range = report.topology.bus_range;
    if range[0] >= range[1]
        || primary != range[0]
        || secondary <= primary
        || subordinate < secondary
        || subordinate > range[1]
    {
        return Err(Error::BusNumbers);
    }
    let mut chosen = None;
    for (i, region) in dbi.outbound.iter().enumerate() {
        let Some(physical) = region_page(region, secondary)? else {
            continue;
        };
        if !permitted_page(&report.topology, physical) {
            return Err(Error::UnsupportedWindow);
        }
        if chosen.is_some() {
            return Err(Error::OverlappingWindows);
        }
        chosen = Some(ConfigPage {
            physical,
            bus: secondary,
            region: i as u8,
            shifted: region.ctrl2 & SHIFT != 0,
        });
    }
    let chosen = chosen.ok_or(Error::NoConfigWindow)?;
    for (i, region) in dbi.outbound.iter().enumerate() {
        if !region.enabled() || i == usize::from(chosen.region) {
            continue;
        }
        if region.ctrl2 & (1 << 29) != 0
            || (region.base() <= chosen.physical + PAGE - 1 && region.limit() >= chosen.physical)
        {
            return Err(Error::OverlappingWindows);
        }
    }
    Ok(chosen)
}

/// Offsets are aligned and bounded by this module before reaching the implementation.
pub trait ConfigRead {
    fn read32(&mut self, offset: u16) -> u32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bar {
    Unassigned,
    Io(u32),
    Memory {
        address: u64,
        wide: bool,
        prefetch: bool,
    },
    Upper,
}

pub fn decode_bar(raw: &[u32; 6], index: u8) -> Result<Bar, Error> {
    if index >= 6 {
        return Err(Error::Bar);
    }
    let mut i = 0;
    while i < 6 {
        let value = raw[i];
        let wide = value & 7 == 4;
        if wide && i == 5 {
            return Err(Error::Bar);
        }
        if i == usize::from(index) {
            return if value == 0 {
                Ok(Bar::Unassigned)
            } else if value & 1 != 0 {
                Ok(Bar::Io(value & !3))
            } else if value & 6 == 0 || wide {
                let upper = if wide { u64::from(raw[i + 1]) << 32 } else { 0 };
                Ok(Bar::Memory {
                    address: upper | u64::from(value & !15),
                    wide,
                    prefetch: value & 8 != 0,
                })
            } else {
                Err(Error::Bar)
            };
        }
        if wide && i + 1 == usize::from(index) {
            return Ok(Bar::Upper);
        }
        i += if wide { 2 } else { 1 };
    }
    Err(Error::Bar)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pcie {
    pub offset: u8,
    pub version: u8,
    pub kind: u8,
    pub link_cap: u32,
    pub link: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Msix {
    pub offset: u8,
    pub control: u16,
    pub table: u32,
    pub pba: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pm {
    pub offset: u8,
    pub capability: u16,
    pub control: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub page: ConfigPage,
    pub id: u32,
    pub class_revision: u32,
    pub header: u8,
    pub command: u16,
    pub status: u16,
    pub bars: [u32; 6],
    pub subsystem: u32,
    pub cap_count: u8,
    pub pcie: Option<Pcie>,
    pub msix: Option<Msix>,
    pub pm: Option<Pm>,
}

fn cap_offset(pointer: u8, bytes: u16) -> Result<u16, Error> {
    let p = u16::from(pointer);
    if p < 0x40 || p & 3 != 0 || p + bytes > 0x100 {
        Err(Error::Capabilities)
    } else {
        Ok(p)
    }
}

/// Header and selected standard capabilities only. Unknown capabilities cost one header read.
pub fn probe(report: &Report, io: &mut impl ConfigRead) -> Result<Endpoint, Error> {
    let page = select_config_page(report)?;
    let id = io.read32(0);
    if id == 0xffff_0001 {
        return Err(Error::Retry);
    }
    if id as u16 == 0xffff || id as u16 == 0 {
        return Err(Error::Absent);
    }
    let command_status = io.read32(4);
    let class_revision = io.read32(8);
    let header = (io.read32(12) >> 16) as u8;
    if header & 0x7f != 0 {
        return Err(Error::Header);
    }
    if class_revision >> 8 != 0x01_0802 {
        return Err(Error::Class);
    }
    let mut result = Endpoint {
        page,
        id,
        class_revision,
        header,
        command: command_status as u16,
        status: (command_status >> 16) as u16,
        bars: core::array::from_fn(|i| io.read32(0x10 + i as u16 * 4)),
        subsystem: io.read32(0x2c),
        cap_count: 0,
        pcie: None,
        msix: None,
        pm: None,
    };
    // Validate all slots, including a truncated 64-bit BAR at slot 5.
    for index in 0..6 {
        decode_bar(&result.bars, index)?;
    }
    if result.status & 0x10 == 0 {
        return Err(Error::Capabilities);
    }
    let mut next = io.read32(0x34) as u8;
    let mut visited = 0u64;
    while next != 0 {
        let offset = cap_offset(next, 4)?;
        let mask = 1u64 << (offset / 4);
        if visited & mask != 0 {
            return Err(Error::Capabilities);
        }
        visited |= mask;
        let head = io.read32(offset);
        let bytes = match head as u8 {
            1 => 8,
            0x10 => 20,
            0x11 => 12,
            _ => 4,
        };
        cap_offset(next, bytes)?;
        let extent = ((1u64 << (bytes / 4)) - 1) << (offset / 4);
        if visited & (extent & !mask) != 0 {
            return Err(Error::Capabilities);
        }
        visited |= extent;
        result.cap_count += 1;
        match head as u8 {
            1 => {
                if result.pm.is_some() {
                    return Err(Error::Capabilities);
                }
                cap_offset(next, 8)?;
                result.pm = Some(Pm {
                    offset: next,
                    capability: (head >> 16) as u16,
                    control: io.read32(offset + 4) as u16,
                });
            }
            0x10 => {
                if result.pcie.is_some() {
                    return Err(Error::Capabilities);
                }
                cap_offset(next, 20)?;
                let cap = Pcie {
                    offset: next,
                    version: ((head >> 16) & 15) as u8,
                    kind: ((head >> 20) & 15) as u8,
                    link_cap: io.read32(offset + 12),
                    link: io.read32(offset + 16),
                };
                let link = (cap.link >> 16) & 0x3ff;
                let (speed, width) = report.dbi.as_ref().ok_or(Error::Nv0Stopped)?.link_status();
                if cap.version == 0
                    || cap.kind != 0
                    || cap.link & (1 << 27) != 0
                    || link != (speed | (width << 4))
                {
                    return Err(Error::LinkMismatch);
                }
                result.pcie = Some(cap);
            }
            0x11 => {
                if result.msix.is_some() {
                    return Err(Error::Capabilities);
                }
                cap_offset(next, 12)?;
                let cap = Msix {
                    offset: next,
                    control: (head >> 16) as u16,
                    table: io.read32(offset + 4),
                    pba: io.read32(offset + 8),
                };
                for entry in [cap.table, cap.pba] {
                    if !matches!(
                        decode_bar(&result.bars, (entry & 7) as u8)?,
                        Bar::Memory { .. } | Bar::Unassigned
                    ) {
                        return Err(Error::Bar);
                    }
                }
                result.msix = Some(cap);
            }
            _ => {}
        }
        next = (head >> 8) as u8;
    }
    if result.pcie.is_none() {
        return Err(Error::Capabilities);
    }
    Ok(result)
}

impl fmt::Display for Bar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Unassigned => f.write_str("unassigned"),
            Self::Io(address) => write!(f, "io@{address:#x}"),
            Self::Memory {
                address,
                wide,
                prefetch,
            } => write!(
                f,
                "mem{}@{address:#x} prefetch={}",
                if wide { 64 } else { 32 },
                prefetch as u8
            ),
            Self::Upper => f.write_str("upper-half"),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "NV1 CONFIG       Check     {:02x}:00.0 via={:#x} page=4096 region={} shifted={} read-only   OK",
            self.page.bus, self.page.physical, self.page.region, self.page.shifted as u8)?;
        writeln!(f, "NV1 ENDPOINT     Check     id={:04x}:{:04x} class={:06x} rev={:#x} header={:#x} cmd={:#x} mse={} bme={} status={:#x} subsys={:04x}:{:04x}   OK",
            self.id as u16, self.id >> 16, self.class_revision >> 8, self.class_revision as u8,
            self.header, self.command, ((self.command >> 1) & 1), ((self.command >> 2) & 1), self.status,
            self.subsystem as u16, self.subsystem >> 16)?;
        writeln!(f, "NV1 BARS         Check     raw={:08x}/{:08x}/{:08x}/{:08x}/{:08x}/{:08x} bar0={} sizes not probed   OK",
            self.bars[0], self.bars[1], self.bars[2], self.bars[3], self.bars[4], self.bars[5], decode_bar(&self.bars, 0).map_err(|_|fmt::Error)?)?;
        if let Some(cap) = self.pcie {
            let status = cap.link >> 16;
            writeln!(f, "NV1 PCIE         Check     cap={:#x} v{} type={} link-cap={:#x} link-status={:#x} gen{} x{} matches root   OK",
                cap.offset, cap.version, cap.kind, cap.link_cap, status, status & 15, (status >> 4) & 63)?;
        }
        if let Some(cap) = self.msix {
            writeln!(f, "NV1 MSI-X        Check     cap={:#x} vectors={} enabled={} masked={} table=BAR{}+{:#x} pba=BAR{}+{:#x}; not accessed   OK",
                cap.offset, (cap.control & 0x7ff) + 1, (cap.control >> 15) & 1, (cap.control >> 14) & 1,
                cap.table & 7, cap.table & !7, cap.pba & 7, cap.pba & !7)?;
        }
        if let Some(cap) = self.pm {
            writeln!(
                f,
                "NV1 PM           Check     cap={:#x} pmc={:#x} pmcsr={:#x} D{}; unchanged   OK",
                cap.offset,
                cap.capability,
                cap.control,
                cap.control & 3
            )?;
        }
        writeln!(f, "NV1 READ-ONLY    Check     {} standard capabilities; no config/BAR/iATU/NVMe writes   OK", self.cap_count)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{discover_rk3588_pcie30x4, Client, Clock, Dbi, Power, Stop};
    use std::{format, vec::Vec};

    fn snapshot() -> Report {
        let mut outbound = [AtuRegion::default(); 16];
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
            topology: discover_rk3588_pcie30x4(include_bytes!(
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
                bus: 0x0001_0100,
                link_cap: 0x43,
                link_control_status: 0x2043_0000,
                outbound,
                ..Dbi::default()
            }),
            stop: None,
        }
    }
    struct Fake {
        words: [u32; 64],
        reads: Vec<u16>,
    }
    impl Fake {
        fn nvme() -> Self {
            let mut f = Self {
                words: [0; 64],
                reads: Vec::new(),
            };
            for (off, value) in [
                (0, 0xa501_144d),
                (4, 0x10_0006),
                (8, 0x0108_0201),
                (0x10, 0x2000_0004),
                (0x14, 9),
                (0x2c, 0xa501_144d),
                (0x34, 0x40),
                (0x40, 0x0003_5001),
                (0x44, 0),
                (0x50, 0x0002_7010),
                (0x5c, 0x43),
                (0x60, 0x43_0000),
                (0x70, 0x000f_0011),
                (0x74, 0x2000),
                (0x78, 0x3000),
            ] {
                f.set(off, value);
            }
            f
        }
        fn set(&mut self, offset: u16, value: u32) {
            self.words[usize::from(offset / 4)] = value;
        }
    }
    impl ConfigRead for Fake {
        fn read32(&mut self, offset: u16) -> u32 {
            assert!(
                offset & 3 == 0 && offset < 0x100,
                "unexpected config access {offset:#x}"
            );
            self.reads.push(offset);
            self.words[usize::from(offset / 4)]
        }
    }
    #[test]
    fn firmware_ecam_routing_is_derived_from_bus_and_cpu_address() {
        let mut r = snapshot();
        let p = select_config_page(&r).unwrap();
        assert_eq!((p.physical, p.bus, p.region), (0x9_0010_0000, 1, 0));
        r.dbi.as_mut().unwrap().outbound[0].lower_target = 0xdead_0000;
        assert_eq!(select_config_page(&r).unwrap(), p);
        let w = &mut r.dbi.as_mut().unwrap().outbound[0];
        w.ctrl2 = ENABLE;
        w.lower_target = 1 << 24;
        assert_eq!(select_config_page(&r).unwrap().physical, p.physical);
        r.dbi.as_mut().unwrap().outbound[0].lower_target = 2 << 24;
        assert_eq!(select_config_page(&r), Err(Error::NoConfigWindow));
    }
    #[test]
    fn closed_preconditions_make_zero_config_reads() {
        let mut cases = Vec::new();
        let mut r = snapshot();
        r.stop = Some(Stop::PhyNotRunning);
        cases.push((r, Error::Nv0Stopped));
        let mut r = snapshot();
        r.client.as_mut().unwrap().ltssm_status = 0;
        cases.push((r, Error::LinkDown));
        for bit in [0, 2, 3, 4] {
            let mut r = snapshot();
            r.dbi.as_mut().unwrap().amba_error_response = if bit >= 3 { 3 << 3 } else { 1 << bit };
            cases.push((r, Error::ErrorResponse));
        }
        let mut r = snapshot();
        r.dbi.as_mut().unwrap().bus = 0;
        cases.push((r, Error::BusNumbers));
        let mut r = snapshot();
        r.dbi.as_mut().unwrap().outbound[0].ctrl2 = 0;
        cases.push((r, Error::NoConfigWindow));
        for (r, e) in cases {
            let mut f = Fake::nvme();
            assert_eq!(probe(&r, &mut f), Err(e));
            assert!(f.reads.is_empty());
        }
    }
    #[test]
    fn ambiguous_unsupported_and_out_of_aperture_windows_are_refused() {
        for flag in [1 << 29, 1 << 23, 1 << 27] {
            let mut r = snapshot();
            r.dbi.as_mut().unwrap().outbound[0].ctrl2 |= flag;
            assert_eq!(select_config_page(&r), Err(Error::UnsupportedWindow));
        }
        let mut r = snapshot();
        let w = r.dbi.as_ref().unwrap().outbound[0];
        r.dbi.as_mut().unwrap().outbound[1] = AtuRegion { ctrl1: 0, ..w };
        assert_eq!(select_config_page(&r), Err(Error::OverlappingWindows));
        r.dbi.as_mut().unwrap().outbound[1] = w;
        assert_eq!(select_config_page(&r), Err(Error::OverlappingWindows));
        let mut r = snapshot();
        r.dbi.as_mut().unwrap().outbound[0].upper_base = 0;
        assert_eq!(select_config_page(&r), Err(Error::UnsupportedWindow));
        let mut r = snapshot();
        r.topology.windows[2] = None;
        assert_eq!(select_config_page(&r), Err(Error::UnsupportedWindow));
        let mut r = snapshot();
        r.dbi.as_mut().unwrap().outbound[0].limit = 0x10_0ffe;
        assert_eq!(select_config_page(&r), Err(Error::NoConfigWindow));
    }
    #[test]
    fn endpoint_identity_bars_and_capabilities_are_read_without_sizing_or_table_access() {
        let mut f = Fake::nvme();
        let e = probe(&snapshot(), &mut f).unwrap();
        assert_eq!(e.id, 0xa501_144d);
        assert_eq!(e.command, 6);
        assert_eq!(e.class_revision >> 8, 0x010802);
        assert_eq!(
            decode_bar(&e.bars, 0),
            Ok(Bar::Memory {
                address: 0x9_2000_0000,
                wide: true,
                prefetch: false
            })
        );
        assert_eq!(decode_bar(&e.bars, 1), Ok(Bar::Upper));
        assert_eq!(e.cap_count, 3);
        assert_eq!(e.pcie.unwrap().offset, 0x50);
        assert_eq!(e.msix.unwrap().table, 0x2000);
        assert_eq!(e.pm.unwrap().control, 0);
        let text = format!("{e}");
        assert!(text.contains("mem64@0x920000000"));
        assert!(text.contains("vectors=16"));
        assert!(text.lines().all(|l| l.len() < 240), "{text}");
        assert_eq!(
            f.reads,
            [0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 44, 52, 64, 68, 80, 92, 96, 112, 116, 120]
        );
    }
    #[test]
    fn absent_retry_and_unexpected_headers_stop_before_bars() {
        for (value, error) in [
            (u32::MAX, Error::Absent),
            (0, Error::Absent),
            (0xffff_0001, Error::Retry),
        ] {
            let mut f = Fake::nvme();
            f.set(0, value);
            assert_eq!(probe(&snapshot(), &mut f), Err(error));
            assert_eq!(f.reads, [0]);
        }
        for (offset, value, error) in [(12, 1 << 16, Error::Header), (8, 0x0200_0000, Error::Class)]
        {
            let mut f = Fake::nvme();
            f.set(offset, value);
            assert_eq!(probe(&snapshot(), &mut f), Err(error));
            assert!(!f.reads.contains(&16));
        }
    }
    #[test]
    fn malformed_capability_lists_are_bounded_and_unknown_caps_only_read_their_header() {
        for pointer in [0x3c, 0x41, 0xff] {
            let mut f = Fake::nvme();
            f.set(0x34, pointer);
            assert_eq!(probe(&snapshot(), &mut f), Err(Error::Capabilities));
            assert!(f.reads.iter().all(|&offset| offset < 0x40));
        }
        let mut f = Fake::nvme();
        f.set(0x70, 0x000f_4011);
        assert_eq!(probe(&snapshot(), &mut f), Err(Error::Capabilities));
        assert_eq!(f.reads.iter().filter(|&&o| o == 0x40).count(), 1);
        let mut f = Fake::nvme();
        f.set(0x70, 0x000f_f811);
        f.set(0xf8, 0x11);
        assert_eq!(probe(&snapshot(), &mut f), Err(Error::Capabilities));
        assert!(!f.reads.contains(&0xfc));
        let mut f = Fake::nvme();
        f.set(0x34, 0x30);
        assert_eq!(probe(&snapshot(), &mut f), Err(Error::Capabilities));
        let mut f = Fake::nvme();
        f.set(0x34, 0x80);
        f.set(0x80, 0x4009);
        assert_eq!(probe(&snapshot(), &mut f).unwrap().cap_count, 4);
        assert!(!f.reads.contains(&0x84));
        let mut f = Fake::nvme();
        f.set(0x40, 0x0003_4401); // next points into this PM capability's payload
        assert_eq!(probe(&snapshot(), &mut f), Err(Error::Capabilities));
        assert_eq!(f.reads.iter().filter(|&&o| o == 0x44).count(), 1);
    }
    #[test]
    fn invalid_bar_references_and_link_mismatch_are_refused() {
        for bir in [1, 6, 7] {
            let mut f = Fake::nvme();
            f.set(0x74, 0x2000 | bir);
            assert_eq!(probe(&snapshot(), &mut f), Err(Error::Bar));
        }
        let mut f = Fake::nvme();
        f.set(0x24, 4);
        assert_eq!(probe(&snapshot(), &mut f), Err(Error::Bar));
        let mut f = Fake::nvme();
        f.set(0x60, 0x23_0000);
        assert_eq!(probe(&snapshot(), &mut f), Err(Error::LinkMismatch));
    }
}
