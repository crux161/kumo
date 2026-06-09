use alloc::vec::Vec;

use kumo_abi::{KoId, ObjectKind, Signals};
use kumo_hal::active::ThreadContext;

use crate::mm::{Vmar, PAGE_SIZE};
use crate::object::{HandleTable, KernelObject, ObjectManager};

pub const DEFAULT_KERNEL_STACK_SIZE: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskError {
    EmptyStack,
    StackTooSmall,
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
}

impl Process {
    pub fn new(objects: &mut ObjectManager, job: &Job, root_vmar: Vmar) -> Self {
        Self {
            object: objects.create(ObjectKind::Process),
            job: job.koid(),
            root_vmar,
            handles: HandleTable::new(),
        }
    }

    pub const fn koid(&self) -> KoId {
        self.object.koid()
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Thread {
    object: KernelObject,
    process: KoId,
    state: ThreadState,
    stack: KernelStack,
    context: ThreadContext,
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
        })
    }

    pub const fn koid(&self) -> KoId {
        self.object.koid()
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
        assert_eq!(KernelStack::new(0), Err(TaskError::EmptyStack));
        assert_eq!(
            KernelStack::new(PAGE_SIZE as usize - 1),
            Err(TaskError::StackTooSmall)
        );
    }
}
