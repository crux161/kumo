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

    pub fn terminate(&mut self) {
        self.state = ThreadState::Terminated;
        self.object.signal(Signals::TERMINATED);
    }
}

#[derive(Clone, Debug, Default)]
pub struct Scheduler {
    current: Option<KoId>,
    run_queue: Vec<KoId>,
    quantum_ticks: u32,
    ticks_in_quantum: u32,
}

impl Scheduler {
    pub const fn new() -> Self {
        Self {
            current: None,
            run_queue: Vec::new(),
            quantum_ticks: 1,
            ticks_in_quantum: 0,
        }
    }

    pub const fn with_quantum(quantum_ticks: u32) -> Self {
        Self {
            current: None,
            run_queue: Vec::new(),
            quantum_ticks: if quantum_ticks == 0 { 1 } else { quantum_ticks },
            ticks_in_quantum: 0,
        }
    }

    pub fn enqueue(&mut self, thread: &mut Thread) {
        if matches!(thread.state, ThreadState::Terminated) {
            return;
        }
        thread.state = ThreadState::Ready;
        let koid = thread.koid();
        if !self.run_queue.iter().any(|queued| *queued == koid) {
            self.run_queue.push(koid);
        }
    }

    pub fn pick_next(&mut self) -> Option<KoId> {
        if self.run_queue.is_empty() {
            None
        } else {
            Some(self.run_queue.remove(0))
        }
    }

    pub fn set_running(&mut self, thread: &mut Thread) {
        thread.state = ThreadState::Running;
        self.current = Some(thread.koid());
        self.ticks_in_quantum = 0;
    }

    pub fn on_timer_tick(&mut self) -> ScheduleDecision {
        self.ticks_in_quantum = self.ticks_in_quantum.saturating_add(1);
        let Some(current) = self.current else {
            return match self.pick_next() {
                Some(next) => {
                    self.current = Some(next);
                    self.ticks_in_quantum = 0;
                    ScheduleDecision::Switch {
                        from: None,
                        to: next,
                    }
                }
                None => ScheduleDecision::Idle,
            };
        };

        if self.ticks_in_quantum < self.quantum_ticks {
            return ScheduleDecision::Continue(current);
        }

        self.ticks_in_quantum = 0;
        match self.pick_next() {
            Some(next) => {
                self.run_queue.push(current);
                self.current = Some(next);
                ScheduleDecision::Switch {
                    from: Some(current),
                    to: next,
                }
            }
            None => ScheduleDecision::Continue(current),
        }
    }

    pub fn current(&self) -> Option<KoId> {
        self.current
    }

    pub fn queued_count(&self) -> usize {
        self.run_queue.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleDecision {
    Idle,
    Continue(KoId),
    Switch { from: Option<KoId>, to: KoId },
}

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
    fn scheduler_is_fifo_and_does_not_duplicate_ready_threads() {
        let mut objects = ObjectManager::new();
        let root = Job::root(&mut objects);
        let process = Process::new(&mut objects, &root, test_vmar());
        let mut a = Thread::new(
            &mut objects,
            &process,
            test_entry_addr(),
            1,
            DEFAULT_KERNEL_STACK_SIZE,
        )
        .unwrap();
        let mut b = Thread::new(
            &mut objects,
            &process,
            test_entry_addr(),
            2,
            DEFAULT_KERNEL_STACK_SIZE,
        )
        .unwrap();
        let mut scheduler = Scheduler::new();

        scheduler.enqueue(&mut a);
        scheduler.enqueue(&mut a);
        scheduler.enqueue(&mut b);

        assert_eq!(scheduler.queued_count(), 2);
        assert_eq!(scheduler.pick_next(), Some(a.koid()));
        assert_eq!(scheduler.pick_next(), Some(b.koid()));
        assert_eq!(scheduler.pick_next(), None);
    }

    #[test]
    fn terminated_threads_signal_and_stay_off_the_run_queue() {
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
        let mut scheduler = Scheduler::new();

        thread.terminate();
        scheduler.enqueue(&mut thread);

        assert_eq!(thread.state(), ThreadState::Terminated);
        assert!(thread.signals().contains(Signals::TERMINATED));
        assert_eq!(scheduler.queued_count(), 0);
    }

    #[test]
    fn rejects_unusable_kernel_stacks() {
        assert_eq!(KernelStack::new(0), Err(TaskError::EmptyStack));
        assert_eq!(
            KernelStack::new(PAGE_SIZE as usize - 1),
            Err(TaskError::StackTooSmall)
        );
    }

    #[test]
    fn timer_tick_decisions_rotate_on_quantum() {
        let mut objects = ObjectManager::new();
        let root = Job::root(&mut objects);
        let process = Process::new(&mut objects, &root, test_vmar());
        let mut a = Thread::new(
            &mut objects,
            &process,
            test_entry_addr(),
            1,
            DEFAULT_KERNEL_STACK_SIZE,
        )
        .unwrap();
        let mut b = Thread::new(
            &mut objects,
            &process,
            test_entry_addr(),
            2,
            DEFAULT_KERNEL_STACK_SIZE,
        )
        .unwrap();
        let mut scheduler = Scheduler::with_quantum(2);

        scheduler.set_running(&mut a);
        scheduler.enqueue(&mut b);

        assert_eq!(
            scheduler.on_timer_tick(),
            ScheduleDecision::Continue(a.koid())
        );
        assert_eq!(
            scheduler.on_timer_tick(),
            ScheduleDecision::Switch {
                from: Some(a.koid()),
                to: b.koid()
            }
        );
        assert_eq!(scheduler.current(), Some(b.koid()));
        assert_eq!(scheduler.queued_count(), 1);
    }
}
