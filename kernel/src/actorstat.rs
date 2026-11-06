//j822
//! AX0 observation, enabled only by an explicit Sora diagnostic request.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use kumo_abi::actor::Snapshot;
static ENABLED: AtomicBool = AtomicBool::new(false);
static COUNT: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static MIN: AtomicU64 = AtomicU64::new(u64::MAX);
static MAX: AtomicU64 = AtomicU64::new(0);

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub fn record_root(ns: u64) {
    COUNT.fetch_add(1, Ordering::Relaxed);
    TOTAL.fetch_add(ns, Ordering::Relaxed);
    MIN.fetch_min(ns, Ordering::Relaxed);
    MAX.fetch_max(ns, Ordering::Relaxed);
}
pub fn snapshot(op: u8) -> Snapshot {
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
    let mut s = crate::user_thread::resident_snapshot();
    if op == 8 {
        for _ in 0..1024 {
            let a = kumo_hal::active::monotonic_nanos();
            s.root_ns += kumo_hal::active::monotonic_nanos().saturating_sub(a);
        }
        s.roots = 1024;
        return s;
    }
    if (3..=7).contains(&op) {
        let n = [1, 2, 4, 8, 12][(op - 3) as usize];
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
    if !(1..=12).contains(&n) || steps == 0 {
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
    let s = snapshot(0);
    let _=writeln!(out,"actorstat: capacity={} live={} stored={} blocked={} ready={} running={} free={} frames-issued={} heap={} roots={} root-ns={} min={} max={}",s.capacity,s.live,s.stored,s.blocked,s.ready,s.running,s.capacity.saturating_sub(s.live),s.frames_issued,s.heap_used,s.roots,s.root_ns,s.root_min_ns,s.root_max_ns);
}
#[cfg(test)]
mod tests {
    #[test]
    fn private_dispatcher_visits_every_runnable_without_changing_live_state() {
        for n in 1..=12 {
            assert_eq!(super::scheduler_sample(n, 1024, || 17), Some(0));
        }
        assert_eq!(super::scheduler_sample(0, 1, || 0), None);
        assert_eq!(super::scheduler_sample(13, 1, || 0), None);
        assert_eq!(super::scheduler_sample(1, 0, || 0), None);
    }
}
