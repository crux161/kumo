#![no_std]
#![no_main]
//j547
//j548

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
//!
//! J547: when Sora grants a filesystem instead (`ls /media/<name>[/dir]`), `ls` lists the
//! directory its argument names on that filesystem, through `kumo-vfs`, in the same columns.
//! J548: names come back long, and those starting with `.` are hidden unless `-a` is given,
//! as `ls` does elsewhere.

use kumo_abi::{entries, entry_table_bytes, unpack_argv, Handle, INITRD_HEADER_LEN};
use kumo_rt::{channel_read, channel_write, debug_write, process_exit, startup, vmo_read};
use kumo_vfs::{parse_dir_reply, Request, VfsError, MAX_PATH, VFS_NOT_DIR, VFS_NOT_FOUND};
use ls::Options;

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

fn fail(message: &[u8]) -> ! {
    debug_write(message.as_ptr(), message.len());
    process_exit(1)
}

/// J547: list `options.path` on the filesystem behind `fs`, a `kumo-vfs` channel, page by
/// page until the server answers with no more entries. Files show their size and directories
/// `<dir>` and a trailing `/`. Dot-files are skipped unless `options.show_all` (J548).
fn list_volume(fs: Handle, stdout: Handle, options: Options<'_>) -> ! {
    let mut request = [0u8; 7 + MAX_PATH];
    // Room for a whole `fatfs` directory page (1024 bytes since J548's long names).
    let mut reply = [0u8; 1024];
    let mut start = 0u32;
    loop {
        let Some(len) = (Request::ReadDir {
            path: options.path,
            start,
        })
        .encode_into(&mut request) else {
            fail(b"ls: path too long\n");
        };
        if channel_write(fs, request.as_ptr(), len) != 0 {
            fail(b"ls: filesystem gone\n");
        }
        let got = channel_read(fs, reply.as_mut_ptr(), reply.len());
        if got == 0 || got == u64::MAX {
            fail(b"ls: filesystem gone\n");
        }
        let (count, records) = match parse_dir_reply(&reply[..got as usize]) {
            Ok(listing) => listing,
            Err(VfsError::Status(VFS_NOT_FOUND)) => fail(b"ls: no such directory\n"),
            Err(VfsError::Status(VFS_NOT_DIR)) => fail(b"ls: not a directory\n"),
            Err(_) => fail(b"ls: listing failed\n"),
        };
        if count == 0 {
            process_exit(0);
        }
        for record in records {
            if !options.includes(record.name) {
                continue;
            }
            let mut column = [b' '; 10];
            if record.is_dir {
                column[5..].copy_from_slice(b"<dir>");
            } else {
                fmt_dec_right(record.size, &mut column);
            }
            emit(stdout, &column);
            emit(stdout, b"  ");
            emit(stdout, record.name);
            emit(stdout, if record.is_dir { b"/\n" } else { b"\n" });
        }
        start += count as u32;
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
    let mut argv_buf = [0u8; 256];
    if let Some(argv) = startup.argv {
        if vmo_read(argv, 0, argv_buf.as_mut_ptr(), argv_buf.len()) != 0 {
            fail(b"ls: argv read fail\n");
        }
    }
    let Some(options) = Options::parse(unpack_argv(&argv_buf).skip(1)) else {
        fail(b"usage: ls [-a] [path]\n");
    };
    if let Some(fs) = startup.fs {
        list_volume(fs, startup.stdout.unwrap_or(Handle(0)), options);
    }
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
        if !options.includes(path.as_bytes()) {
            continue;
        }
        let mut sizebuf = [b' '; 10];
        fmt_dec_right(size, &mut sizebuf);
        emit(stdout, &sizebuf);
        emit(stdout, b"  ");
        emit(stdout, path.as_bytes());
        emit(stdout, b"\n");
    }
    process_exit(0)
}
