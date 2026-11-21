#![no_std]
//j928
//! `redoxprobe`, the SH8-b gate as a client sees it. Everything goes through one `kumo-vfs`
//! session: no block, no RedoxFS and no server internals. The same sequence runs on the host
//! against redoxfsd's handler (this crate's tests) and on KUMO behind `media`.
//!
//! `write` builds a known tree under `/sh8` and checks every step on the way. `verify`
//! finds exactly that tree, with exactly those bytes, after the server has been replaced.

use kumo_vfs::{
    parse_dir_reply, parse_open_reply, parse_stat_reply, parse_write_reply, read_payload, Request,
    VfsError, MAX_WRITE_DATA, OPEN_CREATE, OPEN_READ, OPEN_WRITE, STAT_KIND_DIRECTORY,
    STAT_KIND_FILE, VFS_NOT_FOUND, VFS_OK,
};

/// One `kumo-vfs` exchange: write the request frame, read the reply into `reply`.
pub trait Vfs {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize>;
}

/// The large file: incompressible (RedoxFS lz4-compresses records) and longer than two
/// 128 KiB records, so it spans three.
pub const BIG_LEN: usize = 300_000;
pub const SMALL: &[u8] = b"redoxfs on kumo\n";

/// Byte `i` of the large file: an xorshift stream, so no prefix repeats.
pub struct Stream(u64);

impl Stream {
    pub const fn new() -> Self {
        Self(0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_byte(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 24) as u8
    }
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a phase failed: a short step name for the console.
pub type Fail = &'static str;

struct Session<'v, V: Vfs> {
    vfs: &'v mut V,
    reply: [u8; 1024],
}

impl<V: Vfs> Session<'_, V> {
    fn call(&mut self, request: Request<'_>, step: Fail) -> Result<&[u8], Fail> {
        let mut frame = [0u8; 15 + MAX_WRITE_DATA + 256];
        let n = request.encode_into(&mut frame).ok_or(step)?;
        let got = self.vfs.call(&frame[..n], &mut self.reply).ok_or(step)?;
        self.reply.get(..got).ok_or(step)
    }

    fn ok(&mut self, request: Request<'_>, step: Fail) -> Result<(), Fail> {
        match self.call(request, step)? {
            [VFS_OK, ..] => Ok(()),
            _ => Err(step),
        }
    }

    fn status(&mut self, request: Request<'_>, step: Fail) -> Result<u8, Fail> {
        self.call(request, step)?.first().copied().ok_or(step)
    }

    fn open(&mut self, path: &[u8], options: u8, step: Fail) -> Result<(u32, u64), Fail> {
        let reply = self.call(Request::OpenOptions { path, options }, step)?;
        parse_open_reply(reply).map_err(|_| step)
    }

    fn close(&mut self, handle: u32, step: Fail) -> Result<(), Fail> {
        self.ok(Request::Close { handle }, step)
    }

    fn write(&mut self, handle: u32, offset: u64, data: &[u8], step: Fail) -> Result<(), Fail> {
        for (i, chunk) in data.chunks(MAX_WRITE_DATA).enumerate() {
            let at = offset + (i * MAX_WRITE_DATA) as u64;
            let reply = self.call(
                Request::Write {
                    handle,
                    offset: at,
                    data: chunk,
                },
                step,
            )?;
            if parse_write_reply(reply) != Ok(chunk.len() as u32) {
                return Err(step);
            }
        }
        Ok(())
    }

    /// Read the whole open file, handing each piece to `each` in order.
    fn read(
        &mut self,
        handle: u32,
        step: Fail,
        mut each: impl FnMut(&[u8]) -> bool,
    ) -> Result<u64, Fail> {
        let mut offset = 0u64;
        loop {
            let reply = self.call(
                Request::Read {
                    handle,
                    offset,
                    len: 256,
                },
                step,
            )?;
            let data = read_payload(reply).map_err(|_| step)?;
            if data.is_empty() {
                return Ok(offset);
            }
            if !each(data) {
                return Err(step);
            }
            offset += data.len() as u64;
        }
    }

    fn stat(&mut self, path: &[u8], step: Fail) -> Result<kumo_vfs::StatReply, Fail> {
        let reply = self.call(Request::Stat { path }, step)?;
        parse_stat_reply(reply).map_err(|_| step)
    }

    fn missing(&mut self, path: &[u8], step: Fail) -> Result<(), Fail> {
        let reply = self.call(Request::Stat { path }, step)?;
        match parse_stat_reply(reply) {
            Err(VfsError::Status(VFS_NOT_FOUND)) => Ok(()),
            _ => Err(step),
        }
    }

    /// `/sh8` lists exactly `dir/` and `keep.bin` (300000 bytes).
    fn listing_is_final(&mut self, step: Fail) -> Result<(), Fail> {
        let reply = self.call(
            Request::ReadDir {
                path: b"/sh8",
                start: 0,
            },
            step,
        )?;
        let (count, records) = parse_dir_reply(reply).map_err(|_| step)?;
        if count != 2 {
            return Err(step);
        }
        let mut seen = 0;
        for r in records {
            match (r.name, r.is_dir) {
                (b"dir", true) => seen |= 1,
                (b"keep.bin", false) if r.size == BIG_LEN as u64 => seen |= 2,
                _ => return Err(step),
            }
        }
        if seen != 3 {
            return Err(step);
        }
        let reply = self.call(
            Request::ReadDir {
                path: b"/sh8",
                start: 2,
            },
            step,
        )?;
        match parse_dir_reply(reply) {
            Ok((0, _)) => Ok(()),
            _ => Err(step),
        }
    }

    fn big_reads_back(&mut self, path: &[u8], step: Fail) -> Result<(), Fail> {
        let (h, size) = self.open(path, OPEN_READ, step)?;
        if size != BIG_LEN as u64 {
            return Err(step);
        }
        let mut stream = Stream::new();
        let read = self.read(h, step, |data| {
            data.iter().all(|b| *b == stream.next_byte())
        })?;
        if read != BIG_LEN as u64 {
            return Err(step);
        }
        self.close(h, step)
    }
}

/// Clear a previous run's tree; absent paths are fine.
fn clean<V: Vfs>(s: &mut Session<'_, V>) -> Result<(), Fail> {
    for path in [
        &b"/sh8/tmp/big.bin"[..],
        b"/sh8/tmp/gone.txt",
        b"/sh8/tmp/trunc.bin",
        b"/sh8/keep.bin",
        b"/sh8/dir/small.txt",
    ] {
        s.status(Request::Unlink { path }, "clean")?;
    }
    for path in [&b"/sh8/tmp"[..], b"/sh8/dir", b"/sh8"] {
        s.status(Request::Rmdir { path }, "clean")?;
    }
    s.missing(b"/sh8", "clean")
}

/// Build the tree, checking each operation's effect as it goes.
pub fn write_phase<V: Vfs>(vfs: &mut V) -> Result<(), Fail> {
    let s = &mut Session {
        vfs,
        reply: [0; 1024],
    };
    clean(s)?;
    for path in [&b"/sh8"[..], b"/sh8/dir", b"/sh8/tmp"] {
        s.ok(Request::Mkdir { path }, "mkdir")?;
    }
    if s.stat(b"/sh8/dir", "stat dir")?.kind != STAT_KIND_DIRECTORY {
        return Err("stat dir");
    }

    // Three records, in 256-byte VFS writes.
    let (h, size) = s.open(
        b"/sh8/tmp/big.bin",
        OPEN_READ | OPEN_WRITE | OPEN_CREATE,
        "create big",
    )?;
    if size != 0 {
        return Err("create big");
    }
    let mut stream = Stream::new();
    let mut chunk = [0u8; MAX_WRITE_DATA];
    let mut at = 0usize;
    while at < BIG_LEN {
        let n = MAX_WRITE_DATA.min(BIG_LEN - at);
        for b in &mut chunk[..n] {
            *b = stream.next_byte();
        }
        s.write(h, at as u64, &chunk[..n], "write big")?;
        at += n;
    }
    s.ok(Request::Flush { handle: h }, "flush big")?;
    s.close(h, "close big")?;
    let st = s.stat(b"/sh8/tmp/big.bin", "stat big")?;
    if st.kind != STAT_KIND_FILE || st.size != BIG_LEN as u64 {
        return Err("stat big");
    }
    s.big_reads_back(b"/sh8/tmp/big.bin", "read big")?;

    // Rename across directories.
    s.ok(
        Request::Rename {
            from: b"/sh8/tmp/big.bin",
            to: b"/sh8/keep.bin",
        },
        "rename",
    )?;
    s.missing(b"/sh8/tmp/big.bin", "rename")?;

    let (h, _) = s.open(
        b"/sh8/dir/small.txt",
        OPEN_WRITE | OPEN_CREATE,
        "create small",
    )?;
    s.write(h, 0, SMALL, "write small")?;
    s.close(h, "close small")?;

    // An unlinked file stays readable through a handle opened before the unlink.
    let (h, _) = s.open(
        b"/sh8/tmp/gone.txt",
        OPEN_READ | OPEN_WRITE | OPEN_CREATE,
        "create gone",
    )?;
    s.write(h, 0, b"still here", "write gone")?;
    s.ok(
        Request::Unlink {
            path: b"/sh8/tmp/gone.txt",
        },
        "unlink gone",
    )?;
    s.missing(b"/sh8/tmp/gone.txt", "unlink gone")?;
    let mut held = [0u8; 16];
    let mut len = 0;
    s.read(h, "read unlinked", |data| {
        let fits = len + data.len() <= held.len();
        if fits {
            held[len..len + data.len()].copy_from_slice(data);
            len += data.len();
        }
        fits
    })?;
    if &held[..len] != b"still here" {
        return Err("read unlinked");
    }
    s.close(h, "close gone")?;

    // Truncate.
    let (h, _) = s.open(
        b"/sh8/tmp/trunc.bin",
        OPEN_WRITE | OPEN_CREATE,
        "create trunc",
    )?;
    s.write(h, 0, &[0x33; 5000], "write trunc")?;
    s.ok(
        Request::SetLen {
            handle: h,
            len: 100,
        },
        "truncate",
    )?;
    s.close(h, "close trunc")?;
    if s.stat(b"/sh8/tmp/trunc.bin", "stat trunc")?.size != 100 {
        return Err("stat trunc");
    }
    s.ok(
        Request::Unlink {
            path: b"/sh8/tmp/trunc.bin",
        },
        "unlink trunc",
    )?;

    // A populated directory refuses rmdir; an empty one goes.
    if s.status(Request::Rmdir { path: b"/sh8/dir" }, "rmdir full")? == VFS_OK {
        return Err("rmdir full");
    }
    s.ok(Request::Rmdir { path: b"/sh8/tmp" }, "rmdir")?;
    s.listing_is_final("readdir")
}

/// Find exactly the tree `write_phase` left, byte for byte.
pub fn verify_phase<V: Vfs>(vfs: &mut V) -> Result<(), Fail> {
    let s = &mut Session {
        vfs,
        reply: [0; 1024],
    };
    s.listing_is_final("readdir")?;
    s.big_reads_back(b"/sh8/keep.bin", "read keep")?;
    let (h, size) = s.open(b"/sh8/dir/small.txt", OPEN_READ, "open small")?;
    if size != SMALL.len() as u64 {
        return Err("open small");
    }
    let mut at = 0;
    s.read(h, "read small", |data| {
        let same = SMALL.get(at..at + data.len()) == Some(data);
        at += data.len();
        same
    })?;
    s.close(h, "close small")?;
    s.missing(b"/sh8/tmp", "tmp gone")
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
