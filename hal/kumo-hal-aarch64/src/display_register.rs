//j614

//! DESIGN/022 D1d-a: protect all VOP/MMU pages, delegate only reviewed D0/D1 fields.
//! Register facts: RK3588 TRM Part 2 ch. 7. Sequence/presentation policy stays in drv-fb.

use crate::{display_unit, power_unit::PowerIo};
use core::sync::atomic::{AtomicU64, Ordering};
use kumo_abi::sys::DisplayRegister as R;

const BASE: u64 = display_unit::VOP;
const PROTECTED_LEN: u64 = 0x8000;

/// Immutable reservation, including the MMU page and every bypass/reset/RID/security field.
/// It applies before discovery/minting, even with an absent or malformed DTB. — KESTREL
pub fn read_only(base: u64, len: u64) -> bool {
    len != 0
        && base
            .checked_add(len)
            .is_none_or(|end| base < BASE + PROTECTED_LEN && BASE < end)
}

pub fn window(base: u64) -> Option<u64> {
    (base == BASE).then_some(0x7000)
}

#[derive(Clone, Copy)]
enum Rule {
    Fields(u32),
    Command(u32),
    EnableFrame,
}

/// No offset or mask comes from the caller. Masked command registers must never be RMW'd:
/// their upper half is a write-enable mask, not persistent register state. — KESTREL
fn register(index: u32) -> Option<(u64, Rule)> {
    let (offset, rule) = match index {
        x if x == R::Vp2Clock as u32 => (0x0e0c, Rule::Fields(0x3)),
        x if x == R::InterfaceControl as u32 => (0x002c, Rule::Fields(0x0007_0000)),
        x if x == R::HorizontalTotal as u32 => (0x0e48, Rule::Fields(0x1fff_1fff)),
        x if x == R::HorizontalActive as u32 => (0x0e4c, Rule::Fields(0x1fff_1fff)),
        x if x == R::VerticalTotal as u32 => (0x0e50, Rule::Fields(0x1fff_1fff)),
        x if x == R::VerticalActive as u32 => (0x0e54, Rule::Fields(0x1fff_1fff)),
        x if x == R::RegionControl as u32 => (0x1c10, Rule::Fields(1)),
        x if x == R::RegionAddress as u32 => (0x1c14, Rule::Fields(0xffff_fffc)),
        x if x == R::RegionStride as u32 => (0x1c1c, Rule::Fields(0xffff)),
        x if x == R::RegionActive as u32 => (0x1c20, Rule::Fields(0x1fff_1fff)),
        x if x == R::RegionDisplay as u32 => (0x1c24, Rule::Fields(0x1fff_1fff)),
        x if x == R::RegionOffset as u32 => (0x1c28, Rule::Fields(0x1fff_1fff)),
        x if x == R::InterfaceEnable as u32 => (0x0028, Rule::Fields(0x0003_000c)),
        x if x == R::Vp2Control as u32 => (0x0e00, Rule::Fields(0x8800_000f)),
        x if x == R::Commit as u32 => (0x0000, Rule::Command(0x0004_8004)),
        x if x == R::FrameInterruptEnable as u32 => (0x00c0, Rule::EnableFrame),
        x if x == R::FrameInterruptClear as u32 => (0x00c4, Rule::Command(0x0001_0001)),
        _ => return None,
    };
    Some((BASE + offset, rule))
}

pub fn valid(base: u64, index: u32, value: u32) -> bool {
    if window(base).is_none() {
        return false;
    }
    match register(index).map(|(_, rule)| rule) {
        Some(Rule::Fields(mask)) => value & !mask == 0,
        Some(Rule::Command(word)) => value == word,
        Some(Rule::EnableFrame) => matches!(value, 0x0001_0000 | 0x0001_0001),
        None => false,
    }
}

pub trait RegisterIo: PowerIo {
    fn barrier(&mut self);
}

/// Cache only successful discovery of the kernel's immutable, retained boot DTB. Rewalking
/// 107 KB of device-mapped DTB for each IRQ clear loses frame starts on metal. Register
/// values and live power/idle guards are still checked for every write. — KESTREL
pub struct BootRegisters {
    dtb: AtomicU64,
}

impl BootRegisters {
    pub const fn new() -> Self {
        Self {
            dtb: AtomicU64::new(0),
        }
    }

    pub fn write<'a>(
        &self,
        dtb: u64,
        base: u64,
        index: u32,
        value: u32,
        bytes: impl FnOnce() -> Option<&'a [u8]>,
        io: &mut impl RegisterIo,
    ) -> Option<bool> {
        if !valid(base, index, value) {
            return Some(false);
        }
        if dtb == 0 {
            return None;
        }
        if self.dtb.load(Ordering::Acquire) == dtb {
            return Some(write_unit(&display_unit::UNITS[0], index, value, io));
        }
        let result = write(bytes()?, base, index, value, io);
        if result.is_some() {
            self.dtb.store(dtb, Ordering::Release);
        }
        result
    }
}

/// None: no live unit. Some(false): guard failed. Invalid words perform no I/O at all.
pub fn write(
    bytes: &[u8],
    base: u64,
    index: u32,
    value: u32,
    io: &mut impl RegisterIo,
) -> Option<bool> {
    if !valid(base, index, value) {
        return Some(false);
    }
    let unit = display_unit::live_unit(bytes, base)?;
    Some(write_unit(unit, index, value, io))
}

fn write_unit(unit: &display_unit::Unit, index: u32, value: u32, io: &mut impl RegisterIo) -> bool {
    if !valid(unit.base, index, value) || !unit.ready(io) {
        return false;
    }
    let Some((address, rule)) = register(index) else {
        return false;
    };
    let word = match rule {
        Rule::Fields(mask) => (io.read(address) & !mask) | value,
        _ => value,
    };
    io.write(address, word);
    io.barrier();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::{collections::BTreeMap, vec::Vec};
    const DTB: &[u8] = include_bytes!("../../../rk3588-orangepi-5-plus.dtb");
    #[derive(Debug, PartialEq)]
    enum Event {
        Read(u64),
        Write(u64, u32),
        Barrier,
    }
    #[derive(Default)]
    struct Io {
        regs: BTreeMap<u64, u32>,
        events: Vec<Event>,
    }
    impl PowerIo for Io {
        fn read(&mut self, p: u64) -> u32 {
            self.events.push(Event::Read(p));
            *self.regs.get(&p).unwrap_or(&0)
        }
        fn write(&mut self, p: u64, v: u32) {
            self.events.push(Event::Write(p, v));
        }
        fn now_ns(&mut self) -> u64 {
            0
        }
    }
    impl RegisterIo for Io {
        fn barrier(&mut self) {
            self.events.push(Event::Barrier);
        }
    }

    #[test]
    fn cached_discovery_keeps_each_write_guarded_and_rejects_a_different_boot_tree() {
        let cache = BootRegisters::new();
        let mut io = Io::default();
        let mut discoveries = 0;
        for _ in 0..3 {
            assert_eq!(
                cache.write(
                    8,
                    BASE,
                    0,
                    2,
                    || {
                        discoveries += 1;
                        Some(DTB)
                    },
                    &mut io
                ),
                Some(true)
            );
        }
        assert_eq!(discoveries, 1);
        assert_eq!(
            io.events
                .iter()
                .filter(|e| matches!(e, Event::Read(0xfd8d8150)))
                .count(),
            3
        );
        io.events.clear();
        io.regs.insert(0xfd8d8150, 1 << 17);
        assert_eq!(
            cache.write(8, BASE, 0, 2, || panic!("cached tree rewalked"), &mut io),
            Some(false)
        );
        assert_eq!(io.events, [Event::Read(0xfd8d8150)]);
        io.events.clear();
        assert_eq!(cache.write(16, BASE, 0, 2, || Some(&[]), &mut io), None);
        assert_eq!(
            cache.write(0, BASE, 0, 2, || panic!("absent tree read"), &mut io),
            None
        );
        assert_eq!(
            cache.write(
                8,
                BASE,
                0x24,
                0,
                || panic!("bad selector read tree"),
                &mut io
            ),
            Some(false)
        );
        assert!(io.events.is_empty());
        // Failed discovery is never admitted to the cache. — KESTREL
        assert_eq!(
            cache.write(16, BASE, 0, 2, || Some(DTB), &mut io),
            Some(false)
        );
        assert_eq!(io.events, [Event::Read(0xfd8d8150)]);
    }

    #[test]
    fn reserves_full_aperture_and_partial_aliases_without_discovery() {
        for (base, len) in [
            (BASE, 0x8000),
            (BASE - 1, 2),
            (BASE + 0x7fff, 1),
            (BASE + 0x7000, 0x1000),
            (0, u64::MAX),
            (u64::MAX, 2),
        ] {
            assert!(read_only(base, len));
        }
        for (base, len) in [(BASE - 0x1000, 0x1000), (BASE + 0x8000, 1), (BASE, 0)] {
            assert!(!read_only(base, len));
        }
    }

    #[test]
    fn table_preserves_other_fields_and_orders_each_write() {
        let words = [
            2, 0x60000, 0x07080050, 0x00b006f0, 0x03e80003, 0x006303e7, 1, 0xed0c0000, 1600,
            0x0383063f, 0x0383063f, 0, 0x20008, 0x8800000f, 0x48004, 0x10001, 0x10001,
        ];
        let offsets = [
            0xe0c, 0x2c, 0xe48, 0xe4c, 0xe50, 0xe54, 0x1c10, 0x1c14, 0x1c1c, 0x1c20, 0x1c24,
            0x1c28, 0x28, 0xe00, 0, 0xc0, 0xc4,
        ];
        for (index, value) in words.into_iter().enumerate() {
            let mut io = Io::default();
            let (p, rule) = register(index as u32).unwrap();
            assert_eq!(p, BASE + offsets[index]);
            io.regs.insert(p, u32::MAX);
            assert_eq!(write(DTB, BASE, index as u32, value, &mut io), Some(true));
            let expected = match rule {
                Rule::Fields(mask) => !mask | value,
                _ => value,
            };
            assert_eq!(
                &io.events[io.events.len() - 2..],
                &[Event::Write(p, expected), Event::Barrier]
            );
            if !matches!(rule, Rule::Fields(_)) {
                assert!(!io.events.contains(&Event::Read(p)));
            }
            for bit in 0..32 {
                let bad = value ^ (1 << bit);
                if valid(BASE, index as u32, bad) {
                    continue;
                }
                io.events.clear();
                assert_eq!(write(DTB, BASE, index as u32, bad, &mut io), Some(false));
                assert!(io.events.is_empty());
            }
        }
        assert!(valid(BASE, R::FrameInterruptEnable as u32, 0x10000));
    }

    #[test]
    fn offsets_bypass_reset_rid_security_and_mmu_sources_are_unreachable() {
        // Every slot is pinned above. Physical offsets cannot select another register. — KESTREL
        for index in [17, 0x24, 0x1c08, 0x1100, 0x7000, u32::MAX] {
            let mut io = Io::default();
            assert_eq!(write(DTB, BASE, index, u32::MAX, &mut io), Some(false));
            assert!(io.events.is_empty());
        }
        let mut io = Io::default();
        assert_eq!(write(DTB, display_unit::HDMI, 0, 0, &mut io), Some(false));
        assert!(io.events.is_empty());
    }

    #[test]
    fn discovery_and_each_power_or_idle_guard_precede_vop_access() {
        for bytes in [&[][..], &DTB[..40]] {
            let mut io = Io::default();
            assert_eq!(write(bytes, BASE, 0, 2, &mut io), None);
            assert!(io.events.is_empty());
        }
        // Wrong VOP/root compatible fails without PMU/VOP access. — KESTREL
        for (needle, replacement) in [
            (
                b"rockchip,rk3588-vop\0".as_slice(),
                b"rockchip,xxxxxx-vop\0".as_slice(),
            ),
            (
                b"rockchip,rk3588\0".as_slice(),
                b"rockchip,xxxxxx\0".as_slice(),
            ),
        ] {
            let mut bytes = DTB.to_vec();
            let start = bytes
                .windows(needle.len())
                .position(|w| w == needle)
                .unwrap();
            bytes[start..start + needle.len()].copy_from_slice(replacement);
            let mut io = Io::default();
            assert_eq!(write(&bytes, BASE, 0, 2, &mut io), None);
            assert!(io.events.is_empty());
        }
        let fdt = kumo_fdt::Fdt::new(DTB).unwrap();
        let vop = fdt
            .root()
            .unwrap()
            .children()
            .find(|n| {
                n.property(b"compatible")
                    .is_some_and(|c| kumo_fdt::string_list_contains(c, b"rockchip,rk3588-vop"))
            })
            .unwrap();
        let status = vop.property(b"status").unwrap();
        let start = status.as_ptr() as usize - DTB.as_ptr() as usize;
        let mut disabled = DTB.to_vec();
        disabled[start] = b'x';
        let mut io = Io::default();
        assert_eq!(write(&disabled, BASE, 0, 2, &mut io), None);
        assert!(io.events.is_empty());
        for (p, value) in [
            (0xfd8d8150, 1 << 17),
            (0xfd8d810c, 1 << 13),
            (0xfd8d810c, 1 << 14),
            (0xfd8d8118, 1 << 13),
            (0xfd8d8118, 1 << 14),
            (0xfd8d8120, 1 << 13),
            (0xfd8d8120, 1 << 14),
        ] {
            let mut io = Io::default();
            io.regs.insert(p, value);
            assert_eq!(write(DTB, BASE, 0, 2, &mut io), Some(false));
            assert!(io
                .events
                .iter()
                .all(|e| matches!(e, Event::Read(p) if *p < BASE || *p >= BASE+PROTECTED_LEN)));
        }
    }
}
