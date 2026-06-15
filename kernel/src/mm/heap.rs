//! Stage-A kernel heap: a bump `#[global_allocator]` over a fixed in-image region.
//!
//! This is the simplest *correct* allocator — it hands out aligned slices and never
//! reclaims (`dealloc` is a no-op). It exists so the kernel can use `alloc` (`Vec`,
//! `Box`, `String`) while the object/task/IPC machinery is built; a reclaiming
//! allocator (free-list/slab over the frame allocator) replaces it before that
//! machinery actually needs to free. The global instance is gated to the
//! freestanding kernel (`target_os = "none"`) so it never overrides the host
//! allocator used by unit tests or the stage-a-smoke tool.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Size of the Stage-A bump heap (lives in the kernel image's BSS — NOBITS, so it costs
/// memory but not image bytes). Sized for the whole Stage-A POST: every kernel-side IPC
/// message (`KernelMessage::from_borrowed` → `Vec`), the console-route klogs once Sora is
/// serving, and the P10 process-model demos all allocate here and **never free** (`dealloc`
/// is a no-op). 1 MiB was exhausted partway through the userspace serve checks once the
/// serve/console-route paths actually round-tripped; the headroom here carries the boot to
/// the shell. The real fix remains the reclaiming allocator described above.
pub const HEAP_SIZE: usize = 8 * 1024 * 1024;

pub struct Bump {
    next: AtomicUsize,
}

impl Bump {
    pub const fn new() -> Self {
        Self {
            next: AtomicUsize::new(0),
        }
    }

    /// Reserve `size` bytes aligned to `align` within `capacity`, returning the byte
    /// offset of the allocation. Pure bump arithmetic — testable without a real heap.
    pub fn reserve(&self, size: usize, align: usize, capacity: usize) -> Option<usize> {
        loop {
            let cur = self.next.load(Ordering::Relaxed);
            let aligned = align_up(cur, align)?;
            let end = aligned.checked_add(size)?;
            if end > capacity {
                return None;
            }
            if self
                .next
                .compare_exchange(cur, end, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(aligned);
            }
        }
    }

    pub fn used(&self) -> usize {
        self.next.load(Ordering::Relaxed)
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align == 0 || (align & (align - 1)) != 0 {
        return None; // alignment must be a non-zero power of two
    }
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}

#[cfg(target_os = "none")]
mod global {
    use super::{Bump, HEAP_SIZE};
    use core::alloc::{GlobalAlloc, Layout};
    use core::cell::UnsafeCell;

    #[repr(align(16))]
    struct Heap(UnsafeCell<[u8; HEAP_SIZE]>);

    // The kernel is single-core at Stage-A; access is serialized by the bump cursor.
    unsafe impl Sync for Heap {}

    static HEAP: Heap = Heap(UnsafeCell::new([0u8; HEAP_SIZE]));

    #[global_allocator]
    static ALLOC: Bump = Bump::new();

    /// Bytes handed out so far by the global bump allocator (diagnostic only).
    pub fn used() -> usize {
        ALLOC.used()
    }

    unsafe impl GlobalAlloc for Bump {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            match self.reserve(layout.size(), layout.align(), HEAP_SIZE) {
                Some(offset) => unsafe { (HEAP.0.get() as *mut u8).add(offset) },
                None => core::ptr::null_mut(),
            }
        }

        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
            // Bump allocator: reclamation arrives with the real heap.
        }
    }
}

/// Bytes handed out by the global bump allocator so far (diagnostic). 0 on host.
#[cfg(target_os = "none")]
pub fn used() -> usize {
    global::used()
}
#[cfg(not(target_os = "none"))]
pub fn used() -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserves_aligned_and_respects_capacity() {
        let bump = Bump::new();
        assert_eq!(bump.reserve(10, 8, 64), Some(0));
        // next alloc rounds up to the next multiple of 8.
        assert_eq!(bump.reserve(4, 8, 64), Some(16));
        assert_eq!(bump.used(), 20);
        // does not fit.
        assert_eq!(bump.reserve(64, 8, 64), None);
    }

    #[test]
    fn rejects_bad_alignment() {
        let bump = Bump::new();
        assert_eq!(bump.reserve(1, 3, 64), None);
        assert_eq!(bump.reserve(1, 0, 64), None);
    }
}
