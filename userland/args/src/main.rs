#![no_std]
#![no_main]

//! `args` — the argv proof program.
//!
//! It receives a **read-only argv VMO** handle in `x1` (the second entry arg; `x0` is
//! the capability slot, unused here), reads it, and prints each argument on its own
//! line prefixed by its index. This is the first KUMO program parameterised by its
//! launch arguments — the template for real tools like `cat <path>`. The argv format is
//! `kumo_abi::unpack_argv`, shared with the host tests; `argv[0]` is the program name.

use kumo_abi::{unpack_argv, Handle};
use kumo_rt::{debug_write, handle_close, process_exit, vmo_read};

kumo_rt::entry!(main);

#[no_mangle]
extern "C" fn main(
    _cap: u64,
    argv_handle: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    if argv_handle == 0 {
        const NONE: &[u8] = b"args: (no argv)\n";
        debug_write(NONE.as_ptr(), NONE.len());
        process_exit(0);
    }

    // The argv VMO is one fixed page; a single read (the 256-byte VmoRead cap) covers it.
    let mut buf = [0u8; 256];
    if vmo_read(Handle(argv_handle as u32), 0, buf.as_mut_ptr(), buf.len()) != 0 {
        const ERR: &[u8] = b"args: read fail\n";
        debug_write(ERR.as_ptr(), ERR.len());
        process_exit(1);
    }

    for (index, arg) in unpack_argv(&buf).enumerate() {
        // "argv[<index>] = <arg>\n" — index is single-digit for any real command line.
        let mut head = *b"argv[0] = ";
        head[5] = b'0' + (index % 10) as u8;
        debug_write(head.as_ptr(), head.len());
        debug_write(arg.as_ptr(), arg.len());
        debug_write(b"\n".as_ptr(), 1);
    }

    // Prove HandleClose through the child-process SVC route: close argv, then the
    // same handle must no longer authorize a VmoRead.
    let argv = Handle(argv_handle as u32);
    let mut probe = [0u8; 1];
    if handle_close(argv) != 0 || vmo_read(argv, 0, probe.as_mut_ptr(), probe.len()) == 0 {
        const ERR: &[u8] = b"args: close fail\n";
        debug_write(ERR.as_ptr(), ERR.len());
        process_exit(1);
    }
    const OK: &[u8] = b"args: close ok\n";
    debug_write(OK.as_ptr(), OK.len());
    process_exit(0)
}
