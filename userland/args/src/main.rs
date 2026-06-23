#![no_std]
#![no_main]

//! `args` — the argv proof program.
//!
//! Sora transfers one bootstrap channel in `x0`. Its finite startup message grants stdout and a
//! read-only argv VMO; `args` reads the argv and prints each argument on its own line prefixed by
//! its index. This is the template for any program parameterised by its launch arguments. The
//! argv format is `kumo_abi::unpack_argv`, shared with the host tests; `argv[0]` is the program
//! name. It receives no other authority — a plain `run` is capability-free.

use kumo_abi::{unpack_argv, Handle};
use kumo_rt::{channel_write, debug_write, handle_close, process_exit, startup, vmo_read};

kumo_rt::entry!(main);

/// Emit ordinary output over stdout; fall back to `DebugWrite` when no stdout was granted.
fn emit(stdout: Handle, bytes: &[u8]) {
    if stdout.0 == 0 {
        debug_write(bytes.as_ptr(), bytes.len());
    } else {
        let _ = channel_write(stdout, bytes.as_ptr(), bytes.len());
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
    let startup = startup(Handle(bootstrap_handle as u32));
    let stdout = startup.stdout.unwrap_or(Handle(0));
    let Some(argv_handle) = startup.argv else {
        const NONE: &[u8] = b"args: (no argv)\n";
        emit(stdout, NONE);
        process_exit(0);
    };

    // The argv VMO is one fixed page; a single read (the 256-byte VmoRead cap) covers it.
    let mut buf = [0u8; 256];
    if vmo_read(argv_handle, 0, buf.as_mut_ptr(), buf.len()) != 0 {
        const ERR: &[u8] = b"args: read fail\n";
        debug_write(ERR.as_ptr(), ERR.len());
        process_exit(1);
    }

    for (index, arg) in unpack_argv(&buf).enumerate() {
        // "argv[<index>] = <arg>\n" — index is single-digit for any real command line.
        let mut head = *b"argv[0] = ";
        head[5] = b'0' + (index % 10) as u8;
        emit(stdout, &head);
        emit(stdout, arg);
        emit(stdout, b"\n");
    }

    // Prove HandleClose through the child-process SVC route: close argv, then the
    // same handle must no longer authorize a VmoRead.
    let mut probe = [0u8; 1];
    if handle_close(argv_handle) != 0
        || vmo_read(argv_handle, 0, probe.as_mut_ptr(), probe.len()) == 0
    {
        const ERR: &[u8] = b"args: close fail\n";
        debug_write(ERR.as_ptr(), ERR.len());
        process_exit(1);
    }
    const OK: &[u8] = b"args: close ok\n";
    emit(stdout, OK);
    process_exit(0)
}
