//j804
//! One-purpose disposable NV7 media preparation, absent from default images.
//! This is a quick format, not secure erase or an NVMe driver write path. Only
//! the identified, explicitly disposable Samsung namespace is eligible. A prior
//! matching format is never repeated, preserving later NV9 files. — TIMBERDOODLE
use crate::nvme::{Controller, Namespace};
use kumo_sha256::digest;

pub const BLOCKS: u64 = 976_773_168;
pub const START: u64 = 2048;
pub const LENGTH: u64 = 524_288;
pub const DATA: u64 = 8224;
pub const HELLO: &[u8] = b"KUMO NV7 NVMe FAT32 proof.\r\n";
pub const LABEL: &[u8; 11] = b"KUMONVME   ";
const DISK_GUID: [u8; 16] = [
    0xc8, 0xe4, 0xc0, 0x50, 0xc9, 0xba, 0x41, 0x43, 0x91, 0x4b, 0x03, 0x37, 0x94, 0x86, 0x08, 0x04,
];
const PART_GUID: [u8; 16] = [
    0xd3, 0x6f, 0x1b, 0x42, 0x42, 0xed, 0x0b, 0x44, 0xa6, 0x19, 0xb8, 0x39, 0x49, 0xb6, 0x08, 0x04,
];
const BASIC: [u8; 16] = [
    0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7,
];
// Volume contents first; publish primary VBR last. Every phase is flushed and
// read back before the next. Changed GPT markers refuse retry. — TIMBERDOODLE
pub const PHASES: [(u64, u64); 8] = [
    (START + 1, START + 5),
    (START + 7, START + DATA + 1),
    (BLOCKS - 33, BLOCKS - 1),
    (2, 33),
    (0, 0),
    (1, 1),
    (START + 6, START + 6),
    (START, START),
];
pub const AUDIT_RANGES: [(u64, u64); 3] = [(0, 33), (BLOCKS - 33, BLOCKS - 1), (START, START + 1)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Target,
    Layout,
    Io,
    Verify,
}
pub trait Disk {
    fn read(&mut self, lba: u64, sector: &mut [u8; 512]) -> Result<(), Error>;
    fn write(&mut self, lba: u64, sector: &[u8; 512]) -> Result<(), Error>;
    fn flush(&mut self) -> Result<(), Error>;
}
// Only inspect can authorize production writes. — TIMBERDOODLE
pub struct Original(());
pub enum Inspection {
    Original(Original),
    Prepared,
}
pub fn target(c: &Controller, n: &Namespace) -> bool {
    c.vid == 0x144d
        && c.ssvid == 0x144d
        && c.model() == "Samsung SSD 970 EVO Plus 500GB"
        && c.serial() == "S58SNM0R302625Z"
        && c.firmware() == "2B2QEXM7"
        && c.namespaces == 1
        && n.id == 1
        && n.size == BLOCKS
        && n.capacity == BLOCKS
        && n.lba == 512
        && n.bytes == BLOCKS * 512
        && n.metadata == 0
        && n.protection == 0
}
pub fn inspect<D: Disk>(d: &mut D, c: &Controller, n: &Namespace) -> Result<Inspection, Error> {
    if !target(c, n) {
        return Err(Error::Target);
    }
    let mut b = [0; 512];
    let mut prepared = true;
    // Do not inspect mutable FAT/root/FSInfo for idempotence: an already prepared
    // disk can contain later files, and must never be reformatted. — TIMBERDOODLE
    for (first, last) in [
        (0, 33),
        (BLOCKS - 33, BLOCKS - 1),
        (START, START),
        (START + 6, START + 6),
    ] {
        for lba in first..=last {
            d.read(lba, &mut b)?;
            if b != sector(lba).ok_or(Error::Layout)? {
                prepared = false;
                break;
            }
        }
        if !prepared {
            break;
        }
    }
    if prepared {
        return Ok(Inspection::Prepared);
    }
    let hashes = [
        "921fa0148f232b7590918269041d2c728a53dcedb72d95d2f5c9a8027a7e54e5",
        "3723a838a20457b0056099217b80acdfdde5a7cb1be9d0f5503e689d77a62ada",
        "f86d4b340a250b8bb8d0e0c6305d3926f286d060d271eaacafe0a5865be08cfe",
    ];
    for (lba, hash) in [0, 1, BLOCKS - 1].into_iter().zip(hashes) {
        d.read(lba, &mut b)?;
        let sum = digest(&b);
        if !sum.iter().enumerate().all(|(i, x)| {
            let hex = b"0123456789abcdef";
            hash.as_bytes()[i * 2] == hex[(x >> 4) as usize]
                && hash.as_bytes()[i * 2 + 1] == hex[(x & 15) as usize]
        }) {
            return Err(Error::Layout);
        }
    }
    Ok(Inspection::Original(Original(())))
}
pub fn apply<D: Disk>(
    d: &mut D,
    _original: Original,
    mut phase: impl FnMut(usize),
) -> Result<u64, Error> {
    let mut count = 0;
    let mut read = [0; 512];
    for (i, (first, last)) in PHASES.into_iter().enumerate() {
        for lba in first..=last {
            d.write(lba, &sector(lba).ok_or(Error::Layout)?)?;
            count += 1;
        }
        d.flush()?;
        for lba in first..=last {
            d.read(lba, &mut read)?;
            if read != sector(lba).ok_or(Error::Layout)? {
                return Err(Error::Verify);
            }
        }
        phase(i + 1);
    }
    Ok(count)
}
fn u16_at(b: &mut [u8], at: usize, x: u16) {
    b[at..at + 2].copy_from_slice(&x.to_le_bytes());
}
fn u32_at(b: &mut [u8], at: usize, x: u32) {
    b[at..at + 4].copy_from_slice(&x.to_le_bytes());
}
fn u64_at(b: &mut [u8], at: usize, x: u64) {
    b[at..at + 8].copy_from_slice(&x.to_le_bytes());
}
fn crc(bytes: impl Iterator<Item = u8>) -> u32 {
    let mut c = !0u32;
    for b in bytes {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = (c >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(c & 1));
        }
    }
    !c
}
fn table(lba: u64) -> [u8; 512] {
    let mut b = [0; 512];
    if lba != 0 {
        return b;
    }
    b[..16].copy_from_slice(&BASIC);
    b[16..32].copy_from_slice(&PART_GUID);
    u64_at(&mut b, 32, START);
    u64_at(&mut b, 40, START + LENGTH - 1);
    for (i, x) in "KUMO NVMe test".encode_utf16().enumerate() {
        u16_at(&mut b, 56 + i * 2, x);
    }
    b
}
fn header(backup: bool) -> [u8; 512] {
    let mut b = [0; 512];
    b[..8].copy_from_slice(b"EFI PART");
    u32_at(&mut b, 8, 0x10000);
    u32_at(&mut b, 12, 92);
    u64_at(&mut b, 24, if backup { BLOCKS - 1 } else { 1 });
    u64_at(&mut b, 32, if backup { 1 } else { BLOCKS - 1 });
    u64_at(&mut b, 40, 34);
    u64_at(&mut b, 48, BLOCKS - 34);
    b[56..72].copy_from_slice(&DISK_GUID);
    u64_at(&mut b, 72, if backup { BLOCKS - 33 } else { 2 });
    u32_at(&mut b, 80, 128);
    u32_at(&mut b, 84, 128);
    let sum = crc((0..32).flat_map(|i| table(i).into_iter()));
    u32_at(&mut b, 88, sum);
    let sum = crc(b[..92].iter().copied());
    u32_at(&mut b, 16, sum);
    b
}
/// Only sectors in PHASES can be emitted; no caller-selected layout or size.
pub fn sector(lba: u64) -> Option<[u8; 512]> {
    let mut b = [0; 512];
    match lba {
        0 => {
            b[447..450].copy_from_slice(&[0, 2, 0]);
            b[450] = 0xee;
            b[451..454].fill(255);
            u32_at(&mut b, 454, 1);
            u32_at(&mut b, 458, (BLOCKS - 1) as u32);
            b[510..].copy_from_slice(&[0x55, 0xaa]);
        }
        1 => return Some(header(false)),
        x if x == BLOCKS - 1 => return Some(header(true)),
        2..=33 => return Some(table(lba - 2)),
        x if (BLOCKS - 33..BLOCKS - 1).contains(&x) => return Some(table(x - (BLOCKS - 33))),
        x if (START..=START + DATA + 1).contains(&x) => match x - START {
            0 | 6 => {
                b[..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
                b[3..11].copy_from_slice(b"KUMO0804");
                u16_at(&mut b, 11, 512);
                b[13] = 1;
                u16_at(&mut b, 14, 32);
                b[16] = 2;
                b[21] = 0xf8;
                u16_at(&mut b, 24, 63);
                u16_at(&mut b, 26, 255);
                u32_at(&mut b, 28, START as u32);
                u32_at(&mut b, 32, LENGTH as u32);
                u32_at(&mut b, 36, 4096);
                u32_at(&mut b, 44, 2);
                u16_at(&mut b, 48, 1);
                u16_at(&mut b, 50, 6);
                b[64] = 0x80;
                b[66] = 0x29;
                u32_at(&mut b, 67, 0x4b4d0804);
                b[71..82].copy_from_slice(LABEL);
                b[82..90].copy_from_slice(b"FAT32   ");
                b[510..].copy_from_slice(&[0x55, 0xaa]);
            }
            1 | 7 => {
                u32_at(&mut b, 0, 0x41615252);
                u32_at(&mut b, 484, 0x61417272);
                u32_at(&mut b, 488, (LENGTH - DATA - 2) as u32);
                u32_at(&mut b, 492, 4);
                u32_at(&mut b, 508, 0xaa550000);
            }
            32 | 4128 => {
                for (i, x) in [0x0ffffff8, 0x0fffffff, 0x0fffffff, 0x0fffffff]
                    .into_iter()
                    .enumerate()
                {
                    u32_at(&mut b, i * 4, x);
                }
            }
            DATA => {
                b[..11].copy_from_slice(LABEL);
                b[11] = 8;
                b[32..43].copy_from_slice(b"HELLO   TXT");
                b[43] = 0x20;
                for off in [48, 50, 56] {
                    u16_at(&mut b, off, ((2026 - 1980) << 9) | (11 << 5) | 5);
                }
                u16_at(&mut b, 58, 3);
                u32_at(&mut b, 60, HELLO.len() as u32);
            }
            x if x == DATA + 1 => b[..HELLO.len()].copy_from_slice(HELLO),
            _ => {}
        },
        _ => return None,
    }
    Some(b)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{collections::BTreeMap, vec::Vec};
    #[derive(Default)]
    struct Memory {
        blocks: BTreeMap<u64, [u8; 512]>,
        ops: Vec<(u8, u64)>,
        fail: Option<usize>,
        corrupt: bool,
    }
    impl Memory {
        fn op(&mut self, kind: u8, lba: u64) -> Result<(), Error> {
            self.ops.push((kind, lba));
            if self.fail == Some(self.ops.len()) {
                Err(Error::Io)
            } else {
                Ok(())
            }
        }
    }
    impl Disk for Memory {
        fn read(&mut self, lba: u64, b: &mut [u8; 512]) -> Result<(), Error> {
            self.op(0, lba)?;
            *b = self.blocks.get(&lba).copied().unwrap_or([0; 512]);
            if self.corrupt {
                b[100] ^= 1;
            }
            Ok(())
        }
        fn write(&mut self, lba: u64, b: &[u8; 512]) -> Result<(), Error> {
            self.op(1, lba)?;
            self.blocks.insert(lba, *b);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), Error> {
            self.op(2, 0)
        }
    }
    fn identity() -> (Controller, Namespace) {
        let mut b = [0; 4096];
        b[..2].copy_from_slice(&0x144du16.to_le_bytes());
        b[2..4].copy_from_slice(&0x144du16.to_le_bytes());
        b[4..72].fill(b' ');
        b[4..19].copy_from_slice(b"S58SNM0R302625Z");
        b[24..54].copy_from_slice(b"Samsung SSD 970 EVO Plus 500GB");
        b[64..72].copy_from_slice(b"2B2QEXM7");
        b[516] = 1;
        let c = Controller::parse(&b).unwrap();
        b.fill(0);
        u64_at(&mut b, 0, BLOCKS);
        u64_at(&mut b, 8, BLOCKS);
        b[130] = 9;
        (c, Namespace::parse(1, &b).unwrap())
    }
    #[test]
    fn exact_target_unknown_layout_and_read_failure_never_write() {
        let (c, mut n) = identity();
        let mut d = Memory::default();
        n.id = 2;
        assert!(matches!(inspect(&mut d, &c, &n), Err(Error::Target)));
        assert!(d.ops.is_empty());
        n.id = 1;
        assert!(matches!(inspect(&mut d, &c, &n), Err(Error::Layout)));
        assert!(d.ops.iter().all(|x| x.0 == 0));
        d.ops.clear();
        d.fail = Some(1);
        assert!(matches!(inspect(&mut d, &c, &n), Err(Error::Io)));
        assert_eq!(d.ops.len(), 1);
        for field in 0..6 {
            let (_, mut n) = identity();
            match field {
                0 => n.size -= 1,
                1 => n.capacity -= 1,
                2 => n.lba = 4096,
                3 => n.bytes -= 1,
                4 => n.metadata = 8,
                _ => n.protection = 1,
            };
            assert!(!target(&c, &n));
        }
    }
    #[test]
    fn gpt_fat_and_seed_pass_independent_parsers_and_reformat_is_skipped() {
        let mut d = Memory::default();
        let count = apply(&mut d, Original(()), |_| {}).unwrap();
        assert_eq!(count, 8293);
        kumo_gpt::validate_protective_mbr(&d.blocks[&0], 512, BLOCKS).unwrap();
        let h = kumo_gpt::GptHeader::parse_primary(&d.blocks[&1], 512, BLOCKS).unwrap();
        let entries: Vec<_> = (2..34).flat_map(|i| d.blocks[&i]).collect();
        h.parse_partition_table(&entries).unwrap();
        let primary = d.blocks[&1];
        let backup = d.blocks[&(BLOCKS - 1)];
        assert_eq!(&primary[40..72], &backup[40..72]);
        assert_eq!(&primary[80..92], &backup[80..92]);
        let mut check = backup;
        check[16..20].fill(0);
        assert_eq!(
            crc(check[..92].iter().copied()).to_le_bytes(),
            backup[16..20]
        );
        assert_eq!(&backup[24..32], &(BLOCKS - 1).to_le_bytes());
        assert_eq!(&backup[32..40], &1u64.to_le_bytes());
        for i in 0..32 {
            assert_eq!(d.blocks[&(2 + i)], d.blocks[&(BLOCKS - 33 + i)]);
        }
        struct Partition<'a>(&'a mut Memory);
        impl kumo_fatfs::SectorReader for Partition<'_> {
            fn read_sector(&mut self, lba: u32, b: &mut [u8; 512]) -> bool {
                if u64::from(lba) >= LENGTH {
                    return false;
                }
                self.0.read(START + u64::from(lba), b).is_ok()
            }
        }
        let mut p = Partition(&mut d);
        let v = kumo_fatfs::FatVolume::mount(&mut p).unwrap();
        let e = v.resolve_path(&mut p, b"HELLO.TXT").unwrap();
        let mut out = [0; 64];
        let len = v.read_file(&mut p, &e, &mut out);
        assert_eq!(&out[..len], HELLO);
        assert_eq!(v.volume_label(&mut p), Some(*LABEL));
        d.blocks.get_mut(&(START + DATA)).unwrap()[100] = 42;
        d.blocks.get_mut(&(START + 1)).unwrap()[488] = 0;
        let (c, n) = identity();
        d.ops.clear();
        assert!(matches!(inspect(&mut d, &c, &n), Ok(Inspection::Prepared)));
        assert!(d.ops.iter().all(|x| x.0 == 0));
        assert_eq!(d.blocks[&(START + DATA)][100], 42);
    }
    #[test]
    fn ordered_flush_readback_failure_stops_and_no_outside_sector_is_written() {
        let mut d = Memory::default();
        apply(&mut d, Original(()), |_| {}).unwrap();
        let mut cursor = 0;
        let mut writes = std::collections::BTreeSet::new();
        for (first, last) in PHASES {
            for lba in first..=last {
                assert_eq!(d.ops[cursor], (1, lba));
                cursor += 1;
                assert!(writes.insert(lba));
            }
            assert_eq!(d.ops[cursor], (2, 0));
            cursor += 1;
            for lba in first..=last {
                assert_eq!(d.ops[cursor], (0, lba));
                cursor += 1;
            }
        }
        assert_eq!(cursor, d.ops.len());
        assert_eq!(d.ops.iter().rev().find(|x| x.0 == 1), Some(&(1, START)));
        for lba in [
            34,
            START - 1,
            START + DATA + 2,
            START + LENGTH - 1,
            START + LENGTH,
            BLOCKS - 34,
            BLOCKS,
            u64::MAX,
        ] {
            assert!(sector(lba).is_none());
        }
        for fail in [1, 5, 6, 7, 11, 100, 16450, 16594] {
            let mut d = Memory {
                fail: Some(fail),
                ..Memory::default()
            };
            assert_eq!(apply(&mut d, Original(()), |_| {}), Err(Error::Io));
            assert_eq!(d.ops.len(), fail);
        }
        let mut d = Memory {
            corrupt: true,
            ..Memory::default()
        };
        assert_eq!(apply(&mut d, Original(()), |_| {}), Err(Error::Verify));
        assert_eq!(d.ops.len(), 7);
    }
}
