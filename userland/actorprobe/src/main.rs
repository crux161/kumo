#![no_std]
#![no_main]
//j822
//! AX0 stateless, heap-free actor. Only two explicitly transferred channels, one Port,
//! one deadline; one outstanding 64-byte message. No device or root authority.
use kumo_abi::{
    actor::{fill_ping, PAYLOAD, ROUNDS},
    Handle, Syscall,
};
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
fn report(control: Handle, tag: u8, rounds: u32, ns: u64, errors: u64) {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(b"AXP1");
    b[4] = tag;
    b[8..12].copy_from_slice(&rounds.to_le_bytes());
    b[16..24].copy_from_slice(&ns.to_le_bytes());
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
            inlateout("x0") u64::from(port.0)=>source, in("x8") Syscall::PortWait as u64,
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
extern "C" fn main(control: u64, peer: u64) -> ! {
    let control = checked(control);
    let peer = checked(peer);
    let port = checked(port_create());
    let timer = checked(timer_create(10_000_000_000));
    for h in [control, peer, timer] {
        if port_bind(port, h) != 0 {
            process_exit(1);
        }
    }
    let ck = handle_koid(control);
    let pk = handle_koid(peer);
    let tk = handle_koid(timer);
    let mut initiating = false;
    let mut sequence = 1;
    let mut received = 0;
    let mut errors = 0;
    let mut began = 0;
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
            let mut cmd = [0; 1];
            let n = channel_read(control, cmd.as_mut_ptr(), 1);
            if n == 0 || (n == 1 && cmd[0] == b'Q') {
                process_exit(0);
            }
            if n != 1 || cmd[0] != b'G' || initiating || received != 0 {
                process_exit(1);
            }
            initiating = true;
            began = clock_get();
            let b = fill_ping(1);
            if channel_write(peer, b.as_ptr(), b.len()) != 0 {
                process_exit(1);
            }
        } else if source == pk {
            let mut b = [0; PAYLOAD];
            let n = channel_read(peer, b.as_mut_ptr(), b.len());
            if n != PAYLOAD as u64 || b != fill_ping(sequence) {
                process_exit(1);
            }
            received += 1;
            if !initiating && channel_write(peer, b.as_ptr(), b.len()) != 0 {
                process_exit(1);
            }
            if received == ROUNDS {
                report(
                    control,
                    b'D',
                    received,
                    if initiating {
                        clock_get().saturating_sub(began)
                    } else {
                        0
                    },
                    errors,
                );
            } else {
                sequence += 1;
                if initiating {
                    let b = fill_ping(sequence);
                    if channel_write(peer, b.as_ptr(), b.len()) != 0 {
                        process_exit(1);
                    }
                }
            }
        } else {
            process_exit(1);
        }
    }
}
