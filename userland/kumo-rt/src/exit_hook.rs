//j894
//! A process-local last action for device fail-safes, before panic logging or ProcessExit.
use core::sync::atomic::{AtomicUsize, Ordering};
static HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register an optional, idempotent hook. It must not allocate, block, panic or call exit.
/// Other processes have independent statics and cannot replace this process's hook. — KESTREL
pub fn set_exit_hook(hook: Option<fn()>) {
    HOOK.store(hook.map_or(0, |f| f as usize), Ordering::Release);
}

pub(crate) fn run() {
    let pointer = HOOK.load(Ordering::Acquire);
    if pointer != 0 {
        // SAFETY: the only writer stores a function pointer of exactly this signature.
        let hook: fn() = unsafe { core::mem::transmute(pointer) };
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    fn called() {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }
    #[test]
    fn registered_hook_runs_and_can_be_cleared() {
        set_exit_hook(Some(called));
        run();
        run();
        set_exit_hook(None);
        run();
        assert_eq!(CALLS.load(Ordering::Relaxed), 2);
    }
}
