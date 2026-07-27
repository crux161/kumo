//! Growable userland heap with a size-class front end (PLAN/008 S1).
//!
//! Two layers, both host-provable:
//! - [`HeapCore`] — a **size-class free-list front end**: small allocations (`<= 256` bytes,
//!   `align <= 16`) recycle through per-class intrusive free lists in O(1), resisting the
//!   fragmentation a heavy-alloc userland (RedoxFS's CoW B-tree churn) inflicts on a plain
//!   first-fit heap. Larger or over-aligned requests pass straight through to the backing.
//! - [`MultiRegionHeap`] — the backing: a set of **regions**, each an independent
//!   `linked_list_allocator::Heap` over a contiguous span. Region 0 is a fixed BSS floor
//!   available before any syscall runs (bootstrap). When every region is full it asks its
//!   [`RegionSource`] for another span and admits it as a new region — so the heap grows on
//!   demand instead of hitting the old fixed 64 KiB ceiling (PLAN/008 Pushback 2). `dealloc`
//!   routes a pointer back to the region that owns its address range.
//!
//! The multi-region growth *logic* is host-provable: [`MultiRegionHeap`] is generic over
//! its [`RegionSource`], so a host `#[cfg(test)]` source can hand it real spans (from
//! `std::alloc::System`) and the soak test drives growth, dealloc-routing, and the region
//! cap without any target syscalls. The source carries its own state, so parallel tests do
//! not share a growth budget.
//!
//! On the freestanding target, growth is **wired**: [`TargetRegionSource`] creates a VMO and maps
//! it with `virt == 0`, which asks the **kernel** to place it. That is the answer to the question
//! this comment used to leave open — there is no single safe hardcoded base (Sora's root VMAR is
//! 2 GiB from 0x0020_0000; child VMARs are 512 MiB from 0 and already carry image, stacks and
//! device maps), and the kernel is the only party that knows every existing mapping. So it picks,
//! and reports the address back. The first page is never handed out, which is what makes 0 a safe
//! "you choose" sentinel.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use linked_list_allocator::Heap as LinkedListHeap;

/// Bootstrap floor: region 0, a fixed BSS span available before any syscall. Raised from
/// the historical 64 KiB so current single-region boots have real headroom while on-target
/// growth is still deferred.
const INITIAL_FLOOR: usize = 256 * 1024;

/// Largest number of regions (floor + grown spans). Bounds the static bookkeeping array.
const MAX_REGIONS: usize = 32;

/// A source of fresh heap spans. `grow` returns a `(base, len)` the caller may treat as an
/// exclusive, writable, `usize`-aligned region for at least `min` bytes, or `None` when no
/// more memory can be supplied (growth cap, syscall failure). A returned span must stay
/// valid for the life of the process — grown spans are never handed back. The source owns
/// its own state (budget, VA cursor), so distinct heaps never share it.
pub trait RegionSource {
    fn grow(&mut self, min: usize) -> Option<(*mut u8, usize)>;
}

/// One contiguous heap span and the allocator that manages it. `base`/`len` are retained
/// for O(1) ownership routing on `dealloc`.
struct Region {
    heap: LinkedListHeap,
    base: *mut u8,
    len: usize,
}

impl Region {
    fn contains(&self, ptr: *mut u8) -> bool {
        let addr = ptr as usize;
        let start = self.base as usize;
        addr >= start && addr < start + self.len
    }
}

/// A heap composed of one or more [`Region`]s that grows on demand via a [`RegionSource`].
pub struct MultiRegionHeap<S: RegionSource> {
    regions: [Option<Region>; MAX_REGIONS],
    count: usize,
    source: S,
}

impl<S: RegionSource> MultiRegionHeap<S> {
    /// Build a heap whose first region spans `[base, base+len)` and which grows via `source`.
    ///
    /// # Safety
    /// `base`/`len` must describe a span this heap may own exclusively for the process
    /// lifetime (no aliasing, writable, `usize`-aligned).
    pub const unsafe fn new(base: *mut u8, len: usize, source: S) -> Self {
        let mut regions = [const { None }; MAX_REGIONS];
        regions[0] = Some(Region {
            heap: LinkedListHeap::empty(),
            base,
            len,
        });
        Self {
            regions,
            count: 1,
            source,
        }
    }

    /// Initialize region 0's allocator over its span.
    ///
    /// # Safety
    /// The region-0 span passed to [`new`](Self::new) must be valid and unaliased, and this
    /// must run exactly once (callers guard with a flag).
    unsafe fn init_floor(&mut self) {
        if let Some(region) = self.regions[0].as_mut() {
            unsafe { region.heap.init(region.base, region.len) };
        }
    }

    /// Try to satisfy `layout` from an already-admitted region.
    fn alloc_existing(&mut self, layout: Layout) -> *mut u8 {
        for slot in self.regions[..self.count].iter_mut() {
            if let Some(region) = slot.as_mut() {
                if let Ok(allocation) = region.heap.allocate_first_fit(layout) {
                    return allocation.as_ptr();
                }
            }
        }
        core::ptr::null_mut()
    }

    /// Ask the source for a span large enough for `layout` plus allocator overhead, admit
    /// it as a new region, and return that region's index — or `None` if the source is
    /// tapped out or the region cap is reached.
    fn grow_for(&mut self, layout: Layout) -> Option<usize> {
        if self.count >= MAX_REGIONS {
            return None;
        }
        // A grown span must hold the request AND the linked-list allocator's own node
        // overhead, with slack so a single large alloc plus alignment still fits.
        let need = layout
            .size()
            .saturating_add(layout.align())
            .saturating_add(2 * core::mem::size_of::<usize>());
        let (base, len) = self.source.grow(need)?;
        if base.is_null() || len < need {
            return None;
        }
        let index = self.count;
        let mut heap = LinkedListHeap::empty();
        // SAFETY: the source contract guarantees `[base, base+len)` is an exclusive,
        // writable, aligned span valid for the process lifetime.
        unsafe { heap.init(base, len) };
        self.regions[index] = Some(Region { heap, base, len });
        self.count += 1;
        Some(index)
    }

    /// Allocate `layout`, growing by one region if the existing ones cannot satisfy it.
    fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let existing = self.alloc_existing(layout);
        if !existing.is_null() {
            return existing;
        }
        match self.grow_for(layout) {
            Some(index) => self.regions[index]
                .as_mut()
                .and_then(|region| region.heap.allocate_first_fit(layout).ok())
                .map_or(core::ptr::null_mut(), |allocation| allocation.as_ptr()),
            None => core::ptr::null_mut(),
        }
    }

    /// Free `ptr` back to the region that owns its address range.
    ///
    /// # Safety
    /// `ptr`/`layout` must come from a prior [`alloc`](Self::alloc) on this heap.
    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        let Some(nonnull) = core::ptr::NonNull::new(ptr) else {
            return;
        };
        for slot in self.regions[..self.count].iter_mut() {
            if let Some(region) = slot.as_mut() {
                if region.contains(ptr) {
                    unsafe { region.heap.deallocate(nonnull, layout) };
                    return;
                }
            }
        }
        // A pointer owned by no region is a caller contract violation; drop it rather than
        // corrupt an unrelated allocator (leaks in release, trips the test in debug).
        debug_assert!(false, "dealloc of pointer owned by no heap region");
    }

    /// Number of admitted regions (floor + grown).
    fn region_count(&self) -> usize {
        self.count
    }
}

/// Size classes served by the fast free-list front end (bytes). Powers of two so a block
/// carved for a class satisfies any request that rounds up to it; 16-byte alignment covers
/// the common `align <= 16` allocations (Box/Vec of primitives), which is why over-aligned
/// requests bypass the cache.
const CLASS_SIZES: [usize; 5] = [16, 32, 64, 128, 256];
const NUM_CLASSES: usize = CLASS_SIZES.len();
const CLASS_ALIGN: usize = 16;
/// Cap on retained free blocks per class. Bounds the memory the cache holds back from the
/// backing under churn — worst case `sum(CLASS_SIZES) * RETAIN_CAP` — so a burst of frees in
/// one class cannot hoard the whole heap (the slab tradeoff PLAN/008 S1 calls out).
const RETAIN_CAP: usize = 64;

/// An intrusive singly-linked free list of same-class blocks: each free block's first word
/// holds the next pointer. Blocks are `>= 16` bytes, always big enough for the pointer.
struct FreeList {
    head: *mut u8,
    len: usize,
}

impl FreeList {
    const fn new() -> Self {
        Self {
            head: core::ptr::null_mut(),
            len: 0,
        }
    }

    /// Pop a recycled block, or null when the list is empty.
    fn pop(&mut self) -> *mut u8 {
        if self.head.is_null() {
            return core::ptr::null_mut();
        }
        let block = self.head;
        // SAFETY: `head` is a live free block whose first word is the next pointer (written
        // by `push`); the block is >= 16 bytes so the read is in-bounds.
        self.head = unsafe { *(block as *const *mut u8) };
        self.len -= 1;
        block
    }

    /// Retain `block` if under the cap. Returns false when full, so the caller frees the
    /// block to the backing instead of hoarding it.
    fn push(&mut self, block: *mut u8) -> bool {
        if self.len >= RETAIN_CAP {
            return false;
        }
        // SAFETY: `block` is an exclusively-owned free span >= 16 bytes; writing the next
        // pointer into its first word is in-bounds and cannot alias a live allocation.
        unsafe { *(block as *mut *mut u8) = self.head };
        self.head = block;
        self.len += 1;
        true
    }
}

/// Index of the smallest class that fits `layout`, or `None` when the request cannot use the
/// cache (larger than the biggest class, or aligned beyond [`CLASS_ALIGN`]).
fn class_index(layout: Layout) -> Option<usize> {
    if layout.align() > CLASS_ALIGN {
        return None;
    }
    let size = layout.size().max(1);
    CLASS_SIZES.iter().position(|&class| size <= class)
}

/// The layout a class's backing block is carved and freed with — fixed per class so the
/// backing allocator always sees a matching alloc/dealloc pair for a recycled block.
fn class_layout(index: usize) -> Layout {
    // SAFETY: `CLASS_ALIGN` is a power of two and `CLASS_SIZES[index]` rounded up to it does
    // not overflow, so the layout is always valid — avoids a panic path in the allocator.
    unsafe { Layout::from_size_align_unchecked(CLASS_SIZES[index], CLASS_ALIGN) }
}

/// The full userland heap: a size-class free-list front end over a growable
/// [`MultiRegionHeap`] backing. Small allocations recycle through the per-class lists (O(1),
/// fragmentation-resistant); everything else goes straight to the backing.
struct HeapCore<S: RegionSource> {
    classes: [FreeList; NUM_CLASSES],
    backing: MultiRegionHeap<S>,
}

impl<S: RegionSource> HeapCore<S> {
    /// # Safety
    /// Same contract as [`MultiRegionHeap::new`] for `base`/`len`.
    const unsafe fn new(base: *mut u8, len: usize, source: S) -> Self {
        Self {
            classes: [const { FreeList::new() }; NUM_CLASSES],
            // SAFETY: forwarded to the caller of `HeapCore::new`.
            backing: unsafe { MultiRegionHeap::new(base, len, source) },
        }
    }

    /// # Safety
    /// Must run exactly once; see [`MultiRegionHeap::init_floor`].
    unsafe fn init_floor(&mut self) {
        unsafe { self.backing.init_floor() };
    }

    fn alloc(&mut self, layout: Layout) -> *mut u8 {
        if let Some(index) = class_index(layout) {
            let recycled = self.classes[index].pop();
            if !recycled.is_null() {
                return recycled;
            }
            // Cold class: carve a fresh block sized/aligned for the whole class so it can be
            // recycled for any request that maps here.
            return self.backing.alloc(class_layout(index));
        }
        self.backing.alloc(layout)
    }

    /// # Safety
    /// `ptr`/`layout` must come from a prior [`alloc`](Self::alloc) on this heap.
    unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        if let Some(index) = class_index(layout) {
            if self.classes[index].push(ptr) {
                return;
            }
            // Over the retention cap: hand the class-sized block back to the backing.
            unsafe { self.backing.dealloc(ptr, class_layout(index)) };
            return;
        }
        unsafe { self.backing.dealloc(ptr, layout) };
    }

    /// Retained free-block count for a class. Test/observability only.
    #[cfg(test)]
    fn cache_len(&self, index: usize) -> usize {
        self.classes[index].len
    }

    /// Number of backing regions (floor + grown). The one observable that distinguishes a heap
    /// which *can* grow from one that only claims to, so it is available on target too.
    fn region_count(&self) -> usize {
        self.backing.region_count()
    }
}

// The bootstrap floor must be interior-mutable: the allocator hands out `*mut u8` pointers
// into it and writes through them. A plain `static [u8; N]` is immutable — casting `&` to
// `*mut` and writing is UB (recorded in the J221/223/224 heap history). `UnsafeCell` tells
// the compiler the contents are mutable.
#[repr(align(16))]
struct HeapMem(UnsafeCell<[u8; INITIAL_FLOOR]>);

// SAFETY: single-threaded on target; serialized by the KumoHeap spinlock on host.
unsafe impl Sync for HeapMem {}

static HEAP: HeapMem = HeapMem(UnsafeCell::new([0; INITIAL_FLOOR]));

// On the host `cargo test` runs multiple test binaries in one process; the
// linked_list_allocator is not thread-safe so concurrent alloc/dealloc can observe torn
// freelist state (the "Freed node aliases existing hole" panic recorded in J221, J223,
// J224). A simple spinlock serializes access without pulling in std. On the freestanding
// target the EL0 process is single-threaded and the lock compiles to nothing.
#[cfg(not(target_os = "none"))]
mod sync {
    use core::sync::atomic::{AtomicBool, Ordering};

    pub struct SpinLock {
        locked: AtomicBool,
    }

    impl SpinLock {
        pub const fn new() -> Self {
            Self {
                locked: AtomicBool::new(false),
            }
        }

        pub fn lock(&self) {
            while self.locked.swap(true, Ordering::Acquire) {
                core::hint::spin_loop();
            }
        }

        pub fn unlock(&self) {
            self.locked.store(false, Ordering::Release);
        }
    }
}

#[cfg(target_os = "none")]
mod sync {
    pub struct SpinLock;

    impl SpinLock {
        pub const fn new() -> Self {
            Self
        }
        pub fn lock(&self) {}
        pub fn unlock(&self) {}
    }
}

/// The process global-allocator's growth source.
///
/// On-target heap growth beyond the floor is **deferred** (PLAN/008 S1 next sub-slice): it
/// needs a per-process heap-growth VA window (`vmar_map` takes an explicit VA and there is
/// no single base safe for both Sora's 2 GiB root VMAR and a child's 512 MiB VMAR), so it is
/// designed in its own slice rather than guessed here. On the host this keeps the global
/// `KumoHeap` within its floor (the growth *path* is proven separately by the tests below,
/// which drive a `MultiRegionHeap` with an explicit `System`-backed source).
struct TargetRegionSource;

impl RegionSource for TargetRegionSource {
    fn grow(&mut self, min: usize) -> Option<(*mut u8, usize)> {
        // Round the request up to whole pages and to a sensible minimum: one syscall pair per
        // allocation would make growth cost more than the allocation it serves.
        const PAGE: usize = 4096;
        const GROWTH_MIN: usize = 256 * 1024;
        let want = min.max(GROWTH_MIN).checked_add(PAGE - 1)? & !(PAGE - 1);

        let vmo = crate::sys::vmo_create(want as u64);
        if vmo == u64::MAX || vmo == 0 {
            return None;
        }
        // The kernel chooses the address: this process cannot know what is already mapped in its
        // own VMAR, and the previous sub-slice declined — correctly — to hardcode a base.
        let (status, addr) = crate::sys::vmar_map_anywhere(
            kumo_abi::Handle(vmo as u32),
            want as u64,
            (kumo_abi::VmarFlags::READ | kumo_abi::VmarFlags::WRITE).0,
        );
        if status != 0 || addr == 0 {
            return None;
        }
        Some((addr as *mut u8, want))
    }
}

pub struct KumoHeap {
    inner: UnsafeCell<HeapCore<TargetRegionSource>>,
    initialized: AtomicBool,
    lock: sync::SpinLock,
}

unsafe impl Sync for KumoHeap {}

impl KumoHeap {
    pub const fn empty() -> Self {
        Self {
            // SAFETY: `HEAP` is a process-lifetime, unaliased, 16-aligned BSS span; it is
            // this heap's exclusive region 0.
            inner: UnsafeCell::new(unsafe {
                HeapCore::new(HEAP.0.get() as *mut u8, INITIAL_FLOOR, TargetRegionSource)
            }),
            initialized: AtomicBool::new(false),
            lock: sync::SpinLock::new(),
        }
    }

    /// How many regions back this heap: 1 is the bootstrap floor alone, more means growth
    /// actually happened. The only observable proof that the target growth path ran.
    pub fn region_count(&self) -> usize {
        self.ensure_init();
        let _guard = self.lock.lock();
        unsafe { (*self.inner.get()).region_count() }
    }

    fn ensure_init(&self) {
        if !self.initialized.swap(true, Ordering::SeqCst) {
            unsafe { (*self.inner.get()).init_floor() };
        }
    }
}

unsafe impl GlobalAlloc for KumoHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.lock.lock();
        self.ensure_init();
        let result = unsafe { (*self.inner.get()).alloc(layout) };
        self.lock.unlock();
        result
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.lock.lock();
        unsafe { (*self.inner.get()).dealloc(ptr, layout) };
        self.lock.unlock();
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::vec::Vec;

    // A host region source backed by the system allocator, with per-instance state so
    // parallel tests never share a growth budget. Each call leaks a fresh span so the region
    // stays valid for the whole test (grown spans are never returned).
    const HOST_CHUNK: usize = 64 * 1024;

    struct HostSource {
        grows_allowed: usize,
        grows_taken: usize,
    }

    impl HostSource {
        fn new(grows_allowed: usize) -> Self {
            Self {
                grows_allowed,
                grows_taken: 0,
            }
        }
    }

    impl RegionSource for HostSource {
        fn grow(&mut self, min: usize) -> Option<(*mut u8, usize)> {
            if self.grows_taken >= self.grows_allowed {
                return None;
            }
            let len = min.max(HOST_CHUNK);
            let layout = Layout::from_size_align(len, 16).ok()?;
            // SAFETY: non-zero size; the span is intentionally leaked (process-lifetime).
            let base = unsafe { std::alloc::System.alloc(layout) };
            if base.is_null() {
                return None;
            }
            self.grows_taken += 1;
            Some((base, len))
        }
    }

    // Build a heap whose region 0 is a small leaked System span, so the test never touches
    // the process global allocator's own floor.
    fn heap_with_floor(floor: usize, grows_allowed: usize) -> MultiRegionHeap<HostSource> {
        let layout = Layout::from_size_align(floor, 16).unwrap();
        let base = unsafe { std::alloc::System.alloc(layout) };
        assert!(!base.is_null());
        let mut heap = unsafe { MultiRegionHeap::new(base, floor, HostSource::new(grows_allowed)) };
        unsafe { heap.init_floor() };
        heap
    }

    #[test]
    fn serves_small_allocations_from_the_floor_without_growing() {
        let mut heap = heap_with_floor(16 * 1024, 8);
        let layout = Layout::from_size_align(64, 8).unwrap();
        let p = heap.alloc(layout);
        assert!(!p.is_null());
        assert_eq!(
            heap.region_count(),
            1,
            "a small alloc must not grow the heap"
        );
        unsafe { heap.dealloc(p, layout) };
    }

    #[test]
    fn grows_a_new_region_when_the_floor_is_exhausted() {
        // Tiny floor so a handful of allocations forces growth.
        let mut heap = heap_with_floor(2 * 1024, 8);
        let layout = Layout::from_size_align(512, 8).unwrap();
        let mut ptrs = Vec::new();
        for _ in 0..64 {
            let p = heap.alloc(layout);
            assert!(
                !p.is_null(),
                "growth must keep allocations succeeding past the floor"
            );
            ptrs.push(p);
        }
        assert!(
            heap.region_count() > 1,
            "sustained allocation past the floor must admit new regions",
        );
        for p in ptrs {
            unsafe { heap.dealloc(p, layout) };
        }
    }

    #[test]
    fn dealloc_routes_across_regions_and_memory_round_trips() {
        let mut heap = heap_with_floor(2 * 1024, 32);
        // Mixed sizes and interleaved frees across region boundaries; write a byte pattern
        // and read it back to prove no two live allocations alias.
        let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
        for i in 0..200usize {
            let size = 32 + (i % 7) * 96;
            let layout = Layout::from_size_align(size, 8).unwrap();
            let p = heap.alloc(layout);
            assert!(!p.is_null(), "alloc {i} failed");
            let tag = (i & 0xff) as u8;
            unsafe { core::ptr::write_bytes(p, tag, size) };
            live.push((p, layout, tag));
            // Free an earlier one every third step to churn the free lists.
            if i % 3 == 0 && live.len() > 4 {
                let (op, ol, _) = live.remove(1);
                unsafe { heap.dealloc(op, ol) };
            }
        }
        // Every still-live allocation must read back exactly what we wrote.
        for (p, layout, tag) in &live {
            let slice = unsafe { core::slice::from_raw_parts(*p, layout.size()) };
            assert!(
                slice.iter().all(|b| b == tag),
                "allocation corrupted / aliased"
            );
        }
        for (p, layout, _) in live {
            unsafe { heap.dealloc(p, layout) };
        }
        assert!(
            heap.region_count() > 1,
            "the churn should have forced growth"
        );
    }

    #[test]
    fn returns_null_when_the_source_is_tapped_out() {
        // No growth permitted: once the tiny floor is full, alloc must fail cleanly.
        let mut heap = heap_with_floor(1024, 0);
        let layout = Layout::from_size_align(512, 8).unwrap();
        let mut got_null = false;
        for _ in 0..32 {
            let p = heap.alloc(layout);
            if p.is_null() {
                got_null = true;
                break;
            }
        }
        assert!(
            got_null,
            "a heap with no growth source must return null when full"
        );
        assert_eq!(
            heap.region_count(),
            1,
            "no region admitted when growth is denied"
        );
    }

    // Build a full `HeapCore` (size-class front end + backing) over a leaked System floor.
    fn core_with_floor(floor: usize, grows_allowed: usize) -> HeapCore<HostSource> {
        let layout = Layout::from_size_align(floor, 16).unwrap();
        let base = unsafe { std::alloc::System.alloc(layout) };
        assert!(!base.is_null());
        let mut core = unsafe { HeapCore::new(base, floor, HostSource::new(grows_allowed)) };
        unsafe { core.init_floor() };
        core
    }

    #[test]
    fn size_class_recycles_a_freed_block_of_the_same_class() {
        let mut core = core_with_floor(16 * 1024, 4);
        let layout = Layout::from_size_align(48, 8).unwrap(); // rounds up to the 64 class
        let p1 = core.alloc(layout);
        assert!(!p1.is_null());
        unsafe { core.dealloc(p1, layout) };
        // The free went to the class list, not the backing.
        assert_eq!(core.cache_len(2), 1, "a class-sized free must be retained");
        // The next same-class alloc pops that exact block back — the O(1) recycle path.
        let p2 = core.alloc(layout);
        assert_eq!(p2, p1, "a same-class alloc must recycle the freed block");
        assert_eq!(core.cache_len(2), 0, "the recycled block leaves the list");
        unsafe { core.dealloc(p2, layout) };
    }

    #[test]
    fn size_class_returns_overflow_past_the_retention_cap_to_the_backing() {
        let mut core = core_with_floor(256 * 1024, 8);
        let layout = Layout::from_size_align(16, 8).unwrap(); // the smallest class
                                                              // Allocate then free more than the cap; the list must saturate at RETAIN_CAP and the
                                                              // remainder must go back to the backing rather than hoard unboundedly.
        let mut ptrs = Vec::new();
        for _ in 0..(RETAIN_CAP + 10) {
            let p = core.alloc(layout);
            assert!(!p.is_null());
            ptrs.push(p);
        }
        for p in ptrs {
            unsafe { core.dealloc(p, layout) };
        }
        assert_eq!(
            core.cache_len(0),
            RETAIN_CAP,
            "the class list must saturate at the retention cap"
        );
    }

    #[test]
    fn large_and_overaligned_requests_bypass_the_cache() {
        let mut core = core_with_floor(64 * 1024, 8);
        // Larger than the biggest class: served by the backing, never cached on free.
        let big = Layout::from_size_align(1024, 8).unwrap();
        let pb = core.alloc(big);
        assert!(!pb.is_null());
        unsafe { core.dealloc(pb, big) };
        // Over-aligned but small: also bypasses (align > CLASS_ALIGN).
        let aligned = Layout::from_size_align(64, 64).unwrap();
        let pa = core.alloc(aligned);
        assert!(!pa.is_null());
        assert_eq!(
            pa as usize % 64,
            0,
            "over-aligned request must honor its align"
        );
        unsafe { core.dealloc(pa, aligned) };
        for index in 0..NUM_CLASSES {
            assert_eq!(
                core.cache_len(index),
                0,
                "bypassing requests must never populate a class list"
            );
        }
    }

    #[test]
    fn size_class_round_trips_memory_under_mixed_churn() {
        let mut core = core_with_floor(4 * 1024, 32);
        let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
        for i in 0..400usize {
            // Sizes spanning several classes plus a bypass (600 > max class).
            let size = [8, 24, 100, 200, 600][i % 5];
            let layout = Layout::from_size_align(size, 8).unwrap();
            let p = core.alloc(layout);
            assert!(!p.is_null(), "alloc {i} failed");
            let tag = (i & 0xff) as u8;
            unsafe { core::ptr::write_bytes(p, tag, size) };
            live.push((p, layout, tag));
            if i % 3 == 0 && live.len() > 5 {
                let (op, ol, _) = live.remove(1);
                unsafe { core.dealloc(op, ol) };
            }
        }
        for (p, layout, tag) in &live {
            let slice = unsafe { core::slice::from_raw_parts(*p, layout.size()) };
            assert!(
                slice.iter().all(|b| b == tag),
                "live allocation corrupted / aliased through the cache"
            );
        }
        for (p, layout, _) in live {
            unsafe { core.dealloc(p, layout) };
        }
        assert!(
            core.region_count() > 1,
            "sustained churn should still drive backing growth under the cache"
        );
    }
}
