#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//! `kumo-fatfs` — a read-only FAT32 parser for the ESP / interop role.
//!
//! Per `DESIGN/010` and `PLAN_IV §5`, FAT32 is **not** KUMO's root filesystem
//! (RedoxFS is); FAT is relegated to the EFI System Partition and removable
//! interop media. This crate is the matched pure-logic library: it turns raw
//! 512-byte sectors — as served by `drv-blk`'s block protocol — into the
//! geometry and directory entries a `fatfs` server (`userland/servers/houtu/`)
//! will later expose over the vfs protocol (`PLAN §12`).
//!
//! What this library does (grown one provable slice at a time, `GUIDANCE/006 §6`):
//!   * parse + validate the BPB (boot sector) of a FAT32 volume,
//!   * derive the on-disk geometry (FAT region, data region, cluster→sector),
//!   * decode a 32-byte short (8.3) directory entry,
//!   * decode a FAT-table entry and follow a file's cluster chain to its end,
//!   * mount a volume and read a file by 8.3 name through a [`SectorReader`].
//!
//! It owns no device and performs no I/O itself — every read goes through a
//! [`SectorReader`] the caller supplies (over `drv-blk` in the server, over a
//! byte source in tests). That keeps the whole crate host-testable with no
//! runtime dependency, and allocation-free so it runs in any `no_std` server.

/// FAT logical sector size. KUMO only targets 512-byte-sector volumes (the
/// historical MBR/GPT sector and `drv-blk`'s `BLOCK_SIZE`).
pub const SECTOR_SIZE: usize = 512;

/// A short (8.3) directory entry is 32 bytes.
pub const DIR_ENTRY_SIZE: usize = 32;

/// Directory-entry attribute bits (`FatFs`/MS-DOS standard).
pub mod attr {
    pub const READ_ONLY: u8 = 0x01;
    pub const HIDDEN: u8 = 0x02;
    pub const SYSTEM: u8 = 0x04;
    pub const VOLUME_ID: u8 = 0x08;
    pub const DIRECTORY: u8 = 0x10;
    pub const ARCHIVE: u8 = 0x20;
    /// A long-name (LFN) component is flagged by all four low bits set.
    pub const LONG_NAME: u8 = READ_ONLY | HIDDEN | SYSTEM | VOLUME_ID; // 0x0F
}

/// Why a boot sector failed to parse as a FAT32 volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatError {
    /// The supplied buffer is shorter than one sector.
    TooShort,
    /// The 0x55AA boot signature at offset 0x1FE is missing.
    BadSignature,
    /// The "FAT32   " filesystem-type string at offset 0x52 is absent.
    NotFat32,
    /// A geometry field that must be non-zero (sector size, sectors/cluster,
    /// FAT count, sectors/FAT) was zero, or the root cluster was below 2.
    BadGeometry,
    /// The backing [`SectorReader`] failed to return the boot sector.
    ReadFailed,
}

/// The parsed BIOS Parameter Block of a FAT32 volume — just the fields KUMO
/// needs to locate the FAT and data regions. All sector counts are in logical
/// sectors of [`SECTOR_SIZE`] bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bpb {
    /// Bytes per logical sector (offset 0x0B). KUMO requires 512.
    pub bytes_per_sector: u16,
    /// Logical sectors per cluster (offset 0x0D).
    pub sectors_per_cluster: u8,
    /// Reserved sectors before the first FAT, incl. the boot sector (0x0E).
    pub reserved_sectors: u16,
    /// Number of FAT copies (offset 0x10).
    pub num_fats: u8,
    /// Total logical sectors, FAT32 32-bit field (offset 0x20).
    pub total_sectors: u32,
    /// Sectors occupied by one FAT, FAT32 `BPB_FATSz32` (offset 0x24).
    pub sectors_per_fat: u32,
    /// First cluster of the root directory (offset 0x2C); normally 2.
    pub root_cluster: u32,
}

impl Bpb {
    /// Parse and validate a FAT32 boot sector. `sector` must be at least
    /// [`SECTOR_SIZE`] bytes; only the first sector is inspected.
    pub fn parse(sector: &[u8]) -> Result<Bpb, FatError> {
        if sector.len() < SECTOR_SIZE {
            return Err(FatError::TooShort);
        }
        // Boot signature: 0x55 0xAA at the end of the sector.
        if sector[0x1FE] != 0x55 || sector[0x1FF] != 0xAA {
            return Err(FatError::BadSignature);
        }
        // FAT32 filesystem-type hint. Not authoritative per the spec, but it is
        // what our images write and what every real FAT32 formatter emits; for
        // an interop reader it is a cheap, effective guard against FAT12/16.
        if &sector[0x52..0x5A] != b"FAT32   " {
            return Err(FatError::NotFat32);
        }

        let bpb = Bpb {
            bytes_per_sector: u16_le(sector, 0x0B),
            sectors_per_cluster: sector[0x0D],
            reserved_sectors: u16_le(sector, 0x0E),
            num_fats: sector[0x10],
            total_sectors: u32_le(sector, 0x20),
            sectors_per_fat: u32_le(sector, 0x24),
            root_cluster: u32_le(sector, 0x2C),
        };

        if bpb.bytes_per_sector == 0
            || bpb.sectors_per_cluster == 0
            || bpb.num_fats == 0
            || bpb.sectors_per_fat == 0
            || bpb.reserved_sectors == 0
            || bpb.root_cluster < 2
        {
            return Err(FatError::BadGeometry);
        }

        Ok(bpb)
    }

    /// Sector index of the first FAT (the FAT region follows the reserved area).
    pub const fn fat_start_sector(&self) -> u32 {
        self.reserved_sectors as u32
    }

    /// Sector index where the data region (cluster 2) begins, just past all FATs.
    pub const fn data_start_sector(&self) -> u32 {
        self.reserved_sectors as u32 + self.num_fats as u32 * self.sectors_per_fat
    }

    /// Sector index of the first sector of `cluster`. Clusters are numbered from
    /// 2 (clusters 0 and 1 are reserved); callers must pass `cluster >= 2`.
    pub const fn cluster_to_sector(&self, cluster: u32) -> u32 {
        self.data_start_sector() + (cluster - 2) * self.sectors_per_cluster as u32
    }

    /// Sector index of the first sector of the root directory.
    pub const fn root_dir_sector(&self) -> u32 {
        self.cluster_to_sector(self.root_cluster)
    }

    /// Bytes in one cluster.
    pub const fn cluster_size_bytes(&self) -> u32 {
        self.bytes_per_sector as u32 * self.sectors_per_cluster as u32
    }
}

/// A decoded short (8.3) directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry {
    /// Raw 8.3 name: 8 bytes name + 3 bytes extension, space-padded, no dot.
    pub name: [u8; 11],
    /// Attribute byte (see [`attr`]).
    pub attr: u8,
    /// First data cluster (high u16 @ 0x14 combined with low u16 @ 0x1A).
    pub first_cluster: u32,
    /// File size in bytes (0 for directories).
    pub size: u32,
}

impl DirEntry {
    /// Is this entry a subdirectory?
    pub const fn is_dir(&self) -> bool {
        self.attr & attr::DIRECTORY != 0
    }
}

/// The classification of a raw 32-byte directory slot. A directory is an array
/// of these; a reader walks it until it hits [`DirSlot::End`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirSlot {
    /// First byte 0x00 — this and every following slot are unused; stop here.
    End,
    /// First byte 0xE5 — a deleted/free entry; skip it but keep scanning.
    Free,
    /// A long-filename (LFN) component; skip it (this slice ignores LFNs).
    LongName,
    /// The volume-label entry; not a file, skip it for listings.
    VolumeLabel,
    /// A regular file or directory entry.
    File(DirEntry),
}

/// Decode one 32-byte directory slot. Buffers shorter than [`DIR_ENTRY_SIZE`]
/// are treated as the end of the directory.
pub fn parse_dir_entry(raw: &[u8]) -> DirSlot {
    if raw.len() < DIR_ENTRY_SIZE {
        return DirSlot::End;
    }
    match raw[0] {
        0x00 => return DirSlot::End,
        0xE5 => return DirSlot::Free,
        _ => {}
    }
    let attr = raw[11];
    if attr & attr::LONG_NAME == attr::LONG_NAME {
        return DirSlot::LongName;
    }
    if attr & attr::VOLUME_ID != 0 {
        return DirSlot::VolumeLabel;
    }
    let mut name = [0u8; 11];
    name.copy_from_slice(&raw[0..11]);
    let first_cluster = ((u16_le(raw, 0x14) as u32) << 16) | (u16_le(raw, 0x1A) as u32);
    DirSlot::File(DirEntry {
        name,
        attr,
        first_cluster,
        size: u32_le(raw, 0x1C),
    })
}

/// FAT32 entries are 32-bit but only the low 28 bits are meaningful; the top 4
/// bits are reserved and must be ignored when interpreting a link.
pub const FAT32_ENTRY_MASK: u32 = 0x0FFF_FFFF;
/// Sentinel for a cluster marked bad/unusable.
pub const FAT32_BAD: u32 = 0x0FFF_FFF7;
/// Any value at or above this marks the end of a cluster chain.
pub const FAT32_EOF_MIN: u32 = 0x0FFF_FFF8;

/// A decoded FAT-table entry (the link that follows one cluster).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatEntry {
    /// Unallocated (value 0).
    Free,
    /// Points to the next cluster in the chain.
    Next(u32),
    /// Cluster marked bad.
    Bad,
    /// End of chain (>= [`FAT32_EOF_MIN`]); also covers the reserved clusters 0/1.
    End,
}

/// Decode the FAT entry for `cluster` from an in-memory FAT region `fat` (the
/// bytes of the first FAT, i.e. starting at [`Bpb::fat_start_sector`]). Returns
/// `None` if the entry would fall outside `fat`.
pub fn fat_entry(fat: &[u8], cluster: u32) -> Option<FatEntry> {
    let off = cluster as usize * 4;
    if off + 4 > fat.len() {
        return None;
    }
    let raw = u32_le(fat, off) & FAT32_ENTRY_MASK;
    Some(match raw {
        0 => FatEntry::Free,
        FAT32_BAD => FatEntry::Bad,
        v if v >= FAT32_EOF_MIN => FatEntry::End,
        v => FatEntry::Next(v),
    })
}

/// Walks a file's cluster chain, reading links from an in-memory FAT region.
///
/// Yields each cluster of the chain in order, starting at `start`, and stops
/// after the cluster whose FAT entry is `End`/`Bad`/`Free` or out of range. A
/// `start` below 2 (the reserved clusters, used to denote an empty file) yields
/// nothing. The iterator allocates nothing, so it works in any `no_std` server.
pub struct ClusterChain<'a> {
    fat: &'a [u8],
    next: Option<u32>,
}

impl<'a> ClusterChain<'a> {
    /// Begin a chain walk at `start`, following links in `fat`.
    pub fn new(fat: &'a [u8], start: u32) -> Self {
        let next = if start < 2 { None } else { Some(start) };
        Self { fat, next }
    }
}

impl Iterator for ClusterChain<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let cluster = self.next?;
        // Look up what follows this cluster; anything that is not another data
        // cluster terminates the chain after we have yielded the current one.
        self.next = match fat_entry(self.fat, cluster) {
            Some(FatEntry::Next(n)) => Some(n),
            _ => None,
        };
        Some(cluster)
    }
}

/// FAT32 entries per [`SECTOR_SIZE`] sector (512 / 4).
const FAT_ENTRIES_PER_SECTOR: u32 = (SECTOR_SIZE / 4) as u32;

/// A source of 512-byte logical sectors — the one capability `kumo-fatfs` needs
/// from the outside world. The `fatfs` server implements this over `drv-blk`'s
/// block protocol; tests implement it over an in-memory image.
pub trait SectorReader {
    /// Read the sector at logical block address `lba` into `buf`. Returns
    /// `false` on failure (the read engine then stops, returning what it has).
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool;
}

/// A mounted FAT32 volume: the parsed geometry plus read helpers that drive a
/// [`SectorReader`]. Holds no buffers — each call reads one sector at a time, so
/// a large FAT never has to be resident.
pub struct FatVolume {
    bpb: Bpb,
}

impl FatVolume {
    /// Mount a volume by reading and parsing the boot sector (LBA 0).
    pub fn mount<R: SectorReader>(reader: &mut R) -> Result<FatVolume, FatError> {
        let mut buf = [0u8; SECTOR_SIZE];
        if !reader.read_sector(0, &mut buf) {
            return Err(FatError::ReadFailed);
        }
        Ok(FatVolume {
            bpb: Bpb::parse(&buf)?,
        })
    }

    /// The parsed BIOS Parameter Block.
    pub const fn bpb(&self) -> &Bpb {
        &self.bpb
    }

    /// Follow the FAT link out of `cluster` by reading only the one FAT sector
    /// that holds its entry.
    fn next_link<R: SectorReader>(&self, reader: &mut R, cluster: u32) -> Option<FatEntry> {
        let fat_sector = self.bpb.fat_start_sector() + cluster / FAT_ENTRIES_PER_SECTOR;
        let index = cluster % FAT_ENTRIES_PER_SECTOR;
        let mut buf = [0u8; SECTOR_SIZE];
        if !reader.read_sector(fat_sector, &mut buf) {
            return None;
        }
        fat_entry(&buf, index)
    }

    /// Find a file or subdirectory by raw 8.3 name (space-padded, e.g.
    /// `b"HELLO   TXT"`) in the root directory. Walks the root's cluster chain
    /// sector by sector; returns the first matching [`DirEntry`].
    pub fn find_in_root<R: SectorReader>(
        &self,
        reader: &mut R,
        name: &[u8; 11],
    ) -> Option<DirEntry> {
        let spc = self.bpb.sectors_per_cluster as u32;
        let mut cluster = self.bpb.root_cluster;
        let mut buf = [0u8; SECTOR_SIZE];
        loop {
            let first = self.bpb.cluster_to_sector(cluster);
            for s in 0..spc {
                if !reader.read_sector(first + s, &mut buf) {
                    return None;
                }
                let mut off = 0;
                while off + DIR_ENTRY_SIZE <= SECTOR_SIZE {
                    match parse_dir_entry(&buf[off..off + DIR_ENTRY_SIZE]) {
                        DirSlot::End => return None,
                        DirSlot::File(e) if &e.name == name => return Some(e),
                        _ => {}
                    }
                    off += DIR_ENTRY_SIZE;
                }
            }
            match self.next_link(reader, cluster) {
                Some(FatEntry::Next(n)) => cluster = n,
                _ => return None,
            }
        }
    }

    /// Read `entry`'s data into `out`, following its cluster chain. Copies at
    /// most `min(entry.size, out.len())` bytes and returns how many were written
    /// (which may be short if a sector read fails or the chain ends early).
    pub fn read_file<R: SectorReader>(
        &self,
        reader: &mut R,
        entry: &DirEntry,
        out: &mut [u8],
    ) -> usize {
        let limit = (entry.size as usize).min(out.len());
        if limit == 0 || entry.first_cluster < 2 {
            return 0;
        }
        let spc = self.bpb.sectors_per_cluster as u32;
        let mut cluster = entry.first_cluster;
        let mut buf = [0u8; SECTOR_SIZE];
        let mut written = 0usize;
        loop {
            let first = self.bpb.cluster_to_sector(cluster);
            for s in 0..spc {
                if written >= limit {
                    return written;
                }
                if !reader.read_sector(first + s, &mut buf) {
                    return written;
                }
                let take = (limit - written).min(SECTOR_SIZE);
                out[written..written + take].copy_from_slice(&buf[..take]);
                written += take;
            }
            match self.next_link(reader, cluster) {
                Some(FatEntry::Next(n)) => cluster = n,
                _ => return written,
            }
        }
    }
}

#[inline]
fn u16_le(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

#[inline]
fn u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Geometry constants identical to xtask's `build_fat32_image()` fixture so
    // these tests pin the exact image the initrd ships.
    const SEC: u16 = 512;
    const RESERVED: u16 = 32;
    const NUM_FATS: u8 = 2;
    const SPC: u8 = 1;
    const SPF: u32 = 32;
    const TOTAL: u32 = 4096;
    const ROOT_CLUSTER: u32 = 2;

    /// Reconstruct the fixture's boot sector (xtask `build_fat32_image`, sector 0).
    fn fixture_boot_sector() -> [u8; SECTOR_SIZE] {
        let mut s = [0u8; SECTOR_SIZE];
        s[0..3].copy_from_slice(&[0xEB, 0xFE, 0x90]);
        s[3..11].copy_from_slice(b"MSDOS5.0");
        s[0x0B..0x0D].copy_from_slice(&SEC.to_le_bytes());
        s[0x0D] = SPC;
        s[0x0E..0x10].copy_from_slice(&RESERVED.to_le_bytes());
        s[0x10] = NUM_FATS;
        s[0x15] = 0xF8;
        s[0x20..0x24].copy_from_slice(&TOTAL.to_le_bytes());
        s[0x24..0x28].copy_from_slice(&SPF.to_le_bytes());
        s[0x2C..0x30].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
        s[0x52..0x5A].copy_from_slice(b"FAT32   ");
        s[0x1FE..0x200].copy_from_slice(&[0x55, 0xAA]);
        s
    }

    /// Build a 32-byte short dir entry the way the fixture's `put_entry` does.
    fn dir_entry(name: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; DIR_ENTRY_SIZE] {
        let mut e = [0u8; DIR_ENTRY_SIZE];
        e[..11].copy_from_slice(name);
        e[11] = attr;
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    #[test]
    fn parses_fixture_bpb_fields() {
        let bpb = Bpb::parse(&fixture_boot_sector()).expect("valid FAT32");
        assert_eq!(bpb.bytes_per_sector, 512);
        assert_eq!(bpb.sectors_per_cluster, 1);
        assert_eq!(bpb.reserved_sectors, 32);
        assert_eq!(bpb.num_fats, 2);
        assert_eq!(bpb.total_sectors, 4096);
        assert_eq!(bpb.sectors_per_fat, 32);
        assert_eq!(bpb.root_cluster, 2);
    }

    #[test]
    fn derives_fixture_geometry() {
        let bpb = Bpb::parse(&fixture_boot_sector()).unwrap();
        // FAT region starts right after the 32 reserved sectors.
        assert_eq!(bpb.fat_start_sector(), 32);
        // Data region: 32 reserved + 2 FATs * 32 sectors = sector 96.
        assert_eq!(bpb.data_start_sector(), 96);
        // Root dir is cluster 2 -> first data sector.
        assert_eq!(bpb.root_dir_sector(), 96);
        // HELLO.TXT lives in cluster 3 -> sector 97 (matches the fixture).
        assert_eq!(bpb.cluster_to_sector(3), 97);
        assert_eq!(bpb.cluster_size_bytes(), 512);
    }

    #[test]
    fn accepts_512_byte_sector_with_trailing_data() {
        // A reader will hand us a full block; extra bytes past the sector must
        // be ignored, not rejected.
        let mut buf = [0u8; 1024];
        buf[..SECTOR_SIZE].copy_from_slice(&fixture_boot_sector());
        assert!(Bpb::parse(&buf).is_ok());
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(Bpb::parse(&[0u8; 64]), Err(FatError::TooShort));
    }

    #[test]
    fn rejects_missing_boot_signature() {
        let mut s = fixture_boot_sector();
        s[0x1FF] = 0x00;
        assert_eq!(Bpb::parse(&s), Err(FatError::BadSignature));
    }

    #[test]
    fn rejects_non_fat32_type() {
        let mut s = fixture_boot_sector();
        s[0x52..0x5A].copy_from_slice(b"FAT16   ");
        assert_eq!(Bpb::parse(&s), Err(FatError::NotFat32));
    }

    #[test]
    fn rejects_zero_geometry() {
        let mut s = fixture_boot_sector();
        s[0x24..0x28].copy_from_slice(&0u32.to_le_bytes()); // sectors_per_fat = 0
        assert_eq!(Bpb::parse(&s), Err(FatError::BadGeometry));
    }

    #[test]
    fn decodes_fixture_root_directory() {
        // The fixture's root dir: volume label, README.TXT, HELLO.TXT, then end.
        assert_eq!(
            parse_dir_entry(&dir_entry(b"KUMO       ", attr::VOLUME_ID, 0, 0)),
            DirSlot::VolumeLabel
        );

        let readme = dir_entry(b"README  TXT", attr::ARCHIVE, 0, 128);
        match parse_dir_entry(&readme) {
            DirSlot::File(e) => {
                assert_eq!(&e.name, b"README  TXT");
                assert_eq!(e.first_cluster, 0);
                assert_eq!(e.size, 128);
                assert!(!e.is_dir());
            }
            other => panic!("expected File, got {other:?}"),
        }

        let hello = dir_entry(b"HELLO   TXT", attr::ARCHIVE, 3, 6);
        match parse_dir_entry(&hello) {
            DirSlot::File(e) => {
                assert_eq!(&e.name, b"HELLO   TXT");
                assert_eq!(e.first_cluster, 3);
                assert_eq!(e.size, 6);
            }
            other => panic!("expected File, got {other:?}"),
        }

        // A zeroed slot terminates the directory scan.
        assert_eq!(parse_dir_entry(&[0u8; DIR_ENTRY_SIZE]), DirSlot::End);
    }

    #[test]
    fn classifies_free_and_lfn_slots() {
        let mut deleted = dir_entry(b"GONE    TXT", attr::ARCHIVE, 5, 10);
        deleted[0] = 0xE5;
        assert_eq!(parse_dir_entry(&deleted), DirSlot::Free);

        let mut lfn = [0u8; DIR_ENTRY_SIZE];
        lfn[0] = 0x41; // LFN sequence byte (non-zero, non-0xE5)
        lfn[11] = attr::LONG_NAME;
        assert_eq!(parse_dir_entry(&lfn), DirSlot::LongName);
    }

    #[test]
    fn high_and_low_cluster_words_combine() {
        // first_cluster spans two 16-bit fields; prove the high word matters.
        let e = dir_entry(b"BIG     BIN", attr::ARCHIVE, 0x0001_2345, 0);
        match parse_dir_entry(&e) {
            DirSlot::File(d) => assert_eq!(d.first_cluster, 0x0001_2345),
            other => panic!("expected File, got {other:?}"),
        }
    }

    /// Build a small FAT region from a list of 32-bit entries (LE), as on disk.
    fn fat_with(entries: &[u32]) -> [u8; 64] {
        let mut fat = [0u8; 64];
        for (i, &e) in entries.iter().enumerate() {
            fat[i * 4..i * 4 + 4].copy_from_slice(&e.to_le_bytes());
        }
        fat
    }

    #[test]
    fn fixture_fat_entries_are_end_of_chain() {
        // xtask's fixture marks entries 0..=3 with EOF/reserved values.
        let fat = fat_with(&[0x0FFF_FFF8, 0x0FFF_FFFF, 0x0FFF_FFFF, 0x0FFF_FFFF]);
        assert_eq!(fat_entry(&fat, 0), Some(FatEntry::End)); // reserved/media
        assert_eq!(fat_entry(&fat, 2), Some(FatEntry::End)); // root dir, single cluster
        assert_eq!(fat_entry(&fat, 3), Some(FatEntry::End)); // HELLO.TXT, single cluster
    }

    #[test]
    fn classifies_free_bad_and_next_entries() {
        // entry 5 = free, 6 = bad, 7 = link to 4. Top 4 bits are reserved noise.
        let fat = fat_with(&[0, 0, 0, 0, 0, 0x0000_0000, 0x0FFF_FFF7, 0xF000_0004]);
        assert_eq!(fat_entry(&fat, 5), Some(FatEntry::Free));
        assert_eq!(fat_entry(&fat, 6), Some(FatEntry::Bad));
        assert_eq!(fat_entry(&fat, 7), Some(FatEntry::Next(4)));
    }

    #[test]
    fn fat_entry_out_of_range_is_none() {
        let fat = fat_with(&[0x0FFF_FFFF, 0x0FFF_FFFF]);
        assert_eq!(fat_entry(&fat, 1000), None);
    }

    #[test]
    fn single_cluster_chain_yields_one_cluster() {
        // HELLO.TXT: starts at cluster 3, whose FAT entry is EOF.
        let fat = fat_with(&[0x0FFF_FFF8, 0x0FFF_FFFF, 0x0FFF_FFFF, 0x0FFF_FFFF]);
        let mut chain = ClusterChain::new(&fat, 3);
        assert_eq!(chain.next(), Some(3));
        assert_eq!(chain.next(), None);
    }

    #[test]
    fn follows_multi_cluster_chain_to_end() {
        // 2 -> 3 -> 4 -> EOF.
        let fat = fat_with(&[0x0FFF_FFF8, 0x0FFF_FFFF, 3, 4, 0x0FFF_FFFF]);
        let mut chain = ClusterChain::new(&fat, 2);
        assert_eq!(chain.next(), Some(2));
        assert_eq!(chain.next(), Some(3));
        assert_eq!(chain.next(), Some(4));
        assert_eq!(chain.next(), None);
    }

    #[test]
    fn empty_file_start_cluster_yields_nothing() {
        // first_cluster 0 (and the reserved cluster 1) denote a file with no data.
        let fat = fat_with(&[0x0FFF_FFF8, 0x0FFF_FFFF, 0x0FFF_FFFF]);
        assert_eq!(ClusterChain::new(&fat, 0).next(), None);
        assert_eq!(ClusterChain::new(&fat, 1).next(), None);
    }

    /// A `SectorReader` over xtask's fixture image. Sectors 0/32/96/97 mirror the
    /// real `build_fat32_image()` (BPB, FAT, root dir, HELLO.TXT). A synthetic
    /// `BIG.BIN` (cluster 4 -> 5, 600 bytes) is added — the real fixture has no
    /// multi-cluster file — to exercise the chain-spanning read loop.
    struct FixtureDisk;
    impl SectorReader for FixtureDisk {
        fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
            *buf = [0u8; SECTOR_SIZE];
            match lba {
                0 => *buf = fixture_boot_sector(),
                32 => {
                    // FAT: 0/1 reserved, 2/3 single-cluster EOF, 4 -> 5, 5 EOF.
                    let entries = [
                        0x0FFF_FFF8u32,
                        0x0FFF_FFFF,
                        0x0FFF_FFFF,
                        0x0FFF_FFFF,
                        5,
                        0x0FFF_FFFF,
                    ];
                    for (i, e) in entries.iter().enumerate() {
                        buf[i * 4..i * 4 + 4].copy_from_slice(&e.to_le_bytes());
                    }
                }
                96 => {
                    buf[0..32].copy_from_slice(&dir_entry(b"KUMO       ", attr::VOLUME_ID, 0, 0));
                    buf[32..64].copy_from_slice(&dir_entry(b"README  TXT", attr::ARCHIVE, 0, 128));
                    buf[64..96].copy_from_slice(&dir_entry(b"HELLO   TXT", attr::ARCHIVE, 3, 6));
                    buf[96..128].copy_from_slice(&dir_entry(b"BIG     BIN", attr::ARCHIVE, 4, 600));
                    // offset 128 stays 0x00 -> end of directory.
                }
                97 => buf[..6].copy_from_slice(b"hello!"),
                98 => *buf = [0xAA; SECTOR_SIZE], // BIG.BIN cluster 4
                99 => *buf = [0xBB; SECTOR_SIZE], // BIG.BIN cluster 5
                _ => {}
            }
            true
        }
    }

    #[test]
    fn mount_reads_fixture_bpb() {
        let vol = FatVolume::mount(&mut FixtureDisk).expect("mount");
        assert_eq!(vol.bpb().root_cluster, 2);
        assert_eq!(vol.bpb().data_start_sector(), 96);
    }

    #[test]
    fn reads_hello_txt_end_to_end() {
        // The real-fixture proof: resolve HELLO.TXT and read its bytes.
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        let entry = vol
            .find_in_root(&mut FixtureDisk, b"HELLO   TXT")
            .expect("HELLO.TXT present");
        assert_eq!(entry.first_cluster, 3);
        assert_eq!(entry.size, 6);

        let mut out = [0u8; 16];
        let n = vol.read_file(&mut FixtureDisk, &entry, &mut out);
        assert_eq!(n, 6);
        assert_eq!(&out[..6], b"hello!");
    }

    #[test]
    fn reads_multi_cluster_file() {
        // BIG.BIN spans clusters 4 -> 5; 600 bytes = a full sector + 88 bytes.
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        let entry = vol.find_in_root(&mut FixtureDisk, b"BIG     BIN").unwrap();
        assert_eq!(entry.size, 600);

        let mut out = [0u8; 700];
        let n = vol.read_file(&mut FixtureDisk, &entry, &mut out);
        assert_eq!(n, 600);
        assert!(out[..512].iter().all(|&b| b == 0xAA));
        assert!(out[512..600].iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn missing_file_returns_none() {
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        assert_eq!(vol.find_in_root(&mut FixtureDisk, b"NOPE    TXT"), None);
    }

    #[test]
    fn empty_file_reads_zero_bytes() {
        // README.TXT has size 128 but first_cluster 0 (no data) in the fixture.
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        let entry = vol.find_in_root(&mut FixtureDisk, b"README  TXT").unwrap();
        assert_eq!(entry.first_cluster, 0);
        let mut out = [0u8; 16];
        assert_eq!(vol.read_file(&mut FixtureDisk, &entry, &mut out), 0);
    }

    #[test]
    fn mount_propagates_read_failure() {
        struct DeadDisk;
        impl SectorReader for DeadDisk {
            fn read_sector(&mut self, _lba: u32, _buf: &mut [u8; SECTOR_SIZE]) -> bool {
                false
            }
        }
        assert!(matches!(
            FatVolume::mount(&mut DeadDisk),
            Err(FatError::ReadFailed)
        ));
    }
}
