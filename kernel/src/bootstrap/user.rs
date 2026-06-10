//! Stage-A userspace bootstrap planning.
//!
//! This is the host-testable half of "load Sora from the initrd": given an initrd image
//! range, build the first process object and the VMAR mappings the HAL page-table layer
//! will later materialize before entering EL0.

use alloc::vec::Vec;

use kumo_abi::{find_file, InitrdError, Range, SORA_INIT_PATH};
use kumo_hal::PageFlags;

use crate::mm::{Mapping, MemoryError, Vmar, Vmo, PAGE_SIZE};
use crate::object::ObjectManager;
use crate::task::{Job, Process};

pub const USER_ROOT_BASE: u64 = 0x0000_0000_0020_0000;
pub const USER_ROOT_SIZE: u64 = 0x0000_0000_1000_0000;
pub const USER_IMAGE_BASE: u64 = USER_ROOT_BASE;
pub const USER_STACK_SIZE: u64 = PAGE_SIZE * 16;
pub const USER_STACK_TOP: u64 = USER_ROOT_BASE + USER_ROOT_SIZE;

const ELF_HEADER_LEN: usize = 64;
const ELF_PHDR_LEN: usize = 56;
const ET_EXEC: u16 = 2;
const EM_AARCH64: u16 = 0xb7;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1 << 0;
const PF_W: u32 = 1 << 1;
const PF_R: u32 = 1 << 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserBootstrapError {
    EmptyImage,
    BadElf(ElfError),
    Initrd(InitrdError),
    MissingSora,
    Memory(MemoryError),
}

impl From<MemoryError> for UserBootstrapError {
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}

impl From<InitrdError> for UserBootstrapError {
    fn from(error: InitrdError) -> Self {
        Self::Initrd(error)
    }
}

impl From<ElfError> for UserBootstrapError {
    fn from(error: ElfError) -> Self {
        Self::BadElf(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfError {
    TooSmall,
    BadMagic,
    NotElf64,
    NotLittleEndian,
    NotExecutable,
    WrongMachine,
    BadProgramHeaderSize,
    ProgramHeaderOutOfFile,
    SegmentOutOfFile,
    SegmentMemSmallerThanFile,
    NoLoadSegments,
}

#[derive(Clone, Debug)]
pub struct UserProcessPlan {
    pub root_job: Job,
    pub process: Process,
    pub load_segments: Vec<ElfSegment>,
    pub image_mappings: Vec<Mapping>,
    pub image_mapping: Mapping,
    pub stack_mapping: Mapping,
    pub entry: u64,
    pub stack_top: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ElfSegment {
    pub file_offset: u64,
    pub file_size: u64,
    pub virt_addr: u64,
    pub mem_size: u64,
    pub flags: PageFlags,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserElfImage {
    pub entry: u64,
    pub segments: Vec<ElfSegment>,
}

/// Plan the first userspace process from an initrd-resident image.
///
/// Today `image` is an opaque blob; the coming ELF loader will replace the single RX
/// mapping with segment mappings while preserving the returned process/VMAR shape.
pub fn plan_initrd_process(
    objects: &mut ObjectManager,
    image: Range,
) -> Result<UserProcessPlan, UserBootstrapError> {
    if image.is_empty() {
        return Err(UserBootstrapError::EmptyImage);
    }

    let root_job = Job::root(objects);
    let root_vmar = Vmar::new(USER_ROOT_BASE, USER_ROOT_SIZE)?;
    let process = Process::new(objects, &root_job, root_vmar);

    let image_vmo = Vmo::new(image.len)?;
    let image_mapping = root_vmar.map(
        image_vmo,
        0,
        USER_IMAGE_BASE,
        image_vmo.len(),
        PageFlags::READ | PageFlags::EXECUTE | PageFlags::USER,
    )?;

    let stack_vmo = Vmo::new(USER_STACK_SIZE)?;
    let stack_base = USER_STACK_TOP - USER_STACK_SIZE;
    let stack_mapping = root_vmar.map(
        stack_vmo,
        0,
        stack_base,
        USER_STACK_SIZE,
        PageFlags::READ | PageFlags::WRITE | PageFlags::USER,
    )?;

    Ok(UserProcessPlan {
        root_job,
        process,
        load_segments: alloc::vec![ElfSegment {
            file_offset: 0,
            file_size: image.len,
            virt_addr: USER_IMAGE_BASE,
            mem_size: image.len,
            flags: PageFlags::READ | PageFlags::EXECUTE | PageFlags::USER,
        }],
        image_mappings: alloc::vec![image_mapping],
        image_mapping,
        stack_mapping,
        entry: USER_IMAGE_BASE,
        stack_top: USER_STACK_TOP,
    })
}

pub fn plan_elf_process(
    objects: &mut ObjectManager,
    image: &[u8],
) -> Result<UserProcessPlan, UserBootstrapError> {
    let elf = parse_user_elf(image)?;
    let root_job = Job::root(objects);
    let root_vmar = Vmar::new(USER_ROOT_BASE, USER_ROOT_SIZE)?;
    let process = Process::new(objects, &root_job, root_vmar);
    let image_vmo = Vmo::new(image.len() as u64)?;

    let mut image_mappings = Vec::new();
    for segment in &elf.segments {
        let page_delta = segment.virt_addr % PAGE_SIZE;
        let virt = align_down(segment.virt_addr);
        let vmo_offset = align_down(segment.file_offset);
        let len = align_up(page_delta.saturating_add(segment.mem_size))
            .ok_or(MemoryError::InvalidRange)?;
        image_mappings.push(root_vmar.map(image_vmo, vmo_offset, virt, len, segment.flags)?);
    }

    let image_mapping = *image_mappings
        .first()
        .ok_or(UserBootstrapError::BadElf(ElfError::NoLoadSegments))?;
    let stack_vmo = Vmo::new(USER_STACK_SIZE)?;
    let stack_base = USER_STACK_TOP - USER_STACK_SIZE;
    let stack_mapping = root_vmar.map(
        stack_vmo,
        0,
        stack_base,
        USER_STACK_SIZE,
        PageFlags::READ | PageFlags::WRITE | PageFlags::USER,
    )?;

    Ok(UserProcessPlan {
        root_job,
        process,
        load_segments: elf.segments,
        image_mappings,
        image_mapping,
        stack_mapping,
        entry: elf.entry,
        stack_top: USER_STACK_TOP,
    })
}

pub fn plan_sora_from_initrd(
    objects: &mut ObjectManager,
    initrd: &[u8],
) -> Result<UserProcessPlan, UserBootstrapError> {
    let sora = find_file(initrd, SORA_INIT_PATH)?.ok_or(UserBootstrapError::MissingSora)?;
    plan_elf_process(objects, sora.bytes)
}

pub fn parse_user_elf(image: &[u8]) -> Result<UserElfImage, ElfError> {
    if image.len() < ELF_HEADER_LEN {
        return Err(ElfError::TooSmall);
    }
    if image[0..4] != *b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if image[4] != 2 {
        return Err(ElfError::NotElf64);
    }
    if image[5] != 1 {
        return Err(ElfError::NotLittleEndian);
    }
    if read_u16(image, 16)? != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    if read_u16(image, 18)? != EM_AARCH64 {
        return Err(ElfError::WrongMachine);
    }

    let entry = read_u64(image, 24)?;
    let phoff =
        usize::try_from(read_u64(image, 32)?).map_err(|_| ElfError::ProgramHeaderOutOfFile)?;
    let phentsize = read_u16(image, 54)? as usize;
    let phnum = read_u16(image, 56)? as usize;
    if phentsize != ELF_PHDR_LEN {
        return Err(ElfError::BadProgramHeaderSize);
    }

    let mut segments = Vec::new();
    for index in 0..phnum {
        let base = phoff
            .checked_add(
                index
                    .checked_mul(phentsize)
                    .ok_or(ElfError::ProgramHeaderOutOfFile)?,
            )
            .ok_or(ElfError::ProgramHeaderOutOfFile)?;
        let end = base
            .checked_add(ELF_PHDR_LEN)
            .ok_or(ElfError::ProgramHeaderOutOfFile)?;
        if end > image.len() {
            return Err(ElfError::ProgramHeaderOutOfFile);
        }
        if read_u32(image, base)? != PT_LOAD {
            continue;
        }

        let flags = read_u32(image, base + 4)?;
        let file_offset = read_u64(image, base + 8)?;
        let virt_addr = read_u64(image, base + 16)?;
        let file_size = read_u64(image, base + 32)?;
        let mem_size = read_u64(image, base + 40)?;
        if mem_size < file_size {
            return Err(ElfError::SegmentMemSmallerThanFile);
        }
        let file_end = file_offset
            .checked_add(file_size)
            .ok_or(ElfError::SegmentOutOfFile)?;
        if file_end > image.len() as u64 {
            return Err(ElfError::SegmentOutOfFile);
        }

        let mut page_flags = PageFlags::USER;
        if flags & PF_R != 0 {
            page_flags |= PageFlags::READ;
        }
        if flags & PF_W != 0 {
            page_flags |= PageFlags::WRITE;
        }
        if flags & PF_X != 0 {
            page_flags |= PageFlags::EXECUTE;
        }

        segments.push(ElfSegment {
            file_offset,
            file_size,
            virt_addr,
            mem_size,
            flags: page_flags,
        });
    }

    if segments.is_empty() {
        return Err(ElfError::NoLoadSegments);
    }

    Ok(UserElfImage { entry, segments })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
    let end = offset.checked_add(2).ok_or(ElfError::TooSmall)?;
    if end > bytes.len() {
        return Err(ElfError::TooSmall);
    }
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
    let end = offset.checked_add(4).ok_or(ElfError::TooSmall)?;
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
    let end = offset.checked_add(8).ok_or(ElfError::TooSmall)?;
    if end > bytes.len() {
        return Err(ElfError::TooSmall);
    }
    Ok(u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ]))
}

const fn align_down(value: u64) -> u64 {
    value & !(PAGE_SIZE - 1)
}

const fn align_up(value: u64) -> Option<u64> {
    let mask = PAGE_SIZE - 1;
    match value.checked_add(mask) {
        Some(value) => Some(value & !mask),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_abi::initrd::{
        INITRD_ENTRY_LEN, INITRD_HEADER_LEN, INITRD_MAGIC, INITRD_PATH_MAX, INITRD_VERSION,
    };

    fn initrd_with_sora(bytes: &[u8]) -> Vec<u8> {
        let mut initrd = vec![0; INITRD_HEADER_LEN + INITRD_ENTRY_LEN];
        initrd[..8].copy_from_slice(&INITRD_MAGIC);
        initrd[8..12].copy_from_slice(&INITRD_VERSION.to_le_bytes());
        initrd[12..16].copy_from_slice(&1u32.to_le_bytes());
        initrd[INITRD_HEADER_LEN..INITRD_HEADER_LEN + SORA_INIT_PATH.len()]
            .copy_from_slice(SORA_INIT_PATH.as_bytes());
        let offset = initrd.len() as u64;
        initrd[INITRD_HEADER_LEN + INITRD_PATH_MAX..INITRD_HEADER_LEN + INITRD_PATH_MAX + 8]
            .copy_from_slice(&offset.to_le_bytes());
        initrd[INITRD_HEADER_LEN + INITRD_PATH_MAX + 8..INITRD_HEADER_LEN + INITRD_PATH_MAX + 16]
            .copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        initrd.extend_from_slice(bytes);
        initrd
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

    fn synthetic_sora_elf() -> Vec<u8> {
        let mut elf = vec![0; 0x3020];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[6] = 1;
        put_u16(&mut elf, 16, ET_EXEC);
        put_u16(&mut elf, 18, EM_AARCH64);
        put_u64(&mut elf, 24, 0x0021_0004);
        put_u64(&mut elf, 32, ELF_HEADER_LEN as u64);
        put_u16(&mut elf, 54, ELF_PHDR_LEN as u16);
        put_u16(&mut elf, 56, 2);

        let ro = ELF_HEADER_LEN;
        put_u32(&mut elf, ro, PT_LOAD);
        put_u32(&mut elf, ro + 4, PF_R);
        put_u64(&mut elf, ro + 8, 0);
        put_u64(&mut elf, ro + 16, 0x0020_0000);
        put_u64(&mut elf, ro + 32, 0x1b4);
        put_u64(&mut elf, ro + 40, 0x1b4);

        let text = ELF_HEADER_LEN + ELF_PHDR_LEN;
        put_u32(&mut elf, text, PT_LOAD);
        put_u32(&mut elf, text + 4, PF_R | PF_X);
        put_u64(&mut elf, text + 8, 0x1004);
        put_u64(&mut elf, text + 16, 0x0021_0004);
        put_u64(&mut elf, text + 32, 0x8);
        put_u64(&mut elf, text + 40, 0x8);
        elf
    }

    #[test]
    fn initrd_process_gets_rx_image_and_rw_stack() {
        let mut objects = ObjectManager::new();
        let plan =
            plan_initrd_process(&mut objects, Range::new(0x8000_0000, PAGE_SIZE + 17)).unwrap();

        assert_eq!(plan.root_job.parent(), None);
        assert_eq!(plan.process.job(), plan.root_job.koid());
        assert_eq!(plan.process.root_vmar().base(), USER_ROOT_BASE);
        assert_eq!(plan.process.root_vmar().len(), USER_ROOT_SIZE);
        assert_eq!(plan.entry, USER_IMAGE_BASE);
        assert_eq!(plan.stack_top, USER_STACK_TOP);

        assert_eq!(plan.image_mapping.virt, USER_IMAGE_BASE);
        assert_eq!(plan.image_mapping.len, PAGE_SIZE * 2);
        assert!(plan
            .image_mapping
            .flags
            .contains(PageFlags::READ | PageFlags::EXECUTE | PageFlags::USER));
        assert!(!plan.image_mapping.flags.contains(PageFlags::WRITE));

        assert_eq!(plan.stack_mapping.virt, USER_STACK_TOP - USER_STACK_SIZE);
        assert_eq!(plan.stack_mapping.len, USER_STACK_SIZE);
        assert!(plan
            .stack_mapping
            .flags
            .contains(PageFlags::READ | PageFlags::WRITE | PageFlags::USER));
        assert!(!plan.stack_mapping.flags.contains(PageFlags::EXECUTE));
    }

    #[test]
    fn empty_initrd_image_is_not_a_process() {
        let mut objects = ObjectManager::new();
        assert_eq!(
            plan_initrd_process(&mut objects, Range::empty()).unwrap_err(),
            UserBootstrapError::EmptyImage
        );
    }

    #[test]
    fn sora_is_found_inside_initrd_before_planning_process() {
        let initrd = initrd_with_sora(&synthetic_sora_elf());
        let mut objects = ObjectManager::new();
        let plan = plan_sora_from_initrd(&mut objects, &initrd).unwrap();
        assert_eq!(plan.image_mappings.len(), 2);
        assert_eq!(plan.entry, 0x0021_0004);
        assert_eq!(plan.image_mappings[0].virt, 0x0020_0000);
        assert_eq!(plan.image_mappings[0].len, PAGE_SIZE);
        assert_eq!(plan.image_mappings[0].vmo_offset, 0);
        assert!(plan.image_mappings[0]
            .flags
            .contains(PageFlags::READ | PageFlags::USER));
        assert!(!plan.image_mappings[0].flags.contains(PageFlags::EXECUTE));
        assert_eq!(plan.image_mappings[1].virt, 0x0021_0000);
        assert_eq!(plan.image_mappings[1].len, PAGE_SIZE);
        assert_eq!(plan.image_mappings[1].vmo_offset, PAGE_SIZE);
        assert!(plan.image_mappings[1]
            .flags
            .contains(PageFlags::READ | PageFlags::EXECUTE | PageFlags::USER));
        assert!(!plan.image_mappings[1].flags.contains(PageFlags::WRITE));
    }

    #[test]
    fn missing_sora_entry_is_reported() {
        let initrd = initrd_with_sora(b"not used");
        let mut without_sora = initrd;
        without_sora[INITRD_HEADER_LEN] = b'x';

        let mut objects = ObjectManager::new();
        assert_eq!(
            plan_sora_from_initrd(&mut objects, &without_sora).unwrap_err(),
            UserBootstrapError::MissingSora
        );
    }

    #[test]
    fn rejects_malformed_sora_elf() {
        let initrd = initrd_with_sora(b"not an elf");
        let mut objects = ObjectManager::new();
        assert_eq!(
            plan_sora_from_initrd(&mut objects, &initrd).unwrap_err(),
            UserBootstrapError::BadElf(ElfError::TooSmall)
        );
    }
}
