//j485
//j486
//j487
//j488
//j489
#![no_std]
#![no_main]

use kumo_abi::Handle;
use kumo_rt::{channel_read, channel_write, debug_write, process_exit, startup};

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
    let stdin = startup.stdin.unwrap_or(Handle(0));
    let stdout = startup.stdout.unwrap_or(Handle(0));

    if stdin.0 == 0 {
        emit(stdout, b"lua-repl: no stdin\n");
        process_exit(1);
    }

    let mut input = [0u8; 256];
    let read = channel_read(stdin, input.as_mut_ptr(), input.len());
    if read == 0 || read == u64::MAX {
        emit(stdout, b"lua-repl: input unavailable\n");
        process_exit(1);
    }
    let mut len = read as usize;
    while len != 0 && matches!(input[len - 1], b'\r' | b'\n') {
        len -= 1;
    }
    if len == 0 {
        emit(stdout, b"lua-repl: empty input\n");
        process_exit(1);
    }

    match lua_repl::evaluate_line_with_print(&input[..len], move |line| emit(stdout, line)) {
        Ok(evaluation) => {
            emit(stdout, b"lua-repl: ");
            if evaluation.output.is_empty() {
                emit(stdout, b"ok");
            } else {
                emit(stdout, &evaluation.output);
            }
            if evaluation.truncated {
                emit(stdout, b"...");
            }
            emit(stdout, b"\n");
            process_exit(0);
        }
        Err(lua_repl::EvalError::InstructionLimit) => {
            emit(stdout, b"lua-repl: instruction limit\n");
            process_exit(1);
        }
        Err(lua_repl::EvalError::Vm(_)) => {
            emit(stdout, b"lua-repl: evaluation error\n");
            process_exit(1);
        }
    }
}
