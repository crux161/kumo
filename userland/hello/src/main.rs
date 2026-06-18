#![no_std]
#![no_main]

//! `hello` — the smallest native KUMO userland program.
//!
//! It writes one line through the `DebugWrite` syscall (which reaches the console
//! drv-fb paints) and exits cleanly through `ProcessExit`. This is the template a
//! program author copies to write KUMO programs, and the binary the exec-vertical
//! uses to prove Sora's native spawn path end to end. The panic handler and global
//! allocator come from `kumo-rt`, so this file needs nothing but `entry!` + the two
//! syscalls.

use kumo_rt::{debug_write, process_exit};

kumo_rt::entry!(main);

#[no_mangle]
extern "C" fn main(
    _a1: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    const MSG: &[u8] = b"hello from a native KUMO program!\n";
    debug_write(MSG.as_ptr(), MSG.len());
    process_exit(0)
}
