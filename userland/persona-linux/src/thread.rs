//j916
//! SH7-d3c-2: the rules of the Linux thread calls the persona shim serves, kept pure so the
//! host can test them: which `clone` flag sets make a thread, how a `futex` op decodes, and
//! what a `timespec` is worth. The shim does the KUMO side (ThreadSpawn, the kernel futex).
//! — TEAL

/// Linux `clone` flags (include/uapi/linux/sched.h).
pub mod clone {
    pub const VM: u64 = 0x0000_0100;
    pub const FS: u64 = 0x0000_0200;
    pub const FILES: u64 = 0x0000_0400;
    pub const SIGHAND: u64 = 0x0000_0800;
    pub const THREAD: u64 = 0x0001_0000;
    pub const SYSVSEM: u64 = 0x0004_0000;
    pub const SETTLS: u64 = 0x0008_0000;
    pub const PARENT_SETTID: u64 = 0x0010_0000;
    pub const CHILD_CLEARTID: u64 = 0x0020_0000;
    pub const DETACHED: u64 = 0x0040_0000;
    pub const CHILD_SETTID: u64 = 0x0100_0000;

    /// What every thread shares with its creator: one address space, one cwd, one descriptor
    /// table, one set of signal handlers, one thread group.
    pub const THREAD_REQUIRED: u64 = VM | FS | FILES | SIGHAND | THREAD;
    /// Everything a thread `clone` may add (musl's `pthread_create` passes all but
    /// CHILD_SETTID; glibc passes CHILD_SETTID's neighbours). DETACHED is ignored by Linux.
    pub const THREAD_ALLOWED: u64 = THREAD_REQUIRED
        | SYSVSEM
        | SETTLS
        | PARENT_SETTID
        | CHILD_CLEARTID
        | DETACHED
        | CHILD_SETTID;
}

/// A `clone` the shim can serve: a sibling thread in this process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadClone {
    pub set_tls: bool,
    pub parent_set_tid: bool,
    pub child_set_tid: bool,
    pub child_clear_tid: bool,
}

/// Decide a `clone` flag word. Only a thread is served (the SH7-d3 cut: "musl's pthread flag
/// set only; anything else EINVAL"): every [`clone::THREAD_REQUIRED`] bit, nothing outside
/// [`clone::THREAD_ALLOWED`], and no exit signal (a thread reports to nobody).
pub fn thread_clone(flags: u64) -> Option<ThreadClone> {
    if flags & clone::THREAD_REQUIRED != clone::THREAD_REQUIRED
        || flags & !clone::THREAD_ALLOWED != 0
    {
        return None;
    }
    Some(ThreadClone {
        set_tls: flags & clone::SETTLS != 0,
        parent_set_tid: flags & clone::PARENT_SETTID != 0,
        child_set_tid: flags & clone::CHILD_SETTID != 0,
        child_clear_tid: flags & clone::CHILD_CLEARTID != 0,
    })
}

/// Linux `futex` ops (include/uapi/linux/futex.h).
pub mod futex {
    pub const WAIT: u64 = 0;
    pub const WAKE: u64 = 1;
    pub const REQUEUE: u64 = 3;
    pub const CMP_REQUEUE: u64 = 4;
    pub const WAIT_BITSET: u64 = 9;
    pub const WAKE_BITSET: u64 = 10;
    pub const PRIVATE_FLAG: u64 = 128;
    pub const CLOCK_REALTIME: u64 = 256;
    pub const CMD_MASK: u64 = !(PRIVATE_FLAG | CLOCK_REALTIME);
}

/// When a futex wait gives up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Deadline {
    /// No timeout pointer: wait until woken.
    Never,
    /// `FUTEX_WAIT`: the timespec is a duration.
    Relative,
    /// `FUTEX_WAIT_BITSET`: the timespec is a point on the clock.
    Absolute,
}

/// A decoded `futex` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FutexCall {
    /// Sleep while `*uaddr == expected`.
    Wait { expected: u32, deadline: Deadline },
    /// Wake up to `count` sleepers on `uaddr`.
    Wake { count: u32 },
    /// Wake up to `wake` on `uaddr` and move up to `requeue` to `uaddr2`. The kernel futex
    /// has no requeue, so the shim wakes both counts on `uaddr`: a woken waiter re-checks its
    /// condition and sleeps on the second word itself, which Linux's contract (wakeups may be
    /// spurious) already allows. `compare` is CMP_REQUEUE's `val3`.
    Requeue {
        wake: u32,
        requeue: u32,
        compare: Option<u32>,
    },
}

/// Why a `futex` call is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FutexRefusal {
    /// A malformed call Linux answers with EINVAL (a zero bitset, say).
    Invalid,
    /// An op this slice does not serve (ENOSYS, logged).
    Unsupported,
}

/// Decode `futex(uaddr, op, val, timeout_or_val2, uaddr2, val3)`. `timeout` is the fourth
/// argument as a raw word: a timespec pointer for the waits, a count for the requeues.
pub fn futex_call(op: u64, val: u64, timeout: u64, val3: u64) -> Result<FutexCall, FutexRefusal> {
    let cmd = op & futex::CMD_MASK;
    let bitset_ok = val3 as u32 != 0;
    match cmd {
        futex::WAIT | futex::WAIT_BITSET => {
            if cmd == futex::WAIT_BITSET && !bitset_ok {
                return Err(FutexRefusal::Invalid);
            }
            // FUTEX_CLOCK_REALTIME means something only for the absolute (bitset) wait.
            if cmd == futex::WAIT && op & futex::CLOCK_REALTIME != 0 {
                return Err(FutexRefusal::Unsupported);
            }
            let deadline = match (timeout, cmd) {
                (0, _) => Deadline::Never,
                (_, futex::WAIT) => Deadline::Relative,
                _ => Deadline::Absolute,
            };
            Ok(FutexCall::Wait {
                expected: val as u32,
                deadline,
            })
        }
        futex::WAKE | futex::WAKE_BITSET => {
            if cmd == futex::WAKE_BITSET && !bitset_ok {
                return Err(FutexRefusal::Invalid);
            }
            Ok(FutexCall::Wake { count: val as u32 })
        }
        futex::REQUEUE | futex::CMP_REQUEUE => {
            // Linux takes both counts as signed ints and refuses negatives.
            if (val as u32 as i32) < 0 || (timeout as u32 as i32) < 0 {
                return Err(FutexRefusal::Invalid);
            }
            Ok(FutexCall::Requeue {
                wake: val as u32,
                requeue: timeout as u32,
                compare: (cmd == futex::CMP_REQUEUE).then_some(val3 as u32),
            })
        }
        _ => Err(FutexRefusal::Unsupported),
    }
}

/// A `struct timespec` in nanoseconds, or `None` when Linux would answer EINVAL (a negative
/// second or a nanosecond field outside `0..1_000_000_000`). Saturates far in the future.
pub fn timespec_ns(sec: i64, nsec: i64) -> Option<u64> {
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return None;
    }
    Some(
        (sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(nsec as u64),
    )
}

/// A Linux thread id for a KUMO thread koid: Linux tids are positive ints, and futex-based
/// owners (robust and PI mutexes) keep only the low 30 bits (`FUTEX_TID_MASK`), so a koid that
/// does not fit there cannot be a tid.
pub fn tid_of_koid(koid: u64) -> Option<i32> {
    (koid != 0 && koid < 0x4000_0000).then_some(koid as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// musl 1.2.5 `pthread_create` passes exactly this set (src/thread/pthread_create.c).
    const MUSL_PTHREAD: u64 = clone::VM
        | clone::FS
        | clone::FILES
        | clone::SIGHAND
        | clone::THREAD
        | clone::SYSVSEM
        | clone::SETTLS
        | clone::PARENT_SETTID
        | clone::CHILD_CLEARTID
        | clone::DETACHED;

    #[test]
    fn musl_pthread_create_is_a_thread_and_fork_is_not() {
        assert_eq!(MUSL_PTHREAD, 0x7d_0f00);
        assert_eq!(
            thread_clone(MUSL_PTHREAD),
            Some(ThreadClone {
                set_tls: true,
                parent_set_tid: true,
                child_set_tid: false,
                child_clear_tid: true,
            })
        );
        // musl fork(): SIGCHLD only. posix_spawn: CLONE_VM | CLONE_VFORK | SIGCHLD.
        assert_eq!(thread_clone(17), None);
        assert_eq!(thread_clone(clone::VM | 0x4000 | 17), None);
        // A thread with an exit signal, or missing one shared resource, is refused.
        assert_eq!(thread_clone(MUSL_PTHREAD | 17), None);
        assert_eq!(thread_clone(MUSL_PTHREAD & !clone::FILES), None);
        // The bare minimum is a thread with nothing set or cleared.
        assert_eq!(
            thread_clone(clone::THREAD_REQUIRED),
            Some(ThreadClone {
                set_tls: false,
                parent_set_tid: false,
                child_set_tid: false,
                child_clear_tid: false,
            })
        );
    }

    #[test]
    fn futex_ops_decode_with_private_and_clock_flags() {
        let private = futex::PRIVATE_FLAG;
        assert_eq!(
            futex_call(futex::WAIT | private, 7, 0, 0),
            Ok(FutexCall::Wait {
                expected: 7,
                deadline: Deadline::Never
            })
        );
        assert_eq!(
            futex_call(futex::WAIT, 0xffff_ffff_0000_0002, 0x1000, 0),
            Ok(FutexCall::Wait {
                expected: 2,
                deadline: Deadline::Relative
            })
        );
        assert_eq!(
            futex_call(
                futex::WAIT_BITSET | private | futex::CLOCK_REALTIME,
                1,
                0x1000,
                u32::MAX as u64
            ),
            Ok(FutexCall::Wait {
                expected: 1,
                deadline: Deadline::Absolute
            })
        );
        assert_eq!(
            futex_call(futex::WAIT_BITSET, 1, 0, 0),
            Err(FutexRefusal::Invalid)
        );
        assert_eq!(
            futex_call(futex::WAIT | futex::CLOCK_REALTIME, 1, 0, 0),
            Err(FutexRefusal::Unsupported)
        );
        assert_eq!(
            futex_call(futex::WAKE | private, 1, 0, 0),
            Ok(FutexCall::Wake { count: 1 })
        );
        assert_eq!(
            futex_call(futex::WAKE_BITSET, i32::MAX as u64, 0, u32::MAX as u64),
            Ok(FutexCall::Wake {
                count: i32::MAX as u32
            })
        );
        assert_eq!(
            futex_call(futex::CMP_REQUEUE | private, 0, 1, 5),
            Ok(FutexCall::Requeue {
                wake: 0,
                requeue: 1,
                compare: Some(5)
            })
        );
        assert_eq!(
            futex_call(futex::REQUEUE, 1, (-1i32) as u32 as u64, 0),
            Err(FutexRefusal::Invalid)
        );
        // FUTEX_WAKE_OP, the PI family, FUTEX_FD: not served.
        for op in [5, 6, 7, 8, 11, 12, 13] {
            assert_eq!(futex_call(op, 0, 0, 0), Err(FutexRefusal::Unsupported));
        }
    }

    #[test]
    fn timespecs_and_tids() {
        assert_eq!(timespec_ns(0, 0), Some(0));
        assert_eq!(timespec_ns(2, 500), Some(2_000_000_500));
        assert_eq!(timespec_ns(0, 999_999_999), Some(999_999_999));
        assert_eq!(timespec_ns(0, 1_000_000_000), None);
        assert_eq!(timespec_ns(-1, 0), None);
        assert_eq!(timespec_ns(0, -1), None);
        assert_eq!(timespec_ns(i64::MAX, 0), Some(u64::MAX));
        assert_eq!(tid_of_koid(0), None);
        assert_eq!(tid_of_koid(0x2a5), Some(0x2a5));
        assert_eq!(tid_of_koid(0x3fff_ffff), Some(0x3fff_ffff));
        assert_eq!(tid_of_koid(0x4000_0000), None);
    }
}
