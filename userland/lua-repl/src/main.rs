#![no_std]
#![no_main]

use kumo_abi::Handle;

/// Placeholder Lua REPL — Piccolo (pure-Rust Lua 5.4) is deferred until
/// vendored for offline builds. See DEFERRED/003. This binary prints a
/// status message to the console and exits cleanly so the boot log is
/// honest rather than showing "lua-repl: missing."
#[no_mangle]
pub extern "C" fn _start(_stdin: Handle, stdout: Handle) -> ! {
    kumo_rt::init();

    let msg = b"KUMO Lua REPL: not available (piccolo not vendored)\n";
    let _ = kumo_rt::sys::debug_write(msg.as_ptr(), msg.len());

    // Write to the console channel too so the message is visible on
    // framebuffer consoles that don't receive the debug log.
    let _ = kumo_rt::sys::channel_write(stdout, msg.as_ptr(), msg.len());

    kumo_rt::sys::process_exit(0);
}
