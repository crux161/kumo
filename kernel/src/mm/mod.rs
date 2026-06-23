use core::sync::atomic::{AtomicU64, Ordering};

use kumo_abi::{BootInfo, MemRegion, MemRegionKind, Range};
use kumo_hal::PageFlags;

pub mod heap;

pub const PAGE_SIZE: u64 = 4096;
const BLOCK_2M: u64 = 1 << 21;
const GIB: u64 = 1 << 30;

/// Start of the next never-yet-allocated physical frame. A persistent watermark so a
/// frame handed out for the kernel page tables (`enable_paging`) is never reissued when a
/// per-process address space is built (`alloc_zeroed_frame`). 0 = nothing allocated yet.
static FRAME_WATERMARK: AtomicU64 = AtomicU64::new(0);

/// Hand out the next usable physical frame, zeroed, advancing [`FRAME_WATERMARK`] so the
/// same frame is never returned twice across the whole boot (kernel tables *and* user
/// address spaces draw from this one cursor). The frame is touched by physical address, so
/// the caller must hold the kernel identity map active in TTBR0 — i.e. allocate *before*
/// switching to a process page-table tree.
///
/// O(frames-scanned) per call (it re-derives the boot allocator and skips below the
/// watermark); Stage-A only ever needs a few dozen frames, so the simplicity wins.
///
/// # Safety
/// `boot.mem_regions` must be the validated handoff slice, readable for the call, and the
/// returned frame must be mapped by the currently-active page tables.
pub unsafe fn alloc_zeroed_frame(boot: &BootInfo) -> Option<u64> {
    let plan = unsafe { KernelMemoryPlan::from_boot_info(boot) };
    let mut frames = plan.frame_allocator();
    let watermark = FRAME_WATERMARK.load(Ordering::Relaxed);
    loop {
        let frame = frames.next_frame()?.start;
        if frame < watermark {
            continue;
        }
        FRAME_WATERMARK.store(frame.saturating_add(PAGE_SIZE), Ordering::Relaxed);
        // SAFETY: usable RAM, mapped now; zero it for a clean table / BSS-clear page.
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE as usize) };
        return Some(frame);
    }
}

/// What kernel paging brought up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PagingReport {
    pub tables: usize,
    pub mapped_bytes: u64,
}

fn is_ram_kind(kind: MemRegionKind) -> bool {
    matches!(
        kind,
        MemRegionKind::Usable
            | MemRegionKind::Bootloader
            | MemRegionKind::Kernel
            | MemRegionKind::Initrd
            | MemRegionKind::Acpi
    )
}

/// Build KUMO-owned split page tables and switch to them. TTBR0 retains the bootstrap
/// identity map; TTBR1 owns the high-linked kernel and the permanent physical-memory
/// window. RAM becomes Normal-WB, unlisted/MMIO ranges become Device, and the
/// framebuffer becomes Normal-NC so the display controller sees writes.
///
/// # Safety
///
/// Must run once at EL1 while the firmware identity map is active, before anything
/// depends on a particular page table. `boot.mem_regions` must be the validated
/// handoff slice and remain readable for the call.
pub unsafe fn enable_paging(boot: &BootInfo) -> Option<PagingReport> {
    let regions = unsafe { boot.mem_regions.as_slice() };
    if regions.is_empty() {
        return None;
    }

    let mut top = 0u64;
    for region in regions {
        top = top.max(region.range.end());
    }
    let (fb_phys, fb_len) = if boot.has_framebuffer() {
        (boot.framebuffer.phys, boot.framebuffer.len)
    } else {
        (0, 0)
    };
    top = top.max(fb_phys.saturating_add(fb_len));
    top = top.div_ceil(GIB).saturating_mul(GIB);
    if top == 0 {
        return None;
    }

    // Page-table frames come from usable RAM (which we then map Normal-WB) via the shared
    // watermark allocator, so frames spent on kernel tables here are off-limits when a
    // process address space is built later. The boot frame allocator already excludes the
    // kernel, initrd, and framebuffer.
    let mut alloc = || -> Option<u64> { unsafe { alloc_zeroed_frame(boot) } };
    let is_ram = |pa: u64| {
        regions.iter().any(|region| {
            is_ram_kind(region.kind) && overlaps(pa, BLOCK_2M, region.range.start, region.range.len)
        })
    };

    let kernel = boot.kernel_phys;
    let kernel_virt = boot.kernel_virt;
    let (tables, mapped_bytes) = unsafe {
        kumo_hal::active::enable_kernel_mmu(
            top,
            kernel.start,
            kernel_virt.start,
            kernel.len,
            fb_phys,
            fb_len,
            &is_ram,
            &mut alloc,
        )
    }
    .ok()?;
    Some(PagingReport {
        tables,
        mapped_bytes,
    })
}

/// What M1 memory bring-up found and proved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryReport {
    pub usable_frames: u64,
    pub usable_bytes: u64,
    pub heap_bytes: u64,
    pub sample_frames: [u64; 3],
    pub sample_count: usize,
}

/// Bring up M1 memory from a validated boot handoff: account the usable frames and
/// take a few sample frames to prove the boot frame allocator actually yields real
/// addresses. The kernel heap (the bump `#[global_allocator]`) is already online.
///
/// # Safety
///
/// `boot.mem_regions` must be the validated handoff slice and remain readable for
/// the duration of the call.
pub unsafe fn init(boot: &BootInfo) -> MemoryReport {
    let plan = unsafe { KernelMemoryPlan::from_boot_info(boot) };
    let usable_frames = plan.usable_frame_count();

    let mut allocator = plan.frame_allocator();
    let mut sample_frames = [0u64; 3];
    let mut sample_count = 0;
    while sample_count < sample_frames.len() {
        match allocator.next_frame() {
            Some(frame) => {
                sample_frames[sample_count] = frame.start;
                sample_count += 1;
            }
            None => break,
        }
    }

    MemoryReport {
        usable_frames,
        usable_bytes: usable_frames.saturating_mul(PAGE_SIZE),
        heap_bytes: heap::HEAP_SIZE as u64,
        sample_frames,
        sample_count,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryError {
    Empty,
    InvalidRange,
    Unaligned,
    OutOfBounds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysFrame {
    pub start: u64,
}

impl PhysFrame {
    pub const fn containing(addr: u64) -> Self {
        Self {
            start: align_down(addr),
        }
    }

    pub const fn end(self) -> u64 {
        self.start.saturating_add(PAGE_SIZE)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameRange {
    pub start: PhysFrame,
    pub count: u64,
}

impl FrameRange {
    pub fn from_byte_range(range: Range) -> Result<Self, MemoryError> {
        if range.is_empty() {
            return Err(MemoryError::Empty);
        }

        let start = align_up(range.start).ok_or(MemoryError::InvalidRange)?;
        let end = align_down(range.end());
        if start >= end {
            return Err(MemoryError::InvalidRange);
        }

        Ok(Self {
            start: PhysFrame { start },
            count: (end - start) / PAGE_SIZE,
        })
    }

    pub const fn byte_len(self) -> u64 {
        self.count.saturating_mul(PAGE_SIZE)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mapping {
    pub virt: u64,
    pub len: u64,
    pub vmo_offset: u64,
    pub flags: PageFlags,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmoBacking {
    /// Anonymous memory: fresh zeroed frames allocated on map.
    Anonymous,
    /// Ordinary physical RAM at a fixed address (initrd, boot-info snapshot).
    PhysicalRam { phys_base: u64 },
    /// Resource-minted MMIO. Maps Device-nGnRnE unless the caller explicitly
    /// requests the framebuffer-specific Normal-NC policy.
    Mmio { phys_base: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vmo {
    len: u64,
    backing: VmoBacking,
}

impl Vmo {
    pub fn new(len: u64) -> Result<Self, MemoryError> {
        if len == 0 {
            return Err(MemoryError::Empty);
        }
        Ok(Self {
            len: align_up(len).ok_or(MemoryError::InvalidRange)?,
            backing: VmoBacking::Anonymous,
        })
    }

    pub fn from_physical_range(phys_base: u64, len: u64) -> Result<Self, MemoryError> {
        if len == 0 {
            return Err(MemoryError::Empty);
        }
        if !is_page_aligned(phys_base) {
            return Err(MemoryError::Unaligned);
        }
        Ok(Self {
            len: align_up(len).ok_or(MemoryError::InvalidRange)?,
            backing: VmoBacking::PhysicalRam { phys_base },
        })
    }

    pub fn from_mmio_range(phys_base: u64, len: u64) -> Result<Self, MemoryError> {
        if len == 0 {
            return Err(MemoryError::Empty);
        }
        if !is_page_aligned(phys_base) {
            return Err(MemoryError::Unaligned);
        }
        Ok(Self {
            len: align_up(len).ok_or(MemoryError::InvalidRange)?,
            backing: VmoBacking::Mmio { phys_base },
        })
    }

    pub const fn len(self) -> u64 {
        self.len
    }

    pub const fn frame_count(self) -> u64 {
        self.len / PAGE_SIZE
    }

    pub const fn backing(self) -> VmoBacking {
        self.backing
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Vmar {
    base: u64,
    len: u64,
}

impl Vmar {
    pub fn new(base: u64, len: u64) -> Result<Self, MemoryError> {
        if len == 0 {
            return Err(MemoryError::Empty);
        }
        if !is_page_aligned(base) || !is_page_aligned(len) {
            return Err(MemoryError::Unaligned);
        }

        Ok(Self { base, len })
    }

    pub const fn base(self) -> u64 {
        self.base
    }

    pub const fn len(self) -> u64 {
        self.len
    }

    pub fn map(
        self,
        vmo: Vmo,
        vmo_offset: u64,
        virt: u64,
        len: u64,
        flags: PageFlags,
    ) -> Result<Mapping, MemoryError> {
        if len == 0 {
            return Err(MemoryError::Empty);
        }
        if !is_page_aligned(vmo_offset) || !is_page_aligned(virt) || !is_page_aligned(len) {
            return Err(MemoryError::Unaligned);
        }
        if !contains_range(self.base, self.len, virt, len) {
            return Err(MemoryError::OutOfBounds);
        }
        if !contains_range(0, vmo.len, vmo_offset, len) {
            return Err(MemoryError::OutOfBounds);
        }

        Ok(Mapping {
            virt,
            len,
            vmo_offset,
            flags,
        })
    }
}

#[derive(Clone, Debug)]
pub struct KernelMemoryPlan<'a> {
    regions: &'a [MemRegion],
    exclusions: [Range; 3],
    exclusion_len: usize,
}

impl<'a> KernelMemoryPlan<'a> {
    /// Build an M1 memory plan from a validated boot handoff.
    ///
    /// # Safety
    ///
    /// `boot.mem_regions` must have been validated by Nijigumo/Ziwei handoff
    /// code and must remain readable for `'a`.
    pub unsafe fn from_boot_info(boot: &'a BootInfo) -> Self {
        let regions = unsafe { boot.mem_regions.as_slice() };
        let mut exclusions = [Range::empty(); 3];
        let mut exclusion_len = 0;

        push_exclusion(&mut exclusions, &mut exclusion_len, boot.kernel_phys);
        push_exclusion(&mut exclusions, &mut exclusion_len, boot.initrd);
        if boot.has_framebuffer() {
            push_exclusion(
                &mut exclusions,
                &mut exclusion_len,
                Range::new(boot.framebuffer.phys, boot.framebuffer.len),
            );
        }

        Self {
            regions,
            exclusions,
            exclusion_len,
        }
    }

    pub const fn regions(&self) -> &'a [MemRegion] {
        self.regions
    }

    pub fn exclusions(&self) -> &[Range] {
        &self.exclusions[..self.exclusion_len]
    }

    pub fn frame_allocator(&self) -> BootFrameAllocator<'_> {
        BootFrameAllocator::new(self.regions, self.exclusions())
    }

    pub fn usable_frame_count(&self) -> u64 {
        self.frame_allocator().count_remaining()
    }
}

#[derive(Clone, Debug)]
pub struct BootFrameAllocator<'a> {
    regions: &'a [MemRegion],
    exclusions: &'a [Range],
    region_index: usize,
    cursor: u64,
    limit: u64,
}

impl<'a> BootFrameAllocator<'a> {
    pub fn new(regions: &'a [MemRegion], exclusions: &'a [Range]) -> Self {
        let mut allocator = Self {
            regions,
            exclusions,
            region_index: 0,
            cursor: 0,
            limit: 0,
        };
        allocator.advance_to_usable_region();
        allocator
    }

    pub fn next_frame(&mut self) -> Option<PhysFrame> {
        loop {
            if self.cursor >= self.limit {
                self.region_index = self.region_index.saturating_add(1);
                self.advance_to_usable_region();
                if self.cursor >= self.limit {
                    return None;
                }
            }

            let frame = PhysFrame { start: self.cursor };
            self.cursor = self.cursor.saturating_add(PAGE_SIZE);
            if !self.is_excluded(frame) {
                return Some(frame);
            }
        }
    }

    pub fn count_remaining(mut self) -> u64 {
        let mut count = 0_u64;
        while self.next_frame().is_some() {
            count = count.saturating_add(1);
        }
        count
    }

    fn advance_to_usable_region(&mut self) {
        while let Some(region) = self.regions.get(self.region_index) {
            if region.kind != MemRegionKind::Usable {
                self.region_index = self.region_index.saturating_add(1);
                continue;
            }

            let Some(cursor) = align_up(region.range.start) else {
                self.cursor = u64::MAX;
                self.limit = u64::MAX;
                return;
            };
            let limit = align_down(region.range.end());
            if cursor < limit {
                self.cursor = cursor;
                self.limit = limit;
                return;
            }

            self.region_index = self.region_index.saturating_add(1);
        }

        self.cursor = u64::MAX;
        self.limit = u64::MAX;
    }

    fn is_excluded(&self, frame: PhysFrame) -> bool {
        self.exclusions
            .iter()
            .any(|range| overlaps(frame.start, PAGE_SIZE, range.start, range.len))
    }
}

pub const fn is_page_aligned(value: u64) -> bool {
    value & (PAGE_SIZE - 1) == 0
}

pub const fn align_down(value: u64) -> u64 {
    value & !(PAGE_SIZE - 1)
}

pub const fn align_up(value: u64) -> Option<u64> {
    let down = align_down(value);
    if down == value {
        Some(value)
    } else {
        down.checked_add(PAGE_SIZE)
    }
}

fn push_exclusion(exclusions: &mut [Range; 3], len: &mut usize, range: Range) {
    if range.is_empty() {
        return;
    }

    exclusions[*len] = range;
    *len += 1;
}

fn contains_range(base: u64, len: u64, start: u64, range_len: u64) -> bool {
    let Some(limit) = base.checked_add(len) else {
        return false;
    };
    let Some(end) = start.checked_add(range_len) else {
        return false;
    };
    base <= start && start <= end && end <= limit
}

fn overlaps(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_abi::{RawSlice, ABI_VERSION};

    #[test]
    fn frame_ranges_are_page_aligned() {
        let range = FrameRange::from_byte_range(Range::new(0x1003, 0x3000)).unwrap();
        assert_eq!(range.start.start, 0x2000);
        assert_eq!(range.count, 2);
        assert_eq!(range.byte_len(), 0x2000);
    }

    #[test]
    fn boot_allocator_skips_non_usable_and_excluded_frames() {
        let regions = [
            MemRegion {
                range: Range::new(0x1000, 0x3000),
                kind: MemRegionKind::Usable,
                _reserved: 0,
            },
            MemRegion {
                range: Range::new(0x4000, 0x2000),
                kind: MemRegionKind::Reserved,
                _reserved: 0,
            },
            MemRegion {
                range: Range::new(0x6000, 0x3000),
                kind: MemRegionKind::Usable,
                _reserved: 0,
            },
        ];
        let exclusions = [Range::new(0x2000, 0x1000), Range::new(0x7000, 0x0800)];
        let mut allocator = BootFrameAllocator::new(&regions, &exclusions);

        assert_eq!(allocator.next_frame(), Some(PhysFrame { start: 0x1000 }));
        assert_eq!(allocator.next_frame(), Some(PhysFrame { start: 0x3000 }));
        assert_eq!(allocator.next_frame(), Some(PhysFrame { start: 0x6000 }));
        assert_eq!(allocator.next_frame(), Some(PhysFrame { start: 0x8000 }));
        assert_eq!(allocator.next_frame(), None);
    }

    #[test]
    fn memory_plan_excludes_kernel_initrd_and_framebuffer() {
        static REGIONS: [MemRegion; 1] = [MemRegion {
            range: Range {
                start: 0x1000,
                len: 0x8000,
            },
            kind: MemRegionKind::Usable,
            _reserved: 0,
        }];

        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&REGIONS);
        boot.kernel_phys = Range::new(0x2000, 0x1000);
        boot.initrd = Range::new(0x4000, 0x1000);
        boot.flags = BootInfo::FLAG_FRAMEBUFFER_PRESENT;
        boot.framebuffer.phys = 0x6000;
        boot.framebuffer.len = 0x1000;

        let plan = unsafe { KernelMemoryPlan::from_boot_info(&boot) };
        let mut allocator = plan.frame_allocator();
        assert_eq!(plan.exclusions().len(), 3);
        assert_eq!(plan.usable_frame_count(), 5);
        assert_eq!(allocator.next_frame(), Some(PhysFrame { start: 0x1000 }));
        assert_eq!(allocator.next_frame(), Some(PhysFrame { start: 0x3000 }));
        assert_eq!(allocator.next_frame(), Some(PhysFrame { start: 0x5000 }));
    }

    #[test]
    fn init_accounts_frames_and_samples_real_addresses() {
        static REGIONS: [MemRegion; 2] = [
            MemRegion {
                range: Range {
                    start: 0x1000,
                    len: 0x4000,
                },
                kind: MemRegionKind::Usable,
                _reserved: 0,
            },
            MemRegion {
                range: Range {
                    start: 0x8000,
                    len: 0x2000,
                },
                kind: MemRegionKind::Reserved,
                _reserved: 0,
            },
        ];
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&REGIONS);
        boot.kernel_phys = Range::new(0x2000, 0x1000); // excludes one usable frame

        let report = unsafe { init(&boot) };
        assert_eq!(report.usable_frames, 3); // 4 usable frames minus the kernel one
        assert_eq!(report.usable_bytes, 3 * PAGE_SIZE);
        assert_eq!(report.heap_bytes, heap::HEAP_SIZE as u64);
        assert_eq!(report.sample_count, 3);
        assert_eq!(report.sample_frames[0], 0x1000);
        assert_eq!(report.sample_frames[1], 0x3000); // 0x2000 excluded (kernel)
        assert_eq!(report.sample_frames[2], 0x4000);
    }

    #[test]
    fn vmo_rounds_to_pages() {
        let vmo = Vmo::new(PAGE_SIZE + 1).unwrap();
        assert_eq!(vmo.len(), PAGE_SIZE * 2);
        assert_eq!(vmo.frame_count(), 2);
    }

    #[test]
    fn physical_ram_and_mmio_keep_distinct_mapping_policies() {
        assert_eq!(
            Vmo::from_physical_range(0x4000, PAGE_SIZE)
                .unwrap()
                .backing(),
            VmoBacking::PhysicalRam { phys_base: 0x4000 }
        );
        assert_eq!(
            Vmo::from_mmio_range(0x8000, PAGE_SIZE).unwrap().backing(),
            VmoBacking::Mmio { phys_base: 0x8000 }
        );
    }

    #[test]
    fn vmar_validates_mapping_bounds() {
        let vmo = Vmo::new(PAGE_SIZE * 2).unwrap();
        let vmar = Vmar::new(0x4000_0000, PAGE_SIZE * 4).unwrap();
        let mapping = vmar
            .map(
                vmo,
                PAGE_SIZE,
                0x4000_1000,
                PAGE_SIZE,
                PageFlags::READ | PageFlags::WRITE,
            )
            .unwrap();
        assert_eq!(mapping.virt, 0x4000_1000);
        assert_eq!(mapping.vmo_offset, PAGE_SIZE);

        assert_eq!(
            vmar.map(vmo, 0, 0x4000_0123, PAGE_SIZE, PageFlags::READ),
            Err(MemoryError::Unaligned)
        );
        assert_eq!(
            vmar.map(vmo, PAGE_SIZE, 0x4000_3000, PAGE_SIZE * 2, PageFlags::READ),
            Err(MemoryError::OutOfBounds)
        );
    }
}
