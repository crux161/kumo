//j822
//j825
//! AX0 observation, enabled only by an explicit Sora diagnostic request.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kumo_abi::actor::{Snapshot, SnapshotWide};
static ENABLED: AtomicBool = AtomicBool::new(false);
static COUNT: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static MIN: AtomicU64 = AtomicU64::new(u64::MAX);
static MAX: AtomicU64 = AtomicU64::new(0);

/// AX4: the private-dispatcher sample sizes. The first five are AX0's original ops 3..=7;
/// the rest are AX4's ops 9..=13 and exercise the same code at the actor-capacity ceiling.
pub const SCHEDULER_SAMPLES: [u32; 10] = [1, 2, 4, 8, 12, 16, 32, 64, 128, 256];

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub fn record_root(ns: u64) {
    COUNT.fetch_add(1, Ordering::Relaxed);
    TOTAL.fetch_add(ns, Ordering::Relaxed);
    MIN.fetch_min(ns, Ordering::Relaxed);
    MAX.fetch_max(ns, Ordering::Relaxed);
}

/// AX0's frozen query: an AXR1 frame or `None` when the true census does not fit the legacy
/// 12-slot view. The caller must turn `None` into a clean status reply, never a frame.
pub fn snapshot(op: u8) -> Option<Snapshot> {
    legacy(sample(op))
}

/// AX4's query: the capacity-bounded AXR2 view, valid for every op `0..=13`.
pub fn snapshot_wide(op: u8) -> SnapshotWide {
    sample(op)
}

fn legacy(w: SnapshotWide) -> Option<Snapshot> {
    if w.live as usize > kumo_abi::actor::RESIDENT_LIMIT
        || w.stored as usize > kumo_abi::actor::RESIDENT_LIMIT
    {
        return None;
    }
    let mut s = Snapshot {
        enabled: w.enabled,
        capacity: kumo_abi::actor::RESIDENT_LIMIT as u32,
        live: w.live,
        stored: w.stored,
        blocked: w.blocked,
        ready: w.ready,
        running: w.running,
        frames_issued: w.frames_issued,
        heap_used: w.heap_used,
        roots: w.roots,
        root_ns: w.root_ns,
        root_min_ns: w.root_min_ns,
        root_max_ns: w.root_max_ns,
        syscalls: w.syscalls,
        switches: w.switches,
        ..Default::default()
    };
    s.processes[..w.live as usize].copy_from_slice(&w.processes[..w.live as usize]);
    Some(s)
}

/// One snapshot path for both wire views: the op's side effects and timing fields must be
/// identical whether an AX0 or an AX4 probe asked, or two probes would perturb each other.
fn sample(op: u8) -> SnapshotWide {
    if op == 1 {
        ENABLED.store(false, Ordering::Relaxed);
        COUNT.store(0, Ordering::Relaxed);
        TOTAL.store(0, Ordering::Relaxed);
        MIN.store(u64::MAX, Ordering::Relaxed);
        MAX.store(0, Ordering::Relaxed);
        ENABLED.store(true, Ordering::Relaxed);
    } else if op == 2 {
        ENABLED.store(false, Ordering::Relaxed);
    }
    let mut s = crate::user_thread::resident_snapshot_wide();
    if op == 8 {
        for _ in 0..1024 {
            let a = kumo_hal::active::monotonic_nanos();
            s.root_ns += kumo_hal::active::monotonic_nanos().saturating_sub(a);
        }
        s.roots = 1024;
        return s;
    }
    if (3..=7).contains(&op) {
        let n = SCHEDULER_SAMPLES[(op - 3) as usize];
        s.roots = 32768;
        s.root_ns =
            scheduler_sample(n, 32768, kumo_hal::active::monotonic_nanos).unwrap_or(u64::MAX);
        return s;
    }
    // AX4's new ops keep AX0's op numbers and line labels untouched. They are answered only
    // on the AXB2 path; the AXB1 front end still refuses op > 8.
    if (9..=13).contains(&op) {
        let n = SCHEDULER_SAMPLES[5 + (op - 9) as usize];
        s.roots = 32768;
        s.root_ns =
            scheduler_sample(n, 32768, kumo_hal::active::monotonic_nanos).unwrap_or(u64::MAX);
        return s;
    }
    s.syscalls = kumo_hal::active::syscall_count() as u64;
    s.switches = if crate::user_thread::is_started() {
        crate::user_thread::switch_count()
    } else {
        0
    };
    s.enabled = enabled();
    s.frames_issued = crate::mm::frames_issued();
    s.heap_used = crate::mm::heap::used() as u64;
    s.roots = COUNT.load(Ordering::Relaxed);
    s.root_ns = TOTAL.load(Ordering::Relaxed);
    s.root_min_ns = if s.roots == 0 {
        0
    } else {
        MIN.load(Ordering::Relaxed)
    };
    s.root_max_ns = MAX.load(Ordering::Relaxed);
    s
}

/// Actual Dispatcher round-robin selection over a private instance. No live thread is
/// admitted or run; report this CPU cost separately from an address-space handoff.
/// Zero allocations in the timed loop, and no clocks in each selection. — TIMBERDOODLE
pub fn scheduler_sample(n: u32, steps: u32, stamp: impl Fn() -> u64) -> Option<u64> {
    use crate::sched::{ClassId, Decision, Dispatcher, Priority};
    use kumo_abi::KoId;
    if !SCHEDULER_SAMPLES.contains(&n) || steps == 0 {
        return None;
    }
    let mut d = Dispatcher::new(1);
    d.set_running(KoId(1), Priority(63), ClassId::Rt);
    for i in 2..=n {
        d.admit(KoId(i as u64), Priority(63));
    }
    for _ in 0..n {
        let _ = d.yield_current();
    }
    let start = stamp();
    for i in 0..steps {
        let next = (i + 1) % n + 1;
        let decision = d.yield_current();
        let id = match decision {
            Decision::Continue(id) => id,
            Decision::Switch { to, .. } => to,
            _ => return None,
        };
        if id != KoId(next as u64) {
            return None;
        }
    }
    Some(stamp().checked_sub(start)?)
}

pub fn report(out: &mut dyn core::fmt::Write) {
    let s = snapshot_wide(0);
    let _=writeln!(out,"actorstat: capacity={} live={} stored={} blocked={} ready={} running={} free={} frames-issued={} heap={} roots={} root-ns={} min={} max={}",s.capacity,s.live,s.stored,s.blocked,s.ready,s.running,s.capacity.saturating_sub(s.live),s.frames_issued,s.heap_used,s.roots,s.root_ns,s.root_min_ns,s.root_max_ns);
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::{ClassId, Decision, Dispatcher, Priority};
    use kumo_abi::KoId;

    #[test]
    fn private_dispatcher_visits_every_runnable_without_changing_live_state() {
        for n in SCHEDULER_SAMPLES {
            assert_eq!(super::scheduler_sample(n, 1024, || 17), Some(0), "n={n}");
        }
        assert_eq!(super::scheduler_sample(0, 1, || 0), None);
        assert_eq!(super::scheduler_sample(3, 1, || 0), None);
        assert_eq!(super::scheduler_sample(13, 1, || 0), None);
        assert_eq!(super::scheduler_sample(257, 1, || 0), None);
        assert_eq!(super::scheduler_sample(1, 0, || 0), None);
    }

    #[test]
    fn two_hundred_and_fifty_six_equal_peers_still_yield_to_a_more_urgent_wake() {
        // AX4: the starvation clause. 256 runnables at the child priority rotate by FIFO, and
        // Sora's wake (priority 62 is more urgent than 63) must preempt the whole class at
        // once rather than waiting for the queue to drain.
        let mut d = Dispatcher::new(1);
        d.set_running(KoId(1), Priority(63), ClassId::Rt);
        for i in 2..=256 {
            d.admit(KoId(i as u64), Priority(63));
        }
        assert_eq!(d.runnable_count(), 255);
        // Sora is not in the RT class until her wake; admitting her at 62 must select her.
        match d.wake_rt(KoId(900), Priority(62)) {
            Decision::Switch { from, to } => {
                assert_eq!(from, Some(KoId(1)));
                assert_eq!(to, KoId(900));
            }
            other => panic!("more urgent wake did not preempt: {other:?}"),
        }
        assert_eq!(d.runnable_count(), 256);
    }

    #[test]
    fn legacy_view_fails_closed_above_twelve_and_wide_view_does_not() {
        let empty = SnapshotWide::default();
        let empty_legacy = legacy(empty).expect("an empty census fits");
        assert_eq!(empty_legacy.capacity, 12);
        assert_eq!(empty_legacy.live, 0);
        let mut wide = SnapshotWide {
            capacity: kumo_abi::actor::RESIDENT_MAX as u32,
            live: 13,
            stored: 13,
            blocked: 13,
            ..SnapshotWide::default()
        };
        for (i, slot) in wide.processes[..13].iter_mut().enumerate() {
            *slot = i as u64 + 1;
        }
        assert_eq!(legacy(wide.clone()), None);
        wide.live = 12;
        wide.stored = 12;
        wide.blocked = 12;
        let legacy12 = legacy(wide.clone()).expect("12 fit");
        assert_eq!(legacy12.capacity, 12);
        assert_eq!(legacy12.live, 12);
        assert_eq!(legacy12.processes[11], 12);
        assert_eq!(wide.processes[12], 13);
    }
}
