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

// SAFETY: on freestanding targets the EL0 process is single-threaded;
// on the host the allocator spinlock (below) serializes every access.
unsafe impl Sync for HeapMem {}

static HEAP: HeapMem = HeapMem(UnsafeCell::new([0; HEAP_SIZE]));

// On the host `cargo test` runs multiple test binaries in one process; the
// linked_list_allocator is not thread-safe so concurrent alloc/dealloc can
// observe torn freelist state (the "Freed node aliases existing hole" panic
// recorded in J221, J223, J224). A simple spinlock serializes access without
// pulling in std. On the freestanding target the EL0 process is
// single-threaded and the lock compiles to nothing.
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

pub struct KumoHeap {
    inner: UnsafeCell<LinkedListHeap>,
    initialized: AtomicBool,
    lock: sync::SpinLock,
}

unsafe impl Sync for KumoHeap {}

impl KumoHeap {
    pub const fn empty() -> Self {
        Self {
            inner: UnsafeCell::new(LinkedListHeap::empty()),
            initialized: AtomicBool::new(false),
            lock: sync::SpinLock::new(),
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
        self.lock.lock();
        self.ensure_init();
        let result = unsafe { (*self.inner.get()).allocate_first_fit(layout) }
            .ok()
            .map_or(core::ptr::null_mut(), |allocation| allocation.as_ptr());
        self.lock.unlock();
        result
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.lock.lock();
        unsafe {
            (*self.inner.get()).deallocate(core::ptr::NonNull::new_unchecked(ptr), layout);
        }
        self.lock.unlock();
    }
}
