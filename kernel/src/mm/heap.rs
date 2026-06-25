//! Kernel heap: a first-fit free-list (`linked_list_allocator`) over a fixed 8 MiB
//! in-image region (`BACKING`).
//!
//! This *does* reclaim — `dealloc` returns memory to the free list and coalesces
//! adjacent holes — so the kernel can use `alloc` (`Vec`, `Box`, `String`) freely.
//! Two caveats remain, and they matter for any future heavy-alloc kernel path
//! (e.g. a write-back FS cache): the region is **fixed-size** (no growth yet — it
//! cannot back-fill from the frame allocator) and first-fit is **fragmentation-prone**
//! under sustained mixed-size churn. The planned upgrade is a growable slab/free-list
//! over the frame allocator; until then, keep kernel-heap residents bounded. The
//! global instance is gated to the freestanding kernel (`target_os = "none"`) so it
//! never overrides the host allocator used by unit tests or the stage-a-smoke tool.
//
// — CORVUS 2026-06-24: corrected — this was documented as a non-reclaiming *bump*
// allocator, but the code is `linked_list_allocator` with a real `deallocate`.

use core::sync::atomic::{AtomicBool, Ordering};

pub const HEAP_SIZE: usize = 8 * 1024 * 1024;

#[cfg(target_os = "none")]
mod global {
    use super::HEAP_SIZE;
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, Ordering};
    use linked_list_allocator::Heap as LinkedListHeap;

    #[repr(align(16))]
    struct Backing(UnsafeCell<[u8; HEAP_SIZE]>);
    unsafe impl Sync for Backing {}
    static BACKING: Backing = Backing(UnsafeCell::new([0u8; HEAP_SIZE]));

    pub struct KumoHeap {
        inner: UnsafeCell<LinkedListHeap>,
        initialized: AtomicBool,
    }
    unsafe impl Sync for KumoHeap {}

    impl KumoHeap {
        pub const fn empty() -> Self {
            Self {
                inner: UnsafeCell::new(LinkedListHeap::empty()),
                initialized: AtomicBool::new(false),
            }
        }

        fn ensure_init(&self) {
            if !self.initialized.swap(true, Ordering::SeqCst) {
                unsafe {
                    (*self.inner.get()).init(BACKING.0.get() as *mut u8, HEAP_SIZE);
                }
            }
        }
    }

    unsafe impl GlobalAlloc for KumoHeap {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            self.ensure_init();
            unsafe { (*self.inner.get()).allocate_first_fit(layout) }
                .ok()
                .map_or(core::ptr::null_mut(), |allocation| allocation.as_ptr())
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe {
                (*self.inner.get()).deallocate(core::ptr::NonNull::new_unchecked(ptr), layout);
            }
        }
    }

    #[global_allocator]
    static ALLOC: KumoHeap = KumoHeap::empty();

    pub fn used() -> usize {
        if ALLOC.initialized.load(Ordering::Relaxed) {
            unsafe { (*ALLOC.inner.get()).used() }
        } else {
            0
        }
    }
}

#[cfg(target_os = "none")]
pub fn used() -> usize {
    global::used()
}
#[cfg(not(target_os = "none"))]
pub fn used() -> usize {
    0
}
