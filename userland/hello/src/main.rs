#![no_std]
#![no_main]

//! `hello` — the smallest native KUMO userland program.
//!
//! Sora transfers one bootstrap channel in `x0`; `hello` drains its finite startup message for a
//! stdout channel, writes one line to it (falling back to `DebugWrite` if none was granted), and
//! exits cleanly through `ProcessExit`. This is the template a program author copies — the minimal
//! shape of a program that produces output through the startup message. The panic handler and
//! global allocator come from `kumo-rt`.

use kumo_abi::Handle;
use kumo_rt::{channel_write, debug_write, process_exit, startup};

kumo_rt::entry!(main);

#[no_mangle]
extern "C" fn main(
    bootstrap_handle: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    let startup = startup(Handle(bootstrap_handle as u32));
    const MSG: &[u8] = b"hello from a native KUMO program!\n";
    match startup.stdout {
        Some(stdout) => {
            let _ = channel_write(stdout, MSG.as_ptr(), MSG.len());
        }
        None => {
            debug_write(MSG.as_ptr(), MSG.len());
        }
    }
    process_exit(0)
}
