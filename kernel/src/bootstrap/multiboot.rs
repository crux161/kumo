//! Safe, host-testable Multiboot2 handoff parsing and Multiboot1/2 memory-map normalization.

use alloc::vec::Vec;
use kumo_abi::{MemRegion, MemRegionKind, Range};

const ENTRY_PAYLOAD_MIN: usize = 20;
const LEGACY_LOW_MEMORY_END: u64 = 1 << 20;
pub const MULTIBOOT2_BOOT_MAGIC: u64 = 0x36d7_6289;

const MB2_END_TAG: u32 = 0;
const MB2_MODULE_TAG: u32 = 3;
const MB2_BASIC_MEMORY_TAG: u32 = 4;
const MB2_MEMORY_MAP_TAG: u32 = 6;
const MB2_ACPI_OLD_TAG: u32 = 14;
const MB2_ACPI_NEW_TAG: u32 = 15;
const MB2_HEADER_LEN: usize = 8;
const MB2_TAG_HEADER_LEN: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Multiboot2Error {
    Truncated,
    BadTotalSize,
    BadReserved,
    BadTagSize,
    MissingEndTag,
    BadModule,
    BadMemoryMap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Multiboot2Module<'a> {
    pub start: u64,
    pub end: u64,
    pub command_line: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Multiboot2MemoryMap<'a> {
    entries: &'a [u8],
    entry_size: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Multiboot2Info<'a> {
    bytes: &'a [u8],
}

impl<'a> Multiboot2Info<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Multiboot2Error> {
        if bytes.len() < MB2_HEADER_LEN + MB2_TAG_HEADER_LEN {
            return Err(Multiboot2Error::Truncated);
        }
        let total_size = read_u32(bytes, 0).ok_or(Multiboot2Error::Truncated)? as usize;
        if total_size != bytes.len() || total_size & 7 != 0 {
            return Err(Multiboot2Error::BadTotalSize);
        }
        if read_u32(bytes, 4) != Some(0) {
            return Err(Multiboot2Error::BadReserved);
        }

        let mut cursor = MB2_HEADER_LEN;
        while cursor < bytes.len() {
            let tag_type = read_u32(bytes, cursor).ok_or(Multiboot2Error::Truncated)?;
            let size = read_u32(bytes, cursor + 4).ok_or(Multiboot2Error::Truncated)? as usize;
            if size < MB2_TAG_HEADER_LEN {
                return Err(Multiboot2Error::BadTagSize);
            }
            let end = cursor
                .checked_add(size)
                .filter(|&end| end <= bytes.len())
                .ok_or(Multiboot2Error::Truncated)?;
            if tag_type == MB2_END_TAG {
                return if size == MB2_TAG_HEADER_LEN && end == bytes.len() {
                    Ok(Self { bytes })
                } else {
                    Err(Multiboot2Error::BadTagSize)
                };
            }
            cursor = align_up_8(end).ok_or(Multiboot2Error::BadTagSize)?;
        }
        Err(Multiboot2Error::MissingEndTag)
    }

    fn tag_payload(self, wanted: u32) -> Option<&'a [u8]> {
        let mut cursor = MB2_HEADER_LEN;
        while cursor < self.bytes.len() {
            let tag_type = read_u32(self.bytes, cursor)?;
            let size = read_u32(self.bytes, cursor + 4)? as usize;
            let end = cursor.checked_add(size)?;
            if tag_type == wanted {
                return self.bytes.get(cursor + MB2_TAG_HEADER_LEN..end);
            }
            if tag_type == MB2_END_TAG {
                break;
            }
            cursor = align_up_8(end)?;
        }
        None
    }

    fn tag_count(self, wanted: u32) -> u32 {
        let mut count = 0u32;
        let mut cursor = MB2_HEADER_LEN;
        while cursor < self.bytes.len() {
            let Some(tag_type) = read_u32(self.bytes, cursor) else {
                break;
            };
            let Some(size) = read_u32(self.bytes, cursor + 4).map(|size| size as usize) else {
                break;
            };
            if tag_type == wanted {
                count = count.saturating_add(1);
            }
            if tag_type == MB2_END_TAG {
                break;
            }
            let Some(end) = cursor.checked_add(size).and_then(align_up_8) else {
                break;
            };
            cursor = end;
        }
        count
    }

    pub fn module_count(self) -> u32 {
        self.tag_count(MB2_MODULE_TAG)
    }

    pub fn total_size(self) -> usize {
        self.bytes.len()
    }

    pub fn first_module(self) -> Result<Option<Multiboot2Module<'a>>, Multiboot2Error> {
        let Some(payload) = self.tag_payload(MB2_MODULE_TAG) else {
            return Ok(None);
        };
        let start = u64::from(read_u32(payload, 0).ok_or(Multiboot2Error::BadModule)?);
        let end = u64::from(read_u32(payload, 4).ok_or(Multiboot2Error::BadModule)?);
        // Multiboot2 permits a module to begin at physical address zero. The parser keeps
        // that metadata representable; a consumer that needs to form a Rust slice must use
        // a non-null virtual alias or reject the range before doing so.
        if start >= end {
            return Err(Multiboot2Error::BadModule);
        }
        let command_line = payload
            .get(8..)
            .ok_or(Multiboot2Error::BadModule)?
            .split(|&byte| byte == 0)
            .next()
            .unwrap_or(&[]);
        Ok(Some(Multiboot2Module {
            start,
            end,
            command_line,
        }))
    }

    pub fn memory_map(self) -> Result<Option<Multiboot2MemoryMap<'a>>, Multiboot2Error> {
        let Some(payload) = self.tag_payload(MB2_MEMORY_MAP_TAG) else {
            return Ok(None);
        };
        let entry_size = read_u32(payload, 0).ok_or(Multiboot2Error::BadMemoryMap)? as usize;
        let _entry_version = read_u32(payload, 4).ok_or(Multiboot2Error::BadMemoryMap)?;
        let entries = payload.get(8..).ok_or(Multiboot2Error::BadMemoryMap)?;
        if entry_size < 24 || entries.is_empty() || entries.len() % entry_size != 0 {
            return Err(Multiboot2Error::BadMemoryMap);
        }
        Ok(Some(Multiboot2MemoryMap {
            entries,
            entry_size,
        }))
    }

    pub fn basic_memory(self) -> Option<(u32, u32)> {
        let payload = self.tag_payload(MB2_BASIC_MEMORY_TAG)?;
        Some((read_u32(payload, 0)?, read_u32(payload, 4)?))
    }

    pub fn acpi_rsdp(self) -> Option<&'a [u8]> {
        self.tag_payload(MB2_ACPI_NEW_TAG)
            .or_else(|| self.tag_payload(MB2_ACPI_OLD_TAG))
    }
}

fn align_up_8(value: usize) -> Option<usize> {
    value.checked_add(7).map(|value| value & !7)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryMapError {
    Empty,
    Truncated,
    BadEntrySize,
    AddressOverflow,
    NoAccessibleMemory,
    NoUsableMemory,
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn push_region(regions: &mut Vec<MemRegion>, start: u64, end: u64, kind: MemRegionKind) {
    if start < end {
        regions.push(MemRegion {
            range: Range::new(start, end - start),
            kind,
            _reserved: 0,
        });
    }
}

fn push_firmware_region(
    parsed: &mut Vec<MemRegion>,
    start: u64,
    len: u64,
    firmware_kind: u32,
    accessible_limit: u64,
) -> Result<(), MemoryMapError> {
    let end = start
        .checked_add(len)
        .ok_or(MemoryMapError::AddressOverflow)?
        .min(accessible_limit);
    if len == 0 || start >= accessible_limit || start >= end {
        return Ok(());
    }
    let kind = match firmware_kind {
        1 => MemRegionKind::Usable,
        3 | 4 => MemRegionKind::Acpi,
        _ => MemRegionKind::Reserved,
    };
    if kind == MemRegionKind::Usable && start < LEGACY_LOW_MEMORY_END {
        push_region(
            parsed,
            start,
            end.min(LEGACY_LOW_MEMORY_END),
            MemRegionKind::Reserved,
        );
        push_region(
            parsed,
            start.max(LEGACY_LOW_MEMORY_END),
            end,
            MemRegionKind::Usable,
        );
    } else {
        push_region(parsed, start, end, kind);
    }
    Ok(())
}

fn normalize_parsed_regions(parsed: Vec<MemRegion>) -> Result<Vec<MemRegion>, MemoryMapError> {
    if parsed.is_empty() {
        return Err(MemoryMapError::NoAccessibleMemory);
    }
    let mut boundaries = Vec::with_capacity(parsed.len() * 2);
    for region in &parsed {
        boundaries.push(region.range.start);
        boundaries.push(region.range.end());
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let rank = |kind| match kind {
        MemRegionKind::Usable => 0,
        MemRegionKind::Acpi => 1,
        _ => 2,
    };
    let mut normalized: Vec<MemRegion> = Vec::with_capacity(boundaries.len());
    for window in boundaries.windows(2) {
        let start = window[0];
        let end = window[1];
        let Some(kind) = parsed
            .iter()
            .filter(|region| region.range.start <= start && end <= region.range.end())
            .map(|region| region.kind)
            .max_by_key(|&kind| rank(kind))
        else {
            continue;
        };
        if let Some(previous) = normalized.last_mut() {
            let previous_end = previous.range.end();
            if start == previous_end && kind == previous.kind {
                previous.range.len = end - previous.range.start;
                continue;
            }
        }
        push_region(&mut normalized, start, end, kind);
    }

    if !normalized
        .iter()
        .any(|region| region.kind == MemRegionKind::Usable)
    {
        return Err(MemoryMapError::NoUsableMemory);
    }
    Ok(normalized)
}

/// Parse Multiboot1 variable-sized memory-map entries and restrict them to a caller-selected
/// physical window. The x86 bootstrap uses this both for its low allocation view and for the
/// permanent kernel physical map.
///
/// Type-1 RAM below 1 MiB remains represented but is forced reserved: the null page, BIOS
/// data, EBDA, and real-mode firmware windows must never become general frame allocations.
/// Adjacent equal regions are coalesced. If firmware entries overlap, the most restrictive
/// kind wins (`Reserved` over `Acpi` over `Usable`), so the shared frame allocator can never
/// return the same physical frame twice or allocate a reservation hidden beneath usable RAM.
pub fn normalize_memory_map(
    bytes: &[u8],
    accessible_limit: u64,
) -> Result<Vec<MemRegion>, MemoryMapError> {
    if bytes.is_empty() {
        return Err(MemoryMapError::Empty);
    }

    let mut parsed = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let payload_size = read_u32(bytes, cursor).ok_or(MemoryMapError::Truncated)? as usize;
        if payload_size < ENTRY_PAYLOAD_MIN {
            return Err(MemoryMapError::BadEntrySize);
        }
        let entry_size = payload_size
            .checked_add(4)
            .ok_or(MemoryMapError::BadEntrySize)?;
        let entry_end = cursor
            .checked_add(entry_size)
            .filter(|&end| end <= bytes.len())
            .ok_or(MemoryMapError::Truncated)?;
        let start = read_u64(bytes, cursor + 4).ok_or(MemoryMapError::Truncated)?;
        let len = read_u64(bytes, cursor + 12).ok_or(MemoryMapError::Truncated)?;
        let firmware_kind = read_u32(bytes, cursor + 20).ok_or(MemoryMapError::Truncated)?;
        push_firmware_region(&mut parsed, start, len, firmware_kind, accessible_limit)?;
        cursor = entry_end;
    }

    normalize_parsed_regions(parsed)
}

/// Normalize a Multiboot2 memory-map tag through the same conservative region policy used for
/// Multiboot1. Extended entry bytes are ignored as required by the Multiboot2 entry-size field.
pub fn normalize_multiboot2_memory_map(
    map: Multiboot2MemoryMap<'_>,
    accessible_limit: u64,
) -> Result<Vec<MemRegion>, MemoryMapError> {
    let mut parsed = Vec::new();
    let mut cursor = 0usize;
    while cursor < map.entries.len() {
        let entry = map
            .entries
            .get(cursor..cursor + map.entry_size)
            .ok_or(MemoryMapError::Truncated)?;
        let start = read_u64(entry, 0).ok_or(MemoryMapError::Truncated)?;
        let len = read_u64(entry, 8).ok_or(MemoryMapError::Truncated)?;
        let firmware_kind = read_u32(entry, 16).ok_or(MemoryMapError::Truncated)?;
        push_firmware_region(&mut parsed, start, len, firmware_kind, accessible_limit)?;
        cursor = cursor
            .checked_add(map.entry_size)
            .ok_or(MemoryMapError::BadEntrySize)?;
    }
    normalize_parsed_regions(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(start: u64, len: u64, kind: u32) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[0..4].copy_from_slice(&20u32.to_le_bytes());
        bytes[4..12].copy_from_slice(&start.to_le_bytes());
        bytes[12..20].copy_from_slice(&len.to_le_bytes());
        bytes[20..24].copy_from_slice(&kind.to_le_bytes());
        bytes
    }

    fn mb2_entry(start: u64, len: u64, kind: u32) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[0..8].copy_from_slice(&start.to_le_bytes());
        bytes[8..16].copy_from_slice(&len.to_le_bytes());
        bytes[16..20].copy_from_slice(&kind.to_le_bytes());
        bytes
    }

    fn mb2_info(tags: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = vec![0u8; MB2_HEADER_LEN];
        for (tag_type, payload) in tags {
            let size = MB2_TAG_HEADER_LEN + payload.len();
            bytes.extend_from_slice(&tag_type.to_le_bytes());
            bytes.extend_from_slice(&(size as u32).to_le_bytes());
            bytes.extend_from_slice(payload);
            while bytes.len() & 7 != 0 {
                bytes.push(0);
            }
        }
        bytes.extend_from_slice(&MB2_END_TAG.to_le_bytes());
        bytes.extend_from_slice(&(MB2_TAG_HEADER_LEN as u32).to_le_bytes());
        let total_size = bytes.len() as u32;
        bytes[0..4].copy_from_slice(&total_size.to_le_bytes());
        bytes
    }

    #[test]
    fn multiboot2_tags_expose_module_memory_and_new_acpi() {
        let mut module = Vec::new();
        module.extend_from_slice(&0x20_0000u32.to_le_bytes());
        module.extend_from_slice(&0x21_0000u32.to_le_bytes());
        module.extend_from_slice(b"kumo-initrd\0");

        let mut memory = Vec::new();
        memory.extend_from_slice(&24u32.to_le_bytes());
        memory.extend_from_slice(&0u32.to_le_bytes());
        memory.extend_from_slice(&mb2_entry(0, 0x20_0000, 1));
        memory.extend_from_slice(&mb2_entry(0x20_0000, 0x10_0000, 3));

        let basic = [640u32.to_le_bytes(), 1024u32.to_le_bytes()].concat();
        let bytes = mb2_info(&[
            (MB2_MODULE_TAG, module),
            (MB2_BASIC_MEMORY_TAG, basic),
            (MB2_MEMORY_MAP_TAG, memory),
            (MB2_ACPI_OLD_TAG, b"old-rsdp".to_vec()),
            (MB2_ACPI_NEW_TAG, b"new-rsdp".to_vec()),
        ]);
        let info = Multiboot2Info::parse(&bytes).unwrap();
        let module = info.first_module().unwrap().unwrap();
        assert_eq!((module.start, module.end), (0x20_0000, 0x21_0000));
        assert_eq!(module.command_line, b"kumo-initrd");
        assert_eq!(info.basic_memory(), Some((640, 1024)));
        assert_eq!(info.acpi_rsdp(), Some(b"new-rsdp".as_slice()));
        assert_eq!(
            normalize_multiboot2_memory_map(info.memory_map().unwrap().unwrap(), 1 << 30).unwrap(),
            vec![
                MemRegion {
                    range: Range::new(0, 0x10_0000),
                    kind: MemRegionKind::Reserved,
                    _reserved: 0,
                },
                MemRegion {
                    range: Range::new(0x10_0000, 0x10_0000),
                    kind: MemRegionKind::Usable,
                    _reserved: 0,
                },
                MemRegion {
                    range: Range::new(0x20_0000, 0x10_0000),
                    kind: MemRegionKind::Acpi,
                    _reserved: 0,
                },
            ]
        );
    }

    #[test]
    fn multiboot2_rejects_unbounded_or_malformed_tags() {
        let valid = mb2_info(&[]);
        assert!(Multiboot2Info::parse(&valid).is_ok());

        let mut wrong_total = valid.clone();
        wrong_total[0..4].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(
            Multiboot2Info::parse(&wrong_total),
            Err(Multiboot2Error::BadTotalSize)
        );

        let mut bad_tag = valid;
        bad_tag[12..16].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(
            Multiboot2Info::parse(&bad_tag),
            Err(Multiboot2Error::BadTagSize)
        );
    }

    #[test]
    fn multiboot2_represents_a_module_at_physical_zero() {
        let mut module = Vec::new();
        module.extend_from_slice(&0u32.to_le_bytes());
        module.extend_from_slice(&0x2328u32.to_le_bytes());
        module.extend_from_slice(b"kumo-initrd\0");

        let bytes = mb2_info(&[(MB2_MODULE_TAG, module)]);
        let module = Multiboot2Info::parse(&bytes)
            .unwrap()
            .first_module()
            .unwrap()
            .unwrap();
        assert_eq!((module.start, module.end), (0, 0x2328));
    }

    #[test]
    fn clips_high_ram_and_reserves_the_legacy_megabyte() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&entry(0, 0x20_0000, 1));
        bytes.extend_from_slice(&entry(0x20_0000, 0x1000, 3));
        bytes.extend_from_slice(&entry(0x3ff0_0000, 0x20_0000, 1));
        bytes.extend_from_slice(&entry(0x1_0000_0000, 0x10_0000, 1));

        let regions = normalize_memory_map(&bytes, 1 << 30).unwrap();
        assert_eq!(
            regions,
            vec![
                MemRegion {
                    range: Range::new(0, 0x10_0000),
                    kind: MemRegionKind::Reserved,
                    _reserved: 0,
                },
                MemRegion {
                    range: Range::new(0x10_0000, 0x10_0000),
                    kind: MemRegionKind::Usable,
                    _reserved: 0,
                },
                MemRegion {
                    range: Range::new(0x20_0000, 0x1000),
                    kind: MemRegionKind::Acpi,
                    _reserved: 0,
                },
                MemRegion {
                    range: Range::new(0x3ff0_0000, 0x10_0000),
                    kind: MemRegionKind::Usable,
                    _reserved: 0,
                },
            ]
        );
    }

    #[test]
    fn accepts_extended_entries_and_resolves_overlaps_conservatively() {
        let mut extended = entry(0x10_0000, 0x20_0000, 1).to_vec();
        extended[0..4].copy_from_slice(&24u32.to_le_bytes());
        extended.extend_from_slice(&[0; 4]);
        assert_eq!(normalize_memory_map(&extended, 1 << 30).unwrap().len(), 1);

        assert_eq!(
            normalize_memory_map(&extended[..23], 1 << 30),
            Err(MemoryMapError::Truncated)
        );

        let mut overlapping = Vec::new();
        overlapping.extend_from_slice(&entry(0x10_0000, 0x20_0000, 1));
        overlapping.extend_from_slice(&entry(0x20_0000, 0x20_0000, 2));
        assert_eq!(
            normalize_memory_map(&overlapping, 1 << 30).unwrap(),
            vec![
                MemRegion {
                    range: Range::new(0x10_0000, 0x10_0000),
                    kind: MemRegionKind::Usable,
                    _reserved: 0,
                },
                MemRegion {
                    range: Range::new(0x20_0000, 0x20_0000),
                    kind: MemRegionKind::Reserved,
                    _reserved: 0,
                },
            ]
        );
    }

    #[test]
    fn rejects_bad_sizes_and_wrapping_addresses() {
        let mut undersized = entry(0x10_0000, 0x1000, 1);
        undersized[0..4].copy_from_slice(&16u32.to_le_bytes());
        assert_eq!(
            normalize_memory_map(&undersized, 1 << 30),
            Err(MemoryMapError::BadEntrySize)
        );

        let wrapping = entry(u64::MAX - 0xfff, 0x2000, 1);
        assert_eq!(
            normalize_memory_map(&wrapping, 1 << 30),
            Err(MemoryMapError::AddressOverflow)
        );
    }
}
