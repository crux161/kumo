//j485
//j486
#![no_std]
#![no_main]

use kumo_abi::Handle;

/// Placeholder Lua REPL while the now-freestanding Piccolo VM is connected to KUMO's
/// allocator-backed evaluator and channel-backed host functions.
#[no_mangle]
pub extern "C" fn _start(_stdin: Handle, stdout: Handle) -> ! {
    kumo_rt::init();

    let msg = b"KUMO Lua REPL: Piccolo ready (evaluator wiring pending)\n";
    let _ = kumo_rt::sys::debug_write(msg.as_ptr(), msg.len());

    // Write to the console channel too so the message is visible on
    // framebuffer consoles that don't receive the debug log.
    let _ = kumo_rt::sys::channel_write(stdout, msg.as_ptr(), msg.len());

    kumo_rt::sys::process_exit(0);
}
