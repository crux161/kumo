//j615
//j628

//! DESIGN/022 D1d-b1: Rockchip-v2 descriptors and an owning read-only page table.
//! 1024 directory words × 1024 leaf words × 4 KiB cover a 32-bit IOVA. The descriptor
//! codec represents 40-bit physical addresses; this first walker admits only below 4 GiB.
//!
//! Facts: upstream Linux rockchip-iommu.c (J613 snapshot), with Rockchip permission fix
//! e3bc123a2260145e34b57454da3db0edd117eb8e. READ is bit 1, WRITE bit 2 in both versions;
//! the old kernel-next oracle reverses them. No GPL implementation code is imported.
//!
//! No MMIO, live context or ABI admission exists here. Publication freezes the walker
//! and retains every table page before cleaning/exposing its root. Only a matching
//! two-bank reset witness can restore the offline destructor. — KESTREL

extern crate alloc;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;

pub const PAGE_SIZE: u64 = 4096;
pub const ENTRIES: usize = 1024;
const IOVA_END: u64 = 1 << 32;
const PHYS_END: u64 = 1 << 40;
const VALID: u32 = 1;
const READ: u32 = 1 << 1;
const WRITE: u32 = 1 << 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRange,
    UnsupportedAddress,
    InvalidDescriptor,
    CorruptTable,
    TableAlias,
    AlreadyMapped,
    NotMapped,
    PermissionDenied,
    NoMemory,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Leaf {
    pub phys: u64,
    pub access: Access,
}

fn address_word(phys: u64) -> Result<u32, Error> {
    if phys >= PHYS_END {
        return Err(Error::UnsupportedAddress);
    }
    if phys % PAGE_SIZE != 0 {
        return Err(Error::InvalidRange);
    }
    // Physical 35:32 occupy 11:8, and 39:36 occupy 7:4; never truncate the high byte.
    // These are address bits, not v1 cache attributes. — KESTREL
    Ok((phys as u32 & 0xffff_f000)
        | (((phys >> 32) as u32 & 0xf) << 8)
        | (((phys >> 36) as u32 & 0xf) << 4))
}

fn word_address(word: u32) -> u64 {
    u64::from(word & 0xffff_f000)
        | (u64::from((word >> 8) & 0xf) << 32)
        | (u64::from((word >> 4) & 0xf) << 36)
}

pub fn directory_word(phys: u64) -> Result<u32, Error> {
    Ok(address_word(phys)? | VALID)
}

pub fn decode_directory(word: u32) -> Result<Option<u64>, Error> {
    if word == 0 {
        return Ok(None);
    }
    if word & VALID == 0 || word & 0xe != 0 {
        return Err(Error::InvalidDescriptor);
    }
    Ok(Some(word_address(word)))
}

pub fn leaf_word(phys: u64, access: Access) -> Result<u32, Error> {
    let flags = match access {
        Access::Read => READ,
        Access::Write => WRITE,
        Access::ReadWrite => READ | WRITE,
    };
    Ok(address_word(phys)? | VALID | flags)
}

pub fn decode_leaf(word: u32) -> Result<Option<Leaf>, Error> {
    if word == 0 {
        return Ok(None);
    }
    if word & VALID == 0 || word & 8 != 0 {
        return Err(Error::InvalidDescriptor);
    }
    let access = match word & (READ | WRITE) {
        READ => Access::Read,
        WRITE => Access::Write,
        bits if bits == READ | WRITE => Access::ReadWrite,
        _ => return Err(Error::InvalidDescriptor),
    };
    Ok(Some(Leaf {
        phys: word_address(word),
        access,
    }))
}

fn low_page(phys: u64) -> Result<(), Error> {
    address_word(phys)?;
    if phys >= IOVA_END {
        return Err(Error::UnsupportedAddress);
    }
    Ok(())
}

fn range(iova: u64, len: u64) -> Result<(), Error> {
    if len == 0
        || iova % PAGE_SIZE != 0
        || len % PAGE_SIZE != 0
        || iova.checked_add(len).is_none_or(|end| end > IOVA_END)
    {
        return Err(Error::InvalidRange);
    }
    Ok(())
}

fn indices(iova: u64) -> (usize, usize) {
    ((iova >> 22) as usize, ((iova >> 12) & 1023) as usize)
}

/// Private RAM pages, disjoint from all data backing and every other allocation. `allocate`
/// hands out one fresh 4 KiB page. Reads/writes of its 1024 words and release are infallible;
/// allocation failure is reported before returning a page. The walker zeroes new pages.
/// Implementations must not hand these pages to VMOs. Runtime pinning/admission is a later
/// subgate; this interface neither proves VMO authority nor attaches a device. — KESTREL
pub trait TableMemory {
    fn allocate(&mut self) -> Option<u64>;
    fn read(&self, page: u64, index: usize) -> u32;
    fn write(&mut self, page: u64, index: usize, word: u32);
    fn release(&mut self, page: u64);
}

#[derive(Clone, Copy, Debug)]
struct OwnedLeaf {
    index: usize,
    phys: u64,
}

/// Owns an unpublished directory and its private leaf pages. No root getter or activation
/// path: dropping this value is safe precisely because no device can reach it. — KESTREL
pub struct OfflineTable<M: TableMemory> {
    memory: M,
    root: u64,
    leaves: Vec<OwnedLeaf>,
}

/// A single-use checked reset result. Only the hardware mechanism can construct
/// this evidence; it names the owned directory observed before both banks reset.
/// It is deliberately neither Copy nor Clone. — KESTREL
#[derive(Debug)]
pub struct DetachedRoot {
    root: u64,
}
impl DetachedRoot {
    pub(crate) fn after_reset(root: u64) -> Self {
        Self { root }
    }
}

/// Own the directory before exposing it. Dropping an uncertain publication retains
/// every table page and its allocator; only a matching checked reset reclaims it.
/// VMO pins belong to the kernel's outer owner and must follow the same rule. — KESTREL
pub struct PublishedTable<M: TableMemory> {
    table: ManuallyDrop<OfflineTable<M>>,
}
impl<M: TableMemory> PublishedTable<M> {
    pub fn root(&self) -> u64 {
        self.table.root
    }

    pub fn reclaim(self, proof: DetachedRoot) -> Result<OfflineTable<M>, Self> {
        if self.root() != proof.root {
            return Err(self);
        }
        Ok(ManuallyDrop::into_inner(self.table))
    }
}

impl<M: TableMemory> OfflineTable<M> {
    /// Freeze mutation and transfer ownership before cache cleaning/root exposure.
    /// The cleaner receives only privately owned pages, never a descriptor address.
    /// A failed/panicking cleaner cannot invoke the offline destructor. — KESTREL
    pub fn publish(self, mut clean: impl FnMut(u64)) -> PublishedTable<M> {
        let owned = PublishedTable {
            table: ManuallyDrop::new(self),
        };
        for leaf in &owned.table.leaves {
            clean(leaf.phys);
        }
        clean(owned.table.root);
        owned
    }

    pub fn new(mut memory: M) -> Result<Self, Error> {
        let root = memory.allocate().ok_or(Error::NoMemory)?;
        if let Err(error) = low_page(root) {
            memory.release(root);
            return Err(error);
        }
        for i in 0..ENTRIES {
            memory.write(root, i, 0);
        }
        Ok(Self {
            memory,
            root,
            leaves: Vec::new(),
        })
    }

    fn owns(&self, phys: u64) -> bool {
        phys == self.root || self.leaves.iter().any(|leaf| leaf.phys == phys)
    }

    fn leaf(&self, index: usize) -> Result<Option<u64>, Error> {
        let word = self.memory.read(self.root, index);
        let recorded = self
            .leaves
            .iter()
            .find(|leaf| leaf.index == index)
            .map(|leaf| leaf.phys);
        let decoded = decode_directory(word)?;
        if decoded != recorded {
            return Err(Error::CorruptTable);
        }
        // Never dereference an address from a DTE until private ownership agrees. — KESTREL
        Ok(recorded)
    }

    pub fn walk(&self, iova: u64, access: Access) -> Result<Option<u64>, Error> {
        if iova >= IOVA_END {
            return Err(Error::InvalidRange);
        }
        let (directory, index) = indices(iova);
        let Some(table) = self.leaf(directory)? else {
            return Ok(None);
        };
        let Some(leaf) = decode_leaf(self.memory.read(table, index))? else {
            return Ok(None);
        };
        low_page(leaf.phys)?;
        if leaf.access != Access::Read || self.owns(leaf.phys) {
            return Err(Error::CorruptTable);
        }
        if access != Access::Read {
            return Err(Error::PermissionDenied);
        }
        Ok(Some(leaf.phys + iova % PAGE_SIZE))
    }

    /// A failed map restores the exact prior directory/PTE image and private-page ownership,
    /// including an allocation failure after a mapped prefix crosses a 4 MiB boundary.
    pub fn map_read_only(&mut self, iova: u64, pages: &[u64]) -> Result<(), Error> {
        let len = (pages.len() as u64)
            .checked_mul(PAGE_SIZE)
            .ok_or(Error::InvalidRange)?;
        range(iova, len)?;
        for &phys in pages {
            low_page(phys)?;
            if self.owns(phys) {
                return Err(Error::TableAlias);
            }
        }
        for n in 0..pages.len() {
            let (directory, index) = indices(iova + n as u64 * PAGE_SIZE);
            if let Some(table) = self.leaf(directory)? {
                let word = self.memory.read(table, index);
                if word != 0 {
                    decode_leaf(word)?;
                    return Err(Error::AlreadyMapped);
                }
            }
        }
        let first = indices(iova).0;
        let last = indices(iova + len - 1).0;
        let needed = (first..=last)
            .filter(|&index| !self.leaves.iter().any(|leaf| leaf.index == index))
            .count();
        self.leaves
            .try_reserve(needed)
            .map_err(|_| Error::NoMemory)?;
        let old_leaves = self.leaves.len();
        for (n, &phys) in pages.iter().enumerate() {
            let (directory, index) = indices(iova + n as u64 * PAGE_SIZE);
            let table = match self.leaves.iter().find(|leaf| leaf.index == directory) {
                Some(leaf) => leaf.phys,
                None => {
                    let Some(table) = self.memory.allocate() else {
                        self.rollback(iova, n, old_leaves);
                        return Err(Error::NoMemory);
                    };
                    let valid = low_page(table).and_then(|_| {
                        if pages.contains(&table) {
                            Err(Error::TableAlias)
                        } else {
                            Ok(())
                        }
                    });
                    if let Err(error) = valid {
                        self.memory.release(table);
                        self.rollback(iova, n, old_leaves);
                        return Err(error);
                    }
                    for i in 0..ENTRIES {
                        self.memory.write(table, i, 0);
                    }
                    self.leaves.push(OwnedLeaf {
                        index: directory,
                        phys: table,
                    });
                    // All fallible work for this leaf precedes publishing its DTE. — KESTREL
                    self.memory.write(
                        self.root,
                        directory,
                        directory_word(table).expect("validated table page"),
                    );
                    table
                }
            };
            self.memory.write(
                table,
                index,
                leaf_word(phys, Access::Read).expect("validated data page"),
            );
        }
        Ok(())
    }

    fn rollback(&mut self, iova: u64, pages: usize, old_leaves: usize) {
        for n in 0..pages {
            let (directory, index) = indices(iova + n as u64 * PAGE_SIZE);
            let table = self
                .leaves
                .iter()
                .find(|leaf| leaf.index == directory)
                .expect("mapped prefix has a table")
                .phys;
            self.memory.write(table, index, 0);
        }
        for leaf in self.leaves.drain(old_leaves..) {
            self.memory.write(self.root, leaf.index, 0);
            self.memory.release(leaf.phys);
        }
    }

    /// Preflight the whole interval; a hole/corrupt word cannot remove a valid prefix.
    /// No hardware can reach these pages, so no live invalidation/drain is implied.
    pub fn unmap(&mut self, iova: u64, len: u64) -> Result<(), Error> {
        range(iova, len)?;
        for n in 0..len / PAGE_SIZE {
            if self.walk(iova + n * PAGE_SIZE, Access::Read)?.is_none() {
                return Err(Error::NotMapped);
            }
        }
        for n in 0..len / PAGE_SIZE {
            let (directory, index) = indices(iova + n * PAGE_SIZE);
            let table = self
                .leaves
                .iter()
                .find(|leaf| leaf.index == directory)
                .expect("preflighted leaf")
                .phys;
            self.memory.write(table, index, 0);
        }
        let first = indices(iova).0;
        let last = indices(iova + len - 1).0;
        let mut i = 0;
        while i < self.leaves.len() {
            let leaf = self.leaves[i];
            if (first..=last).contains(&leaf.index)
                && (0..ENTRIES).all(|n| self.memory.read(leaf.phys, n) == 0)
            {
                self.memory.write(self.root, leaf.index, 0);
                self.memory.release(leaf.phys);
                self.leaves.swap_remove(i);
            } else {
                i += 1;
            }
        }
        Ok(())
    }
}

impl<M: TableMemory> Drop for OfflineTable<M> {
    fn drop(&mut self) {
        for leaf in &self.leaves {
            self.memory.write(self.root, leaf.index, 0);
            self.memory.release(leaf.phys);
        }
        self.memory.release(self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::{boxed::Box, cell::RefCell, collections::BTreeMap, rc::Rc, vec, vec::Vec};

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Event {
        Allocate(u64),
        Write(u64, usize, u32),
        Release(u64),
    }
    struct Model {
        pages: BTreeMap<u64, Box<[u32; ENTRIES]>>,
        next: u64,
        root: Option<u64>,
        remaining: Option<usize>,
        events: Vec<Event>,
    }
    #[derive(Clone)]
    struct Memory(Rc<RefCell<Model>>);
    impl Memory {
        fn new() -> Self {
            Self(Rc::new(RefCell::new(Model {
                pages: BTreeMap::new(),
                next: 0x1000,
                root: None,
                remaining: None,
                events: Vec::new(),
            })))
        }
        fn image(&self) -> BTreeMap<u64, Box<[u32; ENTRIES]>> {
            self.0.borrow().pages.clone()
        }
    }
    impl TableMemory for Memory {
        fn allocate(&mut self) -> Option<u64> {
            let mut m = self.0.borrow_mut();
            if let Some(left) = &mut m.remaining {
                if *left == 0 {
                    return None;
                }
                *left -= 1;
            }
            let p = m.next;
            m.next += PAGE_SIZE;
            // Deliberately dirty pages test initialization instead of assuming it. — KESTREL
            assert!(m
                .pages
                .insert(p, Box::new([0xdead_beef; ENTRIES]))
                .is_none());
            if m.root.is_none() {
                m.root = Some(p);
            }
            m.events.push(Event::Allocate(p));
            Some(p)
        }
        fn read(&self, p: u64, i: usize) -> u32 {
            self.0.borrow().pages[&p][i]
        }
        fn write(&mut self, p: u64, i: usize, value: u32) {
            let mut m = self.0.borrow_mut();
            if Some(p) == m.root && value != 0 {
                let leaf = decode_directory(value).unwrap().unwrap();
                assert!(
                    m.pages[&leaf].iter().all(|&word| word == 0),
                    "new DTE published before leaf zeroing"
                );
            }
            m.pages.get_mut(&p).unwrap()[i] = value;
            m.events.push(Event::Write(p, i, value));
        }
        fn release(&mut self, p: u64) {
            let mut m = self.0.borrow_mut();
            if Some(p) != m.root {
                assert!(
                    !m.pages[&m.root.unwrap()]
                        .iter()
                        .any(|&word| word == directory_word(p).unwrap()),
                    "released a still-published leaf"
                );
            } else {
                assert_eq!(m.pages.len(), 1, "root released before leaves");
            }
            assert!(m.pages.remove(&p).is_some(), "double or foreign release");
            m.events.push(Event::Release(p));
        }
    }

    #[test]
    fn descriptor_vectors_pin_permissions_high_nibbles_and_all_address_bits() {
        assert_eq!(directory_word(0xab_1234_5000), Ok(0x1234_5ba1));
        assert_eq!(leaf_word(0xab_1234_5000, Access::Read), Ok(0x1234_5ba3));
        assert_eq!(leaf_word(0xab_1234_5000, Access::Write), Ok(0x1234_5ba5));
        assert_eq!(
            leaf_word(0xab_1234_5000, Access::ReadWrite),
            Ok(0x1234_5ba7)
        );
        for phys in [
            0,
            0xffff_f000,
            0x01_0000_0000,
            0x10_0000_0000,
            0xff_ffff_f000,
        ]
        .into_iter()
        .chain((12..40).map(|bit| 1 << bit))
        {
            assert_eq!(
                decode_directory(directory_word(phys).unwrap()),
                Ok(Some(phys))
            );
            for access in [Access::Read, Access::Write, Access::ReadWrite] {
                assert_eq!(
                    decode_leaf(leaf_word(phys, access).unwrap()),
                    Ok(Some(Leaf { phys, access }))
                );
            }
        }
        assert_eq!(directory_word(1 << 32), Ok(0x101));
        assert_eq!(directory_word(1 << 36), Ok(0x11));
        assert_eq!(leaf_word(0, Access::Read), Ok(3));
        assert_eq!(leaf_word(0, Access::Write), Ok(5));
    }

    #[test]
    fn descriptor_codec_rejects_truncation_security_reserved_and_missing_valid_bits() {
        for phys in [1 << 40, u64::MAX] {
            assert_eq!(directory_word(phys), Err(Error::UnsupportedAddress));
            assert_eq!(
                leaf_word(phys, Access::Read),
                Err(Error::UnsupportedAddress)
            );
        }
        for phys in [1, 0x1001, 0xffff_ffff] {
            assert_eq!(directory_word(phys), Err(Error::InvalidRange));
        }
        assert_eq!(decode_directory(0), Ok(None));
        assert_eq!(decode_leaf(0), Ok(None));
        for word in [2, 4, 8, 0x1000, 3, 5, 9] {
            assert_eq!(decode_directory(word), Err(Error::InvalidDescriptor));
        }
        for word in [1, 2, 4, 8, 0x1000, 0x100b, 0x100d, 0x100f] {
            assert_eq!(decode_leaf(word), Err(Error::InvalidDescriptor));
        }
    }

    #[test]
    fn owned_pages_hold_real_words_and_walk_nonidentity_read_only_addresses() {
        let memory = Memory::new();
        let mut table = OfflineTable::new(memory.clone()).unwrap();
        let pages = [0x1234_5000, 0x9abc_d000, 0];
        table.map_read_only(0x003f_f000, &pages).unwrap();
        let image = memory.image();
        assert_eq!(image[&table.root][0], 0x2001);
        assert_eq!(image[&table.root][1], 0x3001);
        assert_eq!(image[&0x2000][1023], 0x1234_5003);
        assert_eq!(image[&0x3000][0], 0x9abc_d003);
        assert_eq!(image[&0x3000][1], 3);
        assert_eq!(table.walk(0x003f_ffff, Access::Read), Ok(Some(0x1234_5fff)));
        assert_eq!(table.walk(0x0040_0234, Access::Read), Ok(Some(0x9abc_d234)));
        assert_eq!(table.walk(0x0040_1000, Access::Read), Ok(Some(0)));
        assert_eq!(table.walk(0x0040_2000, Access::Read), Ok(None));
        for access in [Access::Write, Access::ReadWrite] {
            assert_eq!(
                table.walk(0x0040_0000, access),
                Err(Error::PermissionDenied)
            );
        }
        table.unmap(0x003f_f000, 3 * PAGE_SIZE).unwrap();
        assert_eq!(memory.image().len(), 1);
        assert!(memory.image()[&table.root].iter().all(|&w| w == 0));
        drop(table);
        assert!(memory.image().is_empty());
    }

    #[test]
    fn top_iova_and_physical_pages_work_without_wrapping() {
        let mut table = OfflineTable::new(Memory::new()).unwrap();
        table.map_read_only(0xffff_f000, &[0xffff_f000]).unwrap();
        assert_eq!(table.walk(0xffff_ffff, Access::Read), Ok(Some(0xffff_ffff)));
        assert_eq!(table.walk(1 << 32, Access::Read), Err(Error::InvalidRange));
        table.unmap(0xffff_f000, PAGE_SIZE).unwrap();
        assert_eq!(table.walk(0xffff_ffff, Access::Read), Ok(None));
    }

    #[test]
    fn all_bad_ranges_pages_and_overlap_fail_before_any_mutation() {
        let memory = Memory::new();
        let mut table = OfflineTable::new(memory.clone()).unwrap();
        table.map_read_only(0x2000, &[0x1000_0000]).unwrap();
        let before = memory.image();
        memory.0.borrow_mut().events.clear();
        for (iova, pages, error) in [
            (0, vec![], Error::InvalidRange),
            (1, vec![0x2000_0000], Error::InvalidRange),
            (1 << 32, vec![0x2000_0000], Error::InvalidRange),
            (
                0xffff_f000,
                vec![0x2000_0000, 0x2000_1000],
                Error::InvalidRange,
            ),
            (u64::MAX - 4095, vec![0x2000_0000], Error::InvalidRange),
            (0, vec![0x2000_0000, 1 << 32], Error::UnsupportedAddress),
            (0, vec![0x2000_0000, 0x2000_1001], Error::InvalidRange),
            (
                0,
                vec![0x2000_0000, 0x2000_1000, 0x2000_2000],
                Error::AlreadyMapped,
            ),
        ] {
            assert_eq!(table.map_read_only(iova, &pages), Err(error));
            assert_eq!(memory.image(), before);
            assert!(memory.0.borrow().events.is_empty());
        }
    }

    #[test]
    fn allocation_failure_after_each_mapped_prefix_restores_bytes_and_ownership() {
        for permitted in 0..2 {
            let memory = Memory::new();
            let mut table = OfflineTable::new(memory.clone()).unwrap();
            table.map_read_only(0x2000, &[0x2000_0000]).unwrap();
            let before = memory.image();
            memory.0.borrow_mut().remaining = Some(permitted);
            memory.0.borrow_mut().events.clear();
            let pages: Vec<u64> = (0..1026).map(|n| 0x1000_0000 + n * PAGE_SIZE).collect();
            assert_eq!(
                table.map_read_only(0x003f_f000, &pages),
                Err(Error::NoMemory)
            );
            assert_eq!(memory.image(), before);
            assert!(
                memory
                    .0
                    .borrow()
                    .events
                    .iter()
                    .any(|e| matches!(e, Event::Write(_,_,word) if *word == 0x1000_0003)),
                "failure never exercised a mapped prefix"
            );
            assert_eq!(table.walk(0x2000, Access::Read), Ok(Some(0x2000_0000)));
            assert_eq!(table.walk(0x003f_f000, Access::Read), Ok(None));
            memory.0.borrow_mut().remaining = None;
            table.map_read_only(0x003f_f000, &pages).unwrap();
            assert_eq!(table.walk(0x0080_0000, Access::Read), Ok(Some(pages[1025])));
            table
                .unmap(0x003f_f000, pages.len() as u64 * PAGE_SIZE)
                .unwrap();
            assert_eq!(memory.image(), before);
            drop(table);
            assert!(memory.image().is_empty());
        }
    }

    #[test]
    fn high_table_allocation_rolls_back_a_prefix_without_zeroing_high_memory() {
        let memory = Memory::new();
        let mut table = OfflineTable::new(memory.clone()).unwrap();
        table.map_read_only(0, &[0x2000_0000]).unwrap();
        let before = memory.image();
        memory.0.borrow_mut().next = 1 << 32;
        memory.0.borrow_mut().events.clear();
        assert_eq!(
            table.map_read_only(0x003f_f000, &[0x1000_0000, 0x1000_1000]),
            Err(Error::UnsupportedAddress)
        );
        assert_eq!(memory.image(), before);
        assert!(!memory
            .0
            .borrow()
            .events
            .iter()
            .any(|e| matches!(e, Event::Write(p,_,_) if *p >= 1 << 32)));
    }

    #[test]
    fn private_root_existing_leaf_and_new_leaf_cannot_be_mapped_as_data() {
        let memory = Memory::new();
        let mut table = OfflineTable::new(memory.clone()).unwrap();
        table.map_read_only(0x1000, &[0x1000_0000]).unwrap();
        let before = memory.image();
        for page in [table.root, table.leaves[0].phys] {
            assert_eq!(
                table.map_read_only(0x0040_0000, &[page]),
                Err(Error::TableAlias)
            );
        }
        let next_leaf = memory.0.borrow().next;
        memory.0.borrow_mut().events.clear();
        assert_eq!(
            table.map_read_only(0x0040_0000, &[next_leaf]),
            Err(Error::TableAlias)
        );
        assert_eq!(memory.image(), before);
        assert!(!memory
            .0
            .borrow()
            .events
            .iter()
            .any(|e| matches!(e, Event::Write(..))));
    }

    #[test]
    fn unmap_preflights_holes_bad_lengths_and_corrupt_suffix_without_removing_prefix() {
        let memory = Memory::new();
        let mut table = OfflineTable::new(memory.clone()).unwrap();
        table
            .map_read_only(0x003f_f000, &[0x1000_0000, 0x1000_1000])
            .unwrap();
        let before = memory.image();
        memory.0.borrow_mut().events.clear();
        for (iova, len, error) in [
            (0, 0, Error::InvalidRange),
            (1, PAGE_SIZE, Error::InvalidRange),
            (0, PAGE_SIZE + 1, Error::InvalidRange),
            (0xffff_f000, 2 * PAGE_SIZE, Error::InvalidRange),
            (0x003f_f000, 3 * PAGE_SIZE, Error::NotMapped),
        ] {
            assert_eq!(table.unmap(iova, len), Err(error));
            assert_eq!(memory.image(), before);
        }
        let second = table.leaf(1).unwrap().unwrap();
        memory.0.borrow_mut().pages.get_mut(&second).unwrap()[0] |= 8;
        let corrupt = memory.image();
        assert_eq!(
            table.unmap(0x003f_f000, 2 * PAGE_SIZE),
            Err(Error::InvalidDescriptor)
        );
        assert_eq!(memory.image(), corrupt);
        assert!(memory.0.borrow().events.is_empty());
        memory.0.borrow_mut().pages.get_mut(&second).unwrap()[0] &= !8;
        table.unmap(0x003f_f000, 2 * PAGE_SIZE).unwrap();
        assert_eq!(
            table.unmap(0x003f_f000, 2 * PAGE_SIZE),
            Err(Error::NotMapped)
        );
    }

    #[test]
    fn corrupt_entries_never_dereference_or_release_unowned_memory() {
        let memory = Memory::new();
        let mut table = OfflineTable::new(memory.clone()).unwrap();
        table.map_read_only(0x1000, &[0x1000_0000]).unwrap();
        let leaf = table.leaves[0].phys;
        for value in [
            leaf_word(0x1000_0000, Access::ReadWrite).unwrap(),
            leaf_word(1 << 32, Access::Read).unwrap(),
            leaf_word(table.root, Access::Read).unwrap(),
        ] {
            memory.0.borrow_mut().pages.get_mut(&leaf).unwrap()[1] = value;
            assert!(table.walk(0x1000, Access::Read).is_err());
        }
        memory.0.borrow_mut().pages.get_mut(&table.root).unwrap()[0] =
            directory_word(0xdead_0000).unwrap();
        assert_eq!(table.walk(0x1000, Access::Read), Err(Error::CorruptTable));
        assert_eq!(table.unmap(0x1000, PAGE_SIZE), Err(Error::CorruptTable));
        drop(table);
        assert!(memory.image().is_empty());
        let released: Vec<u64> = memory
            .0
            .borrow()
            .events
            .iter()
            .filter_map(|e| {
                if let Event::Release(p) = e {
                    Some(*p)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(released, [leaf, 0x1000]);
    }

    #[test]
    fn directory_allocation_failure_or_high_page_leaves_no_owned_pages() {
        let memory = Memory::new();
        memory.0.borrow_mut().remaining = Some(0);
        assert!(matches!(
            OfflineTable::new(memory.clone()),
            Err(Error::NoMemory)
        ));
        assert!(memory.image().is_empty());
        memory.0.borrow_mut().remaining = None;
        memory.0.borrow_mut().next = 1 << 32;
        assert!(matches!(
            OfflineTable::new(memory.clone()),
            Err(Error::UnsupportedAddress)
        ));
        assert!(memory.image().is_empty());
        assert!(!memory
            .0
            .borrow()
            .events
            .iter()
            .any(|e| matches!(e, Event::Write(..))));
    }
    #[test]
    fn publication_cleans_owned_leaves_before_root_and_uncertain_drop_retains_pages() {
        let memory = Memory::new();
        let mut table = OfflineTable::new(memory.clone()).unwrap();
        table
            .map_read_only(0x3ff000, &[0x10000000, 0x20000000])
            .unwrap();
        let root = table.root;
        let before = memory.image();
        let mut cleaned = Vec::new();
        let published = table.publish(|p| cleaned.push(p));
        assert_eq!(published.root(), root);
        assert_eq!(cleaned.last(), Some(&root));
        assert_eq!(cleaned.len(), 3);
        for page in before.keys() {
            assert!(cleaned.contains(page));
        }
        drop(published);
        assert_eq!(memory.image(), before);
    }
    #[test]
    fn only_a_matching_single_use_reset_can_reclaim_frozen_tables() {
        let memory = Memory::new();
        let mut table = OfflineTable::new(memory.clone()).unwrap();
        table.map_read_only(0x1000, &[0x10000000]).unwrap();
        let root = table.root;
        let published = table.publish(|_| {});
        let published = match published.reclaim(DetachedRoot::after_reset(root + 4096)) {
            Ok(_) => panic!("wrong root reclaimed"),
            Err(p) => p,
        };
        assert_eq!(memory.image().len(), 2);
        let mut offline = match published.reclaim(DetachedRoot::after_reset(root)) {
            Ok(p) => p,
            Err(_) => panic!("matching reset refused"),
        };
        assert_eq!(offline.walk(0x1123, Access::Read), Ok(Some(0x10000123)));
        offline.unmap(0x1000, 4096).unwrap();
        drop(offline);
        assert!(memory.image().is_empty());
    }
    #[test]
    fn panicking_cleaner_never_runs_offline_destruction() {
        let memory = Memory::new();
        let mut table = OfflineTable::new(memory.clone()).unwrap();
        table.map_read_only(0x1000, &[0x10000000]).unwrap();
        let before = memory.image();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            table.publish(|_| panic!("clean failed"));
        }))
        .is_err());
        assert_eq!(memory.image(), before);
    }
}
