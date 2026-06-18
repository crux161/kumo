#![no_std]
#![no_main]

//! `ls` — the first KUMO program that *uses a capability*.
//!
//! Where `hello` only writes and exits, `ls` is handed a **read-only initrd VMO
//! handle in `x0`** by the shell's `ls` builtin (which narrows Sora's initrd to
//! `Rights::READ` before granting it — least privilege: `ls` can read the image but
//! not write it). It reads the initrd header + entry table and prints each path, so a
//! `bin/<name>` entry is visibly runnable via `run <name>`. The parsing is
//! `kumo_abi::entry_paths`, shared with the host tests.
//!
//! `VmoRead` is capped at 256 bytes/call (kernel `usermode.rs`), so the table is read
//! in 256-byte chunks — the same reason the loaders chunk. `ls` reads the header first,
//! computes the exact entry-table length, then reads only that many table bytes. If the
//! table outgrows this first-stage stack buffer, it fails loudly instead of truncating
//! the listing.

use kumo_abi::{entry_paths, entry_table_bytes, Handle, INITRD_HEADER_LEN};
use kumo_rt::{debug_write, process_exit, vmo_read};

kumo_rt::entry!(main);

#[no_mangle]
extern "C" fn main(
    initrd_handle: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    let initrd = Handle(initrd_handle as u32);

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

    for path in entry_paths(&table[..table_len]) {
        debug_write(path.as_ptr(), path.len());
        debug_write(b"\n".as_ptr(), 1);
    }
    process_exit(0)
}
