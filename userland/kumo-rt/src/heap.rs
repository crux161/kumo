use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};
use linked_list_allocator::Heap as LinkedListHeap;

const HEAP_SIZE: usize = 64 * 1024;

// The backing store must be interior-mutable: the allocator hands out `*mut u8`
// pointers into it and writes through them. A plain `static HEAP: [u8; N]` is an
// *immutable* static — casting `&HEAP` to `*mut` and writing is UB, and the
// optimizer is entitled to assume the bytes never change from their zero
// initializer (so e.g. a freshly built `String`'s data reads back empty).
// `UnsafeCell` tells the compiler the contents are mutable.
#[repr(align(16))]
struct HeapMem(UnsafeCell<[u8; HEAP_SIZE]>);

// SAFETY: this EL0 process is single-threaded; the `AtomicUsize` cursor is the only
// path that derives pointers into the cell and it serializes every allocation.
unsafe impl Sync for HeapMem {}

static HEAP: HeapMem = HeapMem(UnsafeCell::new([0; HEAP_SIZE]));

pub struct KumoHeap {
    inner: UnsafeCell<LinkedListHeap>,
    initialized: AtomicBool,
}

// SAFETY: EL0 single-threaded
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
                (*self.inner.get()).init(HEAP.0.get() as *mut u8, HEAP_SIZE);
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
