//j933
//! SH8-g (G-a): add the RedoxFS root partition to the disposable SSD, once. Absent from default
//! images (feature `nvme-prepare-root`). It accepts only J804's exact prepared layout (with a
//! backup table that is J804's or already this one, from an interrupted run), writes the
//! host-built extent file into partition 2's LBAs, then publishes GPT v2: backup table and
//! header, then primary table, then primary header. Each phase is flushed and read back. A disk
//! already in this layout with a RedoxFS volume on partition 2 is "prepared" and never
//! rewritten, so later root files are safe. The protective MBR and every byte of J804's FAT
//! partition are untouched. KUMO itself never formats this partition (j930's rule). — TEAL
use crate::nvme::{Controller, Namespace};
use crate::nvme_prepare::{self as j804, crc, u16_at, u32_at, u64_at, Disk, Error, BLOCKS};
use kumo_gpt::sh8g::{ROOT_BLOCKS, ROOT_FIRST, ROOT_NAME, ROOT_PART_GUID};
use kumo_gpt::KUMO_REDOXFS_TYPE_GUID;

const _: () = assert!(BLOCKS == kumo_gpt::sh8g::NAMESPACE_BLOCKS);
const _: () = assert!(j804::START == kumo_gpt::sh8g::FAT_FIRST);
const _: () = assert!(j804::LENGTH == kumo_gpt::sh8g::FAT_BLOCKS);

/// The extent file's ESP path (staged by `cargo xtask image` when KUMO_NVME_PREPARE_ROOT=1).
pub const EXTENT_PATH: &str = "\\EFI\\KUMO\\root-nvme.ext";
const MAGIC: &[u8; 8] = b"KUMOEXT1";
const RECORD: usize = 8 + 4096;
const SECTORS_PER_BLOCK: u64 = 8;

/// The ranges this step may write, in publication order: the partition's extents are written
/// first (`apply_root`), then these. LBA 0 (the protective MBR) is identical in J804 and v2 and is
/// only checked.
pub const PHASES: [(u64, u64); 3] = [(BLOCKS - 33, BLOCKS - 1), (2, 33), (1, 1)];

/// GPT v2's partition entry array sector `index` (0..32): J804's entry 1, then the root.
pub fn table(index: u64) -> [u8; 512] {
    let mut b = j804::table(index);
    if index != 0 {
        return b;
    }
    let e = &mut b[128..256];
    e[..16].copy_from_slice(&KUMO_REDOXFS_TYPE_GUID.0);
    e[16..32].copy_from_slice(&ROOT_PART_GUID.0);
    u64_at(e, 32, ROOT_FIRST);
    u64_at(e, 40, ROOT_FIRST + ROOT_BLOCKS - 1);
    for (i, x) in ROOT_NAME.encode_utf16().enumerate() {
        u16_at(e, 56 + i * 2, x);
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
    b[56..72].copy_from_slice(&j804::DISK_GUID);
    u64_at(&mut b, 72, if backup { BLOCKS - 33 } else { 2 });
    u32_at(&mut b, 80, 128);
    u32_at(&mut b, 84, 128);
    let sum = crc((0..32).flat_map(|i| table(i).into_iter()));
    u32_at(&mut b, 88, sum);
    let sum = crc(b[..92].iter().copied());
    u32_at(&mut b, 16, sum);
    b
}

/// GPT v2's sector at `lba`: only the MBR and the two GPT copies exist.
pub fn sector(lba: u64) -> Option<[u8; 512]> {
    match lba {
        0 => j804::sector(0),
        1 => Some(header(false)),
        x if x == BLOCKS - 1 => Some(header(true)),
        2..=33 => Some(table(lba - 2)),
        x if (BLOCKS - 33..BLOCKS - 1).contains(&x) => Some(table(x - (BLOCKS - 33))),
        _ => None,
    }
}

/// A verified extent file: its UUID and records (block index + 4096 bytes each).
pub struct Extents<'a> {
    pub uuid: [u8; 16],
    records: &'a [u8],
}

impl<'a> Extents<'a> {
    /// Check the whole file before anything is written: magic, digest, the partition's size,
    /// record bounds and order, and a RedoxFS header with this UUID in block 0.
    pub fn parse(file: &'a [u8]) -> Result<Self, Error> {
        if file.len() < 40 + RECORD + 32 || &file[..8] != MAGIC {
            return Err(Error::Layout);
        }
        let (body, sum) = file.split_at(file.len() - 32);
        if kumo_sha256::digest(body)[..] != sum[..] {
            return Err(Error::Verify);
        }
        let uuid: [u8; 16] = body[8..24].try_into().map_err(|_| Error::Layout)?;
        let sectors = u64::from_le_bytes(body[24..32].try_into().map_err(|_| Error::Layout)?);
        let count = u64::from_le_bytes(body[32..40].try_into().map_err(|_| Error::Layout)?);
        let records = &body[40..];
        if sectors != ROOT_BLOCKS
            || count == 0
            || (records.len() as u64) != count.saturating_mul(RECORD as u64)
        {
            return Err(Error::Layout);
        }
        let ext = Self { uuid, records };
        let mut last = None;
        for (block, data) in ext.records() {
            if last.is_some_and(|prev| block <= prev)
                || !(block + 1)
                    .checked_mul(SECTORS_PER_BLOCK)
                    .is_some_and(|end| end <= ROOT_BLOCKS)
            {
                return Err(Error::Layout);
            }
            if last.is_none() && (block != 0 || &data[..8] != b"RedoxFS\0" || data[16..32] != uuid)
            {
                return Err(Error::Layout);
            }
            last = Some(block);
        }
        Ok(ext)
    }

    pub fn records(&self) -> impl Iterator<Item = (u64, &'a [u8])> + '_ {
        self.records
            .chunks_exact(RECORD)
            .map(|r| (u64::from_le_bytes(r[..8].try_into().unwrap()), &r[8..]))
    }

    pub fn count(&self) -> usize {
        self.records.len() / RECORD
    }
}

pub struct Original(());
pub enum Inspection {
    Original(Original),
    Prepared,
    /// j933: already v2 with a RedoxFS volume, inspected by a re-image build
    /// (`nvme-prepare-root-reimage`): the token re-images partition 2 (its contents are lost),
    /// and is minted only there, so an ordinary preparation can never overwrite a live root.
    Reimage(Original),
}

fn matches<D: Disk>(
    d: &mut D,
    (first, last): (u64, u64),
    want: fn(u64) -> Option<[u8; 512]>,
) -> Result<bool, Error> {
    let mut b = [0; 512];
    for lba in first..=last {
        d.read(lba, &mut b)?;
        if b != want(lba).ok_or(Error::Layout)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether every sector of `range` is J804's or v2's (each sector either), and whether all
/// of them are v2's. Anything else (`None`) is a layout this step does not know.
fn known<D: Disk>(d: &mut D, (first, last): (u64, u64)) -> Result<Option<bool>, Error> {
    let mut b = [0; 512];
    let mut all_v2 = true;
    for lba in first..=last {
        d.read(lba, &mut b)?;
        let v2 = b == sector(lba).ok_or(Error::Layout)?;
        if !v2 && b != j804::sector(lba).ok_or(Error::Layout)? {
            return Ok(None);
        }
        all_v2 &= v2;
    }
    Ok(Some(all_v2))
}

/// Only this can authorize writes. J804's markers are compared, never its mutable FAT data.
/// Every GPT sector must be J804's or v2's: an interrupted run (which publishes v2 sector by
/// sector, backup first) resumes; any other byte refuses. All v2 plus a RedoxFS header on
/// partition 2 is "prepared" and is never written again.
pub fn inspect<D: Disk>(d: &mut D, c: &Controller, n: &Namespace) -> Result<Inspection, Error> {
    if !j804::target(c, n) {
        return Err(Error::Target);
    }
    let (Some(primary_v2), Some(backup_v2)) =
        (known(d, (0, 33))?, known(d, (BLOCKS - 33, BLOCKS - 1))?)
    else {
        return Err(Error::Layout);
    };
    if primary_v2 && backup_v2 {
        let mut b = [0; 512];
        d.read(ROOT_FIRST, &mut b)?;
        return if &b[..8] != b"RedoxFS\0" {
            Err(Error::Layout)
        } else if cfg!(feature = "nvme-prepare-root-reimage") {
            Ok(Inspection::Reimage(Original(())))
        } else {
            Ok(Inspection::Prepared)
        };
    }
    let fat = matches(d, (j804::START, j804::START), j804::sector)?
        && matches(d, (j804::START + 6, j804::START + 6), j804::sector)?;
    if fat {
        return Ok(Inspection::Original(Original(())));
    }
    Err(Error::Layout)
}

/// Write the volume, then publish GPT v2. `phase(n)` reports each flushed, verified phase
/// (1 = the partition's extents, 2..4 = PHASES). Returns the sectors written.
pub fn apply<D: Disk>(
    d: &mut D,
    _original: Original,
    ext: &Extents<'_>,
    mut phase: impl FnMut(usize),
) -> Result<u64, Error> {
    let mut count = 0;
    let mut sector_buf = [0u8; 512];
    let mut read = [0u8; 512];
    for (block, data) in ext.records() {
        for (i, chunk) in data.chunks_exact(512).enumerate() {
            sector_buf.copy_from_slice(chunk);
            d.write(
                ROOT_FIRST + block * SECTORS_PER_BLOCK + i as u64,
                &sector_buf,
            )?;
            count += 1;
        }
    }
    d.flush()?;
    for (block, data) in ext.records() {
        for (i, chunk) in data.chunks_exact(512).enumerate() {
            d.read(ROOT_FIRST + block * SECTORS_PER_BLOCK + i as u64, &mut read)?;
            if read[..] != chunk[..] {
                return Err(Error::Verify);
            }
        }
    }
    phase(1);
    for (i, (first, last)) in PHASES.into_iter().enumerate() {
        for lba in first..=last {
            d.write(lba, &sector(lba).ok_or(Error::Layout)?)?;
            count += 1;
        }
        d.flush()?;
        if !matches(d, (first, last), sector)? {
            return Err(Error::Verify);
        }
        phase(i + 2);
    }
    if !matches(d, (0, 0), sector)? {
        return Err(Error::Verify);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::nvme_prepare::PHASES as J804_PHASES;
    use std::{collections::BTreeMap, vec, vec::Vec};

    #[derive(Default)]
    struct Memory {
        blocks: BTreeMap<u64, [u8; 512]>,
        writes: Vec<u64>,
        fail_write: Option<usize>,
    }
    impl Disk for Memory {
        fn read(&mut self, lba: u64, b: &mut [u8; 512]) -> Result<(), Error> {
            *b = self.blocks.get(&lba).copied().unwrap_or([0; 512]);
            Ok(())
        }
        fn write(&mut self, lba: u64, b: &[u8; 512]) -> Result<(), Error> {
            self.writes.push(lba);
            if self.fail_write == Some(self.writes.len()) {
                return Err(Error::Io);
            }
            self.blocks.insert(lba, *b);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    /// "Prepared", as this build reports it (a re-image build offers the re-image instead).
    fn prepared(verdict: Inspection) -> bool {
        match verdict {
            Inspection::Prepared => !cfg!(feature = "nvme-prepare-root-reimage"),
            Inspection::Reimage(_) => cfg!(feature = "nvme-prepare-root-reimage"),
            Inspection::Original(_) => false,
        }
    }

    fn ssd() -> (Controller, Namespace) {
        crate::nvme_prepare::tests_support::target_pair()
    }

    fn j804_disk() -> Memory {
        let mut m = Memory::default();
        for (first, last) in J804_PHASES {
            for lba in first..=last {
                m.blocks.insert(lba, j804::sector(lba).unwrap());
            }
        }
        m
    }

    fn extent_file(uuid: [u8; 16], blocks: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&uuid);
        out.extend_from_slice(&ROOT_BLOCKS.to_le_bytes());
        out.extend_from_slice(&(blocks.len() as u64).to_le_bytes());
        for &block in blocks {
            out.extend_from_slice(&block.to_le_bytes());
            let mut data = vec![(block as u8).wrapping_mul(29) | 1; 4096];
            if block == 0 {
                data[..8].copy_from_slice(b"RedoxFS\0");
                data[16..32].copy_from_slice(&uuid);
            }
            out.extend_from_slice(&data);
        }
        let sum = kumo_sha256::digest(&out);
        out.extend_from_slice(&sum);
        out
    }

    #[test]
    fn v2_is_a_valid_gpt_with_both_partitions_where_the_constants_say() {
        let mbr = sector(0).unwrap();
        kumo_gpt::validate_protective_mbr(&mbr, 512, BLOCKS).unwrap();
        let header = kumo_gpt::GptHeader::parse_primary(&sector(1).unwrap(), 512, BLOCKS).unwrap();
        let mut array = Vec::new();
        for lba in 2..=33 {
            array.extend_from_slice(&sector(lba).unwrap());
        }
        let table = header.parse_partition_table(&array).unwrap();
        let parts: Vec<_> = table.partitions().collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].first_lba, j804::START);
        assert_eq!(parts[0].block_count(), j804::LENGTH);
        assert_eq!(parts[1].type_guid, KUMO_REDOXFS_TYPE_GUID);
        assert_eq!(parts[1].unique_guid, ROOT_PART_GUID);
        assert_eq!(parts[1].first_lba, ROOT_FIRST);
        assert_eq!(parts[1].block_count(), ROOT_BLOCKS);
        assert!(
            parts[1].first_lba > crate::nvme_prepare::tests_support::NV8_RESERVED_END,
            "J805's reserved extent stays unallocated"
        );
        assert_eq!(
            sector(BLOCKS - 33),
            sector(2),
            "the backup table is the primary's"
        );
        assert_eq!(
            sector(0),
            j804::sector(0),
            "the protective MBR does not change"
        );
    }

    #[test]
    fn j804_becomes_v2_once_and_a_second_run_writes_nothing() {
        let (c, n) = ssd();
        let mut disk = j804_disk();
        let uuid = *b"KUMO-SH8-g-root!";
        let file = extent_file(uuid, &[0, 1, 255, 9000]);
        let ext = Extents::parse(&file).unwrap();
        let Inspection::Original(token) = inspect(&mut disk, &c, &n).unwrap() else {
            panic!("J804 must be the original");
        };
        let mut phases = Vec::new();
        let written = apply(&mut disk, token, &ext, |p| phases.push(p)).unwrap();
        assert_eq!(phases, [1, 2, 3, 4]);
        assert_eq!(written, 4 * 8 + 33 + 32 + 1);
        assert!(
            disk.writes
                .iter()
                .all(|&lba| lba != 0 && !(j804::START..j804::START + j804::LENGTH).contains(&lba)),
            "neither the MBR nor any FAT sector is written"
        );
        let mut b = [0; 512];
        disk.read(ROOT_FIRST + 9000 * 8 + 7, &mut b).unwrap();
        assert_eq!(b[0], (9000u64 as u8).wrapping_mul(29) | 1);
        assert!(prepared(inspect(&mut disk, &c, &n).unwrap()));
        assert!(
            matches!(
                crate::nvme_prepare::inspect(&mut disk, &c, &n),
                Err(Error::Layout)
            ),
            "J804's own preparation refuses a v2 disk rather than reformatting it"
        );
    }

    #[test]
    fn an_interrupted_run_is_resumed_only_from_a_layout_it_recognises() {
        let (c, n) = ssd();
        let uuid = *b"KUMO-SH8-g-root!";
        let file = extent_file(uuid, &[0, 3]);
        let ext = Extents::parse(&file).unwrap();
        // Stop inside the backup header phase: the backup table is v2, the rest J804.
        let mut disk = j804_disk();
        disk.fail_write = Some(16 + 33);
        let Inspection::Original(token) = inspect(&mut disk, &c, &n).unwrap() else {
            panic!()
        };
        assert_eq!(apply(&mut disk, token, &ext, |_| {}), Err(Error::Io));
        disk.fail_write = None;
        let Inspection::Original(token) = inspect(&mut disk, &c, &n).unwrap() else {
            panic!("a half-written backup table resumes")
        };
        apply(&mut disk, token, &ext, |_| {}).unwrap();
        assert!(prepared(inspect(&mut disk, &c, &n).unwrap()));
        // Stop inside the primary table: mixed J804/v2 sectors, J804 header. Resumes.
        let mut disk = j804_disk();
        disk.fail_write = Some(16 + 33 + 10);
        let Inspection::Original(token) = inspect(&mut disk, &c, &n).unwrap() else {
            panic!()
        };
        assert_eq!(apply(&mut disk, token, &ext, |_| {}), Err(Error::Io));
        disk.fail_write = None;
        assert!(matches!(
            inspect(&mut disk, &c, &n).unwrap(),
            Inspection::Original(_)
        ));
        // A GPT sector that is neither layout's: refuse.
        let mut odd = j804_disk();
        odd.blocks.insert(5, [0x11; 512]);
        assert!(matches!(inspect(&mut odd, &c, &n), Err(Error::Layout)));
        // A FAT marker changed under us: refuse.
        let mut odd = j804_disk();
        odd.blocks.insert(j804::START, [0xee; 512]);
        assert!(matches!(inspect(&mut odd, &c, &n), Err(Error::Layout)));
        // GPT v2 without a RedoxFS volume on partition 2: refuse, never write.
        let mut bare = Memory::default();
        for (first, last) in [(0u64, 33u64), (BLOCKS - 33, BLOCKS - 1)] {
            for lba in first..=last {
                bare.blocks.insert(lba, sector(lba).unwrap());
            }
        }
        assert!(matches!(inspect(&mut bare, &c, &n), Err(Error::Layout)));
    }

    #[test]
    fn a_prepared_root_is_rewritten_only_by_a_reimage_build() {
        let (c, n) = ssd();
        let mut disk = j804_disk();
        let file = extent_file(*b"KUMO-SH8-g-root!", &[0, 1]);
        let ext = Extents::parse(&file).unwrap();
        let Inspection::Original(token) = inspect(&mut disk, &c, &n).unwrap() else {
            panic!()
        };
        apply(&mut disk, token, &ext, |_| {}).unwrap();
        let verdict = inspect(&mut disk, &c, &n).unwrap();
        if cfg!(feature = "nvme-prepare-root-reimage") {
            let Inspection::Reimage(token) = verdict else {
                panic!("a re-image build offers the re-image")
            };
            apply(&mut disk, token, &ext, |_| {}).unwrap();
        } else {
            assert!(
                matches!(verdict, Inspection::Prepared),
                "never in an ordinary build"
            );
        }
    }

    #[test]
    fn only_the_exact_ssd_and_a_whole_valid_extent_file_are_accepted() {
        let (c, mut n) = ssd();
        let mut disk = j804_disk();
        n.size -= 1;
        assert!(matches!(inspect(&mut disk, &c, &n), Err(Error::Target)));
        let uuid = *b"KUMO-SH8-g-root!";
        let good = extent_file(uuid, &[0, 1]);
        assert!(Extents::parse(&good).is_ok());
        let mut torn = good.clone();
        torn[60] ^= 1;
        assert!(matches!(Extents::parse(&torn), Err(Error::Verify)));
        let past = extent_file(uuid, &[0, ROOT_BLOCKS / 8]);
        assert!(matches!(Extents::parse(&past), Err(Error::Layout)));
        let unordered = extent_file(uuid, &[0, 5, 5]);
        assert!(matches!(Extents::parse(&unordered), Err(Error::Layout)));
        let headless = extent_file(uuid, &[1, 2]);
        assert!(matches!(Extents::parse(&headless), Err(Error::Layout)));
    }
}
