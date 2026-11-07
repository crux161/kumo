#![no_std]
#![no_main]
//j825
//! AX4 idle actor (PLAN/021): heap-free, exactly two transferred channels (`x0` control,
//! `x1` wake), one Port, one 120 s deadline. It announces `R`, parks in `PortWait`, and on
//! its **first** wake byte reads the clock and announces `W` with that timestamp. Later
//! wake bytes are absorbed without a second frame, so a broadcast can measure every child
//! that had not already been woken by the single-target phase. `Q` or peer close exits 0.
use kumo_abi::Handle;
use kumo_rt::{
    channel_read, channel_write, clock_get, handle_koid, port_bind, port_create, process_exit,
    timer_create,
};
kumo_rt::entry!(main);

fn checked(raw: u64) -> Handle {
    if raw == 0 || raw == u64::MAX {
        process_exit(1)
    }
    Handle(raw as u32)
}

/// Write one 32-byte AXP3 control frame:
/// `0..4` "AXP3", `4` role (`R` ready, `W` first wake), `5..8` zero, `8..16` clock-ns
/// (0 in a ready frame), `16..24` wake count, `24..32` SIMD canary errors so far.
fn report(control: Handle, role: u8, clock: u64, wakes: u64, errors: u64) {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(b"AXP3");
    b[4] = role;
    b[8..16].copy_from_slice(&clock.to_le_bytes());
    b[16..24].copy_from_slice(&wakes.to_le_bytes());
    b[24..32].copy_from_slice(&errors.to_le_bytes());
    if channel_write(control, b.as_ptr(), b.len()) != 0 {
        process_exit(1);
    }
}

/// Check all 128 bits of q31 across the blocking SVC and real context switch. The
/// canary and check live in one asm block, so Rust cannot reuse the register in between.
/// — TIMBERDOODLE
#[cfg(target_arch = "aarch64")]
fn wait(port: Handle) -> (u64, bool) {
    let source: u64;
    let bad: u64;
    unsafe {
        core::arch::asm!(
            "movi v31.16b, #0x5a", "svc #0", "movi v30.16b, #0x5a",
            "eor v30.16b, v30.16b, v31.16b", "umov x9, v30.d[0]", "umov x10, v30.d[1]", "orr x11, x9, x10",
            inlateout("x0") u64::from(port.0)=>source, in("x8") kumo_abi::Syscall::PortWait as u64,
            lateout("x9") _,lateout("x10") _,lateout("x11") bad,lateout("v30") _,lateout("v31") _,options(nostack)
        );
    }
    (source, bad == 0)
}
#[cfg(not(target_arch = "aarch64"))]
fn wait(_port: Handle) -> (u64, bool) {
    process_exit(2)
}

#[no_mangle]
extern "C" fn main(control: u64, wake: u64) -> ! {
    let control = checked(control);
    let wake = checked(wake);
    let port = checked(port_create());
    // The boundary fill admits children serially and the kernel may run them mid-fill, so
    // a short deadline could self-exit fill children before the census. 120 s only ever
    // fires when the harness itself has already failed; the collect's 15 s timer bounds a
    // wedged probe.
    let timer = checked(timer_create(120_000_000_000));
    for h in [control, wake, timer] {
        if port_bind(port, h) != 0 {
            process_exit(1);
        }
    }
    let ck = handle_koid(control);
    let wk = handle_koid(wake);
    let tk = handle_koid(timer);
    let mut errors = 0u64;
    let mut woken = false;
    report(control, b'R', 0, 0, 0);
    loop {
        let (source, simd) = wait(port);
        if !simd {
            errors += 1;
        }
        if source == tk {
            process_exit(1);
        }
        if source == ck {
            let mut cmd = [0u8; 1];
            let n = channel_read(control, cmd.as_mut_ptr(), 1);
            if n == 0 || (n == 1 && cmd[0] == b'Q') {
                process_exit(0);
            }
            process_exit(1);
        } else if source == wk {
            let mut b = [0u8; 1];
            let n = channel_read(wake, b.as_mut_ptr(), 1);
            if n == 0 {
                process_exit(0);
            }
            if n != 1 {
                process_exit(1);
            }
            if !woken {
                woken = true;
                // The clock read is the first userspace instruction after the blocking wait.
                report(control, b'W', clock_get(), 1, errors);
            }
        } else {
            process_exit(1);
        }
    }
}
