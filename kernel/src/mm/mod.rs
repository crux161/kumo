use core::sync::atomic::{AtomicU64, Ordering};

use kumo_abi::{BootInfo, MemRegion, MemRegionKind, Range};
use kumo_hal::PageFlags;

//j616
//j620
//j743
//j822
//j825

pub mod heap;

pub const PAGE_SIZE: u64 = 4096;
const BLOCK_2M: u64 = 1 << 21;
const GIB: u64 = 1 << 30;

/// Start of the next never-yet-allocated physical frame. A persistent watermark so a
/// frame handed out for the kernel page tables (`enable_paging`) is never reissued when a
/// per-process address space is built (`alloc_zeroed_frame`). 0 = nothing allocated yet.
static ISSUED_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Frames actually issued, not address-watermark distance or free/reclaimable frames.
/// — TIMBERDOODLE
pub fn frames_issued() -> u64 {
    ISSUED_FRAMES.load(Ordering::Relaxed)
}

static FRAME_WATERMARK: AtomicU64 = AtomicU64::new(0);

/// AX4: frames returned by a dead owner and available for reuse. Distinct from the DMA run
/// list: only ordinary kernel/user RAM frames enter here, never a DMA run, a device frame,
/// an identity-mapped kernel frame, or a page still named by any live mapping or handle.
///
/// The list is touched from syscall and teardown paths only (never from an IRQ handler), on
/// the single core PLAN/021 AX1 allows. Push/pop are plain Vec operations; a full list or a
/// failed reservation drops the frame rather than panicking (the frame is then simply lost
/// to the boot watermark, as it was before AX4).
struct FreeFrameList(core::cell::UnsafeCell<alloc::vec::Vec<u64>>);
// SAFETY: the kernel is single-core and no interrupt path allocates or frees frames; the
// only concurrent access is reentrancy from an SVC that cannot preempt itself.
unsafe impl Sync for FreeFrameList {}
static FREE_FRAMES: FreeFrameList =
    FreeFrameList(core::cell::UnsafeCell::new(alloc::vec::Vec::new()));
/// Frames actually handed out from [`FREE_FRAMES`] instead of the bump watermark. Not part
/// of `frames_issued`; a flat census means this counter absorbs all post-warm-up churn.
static FRAMES_REUSED: AtomicU64 = AtomicU64::new(0);
/// Monotonic count of frames successfully queued for reuse (accepted by the list).
static FRAMES_FREED: AtomicU64 = AtomicU64::new(0);

/// Frames currently queued for reuse. Test and diagnostic surface.
pub fn free_frame_count() -> usize {
    // SAFETY: single-core; the caller is observing, not holding the list.
    unsafe { (&*FREE_FRAMES.0.get()).len() }
}

/// How many allocations were served from the reuse list rather than the watermark.
pub fn frames_reused() -> u64 {
    FRAMES_REUSED.load(Ordering::Relaxed)
}

/// How many frames have been queued for reuse over the whole boot. Monotonic, unlike the
/// current list length; a test can use a delta without racing a concurrent pop.
pub fn frames_freed() -> u64 {
    FRAMES_FREED.load(Ordering::Relaxed)
}

/// Queue a dead owner's frame for reuse. `false` means the frame could not be recorded
/// (misaligned, zero, or the list's reservation failed); the caller must treat a lost frame
/// as leaked, never as still mapped. The frame is not touched here: at the frees that matter
/// the caller may still have a user TTBR0 live, and only the identity-map-safe allocation
/// path may zero or clean it. — TIMBERDOODLE
pub fn free_frame(frame: u64) -> bool {
    if frame == 0 || frame % PAGE_SIZE != 0 {
        return false;
    }
    // SAFETY: single-core, syscall/teardown context only (see FreeFrameList).
    let list = unsafe { &mut *FREE_FRAMES.0.get() };
    if list.try_reserve(1).is_err() {
        return false;
    }
    list.push(frame);
    FRAMES_FREED.fetch_add(1, Ordering::Relaxed);
    true
}

/// Why a queued frame may not be reused: it falls in an exclusion the caller just named.
/// The list scan leaves such a frame queued for a later allocation.
fn frame_in_excluded(frame: u64, excluded: &[Range]) -> bool {
    excluded.iter().any(|range| {
        range
            .start
            .checked_add(range.len)
            .is_none_or(|end| frame >= range.start && frame < end)
    })
}

/// Take the newest queued frame the caller can use. The scan pops nothing it rejects, so an
/// exclusion or a coarse-alias rejection defers the frame rather than losing it.
fn take_free_frame(usable: impl Fn(u64) -> bool) -> Option<u64> {
    // SAFETY: single-core, syscall context only (see FreeFrameList).
    let list = unsafe { &mut *FREE_FRAMES.0.get() };
    let mut index = list.len();
    while index > 0 {
        index -= 1;
        let frame = list[index];
        if !usable(frame) {
            continue;
        }
        list.swap_remove(index);
        return Some(frame);
    }
    None
}

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
    unsafe { alloc_zeroed_frame_excluding(boot, &[]) }
}

/// Allocate private RAM while excluding already-issued physical aliases. The exclusions are
/// applied before zeroing, so rejecting an aliased candidate cannot damage its VMO. — KESTREL
///
/// # Safety
/// Same boot handoff and active identity-map requirements as [`alloc_zeroed_frame`].
pub unsafe fn alloc_zeroed_frame_excluding(boot: &BootInfo, excluded: &[Range]) -> Option<u64> {
    let plan = unsafe { KernelMemoryPlan::from_boot_info(boot) };
    let mut frames = plan.frame_allocator().excluding(excluded);
    let watermark = FRAME_WATERMARK.load(Ordering::Relaxed);
    #[cfg(feature = "arch_aarch64")]
    let framebuffer = boot_framebuffer_range(boot);
    // AX4: a previously issued frame costs no watermark advance and no ISSUED bump. The
    // frame already passed every filter when it was first issued, but device apertures move:
    // re-check the coarse-alias and current-device filters so a frame that a new DMA or
    // framebuffer grant now covers cannot become user RAM. Zero and clean to PoC before it is
    // mapped anywhere, preserving the j476 visibility guarantee for an uncached alias.
    if let Some(frame) = take_free_frame(|frame| {
        if frame_in_excluded(frame, excluded) {
            return false;
        }
        if !crate::syscall::kernel_device_frame_allowed(frame & !(BLOCK_2M - 1), BLOCK_2M) {
            return false;
        }
        #[cfg(feature = "arch_aarch64")]
        if !normal_identity_block(plan.regions(), framebuffer, frame & !(BLOCK_2M - 1)) {
            return false;
        }
        true
    }) {
        // SAFETY: a previously issued frame, mapped by the active identity map; the same
        // contract as the watermark path below.
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE as usize) };
        kumo_hal::active::clean_dcache_to_poc(frame as usize, PAGE_SIZE as usize);
        FRAMES_REUSED.fetch_add(1, Ordering::Relaxed);
        return Some(frame);
    }
    loop {
        let frame = frames.next_frame()?.start;
        if frame < watermark || !crate::syscall::kernel_device_frame_allowed(frame, PAGE_SIZE) {
            continue;
        }
        // The AArch64 permanent identity/physmap is built from 2 MiB blocks. Do not hand a
        // page-table or userspace backing frame to code that will access it as Normal-WB when
        // its identity alias is Device or framebuffer Normal-NC. A firmware map may split DRAM
        // at boundaries that are not 2 MiB aligned; accepting an arbitrary Usable page from a
        // conservatively typed boundary block creates incompatible Normal/Device aliases. QEMU is
        // permissive; Cortex-A76 can then observe a zero L3 leaf on the first EL0 fetch. The
        // generic boot allocator remains an accounting primitive; only fresh AArch64 frames
        // carry this current coarse-mapper constraint. — KESTREL 2026-07-17
        #[cfg(feature = "arch_aarch64")]
        if !normal_identity_block(plan.regions(), framebuffer, frame & !(BLOCK_2M - 1)) {
            continue;
        }
        FRAME_WATERMARK.store(frame.saturating_add(PAGE_SIZE), Ordering::Relaxed);
        ISSUED_FRAMES.fetch_add(1, Ordering::Relaxed);
        // SAFETY: usable RAM, mapped now; zero it for a clean table / BSS-clear page.
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE as usize) };
        return Some(frame);
    }
}

/// Allocate `count` physically-contiguous, zeroed frames from usable RAM and return the
/// physical base of the run, advancing [`FRAME_WATERMARK`] past it so the run is never
/// reissued. Applies the *same* per-frame filters as [`alloc_zeroed_frame`] (watermark, and
/// on AArch64 the whole-2 MiB-block Normal-WB constraint), tracking a run of adjacent frames.
/// A frame rejected by those filters breaks the current run.
///
/// The run is cleaned to the Point of Coherency after zeroing: a DMA VMO built over it is
/// typically mapped Normal-NC by its driver, so the device and that uncached alias must see
/// DRAM, not dirty write-back lines left by the zeroing store (the j476 hazard class).
///
/// Frames scanned below the returned base are leaked for the rest of boot; DMA allocations
/// are few and small, so the forward bump loss is acceptable.
///
/// # Safety
/// Same contract as [`alloc_zeroed_frame`]: the kernel identity map must be active in TTBR0
/// so the run can be zeroed and cache-maintained by physical address.
pub unsafe fn alloc_contiguous_frames(boot: &BootInfo, count: u64) -> Option<u64> {
    unsafe { alloc_contiguous_frames_excluding(boot, count, &[]) }
}

/// Contiguous DMA allocation with extra private-table/alias exclusions, applied before
/// zeroing and cache maintenance. Same safety contract as [`alloc_contiguous_frames`].
///
/// # Safety
/// The validated boot ranges and selected physical run must be mapped in the active tree.
pub unsafe fn alloc_contiguous_frames_excluding(
    boot: &BootInfo,
    count: u64,
    excluded: &[Range],
) -> Option<u64> {
    if count == 0 {
        return None;
    }
    let bytes = count.checked_mul(PAGE_SIZE)?;
    let plan = unsafe { KernelMemoryPlan::from_boot_info(boot) };
    let mut frames = plan.frame_allocator().excluding(excluded);
    let watermark = FRAME_WATERMARK.load(Ordering::Relaxed);
    #[cfg(feature = "arch_aarch64")]
    let framebuffer = boot_framebuffer_range(boot);

    let mut base: Option<u64> = None;
    let mut have: u64 = 0;
    let mut prev: u64 = 0;
    loop {
        let frame = frames.next_frame()?.start;
        // DMA VMOs can expose a whole uncached 2 MiB CPU block. Skip that actual
        // alias reach before zeroing, just as for the private-table exclusions. — KESTREL
        if frame < watermark
            || !crate::syscall::kernel_device_frame_allowed(frame & !(BLOCK_2M - 1), BLOCK_2M)
        {
            base = None;
            have = 0;
            continue;
        }
        #[cfg(feature = "arch_aarch64")]
        if !normal_identity_block(plan.regions(), framebuffer, frame & !(BLOCK_2M - 1)) {
            base = None;
            have = 0;
            continue;
        }
        match base {
            Some(_) if frame == prev.saturating_add(PAGE_SIZE) => have += 1,
            _ => {
                base = Some(frame);
                have = 1;
            }
        }
        prev = frame;
        if have == count {
            let start = base.expect("run base set once have > 0");
            FRAME_WATERMARK.store(start.saturating_add(bytes), Ordering::Relaxed);
            ISSUED_FRAMES.fetch_add(count, Ordering::Relaxed);
            // SAFETY: usable RAM, mapped Normal-WB now; zero then push to PoC (see doc).
            unsafe { core::ptr::write_bytes(start as *mut u8, 0, bytes as usize) };
            kumo_hal::active::clean_dcache_to_poc(start as usize, bytes as usize);
            return Some(start);
        }
    }
}

/// Fill a preallocated DMA page list in one allocator walk. A short return owns the prefix:
/// the caller must reclaim it on failure. No contiguous physical run is required. — KESTREL
///
/// # Safety
/// Same boot ranges and active identity-map requirements as contiguous DMA allocation.
pub unsafe fn alloc_dma_pages_excluding(
    boot: &BootInfo,
    pages: &mut [Option<u64>],
    excluded: &[Range],
) -> usize {
    let plan = unsafe { KernelMemoryPlan::from_boot_info(boot) };
    let mut frames = plan.frame_allocator().excluding(excluded);
    let watermark = FRAME_WATERMARK.load(Ordering::Relaxed);
    #[cfg(feature = "arch_aarch64")]
    let framebuffer = boot_framebuffer_range(boot);
    let mut filled = 0;
    while filled < pages.len() {
        let Some(range) = frames.next_frame() else {
            break;
        };
        let frame = range.start;
        if frame < watermark
            || !crate::syscall::kernel_device_frame_allowed(frame & !(BLOCK_2M - 1), BLOCK_2M)
        {
            continue;
        }
        #[cfg(feature = "arch_aarch64")]
        if !normal_identity_block(plan.regions(), framebuffer, frame & !(BLOCK_2M - 1)) {
            continue;
        }
        FRAME_WATERMARK.store(frame.saturating_add(PAGE_SIZE), Ordering::Relaxed);
        ISSUED_FRAMES.fetch_add(1, Ordering::Relaxed);
        unsafe { core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE as usize) };
        kumo_hal::active::clean_dcache_to_poc(frame as usize, PAGE_SIZE as usize);
        pages[filled] = Some(frame);
        filled += 1;
    }
    filled
}

/// AX4: free a dead user address space's page-table frames (the walk that used to leak).
///
/// The walk only follows valid table descriptors at L0..=L2 (`bits 1:0 == 0b11`); L3 entries
/// and L1/L2 block leaves are mappings, not tables, and are never freed here. Children are
/// freed before the table that names them and the L0 root is freed last, so no frame is
/// reusable while a higher-level descriptor still points at it. The descriptors are read
/// through the kernel physmap, which stays live under any TTBR0, so this is safe on the
/// dying process's own SVC path after its ASID has already been invalidated and released.
#[cfg(feature = "arch_aarch64")]
mod user_tables {
    use super::{free_frame, PAGE_SIZE};
    const PA_MASK: u64 = 0x0000_ffff_ffff_f000;

    #[cfg(target_os = "none")]
    fn read_phys_u64(phys: u64) -> u64 {
        let mut bytes = [0u8; 8];
        kumo_hal::active::read_phys(phys, &mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn walk(table: u64, level: u8, read: &mut dyn FnMut(u64) -> u64, freed: &mut usize) {
        // L3 entries are leaf pages. L0..=L2 entries with bits 1:0 == 0b11 are tables; a
        // block (0b01) is a mapping and stays with the VMO that owns it.
        if level >= 3 {
            return;
        }
        for index in 0..512u64 {
            let desc = read(table + index * 8);
            if desc & 0b11 != 0b11 {
                continue;
            }
            let child = desc & PA_MASK;
            if child == 0 {
                continue;
            }
            walk(child, level + 1, read, freed);
            if free_frame(child) {
                *freed += 1;
            }
        }
    }

    /// Host-testable core: `read` returns the descriptor word at a physical address.
    pub(super) fn free_user_tables_with(root: u64, read: &mut dyn FnMut(u64) -> u64) -> usize {
        let root = root & PA_MASK;
        if root == 0 || root % PAGE_SIZE != 0 {
            return 0;
        }
        let mut freed = 0;
        walk(root, 0, read, &mut freed);
        if free_frame(root) {
            freed += 1;
        }
        freed
    }

    #[cfg(target_os = "none")]
    pub(super) fn free_user_tables(root: u64) -> usize {
        free_user_tables_with(root, &mut read_phys_u64)
    }

    /// AX4: free the leaf pages one address space mapped for an anonymous range (its user
    /// stack).
    ///
    /// AddressSpaceCreate installs the stack through the HAL builder, so no VMO owns those
    /// frames; without this walk they are the only user frames no retire pass can see. Only
    /// L3 page leaves inside `[start, start + len)` are freed: a missing table, a hole or an
    /// L1/L2 block leaves that page untouched. The range was reserved against
    /// `Process::mappings` when it was installed, so it cannot alias a VMO leaf and this
    /// cannot double-free. Runs before `free_user_tables`, whose walk retires the tables
    /// this one reads. — TIMBERDOODLE
    pub(super) fn free_user_pages_with(
        root: u64,
        start: u64,
        len: u64,
        read: &mut dyn FnMut(u64) -> u64,
    ) -> usize {
        let root = root & PA_MASK;
        let Some(end) = start.checked_add(len) else {
            return 0;
        };
        if root == 0
            || root % PAGE_SIZE != 0
            || len == 0
            || start % PAGE_SIZE != 0
            || len % PAGE_SIZE != 0
        {
            return 0;
        }
        let mut freed = 0;
        let mut va = start;
        while va < end {
            if let Some(frame) = leaf_frame(root, va, read) {
                if free_frame(frame) {
                    freed += 1;
                }
            }
            va += PAGE_SIZE;
        }
        freed
    }

    /// Translate one 4 KiB page from a 48-bit VA space. Every level must be a table
    /// descriptor (`bits 1:0 == 0b11`); the L3 entry must be a page leaf. A 2 MiB block at
    /// L2 is a mapping the VMO/image path owns, not an allocator frame, so it is skipped.
    fn leaf_frame(root: u64, va: u64, read: &mut dyn FnMut(u64) -> u64) -> Option<u64> {
        let l1 = read(root + ((va >> 39) & 0x1ff) * 8);
        if l1 & 0b11 != 0b11 {
            return None;
        }
        let l1 = l1 & PA_MASK;
        if l1 == 0 {
            return None;
        }
        let l2 = read(l1 + ((va >> 30) & 0x1ff) * 8);
        if l2 & 0b11 != 0b11 {
            return None;
        }
        let l2 = l2 & PA_MASK;
        if l2 == 0 {
            return None;
        }
        let l3 = read(l2 + ((va >> 21) & 0x1ff) * 8);
        if l3 & 0b11 != 0b11 {
            return None;
        }
        let l3 = l3 & PA_MASK;
        if l3 == 0 {
            return None;
        }
        let leaf = read(l3 + ((va >> 12) & 0x1ff) * 8);
        if leaf & 0b11 != 0b11 {
            return None;
        }
        let frame = leaf & PA_MASK;
        (frame != 0).then_some(frame)
    }

    #[cfg(target_os = "none")]
    pub(super) fn free_user_pages(root: u64, start: u64, len: u64) -> usize {
        free_user_pages_with(root, start, len, &mut read_phys_u64)
    }
}

/// Retire a user address space: invalidate and release its ASID through the HAL, then free
/// the anonymous leaf pages the space mapped outside a VMO (`user_stack`, installed by
/// AddressSpaceCreate) and the L0-L3 table frames for every non-kernel ASID. ASID 0 is the
/// kernel's identity map and owns no user tables, so nothing is walked. Returns whether the
/// HAL found a live ASID.
///
/// The order is the teardown safety contract: mappings were cleared from the live tree
/// while it still existed, then the ASID was invalidated (HAL `release_asid` invalidates
/// before returning the number), and only then may frames be queued for reuse. The stack
/// leaves go first because the VA walk needs the still-linked tables; the table walk then
/// unlinks the frames that named them. Anonymous VMO leaf frames are freed later by
/// `retire_vmos`, after this call — still after the ASID.
#[cfg(all(feature = "arch_aarch64", target_os = "none"))]
pub fn retire_user_aspace(root: u64, user_stack: Option<(u64, u64)>) -> bool {
    let live = kumo_hal::active::retire_user_aspace(root);
    let asid = ((root >> 48) & 0xffff) as u16;
    if live && asid != 0 {
        if let Some((start, len)) = user_stack {
            user_tables::free_user_pages(root, start, len);
        }
        user_tables::free_user_tables(root);
    }
    live
}

/// Non-AArch64 kernel builds have no tagged user tables to walk; keep the HAL contract.
#[cfg(not(all(feature = "arch_aarch64", target_os = "none")))]
pub fn retire_user_aspace(root: u64, _user_stack: Option<(u64, u64)>) -> bool {
    kumo_hal::active::retire_user_aspace(root)
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

fn boot_framebuffer_range(boot: &BootInfo) -> Range {
    if boot.has_framebuffer() {
        Range::new(boot.framebuffer.phys, boot.framebuffer.len)
    } else {
        Range::empty()
    }
}

/// Whether the permanent AArch64 coarse map will give this aligned 2 MiB block one unambiguous
/// Normal-WB identity/physmap alias. The current conservative policy requires one RAM descriptor
/// to contain the whole block; adjacent fragments are not treated as proof that every byte has
/// the same memory type without a page-granular mapper.
/// A framebuffer overlap wins and makes the whole block Normal-NC in the HAL, so it is not a safe
/// source for page tables or ordinary user RAM even outside the exact scanout bytes.
fn normal_identity_block(regions: &[MemRegion], framebuffer: Range, block: u64) -> bool {
    if block & (BLOCK_2M - 1) != 0 || overlaps(block, BLOCK_2M, framebuffer.start, framebuffer.len)
    {
        return false;
    }
    let Some(end) = block.checked_add(BLOCK_2M) else {
        return false;
    };
    regions.iter().any(|region| {
        is_ram_kind(region.kind) && region.range.start <= block && end <= region.range.end()
    })
}

/// Build KUMO-owned page tables with frames supplied by `alloc`, then switch to them.
/// RAM becomes cacheable, while unlisted/MMIO ranges and the framebuffer receive the
/// architecture's conservative device policy.
///
/// # Safety
///
/// Must run once while the bootstrap identity map is active. `boot.mem_regions` must be
/// the validated handoff slice and remain readable. Every frame returned by `alloc` must
/// be unique, zeroable, and reachable through the active bootstrap map.
pub unsafe fn enable_paging_with_allocator(
    boot: &BootInfo,
    alloc: &mut dyn FnMut() -> Option<u64>,
) -> Option<PagingReport> {
    let regions = unsafe { boot.mem_regions.as_slice() };
    if regions.is_empty() {
        return None;
    }

    let mut top = 0u64;
    for region in regions {
        top = top.max(region.range.end());
    }
    let framebuffer = boot_framebuffer_range(boot);
    let (fb_phys, fb_len) = (framebuffer.start, framebuffer.len);
    top = top.max(fb_phys.saturating_add(fb_len));
    top = top.div_ceil(GIB).saturating_mul(GIB);
    if top == 0 {
        return None;
    }

    // Share the exact block predicate with AArch64 fresh-frame allocation. If these two policies
    // drift, a frame can acquire a Device/Normal or NC/Normal alias when it becomes a page table
    // or user page — architecturally invalid even when QEMU happens to accept it.
    let is_ram = |pa: u64| normal_identity_block(regions, framebuffer, pa);

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
            alloc,
        )
    }
    .ok()?;
    Some(PagingReport {
        tables,
        mapped_bytes,
    })
}

/// Build permanent kernel paging using the shared BootInfo frame allocator.
///
/// # Safety
/// See [`enable_paging_with_allocator`]. The active bootstrap map must cover frames yielded
/// from `boot`; frames spent here remain excluded from every later allocation by the global
/// watermark.
pub unsafe fn enable_paging(boot: &BootInfo) -> Option<PagingReport> {
    let mut alloc = || -> Option<u64> { unsafe { alloc_zeroed_frame(boot) } };
    unsafe { enable_paging_with_allocator(boot, &mut alloc) }
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
    /// Always mapped Normal-WB to match the kernel physmap alias.
    PhysicalRam { phys_base: u64 },
    /// Resource-minted MMIO. Maps Device-nGnRnE unless the caller explicitly
    /// requests the framebuffer-specific Normal-NC policy.
    Mmio { phys_base: u64 },
    /// Physically-contiguous RAM allocated for device DMA (xHCI rings/DCBAA/buffers).
    /// Unlike [`Self::PhysicalRam`] it *may* be mapped Normal-NC (uncached) so a driver
    /// gets coherency-free DMA without cache maintenance; the frames are cleaned to PoC at
    /// allocation so the NC alias and the device see the zeroed contents, not stale WB lines.
    Dma { phys_base: u64 },
    /// Eagerly allocated, zeroed and cleaned DMA pages stored in the VMO entry.
    DmaPaged,
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

    pub fn from_dma_range(phys_base: u64, len: u64) -> Result<Self, MemoryError> {
        if len == 0 {
            return Err(MemoryError::Empty);
        }
        if !is_page_aligned(phys_base) {
            return Err(MemoryError::Unaligned);
        }
        Ok(Self {
            len: align_up(len).ok_or(MemoryError::InvalidRange)?,
            backing: VmoBacking::Dma { phys_base },
        })
    }

    pub fn new_dma_paged(len: u64) -> Result<Self, MemoryError> {
        let mut vmo = Self::new(len)?;
        vmo.backing = VmoBacking::DmaPaged;
        Ok(vmo)
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
    additional_exclusions: &'a [Range],
    region_index: usize,
    cursor: u64,
    limit: u64,
}

impl<'a> BootFrameAllocator<'a> {
    pub fn new(regions: &'a [MemRegion], exclusions: &'a [Range]) -> Self {
        let mut allocator = Self {
            regions,
            exclusions,
            additional_exclusions: &[],
            region_index: 0,
            cursor: 0,
            limit: 0,
        };
        allocator.advance_to_usable_region();
        allocator
    }

    /// Set an additional exclusion slice without replacing the boot's reserved spans.
    pub fn excluding(mut self, ranges: &'a [Range]) -> Self {
        self.additional_exclusions = ranges;
        self
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

    /// Return the base of the next run of `count` frames whose physical addresses are
    /// adjacent (no exclusion or region gap between them), consuming every frame up to and
    /// including the run. Frames scanned before a run breaks are discarded — this is a
    /// forward bump allocator, so a rejected prefix is simply skipped, never revisited.
    ///
    /// `count == 0` yields `None`. The returned base is the first frame of the run; the
    /// caller owns `[base, base + count * PAGE_SIZE)`.
    pub fn next_run(&mut self, count: u64) -> Option<PhysFrame> {
        if count == 0 {
            return None;
        }
        let mut base: Option<u64> = None;
        let mut have: u64 = 0;
        let mut prev: u64 = 0;
        loop {
            let frame = self.next_frame()?.start;
            match base {
                Some(_) if frame == prev.saturating_add(PAGE_SIZE) => have += 1,
                _ => {
                    base = Some(frame);
                    have = 1;
                }
            }
            prev = frame;
            if have == count {
                return Some(PhysFrame {
                    start: base.expect("run base set once have > 0"),
                });
            }
        }
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
            .chain(self.additional_exclusions)
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

//j825
/// Serializes host tests that touch the process-global reuse list across modules (the mm
/// recreation tests and the syscall Anonymous-retirement test). — TIMBERDOODLE
#[cfg(test)]
pub(crate) static FREE_FRAME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn clear_free_frames_for_test() {
    // SAFETY: every caller holds FREE_FRAME_TEST_LOCK.
    unsafe { (&mut *FREE_FRAMES.0.get()).clear() };
}

#[cfg(test)]
mod tests {

    use super::*;
    use kumo_abi::{RawSlice, ABI_VERSION};

    #[test]
    fn additional_frame_exclusions_preserve_boot_reservations_and_split_runs() {
        let regions = [MemRegion {
            range: Range::new(0x1000, 0x8000),
            kind: MemRegionKind::Usable,
            _reserved: 0,
        }];
        let boot_reserved = [Range::new(0x2000, PAGE_SIZE)];
        let aliases = [Range::new(0x3fff, 2), Range::new(0x6000, PAGE_SIZE)];
        let allocator = BootFrameAllocator::new(&regions, &boot_reserved).excluding(&aliases);
        let mut singles = allocator.clone();
        let mut got = Vec::new();
        while let Some(frame) = singles.next_frame() {
            got.push(frame.start);
        }
        assert_eq!(got, [0x1000, 0x5000, 0x7000, 0x8000]);
        assert_eq!(allocator.clone().count_remaining(), 4);
        assert_eq!(
            allocator.clone().next_run(2),
            Some(PhysFrame { start: 0x7000 })
        );
        assert_eq!(allocator.clone().next_run(3), None);
    }

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
    fn next_run_finds_a_contiguous_span_and_skips_gaps() {
        // One usable region with a hole punched by an exclusion at 0x3000. The first two
        // frames (0x1000,0x2000) are adjacent; 0x3000 is excluded; then 0x4000..0x6000 form
        // a three-frame run.
        let regions = [MemRegion {
            range: Range::new(0x1000, 0x6000),
            kind: MemRegionKind::Usable,
            _reserved: 0,
        }];
        let exclusions = [Range::new(0x3000, 0x1000)];

        // A run of 3 cannot start at 0x1000 (only two before the hole); it lands at 0x4000.
        let mut allocator = BootFrameAllocator::new(&regions, &exclusions);
        assert_eq!(allocator.next_run(3), Some(PhysFrame { start: 0x4000 }));

        // A run of 2 is satisfied by the very first adjacent pair.
        let mut allocator = BootFrameAllocator::new(&regions, &exclusions);
        assert_eq!(allocator.next_run(2), Some(PhysFrame { start: 0x1000 }));

        // A run longer than any adjacent span is unsatisfiable.
        let mut allocator = BootFrameAllocator::new(&regions, &exclusions);
        assert_eq!(allocator.next_run(4), None);

        // count == 0 is rejected.
        let mut allocator = BootFrameAllocator::new(&regions, &exclusions);
        assert_eq!(allocator.next_run(0), None);
    }

    #[test]
    fn next_run_of_one_matches_next_frame() {
        let regions = [MemRegion {
            range: Range::new(0x1000, 0x2000),
            kind: MemRegionKind::Usable,
            _reserved: 0,
        }];
        let mut allocator = BootFrameAllocator::new(&regions, &[]);
        assert_eq!(allocator.next_run(1), Some(PhysFrame { start: 0x1000 }));
        assert_eq!(allocator.next_run(1), Some(PhysFrame { start: 0x2000 }));
        assert_eq!(allocator.next_run(1), None);
    }

    #[test]
    fn dma_backing_is_a_contiguous_physical_range() {
        let vmo = Vmo::from_dma_range(0xc000, PAGE_SIZE * 3).unwrap();
        assert_eq!(vmo.backing(), VmoBacking::Dma { phys_base: 0xc000 });
        assert_eq!(vmo.frame_count(), 3);
        assert_eq!(
            Vmo::from_dma_range(0xc001, PAGE_SIZE),
            Err(MemoryError::Unaligned)
        );
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

    /// Pi-style UEFI fragmentation must never let the fresh-frame path choose a page whose
    /// containing coarse identity block is Device/NC while TTBR walks and user mappings call the
    /// same PA Normal-WB. Keep this predicate identical to `enable_paging`'s block classifier.
    #[test]
    fn normal_identity_blocks_reject_fragments_and_framebuffer_aliases() {
        let regions = [
            // A usable boundary fragment: candidate pages exist, but block 0 is not wholly RAM.
            MemRegion {
                range: Range::new(0x1000, BLOCK_2M - 0x1000),
                kind: MemRegionKind::Usable,
                _reserved: 0,
            },
            // One complete aligned block: the first safe source for fresh AArch64 frames.
            MemRegion {
                range: Range::new(BLOCK_2M, BLOCK_2M),
                kind: MemRegionKind::Usable,
                _reserved: 0,
            },
            // Two adjacent RAM descriptors are not one conservatively proven L2 RAM block.
            MemRegion {
                range: Range::new(2 * BLOCK_2M, BLOCK_2M / 2),
                kind: MemRegionKind::Usable,
                _reserved: 0,
            },
            MemRegion {
                range: Range::new(2 * BLOCK_2M + BLOCK_2M / 2, BLOCK_2M / 2),
                kind: MemRegionKind::Bootloader,
                _reserved: 0,
            },
        ];

        assert!(!normal_identity_block(&regions, Range::empty(), 0));
        assert!(normal_identity_block(&regions, Range::empty(), BLOCK_2M));
        assert!(!normal_identity_block(
            &regions,
            Range::empty(),
            2 * BLOCK_2M
        ));
        assert!(!normal_identity_block(
            &regions,
            Range::new(BLOCK_2M + 0x1f_0000, PAGE_SIZE),
            BLOCK_2M
        ));
        assert!(!normal_identity_block(
            &regions,
            Range::empty(),
            BLOCK_2M + PAGE_SIZE
        ));
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

    #[test]
    fn the_reuse_list_defers_rejected_frames_and_never_takes_junk() {
        let _serial = super::FREE_FRAME_TEST_LOCK.lock().unwrap();
        super::clear_free_frames_for_test();
        assert!(super::free_frame(0x10_0000));
        assert!(super::free_frame(0x10_1000));
        assert!(super::free_frame(0x10_2000));
        assert_eq!(super::free_frame_count(), 3);
        assert!(!super::free_frame(0));
        assert!(!super::free_frame(0x10_0001));
        // The newest usable frame is taken; a rejected frame stays queued for later.
        assert_eq!(super::take_free_frame(|f| f != 0x10_2000), Some(0x10_1000));
        assert_eq!(super::free_frame_count(), 2);
        assert_eq!(super::take_free_frame(|f| f != 0x10_2000), Some(0x10_0000));
        assert_eq!(super::take_free_frame(|_| true), Some(0x10_2000));
        assert_eq!(super::take_free_frame(|_| true), None);
        assert_eq!(super::free_frame_count(), 0);
        super::clear_free_frames_for_test();
    }

    #[cfg(feature = "arch_aarch64")]
    #[test]
    fn user_table_walk_frees_tables_child_first_and_root_last() {
        let _serial = super::FREE_FRAME_TEST_LOCK.lock().unwrap();
        super::clear_free_frames_for_test();
        // A four-level tree in a fake physical address space:
        //   root L0 -> L1 -> L2 -> L3, plus an L2 block leaf that must not be freed.
        let mut mem = alloc::vec![0u64; 0x10000 / 8];
        let at = |pa: u64| (pa / 8) as usize;
        let root = 0x1000u64;
        let l1 = 0x2000u64;
        let l2 = 0x3000u64;
        let l3 = 0x4000u64;
        let block_leaf = 0x0080_0000u64;
        let page_leaf = 0x0090_0000u64;
        mem[at(root)] = l1 | 0b11;
        mem[at(l1)] = l2 | 0b11;
        mem[at(l2 + 2 * 8)] = l3 | 0b11;
        mem[at(l2 + 5 * 8)] = block_leaf | 0b01;
        mem[at(l3)] = page_leaf | 0b11;
        let mut read = |pa: u64| mem[at(pa)];
        let freed = super::user_tables::free_user_tables_with(root, &mut read);
        assert_eq!(freed, 4);
        // Free order was L3, L2, L1, root; the stack returns them in reverse.
        let mut popped = alloc::vec::Vec::new();
        while let Some(frame) = super::take_free_frame(|_| true) {
            popped.push(frame);
        }
        popped.reverse();
        assert_eq!(popped, alloc::vec![l3, l2, l1, root]);
        // Neither leaf (block or page) entered the reuse list.
        assert!(!popped.contains(&block_leaf) && !popped.contains(&page_leaf));
        assert_eq!(super::free_frame_count(), 0);
        super::clear_free_frames_for_test();
    }

    #[cfg(feature = "arch_aarch64")]
    #[test]
    fn user_stack_walk_frees_only_the_pages_it_owns() {
        let _serial = super::FREE_FRAME_TEST_LOCK.lock().unwrap();
        super::clear_free_frames_for_test();
        // A four-level tree in a fake physical address space. L2 index 2 names the L3 table
        // the stack range lands in; index 5 names a block that must be skipped on sight.
        let mut mem = alloc::vec![0u64; 0x10000 / 8];
        let at = |pa: u64| (pa / 8) as usize;
        let root = 0x1000u64;
        let l1 = 0x2000u64;
        let l2 = 0x3000u64;
        let l3 = 0x4000u64;
        let stack_leaf = 0x0090_0000u64;
        let neighbour_leaf = 0x0090_2000u64;
        let block_leaf = 0x0080_0000u64;
        mem[at(root)] = l1 | 0b11;
        mem[at(l1)] = l2 | 0b11;
        mem[at(l2 + 2 * 8)] = l3 | 0b11;
        mem[at(l2 + 5 * 8)] = block_leaf | 0b01;
        mem[at(l3)] = stack_leaf | 0b11;
        mem[at(l3 + 2 * 8)] = neighbour_leaf | 0b11;
        let mut read = |pa: u64| mem[at(pa)];
        let stack = 0x0040_0000u64; // L0 0, L1 0, L2 2, L3 0

        // Two pages: page 0 is the stack leaf, page 1 is a hole. Only the leaf is freed,
        // and the neighbour outside the range stays untouched.
        assert_eq!(
            super::user_tables::free_user_pages_with(root, stack, 2 * super::PAGE_SIZE, &mut read),
            1
        );
        assert_eq!(super::take_free_frame(|_| true), Some(stack_leaf));
        assert_eq!(super::take_free_frame(|_| true), None);

        // A hole alone frees nothing.
        assert_eq!(
            super::user_tables::free_user_pages_with(
                root,
                stack + super::PAGE_SIZE,
                super::PAGE_SIZE,
                &mut read
            ),
            0
        );

        // The L2 block's VA frees nothing: a block is a mapping, not an allocator leaf.
        let block_va = 0x00a0_0000u64;
        assert_eq!(
            super::user_tables::free_user_pages_with(root, block_va, super::PAGE_SIZE, &mut read),
            0
        );

        // The neighbour outside the range stays put, and invalid ranges are refused.
        assert_eq!(
            super::user_tables::free_user_pages_with(
                root,
                stack + 2 * super::PAGE_SIZE,
                super::PAGE_SIZE,
                &mut read
            ),
            1
        );
        assert_eq!(super::take_free_frame(|_| true), Some(neighbour_leaf));
        assert_eq!(super::take_free_frame(|_| true), None);
        assert_eq!(
            super::user_tables::free_user_pages_with(root, stack + 1, super::PAGE_SIZE, &mut read),
            0
        );
        assert_eq!(
            super::user_tables::free_user_pages_with(root, stack, 0, &mut read),
            0
        );
        super::clear_free_frames_for_test();
    }
}
