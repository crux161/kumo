//! Growable userland heap (PLAN/008 S1, first slice).
//!
//! The heap is a set of **regions**, each an independent `linked_list_allocator::Heap`
//! over a contiguous span. Region 0 is a fixed BSS floor available before any syscall
//! runs (bootstrap). When every region is full, the allocator asks its [`RegionSource`]
//! for another span and admits it as a new region — so the heap grows on demand instead
//! of hitting the old fixed 64 KiB ceiling that could not host a heavy-alloc userland
//! (RedoxFS; see PLAN/008 Pushback 2). `dealloc` routes a pointer back to the region that
//! owns its address range, so freed blocks return to the right allocator.
//!
//! The multi-region growth *logic* is host-provable: [`MultiRegionHeap`] is generic over
//! its [`RegionSource`], so a host `#[cfg(test)]` source can hand it real spans (from
//! `std::alloc::System`) and the soak test drives growth, dealloc-routing, and the region
//! cap without any target syscalls. The source carries its own state, so parallel tests do
//! not share a growth budget.
//!
//! On the freestanding target, growth beyond the floor is **not yet wired** (the
//! [`TargetRegionSource`] returns `None`): it needs a per-process heap-growth VA window, and
//! there is no single safe hardcoded base (Sora's root VMAR is 2 GiB from 0x0020_0000; child
//! VMARs are 512 MiB from 0 and already carry framebuffer/bootinfo maps). That window is
//! designed in the next S1 sub-slice rather than guessed here. Until then the raised floor
//! is the ceiling.

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

    /// Number of admitted regions (floor + grown). Test/observability only.
    #[cfg(test)]
    fn region_count(&self) -> usize {
        self.count
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
    fn grow(&mut self, _min: usize) -> Option<(*mut u8, usize)> {
        None
    }
}

pub struct KumoHeap {
    inner: UnsafeCell<MultiRegionHeap<TargetRegionSource>>,
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
                MultiRegionHeap::new(HEAP.0.get() as *mut u8, INITIAL_FLOOR, TargetRegionSource)
            }),
            initialized: AtomicBool::new(false),
            lock: sync::SpinLock::new(),
        }
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
}
