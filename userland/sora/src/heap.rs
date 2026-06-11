use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

const HEAP_SIZE: usize = 64 * 1024;

#[repr(align(16))]
struct HeapMem([u8; HEAP_SIZE]);

static HEAP: HeapMem = HeapMem([0; HEAP_SIZE]);
static POS: AtomicUsize = AtomicUsize::new(0);

pub struct BumpAlloc;

unsafe impl GlobalAlloc for BumpAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pos = POS.load(Ordering::Relaxed);
        let aligned = (pos + layout.align() - 1) & !(layout.align() - 1);
        let end = aligned + layout.size();
        if end > HEAP_SIZE {
            return core::ptr::null_mut();
        }
        POS.store(end, Ordering::Relaxed);
        unsafe { HEAP.0.as_ptr().add(aligned) as *mut u8 }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}
