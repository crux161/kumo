//j485
//j486
//j487
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use kumo_abi::Handle;
use kumo_rt::{channel_write, debug_write, process_exit, startup};

kumo_rt::entry!(main);

fn emit(stdout: Handle, bytes: &[u8]) {
    if stdout.0 == 0 {
        debug_write(bytes.as_ptr(), bytes.len());
    } else if channel_write(stdout, bytes.as_ptr(), bytes.len()) != 0 {
        const ERR: &[u8] = b"lua-repl: stdout write failed\n";
        debug_write(ERR.as_ptr(), ERR.len());
    }
}

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
    kumo_rt::init();
    let startup = startup(Handle(bootstrap_handle as u32));
    let stdout = startup.stdout.unwrap_or(Handle(0));

    match lua_repl::evaluate_fixed_expression() {
        Ok(value) => {
            let line = format!("lua-repl: {value}\n");
            emit(stdout, line.as_bytes());
            process_exit(0);
        }
        Err(_) => {
            const ERR: &[u8] = b"lua-repl: fixed evaluator failed\n";
            emit(stdout, ERR);
            process_exit(1);
        }
    }
}
