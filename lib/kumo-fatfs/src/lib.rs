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
//! Scope of this slice (the smallest provable unit, `GUIDANCE/006 §6`):
//!   * parse + validate the BPB (boot sector) of a FAT32 volume,
//!   * derive the on-disk geometry (FAT region, data region, cluster→sector),
//!   * decode a 32-byte short (8.3) directory entry.
//!
//! It performs no I/O and holds no state — the caller supplies the bytes. That
//! keeps the whole crate host-testable (no `drv-blk`/runtime dependency) and
//! lets the future server compose it over any block source.

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
}
