#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j495
//j496
//j497
//j498

//! In-process work queues for KUMO userland.
//!
//! Submitters enqueue function-pointer work without crossing a process boundary. A bounded serial
//! queue has one claimed worker; a bounded concurrent queue permits multiple workers to drain the
//! same FIFO. Both disciplines support asynchronous and synchronous submission. Empty workers park
//! on a futex. [`Once`] provides the first small composition gate.

use core::cell::{Cell, UnsafeCell};
use core::hint::spin_loop;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

#[cfg(target_os = "none")]
use kumo_rt::{futex_wait, futex_wake};

#[cfg(target_os = "none")]
fn wait_word(addr: *const u32, expected: u32) {
    let _ = futex_wait(addr, expected);
}

#[cfg(not(target_os = "none"))]
fn wait_word(_addr: *const u32, _expected: u32) {
    // Host tests have native parallel threads but no KUMO syscall ABI; retrying the acquire-load
    // preserves the queue protocol while the OS schedules the worker.
    spin_loop();
}

#[cfg(target_os = "none")]
fn wake_word(addr: *const u32) {
    let _ = futex_wake(addr, 1);
}

#[cfg(not(target_os = "none"))]
fn wake_word(_addr: *const u32) {}

#[cfg(target_os = "none")]
fn wake_all_word(addr: *const u32) {
    let _ = futex_wake(addr, u32::MAX);
}

#[cfg(not(target_os = "none"))]
fn wake_all_word(_addr: *const u32) {}

/// A work item function: it receives one opaque context word and returns one word.
pub type WorkFn = fn(usize) -> usize;

const ONCE_UNSTARTED: u32 = 0;
const ONCE_RUNNING: u32 = 1;
const ONCE_COMPLETE: u32 = 2;

/// An allocation-free gate that runs one initializer and publishes its result to every caller.
///
/// The first caller supplies the function and context that execute. Contending and later callers
/// ignore their own supplied function and context, park while initialization is in progress, and
/// return the first caller's result.
pub struct Once {
    state: AtomicU32,
    result: UnsafeCell<usize>,
}

impl Once {
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(ONCE_UNSTARTED),
            result: UnsafeCell::new(0),
        }
    }

    /// Run the first initializer exactly once and return its cached result.
    pub fn call_once(&self, function: WorkFn, context: usize) -> usize {
        if self
            .state
            .compare_exchange(
                ONCE_UNSTARTED,
                ONCE_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let result = function(context);
            // The successful caller is the only writer; the release-store publishes this value.
            unsafe { *self.result.get() = result };
            self.state.store(ONCE_COMPLETE, Ordering::Release);
            wake_all_word(self.state.as_ptr());
            return result;
        }

        let mut waited = false;
        loop {
            let observed = self.state.load(Ordering::Acquire);
            if observed == ONCE_COMPLETE {
                if waited {
                    // KUMO bounds one wake syscall. A resumed cohort wakes any remaining cohort so
                    // more contenders than that bound cannot remain stranded. — KESTREL 2026-07-26
                    wake_all_word(self.state.as_ptr());
                }
                // The acquire-load pairs with the initializer's release-store.
                return unsafe { *self.result.get() };
            }
            waited = true;
            wait_word(self.state.as_ptr(), observed);
        }
    }

    pub fn is_completed(&self) -> bool {
        self.state.load(Ordering::Acquire) == ONCE_COMPLETE
    }
}

impl Default for Once {
    fn default() -> Self {
        Self::new()
    }
}

// The state word permits one result writer and release-publishes that write before any reader.
// — KESTREL 2026-07-26
unsafe impl Sync for Once {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitError {
    /// The bounded queue has no free slot. No work was accepted.
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerError {
    /// A serial queue already has its one worker.
    AlreadyClaimed,
}

#[derive(Clone, Copy)]
struct Work {
    function: WorkFn,
    context: usize,
    completion: *const Completion,
}

struct Completion {
    state: AtomicU32,
    result: UnsafeCell<usize>,
}

impl Completion {
    const fn new() -> Self {
        Self {
            state: AtomicU32::new(0),
            result: UnsafeCell::new(0),
        }
    }

    fn finish(&self, result: usize) {
        // The worker is the only writer, and the submitter reads only after the release-store.
        unsafe { *self.result.get() = result };
        self.state.store(1, Ordering::Release);
        wake_word(self.state.as_ptr());
    }

    fn wait(&self) -> usize {
        loop {
            let observed = self.state.load(Ordering::Acquire);
            if observed != 0 {
                // The acquire-load pairs with `finish` and makes the result write visible.
                return unsafe { *self.result.get() };
            }
            wait_word(self.state.as_ptr(), observed);
        }
    }
}

// A Completion is shared by exactly one blocked submitter and the queue's claimed worker. The
// state word publishes the single result write before the submitter reads it. — KESTREL 2026-07-26
unsafe impl Sync for Completion {}

struct QueueState<const N: usize> {
    slots: [MaybeUninit<Work>; N],
    head: usize,
    len: usize,
}

impl<const N: usize> QueueState<N> {
    const fn new() -> Self {
        Self {
            slots: [MaybeUninit::uninit(); N],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, work: Work) -> Result<(), SubmitError> {
        if self.len == N || N == 0 {
            return Err(SubmitError::Full);
        }
        let tail = (self.head + self.len) % N;
        self.slots[tail].write(work);
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<Work> {
        if self.len == 0 {
            return None;
        }
        let index = self.head;
        self.head = (self.head + 1) % N;
        self.len -= 1;
        // `len > 0` proves this FIFO slot was initialized and not yet consumed.
        Some(unsafe { self.slots[index].assume_init_read() })
    }
}

struct GateGuard<'a> {
    gate: &'a AtomicBool,
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.gate.store(false, Ordering::Release);
    }
}

/// Shared bounded-ring mechanism beneath the public queue disciplines.
///
/// The short spin gate protects only ring metadata and slot moves; work executes after the gate is
/// released. On today's single core, a thread preempted inside that small critical section is
/// resumed by D3's timer fairness, so a peer cannot strand the gate permanently.
/// — KESTREL 2026-07-26
struct QueueCore<const N: usize> {
    gate: AtomicBool,
    state: UnsafeCell<QueueState<N>>,
    sequence: AtomicU32,
}

impl<const N: usize> QueueCore<N> {
    const fn new() -> Self {
        Self {
            gate: AtomicBool::new(false),
            state: UnsafeCell::new(QueueState::new()),
            sequence: AtomicU32::new(0),
        }
    }

    fn len(&self) -> usize {
        self.with_state(|state| state.len)
    }

    fn push(&self, work: Work) -> Result<(), SubmitError> {
        self.with_state(|state| state.push(work))?;
        self.sequence.fetch_add(1, Ordering::Release);
        wake_word(self.sequence.as_ptr());
        Ok(())
    }

    fn pop(&self) -> Option<Work> {
        self.with_state(QueueState::pop)
    }

    fn try_run_one(&self) -> Option<usize> {
        let work = self.pop()?;
        Some(execute(work))
    }

    fn run_one(&self) -> usize {
        loop {
            let observed = self.sequence.load(Ordering::Acquire);
            if let Some(result) = self.try_run_one() {
                return result;
            }
            // A producer publishes by incrementing `sequence` before waking. If it races between
            // the empty check and this syscall, FutexWait observes the mismatch and does not park.
            wait_word(self.sequence.as_ptr(), observed);
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut QueueState<N>) -> R) -> R {
        while self
            .gate
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }
        let _guard = GateGuard { gate: &self.gate };
        // `gate` serializes every access to the state and remains held through `f`.
        f(unsafe { &mut *self.state.get() })
    }
}

// Every state access is serialized by `gate`; work executes after the state borrow and gate guard
// are gone. Function/context validity never depends on the queue core's internal memory.
// — KESTREL 2026-07-26
unsafe impl<const N: usize> Send for QueueCore<N> {}
unsafe impl<const N: usize> Sync for QueueCore<N> {}

/// A bounded in-process queue with exactly one active worker.
///
/// Submission is multi-producer safe. Work executes outside the queue's metadata gate.
pub struct SerialQueue<const N: usize> {
    core: QueueCore<N>,
    worker_claimed: AtomicBool,
}

impl<const N: usize> SerialQueue<N> {
    pub const fn new() -> Self {
        Self {
            core: QueueCore::new(),
            worker_claimed: AtomicBool::new(false),
        }
    }

    pub const fn capacity(&self) -> usize {
        N
    }

    pub fn len(&self) -> usize {
        self.core.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Claim the queue's sole worker. Dropping the token permits a replacement worker.
    pub fn worker(&self) -> Result<SerialWorker<'_, N>, WorkerError> {
        self.worker_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| WorkerError::AlreadyClaimed)?;
        Ok(SerialWorker {
            queue: self,
            _not_sync: core::marker::PhantomData,
        })
    }

    /// Enqueue work and return immediately.
    pub fn submit_async(&self, function: WorkFn, context: usize) -> Result<(), SubmitError> {
        self.core.push(Work {
            function,
            context,
            completion: core::ptr::null(),
        })
    }

    /// Enqueue work and park until the serial worker completes it.
    ///
    /// Calling this from the queue's own worker would wait on itself; the caller must keep
    /// synchronous submission outside work executed by this same serial queue.
    pub fn submit_sync(&self, function: WorkFn, context: usize) -> Result<usize, SubmitError> {
        let completion = Completion::new();
        self.core.push(Work {
            function,
            context,
            completion: &completion,
        })?;
        Ok(completion.wait())
    }
}

impl<const N: usize> Default for SerialQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Exclusive drain authority for a [`SerialQueue`].
pub struct SerialWorker<'a, const N: usize> {
    queue: &'a SerialQueue<N>,
    // A token may move to another thread, but sharing one token could let two contexts drain the
    // serial queue concurrently. `Cell` keeps the token `!Sync`.
    _not_sync: core::marker::PhantomData<Cell<()>>,
}

impl<const N: usize> SerialWorker<'_, N> {
    /// Execute one queued item, or report that the queue is empty.
    pub fn try_run_one(&self) -> Option<usize> {
        self.queue.core.try_run_one()
    }

    /// Park until one item is available, then execute it.
    pub fn run_one(&self) -> usize {
        self.queue.core.run_one()
    }

    /// Execute exactly `count` items, parking whenever the queue becomes empty.
    pub fn run_n(&self, count: usize) {
        for _ in 0..count {
            self.run_one();
        }
    }
}

impl<const N: usize> Drop for SerialWorker<'_, N> {
    fn drop(&mut self) {
        self.queue.worker_claimed.store(false, Ordering::Release);
    }
}

/// A bounded in-process queue that permits multiple active workers.
///
/// Submission is multi-producer safe. Workers atomically claim distinct FIFO items, then execute
/// them outside the metadata gate; completion order is therefore intentionally unspecified.
pub struct ConcurrentQueue<const N: usize> {
    core: QueueCore<N>,
}

impl<const N: usize> ConcurrentQueue<N> {
    pub const fn new() -> Self {
        Self {
            core: QueueCore::new(),
        }
    }

    pub const fn capacity(&self) -> usize {
        N
    }

    pub fn len(&self) -> usize {
        self.core.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Create one worker token. Any number of tokens may drain this concurrent queue.
    pub fn worker(&self) -> ConcurrentWorker<'_, N> {
        ConcurrentWorker {
            queue: self,
            _not_sync: core::marker::PhantomData,
        }
    }

    /// Enqueue work and return immediately.
    pub fn submit_async(&self, function: WorkFn, context: usize) -> Result<(), SubmitError> {
        self.core.push(Work {
            function,
            context,
            completion: core::ptr::null(),
        })
    }

    /// Enqueue work and park until one concurrent worker completes it.
    ///
    /// A caller that is itself the last available worker would wait on its own queue; synchronous
    /// submission requires another worker execution context to remain available.
    pub fn submit_sync(&self, function: WorkFn, context: usize) -> Result<usize, SubmitError> {
        let completion = Completion::new();
        self.core.push(Work {
            function,
            context,
            completion: &completion,
        })?;
        Ok(completion.wait())
    }
}

impl<const N: usize> Default for ConcurrentQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// One drain context for a [`ConcurrentQueue`].
pub struct ConcurrentWorker<'a, const N: usize> {
    queue: &'a ConcurrentQueue<N>,
    // Multiple tokens are allowed, but one token still represents exactly one execution context.
    _not_sync: core::marker::PhantomData<Cell<()>>,
}

impl<const N: usize> ConcurrentWorker<'_, N> {
    /// Execute one queued item, or report that the queue is empty.
    pub fn try_run_one(&self) -> Option<usize> {
        self.queue.core.try_run_one()
    }

    /// Park until one item is available, then execute it.
    pub fn run_one(&self) -> usize {
        self.queue.core.run_one()
    }
}

fn execute(work: Work) -> usize {
    let result = (work.function)(work.context);
    if let Some(completion) = unsafe { work.completion.as_ref() } {
        completion.finish(result);
    }
    result
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn double(value: usize) -> usize {
        value * 2
    }

    #[test]
    fn once_runs_one_initializer_for_competing_callers() {
        struct Probe {
            calls: AtomicUsize,
            initializer_entered: AtomicBool,
            release_initializer: AtomicBool,
        }

        fn controlled_initializer(context: usize) -> usize {
            let probe = unsafe { &*(context as *const Probe) };
            probe.calls.fetch_add(1, Ordering::AcqRel);
            probe.initializer_entered.store(true, Ordering::Release);
            while !probe.release_initializer.load(Ordering::Acquire) {
                thread::yield_now();
            }
            42
        }

        let once = Arc::new(Once::new());
        let probe = Probe {
            calls: AtomicUsize::new(0),
            initializer_entered: AtomicBool::new(false),
            release_initializer: AtomicBool::new(false),
        };
        let context = &probe as *const Probe as usize;

        let first_once = Arc::clone(&once);
        let first = thread::spawn(move || first_once.call_once(controlled_initializer, context));
        while !probe.initializer_entered.load(Ordering::Acquire) {
            thread::yield_now();
        }
        let second_once = Arc::clone(&once);
        let second = thread::spawn(move || second_once.call_once(double, 999));
        thread::yield_now();
        probe.release_initializer.store(true, Ordering::Release);

        assert_eq!(first.join().unwrap(), 42);
        assert_eq!(second.join().unwrap(), 42);
        assert_eq!(probe.calls.load(Ordering::Acquire), 1);
        assert!(once.is_completed());
    }

    #[test]
    fn once_returns_the_first_result_to_later_callers() {
        let once = Once::new();
        assert_eq!(once.call_once(double, 21), 42);
        assert_eq!(once.call_once(double, 99), 42);
    }

    #[test]
    fn async_work_runs_fifo_on_one_worker() {
        let queue = SerialQueue::<4>::new();
        let shared_log = AtomicUsize::new(0);
        struct SharedRecord {
            log: *const AtomicUsize,
            digit: usize,
        }
        fn record_shared(context: usize) -> usize {
            let task = unsafe { &*(context as *const SharedRecord) };
            let log = unsafe { &*task.log };
            log.fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                Some(old * 10 + task.digit)
            })
            .expect("record update");
            task.digit
        }
        let shared = [
            SharedRecord {
                log: &shared_log,
                digit: 1,
            },
            SharedRecord {
                log: &shared_log,
                digit: 2,
            },
            SharedRecord {
                log: &shared_log,
                digit: 3,
            },
        ];
        for task in &shared {
            queue
                .submit_async(record_shared, task as *const SharedRecord as usize)
                .unwrap();
        }
        let worker = queue.worker().unwrap();
        worker.run_n(3);
        assert_eq!(shared_log.load(Ordering::Acquire), 123);
        assert!(queue.is_empty());
    }

    #[test]
    fn full_queue_rejects_without_reordering() {
        let queue = SerialQueue::<2>::new();
        queue.submit_async(double, 1).unwrap();
        queue.submit_async(double, 2).unwrap();
        assert_eq!(queue.submit_async(double, 3), Err(SubmitError::Full));
        let worker = queue.worker().unwrap();
        assert_eq!(worker.try_run_one(), Some(2));
        assert_eq!(worker.try_run_one(), Some(4));
        assert_eq!(worker.try_run_one(), None);
    }

    #[test]
    fn only_one_serial_worker_can_be_claimed() {
        let queue = SerialQueue::<1>::new();
        let worker = queue.worker().unwrap();
        assert!(matches!(queue.worker(), Err(WorkerError::AlreadyClaimed)));
        drop(worker);
        assert!(queue.worker().is_ok());
    }

    #[test]
    fn concurrent_workers_execute_jobs_together() {
        fn rendezvous(context: usize) -> usize {
            let barrier = unsafe { &*(context as *const Barrier) };
            barrier.wait();
            1
        }

        let queue = Arc::new(ConcurrentQueue::<2>::new());
        let barrier = Barrier::new(2);
        let context = &barrier as *const Barrier as usize;
        queue.submit_async(rendezvous, context).unwrap();
        queue.submit_async(rendezvous, context).unwrap();

        let first_queue = Arc::clone(&queue);
        let first = thread::spawn(move || first_queue.worker().run_one());
        let second_queue = Arc::clone(&queue);
        let second = thread::spawn(move || second_queue.worker().run_one());

        assert_eq!(first.join().unwrap(), 1);
        assert_eq!(second.join().unwrap(), 1);
        assert!(queue.is_empty());
    }

    #[test]
    fn concurrent_queue_preserves_bounded_backpressure() {
        let queue = ConcurrentQueue::<1>::new();
        queue.submit_async(double, 5).unwrap();
        assert_eq!(queue.submit_async(double, 6), Err(SubmitError::Full));
        assert_eq!(queue.submit_sync(double, 6), Err(SubmitError::Full));
        assert_eq!(queue.worker().try_run_one(), Some(10));
        assert!(queue.is_empty());
    }

    #[test]
    fn concurrent_sync_returns_a_peer_workers_result() {
        let queue = Arc::new(ConcurrentQueue::<1>::new());
        let worker_queue = Arc::clone(&queue);
        let worker = thread::spawn(move || worker_queue.worker().run_one());
        assert_eq!(queue.submit_sync(double, 21), Ok(42));
        assert_eq!(worker.join().unwrap(), 42);
    }

    #[test]
    fn synchronous_submit_returns_the_worker_result() {
        let queue = Arc::new(SerialQueue::<1>::new());
        let worker_queue = Arc::clone(&queue);
        let worker = thread::spawn(move || {
            let worker = worker_queue.worker().unwrap();
            worker.run_one()
        });
        assert_eq!(queue.submit_sync(double, 21), Ok(42));
        assert_eq!(worker.join().unwrap(), 42);
    }

    #[test]
    fn zero_capacity_queue_is_total_and_always_full() {
        let queue = SerialQueue::<0>::new();
        assert_eq!(queue.capacity(), 0);
        assert_eq!(queue.submit_async(double, 1), Err(SubmitError::Full));
        assert!(queue.is_empty());
    }
}
