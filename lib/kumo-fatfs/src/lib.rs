#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j514
//j537
//j543
//j545
//j547

//! `kumo-fatfs` — a FAT32 engine for the ESP / interop role.
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
//!   * mount a volume and read a file by 8.3 name through a [`SectorReader`],
//!     including positioned reads for the vfs `Read { offset, len }` protocol,
//!   * find an entry in any directory (not just the root), and resolve a
//!     `/`-separated, case-insensitive ESP path (`/EFI/BOOT/BOOTAA64.EFI`),
//!   * create an 8.3 file, resize it, and write through a [`SectorWriter`].
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
    /// Volume serial number, FAT32 `BS_VolID` (offset 0x43): the FAT filesystem's own identity,
    /// written by the formatter and stable across re-partitioning (PLAN/008 `U2`).
    pub volume_serial: u32,
    /// Volume label, FAT32 `BS_VolLab` (offset 0x47), space-padded. The formatter's copy: a
    /// rename usually updates only the root directory's label entry, so
    /// [`FatVolume::volume_label`] prefers that one.
    pub volume_label: [u8; 11],
}

/// Why a parsed FAT32 boot sector cannot be the volume of the partition it was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionFitError {
    /// The device's logical block is not the [`SECTOR_SIZE`]-byte sector this engine reads.
    BlockSize,
    /// The BPB's own `bytes_per_sector` disagrees with [`SECTOR_SIZE`].
    SectorSize,
    /// The volume claims more sectors than the partition holds.
    VolumeTooLarge,
    /// The FATs, or the root directory's first cluster, start at or beyond the volume's own end.
    RegionsPastEnd,
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
            volume_serial: u32_le(sector, 0x43),
            volume_label: sector[0x47..0x52].try_into().unwrap(),
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

    /// Check that this volume can be mounted from a partition of `partition_blocks` blocks of
    /// `block_size` bytes: the precondition for reading it through a partition-relative view of a
    /// whole disk. A boot sector that passes [`Bpb::parse`] can still claim a volume larger than
    /// its partition, or place its FATs past its own end, and a mount must not follow either one
    /// off the partition. The region arithmetic is done in `u64` with overflow checks, because
    /// [`Bpb::data_start_sector`]'s `u32` sum is only safe once this check has passed.
    /// — TIMBERDOODLE
    pub fn fits_partition(
        &self,
        partition_blocks: u64,
        block_size: usize,
    ) -> Result<(), PartitionFitError> {
        if block_size != SECTOR_SIZE {
            return Err(PartitionFitError::BlockSize);
        }
        if self.bytes_per_sector as usize != SECTOR_SIZE {
            return Err(PartitionFitError::SectorSize);
        }
        let total = u64::from(self.total_sectors);
        if total > partition_blocks {
            return Err(PartitionFitError::VolumeTooLarge);
        }
        let data_start = u64::from(self.num_fats)
            .checked_mul(u64::from(self.sectors_per_fat))
            .and_then(|fats| fats.checked_add(u64::from(self.reserved_sectors)));
        let root_dir = data_start.and_then(|start| {
            u64::from(self.root_cluster - 2)
                .checked_mul(u64::from(self.sectors_per_cluster))
                .and_then(|offset| start.checked_add(offset))
        });
        match root_dir {
            Some(root_dir) if root_dir < total && data_start.is_some_and(|start| start < total) => {
                Ok(())
            }
            _ => Err(PartitionFitError::RegionsPastEnd),
        }
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

    /// One past the highest cluster number the volume can hold: its data-region cluster count
    /// plus the two reserved numbers. A chain link at or past this is corrupt.
    pub const fn cluster_limit(&self) -> u32 {
        self.total_sectors.saturating_sub(self.data_start_sector())
            / self.sectors_per_cluster as u32
            + 2
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
    /// On-disk short-entry slot. Parsed standalone entries have no location;
    /// entries resolved through a mounted volume always do.
    pub location: Option<DirLocation>,
}

/// Location of one 32-byte short entry in its directory sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirLocation {
    pub sector: u32,
    pub offset: u16,
}

impl DirEntry {
    /// Is this entry a subdirectory?
    pub const fn is_dir(&self) -> bool {
        self.attr & attr::DIRECTORY != 0
    }

    /// The entry's short name as a listing shows it: `NAME.EXT`, padding dropped (J547).
    pub fn short_name(&self) -> ShortName {
        let mut name = ShortName {
            bytes: [0; 12],
            len: 0,
        };
        let base = self.name[..8].trim_ascii_end();
        let ext = self.name[8..].trim_ascii_end();
        for byte in base {
            name.bytes[name.len] = *byte;
            name.len += 1;
        }
        // 0x05 in the first byte stands for a real 0xE5, which would read as a deleted slot.
        if name.bytes[0] == 0x05 {
            name.bytes[0] = 0xE5;
        }
        if !ext.is_empty() {
            name.bytes[name.len] = b'.';
            name.len += 1;
            for byte in ext {
                name.bytes[name.len] = *byte;
                name.len += 1;
            }
        }
        name
    }
}

/// A short (8.3) name rendered for display: at most 12 bytes, `NAME.EXT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortName {
    bytes: [u8; 12],
    len: usize,
}

impl ShortName {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
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
    /// The volume-label entry and its space-padded 11-byte name; not a file, skip it for
    /// listings.
    VolumeLabel([u8; 11]),
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
        return DirSlot::VolumeLabel(raw[0..11].try_into().unwrap());
    }
    let mut name = [0u8; 11];
    name.copy_from_slice(&raw[0..11]);
    let first_cluster = ((u16_le(raw, 0x14) as u32) << 16) | (u16_le(raw, 0x1A) as u32);
    DirSlot::File(DirEntry {
        name,
        attr,
        first_cluster,
        size: u32_le(raw, 0x1C),
        location: None,
    })
}

/// Whether a space-padded 11-byte label names something: not blank, and not the `NO NAME`
/// placeholder formatters write for an unnamed volume.
fn label_is_named(label: &[u8; 11]) -> bool {
    label != b"NO NAME    " && label.iter().any(|byte| *byte != b' ' && *byte != 0)
}

/// Encode one path component into the raw 8.3 name field a [`DirEntry`] stores
/// (8-byte base + 3-byte extension, space-padded, no dot) so it can be matched
/// against an on-disk entry.
///
/// The component must be a valid short name: a base of 1..=8 bytes and an
/// optional extension of 1..=3 bytes separated by a single `.`. ASCII letters
/// fold to upper case, because UEFI paths are case-insensitive (so `boot`
/// matches the on-disk `BOOT`). A multi-dot name (a true long name) or one whose
/// base/extension is too long has no 8.3 form and yields `None`; so do `.` and
/// `..` (this is name encoding, not `.`/`..` path navigation). Allocation-free.
pub fn name_to_83(component: &[u8]) -> Option<[u8; 11]> {
    if component.is_empty() {
        return None;
    }
    let (base, ext): (&[u8], &[u8]) = match component.iter().position(|&b| b == b'.') {
        None => (component, b""),
        Some(i) => {
            let ext = &component[i + 1..];
            if ext.contains(&b'.') {
                return None; // a second dot -> not a short name
            }
            (&component[..i], ext)
        }
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    let mut out = [b' '; 11];
    for (i, &b) in base.iter().enumerate() {
        out[i] = b.to_ascii_uppercase();
    }
    for (i, &b) in ext.iter().enumerate() {
        out[8 + i] = b.to_ascii_uppercase();
    }
    Some(out)
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

/// Mutable half of a FAT backing capability. Writes are whole-sector so every
/// metadata/data update remains bounded and allocation-free in the engine.
pub trait SectorWriter {
    fn write_sector(&mut self, lba: u32, buf: &[u8; SECTOR_SIZE]) -> bool;

    /// Commit preceding sector writes. RAM-backed stores may acknowledge this
    /// immediately; hardware-backed implementations forward it to `drv-blk`.
    fn flush(&mut self) -> bool {
        true
    }
}

/// A device read in whole logical blocks of [`SECTOR_SIZE`] bytes by 64-bit LBA: what a block
/// driver offers before anything knows about partitions.
pub trait BlockReader {
    /// Read device block `lba` into `buf`. Returns `false` on failure.
    fn read_block(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> bool;
}

impl<D: BlockReader + ?Sized> BlockReader for &mut D {
    fn read_block(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> bool {
        (**self).read_block(lba, buf)
    }
}

/// The [`SectorReader`] for one partition of a [`BlockReader`]: volume sector `n` is device block
/// `first_lba + n`, so [`FatVolume::mount`] reads the partition's own boot sector. Sectors at or
/// past `sector_count` are refused, so a corrupt FAT cannot steer a read out of the partition
/// (J543).
pub struct PartitionReader<D> {
    device: D,
    first_lba: u64,
    sector_count: u64,
}

impl<D> PartitionReader<D> {
    pub const fn new(device: D, first_lba: u64, sector_count: u64) -> Self {
        Self {
            device,
            first_lba,
            sector_count,
        }
    }
}

impl<D: BlockReader> SectorReader for PartitionReader<D> {
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
        let lba = lba as u64;
        if lba >= self.sector_count {
            return false;
        }
        match self.first_lba.checked_add(lba) {
            Some(device_lba) => self.device.read_block(device_lba, buf),
            None => false,
        }
    }
}

/// Failure classes for FAT mutation. The vfs server maps these onto its stable
/// wire statuses rather than leaking FAT details to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatWriteError {
    Invalid,
    NoSpace,
    Io,
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

    /// Visit every file and subdirectory entry of the directory whose data begins at
    /// `dir_cluster`, in on-disk order, until `visit` returns `false` or the directory ends.
    /// Deleted slots, long-name components and the volume label are skipped; each visited entry
    /// carries its [`DirLocation`]. Returns `false` if a sector read failed, or the chain left
    /// the volume or looped, before the directory ended (J543).
    pub fn for_each_in_dir<R: SectorReader>(
        &self,
        reader: &mut R,
        dir_cluster: u32,
        mut visit: impl FnMut(&DirEntry) -> bool,
    ) -> bool {
        self.walk_dir_slots(reader, dir_cluster, |slot, location| match slot {
            DirSlot::File(mut entry) => {
                entry.location = Some(location);
                visit(&entry)
            }
            _ => true,
        })
    }

    /// The volume's label: the root directory's label entry, which a rename updates, else the
    /// boot sector's `BS_VolLab`. `None` when neither holds a name: blank, or the `NO NAME`
    /// formatters write for an unnamed volume (J545).
    pub fn volume_label<R: SectorReader>(&self, reader: &mut R) -> Option<[u8; 11]> {
        let mut root_label = None;
        self.walk_dir_slots(reader, self.bpb.root_cluster, |slot, _| match slot {
            DirSlot::VolumeLabel(name) => {
                root_label = Some(name);
                false
            }
            _ => true,
        });
        [root_label, Some(self.bpb.volume_label)]
            .into_iter()
            .flatten()
            .find(label_is_named)
    }

    /// Walk every slot of the directory at `dir_cluster` until `visit` returns `false` or the
    /// 0x00 end marker. Bounded as [`for_each_in_dir`] documents.
    fn walk_dir_slots<R: SectorReader>(
        &self,
        reader: &mut R,
        dir_cluster: u32,
        mut visit: impl FnMut(DirSlot, DirLocation) -> bool,
    ) -> bool {
        let spc = self.bpb.sectors_per_cluster as u32;
        let limit = self.bpb.cluster_limit();
        let mut cluster = dir_cluster;
        let mut buf = [0u8; SECTOR_SIZE];
        // A directory has fewer clusters than the volume; more hops than that is a cycle.
        for _ in 0..limit {
            if cluster < 2 || cluster >= limit {
                return false;
            }
            let first = self.bpb.cluster_to_sector(cluster);
            for s in 0..spc {
                if !reader.read_sector(first + s, &mut buf) {
                    return false;
                }
                let mut off = 0;
                while off + DIR_ENTRY_SIZE <= SECTOR_SIZE {
                    let slot = parse_dir_entry(&buf[off..off + DIR_ENTRY_SIZE]);
                    if slot == DirSlot::End {
                        return true;
                    }
                    let location = DirLocation {
                        sector: first + s,
                        offset: off as u16,
                    };
                    if !visit(slot, location) {
                        return true;
                    }
                    off += DIR_ENTRY_SIZE;
                }
            }
            match self.next_link(reader, cluster) {
                Some(FatEntry::Next(n)) => cluster = n,
                Some(FatEntry::End) => return true,
                _ => return false,
            }
        }
        false
    }

    /// Find a file or subdirectory by raw 8.3 name (space-padded, e.g.
    /// `b"HELLO   TXT"`) inside the directory whose data begins at `dir_cluster`.
    /// Walks that directory's own cluster chain sector by sector and returns the
    /// first matching [`DirEntry`].
    ///
    /// This is the engine behind [`find_in_root`]; pass a subdirectory entry's
    /// [`DirEntry::first_cluster`] to descend the ESP tree one level at a time —
    /// resolve `EFI`, then `BOOT`, then `BOOTAA64.EFI` (the nested layout the ESP
    /// role requires, `DESIGN/010 §4`). Returns `None` if `dir_cluster` is a
    /// reserved cluster (< 2), a sector read fails, or the directory ends with no
    /// match.
    pub fn find_in_dir<R: SectorReader>(
        &self,
        reader: &mut R,
        dir_cluster: u32,
        name: &[u8; 11],
    ) -> Option<DirEntry> {
        let mut found = None;
        self.for_each_in_dir(reader, dir_cluster, |entry| {
            if &entry.name == name {
                found = Some(*entry);
            }
            found.is_none()
        });
        found
    }

    /// Find a file or subdirectory by raw 8.3 name in the **root** directory — a
    /// thin wrapper over [`find_in_dir`] at [`Bpb::root_cluster`].
    pub fn find_in_root<R: SectorReader>(
        &self,
        reader: &mut R,
        name: &[u8; 11],
    ) -> Option<DirEntry> {
        self.find_in_dir(reader, self.bpb.root_cluster, name)
    }

    /// Resolve a `/`-separated path from the root to the [`DirEntry`] it names —
    /// the ESP lookup `DESIGN/010 §4` needs (`/EFI/BOOT/BOOTAA64.EFI`). Splits
    /// the path on `/` (a leading slash and empty components are ignored, so
    /// `/EFI/BOOT/X`, `EFI/BOOT/X`, and `EFI//BOOT/X` are equivalent), encodes
    /// each component with [`name_to_83`], and descends with [`find_in_dir`].
    ///
    /// Every component except the last must resolve to a subdirectory; otherwise
    /// — or if any component is missing, is not a valid 8.3 name, or the path has
    /// no components — returns `None`. Allocation-free.
    pub fn resolve_path<R: SectorReader>(&self, reader: &mut R, path: &[u8]) -> Option<DirEntry> {
        let mut comps = path
            .split(|&b| b == b'/')
            .filter(|c| !c.is_empty())
            .peekable();
        let mut cluster = self.bpb.root_cluster;
        while let Some(comp) = comps.next() {
            let name = name_to_83(comp)?;
            let entry = self.find_in_dir(reader, cluster, &name)?;
            if comps.peek().is_none() {
                return Some(entry); // last component: the target itself
            }
            if !entry.is_dir() {
                return None; // an interior component must be a directory to descend
            }
            cluster = entry.first_cluster;
        }
        None // empty path (no components)
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
        self.read_file_at(reader, entry, 0, out)
    }

    /// Read `entry`'s data into `out` starting at byte `offset`, following its
    /// cluster chain. Copies at most `min(entry.size - offset, out.len())` bytes
    /// and returns how many were written. Offsets at or past EOF return 0. This
    /// is the read primitive the vfs protocol's `Read { offset, len }` maps onto.
    pub fn read_file_at<R: SectorReader>(
        &self,
        reader: &mut R,
        entry: &DirEntry,
        offset: u64,
        out: &mut [u8],
    ) -> usize {
        let size = entry.size as u64;
        if offset >= size || out.is_empty() || entry.first_cluster < 2 {
            return 0;
        }
        let limit = ((size - offset).min(out.len() as u64)) as usize;
        let spc = self.bpb.sectors_per_cluster as u32;
        let mut cluster = entry.first_cluster;
        let mut buf = [0u8; SECTOR_SIZE];
        let mut skip = offset;
        let mut written = 0usize;
        loop {
            let first = self.bpb.cluster_to_sector(cluster);
            for s in 0..spc {
                if written >= limit {
                    return written;
                }
                if skip >= SECTOR_SIZE as u64 {
                    skip -= SECTOR_SIZE as u64;
                    continue;
                }
                if !reader.read_sector(first + s, &mut buf) {
                    return written;
                }
                let start = skip as usize;
                skip = 0;
                let take = (limit - written).min(SECTOR_SIZE - start);
                out[written..written + take].copy_from_slice(&buf[start..start + take]);
                written += take;
            }
            match self.next_link(reader, cluster) {
                Some(FatEntry::Next(n)) => cluster = n,
                _ => return written,
            }
        }
    }

    fn cluster_limit(&self) -> u32 {
        let data_sectors = self
            .bpb
            .total_sectors
            .saturating_sub(self.bpb.data_start_sector());
        2 + data_sectors / self.bpb.sectors_per_cluster as u32
    }

    fn chain_len_last<R: SectorReader>(
        &self,
        reader: &mut R,
        start: u32,
    ) -> Result<(u32, Option<u32>), FatWriteError> {
        if start < 2 {
            return Ok((0, None));
        }
        let limit = self.cluster_limit();
        let mut cluster = start;
        let mut count = 0u32;
        loop {
            if cluster < 2 || cluster >= limit || count >= limit.saturating_sub(2) {
                return Err(FatWriteError::Invalid);
            }
            count += 1;
            match self.next_link(reader, cluster) {
                Some(FatEntry::Next(next)) => cluster = next,
                Some(FatEntry::End) => return Ok((count, Some(cluster))),
                Some(FatEntry::Free | FatEntry::Bad) => return Err(FatWriteError::Invalid),
                None => return Err(FatWriteError::Io),
            }
        }
    }

    fn write_fat_entry<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        cluster: u32,
        value: u32,
    ) -> Result<(), FatWriteError> {
        let index = cluster % FAT_ENTRIES_PER_SECTOR;
        let relative_sector = cluster / FAT_ENTRIES_PER_SECTOR;
        if relative_sector >= self.bpb.sectors_per_fat {
            return Err(FatWriteError::Invalid);
        }
        for fat_index in 0..self.bpb.num_fats as u32 {
            let lba = self.bpb.fat_start_sector()
                + fat_index * self.bpb.sectors_per_fat
                + relative_sector;
            let mut sector = [0u8; SECTOR_SIZE];
            if !io.read_sector(lba, &mut sector) {
                return Err(FatWriteError::Io);
            }
            let offset = index as usize * 4;
            let old = u32_le(&sector, offset);
            let raw = (old & !FAT32_ENTRY_MASK) | (value & FAT32_ENTRY_MASK);
            sector[offset..offset + 4].copy_from_slice(&raw.to_le_bytes());
            if !io.write_sector(lba, &sector) {
                return Err(FatWriteError::Io);
            }
        }
        Ok(())
    }

    fn find_free_cluster<R: SectorReader>(&self, reader: &mut R) -> Result<u32, FatWriteError> {
        for cluster in 2..self.cluster_limit() {
            match self.next_link(reader, cluster) {
                Some(FatEntry::Free) => return Ok(cluster),
                Some(_) => {}
                None => return Err(FatWriteError::Io),
            }
        }
        Err(FatWriteError::NoSpace)
    }

    fn zero_cluster<R: SectorWriter>(
        &self,
        writer: &mut R,
        cluster: u32,
    ) -> Result<(), FatWriteError> {
        let zero = [0u8; SECTOR_SIZE];
        let first = self.bpb.cluster_to_sector(cluster);
        for sector in 0..self.bpb.sectors_per_cluster as u32 {
            if !writer.write_sector(first + sector, &zero) {
                return Err(FatWriteError::Io);
            }
        }
        Ok(())
    }

    fn ensure_capacity<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        entry: &mut DirEntry,
        len: u32,
    ) -> Result<(), FatWriteError> {
        let cluster_bytes = self.bpb.cluster_size_bytes();
        let needed = len.saturating_add(cluster_bytes - 1) / cluster_bytes;
        let (mut count, mut last) = self.chain_len_last(io, entry.first_cluster)?;
        while count < needed {
            let cluster = self.find_free_cluster(io)?;
            // Make the new cluster self-contained before linking it into the file.
            self.write_fat_entry(io, cluster, FAT32_EOF_MIN)?;
            self.zero_cluster(io, cluster)?;
            if let Some(previous) = last {
                self.write_fat_entry(io, previous, cluster)?;
            } else {
                entry.first_cluster = cluster;
            }
            last = Some(cluster);
            count += 1;
        }
        Ok(())
    }

    fn update_dir_entry<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        entry: &DirEntry,
    ) -> Result<(), FatWriteError> {
        let location = entry.location.ok_or(FatWriteError::Invalid)?;
        let offset = location.offset as usize;
        if offset + DIR_ENTRY_SIZE > SECTOR_SIZE {
            return Err(FatWriteError::Invalid);
        }
        let mut sector = [0u8; SECTOR_SIZE];
        if !io.read_sector(location.sector, &mut sector) {
            return Err(FatWriteError::Io);
        }
        sector[offset + 20..offset + 22]
            .copy_from_slice(&((entry.first_cluster >> 16) as u16).to_le_bytes());
        sector[offset + 26..offset + 28]
            .copy_from_slice(&(entry.first_cluster as u16).to_le_bytes());
        sector[offset + 28..offset + 32].copy_from_slice(&entry.size.to_le_bytes());
        if !io.write_sector(location.sector, &sector) {
            return Err(FatWriteError::Io);
        }
        Ok(())
    }

    fn write_storage<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        entry: &DirEntry,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, FatWriteError> {
        if data.is_empty() {
            return Ok(0);
        }
        if entry.first_cluster < 2 {
            return Err(FatWriteError::Invalid);
        }
        let cluster_bytes = self.bpb.cluster_size_bytes() as u64;
        let mut cluster = entry.first_cluster;
        let mut skip_clusters = offset / cluster_bytes;
        let mut in_cluster = offset % cluster_bytes;
        while skip_clusters != 0 {
            cluster = match self.next_link(io, cluster) {
                Some(FatEntry::Next(next)) => next,
                _ => return Err(FatWriteError::Invalid),
            };
            skip_clusters -= 1;
        }

        let mut written = 0usize;
        loop {
            let first = self.bpb.cluster_to_sector(cluster);
            for sector_index in 0..self.bpb.sectors_per_cluster as u32 {
                if written == data.len() {
                    return Ok(written);
                }
                if in_cluster >= SECTOR_SIZE as u64 {
                    in_cluster -= SECTOR_SIZE as u64;
                    continue;
                }
                let start = in_cluster as usize;
                in_cluster = 0;
                let take = (data.len() - written).min(SECTOR_SIZE - start);
                let lba = first + sector_index;
                let mut sector = [0u8; SECTOR_SIZE];
                if (start != 0 || take != SECTOR_SIZE) && !io.read_sector(lba, &mut sector) {
                    return Err(FatWriteError::Io);
                }
                sector[start..start + take].copy_from_slice(&data[written..written + take]);
                if !io.write_sector(lba, &sector) {
                    return Err(FatWriteError::Io);
                }
                written += take;
            }
            cluster = match self.next_link(io, cluster) {
                Some(FatEntry::Next(next)) => next,
                Some(FatEntry::End) if written == data.len() => return Ok(written),
                _ => return Err(FatWriteError::Invalid),
            };
        }
    }

    /// Create one missing 8.3 regular file. Parent directories must already
    /// exist; a full directory returns [`FatWriteError::NoSpace`].
    pub fn create_file<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        path: &[u8],
    ) -> Result<DirEntry, FatWriteError> {
        let mut components = path
            .split(|&byte| byte == b'/')
            .filter(|component| !component.is_empty())
            .peekable();
        let mut directory = self.bpb.root_cluster;
        let name = loop {
            let component = components.next().ok_or(FatWriteError::Invalid)?;
            let name = name_to_83(component).ok_or(FatWriteError::Invalid)?;
            if components.peek().is_none() {
                break name;
            }
            let parent = self
                .find_in_dir(io, directory, &name)
                .ok_or(FatWriteError::Invalid)?;
            if !parent.is_dir() {
                return Err(FatWriteError::Invalid);
            }
            directory = parent.first_cluster;
        };
        if self.find_in_dir(io, directory, &name).is_some() {
            return Err(FatWriteError::Invalid);
        }

        let mut cluster = directory;
        let location = loop {
            let first = self.bpb.cluster_to_sector(cluster);
            let mut found = None;
            for sector_index in 0..self.bpb.sectors_per_cluster as u32 {
                let lba = first + sector_index;
                let mut sector = [0u8; SECTOR_SIZE];
                if !io.read_sector(lba, &mut sector) {
                    return Err(FatWriteError::Io);
                }
                for offset in (0..SECTOR_SIZE).step_by(DIR_ENTRY_SIZE) {
                    if sector[offset] == 0 || sector[offset] == 0xE5 {
                        found = Some(DirLocation {
                            sector: lba,
                            offset: offset as u16,
                        });
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            if let Some(location) = found {
                break location;
            }
            cluster = match self.next_link(io, cluster) {
                Some(FatEntry::Next(next)) => next,
                Some(FatEntry::End) => return Err(FatWriteError::NoSpace),
                _ => return Err(FatWriteError::Io),
            };
        };

        let mut sector = [0u8; SECTOR_SIZE];
        if !io.read_sector(location.sector, &mut sector) {
            return Err(FatWriteError::Io);
        }
        let offset = location.offset as usize;
        sector[offset..offset + 11].copy_from_slice(&name);
        sector[offset + 11] = attr::ARCHIVE;
        sector[offset + 12..offset + DIR_ENTRY_SIZE].fill(0);
        if !io.write_sector(location.sector, &sector) {
            return Err(FatWriteError::Io);
        }
        Ok(DirEntry {
            name,
            attr: attr::ARCHIVE,
            first_cluster: 0,
            size: 0,
            location: Some(location),
        })
    }

    /// Change a file's logical length. Growth allocates and zeroes clusters;
    /// shrink retains the existing chain for bounded implementation and fast
    /// re-growth, while the on-disk size immediately hides the truncated tail.
    pub fn set_file_len<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        entry: &mut DirEntry,
        len: u64,
    ) -> Result<(), FatWriteError> {
        let len = u32::try_from(len).map_err(|_| FatWriteError::Invalid)?;
        let old_len = entry.size;
        self.ensure_capacity(io, entry, len)?;
        if len > old_len {
            let zero = [0u8; SECTOR_SIZE];
            let mut offset = old_len as u64;
            let mut remaining = (len - old_len) as usize;
            while remaining != 0 {
                let take = remaining.min(zero.len());
                self.write_storage(io, entry, offset, &zero[..take])?;
                offset += take as u64;
                remaining -= take;
            }
        }
        entry.size = len;
        self.update_dir_entry(io, entry)
    }

    /// Write bytes at an explicit offset, growing the file when necessary.
    pub fn write_file_at<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        entry: &mut DirEntry,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, FatWriteError> {
        if data.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(FatWriteError::Invalid)?;
        if end > entry.size as u64 {
            self.set_file_len(io, entry, end)?;
        }
        self.write_storage(io, entry, offset, data)
    }

    pub fn flush<R: SectorWriter>(&self, writer: &mut R) -> Result<(), FatWriteError> {
        if writer.flush() {
            Ok(())
        } else {
            Err(FatWriteError::Io)
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

    /// An ESP-shaped partition holding the fixture volume with room to spare.
    const ESP_BLOCKS: u64 = 0x10_0000;

    #[test]
    fn the_fixture_volume_fits_its_partition() {
        let bpb = Bpb::parse(&fixture_boot_sector()).unwrap();
        assert_eq!(bpb.fits_partition(ESP_BLOCKS, SECTOR_SIZE), Ok(()));
        assert_eq!(bpb.fits_partition(u64::from(TOTAL), SECTOR_SIZE), Ok(()));
    }

    #[test]
    fn a_volume_larger_than_its_partition_is_refused() {
        let bpb = Bpb::parse(&fixture_boot_sector()).unwrap();
        assert_eq!(
            bpb.fits_partition(u64::from(TOTAL) - 1, SECTOR_SIZE),
            Err(PartitionFitError::VolumeTooLarge)
        );
    }

    #[test]
    fn a_device_or_volume_that_is_not_512_byte_sectored_is_refused() {
        let bpb = Bpb::parse(&fixture_boot_sector()).unwrap();
        assert_eq!(
            bpb.fits_partition(ESP_BLOCKS, 4096),
            Err(PartitionFitError::BlockSize)
        );
        let mut sector = fixture_boot_sector();
        sector[0x0B..0x0D].copy_from_slice(&4096u16.to_le_bytes());
        let bpb = Bpb::parse(&sector).unwrap();
        assert_eq!(
            bpb.fits_partition(ESP_BLOCKS, SECTOR_SIZE),
            Err(PartitionFitError::SectorSize)
        );
    }

    #[test]
    fn regions_past_the_volume_end_are_refused_without_overflowing() {
        // FATs that would end beyond the volume.
        let mut sector = fixture_boot_sector();
        sector[0x24..0x28].copy_from_slice(&(TOTAL).to_le_bytes());
        let bpb = Bpb::parse(&sector).unwrap();
        assert_eq!(
            bpb.fits_partition(ESP_BLOCKS, SECTOR_SIZE),
            Err(PartitionFitError::RegionsPastEnd)
        );
        // A hostile BPB whose u32 region sum would wrap.
        let mut sector = fixture_boot_sector();
        sector[0x10] = 255;
        sector[0x24..0x28].copy_from_slice(&u32::MAX.to_le_bytes());
        let bpb = Bpb::parse(&sector).unwrap();
        assert_eq!(
            bpb.fits_partition(u64::MAX, SECTOR_SIZE),
            Err(PartitionFitError::RegionsPastEnd)
        );
        // A root cluster far beyond the data region.
        let mut sector = fixture_boot_sector();
        sector[0x2C..0x30].copy_from_slice(&0x0FFF_FFF0u32.to_le_bytes());
        let bpb = Bpb::parse(&sector).unwrap();
        assert_eq!(
            bpb.fits_partition(ESP_BLOCKS, SECTOR_SIZE),
            Err(PartitionFitError::RegionsPastEnd)
        );
    }

    #[test]
    fn the_volume_serial_is_read_from_bs_volid() {
        let mut sector = fixture_boot_sector();
        sector[0x43..0x47].copy_from_slice(&0x1234_abcdu32.to_le_bytes());
        assert_eq!(Bpb::parse(&sector).unwrap().volume_serial, 0x1234_abcd);
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
            DirSlot::VolumeLabel(*b"KUMO       ")
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
    /// real `build_fat32_image()` (BPB, FAT, root dir, HELLO.TXT). Two synthetic
    /// additions exercise loops the real fixture cannot: `BIG.BIN` (cluster 4 -> 5,
    /// 600 bytes) for the chain-spanning read, and an ESP-shaped subtree
    /// `EFI/BOOT/BOOTAA64.EFI` (clusters 6/7/8) for nested-directory descent
    /// (`DESIGN/010 §4`).
    struct FixtureDisk;
    impl SectorReader for FixtureDisk {
        fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
            *buf = [0u8; SECTOR_SIZE];
            match lba {
                0 => *buf = fixture_boot_sector(),
                32 => {
                    // FAT: 0/1 reserved; 2/3 single-cluster EOF; 4 -> 5, 5 EOF
                    // (BIG.BIN); 6/7/8 single-cluster EOF (EFI dir, BOOT dir,
                    // BOOTAA64.EFI).
                    let entries = [
                        0x0FFF_FFF8u32,
                        0x0FFF_FFFF,
                        0x0FFF_FFFF,
                        0x0FFF_FFFF,
                        5,
                        0x0FFF_FFFF,
                        0x0FFF_FFFF,
                        0x0FFF_FFFF,
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
                    buf[128..160].copy_from_slice(&dir_entry(
                        b"EFI        ",
                        attr::DIRECTORY,
                        6,
                        0,
                    ));
                    // offset 160 stays 0x00 -> end of directory.
                }
                97 => buf[..6].copy_from_slice(b"hello!"),
                98 => *buf = [0xAA; SECTOR_SIZE], // BIG.BIN cluster 4
                99 => *buf = [0xBB; SECTOR_SIZE], // BIG.BIN cluster 5
                100 => {
                    // EFI/ (cluster 6): real subdirs open with `.` and `..`, which
                    // the scan must skip before reaching BOOT (cluster 7).
                    buf[0..32].copy_from_slice(&dir_entry(b".          ", attr::DIRECTORY, 6, 0));
                    buf[32..64].copy_from_slice(&dir_entry(b"..         ", attr::DIRECTORY, 2, 0));
                    buf[64..96].copy_from_slice(&dir_entry(b"BOOT       ", attr::DIRECTORY, 7, 0));
                    // offset 96 stays 0x00 -> end of directory.
                }
                101 => {
                    // EFI/BOOT/ (cluster 7): `.`, `..`, then BOOTAA64.EFI (cluster 8).
                    buf[0..32].copy_from_slice(&dir_entry(b".          ", attr::DIRECTORY, 7, 0));
                    buf[32..64].copy_from_slice(&dir_entry(b"..         ", attr::DIRECTORY, 6, 0));
                    buf[64..96].copy_from_slice(&dir_entry(b"BOOTAA64EFI", attr::ARCHIVE, 8, 512));
                    // offset 96 stays 0x00 -> end of directory.
                }
                102 => {
                    // EFI/BOOT/BOOTAA64.EFI (cluster 8): a PE "MZ" header + filler.
                    *buf = [0xCC; SECTOR_SIZE];
                    buf[..2].copy_from_slice(b"MZ");
                }
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
    fn reads_file_at_offset() {
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        let entry = vol.find_in_root(&mut FixtureDisk, b"HELLO   TXT").unwrap();

        let mut out = [0u8; 4];
        let n = vol.read_file_at(&mut FixtureDisk, &entry, 1, &mut out);
        assert_eq!(n, 4);
        assert_eq!(&out[..4], b"ello");
    }

    #[test]
    fn read_file_at_offset_past_eof_is_empty() {
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        let entry = vol.find_in_root(&mut FixtureDisk, b"HELLO   TXT").unwrap();

        let mut out = [0u8; 4];
        assert_eq!(vol.read_file_at(&mut FixtureDisk, &entry, 6, &mut out), 0);
        assert_eq!(vol.read_file_at(&mut FixtureDisk, &entry, 100, &mut out), 0);
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
    fn read_file_at_crosses_cluster_boundary() {
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        let entry = vol.find_in_root(&mut FixtureDisk, b"BIG     BIN").unwrap();

        let mut out = [0u8; 96];
        let n = vol.read_file_at(&mut FixtureDisk, &entry, 510, &mut out);
        assert_eq!(n, 90); // bytes 510..600
        assert_eq!(&out[..2], &[0xAA, 0xAA]);
        assert!(out[2..90].iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn missing_file_returns_none() {
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        assert_eq!(vol.find_in_root(&mut FixtureDisk, b"NOPE    TXT"), None);
    }

    #[test]
    fn resolves_file_in_nested_subdirectory() {
        // The gate (DESIGN/010 §4): the ESP keeps its boot loader two directories
        // deep at EFI/BOOT/BOOTAA64.EFI, so a root-only lookup cannot reach it.
        // Descend by applying find_in_dir to each path component's cluster.
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();

        let efi = vol
            .find_in_root(&mut FixtureDisk, b"EFI        ")
            .expect("EFI dir in root");
        assert!(efi.is_dir());

        let boot = vol
            .find_in_dir(&mut FixtureDisk, efi.first_cluster, b"BOOT       ")
            .expect("BOOT dir in EFI");
        assert!(boot.is_dir());

        let app = vol
            .find_in_dir(&mut FixtureDisk, boot.first_cluster, b"BOOTAA64EFI")
            .expect("BOOTAA64.EFI in EFI/BOOT");
        assert!(!app.is_dir());
        assert_eq!(app.size, 512);

        // And its bytes read back end-to-end through read_file.
        let mut out = [0u8; 512];
        let n = vol.read_file(&mut FixtureDisk, &app, &mut out);
        assert_eq!(n, 512);
        assert_eq!(&out[..2], b"MZ"); // the PE header the fixture planted

        // A name absent from the subdirectory returns None, not a root hit.
        assert_eq!(
            vol.find_in_dir(&mut FixtureDisk, boot.first_cluster, b"HELLO   TXT"),
            None
        );
    }

    #[test]
    fn find_in_dir_rejects_reserved_clusters() {
        // Clusters 0 and 1 are reserved; descending into them would underflow
        // cluster_to_sector, so the guard must short-circuit to None.
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        assert_eq!(vol.find_in_dir(&mut FixtureDisk, 0, b"EFI        "), None);
        assert_eq!(vol.find_in_dir(&mut FixtureDisk, 1, b"EFI        "), None);
    }

    #[test]
    fn name_to_83_encodes_short_names() {
        assert_eq!(name_to_83(b"HELLO.TXT"), Some(*b"HELLO   TXT"));
        assert_eq!(name_to_83(b"EFI"), Some(*b"EFI        ")); // no extension
        assert_eq!(name_to_83(b"boot"), Some(*b"BOOT       ")); // case-folded
        assert_eq!(name_to_83(b"bootaa64.efi"), Some(*b"BOOTAA64EFI")); // full 8.3
    }

    #[test]
    fn name_to_83_rejects_non_short_names() {
        assert_eq!(name_to_83(b""), None); // empty component
        assert_eq!(name_to_83(b"."), None); // empty base
        assert_eq!(name_to_83(b"TOOLONGAA.TXT"), None); // base > 8
        assert_eq!(name_to_83(b"FILE.LONG"), None); // extension > 3
        assert_eq!(name_to_83(b"A.B.C"), None); // multi-dot -> a true long name
    }

    #[test]
    fn resolve_path_reaches_nested_esp_file() {
        // The gate: hand the reader the ESP path and get the boot loader back.
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        let app = vol
            .resolve_path(&mut FixtureDisk, b"/EFI/BOOT/BOOTAA64.EFI")
            .expect("resolve /EFI/BOOT/BOOTAA64.EFI");
        assert!(!app.is_dir());
        assert_eq!(app.size, 512);
        assert_eq!(app.first_cluster, 8);

        let mut out = [0u8; 512];
        assert_eq!(vol.read_file(&mut FixtureDisk, &app, &mut out), 512);
        assert_eq!(&out[..2], b"MZ"); // the PE header the fixture planted
    }

    #[test]
    fn resolve_path_is_case_insensitive_and_slash_lenient() {
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        let want = vol
            .resolve_path(&mut FixtureDisk, b"/EFI/BOOT/BOOTAA64.EFI")
            .unwrap();
        // lower case (UEFI paths are case-insensitive)
        assert_eq!(
            vol.resolve_path(&mut FixtureDisk, b"/efi/boot/bootaa64.efi"),
            Some(want)
        );
        // no leading slash, plus a doubled separator -> empty components ignored
        assert_eq!(
            vol.resolve_path(&mut FixtureDisk, b"EFI//BOOT/BOOTAA64.EFI"),
            Some(want)
        );
    }

    #[test]
    fn resolve_path_root_entries_and_rejections() {
        let vol = FatVolume::mount(&mut FixtureDisk).unwrap();
        // a single component resolves a root file
        let hello = vol.resolve_path(&mut FixtureDisk, b"/HELLO.TXT").unwrap();
        assert_eq!(hello.size, 6);
        assert_eq!(hello.first_cluster, 3);
        // a path may also name a directory
        assert!(vol
            .resolve_path(&mut FixtureDisk, b"/EFI/BOOT")
            .unwrap()
            .is_dir());
        // rejections: missing leaf, an interior component that is a file, and a
        // path with no components.
        assert_eq!(vol.resolve_path(&mut FixtureDisk, b"/EFI/NOPE.TXT"), None);
        assert_eq!(vol.resolve_path(&mut FixtureDisk, b"/HELLO.TXT/X"), None);
        assert_eq!(vol.resolve_path(&mut FixtureDisk, b""), None);
        assert_eq!(vol.resolve_path(&mut FixtureDisk, b"/"), None);
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

    struct WritableDisk {
        sectors: [[u8; SECTOR_SIZE]; 128],
        flushed: bool,
    }

    impl WritableDisk {
        fn fixture() -> WritableDisk {
            let mut disk = WritableDisk {
                sectors: [[0u8; SECTOR_SIZE]; 128],
                flushed: false,
            };
            let mut source = FixtureDisk;
            for lba in 0..disk.sectors.len() {
                source.read_sector(lba as u32, &mut disk.sectors[lba]);
            }
            disk
        }
    }

    impl SectorReader for WritableDisk {
        fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
            let Some(sector) = self.sectors.get(lba as usize) else {
                return false;
            };
            *buf = *sector;
            true
        }
    }

    impl SectorWriter for WritableDisk {
        fn write_sector(&mut self, lba: u32, buf: &[u8; SECTOR_SIZE]) -> bool {
            let Some(sector) = self.sectors.get_mut(lba as usize) else {
                return false;
            };
            *sector = *buf;
            true
        }

        fn flush(&mut self) -> bool {
            self.flushed = true;
            true
        }
    }

    #[test]
    fn creates_resizes_writes_flushes_and_reopens() {
        let mut disk = WritableDisk::fixture();
        let volume = FatVolume::mount(&mut disk).unwrap();
        let mut entry = volume.create_file(&mut disk, b"/KILO.TXT").unwrap();
        assert_eq!(entry.size, 0);
        assert_eq!(entry.first_cluster, 0);

        let mut data = [0u8; 700];
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        volume
            .set_file_len(&mut disk, &mut entry, data.len() as u64)
            .unwrap();
        assert_eq!(
            volume.write_file_at(&mut disk, &mut entry, 0, &data),
            Ok(700)
        );
        volume.flush(&mut disk).unwrap();
        assert!(disk.flushed);

        let reopened = volume.resolve_path(&mut disk, b"/KILO.TXT").unwrap();
        assert_eq!(reopened.size, 700);
        assert!(reopened.first_cluster >= 2);
        let mut out = [0u8; 700];
        assert_eq!(volume.read_file(&mut disk, &reopened, &mut out), 700);
        assert_eq!(out, data);
    }

    #[test]
    fn truncate_then_regrow_zeroes_the_hidden_tail() {
        let mut disk = WritableDisk::fixture();
        let volume = FatVolume::mount(&mut disk).unwrap();
        let mut entry = volume.resolve_path(&mut disk, b"/HELLO.TXT").unwrap();
        volume.set_file_len(&mut disk, &mut entry, 2).unwrap();
        volume.set_file_len(&mut disk, &mut entry, 6).unwrap();

        let reopened = volume.resolve_path(&mut disk, b"/HELLO.TXT").unwrap();
        let mut out = [0xFFu8; 6];
        assert_eq!(volume.read_file(&mut disk, &reopened, &mut out), 6);
        assert_eq!(&out[..2], b"he");
        assert_eq!(&out[2..], &[0, 0, 0, 0]);
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

    /// A whole device with the fixture volume at block `at`, as a stick carries its FAT32
    /// partition behind a partition table. Blocks before it read as 0xEE filler, so a reader that
    /// forgot the offset would mount garbage. Counts the device reads it serves.
    struct ShiftedDevice {
        at: u64,
        reads: u32,
    }

    impl BlockReader for ShiftedDevice {
        fn read_block(&mut self, lba: u64, buf: &mut [u8; SECTOR_SIZE]) -> bool {
            self.reads += 1;
            if lba < self.at {
                *buf = [0xEE; SECTOR_SIZE];
                return true;
            }
            FixtureDisk.read_sector((lba - self.at) as u32, buf)
        }
    }

    #[test]
    fn a_partition_reader_mounts_the_volume_at_its_own_offset() {
        // The J542 MBR stick's volume starts at LBA 2048.
        let mut device = ShiftedDevice { at: 2048, reads: 0 };
        let mut volume = PartitionReader::new(&mut device, 2048, TOTAL as u64);
        let fat = FatVolume::mount(&mut volume).unwrap();
        assert_eq!(fat.bpb().root_cluster, ROOT_CLUSTER);
        let hello = fat.resolve_path(&mut volume, b"/HELLO.TXT").unwrap();
        let mut out = [0u8; 16];
        assert_eq!(fat.read_file(&mut volume, &hello, &mut out), 6);
        assert_eq!(&out[..6], b"hello!");
        let boot = fat
            .resolve_path(&mut volume, b"/EFI/BOOT/BOOTAA64.EFI")
            .unwrap();
        assert_eq!(boot.size, 512);
    }

    #[test]
    fn a_partition_reader_never_reads_past_its_partition() {
        let mut device = ShiftedDevice { at: 40, reads: 0 };
        // A partition too short to reach the root directory at volume sector 96.
        let mut volume = PartitionReader::new(&mut device, 40, 96);
        let fat = FatVolume::mount(&mut volume).unwrap();
        let mut buf = [0u8; SECTOR_SIZE];
        assert!(!volume.read_sector(96, &mut buf));
        assert!(!volume.read_sector(u32::MAX, &mut buf));
        assert!(!fat.for_each_in_dir(&mut volume, ROOT_CLUSTER, |_| true));
        assert_eq!(fat.find_in_root(&mut volume, b"HELLO   TXT"), None);
        // Only the boot sector reached the device; every refused sector stopped short of it.
        assert_eq!(device.reads, 1);

        let mut device = ShiftedDevice { at: 0, reads: 0 };
        let mut near_the_end = PartitionReader::new(&mut device, u64::MAX - 1, 4);
        assert!(!near_the_end.read_sector(2, &mut buf));
        assert_eq!(device.reads, 0);
    }

    #[test]
    fn for_each_in_dir_lists_files_and_dirs_in_disk_order() {
        let fat = FatVolume::mount(&mut FixtureDisk).unwrap();
        let mut names = [[0u8; 11]; 8];
        let mut count = 0;
        assert!(
            fat.for_each_in_dir(&mut FixtureDisk, ROOT_CLUSTER, |entry| {
                names[count] = entry.name;
                count += 1;
                assert_eq!(entry.location.unwrap().sector, 96);
                true
            })
        );
        // The volume label is skipped; the listing stops at the 0x00 end marker.
        assert_eq!(
            &names[..count],
            &[
                *b"README  TXT",
                *b"HELLO   TXT",
                *b"BIG     BIN",
                *b"EFI        "
            ]
        );

        let mut subdir = 0;
        assert!(fat.for_each_in_dir(&mut FixtureDisk, 6, |entry| {
            subdir += 1;
            entry.name != *b"..         "
        }));
        assert_eq!(subdir, 2, "visit returning false stops the walk at `..`");
    }

    /// The fixture, with the root directory's sector packed full (no end marker) and its FAT
    /// entry pointing back at itself.
    struct LoopingRoot;

    impl SectorReader for LoopingRoot {
        fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
            match lba {
                32 => {
                    FixtureDisk.read_sector(32, buf);
                    buf[8..12].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
                    true
                }
                96 => {
                    for slot in buf.chunks_exact_mut(DIR_ENTRY_SIZE) {
                        slot.copy_from_slice(&dir_entry(b"LOOP    TXT", attr::ARCHIVE, 3, 1));
                    }
                    true
                }
                _ => FixtureDisk.read_sector(lba, buf),
            }
        }
    }

    #[test]
    fn a_directory_chain_that_loops_or_leaves_the_volume_is_refused() {
        let fat = FatVolume::mount(&mut LoopingRoot).unwrap();
        assert_eq!(fat.bpb().cluster_limit(), 4002);
        let mut visits = 0u32;
        assert!(!fat.for_each_in_dir(&mut LoopingRoot, ROOT_CLUSTER, |_| {
            visits += 1;
            true
        }));
        assert_eq!(
            visits,
            4002 * 16,
            "bounded by one pass per possible cluster"
        );
        assert_eq!(fat.find_in_root(&mut LoopingRoot, b"MISSING TXT"), None);
        assert!(!fat.for_each_in_dir(&mut FixtureDisk, 4002, |_| true));
        assert!(!fat.for_each_in_dir(&mut FixtureDisk, 1, |_| true));
    }

    #[test]
    fn the_root_label_entry_names_the_volume() {
        let fat = FatVolume::mount(&mut FixtureDisk).unwrap();
        // The fixture's root holds `KUMO` as its label entry; its BPB label is zeroed.
        assert_eq!(fat.bpb().volume_label, [0; 11]);
        assert_eq!(fat.volume_label(&mut FixtureDisk), Some(*b"KUMO       "));
    }

    /// The fixture with its root label entry renamed, or gone, and a label in the boot sector.
    struct LabelDisk {
        root_label: Option<[u8; 11]>,
        boot_label: [u8; 11],
    }

    impl SectorReader for LabelDisk {
        fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
            FixtureDisk.read_sector(lba, buf);
            match lba {
                0 => buf[0x47..0x52].copy_from_slice(&self.boot_label),
                96 => match self.root_label {
                    Some(label) => buf[0..11].copy_from_slice(&label),
                    // A deleted label slot: skipped like any freed entry.
                    None => buf[0] = 0xE5,
                },
                _ => {}
            }
            true
        }
    }

    #[test]
    fn a_renamed_volume_keeps_its_new_name_over_the_formatters() {
        let mut disk = LabelDisk {
            root_label: Some(*b"STICKY     "),
            boot_label: *b"OLDNAME    ",
        };
        let fat = FatVolume::mount(&mut disk).unwrap();
        assert_eq!(fat.bpb().volume_label, *b"OLDNAME    ");
        assert_eq!(fat.volume_label(&mut disk), Some(*b"STICKY     "));
    }

    #[test]
    fn without_a_root_entry_the_boot_sector_label_is_used_unless_unnamed() {
        let mut disk = LabelDisk {
            root_label: None,
            boot_label: *b"FROMBPB    ",
        };
        let fat = FatVolume::mount(&mut disk).unwrap();
        assert_eq!(fat.volume_label(&mut disk), Some(*b"FROMBPB    "));

        let mut unnamed = LabelDisk {
            root_label: Some(*b"NO NAME    "),
            boot_label: *b"NO NAME    ",
        };
        let fat = FatVolume::mount(&mut unnamed).unwrap();
        assert_eq!(fat.volume_label(&mut unnamed), None);
        let mut blank = LabelDisk {
            root_label: None,
            boot_label: *b"           ",
        };
        assert_eq!(fat.volume_label(&mut blank), None);
    }

    #[test]
    fn short_names_render_as_a_listing_shows_them() {
        let name = |raw: &[u8; 11], attr: u8| match parse_dir_entry(&dir_entry(raw, attr, 3, 1)) {
            DirSlot::File(entry) => entry.short_name(),
            other => panic!("not a file slot: {other:?}"),
        };
        assert_eq!(name(b"HELLO   TXT", attr::ARCHIVE).as_bytes(), b"HELLO.TXT");
        assert_eq!(name(b"EFI        ", attr::DIRECTORY).as_bytes(), b"EFI");
        assert_eq!(
            name(b"LOREM_JPTXT", attr::ARCHIVE).as_bytes(),
            b"LOREM_JP.TXT"
        );
        assert_eq!(name(b"A       B  ", attr::ARCHIVE).as_bytes(), b"A.B");
        assert_eq!(
            name(b"\x05BC     TXT", attr::ARCHIVE).as_bytes(),
            b"\xE5BC.TXT"
        );
    }
}
