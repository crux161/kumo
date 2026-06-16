#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::format;
use kumo_rt::ipc::{channel_read, channel_write};
use kumo_rt::sys::{Handle, Rights};
use kumo_abi::error::Error;
use piccolo::{Lua, Executor, Value};

/// The entry point spawned by Sora.
/// Sora grants this process exactly two capabilities:
/// - `stdin`: A Channel handle to receive keystrokes (e.g., from drv-ps2 or early console).
/// - `stdout`: A Channel handle to write output (to the early framebuffer console).
#[no_mangle]
pub extern "C" fn _start(stdin: Handle, stdout: Handle) -> ! {
    // 1. Initialize the kumo-rt environment (sets up the global allocator backed by VMOs)
    kumo_rt::init();

    let greeting = b"KUMO Lua REPL (Piccolo) initialized.\n> ";
    let _ = channel_write(stdout, greeting, &[]);

    // 2. Initialize the heavy-heap Lua state
    // If our VMAR/VMO dynamic allocation is broken, it will panic here.
    let mut lua = Lua::core();

    let mut input_buffer = String::new();
    let mut msg_buf = [0u8; 512];

    loop {
        // 3. Block and wait for input over the IPC channel
        match channel_read(stdin, &mut msg_buf) {
            Ok(msg) => {
                if msg.bytes.is_empty() { continue; }
                
                // Parse the incoming bytes as UTF-8
                if let Ok(chunk) = core::str::from_utf8(msg.bytes) {
                    input_buffer.push_str(chunk);
                    
                    // Echo the character back to stdout
                    let _ = channel_write(stdout, chunk.as_bytes(), &[]);

                    // If the user hit Enter, evaluate the buffer
                    if chunk.contains('\n') {
                        evaluate_and_print(&mut lua, &input_buffer, stdout);
                        input_buffer.clear();
                        let _ = channel_write(stdout, b"> ", &[]);
                    }
                }
            }
            Err(Error::PeerClosed) => {
                // The input driver crashed or Sora tore down the routing.
                // Exit cleanly so Sora can restart us.
                kumo_rt::sys::process_exit(0);
            }
            Err(_) => {
                let _ = channel_write(stdout, b"\n[IPC Read Error]\n> ", &[]);
            }
        }
    }
}

/// Evaluates a chunk of Lua code and writes the result to the stdout channel.
fn evaluate_and_print(lua: &mut Lua, code: &str, stdout: Handle) {
    let result = lua.try_run(|ctx| {
        // Compile and execute the input string
        let executor = ctx.stash(Executor::start(ctx, code.as_bytes(), "repl"));
        ctx.run_thread(&executor)
    });

    match result {
        Ok(Value::Nil) => {
            // Do nothing for empty statements
        }
        Ok(val) => {
            // We use alloc::format! to convert the Piccolo Value to a string,
            // further stressing the kumo-rt heap.
            let out = format!("{}\n", val);
            let _ = channel_write(stdout, out.as_bytes(), &[]);
        }
        Err(err) => {
            let err_out = format!("Error: {:?}\n", err);
            let _ = channel_write(stdout, err_out.as_bytes(), &[]);
        }
    }
}
