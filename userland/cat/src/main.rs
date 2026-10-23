#![no_std]
#![no_main]
//j547
//j563

//! `cat` — stream one initrd file through explicit capabilities.
//!
//! Sora transfers one bootstrap channel in `x0`. Its finite startup message grants
//! stdout, a read-only argv VMO, and a read-only initrd VMO as explicit capabilities.
//! `cat` reads only the initrd table into its stack, locates the requested path, and
//! streams the payload to stdout in 256-byte chunks. It has no ambient filesystem
//! authority and cannot mutate either input VMO.
//!
//! J547: when Sora grants a filesystem instead (`cat /media/<name>/<path>`), `cat` opens the
//! path on that filesystem through `kumo-vfs` and streams it the same way.

use kumo_abi::{
    entry_table_bytes, find_entry, unpack_argv, Handle, INITRD_HEADER_LEN, INITRD_TABLE_MAX,
};
use kumo_rt::{channel_read, channel_write, debug_write, process_exit, startup, vmo_read};
use kumo_vfs::{
    parse_open_reply, read_payload, Request, VfsError, MAX_PATH, VFS_IS_DIR, VFS_NOT_FOUND,
};

/// Bytes asked for per `Read`: what `fatfs` answers inline.
const READ_CHUNK: usize = 256;

kumo_rt::entry!(main);

fn fail(message: &[u8]) -> ! {
    debug_write(message.as_ptr(), message.len());
    process_exit(1)
}

fn emit(stdout: Handle, bytes: &[u8]) {
    if stdout.0 == 0 {
        debug_write(bytes.as_ptr(), bytes.len());
    } else {
        let _ = channel_write(stdout, bytes.as_ptr(), bytes.len());
    }
}

/// One `kumo-vfs` request and its reply over `fs`.
fn vfs_call<'a>(fs: Handle, request: &[u8], reply: &'a mut [u8]) -> &'a [u8] {
    if channel_write(fs, request.as_ptr(), request.len()) != 0 {
        fail(b"cat: filesystem gone\n");
    }
    let n = channel_read(fs, reply.as_mut_ptr(), reply.len());
    if n == 0 || n == u64::MAX {
        fail(b"cat: filesystem gone\n");
    }
    &reply[..n as usize]
}

/// J547: stream `path` from the filesystem behind `fs`, a `kumo-vfs` channel, to stdout.
fn cat_volume(fs: Handle, stdout: Handle, path: &[u8]) -> ! {
    let mut request = [0u8; 4 + MAX_PATH];
    let mut reply = [0u8; 1 + READ_CHUNK];
    let Some(len) = Request::open(path).encode_into(&mut request) else {
        fail(b"cat: path too long\n");
    };
    let (handle, size) = match parse_open_reply(vfs_call(fs, &request[..len], &mut reply)) {
        Ok(open) => open,
        Err(VfsError::Status(VFS_NOT_FOUND)) => fail(b"cat: not found\n"),
        Err(VfsError::Status(VFS_IS_DIR)) => fail(b"cat: is a directory\n"),
        Err(_) => fail(b"cat: open failed\n"),
    };
    let mut offset = 0u64;
    while offset < size {
        let len = Request::read(handle, offset, READ_CHUNK as u32)
            .encode_into(&mut request)
            .unwrap_or_else(|| fail(b"cat: read request\n"));
        let data = match read_payload(vfs_call(fs, &request[..len], &mut reply)) {
            Ok(data) if !data.is_empty() => data,
            _ => fail(b"cat: read failed\n"),
        };
        emit(stdout, data);
        offset += data.len() as u64;
    }
    process_exit(0)
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
    let argv_handle = startup
        .argv
        .unwrap_or_else(|| fail(b"cat: no argv handle\n"));
    let stdout = startup.stdout.unwrap_or(Handle(0));

    let mut argv_buf = [0u8; 256];
    if vmo_read(argv_handle, 0, argv_buf.as_mut_ptr(), argv_buf.len()) != 0 {
        fail(b"cat: argv read fail\n");
    }
    let mut argv = unpack_argv(&argv_buf);
    let _program = argv.next();
    let path_bytes = match (argv.next(), argv.next()) {
        (Some(path), None) => path,
        _ => fail(b"usage: cat <path>\n"),
    };
    if let Some(fs) = startup.fs {
        cat_volume(fs, stdout, path_bytes);
    }
    let initrd = startup
        .cap0
        .unwrap_or_else(|| fail(b"cat: no initrd handle\n"));
    let path = match core::str::from_utf8(path_bytes) {
        Ok(path) => path,
        Err(_) => fail(b"cat: bad path\n"),
    };

    let mut table = [0u8; INITRD_TABLE_MAX];
    if vmo_read(initrd, 0, table.as_mut_ptr(), INITRD_HEADER_LEN) != 0 {
        fail(b"cat: initrd read fail\n");
    }
    let table_len = match entry_table_bytes(&table[..INITRD_HEADER_LEN]) {
        Ok(len) if len <= table.len() => len,
        Ok(_) => fail(b"cat: table too large\n"),
        Err(_) => fail(b"cat: bad initrd\n"),
    };
    let mut filled = INITRD_HEADER_LEN;
    while filled < table_len {
        let chunk = (table_len - filled).min(256);
        if vmo_read(initrd, filled as u64, table[filled..].as_mut_ptr(), chunk) != 0 {
            fail(b"cat: initrd read fail\n");
        }
        filled += chunk;
    }

    let entry = match find_entry(&table[..table_len], path) {
        Ok(Some(entry)) => entry,
        Ok(None) => fail(b"cat: not found\n"),
        Err(_) => fail(b"cat: bad initrd\n"),
    };
    let mut offset = entry.offset;
    let mut remaining = entry.len;
    let mut buf = [0u8; 256];
    while remaining != 0 {
        let chunk = remaining.min(buf.len() as u64) as usize;
        if vmo_read(initrd, offset, buf.as_mut_ptr(), chunk) != 0 {
            fail(b"cat: file read fail\n");
        }
        emit(stdout, &buf[..chunk]);
        offset += chunk as u64;
        remaining -= chunk as u64;
    }
    process_exit(0)
}
