//j918
//! SH7-e1 (PLAN/022, cut W1): VFAT long names on the write side.
//!
//! J548 taught the volume to *read* long names; every mutation still spoke 8.3 only, walked its
//! parents by 8.3 name and never grew a directory. A Linux program writes names like
//! `rmeta3NYqFZ/full.rmeta` and `add.add.470e2b1de9f3f846-cgu.0.rcgu.o` (rustc), so this module
//! gives creation, removal and rename what Windows and Linux vfat do:
//!
//! * a component that is exactly a valid upper-case 8.3 name keeps a plain short entry (every
//!   name the image builder and the SH3 tests write stays byte-for-byte as it was);
//! * any other component gets VFAT long-name slots spelling it in UTF-16 plus a unique
//!   `BASIS~N.EXT` short alias whose checksum the slots carry;
//! * interior path components resolve by long or 8.3 name, as `resolve_path` does;
//! * a directory with no run of free slots long enough grows by one zeroed cluster;
//! * removal and rename retire an entry's long-name slots with its short entry.
//!
//! Names compare without ASCII case, as FAT's do. — TEAL

use super::*;

/// Bytes a short-name character may be (besides `A-Z` and `0-9`), per the FAT specification.
const SHORT_EXTRA: &[u8] = b"$%'-_@~`!(){}^#&";
/// Characters no FAT name may contain.
const FAT_ILLEGAL: &[u8] = b"\"*/:<>?\\|";
/// Byte offsets of the 13 UTF-16LE units inside a long-name slot.
const LFN_UNIT_AT: [usize; LFN_UNITS_PER_SLOT] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

fn is_short_char(byte: u8) -> bool {
    byte.is_ascii_uppercase() || byte.is_ascii_digit() || SHORT_EXTRA.contains(&byte)
}

/// `component` as a plain short entry, when it is exactly one: a valid 8.3 name written in
/// upper case. Anything else (lower case, a long base or extension, two dots) is a long name.
pub fn exact_short_name(component: &[u8]) -> Option<[u8; 11]> {
    let name = name_to_83(component)?;
    component
        .iter()
        .all(|&byte| byte == b'.' || is_short_char(byte))
        .then_some(name)
}

/// A long name in UTF-16: the units and how many are used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongNameUnits {
    pub units: [u16; LONG_NAME_UNITS],
    pub len: usize,
}

/// Encode a path component as a VFAT long name: valid UTF-8, at most 255 UTF-16 units, no
/// control character and none of `"*/:<>?\|`, not `.`/`..`, not ending in a space or a dot
/// (Windows strips those, so FAT never stores them). `None` for anything else.
pub fn long_name_units(component: &[u8]) -> Option<LongNameUnits> {
    if component.is_empty() || component == b"." || component == b".." {
        return None;
    }
    if matches!(component.last(), Some(b' ' | b'.')) {
        return None;
    }
    let text = core::str::from_utf8(component).ok()?;
    let mut out = LongNameUnits {
        units: [0; LONG_NAME_UNITS],
        len: 0,
    };
    for ch in text.chars() {
        if (ch as u32) < 0x20 || (ch.is_ascii() && FAT_ILLEGAL.contains(&(ch as u8))) {
            return None;
        }
        let mut pair = [0u16; 2];
        for unit in ch.encode_utf16(&mut pair) {
            *out.units.get_mut(out.len)? = *unit;
            out.len += 1;
        }
    }
    Some(out)
}

/// The basis of a long name's short alias, before its `~N`: upper-cased, characters a short name
/// cannot hold become `_` (one per character), spaces and every dot but the last are dropped; the
/// base keeps 8 characters and the extension 3. An empty base becomes `_`.
pub fn alias_basis(component: &[u8]) -> ([u8; 8], usize, [u8; 3], usize) {
    let trimmed = {
        let start = component
            .iter()
            .position(|&b| b != b'.')
            .unwrap_or(component.len());
        &component[start..]
    };
    let (base, ext): (&[u8], &[u8]) = match trimmed.iter().rposition(|&b| b == b'.') {
        Some(at) => (&trimmed[..at], &trimmed[at + 1..]),
        None => (trimmed, b""),
    };
    fn fold(part: &[u8], out: &mut [u8]) -> usize {
        let mut n = 0;
        for &byte in part {
            if n == out.len() {
                break;
            }
            let mapped = match byte {
                b' ' | b'.' => continue,
                // A UTF-8 continuation byte belongs to a character already counted.
                0x80..=0xBF => continue,
                b if is_short_char(b.to_ascii_uppercase()) => b.to_ascii_uppercase(),
                _ => b'_',
            };
            out[n] = mapped;
            n += 1;
        }
        n
    }
    let mut base_out = [b' '; 8];
    let mut ext_out = [b' '; 3];
    let mut base_len = fold(base, &mut base_out);
    let ext_len = fold(ext, &mut ext_out);
    if base_len == 0 {
        base_out[0] = b'_';
        base_len = 1;
    }
    (base_out, base_len, ext_out, ext_len)
}

/// The `n`th alias for a basis: `BASE~N.EXT`, the base cut so the whole stays 8 characters.
pub fn alias_name(base: &[u8; 8], base_len: usize, ext: &[u8; 3], n: u32) -> [u8; 11] {
    let mut digits = [0u8; 10];
    let mut count = 0;
    let mut value = n;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let keep = base_len.min(8 - 1 - count);
    let mut name = [b' '; 11];
    name[..keep].copy_from_slice(&base[..keep]);
    name[keep] = b'~';
    for i in 0..count {
        name[keep + 1 + i] = digits[count - 1 - i];
    }
    name[8..11].copy_from_slice(ext);
    name
}

/// The 32 bytes of long-name slot `index` (0-based, in name order) of a name spread over
/// `count` slots. The name ends with a 0x0000 unit and 0xFFFF padding, unless it fills its last
/// slot exactly; the slot holding the end carries the 0x40 "last" flag.
pub fn long_slot_bytes(
    name: &LongNameUnits,
    index: usize,
    count: usize,
    checksum: u8,
) -> [u8; DIR_ENTRY_SIZE] {
    let mut raw = [0u8; DIR_ENTRY_SIZE];
    raw[0] = (index as u8 + 1) | if index + 1 == count { LFN_LAST_SLOT } else { 0 };
    raw[11] = attr::LONG_NAME;
    raw[13] = checksum;
    for (k, &at) in LFN_UNIT_AT.iter().enumerate() {
        let position = index * LFN_UNITS_PER_SLOT + k;
        let unit = match position.cmp(&name.len) {
            core::cmp::Ordering::Less => name.units[position],
            core::cmp::Ordering::Equal => 0x0000,
            core::cmp::Ordering::Greater => 0xFFFF,
        };
        raw[at..at + 2].copy_from_slice(&unit.to_le_bytes());
    }
    raw
}

/// The long-name slots in front of a short entry, as found on disk.
#[derive(Debug, Clone, Copy)]
pub struct SlotRun {
    pub at: [DirLocation; LFN_MAX_SLOTS],
    pub len: usize,
}

impl SlotRun {
    const EMPTY: SlotRun = SlotRun {
        at: [DirLocation {
            sector: 0,
            offset: 0,
        }; LFN_MAX_SLOTS],
        len: 0,
    };

    fn push(&mut self, location: DirLocation) {
        if self.len < LFN_MAX_SLOTS {
            self.at[self.len] = location;
            self.len += 1;
        }
    }
}

impl FatVolume {
    /// The directory holding the last component of `path`, and that component. Interior
    /// components resolve by long or 8.3 name and must be directories.
    pub(crate) fn parent_dir<'p, R: SectorReader>(
        &self,
        io: &mut R,
        path: &'p [u8],
    ) -> Result<(u32, &'p [u8]), FatWriteError> {
        let mut components = path
            .split(|&byte| byte == b'/')
            .filter(|component| !component.is_empty())
            .peekable();
        let mut directory = self.bpb.root_cluster;
        loop {
            let component = components.next().ok_or(FatWriteError::Invalid)?;
            if components.peek().is_none() {
                if component == b"." || component == b".." {
                    return Err(FatWriteError::Invalid);
                }
                return Ok((directory, component));
            }
            let parent = self
                .find_named_in_dir(io, directory, component)
                .ok_or(FatWriteError::Invalid)?;
            if !parent.is_dir() {
                return Err(FatWriteError::Invalid);
            }
            directory = parent.first_cluster;
        }
    }

    /// [`Self::find_named_in_dir`] plus the locations of the long-name slots the entry owns
    /// (none for a plain short entry).
    pub(crate) fn find_with_slots<R: SectorReader>(
        &self,
        io: &mut R,
        dir_cluster: u32,
        component: &[u8],
    ) -> Option<(DirEntry, SlotRun)> {
        if component.is_empty() || component == b"." || component == b".." {
            return None;
        }
        let short = name_to_83(component);
        let mut long = LongNameBuilder::new();
        let mut pending = SlotRun::EMPTY;
        let mut found = None;
        self.walk_dir_slots(io, dir_cluster, |slot, location| match slot {
            DirSlot::LongName(slot) => {
                if slot.last {
                    pending = SlotRun::EMPTY;
                }
                pending.push(location);
                long.push(&slot);
                true
            }
            DirSlot::File(mut entry) => {
                entry.location = Some(location);
                let name = EntryName::new(&entry, long.finish(&entry.name));
                if short == Some(entry.name)
                    || (name.is_long() && name.as_bytes().eq_ignore_ascii_case(component))
                {
                    let run = if name.is_long() {
                        pending
                    } else {
                        SlotRun::EMPTY
                    };
                    found = Some((entry, run));
                    return false;
                }
                pending = SlotRun::EMPTY;
                true
            }
            _ => {
                long.reset();
                pending = SlotRun::EMPTY;
                true
            }
        });
        found
    }

    /// `count` consecutive free slots in the directory at `dir_cluster`, in directory order. A
    /// directory whose chain has no such run grows by one zeroed cluster (a fresh cluster is all
    /// free slots, and a run may continue into it from the old end).
    pub(crate) fn free_run<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        dir_cluster: u32,
        count: usize,
    ) -> Result<[DirLocation; LFN_MAX_SLOTS + 1], FatWriteError> {
        let mut run = [DirLocation {
            sector: 0,
            offset: 0,
        }; LFN_MAX_SLOTS + 1];
        if count == 0 || count > run.len() {
            return Err(FatWriteError::Invalid);
        }
        let mut have = 0usize;
        let limit = self.cluster_limit();
        let mut cluster = dir_cluster;
        for _ in 0..limit {
            let first = self.bpb.cluster_to_sector(cluster);
            for sector_index in 0..self.bpb.sectors_per_cluster as u32 {
                let lba = first + sector_index;
                let mut sector = [0u8; SECTOR_SIZE];
                if !io.read_sector(lba, &mut sector) {
                    return Err(FatWriteError::Io);
                }
                for offset in (0..SECTOR_SIZE).step_by(DIR_ENTRY_SIZE) {
                    if sector[offset] == 0 || sector[offset] == 0xE5 {
                        run[have] = DirLocation {
                            sector: lba,
                            offset: offset as u16,
                        };
                        have += 1;
                        if have == count {
                            return Ok(run);
                        }
                    } else {
                        have = 0;
                    }
                }
            }
            cluster = match self.next_link(io, cluster) {
                Some(FatEntry::Next(next)) => next,
                Some(FatEntry::End) => {
                    // Grow: link a zeroed cluster after the last one.
                    let grown = self.find_free_cluster(io)?;
                    self.write_fat_entry(io, grown, FAT32_EOF_MIN)?;
                    self.zero_cluster(io, grown)?;
                    self.write_fat_entry(io, cluster, grown)?;
                    grown
                }
                _ => return Err(FatWriteError::Io),
            };
        }
        Err(FatWriteError::Io)
    }

    /// Write 32 raw bytes at `location`.
    fn write_slot<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        location: DirLocation,
        raw: &[u8; DIR_ENTRY_SIZE],
    ) -> Result<(), FatWriteError> {
        let offset = location.offset as usize;
        if offset + DIR_ENTRY_SIZE > SECTOR_SIZE {
            return Err(FatWriteError::Invalid);
        }
        let mut sector = [0u8; SECTOR_SIZE];
        if !io.read_sector(location.sector, &mut sector) {
            return Err(FatWriteError::Io);
        }
        sector[offset..offset + DIR_ENTRY_SIZE].copy_from_slice(raw);
        if !io.write_sector(location.sector, &sector) {
            return Err(FatWriteError::Io);
        }
        Ok(())
    }

    /// A new entry named `component` in the directory at `dir_cluster`: a plain short entry
    /// for an exact 8.3 name, else long-name slots and a unique short alias. The caller has
    /// checked that no entry by that name exists.
    pub(crate) fn write_named<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        dir_cluster: u32,
        component: &[u8],
        attr: u8,
        first_cluster: u32,
        size: u32,
    ) -> Result<DirEntry, FatWriteError> {
        let (short, long) = match exact_short_name(component) {
            Some(short) => (short, None),
            None => {
                let long = long_name_units(component).ok_or(FatWriteError::Invalid)?;
                let (base, base_len, ext, _) = alias_basis(component);
                let mut alias = None;
                for n in 1..=999_999u32 {
                    let candidate = alias_name(&base, base_len, &ext, n);
                    if self.find_in_dir(io, dir_cluster, &candidate).is_none() {
                        alias = Some(candidate);
                        break;
                    }
                }
                (alias.ok_or(FatWriteError::NoSpace)?, Some(long))
            }
        };
        let slots = long.map_or(0, |long| long.len.div_ceil(LFN_UNITS_PER_SLOT));
        let run = self.free_run(io, dir_cluster, slots + 1)?;
        if let Some(long) = long {
            let checksum = short_name_checksum(&short);
            for i in 0..slots {
                // The slot holding the name's end comes first on disk.
                let raw = long_slot_bytes(&long, slots - 1 - i, slots, checksum);
                self.write_slot(io, run[i], &raw)?;
            }
        }
        let location = run[slots];
        self.write_entry_at(io, location, &short, attr, first_cluster, size)?;
        Ok(DirEntry {
            name: short,
            attr,
            first_cluster,
            size,
            location: Some(location),
        })
    }

    /// Retire an entry's long-name slots and then its short entry.
    pub(crate) fn clear_with_slots<R: SectorReader + SectorWriter>(
        &self,
        io: &mut R,
        entry: &DirEntry,
        run: &SlotRun,
    ) -> Result<(), FatWriteError> {
        for location in &run.at[..run.len] {
            self.clear_entry(io, *location)?;
        }
        self.clear_entry(io, entry.location.ok_or(FatWriteError::Invalid)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_upper_case_short_names_stay_short() {
        assert_eq!(exact_short_name(b"KILO.TXT"), Some(*b"KILO    TXT"));
        assert_eq!(exact_short_name(b"WORK"), Some(*b"WORK       "));
        assert_eq!(exact_short_name(b"A-B_C~1.$$$"), Some(*b"A-B_C~1 $$$"));
        // Lower case, a long base or extension, two dots, a space: long names.
        for long in [
            &b"data.bin"[..],
            b"Write-Probe",
            b"full.rmeta",
            b"rmeta3NYqFZ",
            b"A.B.C",
            b"MY FILE",
            b"a+b",
        ] {
            assert_eq!(exact_short_name(long), None, "{:?}", long);
        }
    }

    #[test]
    fn long_names_encode_to_utf16_and_refuse_what_fat_cannot_hold() {
        let name = long_name_units("añb€😀".as_bytes()).unwrap();
        assert_eq!(
            &name.units[..name.len],
            &[0x61, 0xF1, 0x62, 0x20AC, 0xD83D, 0xDE00]
        );
        for bad in [
            &b""[..],
            b".",
            b"..",
            b"a:b",
            b"what?",
            b"tab\there",
            b"trailing.",
            b"trailing ",
            &[0xFF, 0x41][..],
        ] {
            assert_eq!(long_name_units(bad), None, "{:?}", bad);
        }
        assert!(long_name_units(&[b'a'; 255]).is_some());
        assert!(long_name_units(&[b'a'; 256]).is_none());
    }

    #[test]
    fn aliases_follow_the_vfat_basis_rules() {
        let (base, base_len, ext, ext_len) = alias_basis(b"add.add.470e2b1de9f3f846-cgu.0.rcgu.o");
        assert_eq!(
            (&base[..base_len], &ext[..ext_len]),
            (&b"ADDADD47"[..], &b"O"[..])
        );
        assert_eq!(alias_name(&base, base_len, &ext, 1), *b"ADDADD~1O  ");
        assert_eq!(alias_name(&base, base_len, &ext, 42), *b"ADDAD~42O  ");
        let (base, base_len, ext, ext_len) = alias_basis(b"full.rmeta");
        assert_eq!(
            (&base[..base_len], &ext[..ext_len]),
            (&b"FULL"[..], &b"RME"[..])
        );
        assert_eq!(alias_name(&base, base_len, &ext, 1), *b"FULL~1  RME");
        let (base, base_len, _, ext_len) = alias_basis(b".hidden");
        assert_eq!((&base[..base_len], ext_len), (&b"HIDDEN"[..], 0));
        let (base, base_len, _, _) = alias_basis("ñ+x".as_bytes());
        assert_eq!(&base[..base_len], b"__X");
        let (base, base_len, _, _) = alias_basis(b"...");
        assert_eq!(&base[..base_len], b"_");
    }

    #[test]
    fn long_slots_carry_order_flags_checksum_and_padding() {
        let name = long_name_units(b"full.rmeta").unwrap(); // 10 units: one slot
        let raw = long_slot_bytes(&name, 0, 1, 0x5A);
        assert_eq!(raw[0], 0x41);
        assert_eq!((raw[11], raw[12], raw[13]), (attr::LONG_NAME, 0, 0x5A));
        assert_eq!(u16_le(&raw, 26), 0);
        let slot = match parse_dir_entry(&raw) {
            DirSlot::LongName(slot) => slot,
            other => panic!("{other:?}"),
        };
        assert_eq!(&slot.units[..10], &name.units[..10]);
        assert_eq!(slot.units[10], 0x0000);
        assert_eq!(&slot.units[11..], &[0xFFFF, 0xFFFF]);
        // Exactly 13 units: no terminator, no padding; 14 units: two slots.
        let thirteen = long_name_units(b"abcdefghijklm").unwrap();
        assert!(!long_slot_bytes(&thirteen, 0, 1, 0)[1..32].contains(&0xFF));
        let fourteen = long_name_units(b"abcdefghijklmn").unwrap();
        assert_eq!(long_slot_bytes(&fourteen, 0, 2, 0)[0], 0x01);
        assert_eq!(long_slot_bytes(&fourteen, 1, 2, 0)[0], 0x42);
    }
}
