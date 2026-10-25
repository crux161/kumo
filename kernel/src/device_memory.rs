//j616

//! Private RAM backing for offline device tables (DESIGN/022 D1d-b2a).
//! A pool reserves its frames against VMO aliases for its lifetime and recycles them only
//! through table leases. Frames never return to the global bump allocator. No device root
//! is published; cross-server pin/quarantine lifetime remains a later gate. — KESTREL

use crate::{
    mm::{Vmo, VmoBacking, PAGE_SIZE},
    object::ObjectError,
};
use alloc::{rc::Rc, vec, vec::Vec};
use core::cell::RefCell;
use kumo_abi::{BootInfo, Range};
use kumo_hal::active::DeviceTableMemory;

/// Allocation must exclude every supplied span before zeroing, and return a fresh mapped
/// RAM page. Word access is infallible only for pages owned by the calling lease. — KESTREL
pub(crate) trait PageMemory {
    fn allocate(&mut self, excluded: &[Range]) -> Option<u64>;
    fn read(&self, page: u64, index: usize) -> u32;
    fn write(&mut self, page: u64, index: usize, word: u32);
}

#[derive(Clone, Debug, Default)]
pub(crate) struct KernelPages {
    pub boot: Option<BootInfo>,
}

impl PageMemory for KernelPages {
    fn allocate(&mut self, excluded: &[Range]) -> Option<u64> {
        #[cfg(target_os = "none")]
        {
            let boot = self.boot?;
            let saved = kumo_hal::active::read_user_aspace_root();
            let kernel = if crate::user_thread::is_started() {
                crate::user_thread::kernel_ttbr0()
            } else {
                0
            };
            // Before the first user runtime, the current tree is already the kernel's
            // identity tree. Later allocations temporarily leave the child's tree. — KESTREL
            if kernel != 0 {
                unsafe { kumo_hal::active::set_user_aspace_root(kernel) };
            }
            let frame = unsafe { crate::mm::alloc_zeroed_frame_excluding(&boot, excluded) };
            if kernel != 0 {
                unsafe { kumo_hal::active::set_user_aspace_root(saved) };
            }
            frame
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = excluded;
            None
        }
    }
    fn read(&self, page: u64, index: usize) -> u32 {
        assert!(index < 1024);
        #[cfg(target_os = "none")]
        {
            unsafe {
                core::ptr::read_volatile(
                    crate::syscall::phys_ptr_mut(page).cast::<u32>().add(index),
                )
            }
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = page;
            panic!("host cannot dereference physical RAM")
        }
    }
    fn write(&mut self, page: u64, index: usize, word: u32) {
        assert!(index < 1024);
        #[cfg(target_os = "none")]
        {
            unsafe {
                core::ptr::write_volatile(
                    crate::syscall::phys_ptr_mut(page).cast::<u32>().add(index),
                    word,
                )
            };
        }
        #[cfg(not(target_os = "none"))]
        {
            let _ = (page, word);
            panic!("host cannot dereference physical RAM")
        }
    }
}

#[derive(Debug)]
struct Slot {
    phys: u64,
    leased: bool,
}
#[derive(Debug)]
struct PoolState<A> {
    memory: A,
    frames: Vec<Slot>,
    excluded: Vec<Range>,
}

#[derive(Debug)]
pub(crate) struct TablePool<A>(Rc<RefCell<PoolState<A>>>);
impl<A> Clone for TablePool<A> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<A: Default> Default for TablePool<A> {
    fn default() -> Self {
        Self::new(A::default())
    }
}

fn overlap(a: Range, b: Range) -> bool {
    a.start
        .checked_add(a.len)
        .zip(b.start.checked_add(b.len))
        .is_none_or(|(ae, be)| a.start < be && b.start < ae)
}

impl<A> TablePool<A> {
    pub fn new(memory: A) -> Self {
        Self(Rc::new(RefCell::new(PoolState {
            memory,
            frames: Vec::new(),
            // The initial device walker is below 4 GiB. Exclude high RAM before zeroing,
            // rather than consuming it and then discovering the format cannot use it. — KESTREL
            excluded: vec![Range::new(1 << 32, u64::MAX - (1 << 32))],
        })))
    }

    pub fn lease(&self) -> TableLease<A> {
        TableLease {
            pool: self.clone(),
            owned: Vec::new(),
        }
    }

    /// Reserve metadata before a handle is created. The pool reservation protects
    /// free pages too: recycling a table must not inherit a newly minted CPU alias. — KESTREL
    pub fn prepare_vmo(&self, vmo: Vmo) -> Result<Option<Range>, ObjectError> {
        let base = match vmo.backing() {
            VmoBacking::PhysicalRam { phys_base }
            | VmoBacking::Mmio { phys_base }
            | VmoBacking::Dma { phys_base } => phys_base,
            VmoBacking::Anonymous => return Ok(None),
        };
        let end = base
            .checked_add(vmo.len())
            .ok_or(ObjectError::AccessDenied)?;
        // Initial device/uncached CPU mappings still cover whole 2 MiB blocks. Include
        // that actual reach, even for a small VMO beside a table, until that mapper is
        // replaced. PhysicalRam is always page-granular Normal-WB. — KESTREL
        let span = if matches!(
            vmo.backing(),
            VmoBacking::Mmio { .. } | VmoBacking::Dma { .. }
        ) {
            const MASK: u64 = (1 << 21) - 1;
            let start = base & !MASK;
            let end = end.checked_add(MASK).ok_or(ObjectError::AccessDenied)? & !MASK;
            Range::new(start, end - start)
        } else {
            Range::new(base, vmo.len())
        };
        let mut pool = self.0.borrow_mut();
        if pool
            .frames
            .iter()
            .any(|frame| overlap(span, Range::new(frame.phys, PAGE_SIZE)))
        {
            return Err(ObjectError::AccessDenied);
        }
        pool.excluded
            .try_reserve(1)
            .map_err(|_| ObjectError::TableFull)?;
        Ok(Some(span))
    }

    pub fn commit_vmo(&self, span: Option<Range>) {
        if let Some(span) = span {
            let mut pool = self.0.borrow_mut();
            // root_vmo_create keeps prepare/insert/commit serialized, without a table
            // allocation or kernel re-entry between them. Fail closed on misuse. — KESTREL
            assert!(!pool
                .frames
                .iter()
                .any(|frame| overlap(span, Range::new(frame.phys, PAGE_SIZE))));
            pool.excluded.push(span);
        }
    }

    /// DMA VMOs can later get coarse uncached CPU mappings. Their allocator must skip
    /// private-table blocks before zeroing, so ordinary DMA allocation stays usable.
    /// These conservative padding gaps remain with the boot bump allocator. — KESTREL
    pub fn dma_exclusions(&self) -> Vec<Range> {
        let pool = self.0.borrow();
        let mut blocks = Vec::new();
        for frame in &pool.frames {
            let block = Range::new(frame.phys & !((1 << 21) - 1), 1 << 21);
            if !blocks.contains(&block) {
                blocks.push(block);
            }
        }
        blocks
    }

    #[cfg(target_os = "none")]
    pub fn frames(&self) -> Vec<u64> {
        self.0
            .borrow()
            .frames
            .iter()
            .map(|frame| frame.phys)
            .collect()
    }
    #[cfg(target_os = "none")]
    pub fn all_free(&self) -> bool {
        self.0.borrow().frames.iter().all(|frame| !frame.leased)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_hal::active::{
        DeviceTableAccess as Access, DeviceTableError as Error, OfflineDeviceTable as Table,
    };
    use std::{
        boxed::Box,
        collections::{BTreeMap, VecDeque},
    };

    #[derive(Default, Debug)]
    struct Model {
        candidates: VecDeque<u64>,
        bytes: BTreeMap<u64, Box<[u32; 1024]>>,
        zeroed: Vec<u64>,
    }
    #[derive(Debug, Clone)]
    struct Memory(Rc<RefCell<Model>>);
    impl Memory {
        fn new(candidates: impl IntoIterator<Item = u64>) -> Self {
            let mut model = Model::default();
            for p in candidates {
                model.candidates.push_back(p);
                model.bytes.insert(p, Box::new([0x11223344; 1024]));
            }
            Self(Rc::new(RefCell::new(model)))
        }
    }
    impl PageMemory for Memory {
        fn allocate(&mut self, excluded: &[Range]) -> Option<u64> {
            let mut m = self.0.borrow_mut();
            while let Some(p) = m.candidates.pop_front() {
                if excluded
                    .iter()
                    .any(|&r| overlap(r, Range::new(p, PAGE_SIZE)))
                {
                    continue;
                }
                m.bytes.get_mut(&p).unwrap().fill(0);
                m.zeroed.push(p);
                return Some(p);
            }
            None
        }
        fn read(&self, p: u64, i: usize) -> u32 {
            self.0.borrow().bytes[&p][i]
        }
        fn write(&mut self, p: u64, i: usize, w: u32) {
            self.0.borrow_mut().bytes.get_mut(&p).unwrap()[i] = w;
        }
    }

    #[test]
    fn issued_physical_mmio_and_dma_aliases_are_skipped_before_zeroing() {
        let memory = Memory::new([
            0x1000,
            0x200000,
            0x201000,
            0x202000,
            0x400000,
            0x401000,
            1 << 32,
            0x4000,
        ]);
        let pool = TablePool::new(memory.clone());
        for vmo in [
            Vmo::from_physical_range(0x1000, PAGE_SIZE).unwrap(),
            Vmo::from_mmio_range(0x201000, PAGE_SIZE).unwrap(),
            Vmo::from_dma_range(0x401000, PAGE_SIZE).unwrap(),
        ] {
            pool.commit_vmo(pool.prepare_vmo(vmo).unwrap());
        }
        assert_eq!(pool.prepare_vmo(Vmo::new(PAGE_SIZE).unwrap()), Ok(None));
        let mut lease = pool.lease();
        assert_eq!(lease.allocate(), Some(0x4000));
        assert_eq!(memory.0.borrow().zeroed, [0x4000]);
        for p in [
            0x1000,
            0x200000,
            0x201000,
            0x202000,
            0x400000,
            0x401000,
            1 << 32,
        ] {
            assert!(memory.0.borrow().bytes[&p].iter().all(|&w| w == 0x11223344));
        }
        lease.release(0x4000);
    }

    #[test]
    fn active_and_recyclable_tables_reject_every_physical_vmo_alias() {
        let pool = TablePool::new(Memory::new([0x2000]));
        let mut lease = pool.lease();
        assert_eq!(lease.allocate(), Some(0x2000));
        for freed in [false, true] {
            if freed {
                lease.release(0x2000);
            }
            for constructor in [
                Vmo::from_physical_range,
                Vmo::from_mmio_range,
                Vmo::from_dma_range,
            ] {
                for (base, len) in [(0x2000, PAGE_SIZE), (0x1000, 0x2000), (0x2000, 1)] {
                    assert_eq!(
                        pool.prepare_vmo(constructor(base, len).unwrap()),
                        Err(ObjectError::AccessDenied)
                    );
                }
                let coarse = matches!(
                    constructor(0x1000, PAGE_SIZE).unwrap().backing(),
                    VmoBacking::Mmio { .. } | VmoBacking::Dma { .. }
                );
                for base in [0x1000, 0x3000] {
                    assert_eq!(
                        pool.prepare_vmo(constructor(base, PAGE_SIZE).unwrap())
                            .is_ok(),
                        !coarse
                    );
                }
                assert!(pool
                    .prepare_vmo(constructor(0x200000, PAGE_SIZE).unwrap())
                    .is_ok());
            }
        }
    }

    #[test]
    fn dma_allocator_skips_the_actual_coarse_mapping_reach() {
        let memory = Memory::new([0x2000, 0x3000, 0x200000]);
        let pool = TablePool::new(memory.clone());
        let mut lease = pool.lease();
        let private = lease.allocate().unwrap();
        lease.write(private, 0, 0xa5a5a5a5);
        let excluded = pool.dma_exclusions();
        assert_eq!(excluded, [Range::new(0, 1 << 21)]);
        let mut dma_allocator = memory.clone();
        assert_eq!(dma_allocator.allocate(&excluded), Some(0x200000));
        assert_eq!(lease.read(private, 0), 0xa5a5a5a5);
        assert!(memory.0.borrow().bytes[&0x3000]
            .iter()
            .all(|&w| w == 0x11223344));
        lease.release(private);
        assert_eq!(pool.dma_exclusions(), excluded);
    }

    #[test]
    fn real_walker_uses_scrubs_and_reuses_a_bounded_private_pool() {
        let memory = Memory::new([0x1000, 0x2000, 0x3000]);
        let pool = TablePool::new(memory.clone());
        for _ in 0..3 {
            let mut table = Table::new(pool.lease()).unwrap();
            table
                .map_read_only(0x3ff000, &[0x10000000, 0x20000000])
                .unwrap();
            assert_eq!(table.walk(0x400123, Access::Read), Ok(Some(0x20000123)));
            assert_eq!(
                table.walk(0x400123, Access::Write),
                Err(Error::PermissionDenied)
            );
            table.unmap(0x3ff000, 2 * PAGE_SIZE).unwrap();
            drop(table);
            assert!(pool.0.borrow().frames.iter().all(|f| !f.leased));
            assert!(memory
                .0
                .borrow()
                .bytes
                .values()
                .all(|page| page.iter().all(|&w| w == 0)));
        }
        assert_eq!(memory.0.borrow().zeroed, [0x1000, 0x2000, 0x3000]);
        assert_eq!(pool.0.borrow().frames.len(), 3);
    }

    #[test]
    fn allocation_failure_recycles_only_the_rolled_back_prefix() {
        let pool = TablePool::new(Memory::new([0x1000, 0x2000]));
        let mut table = Table::new(pool.lease()).unwrap();
        assert_eq!(
            table.map_read_only(0x3ff000, &[0x10000000, 0x20000000]),
            Err(Error::NoMemory)
        );
        assert_eq!(table.walk(0x3ff000, Access::Read), Ok(None));
        assert_eq!(
            pool.0.borrow().frames.iter().filter(|f| f.leased).count(),
            1
        );
        table.map_read_only(0x1000, &[0x10000000]).unwrap();
        assert_eq!(table.walk(0x1000, Access::Read), Ok(Some(0x10000000)));
        drop(table);
        assert!(pool.0.borrow().frames.iter().all(|f| !f.leased));
    }

    #[test]
    fn unexpected_provider_drop_keeps_unreleased_pages_reserved() {
        let pool = TablePool::new(Memory::new([0x1000, 0x2000]));
        let mut first = pool.lease();
        assert_eq!(first.allocate(), Some(0x1000));
        drop(first);
        let mut second = pool.lease();
        assert_eq!(second.allocate(), Some(0x2000));
        assert_eq!(
            pool.prepare_vmo(Vmo::from_mmio_range(0x1000, PAGE_SIZE).unwrap()),
            Err(ObjectError::AccessDenied)
        );
        second.release(0x2000);
        assert!(pool.0.borrow().frames[0].leased);
        assert!(!pool.0.borrow().frames[1].leased);
    }

    #[test]
    #[should_panic(expected = "foreign table read")]
    fn one_lease_cannot_read_another_owners_page() {
        let pool = TablePool::new(Memory::new([0x1000]));
        let mut first = pool.lease();
        let p = first.allocate().unwrap();
        let second = pool.lease();
        second.read(p, 0);
    }

    #[test]
    #[should_panic(expected = "foreign/double table release")]
    fn one_lease_cannot_release_another_owners_page() {
        let pool = TablePool::new(Memory::new([0x1000]));
        let mut first = pool.lease();
        let p = first.allocate().unwrap();
        let mut second = pool.lease();
        second.release(p);
    }

    #[test]
    fn invalid_spans_and_uncommitted_admission_cannot_exclude_or_alias_pages() {
        let pool = TablePool::new(Memory::new([0x1000]));
        assert_eq!(
            pool.prepare_vmo(Vmo::from_mmio_range(u64::MAX - 4095, 0x2000).unwrap()),
            Err(ObjectError::AccessDenied)
        );
        let _uncommitted = pool
            .prepare_vmo(Vmo::from_physical_range(0x1000, PAGE_SIZE).unwrap())
            .unwrap();
        let mut lease = pool.lease();
        assert_eq!(lease.allocate(), Some(0x1000));
        lease.release(0x1000);
    }
}

impl TablePool<KernelPages> {
    pub fn set_boot_info(&self, boot: BootInfo) {
        self.0.borrow_mut().memory.boot = Some(boot);
    }
}

/// Each walker receives a distinct lease. Unreleased pages remain leased if the provider is
/// dropped unexpectedly; they cannot be recycled by guessing their physical address.
/// This fail-closed RAM rule does not replace live quiescence/invalidation. — KESTREL
pub(crate) struct TableLease<A> {
    pool: TablePool<A>,
    owned: Vec<u64>,
}

impl<A: PageMemory> DeviceTableMemory for TableLease<A> {
    fn allocate(&mut self) -> Option<u64> {
        self.owned.try_reserve(1).ok()?;
        let mut pool = self.pool.0.borrow_mut();
        if let Some(slot) = pool.frames.iter_mut().find(|frame| !frame.leased) {
            slot.leased = true;
            self.owned.push(slot.phys);
            return Some(slot.phys);
        }
        pool.frames.try_reserve(1).ok()?;
        let PoolState {
            memory,
            excluded,
            frames,
        } = &mut *pool;
        let phys = memory.allocate(excluded)?;
        // A faulty allocator cannot make an existing table or an issued alias writable.
        // Its consumed candidate stays with the global bump allocator on rejection. — KESTREL
        if phys % PAGE_SIZE != 0
            || phys >= 1 << 32
            || excluded
                .iter()
                .any(|&range| overlap(range, Range::new(phys, PAGE_SIZE)))
            || frames.iter().any(|frame| frame.phys == phys)
        {
            return None;
        }
        frames.push(Slot { phys, leased: true });
        self.owned.push(phys);
        Some(phys)
    }
    fn read(&self, page: u64, index: usize) -> u32 {
        assert!(self.owned.contains(&page), "foreign table read");
        assert!(index < 1024);
        self.pool.0.borrow().memory.read(page, index)
    }
    fn write(&mut self, page: u64, index: usize, word: u32) {
        assert!(self.owned.contains(&page), "foreign table write");
        assert!(index < 1024);
        self.pool.0.borrow_mut().memory.write(page, index, word);
    }
    fn release(&mut self, page: u64) {
        let i = self
            .owned
            .iter()
            .position(|&owned| owned == page)
            .expect("foreign/double table release");
        let mut pool = self.pool.0.borrow_mut();
        // Scrub before making the page reusable; no allocation or fallible step follows.
        // The offline owner already removed the directory reference. — KESTREL
        for index in 0..1024 {
            pool.memory.write(page, index, 0);
        }
        let slot = pool
            .frames
            .iter_mut()
            .find(|frame| frame.phys == page)
            .expect("owned table frame");
        assert!(slot.leased);
        slot.leased = false;
        self.owned.swap_remove(i);
    }
}
