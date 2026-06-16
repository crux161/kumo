//! The scheduler — modular, strict-priority-first ("The Magician", `DESIGN/003`).
//!
//! This is the policy substrate that replaces the earlier flat round-robin
//! `task::Scheduler`. The core owns the *mechanism* (run state, the context switch,
//! the per-CPU dispatcher); scheduling *policy* lives behind the **Scheduler
//! Framework Interface** (`SchedClass`) so disciplines can be swapped at build time
//! the way Linux swaps CFS/EEVDF/BORE behind `sched_class`.
//!
//! sched-1 ships two classes:
//!   * [`PriorityClass`] — **Discipline A**, strict O(1) priority (256 levels, a
//!     two-level bitmap, FIFO within a level). The system-plane default.
//!   * [`IdleClass`]     — the always-runnable floor.
//!
//! Dispatch is by **strict class precedence**: the [`Dispatcher`] runs the highest
//! non-empty class. Within `PriorityClass`, a strictly more urgent ready thread
//! preempts immediately; equal-priority threads round-robin on the quantum; a less
//! urgent thread never preempts a running one.
//!
//! Donation (sched-2) and SMP `rebalance` (later) are SFI seams kept as no-ops here.
//! This module is arch-neutral and fully host-tested. The freestanding arm64 path
//! drives it from the timer IRQ, while cooperative demos and future syscalls enter
//! through the same block / wake / yield / finish scheduling points.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use kumo_abi::KoId;

/// Threads are identified to the scheduler by their kernel-object id.
pub type ThreadId = KoId;

/// Strict-priority levels. 0 is the most urgent, 255 the least.
pub const PRIORITY_LEVELS: usize = 256;

/// A scheduling priority. **Lower is more urgent** (0 = highest).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Priority(pub u8);

impl Priority {
    /// The most urgent band — reserved for the kernel's own critical service threads.
    pub const HIGHEST: Priority = Priority(0);
    /// The least urgent band.
    pub const LOWEST: Priority = Priority(255);
    /// A neutral default for ordinary threads.
    pub const DEFAULT: Priority = Priority(128);
}

impl Default for Priority {
    fn default() -> Self {
        Priority::DEFAULT
    }
}

/// Which scheduling class a thread belongs to. Capability-gated at thread creation
/// (only trusted callers may place a thread in `Rt`). sched-1 ships `Rt` + `Idle`;
/// `Deadline` and `Fair` (Discipline B) arrive later.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClassId {
    /// Discipline A — strict O(1) priority. The system-plane default.
    Rt,
    /// The always-runnable floor.
    Idle,
}

/// The Scheduler Framework Interface: one implementation per scheduling class. The
/// dispatcher drives classes only through this contract, never their internals.
pub trait SchedClass {
    /// Make `t` runnable in this class at `prio` (appended to its band).
    fn enqueue(&mut self, t: ThreadId, prio: Priority);
    /// Remove `t` (e.g. it blocked or terminated). Returns whether it was present.
    fn dequeue(&mut self, t: ThreadId, prio: Priority) -> bool;
    /// Take the most-eligible runnable thread out of this class, or `None` if empty.
    fn pick_next(&mut self) -> Option<ThreadId>;
    /// The priority of the most-eligible runnable thread, without removing it.
    fn peek_top(&self) -> Option<Priority>;
    /// Whether the class has any runnable thread.
    fn is_empty(&self) -> bool;

    // ---- SFI seams, filled in by later slices -------------------------------
    /// IPC scheduling-context donation (sched-2). No-op until donation lands.
    fn on_donation(&mut self, _from: ThreadId, _to: ThreadId, _prio: Priority) {}
    /// SMP load rebalancing (later). No-op while uniprocessor.
    fn rebalance(&mut self, _cpu: u32) {}
}

/// **Discipline A** — strict O(1) priority over 256 levels.
///
/// A two-level bitmap makes "find the highest-priority runnable thread" O(1): a
/// 4-bit `summary` says which of the four 64-bit `words` is non-empty, and each word
/// bit marks a non-empty priority level. Selection is one `trailing_zeros` on the
/// summary then one on the chosen word — which lowers to `RBIT;CLZ` on aarch64 and
/// `TZCNT`/`BSF` on x86_64, so the same source is O(1) on both backends.
pub struct PriorityClass {
    /// Bit `w` set ⇒ `words[w]` has at least one set bit. Only the low 4 bits are used.
    summary: u64,
    /// 256 bits: bit `w*64 + b` set ⇒ level `w*64 + b` has a ready thread.
    words: [u64; 4],
    /// One FIFO per priority level.
    queues: Vec<VecDeque<ThreadId>>,
    len: usize,
}

impl PriorityClass {
    pub fn new() -> Self {
        let mut queues = Vec::with_capacity(PRIORITY_LEVELS);
        for _ in 0..PRIORITY_LEVELS {
            queues.push(VecDeque::new());
        }
        Self {
            summary: 0,
            words: [0; 4],
            queues,
            len: 0,
        }
    }

    /// Number of ready threads across all levels.
    pub fn len(&self) -> usize {
        self.len
    }

    fn set_bit(&mut self, level: usize) {
        let (w, b) = (level >> 6, level & 63);
        self.words[w] |= 1u64 << b;
        self.summary |= 1u64 << w;
    }

    fn clear_bit(&mut self, level: usize) {
        let (w, b) = (level >> 6, level & 63);
        self.words[w] &= !(1u64 << b);
        if self.words[w] == 0 {
            self.summary &= !(1u64 << w);
        }
    }

    /// The lowest-numbered (most urgent) non-empty level, via the two-level bitmap.
    fn top_level(&self) -> Option<usize> {
        if self.summary == 0 {
            return None;
        }
        let w = self.summary.trailing_zeros() as usize;
        let b = self.words[w].trailing_zeros() as usize;
        Some((w << 6) | b)
    }
}

impl Default for PriorityClass {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedClass for PriorityClass {
    fn enqueue(&mut self, t: ThreadId, prio: Priority) {
        let level = prio.0 as usize;
        self.queues[level].push_back(t);
        self.set_bit(level);
        self.len += 1;
    }

    fn dequeue(&mut self, t: ThreadId, prio: Priority) -> bool {
        let level = prio.0 as usize;
        let q = &mut self.queues[level];
        if let Some(pos) = q.iter().position(|&queued| queued == t) {
            q.remove(pos);
            if q.is_empty() {
                self.clear_bit(level);
            }
            self.len -= 1;
            true
        } else {
            false
        }
    }

    fn pick_next(&mut self) -> Option<ThreadId> {
        let level = self.top_level()?;
        let t = self.queues[level].pop_front();
        if self.queues[level].is_empty() {
            self.clear_bit(level);
        }
        if t.is_some() {
            self.len -= 1;
        }
        t
    }

    fn peek_top(&self) -> Option<Priority> {
        self.top_level().map(|level| Priority(level as u8))
    }

    fn is_empty(&self) -> bool {
        self.summary == 0
    }
}

/// The always-runnable floor: a single idle thread that is *never consumed* by
/// `pick_next` (it stays available so the CPU always has something to run, even when
/// every real thread has blocked).
pub struct IdleClass {
    idle: Option<ThreadId>,
}

impl IdleClass {
    pub const fn new() -> Self {
        Self { idle: None }
    }

    pub fn set(&mut self, t: ThreadId) {
        self.idle = Some(t);
    }
}

impl Default for IdleClass {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedClass for IdleClass {
    fn enqueue(&mut self, t: ThreadId, _prio: Priority) {
        self.idle = Some(t);
    }

    fn dequeue(&mut self, t: ThreadId, _prio: Priority) -> bool {
        if self.idle == Some(t) {
            self.idle = None;
            true
        } else {
            false
        }
    }

    fn pick_next(&mut self) -> Option<ThreadId> {
        // The idle thread is a floor, not a queue entry: hand it out without removing.
        self.idle
    }

    fn peek_top(&self) -> Option<Priority> {
        self.idle.map(|_| Priority::LOWEST)
    }

    fn is_empty(&self) -> bool {
        self.idle.is_none()
    }
}

/// What the dispatcher decided on a scheduling point (timer tick / wake / yield).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    /// Nothing runnable — not even an idle thread.
    Idle,
    /// Keep running the current thread.
    Continue(ThreadId),
    /// Switch from `from` to `to` (the caller performs the context switch).
    Switch {
        from: Option<ThreadId>,
        to: ThreadId,
    },
}

#[derive(Clone, Copy)]
struct Running {
    id: ThreadId,
    prio: Priority,
    class: ClassId,
}

/// The per-CPU dispatcher: owns the scheduling classes in precedence order and turns
/// scheduling points into [`Decision`]s. One instance per core (SMP wraps this).
pub struct Dispatcher {
    rt: PriorityClass,
    idle: IdleClass,
    current: Option<Running>,
    /// Round-robin quantum, in timer ticks, for equal-priority threads.
    quantum: u32,
    ticks: u32,
}

impl Dispatcher {
    pub fn new(quantum_ticks: u32) -> Self {
        Self {
            rt: PriorityClass::new(),
            idle: IdleClass::new(),
            current: None,
            quantum: quantum_ticks.max(1),
            ticks: 0,
        }
    }

    /// Register the idle thread (the floor). It is never time-sliced or consumed.
    pub fn set_idle(&mut self, t: ThreadId) {
        self.idle.set(t);
    }

    /// Make a thread runnable in the RT (Discipline A) class.
    pub fn admit(&mut self, t: ThreadId, prio: Priority) {
        self.rt.enqueue(t, prio);
    }

    /// Remove a ready RT thread from its run queue. Used when a waiter is cancelled,
    /// killed, or consumed by a bounded demo before it is picked again.
    pub fn remove_ready_rt(&mut self, t: ThreadId, prio: Priority) -> bool {
        self.rt.dequeue(t, prio)
    }

    /// Wake a blocked/new thread into the RT class and immediately reevaluate the
    /// local CPU. A more-urgent wake preempts now; an equal/less-urgent wake waits
    /// for a timer tick or explicit yield.
    pub fn wake_rt(&mut self, t: ThreadId, prio: Priority) -> Decision {
        self.rt.enqueue(t, prio);
        self.reschedule_current()
    }

    /// Mark a thread as the one currently on-CPU (e.g. the bootstrap thread), without
    /// going through a pick. Resets its quantum.
    pub fn set_running(&mut self, t: ThreadId, prio: Priority, class: ClassId) {
        self.current = Some(Running { id: t, prio, class });
        self.ticks = 0;
    }

    pub fn current(&self) -> Option<ThreadId> {
        self.current.map(|r| r.id)
    }

    /// Ready threads in the RT class (excludes the running thread and the idle floor).
    pub fn runnable_count(&self) -> usize {
        self.rt.len()
    }

    /// A scheduling point: account a tick and decide who runs next.
    pub fn on_timer_tick(&mut self) -> Decision {
        self.ticks = self.ticks.saturating_add(1);

        let Some(cur) = self.current else {
            return self.dispatch_fresh();
        };

        // 1. Strict-priority preemption: a ready RT thread strictly more urgent than
        //    the current one (or *any* RT thread, if we're only running idle) wins now.
        if let Some(top) = self.rt.peek_top() {
            if cur.class == ClassId::Idle || top < cur.prio {
                return self.preempt_to_rt(cur);
            }
        }

        // 2. Round-robin among equals: only when the quantum is spent and another
        //    thread waits at the *same* priority. A less urgent thread never preempts.
        if cur.class == ClassId::Rt && self.ticks >= self.quantum {
            self.ticks = 0;
            if self.rt.peek_top() == Some(cur.prio) {
                return self.rotate_same_level(cur);
            }
        }

        Decision::Continue(cur.id)
    }

    /// Reevaluate the current CPU without charging a timer tick. This is the
    /// non-interrupt path used after wakeups and explicit reschedule requests.
    pub fn reschedule_current(&mut self) -> Decision {
        let Some(cur) = self.current else {
            return self.dispatch_fresh();
        };

        if let Some(top) = self.rt.peek_top() {
            if cur.class == ClassId::Idle || top < cur.prio {
                return self.preempt_to_rt(cur);
            }
        }

        Decision::Continue(cur.id)
    }

    /// Voluntarily give up the CPU. RT threads go to the tail of their priority FIFO
    /// and the dispatcher selects the next eligible thread. Idle only yields if real
    /// work is ready.
    pub fn yield_current(&mut self) -> Decision {
        let Some(cur) = self.current else {
            return self.dispatch_fresh();
        };

        match cur.class {
            ClassId::Rt => {
                self.rt.enqueue(cur.id, cur.prio);
                self.dispatch_after_removed(Some(cur.id))
            }
            ClassId::Idle => self.reschedule_current(),
        }
    }

    /// The current thread blocked. It is no longer runnable and must not be
    /// reinserted; choose a replacement, falling back to idle.
    pub fn block_current(&mut self) -> Decision {
        let from = self.current.take().map(|cur| cur.id);
        self.ticks = 0;
        self.dispatch_after_removed(from)
    }

    /// The current thread exited or otherwise left the scheduler forever.
    pub fn finish_current(&mut self) -> Decision {
        self.block_current()
    }

    fn dispatch_fresh(&mut self) -> Decision {
        self.dispatch_after_removed(None)
    }

    fn dispatch_after_removed(&mut self, from: Option<ThreadId>) -> Decision {
        if let Some(prio) = self.rt.peek_top() {
            // Invariant: peek_top() == Some implies pick_next() == Some. If that ever
            // breaks, fall through to the idle floor rather than panic — the scheduler
            // is the §5.6 fixed point and must never bring the kernel down (DESIGN/003).
            if let Some(to) = self.rt.pick_next() {
                self.current = Some(Running {
                    id: to,
                    prio,
                    class: ClassId::Rt,
                });
                self.ticks = 0;
                if from == Some(to) {
                    return Decision::Continue(to);
                }
                return Decision::Switch { from, to };
            }
            debug_assert!(false, "rt peek_top/pick_next disagree");
        }
        if let Some(idle) = self.idle.pick_next() {
            self.current = Some(Running {
                id: idle,
                prio: Priority::LOWEST,
                class: ClassId::Idle,
            });
            self.ticks = 0;
            if from == Some(idle) {
                return Decision::Continue(idle);
            }
            return Decision::Switch { from, to: idle };
        }
        Decision::Idle
    }

    fn preempt_to_rt(&mut self, cur: Running) -> Decision {
        // The displaced thread goes back on its run queue (unless it was the idle floor).
        if cur.class == ClassId::Rt {
            self.rt.enqueue(cur.id, cur.prio);
        }
        // A preempt only fires when an RT thread is ready, so rt is non-empty here.
        // If that invariant is ever violated, keep the current thread running rather
        // than panic — the scheduler must never fault (DESIGN/003).
        let prio = self.rt.peek_top();
        let to = self.rt.pick_next();
        let (Some(prio), Some(to)) = (prio, to) else {
            debug_assert!(false, "preempt_to_rt with empty rt");
            self.current = Some(cur);
            return Decision::Continue(cur.id);
        };
        self.current = Some(Running {
            id: to,
            prio,
            class: ClassId::Rt,
        });
        self.ticks = 0;
        Decision::Switch {
            from: Some(cur.id),
            to,
        }
    }

    fn rotate_same_level(&mut self, cur: Running) -> Decision {
        self.rt.enqueue(cur.id, cur.prio);
        // We just enqueued cur.id, so pick_next() is guaranteed Some; guard anyway so a
        // future invariant break degrades to "keep running" instead of a kernel panic.
        let Some(to) = self.rt.pick_next() else {
            debug_assert!(false, "rotate_same_level: rt empty after enqueue");
            self.current = Some(cur);
            return Decision::Continue(cur.id);
        };
        self.current = Some(Running {
            id: to,
            prio: cur.prio,
            class: ClassId::Rt,
        });
        if to == cur.id {
            Decision::Continue(cur.id)
        } else {
            Decision::Switch {
                from: Some(cur.id),
                to,
            }
        }
    }
}

/// A boot-path self-test of the scheduler substrate, in the spirit of `ipc::smoke`.
/// Deterministic and arch-neutral, so it verifies the same way on the QEMU serial
/// console and on the X13s framebuffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmokeReport {
    pub levels: usize,
    pub picks: usize,
    pub ordered: bool,
    pub idle_floor: bool,
    pub preemptions: usize,
}

pub fn smoke() -> SmokeReport {
    // (1) Strict-priority ordering with FIFO ties. Admit out of order across bands.
    let mut class = PriorityClass::new();
    class.enqueue(KoId(1), Priority(200));
    class.enqueue(KoId(2), Priority(3));
    class.enqueue(KoId(3), Priority(200));
    class.enqueue(KoId(4), Priority(7));
    class.enqueue(KoId(5), Priority(0));
    let expected = [KoId(5), KoId(2), KoId(4), KoId(1), KoId(3)];
    let mut ordered = true;
    let mut picks = 0;
    for want in expected {
        match class.pick_next() {
            Some(got) => {
                if got != want {
                    ordered = false;
                }
                picks += 1;
            }
            None => ordered = false,
        }
    }
    ordered &= class.pick_next().is_none();

    // (2) The idle floor: with no RT threads, the dispatcher still has something to run.
    let mut disp = Dispatcher::new(1);
    disp.set_idle(KoId(99));
    let idle_floor = matches!(
        disp.on_timer_tick(),
        Decision::Switch {
            from: None,
            to: KoId(99)
        }
    );

    // (3) Wake-higher-priority preemption: an urgent thread displaces a running one.
    let mut preemptions = 0;
    disp.admit(KoId(10), Priority(50));
    if let Decision::Switch { to, .. } = disp.on_timer_tick() {
        if to == KoId(10) {
            preemptions += 1; // idle -> RT
        }
    }
    disp.admit(KoId(11), Priority(5)); // more urgent than the running KoId(10)
    if let Decision::Switch {
        from: Some(_),
        to: KoId(11),
    } = disp.on_timer_tick()
    {
        preemptions += 1; // strict-priority preemption
    }

    SmokeReport {
        levels: PRIORITY_LEVELS,
        picks,
        ordered,
        idle_floor,
        preemptions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_selects_strict_priority_order() {
        let mut c = PriorityClass::new();
        c.enqueue(KoId(1), Priority(255));
        c.enqueue(KoId(2), Priority(0));
        c.enqueue(KoId(3), Priority(128));
        assert_eq!(c.peek_top(), Some(Priority(0)));
        assert_eq!(c.pick_next(), Some(KoId(2)));
        assert_eq!(c.pick_next(), Some(KoId(3)));
        assert_eq!(c.pick_next(), Some(KoId(1)));
        assert_eq!(c.pick_next(), None);
        assert!(c.is_empty());
    }

    #[test]
    fn fifo_within_a_priority_level() {
        let mut c = PriorityClass::new();
        c.enqueue(KoId(1), Priority(64));
        c.enqueue(KoId(2), Priority(64));
        c.enqueue(KoId(3), Priority(64));
        assert_eq!(c.pick_next(), Some(KoId(1)));
        assert_eq!(c.pick_next(), Some(KoId(2)));
        assert_eq!(c.pick_next(), Some(KoId(3)));
    }

    #[test]
    fn bitmap_spans_all_four_words() {
        // One thread in each 64-level word: 0, 64, 128, 192. Must come out in order.
        let mut c = PriorityClass::new();
        c.enqueue(KoId(192), Priority(192));
        c.enqueue(KoId(0), Priority(0));
        c.enqueue(KoId(128), Priority(128));
        c.enqueue(KoId(64), Priority(64));
        assert_eq!(c.pick_next(), Some(KoId(0)));
        assert_eq!(c.pick_next(), Some(KoId(64)));
        assert_eq!(c.pick_next(), Some(KoId(128)));
        assert_eq!(c.pick_next(), Some(KoId(192)));
    }

    #[test]
    fn dequeue_removes_and_clears_bits() {
        let mut c = PriorityClass::new();
        c.enqueue(KoId(1), Priority(10));
        c.enqueue(KoId(2), Priority(10));
        assert!(c.dequeue(KoId(1), Priority(10)));
        assert_eq!(c.len(), 1);
        assert!(!c.dequeue(KoId(1), Priority(10))); // already gone
        assert_eq!(c.pick_next(), Some(KoId(2)));
        assert!(c.is_empty());
    }

    #[test]
    fn idle_floor_is_never_consumed() {
        let mut idle = IdleClass::new();
        assert!(idle.is_empty());
        idle.set(KoId(7));
        assert_eq!(idle.pick_next(), Some(KoId(7)));
        assert_eq!(idle.pick_next(), Some(KoId(7))); // still there
        assert!(!idle.is_empty());
    }

    #[test]
    fn dispatcher_runs_idle_when_no_rt_threads() {
        let mut d = Dispatcher::new(2);
        d.set_idle(KoId(99));
        assert_eq!(
            d.on_timer_tick(),
            Decision::Switch {
                from: None,
                to: KoId(99)
            }
        );
        // Idle keeps running while nothing else is ready.
        assert_eq!(d.on_timer_tick(), Decision::Continue(KoId(99)));
    }

    #[test]
    fn rt_thread_preempts_idle_immediately() {
        let mut d = Dispatcher::new(4);
        d.set_idle(KoId(99));
        d.on_timer_tick(); // -> idle
        d.admit(KoId(1), Priority::DEFAULT);
        assert_eq!(
            d.on_timer_tick(),
            Decision::Switch {
                from: Some(KoId(99)),
                to: KoId(1)
            }
        );
    }

    #[test]
    fn more_urgent_thread_preempts_running_one() {
        let mut d = Dispatcher::new(10);
        d.set_running(KoId(1), Priority(100), ClassId::Rt);
        d.admit(KoId(2), Priority(20)); // more urgent
        assert_eq!(
            d.on_timer_tick(),
            Decision::Switch {
                from: Some(KoId(1)),
                to: KoId(2)
            }
        );
        // The displaced thread was re-queued and resumes once the urgent one is gone.
        assert_eq!(d.current(), Some(KoId(2)));
    }

    #[test]
    fn less_urgent_thread_does_not_preempt() {
        let mut d = Dispatcher::new(10);
        d.set_running(KoId(1), Priority(20), ClassId::Rt);
        d.admit(KoId(2), Priority(100)); // less urgent
        assert_eq!(d.on_timer_tick(), Decision::Continue(KoId(1)));
    }

    #[test]
    fn equal_priority_threads_round_robin_on_quantum() {
        let mut d = Dispatcher::new(2);
        d.set_running(KoId(1), Priority(50), ClassId::Rt);
        d.admit(KoId(2), Priority(50));
        // Quantum is 2: first tick keeps running, second rotates.
        assert_eq!(d.on_timer_tick(), Decision::Continue(KoId(1)));
        assert_eq!(
            d.on_timer_tick(),
            Decision::Switch {
                from: Some(KoId(1)),
                to: KoId(2)
            }
        );
    }

    #[test]
    fn wake_more_urgent_thread_preempts_without_waiting_for_tick() {
        let mut d = Dispatcher::new(10);
        d.set_running(KoId(1), Priority(80), ClassId::Rt);
        assert_eq!(
            d.wake_rt(KoId(2), Priority(10)),
            Decision::Switch {
                from: Some(KoId(1)),
                to: KoId(2)
            }
        );
        assert_eq!(d.current(), Some(KoId(2)));
    }

    #[test]
    fn equal_priority_wake_waits_for_tick_or_yield() {
        let mut d = Dispatcher::new(10);
        d.set_running(KoId(1), Priority(50), ClassId::Rt);
        assert_eq!(
            d.wake_rt(KoId(2), Priority(50)),
            Decision::Continue(KoId(1))
        );
        assert_eq!(
            d.yield_current(),
            Decision::Switch {
                from: Some(KoId(1)),
                to: KoId(2)
            }
        );
    }

    #[test]
    fn block_current_selects_next_or_idle_floor() {
        let mut d = Dispatcher::new(1);
        d.set_idle(KoId(99));
        d.set_running(KoId(1), Priority::DEFAULT, ClassId::Rt);
        d.admit(KoId(2), Priority::DEFAULT);
        assert_eq!(
            d.block_current(),
            Decision::Switch {
                from: Some(KoId(1)),
                to: KoId(2)
            }
        );
        assert_eq!(
            d.finish_current(),
            Decision::Switch {
                from: Some(KoId(2)),
                to: KoId(99)
            }
        );
    }

    #[test]
    fn removing_ready_thread_keeps_it_from_being_picked() {
        let mut d = Dispatcher::new(1);
        d.set_idle(KoId(99));
        d.set_running(KoId(1), Priority::DEFAULT, ClassId::Rt);
        d.admit(KoId(2), Priority::DEFAULT);
        assert!(d.remove_ready_rt(KoId(2), Priority::DEFAULT));
        assert_eq!(
            d.finish_current(),
            Decision::Switch {
                from: Some(KoId(1)),
                to: KoId(99)
            }
        );
    }

    #[test]
    fn smoke_reports_all_checks_green() {
        let r = smoke();
        assert_eq!(r.levels, 256);
        assert_eq!(r.picks, 5);
        assert!(r.ordered);
        assert!(r.idle_floor);
        assert_eq!(r.preemptions, 2);
    }
}
