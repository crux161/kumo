#![no_std]
#![no_main]

//! `ls` — the first KUMO program that *uses a capability*.
//!
//! Where `hello` only writes and exits, `ls` receives one bootstrap channel in `x0`.
//! Its startup message carries a **read-only initrd VMO** and stdout channel as explicit
//! transferred capabilities. The initrd grant is least privilege: `ls` can read the
//! image but not write it. It reads the initrd header + entry table and writes each path
//! to stdout, so a `bin/<name>` entry is visibly runnable via `run <name>`, with its
//! payload size (an `ls -l`-style listing). The parsing is `kumo_abi::entries`, shared
//! with the host tests.
//!
//! `VmoRead` is capped at 256 bytes/call (kernel `usermode.rs`), so the table is read
//! in 256-byte chunks — the same reason the loaders chunk. `ls` reads the header first,
//! computes the exact entry-table length, then reads only that many table bytes. If the
//! table outgrows this first-stage stack buffer, it fails loudly instead of truncating
//! the listing.

use kumo_abi::{entries, entry_table_bytes, Handle, INITRD_HEADER_LEN};
use kumo_rt::{channel_write, debug_write, process_exit, startup, vmo_read};

kumo_rt::entry!(main);

/// Right-align `value`'s decimal digits into `field`, space-padded on the left. `field`
/// must already be space-filled; an oversized value keeps its low digits (the field is
/// wide enough for any real initrd entry size).
fn fmt_dec_right(value: u64, field: &mut [u8]) {
    let mut v = value;
    let mut i = field.len();
    loop {
        i -= 1;
        field[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 || i == 0 {
            break;
        }
    }
}

/// Write ordinary program output through the capability supplied by the shell. Keep
/// `debug_write` only as a diagnostic fallback for direct/legacy launches that provide
/// no stdout handle.
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
    let initrd = match startup.cap0 {
        Some(handle) => handle,
        None => {
            const ERR: &[u8] = b"ls: no initrd handle\n";
            debug_write(ERR.as_ptr(), ERR.len());
            process_exit(1);
        }
    };
    let stdout = startup.stdout.unwrap_or(Handle(0));

    let mut table = [0u8; 2048];

    if vmo_read(initrd, 0, table.as_mut_ptr(), INITRD_HEADER_LEN) != 0 {
        const ERR: &[u8] = b"ls: read fail\n";
        debug_write(ERR.as_ptr(), ERR.len());
        process_exit(1);
    }

    let table_len = match entry_table_bytes(&table[..INITRD_HEADER_LEN]) {
        Ok(len) if len <= table.len() => len,
        Ok(_) => {
            const ERR: &[u8] = b"ls: table too large\n";
            debug_write(ERR.as_ptr(), ERR.len());
            process_exit(1);
        }
        Err(_) => {
            const ERR: &[u8] = b"ls: bad initrd\n";
            debug_write(ERR.as_ptr(), ERR.len());
            process_exit(1);
        }
    };

    let mut filled = INITRD_HEADER_LEN;
    while filled < table_len {
        let chunk = (table_len - filled).min(256);
        if vmo_read(initrd, filled as u64, table[filled..].as_mut_ptr(), chunk) != 0 {
            const ERR: &[u8] = b"ls: read fail\n";
            debug_write(ERR.as_ptr(), ERR.len());
            process_exit(1);
        }
        filled += chunk;
    }

    // `<size right-aligned in 10>  <path>` per entry — an ls -l-style listing.
    for (path, size) in entries(&table[..table_len]) {
        let mut sizebuf = [b' '; 10];
        fmt_dec_right(size, &mut sizebuf);
        emit(stdout, &sizebuf);
        emit(stdout, b"  ");
        emit(stdout, path.as_bytes());
        emit(stdout, b"\n");
    }
    process_exit(0)
}
