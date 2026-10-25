//j609

//! DESIGN/020 D0: DTB-gated display apertures and single-line resets, never a display sequence.
//! All SoC register facts come from RK3588 TRM Part 1 ch. 2/7 and RESEARCH/014. The holder
//! supplies only a device aperture, a line index and a boolean; no caller-selected mask or
//! address reaches the CRU. PHY0's reference clock, shared with USB2, is untouched. — KESTREL

use crate::power_unit::PowerIo;
use kumo_fdt::{be32, status_enabled, string_list_contains, Fdt, Node};

pub const VOP: u64 = 0xfdd9_0000;
pub const HDMI: u64 = 0xfde8_0000;
pub const PHY: u64 = 0xfed6_0000;
pub const PHY_GRF: u64 = 0xfd5e_0000;
const PWR_STS: u64 = 0xfd8d_8150;
const IDLE_REQ0: u64 = 0xfd8d_810c;
const IDLE_REQ1: u64 = 0xfd8d_8110;
const IDLE_ACK: u64 = 0xfd8d_8118;
const IDLE_STS: u64 = 0xfd8d_8120;

#[derive(Clone, Copy)]
pub struct Reset {
    name: &'static [u8],
    binding: u32,
    pub register: u64,
    pub bit: u32,
}

#[derive(Clone, Copy)]
pub struct Unit {
    pub base: u64,
    pub len: u64,
    compatible: &'static [u8],
    pub resets: &'static [Reset],
    domain: u32,
    idle_register: u64,
    idle_request: u32,
    idle_state: u32,
}

pub const PHY_RESETS: [Reset; 4] = [
    Reset {
        name: b"apb",
        binding: 538,
        register: 0xfd7c_0b20,
        bit: 5,
    },
    Reset {
        name: b"init",
        binding: 611,
        register: 0xfd7f_0a0c,
        bit: 11,
    },
    Reset {
        name: b"cmn",
        binding: 612,
        register: 0xfd7f_0a0c,
        bit: 12,
    },
    Reset {
        name: b"lane",
        binding: 613,
        register: 0xfd7f_0a0c,
        bit: 13,
    },
];
const HDMI_RESETS: [Reset; 2] = [
    Reset {
        name: b"ref",
        binding: 462,
        register: 0xfd7c_0af4,
        bit: 0,
    },
    Reset {
        name: b"hdp",
        binding: 560,
        register: 0xfd7c_0b24,
        bit: 12,
    },
];
pub const UNITS: [Unit; 3] = [
    Unit {
        base: VOP,
        len: 0x7000,
        compatible: b"rockchip,rk3588-vop",
        resets: &[],
        domain: 1 << 17,
        idle_register: IDLE_REQ0,
        idle_request: (1 << 13) | (1 << 14),
        idle_state: (1 << 13) | (1 << 14),
    },
    Unit {
        base: HDMI,
        len: 0x1000,
        compatible: b"rockchip,rk3588-dw-hdmi-qp",
        resets: &HDMI_RESETS,
        domain: 1 << 19,
        idle_register: IDLE_REQ1,
        idle_request: 1,
        idle_state: 1 << 16,
    },
    Unit {
        base: PHY,
        len: 0x2000,
        compatible: b"rockchip,rk3588-hdptx-phy",
        resets: &PHY_RESETS,
        domain: 1 << 19,
        idle_register: IDLE_REQ1,
        idle_request: 1,
        idle_state: 1 << 16,
    },
];

fn reg(node: Node<'_>) -> Option<(u64, u64)> {
    let bytes = node.property(b"reg")?;
    Some((
        (u64::from(be32(bytes, 0)?) << 32) | u64::from(be32(bytes, 4)?),
        (u64::from(be32(bytes, 8)?) << 32) | u64::from(be32(bytes, 12)?),
    ))
}

fn reset_bindings(fdt: Fdt<'_>, node: Node<'_>, unit: &Unit) -> bool {
    if unit.resets.is_empty() {
        return true;
    }
    let Some(names) = node.property(b"reset-names") else {
        return false;
    };
    let Some(cells) = node.property(b"resets") else {
        return false;
    };
    if names.last() != Some(&0) || cells.len() % 8 != 0 {
        return false;
    }
    let count = names[..names.len() - 1].split(|&b| b == 0).count();
    if count * 8 != cells.len() {
        return false;
    }
    unit.resets.iter().all(|line| {
        let mut found = false;
        for (i, name) in names[..names.len() - 1].split(|&b| b == 0).enumerate() {
            if name != line.name {
                continue;
            }
            if found || be32(cells, i * 8 + 4) != Some(line.binding) {
                return false;
            }
            let Some(provider) = be32(cells, i * 8).and_then(|p| fdt.find_by_phandle(p)) else {
                return false;
            };
            if !status_enabled(provider)
                || !provider
                    .property(b"compatible")
                    .is_some_and(|c| string_list_contains(c, b"rockchip,rk3588-cru"))
                || provider.property(b"#reset-cells").and_then(|b| be32(b, 0)) != Some(1)
                || reg(provider).map(|r| r.0) != Some(0xfd7c_0000)
            {
                return false;
            }
            found = true;
        }
        found
    })
}

pub fn live_unit(bytes: &[u8], base: u64) -> Option<&'static Unit> {
    let unit = UNITS.iter().find(|u| u.base == base)?;
    let fdt = Fdt::new(bytes)?;
    let root = fdt.root()?;
    if !root
        .property(b"compatible")
        .is_some_and(|c| string_list_contains(c, b"rockchip,rk3588"))
        || root.property(b"#address-cells").and_then(|b| be32(b, 0)) != Some(2)
        || root.property(b"#size-cells").and_then(|b| be32(b, 0)) != Some(2)
    {
        return None;
    }
    let node = root.children().find(|&n| {
        status_enabled(n)
            && n.property(b"compatible")
                .is_some_and(|c| string_list_contains(c, unit.compatible))
            && reg(n)
                .is_some_and(|(b, l)| b == base && l >= if base == VOP { 0x4200 } else { unit.len })
    })?;
    reset_bindings(fdt, node, unit).then_some(unit)
}

impl Unit {
    pub fn ready(&self, io: &mut impl PowerIo) -> bool {
        io.read(PWR_STS) & self.domain == 0
            && io.read(self.idle_register) & self.idle_request == 0
            && io.read(IDLE_ACK) & self.idle_state == 0
            && io.read(IDLE_STS) & self.idle_state == 0
    }
}

pub fn reset_set(
    bytes: &[u8],
    base: u64,
    line: u32,
    asserted: bool,
    io: &mut impl PowerIo,
) -> bool {
    let Some(unit) = live_unit(bytes, base) else {
        return false;
    };
    let Some(reset) = unit.resets.get(line as usize) else {
        return false;
    };
    if !unit.ready(io) {
        return false;
    }
    io.write(
        reset.register,
        (1 << (reset.bit + 16)) | (u32::from(asserted) << reset.bit),
    );
    true
}

fn overlaps(base: u64, len: u64, start: u64, size: u64) -> bool {
    len != 0
        && base
            .checked_add(len)
            .is_none_or(|end| base < start + size && start < end)
}

pub fn overlaps_display(base: u64, len: u64) -> bool {
    UNITS.iter().any(|u| {
        overlaps(
            base,
            len,
            u.base,
            if u.base == VOP { 0x8000 } else { u.len },
        )
    }) || overlaps(base, len, PHY_GRF, 0x1000)
}

pub fn aperture_blocked(bytes: &[u8], base: u64, len: u64, io: &mut impl PowerIo) -> bool {
    for unit in &UNITS {
        if overlaps(
            base,
            len,
            unit.base,
            if unit.base == VOP { 0x8000 } else { unit.len },
        ) && live_unit(bytes, unit.base).is_some()
            && !unit.ready(io)
        {
            return true;
        }
    }
    if overlaps(base, len, PHY_GRF, 0x1000) {
        if let Some(unit) = live_unit(bytes, PHY) {
            return !unit.ready(io);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::{collections::BTreeMap, vec::Vec};
    const DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    #[derive(Default)]
    struct Io {
        regs: BTreeMap<u64, u32>,
        writes: Vec<(u64, u32)>,
        reads: Vec<u64>,
    }
    impl PowerIo for Io {
        fn read(&mut self, p: u64) -> u32 {
            self.reads.push(p);
            *self.regs.get(&p).unwrap_or(&0)
        }
        fn write(&mut self, p: u64, v: u32) {
            self.writes.push((p, v));
            let old = *self.regs.get(&p).unwrap_or(&0);
            self.regs.insert(p, (old & !(v >> 16)) | (v & (v >> 16)));
        }
        fn now_ns(&mut self) -> u64 {
            0
        }
    }
    fn change(base: u64, property: &[u8], offset: usize, value: &[u8]) -> Vec<u8> {
        let fdt = Fdt::new(DTB).unwrap();
        let node = fdt
            .root()
            .unwrap()
            .children()
            .find(|&n| reg(n).is_some_and(|r| r.0 == base))
            .unwrap();
        let bytes = node.property(property).unwrap();
        let start = bytes.as_ptr() as usize - DTB.as_ptr() as usize + offset;
        let mut result = DTB.to_vec();
        result[start..start + value.len()].copy_from_slice(value);
        result
    }
    #[test]
    fn live_table_requires_enabled_nodes_named_reset_ids_and_the_actual_cru_provider() {
        for unit in &UNITS {
            assert_eq!(
                live_unit(DTB, unit.base).unwrap().resets.len(),
                unit.resets.len()
            );
        }
        for (index, id) in [(1, 538u32), (2, 611), (3, 612), (4, 613)] {
            let bad = change(PHY, b"resets", index * 8 + 4, &(id + 1).to_be_bytes());
            assert!(live_unit(&bad, PHY).is_none());
        }
        for unit in &UNITS {
            assert!(live_unit(&change(unit.base, b"status", 0, b"nope"), unit.base).is_none());
        }
        assert!(live_unit(&change(PHY, b"resets", 8, &0xffffu32.to_be_bytes()), PHY).is_none());
        assert!(live_unit(&change(PHY, b"reset-names", 4, b"bad"), PHY).is_none());
        assert!(live_unit(&[], PHY).is_none());
        assert!(live_unit(DTB, PHY + 0x2000).is_none());
    }
    #[test]
    fn all_six_reset_lines_write_exactly_one_masked_bit_and_preserve_neighbors() {
        for unit in UNITS.iter().filter(|u| !u.resets.is_empty()) {
            for (line, reset) in unit.resets.iter().enumerate() {
                for asserted in [false, true] {
                    let mut io = Io::default();
                    io.regs.insert(reset.register, 0xa55a);
                    assert!(reset_set(DTB, unit.base, line as u32, asserted, &mut io));
                    let value = (1 << (reset.bit + 16)) | (u32::from(asserted) << reset.bit);
                    assert_eq!(io.writes, [(reset.register, value)]);
                    assert_eq!(
                        io.regs[&reset.register] & !(1 << reset.bit),
                        0xa55a & !(1 << reset.bit)
                    );
                }
            }
        }
        assert_eq!(PHY_RESETS[0].bit, 5);
        assert_eq!(PHY_RESETS[0].register, 0xfd7c_0b20);
    }
    #[test]
    fn invalid_lines_unknown_trees_or_down_idle_domains_never_write() {
        for base in [VOP, HDMI, PHY, PHY + 0x2000] {
            for line in [4, u32::MAX] {
                let mut io = Io::default();
                assert!(!reset_set(DTB, base, line, true, &mut io));
                assert!(io.writes.is_empty());
            }
        }
        for (p, bit) in [
            (PWR_STS, 1 << 19),
            (IDLE_REQ1, 1),
            (IDLE_ACK, 1 << 16),
            (IDLE_STS, 1 << 16),
        ] {
            let mut io = Io::default();
            io.regs.insert(p, bit);
            assert!(!reset_set(DTB, PHY, 0, true, &mut io));
            assert!(io.writes.is_empty());
        }
        let mut io = Io::default();
        assert!(!reset_set(&[], PHY, 0, true, &mut io));
        assert!(io.reads.is_empty());
    }
    #[test]
    fn mint_guard_covers_vop_mmus_hdmi_phy_and_phy_grf_without_touching_other_apertures() {
        for (base, domain) in [
            (VOP, 17),
            (VOP + 0x7fff, 17),
            (HDMI, 19),
            (PHY + 0x1fff, 19),
            (PHY_GRF, 19),
        ] {
            let mut io = Io::default();
            io.regs.insert(PWR_STS, 1 << domain);
            assert!(aperture_blocked(DTB, base, 1, &mut io));
        }
        for bit in [13, 14] {
            for p in [IDLE_REQ0, IDLE_ACK, IDLE_STS] {
                let mut io = Io::default();
                io.regs.insert(p, 1 << bit);
                assert!(aperture_blocked(DTB, VOP, 0x1000, &mut io));
            }
        }
        let mut io = Io::default();
        assert!(!aperture_blocked(DTB, 0xfb00_0000, 0x200000, &mut io));
        assert!(io.reads.is_empty());
        assert!(!aperture_blocked(DTB, PHY, 0x2000, &mut Io::default()));
    }
}
