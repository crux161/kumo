use alloc::vec::Vec;

use kumo_abi::{KoId, ObjectKind, Signals};
use kumo_hal::active::{ThreadContext, UserState};

use crate::mm::{Mapping, Vmar, PAGE_SIZE};
use crate::object::{HandleTable, KernelObject, ObjectManager};

pub const DEFAULT_KERNEL_STACK_SIZE: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskError {
    EmptyStack,
    StackTooSmall,
    /// The allocator returned a block outside the heap it owns. Never a caller error — this is the
    /// allocator's free list having been corrupted, caught before a thread runs on the result.
    StackOutsideHeap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Job {
    object: KernelObject,
    parent: Option<KoId>,
}

impl Job {
    pub fn root(objects: &mut ObjectManager) -> Self {
        Self {
            object: objects.create(ObjectKind::Job),
            parent: None,
        }
    }

    pub fn child(objects: &mut ObjectManager, parent: &Job) -> Self {
        Self {
            object: objects.create(ObjectKind::Job),
            parent: Some(parent.koid()),
        }
    }

    pub const fn koid(self) -> KoId {
        self.object.koid()
    }

    pub const fn parent(self) -> Option<KoId> {
        self.parent
    }
}

#[derive(Clone, Debug)]
pub struct Process {
    object: KernelObject,
    job: KoId,
    root_vmar: Vmar,
    handles: HandleTable,
    mappings: Vec<(Mapping, KoId)>,
    /// User stack range installed by AddressSpaceCreate. Kept separately from VMO
    /// mappings so later live VmarMap calls cannot silently overwrite it.
    user_stack: Option<(u64, u64)>,
    pub ttbr0: Option<u64>,
}

impl Process {
    pub fn new(objects: &mut ObjectManager, job: &Job, root_vmar: Vmar) -> Self {
        Self {
            object: objects.create(ObjectKind::Process),
            job: job.koid(),
            root_vmar,
            handles: HandleTable::new(),
            mappings: Vec::new(),
            user_stack: None,
            ttbr0: None,
        }
    }

    /// Build a Process from raw parts (scaffold for borrow-splitting in syscall
    /// dispatch — lets Thread::new receive a &Process when the real one is behind
    /// a mutable borrow). The returned Process has no handle table and a fake
    /// KernelObject; it exists only to satisfy Thread::new's signature.
    pub fn from_parts(koid: KoId, root_vmar: Vmar) -> Self {
        Self {
            object: crate::object::KernelObject::new(koid, kumo_abi::ObjectKind::Process),
            job: KoId(0),
            root_vmar,
            handles: HandleTable::new(),
            mappings: Vec::new(),
            user_stack: None,
            ttbr0: None,
        }
    }

    pub const fn koid(&self) -> KoId {
        self.object.koid()
    }

    pub const fn object(&self) -> KernelObject {
        self.object
    }

    pub fn add_mapping(&mut self, mapping: Mapping, vmo_koid: KoId) {
        self.mappings.push((mapping, vmo_koid));
    }

    /// Whether `[start, start + len)` is unused by both VMO mappings and the user stack.
    pub fn user_range_is_free(&self, start: u64, len: u64) -> bool {
        let Some(end) = start.checked_add(len) else {
            return false;
        };
        let overlaps = |other_start: u64, other_len: u64| {
            other_start
                .checked_add(other_len)
                .is_none_or(|other_end| start < other_end && other_start < end)
        };
        !self
            .mappings
            .iter()
            .any(|(mapping, _)| overlaps(mapping.virt, mapping.len))
            && !self
                .user_stack
                .is_some_and(|(stack_start, stack_len)| overlaps(stack_start, stack_len))
    }

    /// Find a free, page-aligned span of `len` bytes inside this process's root VMAR.
    ///
    /// The kernel places heap growth because it is the only party that can. `kumo-rt` cannot pick a
    /// base: a child's VMAR is 512 MiB from 0 and already carries its image, its stacks and any
    /// device mappings, while Sora's is 2 GiB from a different base — there is no constant that is
    /// safe in both, which is exactly why the previous sub-slice declined to guess one.
    ///
    /// Searches upward from the highest existing mapping so growth lands away from the image and
    /// stacks rather than in the gaps between them; a gap large enough for one growth is usually a
    /// gap somebody else is about to want. Skips the first page so a null dereference stays a
    /// fault, and never returns a span touching the user stack.
    pub fn find_free_span(&self, len: u64, align: u64) -> Option<u64> {
        if len == 0 || align == 0 || !align.is_power_of_two() {
            return None;
        }
        let vmar_base = self.root_vmar.base();
        let vmar_end = vmar_base.checked_add(self.root_vmar.len())?;

        // Never hand out the first page of the address space: a null pointer must keep faulting.
        let floor = vmar_base.max(align);
        let mut candidate = floor;
        for (mapping, _) in &self.mappings {
            let end = mapping.virt.checked_add(mapping.len)?;
            candidate = candidate.max(end);
        }
        if let Some((stack_start, stack_len)) = self.user_stack {
            candidate = candidate.max(stack_start.checked_add(stack_len)?);
        }

        // Round up, then walk forward past anything the scan above did not dominate.
        let mask = align - 1;
        candidate = candidate.checked_add(mask)? & !mask;
        while candidate
            .checked_add(len)
            .is_some_and(|end| end <= vmar_end)
        {
            if self.user_range_is_free(candidate, len) {
                return Some(candidate);
            }
            candidate = candidate.checked_add(align)?;
        }
        None
    }

    pub fn set_user_stack(&mut self, start: u64, len: u64) {
        self.user_stack = Some((start, len));
    }

    pub fn mappings(&self) -> &[(Mapping, KoId)] {
        &self.mappings
    }

    pub const fn job(&self) -> KoId {
        self.job
    }

    pub const fn root_vmar(&self) -> Vmar {
        self.root_vmar
    }

    pub fn handles(&self) -> &HandleTable {
        &self.handles
    }

    pub fn handles_mut(&mut self) -> &mut HandleTable {
        &mut self.handles
    }

    pub fn signal(&mut self, signals: kumo_abi::Signals) {
        self.object.signal(signals);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelStack {
    bytes: Vec<u8>,
}

impl KernelStack {
    pub fn new(size: usize) -> Result<Self, TaskError> {
        if size == 0 {
            return Err(TaskError::EmptyStack);
        }
        if size < PAGE_SIZE as usize {
            return Err(TaskError::StackTooSmall);
        }

        let size = align_up_usize(size, 16).ok_or(TaskError::StackTooSmall)?;
        let mut bytes = Vec::new();
        bytes.resize(size, 0);
        // A kernel thread runs on this, and an IRQ pushes 0x110 bytes onto it at any moment. If the
        // allocator ever hands back a block outside its own arena — a torn free list, the failure
        // this allocator has a history of — the result is a stack pointer into unmapped memory and a
        // kernel fault later, somewhere else, with nothing to connect it back here. Refuse it at the
        // point of issue instead, where the cause is still in scope.
        let (heap_lo, heap_hi) = crate::mm::heap::range();
        if heap_lo != heap_hi {
            let lo = bytes.as_ptr() as u64;
            let hi = lo.saturating_add(size as u64);
            if lo < heap_lo || hi > heap_hi {
                return Err(TaskError::StackOutsideHeap);
            }
        }
        Ok(Self { bytes })
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn top(&self) -> usize {
        align_down_usize(self.bytes.as_ptr() as usize + self.bytes.len(), 16)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadState {
    New,
    Ready,
    Running,
    Blocked,
    Terminated,
}

pub struct Thread {
    object: KernelObject,
    process: KoId,
    state: ThreadState,
    stack: KernelStack,
    context: ThreadContext,
    pub user_state: Option<UserState>,
}

impl Clone for Thread {
    fn clone(&self) -> Self {
        Self {
            object: self.object,
            process: self.process,
            state: self.state,
            stack: self.stack.clone(),
            context: self.context,
            user_state: None, // UserState isn't Clone; drop on clone
        }
    }
}

impl core::fmt::Debug for Thread {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Thread")
            .field("object", &self.object)
            .field("process", &self.process)
            .field("state", &self.state)
            .field("stack", &self.stack)
            .field("context", &self.context)
            .field(
                "user_state",
                &self.user_state.as_ref().map(|_| "<UserState>"),
            )
            .finish()
    }
}

impl Thread {
    pub fn new(
        objects: &mut ObjectManager,
        process: &Process,
        entry: usize,
        arg: usize,
        stack_size: usize,
    ) -> Result<Self, TaskError> {
        let stack = KernelStack::new(stack_size)?;
        let context = ThreadContext::new(entry, arg, stack.top(), false);
        Ok(Self {
            object: objects.create(ObjectKind::Thread),
            process: process.koid(),
            state: ThreadState::New,
            stack,
            context,
            user_state: None,
        })
    }

    pub const fn koid(&self) -> KoId {
        self.object.koid()
    }

    pub const fn object(&self) -> KernelObject {
        self.object
    }

    pub const fn process(&self) -> KoId {
        self.process
    }

    pub const fn state(&self) -> ThreadState {
        self.state
    }

    pub const fn signals(&self) -> Signals {
        self.object.signals()
    }

    pub fn stack(&self) -> &KernelStack {
        &self.stack
    }

    pub const fn context(&self) -> &ThreadContext {
        &self.context
    }

    pub fn context_mut(&mut self) -> &mut ThreadContext {
        &mut self.context
    }

    pub fn block(&mut self) {
        if !matches!(self.state, ThreadState::Terminated) {
            self.state = ThreadState::Blocked;
        }
    }

    pub fn ready(&mut self) {
        if !matches!(self.state, ThreadState::Terminated) {
            self.state = ThreadState::Ready;
        }
    }

    pub fn run(&mut self) {
        if !matches!(self.state, ThreadState::Terminated) {
            self.state = ThreadState::Running;
        }
    }

    pub fn terminate(&mut self) {
        self.state = ThreadState::Terminated;
        self.object.signal(Signals::TERMINATED);
    }
}

// The run-queue / scheduling policy used to live here as a flat round-robin
// `Scheduler`. It has been replaced by the modular, strict-priority scheduler in
// `crate::sched` (Discipline A — the O(1) bitmap; `DESIGN/003`). `task` now owns only
// the schedulable objects (Job / Process / Thread / KernelStack); the dispatcher owns
// the policy.

fn align_up_usize(value: usize, align: usize) -> Option<usize> {
    let mask = align.checked_sub(1)?;
    value.checked_add(mask).map(|value| value & !mask)
}

fn align_down_usize(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_abi::{ObjectKind, Rights};
    use kumo_hal::PageFlags;

    extern "C" fn test_entry(_arg: usize) {}

    fn test_entry_addr() -> usize {
        test_entry as *const () as usize
    }

    fn test_vmar() -> Vmar {
        Vmar::new(0xffff_0000_0000_0000, PAGE_SIZE * 16).unwrap()
    }

    #[test]
    fn jobs_processes_and_threads_have_kernel_objects() {
        let mut objects = ObjectManager::new();
        let root = Job::root(&mut objects);
        let child = Job::child(&mut objects, &root);
        let process = Process::new(&mut objects, &child, test_vmar());
        let thread = Thread::new(
            &mut objects,
            &process,
            test_entry_addr(),
            0xabc,
            DEFAULT_KERNEL_STACK_SIZE,
        )
        .unwrap();

        assert_eq!(root.parent(), None);
        assert_eq!(child.parent(), Some(root.koid()));
        assert_eq!(process.job(), child.koid());
        assert_eq!(process.root_vmar(), test_vmar());
        assert_eq!(thread.process(), process.koid());
        assert_eq!(thread.state(), ThreadState::New);
        assert_eq!(thread.context().entry(), test_entry_addr() as u64);
        assert_eq!(thread.context().arg(), 0xabc);
        assert_eq!(thread.context().stack_top() as usize % 16, 0);
        assert_eq!(thread.stack().len(), DEFAULT_KERNEL_STACK_SIZE);
    }

    #[test]
    fn process_owns_a_process_local_handle_table() {
        let mut objects = ObjectManager::new();
        let root = Job::root(&mut objects);
        let mut process = Process::new(&mut objects, &root, test_vmar());
        let resource = objects.create(ObjectKind::Resource);
        let handle = process
            .handles_mut()
            .insert(resource, Rights::MANAGE | Rights::DUPLICATE)
            .unwrap();

        assert!(process
            .handles()
            .require(handle, ObjectKind::Resource, Rights::MANAGE)
            .is_ok());
    }

    #[test]
    fn process_rejects_ranges_overlapping_mappings_or_user_stack() {
        let mut objects = ObjectManager::new();
        let root = Job::root(&mut objects);
        let mut process = Process::new(&mut objects, &root, test_vmar());
        let base = test_vmar().base();
        process.add_mapping(
            Mapping {
                virt: base,
                len: PAGE_SIZE * 2,
                vmo_offset: 0,
                flags: PageFlags::READ,
            },
            KoId(99),
        );
        process.set_user_stack(base + PAGE_SIZE * 8, PAGE_SIZE * 4);

        assert!(!process.user_range_is_free(base + PAGE_SIZE, PAGE_SIZE));
        assert!(!process.user_range_is_free(base + PAGE_SIZE * 7, PAGE_SIZE * 2));
        assert!(process.user_range_is_free(base + PAGE_SIZE * 3, PAGE_SIZE));
    }

    #[test]
    fn placement_lands_above_every_existing_mapping_and_the_stack() {
        let mut objects = ObjectManager::new();
        let root = Job::root(&mut objects);
        let mut process = Process::new(&mut objects, &root, test_vmar());
        let base = test_vmar().base();
        process.add_mapping(
            Mapping {
                virt: base,
                len: PAGE_SIZE * 2,
                vmo_offset: 0,
                flags: PageFlags::READ,
            },
            KoId(99),
        );
        process.set_user_stack(base + PAGE_SIZE * 8, PAGE_SIZE * 4);

        // Above the stack (the highest claim), not in the gap between the image and the stack —
        // a gap big enough for one growth is a gap something else is about to want.
        let span = process.find_free_span(PAGE_SIZE, PAGE_SIZE).expect("span");
        assert_eq!(span, base + PAGE_SIZE * 12);
        assert!(process.user_range_is_free(span, PAGE_SIZE));
    }

    #[test]
    fn placement_never_returns_the_first_page() {
        // A VMAR based at 0 (every child has one) must still keep a null dereference faulting.
        let mut objects = ObjectManager::new();
        let root = Job::root(&mut objects);
        let process = Process::new(
            &mut objects,
            &root,
            Vmar::new(0, PAGE_SIZE * 16).expect("vmar"),
        );
        let span = process.find_free_span(PAGE_SIZE, PAGE_SIZE).expect("span");
        assert!(span >= PAGE_SIZE, "handed out the null page: {span:#x}");
    }

    #[test]
    fn placement_refuses_when_the_vmar_cannot_hold_the_request() {
        let mut objects = ObjectManager::new();
        let root = Job::root(&mut objects);
        let process = Process::new(
            &mut objects,
            &root,
            Vmar::new(PAGE_SIZE, PAGE_SIZE * 4).expect("vmar"),
        );
        // Larger than the whole VMAR: no span, rather than an out-of-range address.
        assert_eq!(process.find_free_span(PAGE_SIZE * 8, PAGE_SIZE), None);
        // And a zero-length or non-power-of-two alignment is a caller bug, not a placement.
        assert_eq!(process.find_free_span(0, PAGE_SIZE), None);
        assert_eq!(process.find_free_span(PAGE_SIZE, 3), None);
    }

    #[test]
    fn repeated_placements_do_not_overlap_each_other() {
        // The growth path calls this once per new region; two regions sharing an address would
        // corrupt the heap in a way that looks like anything but placement.
        let mut objects = ObjectManager::new();
        let root = Job::root(&mut objects);
        let mut process = Process::new(&mut objects, &root, test_vmar());
        let mut previous: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();
        for i in 0..4 {
            let span = process
                .find_free_span(PAGE_SIZE * 2, PAGE_SIZE)
                .expect("span");
            for (start, len) in &previous {
                assert!(
                    span >= start + len || span + PAGE_SIZE * 2 <= *start,
                    "placement {i} at {span:#x} overlaps {start:#x}"
                );
            }
            process.add_mapping(
                Mapping {
                    virt: span,
                    len: PAGE_SIZE * 2,
                    vmo_offset: 0,
                    flags: PageFlags::READ | PageFlags::WRITE,
                },
                KoId(100 + i),
            );
            previous.push((span, PAGE_SIZE * 2));
        }
    }

    #[test]
    fn terminated_threads_signal_and_drop_off_runnable() {
        let mut objects = ObjectManager::new();
        let root = Job::root(&mut objects);
        let process = Process::new(&mut objects, &root, test_vmar());
        let mut thread = Thread::new(
            &mut objects,
            &process,
            test_entry_addr(),
            0,
            DEFAULT_KERNEL_STACK_SIZE,
        )
        .unwrap();

        thread.terminate();

        assert_eq!(thread.state(), ThreadState::Terminated);
        assert!(thread.signals().contains(Signals::TERMINATED));
    }

    #[test]
    fn rejects_unusable_kernel_stacks() {
        // On the host the heap range is (0, 0) — "unknown" — and the guard stands down rather
        // than rejecting every stack. The check is live only where the arena is real.
        assert_eq!(crate::mm::heap::range(), (0, 0));
        assert!(KernelStack::new(PAGE_SIZE as usize).is_ok());
        assert_eq!(KernelStack::new(0), Err(TaskError::EmptyStack));
        assert_eq!(
            KernelStack::new(PAGE_SIZE as usize - 1),
            Err(TaskError::StackTooSmall)
        );
    }
}
