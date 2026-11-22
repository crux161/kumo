//J935_r01_001
#![feature(restricted_std)]

use std::panic::{catch_unwind, panic_any, resume_unwind};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

include!("../std-rt/glue.rs");

fn record(line: &str) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, line.as_bytes());
}

fn check(ok: bool) {
    if !ok {
        record("SH9-a native unwind FAILED\n");
        std::process::exit(1);
    }
}

struct Guard<'a>(&'a AtomicUsize, usize);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0
            .fetch_update(SeqCst, SeqCst, |n| Some(n * 10 + self.1))
            .unwrap();
    }
}

#[inline(never)]
fn inner(order: &AtomicUsize) {
    let _guard = Guard(order, 3);
    panic_any(String::from("SH9 payload"));
}

#[inline(never)]
fn middle(order: &AtomicUsize) {
    let _guard = Guard(order, 2);
    inner(order);
}

#[inline(never)]
fn unwind_once() {
    let order = AtomicUsize::new(0);
    let result = catch_unwind(|| {
        let _guard = Guard(&order, 1);
        middle(&order);
    });
    check(order.load(SeqCst) == 321);
    check(
        result
            .err()
            .and_then(|p| p.downcast::<String>().ok())
            .as_deref()
            == Some(&String::from("SH9 payload")),
    );
}

unsafe extern "C" {
    fn _Unwind_Backtrace(callback: extern "C" fn(*mut (), *mut ()) -> i32, arg: *mut ()) -> i32;
    fn _Unwind_GetIP(context: *mut ()) -> usize;
}

extern "C" fn trace(context: *mut (), arg: *mut ()) -> i32 {
    unsafe {
        if _Unwind_GetIP(context) != 0 {
            *arg.cast::<usize>() += 1;
        }
    }
    0
}

#[inline(never)]
fn backtrace() -> usize {
    let mut frames = 0usize;
    unsafe {
        _Unwind_Backtrace(trace, (&mut frames as *mut usize).cast());
    }
    std::hint::black_box(frames)
}

fn main() {
    record("SH9-a native unwind begin\n");
    // Expected panics stay quiet; every acceptance/failure line uses the kernel
    // ring so serial and graphical boots observe the same proof. — KESTREL
    std::panic::set_hook(Box::new(|_| {}));
    for _ in 0..64 {
        unwind_once();
    }
    record("SH9-a: payload and LIFO cleanup x64 -> OK\n");

    let order = AtomicUsize::new(0);
    let result = catch_unwind(|| {
        let _outer = Guard(&order, 2);
        let payload = catch_unwind(|| {
            let _inner = Guard(&order, 1);
            panic_any(91usize);
        })
        .unwrap_err();
        resume_unwind(payload);
    });
    check(order.load(SeqCst) == 12);
    check(result.unwrap_err().downcast_ref::<usize>() == Some(&91));
    record("SH9-a: resume_unwind cleanup -> OK\n");

    let workers: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..16 {
                    unwind_once();
                }
            })
        })
        .collect();
    for worker in workers {
        check(worker.join().is_ok());
    }
    let failed = std::thread::spawn(|| panic_any(73usize)).join();
    check(failed.unwrap_err().downcast_ref::<usize>() == Some(&73));
    check(matches!(std::thread::spawn(|| 42).join(), Ok(42)));
    record("SH9-a: concurrent catches and panicking worker join -> OK\n");

    check(backtrace() >= 3);
    record("SH9-a: real backtrace frames -> OK\n");
    record("SH9-a native unwind green\n");
}
