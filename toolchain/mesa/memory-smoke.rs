//j743
//! Real DMA allocation/CPU aliases under QEMU; GPU tables are covered by host tests.
#![no_std]
#![no_main]
extern crate alloc;
use alloc::vec;
use kumo_abi::{Handle, Rights, VmarFlags};
use kumo_gpu::{MAX_BUFFER_BYTES, PAGE_BYTES};

kumo_rt::entry!(main);
fn handle(raw: u64) -> Handle {
    assert!(raw != 0 && raw <= u32::MAX as u64);
    Handle(raw as u32)
}
fn map(vmo: Handle, bytes: u64) -> u64 {
    let (status, va) = kumo_rt::vmar_map_anywhere(
        vmo,
        bytes,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    );
    assert_eq!(status, 0);
    assert_ne!(va, 0);
    va
}
fn word(va: u64, page: usize, tail: bool) -> *mut u32 {
    (va + page as u64 * PAGE_BYTES + if tail { PAGE_BYTES - 4 } else { 0 }) as *mut u32
}
fn pattern(page: usize) -> u32 {
    0x7430_0000 ^ page as u32
}
fn say(text: &[u8]) {
    let _ = kumo_rt::debug_write(text.as_ptr(), text.len());
}

#[no_mangle]
extern "C" fn main() -> ! {
    say(b"G7o-b GPU memory begin\n");
    let mut refused = [0xfeed_0743];
    assert_eq!(
        kumo_rt::vmo_create_paged(2 * PAGE_BYTES, &mut refused),
        u64::MAX
    );
    assert_eq!(refused, [0xfeed_0743]);
    // Interleaved live guards force holes in the next allocation's physical page list.
    // — KESTREL
    let mut small = [(Handle(0), 0); 4];
    for slot in &mut small {
        let mut physical = [0];
        let h = handle(kumo_rt::vmo_create_paged(PAGE_BYTES, &mut physical));
        let va = map(h, PAGE_BYTES);
        unsafe { (va as *mut u32).write_volatile(0xfeed_0743) };
        assert_eq!(kumo_rt::vmar_unmap(va, PAGE_BYTES), 0);
        *slot = (h, physical[0]);
    }
    for n in [0, 2] {
        assert_eq!(kumo_rt::handle_close(small[n].0), 0);
    }
    let count = (MAX_BUFFER_BYTES / PAGE_BYTES) as usize;
    let mut pages = vec![0; count];
    let h = handle(kumo_rt::vmo_create_paged(MAX_BUFFER_BYTES, &mut pages));
    assert!(pages.windows(2).any(|p| p[1] != p[0] + PAGE_BYTES));
    assert!(pages.contains(&small[0].1) && pages.contains(&small[2].1));
    assert!(!pages.contains(&small[1].1) && !pages.contains(&small[3].1));
    assert!(pages.iter().all(|p| *p != 0 && p % PAGE_BYTES == 0));
    let mut sorted = pages.clone();
    sorted.sort_unstable();
    assert!(sorted.windows(2).all(|p| p[0] != p[1]));
    let va = map(h, MAX_BUFFER_BYTES);
    let duplicate = handle(kumo_rt::handle_duplicate(
        h,
        Rights::READ | Rights::WRITE | Rights::DUPLICATE | Rights::TRANSFER,
    ));
    let alias = map(duplicate, MAX_BUFFER_BYTES);
    for page in 0..count {
        for tail in [false, true] {
            assert_eq!(unsafe { word(va, page, tail).read_volatile() }, 0);
            unsafe { word(va, page, tail).write_volatile(pattern(page)) };
            assert_eq!(
                unsafe { word(alias, page, tail).read_volatile() },
                pattern(page)
            );
        }
    }
    assert_eq!(kumo_rt::vmar_unmap(va, MAX_BUFFER_BYTES), 0);
    assert_eq!(kumo_rt::handle_close(h), 0);
    for page in 0..count {
        assert_eq!(
            unsafe { word(alias, page, true).read_volatile() },
            pattern(page)
        );
    }
    assert_eq!(kumo_rt::vmar_unmap(alias, MAX_BUFFER_BYTES), 0);
    assert_eq!(kumo_rt::handle_close(duplicate), 0);
    let mut reused = vec![0; count];
    let h = handle(kumo_rt::vmo_create_paged(MAX_BUFFER_BYTES, &mut reused));
    assert_eq!(pages, reused);
    let va = map(h, MAX_BUFFER_BYTES);
    for page in 0..count {
        for tail in [false, true] {
            assert_eq!(unsafe { word(va, page, tail).read_volatile() }, 0);
        }
    }
    assert_eq!(kumo_rt::vmar_unmap(va, MAX_BUFFER_BYTES), 0);
    assert_eq!(kumo_rt::handle_close(h), 0);
    for n in [1, 3] {
        let va = map(small[n].0, PAGE_BYTES);
        assert_eq!(unsafe { (va as *const u32).read_volatile() }, 0xfeed_0743);
        assert_eq!(kumo_rt::vmar_unmap(va, PAGE_BYTES), 0);
        assert_eq!(kumo_rt::handle_close(small[n].0), 0);
    }
    say(b"G7o-b GPU memory green: 64 MiB, fragmented pages, shared NC aliases, retained guards, reclamation and zeroed reuse\n");
    kumo_rt::process_exit(0)
}
