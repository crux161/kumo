#![no_std]
//j524
//j542
//j543

//! Allocation-free validation and discovery for GUID Partition Tables.
//!
//! The caller supplies independently read logical blocks: LBA0 to
//! [`validate_protective_mbr`], LBA1 to [`GptHeader::parse_primary`], and the
//! partition-entry bytes described by the resulting header to
//! [`GptHeader::parse_partition_table`]. This crate performs no I/O and owns no
//! storage, so a USB/SCSI driver and host tests use the same parser.
//!
//! Removable media formatted elsewhere usually carry a classic MBR instead of a
//! GPT. When LBA0 is not a protective MBR, [`MbrPartitionTable::parse`] reads
//! its four primary records from the same block.

const MBR_SIZE: usize = 512;
const MBR_PARTITIONS_OFFSET: usize = 446;
const MBR_PARTITION_SIZE: usize = 16;
const GPT_HEADER_MIN_SIZE: usize = 92;
const GPT_ENTRY_MIN_SIZE: usize = 128;
const GPT_ENTRY_ARRAY_MIN_BYTES: u64 = 16_384;
const GPT_HEADER_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_REVISION_1_0: u32 = 0x0001_0000;

/// EFI System Partition type GUID in the byte order stored in a GPT entry.
pub const EFI_SYSTEM_PARTITION_TYPE_GUID: Guid = Guid([
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
]);

/// Microsoft Basic Data partition type GUID (EBD0A0A2-B9E5-4433-87C0-68B6B72699C7) in GPT entry
/// byte order: what macOS, Windows and Linux formatters give a FAT32 (or exFAT/NTFS) data volume.
pub const MICROSOFT_BASIC_DATA_TYPE_GUID: Guid = Guid([
    0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7,
]);

/// A GUID in the mixed-endian byte order used directly by GPT structures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    pub const ZERO: Self = Self([0; 16]);

    pub const fn is_zero(self) -> bool {
        let bytes = self.0;
        bytes[0] == 0
            && bytes[1] == 0
            && bytes[2] == 0
            && bytes[3] == 0
            && bytes[4] == 0
            && bytes[5] == 0
            && bytes[6] == 0
            && bytes[7] == 0
            && bytes[8] == 0
            && bytes[9] == 0
            && bytes[10] == 0
            && bytes[11] == 0
            && bytes[12] == 0
            && bytes[13] == 0
            && bytes[14] == 0
            && bytes[15] == 0
    }
}

/// Why a supplied disk structure is not a valid primary GPT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GptError {
    BufferTooShort,
    InvalidLogicalBlockSize,
    InvalidMediaSize,
    BadMbrSignature,
    BadProtectiveMbr,
    BadHeaderSignature,
    UnsupportedRevision,
    BadHeaderSize,
    BadHeaderCrc,
    NonzeroReservedBytes,
    BadHeaderLocation,
    BadUsableRange,
    BadDiskGuid,
    BadEntryGeometry,
    EntryArrayOutOfBounds,
    EntryArrayTooShort,
    BadEntryArrayCrc,
    BadPartitionEntry,
    OverlappingPartitions,
}

/// Validate the protective MBR stored in logical block zero.
///
/// Exactly one partition record must protect LBA1 through the end of the medium
/// (or through `u32::MAX`), and the other three records must be zero.
pub fn validate_protective_mbr(
    logical_block: &[u8],
    logical_block_size: usize,
    media_block_count: u64,
) -> Result<(), GptError> {
    validate_media(logical_block, logical_block_size, media_block_count)?;
    if logical_block[510..512] != [0x55, 0xaa] {
        return Err(GptError::BadMbrSignature);
    }
    if logical_block[440..446].iter().any(|byte| *byte != 0)
        || logical_block[MBR_SIZE..logical_block_size]
            .iter()
            .any(|byte| *byte != 0)
    {
        return Err(GptError::BadProtectiveMbr);
    }

    let expected_size = (media_block_count - 1).min(u32::MAX as u64) as u32;
    let mut protective_entries = 0;
    for index in 0..4 {
        let offset = MBR_PARTITIONS_OFFSET + index * MBR_PARTITION_SIZE;
        let entry = &logical_block[offset..offset + MBR_PARTITION_SIZE];
        if entry.iter().all(|byte| *byte == 0) {
            continue;
        }
        if entry[0] != 0
            || entry[4] != 0xee
            || u32_le(entry, 8) != 1
            || u32_le(entry, 12) != expected_size
        {
            return Err(GptError::BadProtectiveMbr);
        }
        protective_entries += 1;
    }
    if protective_entries != 1 {
        return Err(GptError::BadProtectiveMbr);
    }
    Ok(())
}

/// Classic MBR partition type: FAT32, CHS-addressed (its LBA fields are still authoritative).
pub const MBR_TYPE_FAT32_CHS: u8 = 0x0b;
/// Classic MBR partition type: FAT32, LBA-addressed.
pub const MBR_TYPE_FAT32_LBA: u8 = 0x0c;
/// Classic MBR partition type: the GPT protective record.
pub const MBR_TYPE_GPT_PROTECTIVE: u8 = 0xee;
/// Classic MBR partition type: an EFI System Partition (FAT) on MBR media.
pub const MBR_TYPE_EFI_SYSTEM: u8 = 0xef;

const MBR_BOOT_INDICATOR_INACTIVE: u8 = 0x00;
const MBR_BOOT_INDICATOR_ACTIVE: u8 = 0x80;

/// Why logical block zero is not a usable classic MBR partition table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MbrError {
    BufferTooShort,
    InvalidLogicalBlockSize,
    InvalidMediaSize,
    BadSignature,
    /// A boot indicator other than 0x00 or 0x80: LBA0 is most likely a volume boot sector (a
    /// "superfloppy" with no partition table), whose boot code overlaps the records.
    BadBootIndicator,
    /// A 0xEE record: the medium is GPT (or a hybrid MBR), so the GPT path owns it.
    ProtectiveRecord,
    /// A used record that starts at LBA0, has no blocks, or runs past the end of the medium.
    BadPartitionEntry,
    OverlappingPartitions,
}

/// A validated primary record from a classic MBR.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MbrPartition {
    /// Zero-based record slot (0..4) in the MBR.
    pub index: u8,
    pub partition_type: u8,
    pub bootable: bool,
    pub first_lba: u64,
    pub block_count: u64,
}

impl MbrPartition {
    pub const fn last_lba(self) -> u64 {
        self.first_lba + self.block_count - 1
    }

    /// Whether the record's type is one a FAT32 volume is filed under: FAT32 (CHS or LBA) or an
    /// EFI System Partition. The volume's own boot sector still has to prove it.
    pub const fn may_hold_fat32(self) -> bool {
        matches!(
            self.partition_type,
            MBR_TYPE_FAT32_CHS | MBR_TYPE_FAT32_LBA | MBR_TYPE_EFI_SYSTEM
        )
    }
}

/// The four primary records of a classic MBR, each checked against the medium. Extended
/// partitions are reported as records but their logical partitions are not followed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MbrPartitionTable {
    partitions: [Option<MbrPartition>; 4],
}

impl MbrPartitionTable {
    /// Parse and validate the classic partition table in logical block zero.
    ///
    /// A record whose type byte is zero is unused. Every record, used or not, must carry a boot
    /// indicator of 0x00 or 0x80, the test that tells a partition table from boot code.
    pub fn parse(
        logical_block: &[u8],
        logical_block_size: usize,
        media_block_count: u64,
    ) -> Result<Self, MbrError> {
        validate_media(logical_block, logical_block_size, media_block_count).map_err(|error| {
            match error {
                GptError::InvalidLogicalBlockSize => MbrError::InvalidLogicalBlockSize,
                GptError::InvalidMediaSize => MbrError::InvalidMediaSize,
                _ => MbrError::BufferTooShort,
            }
        })?;
        if logical_block[510..512] != [0x55, 0xaa] {
            return Err(MbrError::BadSignature);
        }

        let mut partitions = [None; 4];
        for (index, slot) in partitions.iter_mut().enumerate() {
            let offset = MBR_PARTITIONS_OFFSET + index * MBR_PARTITION_SIZE;
            let record = &logical_block[offset..offset + MBR_PARTITION_SIZE];
            let boot_indicator = record[0];
            if boot_indicator != MBR_BOOT_INDICATOR_INACTIVE
                && boot_indicator != MBR_BOOT_INDICATOR_ACTIVE
            {
                return Err(MbrError::BadBootIndicator);
            }
            let partition_type = record[4];
            if partition_type == 0 {
                continue;
            }
            if partition_type == MBR_TYPE_GPT_PROTECTIVE {
                return Err(MbrError::ProtectiveRecord);
            }
            let first_lba = u32_le(record, 8) as u64;
            let block_count = u32_le(record, 12) as u64;
            if first_lba == 0 || block_count == 0 || first_lba + block_count > media_block_count {
                return Err(MbrError::BadPartitionEntry);
            }
            *slot = Some(MbrPartition {
                index: index as u8,
                partition_type,
                bootable: boot_indicator == MBR_BOOT_INDICATOR_ACTIVE,
                first_lba,
                block_count,
            });
        }

        for (left_index, left) in partitions.iter().enumerate() {
            let Some(left) = left else { continue };
            for right in partitions[left_index + 1..].iter().flatten() {
                if left.first_lba <= right.last_lba() && right.first_lba <= left.last_lba() {
                    return Err(MbrError::OverlappingPartitions);
                }
            }
        }
        Ok(Self { partitions })
    }

    /// The record in slot `index` (0..4), if used.
    pub fn partition(&self, index: usize) -> Option<MbrPartition> {
        self.partitions.get(index).copied().flatten()
    }

    /// The used records, in slot order.
    pub fn partitions(&self) -> impl Iterator<Item = MbrPartition> + '_ {
        self.partitions.iter().flatten().copied()
    }

    /// The first record, in slot order, whose type may hold a FAT32 volume.
    pub fn fat32_partition(&self) -> Option<MbrPartition> {
        self.partitions()
            .find(|partition| partition.may_hold_fat32())
    }
}

/// Validated geometry and identity from a primary GPT header at LBA1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GptHeader {
    pub disk_guid: Guid,
    pub backup_lba: u64,
    pub first_usable_lba: u64,
    pub last_usable_lba: u64,
    pub partition_entry_lba: u64,
    pub partition_entry_count: u32,
    pub partition_entry_size: u32,
    pub partition_entry_array_crc32: u32,
    logical_block_size: usize,
    media_block_count: u64,
    partition_entry_array_bytes: usize,
}

impl GptHeader {
    /// Parse and validate the primary GPT header read from LBA1.
    pub fn parse_primary(
        logical_block: &[u8],
        logical_block_size: usize,
        media_block_count: u64,
    ) -> Result<Self, GptError> {
        validate_media(logical_block, logical_block_size, media_block_count)?;
        if &logical_block[0..8] != GPT_HEADER_SIGNATURE {
            return Err(GptError::BadHeaderSignature);
        }
        if u32_le(logical_block, 8) != GPT_REVISION_1_0 {
            return Err(GptError::UnsupportedRevision);
        }

        let header_size = u32_le(logical_block, 12) as usize;
        if !(GPT_HEADER_MIN_SIZE..=logical_block_size).contains(&header_size) {
            return Err(GptError::BadHeaderSize);
        }
        let expected_crc = u32_le(logical_block, 16);
        if crc32_with_zeroed_checksum(&logical_block[..header_size]) != expected_crc {
            return Err(GptError::BadHeaderCrc);
        }
        if logical_block[20..24].iter().any(|byte| *byte != 0)
            || logical_block[header_size..logical_block_size]
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(GptError::NonzeroReservedBytes);
        }

        let current_lba = u64_le(logical_block, 24);
        let backup_lba = u64_le(logical_block, 32);
        if current_lba != 1 || backup_lba != media_block_count - 1 {
            return Err(GptError::BadHeaderLocation);
        }

        let first_usable_lba = u64_le(logical_block, 40);
        let last_usable_lba = u64_le(logical_block, 48);
        if first_usable_lba > last_usable_lba || last_usable_lba >= backup_lba {
            return Err(GptError::BadUsableRange);
        }

        let disk_guid = guid(logical_block, 56);
        if disk_guid.is_zero() {
            return Err(GptError::BadDiskGuid);
        }

        let partition_entry_lba = u64_le(logical_block, 72);
        let partition_entry_count = u32_le(logical_block, 80);
        let partition_entry_size = u32_le(logical_block, 84);
        let entry_size_valid = partition_entry_size >= GPT_ENTRY_MIN_SIZE as u32
            && partition_entry_size.is_power_of_two();
        let entry_array_bytes_u64 = (partition_entry_count as u64)
            .checked_mul(partition_entry_size as u64)
            .ok_or(GptError::BadEntryGeometry)?;
        if partition_entry_count == 0
            || !entry_size_valid
            || entry_array_bytes_u64 < GPT_ENTRY_ARRAY_MIN_BYTES
            || entry_array_bytes_u64 > usize::MAX as u64
        {
            return Err(GptError::BadEntryGeometry);
        }

        let block_size_u64 = logical_block_size as u64;
        let entry_array_blocks = entry_array_bytes_u64
            .checked_add(block_size_u64 - 1)
            .ok_or(GptError::EntryArrayOutOfBounds)?
            / block_size_u64;
        let entry_array_end = partition_entry_lba
            .checked_add(entry_array_blocks)
            .ok_or(GptError::EntryArrayOutOfBounds)?;
        if partition_entry_lba <= current_lba
            || entry_array_end > first_usable_lba
            || entry_array_end > media_block_count
        {
            return Err(GptError::EntryArrayOutOfBounds);
        }

        Ok(Self {
            disk_guid,
            backup_lba,
            first_usable_lba,
            last_usable_lba,
            partition_entry_lba,
            partition_entry_count,
            partition_entry_size,
            partition_entry_array_crc32: u32_le(logical_block, 88),
            logical_block_size,
            media_block_count,
            partition_entry_array_bytes: entry_array_bytes_u64 as usize,
        })
    }

    /// Number of table bytes covered by the partition-entry CRC.
    pub const fn partition_entry_array_byte_len(self) -> usize {
        self.partition_entry_array_bytes
    }

    /// Number of logical blocks the caller must read to cover the entry array.
    pub const fn partition_entry_array_block_count(self) -> u64 {
        let bytes = self.partition_entry_array_bytes as u64;
        let block_size = self.logical_block_size as u64;
        bytes.div_ceil(block_size)
    }

    /// Validate the complete partition-entry array described by this header.
    pub fn parse_partition_table<'a>(
        self,
        entry_array: &'a [u8],
    ) -> Result<PartitionTable<'a>, GptError> {
        if entry_array.len() < self.partition_entry_array_bytes {
            return Err(GptError::EntryArrayTooShort);
        }
        let covered = &entry_array[..self.partition_entry_array_bytes];
        if crc32(covered) != self.partition_entry_array_crc32 {
            return Err(GptError::BadEntryArrayCrc);
        }

        for left_index in 0..self.partition_entry_count {
            let Some(left) = self.partition_at(covered, left_index)? else {
                continue;
            };
            for right_index in left_index + 1..self.partition_entry_count {
                let Some(right) = self.partition_at(covered, right_index)? else {
                    continue;
                };
                if left.first_lba <= right.last_lba && right.first_lba <= left.last_lba {
                    return Err(GptError::OverlappingPartitions);
                }
            }
        }

        Ok(PartitionTable {
            header: self,
            entry_array: covered,
        })
    }

    fn partition_at(self, entry_array: &[u8], index: u32) -> Result<Option<Partition>, GptError> {
        let entry_size = self.partition_entry_size as usize;
        let offset = index as usize * entry_size;
        let entry = &entry_array[offset..offset + entry_size];
        let type_guid = guid(entry, 0);
        if type_guid.is_zero() {
            return Ok(None);
        }

        let unique_guid = guid(entry, 16);
        let first_lba = u64_le(entry, 32);
        let last_lba = u64_le(entry, 40);
        if unique_guid.is_zero()
            || first_lba < self.first_usable_lba
            || first_lba > last_lba
            || last_lba > self.last_usable_lba
            || last_lba >= self.media_block_count
            || entry[GPT_ENTRY_MIN_SIZE..].iter().any(|byte| *byte != 0)
        {
            return Err(GptError::BadPartitionEntry);
        }

        Ok(Some(Partition {
            index,
            type_guid,
            unique_guid,
            first_lba,
            last_lba,
            attributes: u64_le(entry, 48),
        }))
    }
}

/// A validated GPT partition entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Partition {
    /// Zero-based slot in the GPT partition-entry array.
    pub index: u32,
    pub type_guid: Guid,
    pub unique_guid: Guid,
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
}

impl Partition {
    pub const fn block_count(self) -> u64 {
        self.last_lba - self.first_lba + 1
    }
}

/// A CRC-checked, range-checked view of a GPT partition-entry array.
pub struct PartitionTable<'a> {
    header: GptHeader,
    entry_array: &'a [u8],
}

impl PartitionTable<'_> {
    pub const fn header(&self) -> GptHeader {
        self.header
    }

    pub fn partition(&self, index: u32) -> Option<Partition> {
        if index >= self.header.partition_entry_count {
            return None;
        }
        self.header
            .partition_at(self.entry_array, index)
            .expect("validated partition table")
    }

    /// Find the first partition with the requested GPT type GUID.
    pub fn find_by_type_guid(&self, type_guid: Guid) -> Option<Partition> {
        (0..self.header.partition_entry_count)
            .filter_map(|index| self.partition(index))
            .find(|partition| partition.type_guid == type_guid)
    }

    /// Find the first EFI System Partition in table order.
    pub fn efi_system_partition(&self) -> Option<Partition> {
        self.find_by_type_guid(EFI_SYSTEM_PARTITION_TYPE_GUID)
    }

    /// The partitions in use, in table order.
    pub fn partitions(&self) -> impl Iterator<Item = Partition> + '_ {
        (0..self.header.partition_entry_count).filter_map(|index| self.partition(index))
    }

    /// The volume a removable medium keeps its data on: the first Microsoft Basic Data
    /// partition, else the EFI System Partition. `diskutil` and Windows put a small ESP ahead of
    /// the data volume, so taking the ESP first would mount the empty one. The volume's own boot
    /// sector still has to prove it is FAT32 (J543).
    pub fn data_volume(&self) -> Option<Partition> {
        self.find_by_type_guid(MICROSOFT_BASIC_DATA_TYPE_GUID)
            .or_else(|| self.efi_system_partition())
    }
}

fn validate_media(
    logical_block: &[u8],
    logical_block_size: usize,
    media_block_count: u64,
) -> Result<(), GptError> {
    if logical_block_size < MBR_SIZE {
        return Err(GptError::InvalidLogicalBlockSize);
    }
    if media_block_count < 3 {
        return Err(GptError::InvalidMediaSize);
    }
    if logical_block.len() < logical_block_size {
        return Err(GptError::BufferTooShort);
    }
    Ok(())
}

fn guid(bytes: &[u8], offset: usize) -> Guid {
    let mut value = [0; 16];
    value.copy_from_slice(&bytes[offset..offset + 16]);
    Guid(value)
}

fn u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_le(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn crc32(bytes: &[u8]) -> u32 {
    !crc32_update(!0, bytes)
}

fn crc32_with_zeroed_checksum(header: &[u8]) -> u32 {
    let crc = crc32_update(!0, &header[..16]);
    let crc = crc32_update(crc, &[0; 4]);
    !crc32_update(crc, &header[20..])
}

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let polynomial = (crc & 1).wrapping_neg() & 0xedb8_8320;
            crc = (crc >> 1) ^ polynomial;
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec;
    use std::vec::Vec;

    use super::*;

    const DISK_GUID: Guid = Guid([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    const ESP_GUID: Guid = Guid([16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1]);
    const DATA_TYPE_GUID: Guid = Guid([0x44; 16]);

    struct Fixture {
        block_size: usize,
        media_blocks: u64,
        mbr: Vec<u8>,
        header: Vec<u8>,
        entries: Vec<u8>,
        first_usable: u64,
    }

    impl Fixture {
        fn new(block_size: usize) -> Self {
            let media_blocks = 4096;
            let mut mbr = vec![0; block_size];
            mbr[510] = 0x55;
            mbr[511] = 0xaa;
            let protective = &mut mbr[MBR_PARTITIONS_OFFSET..][..MBR_PARTITION_SIZE];
            protective[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
            protective[4] = 0xee;
            protective[5..8].copy_from_slice(&[0xff; 3]);
            protective[8..12].copy_from_slice(&1u32.to_le_bytes());
            protective[12..16].copy_from_slice(&((media_blocks - 1) as u32).to_le_bytes());

            let entry_count = 128u32;
            let entry_size = 128u32;
            let mut entries = vec![0; entry_count as usize * entry_size as usize];
            let entry_blocks = entries.len().div_ceil(block_size) as u64;
            let first_usable = 2 + entry_blocks;
            let last_usable = media_blocks - entry_blocks - 2;

            Self::write_partition(
                &mut entries,
                1,
                EFI_SYSTEM_PARTITION_TYPE_GUID,
                ESP_GUID,
                first_usable + 8,
                first_usable + 71,
                1,
            );
            Self::write_partition(
                &mut entries,
                3,
                DATA_TYPE_GUID,
                Guid([0x55; 16]),
                first_usable + 80,
                first_usable + 159,
                0,
            );

            let mut header = vec![0; block_size];
            header[0..8].copy_from_slice(GPT_HEADER_SIGNATURE);
            header[8..12].copy_from_slice(&GPT_REVISION_1_0.to_le_bytes());
            header[12..16].copy_from_slice(&(GPT_HEADER_MIN_SIZE as u32).to_le_bytes());
            header[24..32].copy_from_slice(&1u64.to_le_bytes());
            header[32..40].copy_from_slice(&(media_blocks - 1).to_le_bytes());
            header[40..48].copy_from_slice(&first_usable.to_le_bytes());
            header[48..56].copy_from_slice(&last_usable.to_le_bytes());
            header[56..72].copy_from_slice(&DISK_GUID.0);
            header[72..80].copy_from_slice(&2u64.to_le_bytes());
            header[80..84].copy_from_slice(&entry_count.to_le_bytes());
            header[84..88].copy_from_slice(&entry_size.to_le_bytes());
            header[88..92].copy_from_slice(&crc32(&entries).to_le_bytes());
            Self::seal_header(&mut header);

            Self {
                block_size,
                media_blocks,
                mbr,
                header,
                entries,
                first_usable,
            }
        }

        fn write_partition(
            entries: &mut [u8],
            index: usize,
            type_guid: Guid,
            unique_guid: Guid,
            first_lba: u64,
            last_lba: u64,
            attributes: u64,
        ) {
            let entry = &mut entries[index * GPT_ENTRY_MIN_SIZE..][..GPT_ENTRY_MIN_SIZE];
            entry[0..16].copy_from_slice(&type_guid.0);
            entry[16..32].copy_from_slice(&unique_guid.0);
            entry[32..40].copy_from_slice(&first_lba.to_le_bytes());
            entry[40..48].copy_from_slice(&last_lba.to_le_bytes());
            entry[48..56].copy_from_slice(&attributes.to_le_bytes());
        }

        fn reseal_entries(&mut self) {
            self.header[88..92].copy_from_slice(&crc32(&self.entries).to_le_bytes());
            Self::seal_header(&mut self.header);
        }

        fn seal_header(header: &mut [u8]) {
            header[16..20].fill(0);
            let checksum = crc32(&header[..GPT_HEADER_MIN_SIZE]);
            header[16..20].copy_from_slice(&checksum.to_le_bytes());
        }

        fn parse_header(&self) -> Result<GptHeader, GptError> {
            GptHeader::parse_primary(&self.header, self.block_size, self.media_blocks)
        }
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn validates_gpt_and_selects_efi_system_partition() {
        let fixture = Fixture::new(512);
        validate_protective_mbr(&fixture.mbr, fixture.block_size, fixture.media_blocks).unwrap();
        let header = fixture.parse_header().unwrap();
        assert_eq!(header.disk_guid, DISK_GUID);
        assert_eq!(header.partition_entry_lba, 2);
        assert_eq!(header.partition_entry_array_byte_len(), 16_384);
        assert_eq!(header.partition_entry_array_block_count(), 32);

        let table = header.parse_partition_table(&fixture.entries).unwrap();
        let esp = table.efi_system_partition().unwrap();
        assert_eq!(esp.index, 1);
        assert_eq!(esp.type_guid, EFI_SYSTEM_PARTITION_TYPE_GUID);
        assert_eq!(esp.unique_guid, ESP_GUID);
        assert_eq!(esp.first_lba, fixture.first_usable + 8);
        assert_eq!(esp.block_count(), 64);
        assert_eq!(esp.attributes, 1);
        assert_eq!(table.partition(0), None);
        assert_eq!(table.find_by_type_guid(DATA_TYPE_GUID).unwrap().index, 3);
    }

    #[test]
    fn supports_native_4096_byte_logical_blocks() {
        let fixture = Fixture::new(4096);
        validate_protective_mbr(&fixture.mbr, fixture.block_size, fixture.media_blocks).unwrap();
        let header = fixture.parse_header().unwrap();
        assert_eq!(fixture.first_usable, 6);
        assert_eq!(header.partition_entry_array_block_count(), 4);
        assert_eq!(
            header
                .parse_partition_table(&fixture.entries)
                .unwrap()
                .efi_system_partition()
                .unwrap()
                .unique_guid,
            ESP_GUID
        );
    }

    #[test]
    fn rejects_invalid_protective_mbr() {
        let mut fixture = Fixture::new(512);
        fixture.mbr[510] = 0;
        assert_eq!(
            validate_protective_mbr(&fixture.mbr, fixture.block_size, fixture.media_blocks),
            Err(GptError::BadMbrSignature)
        );

        let mut fixture = Fixture::new(512);
        fixture.mbr[MBR_PARTITIONS_OFFSET + 4] = 0x83;
        assert_eq!(
            validate_protective_mbr(&fixture.mbr, fixture.block_size, fixture.media_blocks),
            Err(GptError::BadProtectiveMbr)
        );
    }

    #[test]
    fn rejects_bad_header_and_entry_array_checksums() {
        let mut fixture = Fixture::new(512);
        fixture.header[40] ^= 1;
        assert_eq!(fixture.parse_header(), Err(GptError::BadHeaderCrc));

        let mut fixture = Fixture::new(512);
        fixture.entries[0] ^= 1;
        let header = fixture.parse_header().unwrap();
        assert_eq!(
            header.parse_partition_table(&fixture.entries).err(),
            Some(GptError::BadEntryArrayCrc)
        );
    }

    #[test]
    fn rejects_entry_arrays_outside_primary_metadata_region() {
        let mut fixture = Fixture::new(512);
        fixture.header[72..80].copy_from_slice(&fixture.first_usable.to_le_bytes());
        Fixture::seal_header(&mut fixture.header);
        assert_eq!(fixture.parse_header(), Err(GptError::EntryArrayOutOfBounds));
    }

    #[test]
    fn rejects_invalid_and_overlapping_partitions_after_crc_validation() {
        let mut fixture = Fixture::new(512);
        fixture.entries[GPT_ENTRY_MIN_SIZE + 16..GPT_ENTRY_MIN_SIZE + 32].fill(0);
        fixture.reseal_entries();
        assert_eq!(
            fixture
                .parse_header()
                .unwrap()
                .parse_partition_table(&fixture.entries)
                .err(),
            Some(GptError::BadPartitionEntry)
        );

        let mut fixture = Fixture::new(512);
        let overlapping_first = fixture.first_usable + 64;
        let offset = 3 * GPT_ENTRY_MIN_SIZE + 32;
        fixture.entries[offset..offset + 8].copy_from_slice(&overlapping_first.to_le_bytes());
        fixture.reseal_entries();
        assert_eq!(
            fixture
                .parse_header()
                .unwrap()
                .parse_partition_table(&fixture.entries)
                .err(),
            Some(GptError::OverlappingPartitions)
        );
    }

    /// A classic MBR the way retail sticks ship it: boot code zeroed, a disk signature at 440,
    /// and one FAT32-LBA record. 0x7388800 blocks is CRUX's 62 GB "USB DISK 3.0" (J541).
    const STICK_BLOCKS: u64 = 0x738_8800;

    fn classic_mbr(records: &[(usize, u8, u8, u32, u32)]) -> Vec<u8> {
        let mut mbr = vec![0; 512];
        mbr[440..444].copy_from_slice(&0x1234_abcdu32.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xaa;
        for &(index, boot_indicator, partition_type, first_lba, block_count) in records {
            let record = &mut mbr[MBR_PARTITIONS_OFFSET + index * MBR_PARTITION_SIZE..]
                [..MBR_PARTITION_SIZE];
            record[0] = boot_indicator;
            record[1..4].copy_from_slice(&[0xfe, 0xff, 0xff]);
            record[4] = partition_type;
            record[5..8].copy_from_slice(&[0xfe, 0xff, 0xff]);
            record[8..12].copy_from_slice(&first_lba.to_le_bytes());
            record[12..16].copy_from_slice(&block_count.to_le_bytes());
        }
        mbr
    }

    #[test]
    fn a_retail_stick_is_not_gpt_but_its_fat32_partition_is_found() {
        let mbr = classic_mbr(&[(
            0,
            0x00,
            MBR_TYPE_FAT32_LBA,
            2048,
            STICK_BLOCKS as u32 - 2048,
        )]);
        assert_eq!(
            validate_protective_mbr(&mbr, 512, STICK_BLOCKS),
            Err(GptError::BadProtectiveMbr)
        );
        let table = MbrPartitionTable::parse(&mbr, 512, STICK_BLOCKS).unwrap();
        let fat = table.fat32_partition().unwrap();
        assert_eq!(fat.index, 0);
        assert_eq!(fat.partition_type, MBR_TYPE_FAT32_LBA);
        assert!(!fat.bootable);
        assert_eq!(fat.first_lba, 2048);
        assert_eq!(fat.last_lba(), STICK_BLOCKS - 1);
        assert_eq!(table.partitions().count(), 1);
        assert_eq!(table.partition(1), None);
        assert_eq!(table.partition(4), None);
    }

    #[test]
    fn the_first_fat32_typed_record_wins_in_slot_order() {
        let mbr = classic_mbr(&[
            (0, 0x80, 0x83, 2048, 4096),
            (1, 0x00, MBR_TYPE_EFI_SYSTEM, 8192, 1024),
            (3, 0x00, MBR_TYPE_FAT32_CHS, 10240, 1024),
        ]);
        let table = MbrPartitionTable::parse(&mbr, 512, 1 << 20).unwrap();
        assert!(table.partition(0).unwrap().bootable);
        assert!(!table.partition(0).unwrap().may_hold_fat32());
        assert_eq!(table.fat32_partition().unwrap().index, 1);
        assert!(table.partitions().map(|p| p.index).eq([0, 1, 3]));
    }

    #[test]
    fn a_table_with_no_fat32_record_finds_none() {
        let empty = classic_mbr(&[]);
        assert_eq!(
            MbrPartitionTable::parse(&empty, 512, STICK_BLOCKS)
                .unwrap()
                .fat32_partition(),
            None
        );
        let exfat = classic_mbr(&[(0, 0x00, 0x07, 2048, 4096)]);
        assert_eq!(
            MbrPartitionTable::parse(&exfat, 512, STICK_BLOCKS)
                .unwrap()
                .fat32_partition(),
            None
        );
    }

    #[test]
    fn a_volume_boot_sector_is_not_mistaken_for_a_partition_table() {
        // A FAT32 "superfloppy": LBA0 is the volume's own boot sector, whose boot code runs
        // through the record area.
        let mut vbr = vec![0x90; 512];
        vbr[0..3].copy_from_slice(&[0xeb, 0x58, 0x90]);
        vbr[510] = 0x55;
        vbr[511] = 0xaa;
        assert_eq!(
            MbrPartitionTable::parse(&vbr, 512, STICK_BLOCKS),
            Err(MbrError::BadBootIndicator)
        );
    }

    #[test]
    fn gpt_media_are_left_to_the_gpt_path() {
        let fixture = Fixture::new(512);
        assert_eq!(
            MbrPartitionTable::parse(&fixture.mbr, 512, fixture.media_blocks),
            Err(MbrError::ProtectiveRecord)
        );
        let hybrid = classic_mbr(&[(0, 0x00, 0xee, 1, 2047), (1, 0x00, 0x0c, 2048, 4096)]);
        assert_eq!(
            MbrPartitionTable::parse(&hybrid, 512, STICK_BLOCKS),
            Err(MbrError::ProtectiveRecord)
        );
    }

    #[test]
    fn rejects_records_outside_the_medium_and_overlaps() {
        let parse = |mbr: &[u8]| MbrPartitionTable::parse(mbr, 512, 8192);
        let mut unsigned = classic_mbr(&[]);
        unsigned[511] = 0;
        assert_eq!(parse(&unsigned), Err(MbrError::BadSignature));
        for record in [
            (0, 0, 0x0c, 0, 64),
            (0, 0, 0x0c, 64, 0),
            (0, 0, 0x0c, 8000, 193),
        ] {
            assert_eq!(
                parse(&classic_mbr(&[record])),
                Err(MbrError::BadPartitionEntry)
            );
        }
        assert!(parse(&classic_mbr(&[(0, 0, 0x0c, 8000, 192)])).is_ok());
        assert_eq!(
            parse(&classic_mbr(&[(0, 0, 0x0c, 64, 128), (2, 0, 0x83, 191, 8)])),
            Err(MbrError::OverlappingPartitions)
        );
        assert_eq!(
            MbrPartitionTable::parse(&classic_mbr(&[])[..511], 512, 8192),
            Err(MbrError::BufferTooShort)
        );
        assert_eq!(
            MbrPartitionTable::parse(&classic_mbr(&[]), 256, 8192),
            Err(MbrError::InvalidLogicalBlockSize)
        );
    }

    #[test]
    fn native_4096_byte_blocks_count_lbas_in_device_blocks() {
        let mut mbr = classic_mbr(&[(0, 0, MBR_TYPE_FAT32_LBA, 256, 1024)]);
        mbr.resize(4096, 0);
        let fat = MbrPartitionTable::parse(&mbr, 4096, 2048)
            .unwrap()
            .fat32_partition()
            .unwrap();
        assert_eq!((fat.first_lba, fat.block_count), (256, 1024));
    }

    #[test]
    fn a_formatted_sticks_data_volume_wins_over_its_esp() {
        // `diskutil eraseDisk FAT32 … GPT`, as on CRUX's stick at J543: a 200 MiB ESP at LBA 40,
        // then the Basic Data volume.
        let mut fixture = Fixture::new(512);
        Fixture::write_partition(
            &mut fixture.entries,
            2,
            MICROSOFT_BASIC_DATA_TYPE_GUID,
            Guid([0x66; 16]),
            fixture.first_usable + 200,
            fixture.first_usable + 999,
            0,
        );
        fixture.reseal_entries();
        let header = fixture.parse_header().unwrap();
        let table = header.parse_partition_table(&fixture.entries).unwrap();
        assert!(table.partitions().map(|p| p.index).eq([1, 2, 3]));
        assert_eq!(table.efi_system_partition().unwrap().index, 1);
        let data = table.data_volume().unwrap();
        assert_eq!(data.index, 2);
        assert_eq!(data.type_guid, MICROSOFT_BASIC_DATA_TYPE_GUID);
        assert_eq!(data.block_count(), 800);
    }

    #[test]
    fn without_a_data_volume_the_esp_is_the_volume() {
        let fixture = Fixture::new(512);
        let header = fixture.parse_header().unwrap();
        let table = header.parse_partition_table(&fixture.entries).unwrap();
        assert_eq!(table.data_volume().unwrap().index, 1);
    }

    #[test]
    fn basic_data_guid_is_stored_mixed_endian() {
        // EBD0A0A2-B9E5-4433-87C0-68B6B72699C7: the first three fields little-endian.
        let bytes = MICROSOFT_BASIC_DATA_TYPE_GUID.0;
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            0xebd0_a0a2
        );
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 0xb9e5);
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 0x4433);
        assert_eq!(bytes[8..], [0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99, 0xc7]);
    }

    #[test]
    fn valid_table_without_esp_returns_none() {
        let mut fixture = Fixture::new(512);
        fixture.entries[GPT_ENTRY_MIN_SIZE..GPT_ENTRY_MIN_SIZE + 16]
            .copy_from_slice(&DATA_TYPE_GUID.0);
        fixture.reseal_entries();
        let table = fixture
            .parse_header()
            .unwrap()
            .parse_partition_table(&fixture.entries)
            .unwrap();
        assert_eq!(table.efi_system_partition(), None);
    }
}
