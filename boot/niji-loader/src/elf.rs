//! Minimal, allocation-free ELF64 loader support for the kernel image.
//!
//! Nijigumo needs exactly enough of ELF to place a static kernel at a board-chosen
//! address: the entry point and the `PT_LOAD` segments (where in the file, their
//! physical-load offsets, their linked virtual addresses, how much to copy, and how
//! much to zero). Symbols and debug info are not parsed. Everything is bounds-checked
//! against the input slice so a malformed image yields an error instead of a wild read.

/// `e_machine` value for AArch64.
pub const EM_AARCH64: u16 = 0xB7;
/// `e_machine` value for x86-64 (so the symmetric backend can reuse this parser).
pub const EM_X86_64: u16 = 0x3E;

const ET_EXEC: u16 = 2;
const PT_LOAD: u32 = 1;
const EHDR_LEN: usize = 64;
const PHDR_LEN: usize = 56;
const SHDR_LEN: usize = 64;
const RELA_LEN: usize = 24;
const SHT_RELA: u32 = 4;

/// AArch64 relocation types Nijigumo rebases when it relocates a statically-linked
/// kernel (`--emit-relocs`) to a board-chosen physical address. Only absolute
/// relocations need adjusting; PC-relative ones (adrp/add/branches) move with the
/// code and are left untouched.
pub const R_AARCH64_ABS64: u32 = 257;
pub const R_AARCH64_ABS32: u32 = 258;

/// The most `PT_LOAD` segments we will record (the kernel has 3; this is slack).
pub const MAX_LOAD_SEGMENTS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoadSegment {
    pub file_offset: u64,
    pub file_size: u64,
    pub phys_addr: u64,
    pub virt_addr: u64,
    pub mem_size: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Elf64Image {
    pub entry: u64,
    pub machine: u16,
    /// Lowest `p_paddr` across all `PT_LOAD` segments.
    pub load_base: u64,
    /// Highest `p_paddr + p_memsz` across all `PT_LOAD` segments.
    pub load_end: u64,
    /// Lowest `p_vaddr` across all `PT_LOAD` segments.
    pub virt_base: u64,
    /// Highest `p_vaddr + p_memsz` across all `PT_LOAD` segments.
    pub virt_end: u64,
    segments: [LoadSegment; MAX_LOAD_SEGMENTS],
    segment_count: usize,
}

impl Elf64Image {
    pub fn segments(&self) -> &[LoadSegment] {
        &self.segments[..self.segment_count]
    }

    /// Total physical span the image occupies, in bytes.
    pub fn load_span(&self) -> u64 {
        self.load_end.saturating_sub(self.load_base)
    }

    /// Total virtual span occupied by the image.
    pub fn virt_span(&self) -> u64 {
        self.virt_end.saturating_sub(self.virt_base)
    }

    /// Translate a linked virtual address into its corresponding image offset.
    pub fn virt_offset(&self, addr: u64) -> Option<u64> {
        addr.checked_sub(self.virt_base)
            .filter(|offset| *offset < self.virt_span())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfError {
    TooSmall,
    BadMagic,
    NotElf64,
    NotLittleEndian,
    NotExecutable,
    BadProgramHeaderSize,
    BadSectionHeaderSize,
    TooManySegments,
    NoLoadSegments,
    SegmentOutOfFile,
}

/// Parse the ELF64 header and `PT_LOAD` segments of a statically-linked image.
pub fn parse_elf64(bytes: &[u8]) -> Result<Elf64Image, ElfError> {
    if bytes.len() < EHDR_LEN {
        return Err(ElfError::TooSmall);
    }
    if &bytes[0..4] != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if bytes[4] != 2 {
        return Err(ElfError::NotElf64); // EI_CLASS != ELFCLASS64
    }
    if bytes[5] != 1 {
        return Err(ElfError::NotLittleEndian); // EI_DATA != ELFDATA2LSB
    }

    let e_type = read_u16(bytes, 16)?;
    if e_type != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    let machine = read_u16(bytes, 18)?;
    let entry = read_u64(bytes, 24)?;
    let phoff = read_u64(bytes, 32)? as usize;
    let phentsize = read_u16(bytes, 54)? as usize;
    let phnum = read_u16(bytes, 56)? as usize;

    if phentsize != PHDR_LEN {
        return Err(ElfError::BadProgramHeaderSize);
    }

    let mut segments = [LoadSegment {
        file_offset: 0,
        file_size: 0,
        phys_addr: 0,
        virt_addr: 0,
        mem_size: 0,
    }; MAX_LOAD_SEGMENTS];
    let mut segment_count = 0usize;
    let mut load_base = u64::MAX;
    let mut load_end = 0u64;
    let mut virt_base = u64::MAX;
    let mut virt_end = 0u64;

    for index in 0..phnum {
        let base = phoff
            .checked_add(
                index
                    .checked_mul(PHDR_LEN)
                    .ok_or(ElfError::SegmentOutOfFile)?,
            )
            .ok_or(ElfError::SegmentOutOfFile)?;
        if base + PHDR_LEN > bytes.len() {
            return Err(ElfError::SegmentOutOfFile);
        }

        if read_u32(bytes, base)? != PT_LOAD {
            continue;
        }

        let file_offset = read_u64(bytes, base + 8)?;
        let virt_addr = read_u64(bytes, base + 16)?;
        let phys_addr = read_u64(bytes, base + 24)?;
        let file_size = read_u64(bytes, base + 32)?;
        let mem_size = read_u64(bytes, base + 40)?;

        // The bytes we copy must actually be inside the file.
        let file_end = file_offset
            .checked_add(file_size)
            .ok_or(ElfError::SegmentOutOfFile)?;
        if file_end > bytes.len() as u64 {
            return Err(ElfError::SegmentOutOfFile);
        }

        if segment_count >= MAX_LOAD_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }
        segments[segment_count] = LoadSegment {
            file_offset,
            file_size,
            phys_addr,
            virt_addr,
            mem_size,
        };
        segment_count += 1;

        if phys_addr < load_base {
            load_base = phys_addr;
        }
        let seg_end = phys_addr.saturating_add(mem_size);
        if seg_end > load_end {
            load_end = seg_end;
        }
        if virt_addr < virt_base {
            virt_base = virt_addr;
        }
        let virt_seg_end = virt_addr.saturating_add(mem_size);
        if virt_seg_end > virt_end {
            virt_end = virt_seg_end;
        }
    }

    if segment_count == 0 {
        return Err(ElfError::NoLoadSegments);
    }

    Ok(Elf64Image {
        entry,
        machine,
        load_base,
        load_end,
        virt_base,
        virt_end,
        segments,
        segment_count,
    })
}

/// Visit every relocation (from `--emit-relocs` `SHT_RELA` sections) whose target
/// `r_offset` lands inside the loaded image `[load_base, load_end)`. The callback
/// receives `(r_offset, r_type)`; the caller decides which types to apply (absolute
/// ones) and which to ignore (PC-relative ones move with the code). Relocations in
/// non-loaded sections (e.g. `.rela.debug_*`) are skipped by the range filter.
///
/// Returns the number of in-range relocations visited, or an error on a malformed
/// section table. An image with no section headers yields `Ok(0)`.
pub fn for_each_load_reloc(
    bytes: &[u8],
    load_base: u64,
    load_end: u64,
    mut visit: impl FnMut(u64, u32),
) -> Result<usize, ElfError> {
    if bytes.len() < EHDR_LEN {
        return Err(ElfError::TooSmall);
    }
    let shoff = read_u64(bytes, 40)? as usize;
    let shentsize = read_u16(bytes, 58)? as usize;
    let shnum = read_u16(bytes, 60)? as usize;
    if shoff == 0 || shnum == 0 {
        return Ok(0);
    }
    if shentsize < SHDR_LEN {
        return Err(ElfError::BadSectionHeaderSize);
    }

    let mut visited = 0usize;
    for index in 0..shnum {
        let sh = shoff
            .checked_add(
                index
                    .checked_mul(shentsize)
                    .ok_or(ElfError::SegmentOutOfFile)?,
            )
            .ok_or(ElfError::SegmentOutOfFile)?;
        if sh + SHDR_LEN > bytes.len() {
            return Err(ElfError::SegmentOutOfFile);
        }
        if read_u32(bytes, sh + 4)? != SHT_RELA {
            continue;
        }

        let rela_off = read_u64(bytes, sh + 24)? as usize;
        let rela_size = read_u64(bytes, sh + 32)? as usize;
        let rela_ent = read_u64(bytes, sh + 56)? as usize;
        if rela_ent < RELA_LEN {
            return Err(ElfError::BadSectionHeaderSize);
        }
        let count = rela_size / rela_ent;
        for entry in 0..count {
            let base = rela_off
                .checked_add(
                    entry
                        .checked_mul(rela_ent)
                        .ok_or(ElfError::SegmentOutOfFile)?,
                )
                .ok_or(ElfError::SegmentOutOfFile)?;
            if base + RELA_LEN > bytes.len() {
                return Err(ElfError::SegmentOutOfFile);
            }
            let r_offset = read_u64(bytes, base)?;
            let r_info = read_u64(bytes, base + 8)?;
            let r_type = (r_info & 0xffff_ffff) as u32;
            if r_offset >= load_base && r_offset < load_end {
                visit(r_offset, r_type);
                visited += 1;
            }
        }
    }
    Ok(visited)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
    let end = offset.checked_add(2).ok_or(ElfError::SegmentOutOfFile)?;
    if end > bytes.len() {
        return Err(ElfError::TooSmall);
    }
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
    let end = offset.checked_add(4).ok_or(ElfError::SegmentOutOfFile)?;
    if end > bytes.len() {
        return Err(ElfError::TooSmall);
    }
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ElfError> {
    let end = offset.checked_add(8).ok_or(ElfError::SegmentOutOfFile)?;
    if end > bytes.len() {
        return Err(ElfError::TooSmall);
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[offset..end]);
    Ok(u64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal ELF64 (header + one PT_LOAD program header) in a fixed
    /// buffer — no allocator needed.
    fn synthetic_elf() -> [u8; EHDR_LEN + PHDR_LEN] {
        let mut buf = [0u8; EHDR_LEN + PHDR_LEN];
        buf[0..4].copy_from_slice(b"\x7fELF");
        buf[4] = 2; // ELFCLASS64
        buf[5] = 1; // little-endian
        buf[6] = 1; // version
        put_u16(&mut buf, 16, ET_EXEC);
        put_u16(&mut buf, 18, EM_AARCH64);
        put_u64(&mut buf, 24, 0xffff_8000_4800_0000); // e_entry
        put_u64(&mut buf, 32, EHDR_LEN as u64); // e_phoff
        put_u16(&mut buf, 54, PHDR_LEN as u16); // e_phentsize
        put_u16(&mut buf, 56, 1); // e_phnum

        let ph = EHDR_LEN;
        put_u32(&mut buf, ph, PT_LOAD);
        put_u64(&mut buf, ph + 8, 0x1000); // p_offset
        put_u64(&mut buf, ph + 16, 0xffff_8000_4800_0000); // p_vaddr
        put_u64(&mut buf, ph + 24, 0x4800_0000); // p_paddr
        put_u64(&mut buf, ph + 32, 0x20); // p_filesz (kept inside this buffer? no — see test)
        put_u64(&mut buf, ph + 40, 0x1_0000); // p_memsz
        buf
    }

    fn put_u16(buf: &mut [u8], offset: usize, value: u16) {
        buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn put_u64(buf: &mut [u8], offset: usize, value: u64) {
        buf[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn parses_entry_and_single_load_segment() {
        // Give the file enough length to cover p_offset + p_filesz.
        let mut bytes = [0u8; 0x1020];
        let header = synthetic_elf();
        bytes[..header.len()].copy_from_slice(&header);

        let image = parse_elf64(&bytes).unwrap();
        assert_eq!(image.entry, 0xffff_8000_4800_0000);
        assert_eq!(image.machine, EM_AARCH64);
        assert_eq!(image.segments().len(), 1);
        let seg = image.segments()[0];
        assert_eq!(seg.file_offset, 0x1000);
        assert_eq!(seg.file_size, 0x20);
        assert_eq!(seg.phys_addr, 0x4800_0000);
        assert_eq!(seg.mem_size, 0x1_0000);
        assert_eq!(image.load_base, 0x4800_0000);
        assert_eq!(image.load_end, 0x4801_0000);
        assert_eq!(image.load_span(), 0x1_0000);
        assert_eq!(image.virt_base, 0xffff_8000_4800_0000);
        assert_eq!(image.virt_end, 0xffff_8000_4801_0000);
        assert_eq!(image.virt_span(), 0x1_0000);
        assert_eq!(image.virt_offset(image.entry), Some(0));
    }

    #[test]
    fn rejects_non_elf_and_truncated() {
        assert_eq!(parse_elf64(&[0u8; 8]), Err(ElfError::TooSmall));
        let mut not_elf = [0u8; EHDR_LEN];
        not_elf[0] = b'M';
        assert_eq!(parse_elf64(&not_elf), Err(ElfError::BadMagic));
    }

    #[test]
    fn rejects_segment_running_past_end_of_file() {
        // Header claims a 0x20-byte segment at offset 0x1000, but the file is short.
        let header = synthetic_elf();
        assert_eq!(parse_elf64(&header), Err(ElfError::SegmentOutOfFile));
    }

    #[test]
    fn visits_only_in_range_relocs_with_types() {
        // ehdr + 2 section headers (null + one SHT_RELA) + 3 RELA entries.
        const SH0: usize = EHDR_LEN; // section header table base
        const SH1: usize = SH0 + SHDR_LEN;
        const RELA0: usize = SH1 + SHDR_LEN;
        let mut buf = [0u8; RELA0 + 3 * RELA_LEN];

        buf[0..4].copy_from_slice(b"\x7fELF");
        buf[4] = 2;
        buf[5] = 1;
        put_u64(&mut buf, 40, SH0 as u64); // e_shoff
        put_u16(&mut buf, 58, SHDR_LEN as u16); // e_shentsize
        put_u16(&mut buf, 60, 2); // e_shnum

        // Section 1: SHT_RELA covering the three entries.
        put_u32(&mut buf, SH1 + 4, SHT_RELA);
        put_u64(&mut buf, SH1 + 24, RELA0 as u64); // sh_offset
        put_u64(&mut buf, SH1 + 32, (3 * RELA_LEN) as u64); // sh_size
        put_u64(&mut buf, SH1 + 56, RELA_LEN as u64); // sh_entsize

        // In-range ABS64, in-range PC-relative, out-of-range ABS64.
        put_u64(&mut buf, RELA0, 0x4800_0010);
        put_u64(&mut buf, RELA0 + 8, R_AARCH64_ABS64 as u64);
        put_u64(&mut buf, RELA0 + RELA_LEN, 0x4800_0020);
        put_u64(&mut buf, RELA0 + RELA_LEN + 8, 275); // ADR_PREL_PG_HI21 (PC-relative)
        put_u64(&mut buf, RELA0 + 2 * RELA_LEN, 0x0000_1000); // out of range
        put_u64(&mut buf, RELA0 + 2 * RELA_LEN + 8, R_AARCH64_ABS64 as u64);

        let mut seen: [(u64, u32); 4] = [(0, 0); 4];
        let mut n = 0;
        let count = for_each_load_reloc(&buf, 0x4800_0000, 0x4801_0000, |off, ty| {
            seen[n] = (off, ty);
            n += 1;
        })
        .unwrap();

        assert_eq!(count, 2);
        assert_eq!(seen[0], (0x4800_0010, R_AARCH64_ABS64));
        assert_eq!(seen[1], (0x4800_0020, 275));
    }

    #[test]
    fn no_section_headers_means_no_relocs() {
        let mut bytes = [0u8; 0x1020];
        let header = synthetic_elf();
        bytes[..header.len()].copy_from_slice(&header);
        // synthetic_elf leaves e_shoff = 0.
        let count = for_each_load_reloc(&bytes, 0, u64::MAX, |_, _| panic!("unexpected")).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn rejects_32bit_class() {
        let mut bytes = [0u8; 0x1020];
        let header = synthetic_elf();
        bytes[..header.len()].copy_from_slice(&header);
        bytes[4] = 1; // ELFCLASS32
        assert_eq!(parse_elf64(&bytes), Err(ElfError::NotElf64));
    }
}
