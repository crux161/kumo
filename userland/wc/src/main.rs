#![no_std]
#![no_main]

//! `wc` — line/word/byte counts of one initrd file, via explicit capabilities.
//!
//! Like `cat`, Sora grants this process a read-only initrd VMO in `x0` and a read-only argv
//! VMO in `x1`. `wc` locates the named entry, streams it through the host-tested `wc` core in
//! 256-byte chunks, and prints `<lines> <words> <bytes> <path>`. It has no ambient filesystem
//! authority. (Reading standard input — the `… | wc` form — waits for the shell stdio model,
//! DESIGN/013.)

use kumo_abi::{entry_table_bytes, find_entry, unpack_argv, Handle, INITRD_HEADER_LEN};
use kumo_rt::{debug_write, process_exit, vmo_read};
use wc::Counter;

kumo_rt::entry!(main);

fn fail(message: &[u8]) -> ! {
    debug_write(message.as_ptr(), message.len());
    process_exit(1)
}

/// Append one byte to `buf` at `pos`, advancing `pos`; silently drops on overflow.
fn push_byte(buf: &mut [u8], pos: &mut usize, byte: u8) {
    if *pos < buf.len() {
        buf[*pos] = byte;
        *pos += 1;
    }
}

/// Append `value` in decimal to `buf` at `pos`.
fn push_dec(buf: &mut [u8], pos: &mut usize, mut value: usize) {
    let mut digits = [0u8; 20];
    let mut len = 0;
    loop {
        digits[len] = b'0' + (value % 10) as u8;
        value /= 10;
        len += 1;
        if value == 0 {
            break;
        }
    }
    for i in 0..len {
        push_byte(buf, pos, digits[len - 1 - i]);
    }
}

#[no_mangle]
extern "C" fn main(
    initrd_handle: u64,
    argv_handle: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
    _a8: u64,
) -> ! {
    if initrd_handle == 0 || argv_handle == 0 {
        fail(b"wc: missing capability\n");
    }
    let initrd = Handle(initrd_handle as u32);

    let mut argv_buf = [0u8; 256];
    if vmo_read(
        Handle(argv_handle as u32),
        0,
        argv_buf.as_mut_ptr(),
        argv_buf.len(),
    ) != 0
    {
        fail(b"wc: argv read fail\n");
    }
    let mut argv = unpack_argv(&argv_buf);
    let _program = argv.next();
    let path_bytes = match (argv.next(), argv.next()) {
        (Some(path), None) => path,
        _ => fail(b"usage: wc <path>\n"),
    };
    let path = match core::str::from_utf8(path_bytes) {
        Ok(path) => path,
        Err(_) => fail(b"wc: bad path\n"),
    };

    // Read the initrd entry table (mirrors `cat`).
    let mut table = [0u8; 2048];
    if vmo_read(initrd, 0, table.as_mut_ptr(), INITRD_HEADER_LEN) != 0 {
        fail(b"wc: initrd read fail\n");
    }
    let table_len = match entry_table_bytes(&table[..INITRD_HEADER_LEN]) {
        Ok(len) if len <= table.len() => len,
        Ok(_) => fail(b"wc: table too large\n"),
        Err(_) => fail(b"wc: bad initrd\n"),
    };
    let mut filled = INITRD_HEADER_LEN;
    while filled < table_len {
        let chunk = (table_len - filled).min(256);
        if vmo_read(initrd, filled as u64, table[filled..].as_mut_ptr(), chunk) != 0 {
            fail(b"wc: initrd read fail\n");
        }
        filled += chunk;
    }

    let entry = match find_entry(&table[..table_len], path) {
        Ok(Some(entry)) => entry,
        Ok(None) => fail(b"wc: not found\n"),
        Err(_) => fail(b"wc: bad initrd\n"),
    };

    // Stream the file through the counting core, 256 bytes at a time.
    let mut counter = Counter::new();
    let mut offset = entry.offset;
    let mut remaining = entry.len;
    let mut buf = [0u8; 256];
    while remaining != 0 {
        let chunk = remaining.min(buf.len() as u64) as usize;
        if vmo_read(initrd, offset, buf.as_mut_ptr(), chunk) != 0 {
            fail(b"wc: file read fail\n");
        }
        counter.feed(&buf[..chunk]);
        offset += chunk as u64;
        remaining -= chunk as u64;
    }

    let counts = counter.counts();
    let mut line = [0u8; 160];
    let mut pos = 0;
    push_dec(&mut line, &mut pos, counts.lines);
    push_byte(&mut line, &mut pos, b' ');
    push_dec(&mut line, &mut pos, counts.words);
    push_byte(&mut line, &mut pos, b' ');
    push_dec(&mut line, &mut pos, counts.bytes);
    push_byte(&mut line, &mut pos, b' ');
    for &byte in path_bytes {
        push_byte(&mut line, &mut pos, byte);
    }
    push_byte(&mut line, &mut pos, b'\n');
    debug_write(line.as_ptr(), pos);

    process_exit(0)
}
