//j803
//! NV7: find the one FAT32 volume a read-only NVMe service offers. The protective MBR
//! (LBA 0) and the primary GPT (header at LBA 1, CRC-checked entry array) are validated
//! through `kumo-gpt` against the namespace size from Identify. Candidates are Microsoft
//! Basic Data partitions in table order, then EFI System Partitions, as removable media
//! choose (`PartitionTable::data_volume`); the first whose own boot sector parses as FAT32
//! and fits its partition wins. Nothing is written, and no LBA outside the namespace is
//! ever requested. — TIMBERDOODLE

use crate::Error;
use kumo_fatfs::{Bpb, SECTOR_SIZE};
use kumo_gpt::{
    validate_protective_mbr, GptError, GptHeader, Guid, Partition, EFI_SYSTEM_PARTITION_TYPE_GUID,
    MICROSOFT_BASIC_DATA_TYPE_GUID,
};

/// Largest GPT entry array the service reads (the usual array is 16 KiB).
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;
/// Most partitions reported in the discovery log.
pub const MAX_REPORTED: usize = 16;

/// Consecutive 512-byte blocks from the namespace.
pub trait Blocks {
    fn read(&mut self, lba: u64, out: &mut [u8]) -> Result<(), Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Not a type that can hold the offered volume.
    Skipped,
    /// A candidate whose boot sector is not FAT32 or does not fit the partition.
    NotFat32,
    /// The chosen volume.
    Chosen,
    /// A later candidate, not probed because one was already chosen.
    Unprobed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry {
    pub partition: Partition,
    pub verdict: Verdict,
    /// A probed candidate's boot-sector OEM name (bytes 3..11): "EXFAT   ", "NTFS    " or a
    /// FAT formatter's name. Diagnostics only; never used to choose.
    pub oem: Option<[u8; 8]>,
}

/// What discovery saw and chose.
#[derive(Clone, Copy, Debug)]
pub struct Discovery {
    pub disk_guid: Guid,
    pub entries: usize,
    pub reported: [Option<Entry>; MAX_REPORTED],
    pub chosen: Option<(Partition, Bpb)>,
}

fn gpt(error: GptError) -> Error {
    let _ = error;
    Error::Data
}

/// Kind of a partition type: 2 Basic Data, 1 ESP, 0 anything else.
fn rank(partition: &Partition) -> u8 {
    if partition.type_guid == MICROSOFT_BASIC_DATA_TYPE_GUID {
        2
    } else if partition.type_guid == EFI_SYSTEM_PARTITION_TYPE_GUID {
        1
    } else {
        0
    }
}

/// Validate the GPT and choose the volume. `entries` holds the entry array; it must be
/// at least the header's array size (bounded by MAX_ENTRY_BYTES). A disk without a valid
/// GPT is an error; a valid GPT without a FAT32 candidate returns `chosen: None`.
pub fn discover(
    blocks: &mut impl Blocks,
    namespace_blocks: u64,
    entries: &mut [u8],
) -> Result<Discovery, Error> {
    let mut block = [0u8; SECTOR_SIZE];
    blocks.read(0, &mut block)?;
    validate_protective_mbr(&block, SECTOR_SIZE, namespace_blocks).map_err(gpt)?;
    blocks.read(1, &mut block)?;
    let header = GptHeader::parse_primary(&block, SECTOR_SIZE, namespace_blocks).map_err(gpt)?;
    let bytes = header.partition_entry_array_byte_len();
    let count = header.partition_entry_array_block_count();
    if bytes == 0 || bytes > MAX_ENTRY_BYTES || (count as usize) * SECTOR_SIZE > entries.len() {
        return Err(Error::Data);
    }
    let span = &mut entries[..count as usize * SECTOR_SIZE];
    blocks.read(header.partition_entry_lba, span)?;
    let table = header.parse_partition_table(span).map_err(gpt)?;
    let mut found = Discovery {
        disk_guid: header.disk_guid,
        entries: 0,
        reported: [None; MAX_REPORTED],
        chosen: None,
    };
    for partition in table.partitions() {
        if found.entries < MAX_REPORTED {
            found.reported[found.entries] = Some(Entry {
                partition,
                verdict: if rank(&partition) == 0 {
                    Verdict::Skipped
                } else {
                    Verdict::Unprobed
                },
                oem: None,
            });
        }
        found.entries += 1;
    }
    // Basic Data first, then ESP, each in table order.
    for wanted in [2u8, 1] {
        for partition in table.partitions().filter(|p| rank(p) == wanted) {
            if found.chosen.is_some() {
                break;
            }
            blocks.read(partition.first_lba, &mut block)?;
            let verdict = match Bpb::parse(&block) {
                Ok(bpb)
                    if bpb
                        .fits_partition(partition.block_count(), SECTOR_SIZE)
                        .is_ok() =>
                {
                    found.chosen = Some((partition, bpb));
                    Verdict::Chosen
                }
                _ => Verdict::NotFat32,
            };
            let oem: [u8; 8] = block[3..11].try_into().unwrap();
            for entry in found.reported.iter_mut().flatten() {
                if entry.partition.index == partition.index {
                    entry.verdict = verdict;
                    entry.oem = Some(oem);
                }
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    const LINUX: [u8; 16] = [
        0xaf, 0x3d, 0xc6, 0x0f, 0x83, 0x84, 0x72, 0x47, 0x8e, 0x79, 0x3d, 0x69, 0xd8, 0x47, 0x7d,
        0xe4,
    ];

    /// A small GPT disk: protective MBR, header, 128-entry array, partitions given as
    /// (type, first, last, fat32?) with a FAT32 boot sector written where asked.
    struct Disk {
        blocks: Vec<[u8; 512]>,
        reads: Vec<(u64, usize)>,
    }
    impl Disk {
        fn new(size: u64, parts: &[([u8; 16], u64, u64, bool)]) -> Self {
            let mut blocks = std::vec![[0u8; 512]; size as usize];
            let mbr = &mut blocks[0];
            mbr[446 + 4] = 0xee;
            mbr[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
            mbr[446 + 12..446 + 16]
                .copy_from_slice(&((size - 1).min(0xffff_ffff) as u32).to_le_bytes());
            mbr[446 + 1..446 + 4].copy_from_slice(&[0, 2, 0]);
            mbr[446 + 5..446 + 8].copy_from_slice(&[0xff, 0xff, 0xff]);
            mbr[510] = 0x55;
            mbr[511] = 0xaa;
            let mut array = std::vec![0u8; 128 * 128];
            for (i, &(kind, first, last, fat)) in parts.iter().enumerate() {
                let e = &mut array[i * 128..(i + 1) * 128];
                e[..16].copy_from_slice(&kind);
                e[16..32].copy_from_slice(&[i as u8 + 1; 16]);
                e[32..40].copy_from_slice(&first.to_le_bytes());
                e[40..48].copy_from_slice(&last.to_le_bytes());
                if fat {
                    let b = &mut blocks[first as usize];
                    b[0] = 0xeb;
                    b[1] = 0x58;
                    b[2] = 0x90;
                    b[11..13].copy_from_slice(&512u16.to_le_bytes());
                    b[13] = 1;
                    b[14..16].copy_from_slice(&32u16.to_le_bytes());
                    b[16] = 2;
                    b[21] = 0xf8;
                    let total = (last - first + 1) as u32;
                    b[32..36].copy_from_slice(&total.to_le_bytes());
                    b[36..40].copy_from_slice(&64u32.to_le_bytes());
                    b[44..48].copy_from_slice(&2u32.to_le_bytes());
                    b[66] = 0x29;
                    b[67..71].copy_from_slice(&0x1234_abcdu32.to_le_bytes());
                    b[71..82].copy_from_slice(b"NVMEBOOT   ");
                    b[82..90].copy_from_slice(b"FAT32   ");
                    b[510] = 0x55;
                    b[511] = 0xaa;
                }
            }
            for (i, chunk) in array.chunks(512).enumerate() {
                blocks[2 + i].copy_from_slice(chunk);
            }
            let h = &mut blocks[1];
            h[..8].copy_from_slice(b"EFI PART");
            h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
            h[12..16].copy_from_slice(&92u32.to_le_bytes());
            h[24..32].copy_from_slice(&1u64.to_le_bytes());
            h[32..40].copy_from_slice(&(size - 1).to_le_bytes());
            h[40..48].copy_from_slice(&34u64.to_le_bytes());
            h[48..56].copy_from_slice(&(size - 34).to_le_bytes());
            h[56..72].copy_from_slice(&[0x5a; 16]);
            h[72..80].copy_from_slice(&2u64.to_le_bytes());
            h[80..84].copy_from_slice(&128u32.to_le_bytes());
            h[84..88].copy_from_slice(&128u32.to_le_bytes());
            h[88..92].copy_from_slice(&crc32(&array).to_le_bytes());
            let crc = crc32(&h[..92]);
            h[16..20].copy_from_slice(&crc.to_le_bytes());
            Self {
                blocks,
                reads: Vec::new(),
            }
        }
    }
    impl Blocks for Disk {
        fn read(&mut self, lba: u64, out: &mut [u8]) -> Result<(), Error> {
            assert!(out.len() % 512 == 0);
            let n = out.len() / 512;
            assert!(
                lba as usize + n <= self.blocks.len(),
                "read past the namespace"
            );
            self.reads.push((lba, n));
            for (i, chunk) in out.chunks_mut(512).enumerate() {
                chunk.copy_from_slice(&self.blocks[lba as usize + i]);
            }
            Ok(())
        }
    }

    #[test]
    fn linux_disk_offers_its_fat32_esp_and_reports_every_partition() {
        let parts = [
            (EFI_SYSTEM_PARTITION_TYPE_GUID.0, 2048, 4095, true),
            (LINUX, 4096, 9999, false),
        ];
        let mut d = Disk::new(10240, &parts);
        let mut entries = std::vec![0u8; MAX_ENTRY_BYTES];
        let found = discover(&mut d, 10240, &mut entries).unwrap();
        let (p, bpb) = found.chosen.unwrap();
        assert_eq!((p.index, p.first_lba, p.block_count()), (0, 2048, 2048));
        assert_eq!(bpb.volume_serial, 0x1234_abcd);
        assert_eq!(found.entries, 2);
        let verdicts: Vec<_> = found.reported.iter().flatten().map(|e| e.verdict).collect();
        assert_eq!(verdicts, [Verdict::Chosen, Verdict::Skipped]);
        assert_eq!(d.reads, [(0, 1), (1, 1), (2, 32), (2048, 1)]);
    }

    #[test]
    fn basic_data_is_preferred_and_a_non_fat_candidate_is_passed_over() {
        let parts = [
            (EFI_SYSTEM_PARTITION_TYPE_GUID.0, 2048, 4095, true),
            (MICROSOFT_BASIC_DATA_TYPE_GUID.0, 4096, 6143, false),
            (MICROSOFT_BASIC_DATA_TYPE_GUID.0, 6144, 9999, true),
        ];
        let mut d = Disk::new(10240, &parts);
        let mut entries = std::vec![0u8; MAX_ENTRY_BYTES];
        let found = discover(&mut d, 10240, &mut entries).unwrap();
        assert_eq!(found.chosen.unwrap().0.index, 2);
        let verdicts: Vec<_> = found.reported.iter().flatten().map(|e| e.verdict).collect();
        assert_eq!(
            verdicts,
            [Verdict::Unprobed, Verdict::NotFat32, Verdict::Chosen]
        );
        // Only probed candidates carry a boot-sector name; an empty one reads as zeros.
        let oems: Vec<_> = found.reported.iter().flatten().map(|e| e.oem).collect();
        assert_eq!(oems, [None, Some([0; 8]), Some([0; 8])]);
    }

    #[test]
    fn no_fat_candidate_is_reported_not_invented() {
        let parts = [(LINUX, 2048, 9999, false)];
        let mut d = Disk::new(10240, &parts);
        let mut entries = std::vec![0u8; MAX_ENTRY_BYTES];
        let found = discover(&mut d, 10240, &mut entries).unwrap();
        assert!(found.chosen.is_none());
        assert_eq!(found.reported[0].unwrap().verdict, Verdict::Skipped);
    }

    #[test]
    fn corrupt_tables_and_wrong_sizes_are_refused() {
        let parts = [(EFI_SYSTEM_PARTITION_TYPE_GUID.0, 2048, 4095, true)];
        let mut entries = std::vec![0u8; MAX_ENTRY_BYTES];
        let mut d = Disk::new(10240, &parts);
        d.blocks[2][40] ^= 1;
        assert_eq!(
            discover(&mut d, 10240, &mut entries).err(),
            Some(Error::Data)
        );
        let mut d = Disk::new(10240, &parts);
        d.blocks[1][0] = b'X';
        assert_eq!(
            discover(&mut d, 10240, &mut entries).err(),
            Some(Error::Data)
        );
        let mut d = Disk::new(10240, &parts);
        d.blocks[0][510] = 0;
        assert_eq!(
            discover(&mut d, 10240, &mut entries).err(),
            Some(Error::Data)
        );
        // A header describing a disk larger than the namespace is refused before its array.
        let mut d = Disk::new(10240, &parts);
        assert_eq!(
            discover(&mut d, 5000, &mut entries).err(),
            Some(Error::Data)
        );
        assert!(d.reads.iter().all(|&(lba, _)| lba < 2));
        let mut d = Disk::new(10240, &parts);
        let mut small = std::vec![0u8; 4096];
        assert_eq!(discover(&mut d, 10240, &mut small).err(), Some(Error::Data));
    }
}
