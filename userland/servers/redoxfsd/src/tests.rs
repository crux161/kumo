//j928
//! Host proofs for redoxfsd's VFS handler, over the drv-blk protocol and a driver with a
//! volatile write cache (KESTREL's j926 model: only CMD_FLUSH makes bytes stable).
use super::*;
use drv_blk::{
    BlockDevice, Request as BlkRequest, CMD_FLUSH, CMD_READ, CMD_WRITE, REQUEST_LEN,
    STATUS_IO_ERROR, STATUS_OK,
};
use kumo_redoxfs::{BlockDisk, BlockTransport};
use kumo_vfs::{
    parse_dir_reply, parse_open_reply, parse_read_vmo_reply, parse_stat_reply, parse_write_reply,
    read_payload, VfsError,
};
use std::{string::String, vec, vec::Vec};

const SECTORS: u64 = 16 * 1024; // 8 MiB
const UUID: [u8; 16] = *b"KUMO-SH8-b-test1";

#[derive(Clone)]
struct Memory {
    stable: Vec<u8>,
    cache: Vec<u8>,
    sectors: u64,
    writes: usize,
    flushes: usize,
    /// Fail every write after this many more succeed.
    writes_left: Option<usize>,
}

impl Memory {
    fn new(sectors: u64) -> Self {
        let bytes = (sectors * drv_blk::BLOCK_SIZE) as usize;
        Self {
            stable: vec![0; bytes],
            cache: vec![0; bytes],
            sectors,
            writes: 0,
            flushes: 0,
            writes_left: None,
        }
    }

    /// Power loss: the driver's volatile cache is gone; only flushed bytes remain.
    fn power_loss(mut self) -> Self {
        self.cache.clone_from(&self.stable);
        self.writes_left = None;
        self
    }
}

impl BlockTransport for Memory {
    fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize> {
        let req = BlkRequest::decode(request)?;
        let device = BlockDevice::new(self.sectors * drv_blk::BLOCK_SIZE);
        match req.cmd {
            CMD_READ => {
                let data = device.read_blocks(&self.cache, req.lba, req.count).ok()?;
                reply.get_mut(1..1 + data.len())?.copy_from_slice(data);
                reply[0] = STATUS_OK;
                Some(1 + data.len())
            }
            CMD_WRITE => {
                self.writes += 1;
                if let Some(left) = self.writes_left.as_mut() {
                    if *left == 0 {
                        reply[0] = STATUS_IO_ERROR;
                        return Some(1);
                    }
                    *left -= 1;
                }
                reply[0] = device.write_blocks(
                    &mut self.cache,
                    req.lba,
                    req.count,
                    &request[REQUEST_LEN..],
                );
                Some(1)
            }
            CMD_FLUSH => {
                self.flushes += 1;
                self.stable.clone_from(&self.cache);
                reply[0] = STATUS_OK;
                Some(1)
            }
            _ => None,
        }
    }
}

type Server = RedoxServer<CachedDisk<BlockDisk<Memory>>>;

fn clock() -> (u64, u32) {
    (1_700_000_000, 0)
}

fn server_on(memory: Memory, format: bool) -> Server {
    let sectors = memory.sectors;
    let disk = CachedDisk::new(BlockDisk::new(memory, sectors).unwrap(), 256);
    let fs = if format {
        FileSystem::create_unencrypted(disk, UUID, 1, 0).unwrap()
    } else {
        FileSystem::open(disk, None, Some(0), true).unwrap()
    };
    let mut server = RedoxServer::new(fs, clock);
    for session in 0..MAX_SESSIONS {
        server.set_access(session, SESSION_READ_WRITE);
    }
    server
}

fn fresh() -> Server {
    server_on(Memory::new(SECTORS), true)
}

/// Drop every piece of soft state, lose power, and mount again.
fn remount(server: Server) -> Server {
    let memory = server.fs.disk.into_inner().into_transport().power_loss();
    server_on(memory, false)
}

struct Client<'a> {
    server: &'a mut Server,
    session: usize,
}

impl<'a> Client<'a> {
    fn on(server: &'a mut Server, session: usize) -> Self {
        Self { server, session }
    }

    fn call(&mut self, request: Request<'_>) -> Vec<u8> {
        let mut frame = [0u8; REQUEST_BUF_BYTES];
        let n = request.encode_into(&mut frame).expect("encodes");
        let mut reply = [0u8; REPLY_BUF_BYTES];
        let len = self
            .server
            .dispatch(self.session, &frame[..n], 0, &mut NoWindow, &mut reply);
        reply[..len].to_vec()
    }

    fn status(&mut self, request: Request<'_>) -> u8 {
        self.call(request)[0]
    }

    fn open(&mut self, path: &str, options: u8) -> Result<(u32, u64), VfsError> {
        let reply = self.call(Request::OpenOptions {
            path: path.as_bytes(),
            options,
        });
        parse_open_reply(&reply)
    }

    fn create(&mut self, path: &str) -> u32 {
        self.open(path, OPEN_READ | OPEN_WRITE | OPEN_CREATE)
            .expect("create")
            .0
    }

    fn write_all(&mut self, handle: u32, offset: u64, data: &[u8]) {
        for (i, chunk) in data.chunks(MAX_WRITE_DATA).enumerate() {
            let reply = self.call(Request::Write {
                handle,
                offset: offset + (i * MAX_WRITE_DATA) as u64,
                data: chunk,
            });
            assert_eq!(parse_write_reply(&reply), Ok(chunk.len() as u32));
        }
    }

    fn read_all(&mut self, handle: u32) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let reply = self.call(Request::Read {
                handle,
                offset: out.len() as u64,
                len: 256,
            });
            let data = read_payload(&reply).expect("read").to_vec();
            if data.is_empty() {
                return out;
            }
            out.extend_from_slice(&data);
        }
    }

    fn read_file(&mut self, path: &str) -> Vec<u8> {
        let (handle, size) = self.open(path, OPEN_READ).expect("open");
        let data = self.read_all(handle);
        assert_eq!(data.len() as u64, size);
        assert_eq!(self.status(Request::Close { handle }), VFS_OK);
        data
    }

    fn list(&mut self, path: &str) -> Vec<(bool, u64, String)> {
        let mut all = Vec::new();
        loop {
            let reply = self.call(Request::ReadDir {
                path: path.as_bytes(),
                start: all.len() as u32,
            });
            let (count, records) = parse_dir_reply(&reply).expect("readdir");
            if count == 0 {
                all.sort_by(|a: &(bool, u64, String), b| a.2.cmp(&b.2));
                return all;
            }
            for r in records {
                all.push((
                    r.is_dir,
                    r.size,
                    String::from_utf8(r.name.to_vec()).unwrap(),
                ));
            }
        }
    }

    fn stat(&mut self, path: &str) -> Result<kumo_vfs::StatReply, VfsError> {
        let reply = self.call(Request::Stat {
            path: path.as_bytes(),
        });
        parse_stat_reply(&reply)
    }
}

fn pattern(len: usize, seed: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + seed + i / 253) % 256) as u8)
        .collect()
}

#[test]
fn a_tree_written_through_the_vfs_survives_power_loss_and_remount() {
    let mut server = fresh();
    let big = pattern(2 * kumo_redoxfs::redoxfs::RECORD_SIZE as usize + 177, 7);
    {
        let mut c = Client::on(&mut server, 0);
        assert_eq!(c.status(Request::Mkdir { path: b"/ws" }), VFS_OK);
        assert_eq!(c.status(Request::Mkdir { path: b"/ws/src" }), VFS_OK);
        let h = c.create("/ws/src/lib.rs");
        c.write_all(h, 0, &big);
        assert_eq!(c.status(Request::Flush { handle: h }), VFS_OK);
        assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
        let h = c.create("/ws/Cargo.toml");
        c.write_all(h, 0, b"[package]\nname = \"ws\"\n");
        assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
    }
    // 256-byte VFS writes coalesce: a few commits per 64 KiB, not one per write.
    assert!(
        server.write_commits <= (big.len() / WRITE_COALESCE_MAX + 3) as u64,
        "{} commits",
        server.write_commits
    );
    let mut server = remount(server);
    let mut c = Client::on(&mut server, 0);
    assert_eq!(c.read_file("/ws/src/lib.rs"), big);
    assert_eq!(c.read_file("/ws/Cargo.toml"), b"[package]\nname = \"ws\"\n");
    let listed = c.list("/ws");
    assert_eq!(listed.len(), 2);
    assert_eq!(
        (listed[0].0, listed[0].1, listed[0].2.as_str()),
        (false, 22, "Cargo.toml")
    );
    assert_eq!((listed[1].0, listed[1].2.as_str()), (true, "src"));
    let st = c.stat("/ws/src/lib.rs").unwrap();
    assert_eq!(
        (st.kind, st.mode, st.size),
        (STAT_KIND_FILE, 0o644, big.len() as u64)
    );
    let st = c.stat("/ws").unwrap();
    assert_eq!((st.kind, st.mode), (STAT_KIND_DIRECTORY, 0o755));
    let st = c.stat("/").unwrap();
    assert_eq!(st.kind, STAT_KIND_DIRECTORY);

    // Rename, truncate, delete, and each survives another power loss.
    assert_eq!(
        c.status(Request::Rename {
            from: b"/ws/src/lib.rs",
            to: b"/ws/main.rs"
        }),
        VFS_OK
    );
    let (h, _) = c.open("/ws/main.rs", OPEN_READ | OPEN_WRITE).unwrap();
    assert_eq!(
        c.status(Request::SetLen {
            handle: h,
            len: 1000
        }),
        VFS_OK
    );
    assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
    assert_eq!(
        c.status(Request::Unlink {
            path: b"/ws/Cargo.toml"
        }),
        VFS_OK
    );
    assert_eq!(c.status(Request::Rmdir { path: b"/ws/src" }), VFS_OK);
    let mut server = remount(server);
    let mut c = Client::on(&mut server, 0);
    assert_eq!(c.read_file("/ws/main.rs"), big[..1000].to_vec());
    assert_eq!(c.list("/ws").len(), 1);
    assert_eq!(
        c.status(Request::Unlink {
            path: b"/ws/main.rs"
        }),
        VFS_OK
    );
    assert_eq!(c.status(Request::Rmdir { path: b"/ws" }), VFS_OK);
    assert!(c.list("/").is_empty());
}

#[test]
fn acknowledged_but_unflushed_writes_are_page_cache_not_disk() {
    let mut server = fresh();
    let mut c = Client::on(&mut server, 0);
    let h = c.create("/draft");
    c.write_all(h, 0, b"not yet");
    // The handle is still open and nothing else ran: the bytes wait in the coalescing buffer.
    let server = remount(server);
    let mut server = server;
    let mut c = Client::on(&mut server, 0);
    let (_, size) = c.open("/draft", OPEN_READ).unwrap();
    assert_eq!(size, 0, "the create committed; the unflushed write did not");
}

#[test]
fn a_deferred_write_failure_is_reported_once_by_flush_and_never_corrupts() {
    let mut server = fresh();
    {
        let mut c = Client::on(&mut server, 0);
        let h = c.create("/kept");
        c.write_all(h, 0, b"committed bytes");
        assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
        let (h, _) = c.open("/kept", OPEN_READ | OPEN_WRITE).unwrap();
        c.server.fs.disk.inner_mut().transport_mut().writes_left = Some(0);
        c.write_all(h, 0, b"doomed rewrite!");
        assert_eq!(c.status(Request::Flush { handle: h }), VFS_IO_ERROR);
        assert_eq!(
            c.status(Request::Flush { handle: h }),
            VFS_OK,
            "reported once"
        );
        c.server.fs.disk.inner_mut().transport_mut().writes_left = None;
        assert_eq!(c.read_all(h), b"committed bytes");
        assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
    }
    let mut server = remount(server);
    assert_eq!(
        Client::on(&mut server, 0).read_file("/kept"),
        b"committed bytes"
    );
}

#[test]
fn a_failed_commit_at_close_is_the_close_status() {
    let mut server = fresh();
    let mut c = Client::on(&mut server, 0);
    let h = c.create("/f");
    c.server.fs.disk.inner_mut().transport_mut().writes_left = Some(0);
    c.write_all(h, 0, &[1; 600]);
    assert_eq!(c.status(Request::Close { handle: h }), VFS_IO_ERROR);
}

#[test]
fn a_full_volume_answers_no_space_at_the_commit() {
    let mut server = server_on(Memory::new(8192), true); // 4 MiB
    let mut c = Client::on(&mut server, 0);
    let h = c.create("/big");
    let mut status = VFS_OK;
    // Records are lz4-compressed, so the bytes must not compress.
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut chunk = [0u8; MAX_WRITE_DATA];
    for i in 0..(16 * 1024 * 1024 / MAX_WRITE_DATA) {
        for byte in chunk.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *byte = seed as u8;
        }
        let reply = c.call(Request::Write {
            handle: h,
            offset: (i * MAX_WRITE_DATA) as u64,
            data: &chunk,
        });
        if reply[0] != VFS_OK {
            status = reply[0];
            break;
        }
    }
    if status == VFS_OK {
        status = c.status(Request::Flush { handle: h });
    }
    assert_eq!(status, VFS_NO_SPACE);
}

#[test]
fn read_only_sessions_read_and_never_mutate() {
    let mut server = fresh();
    {
        let mut c = Client::on(&mut server, 0);
        let h = c.create("/r");
        c.write_all(h, 0, b"hello");
        assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
    }
    server.set_access(1, SESSION_READ_ONLY);
    let mut ro = Client::on(&mut server, 1);
    assert_eq!(ro.read_file("/r"), b"hello");
    for request in [
        Request::Mkdir { path: b"/d" },
        Request::Unlink { path: b"/r" },
        Request::Rename {
            from: b"/r",
            to: b"/s",
        },
        Request::Chmod {
            path: b"/r",
            mode: 0o600,
        },
        Request::OpenOptions {
            path: b"/r",
            options: OPEN_WRITE,
        },
        Request::OpenOptions {
            path: b"/n",
            options: OPEN_READ | OPEN_CREATE,
        },
    ] {
        assert_eq!(ro.status(request), VFS_ACCESS_DENIED, "{request:?}");
    }
    let (h, _) = ro.open("/r", OPEN_READ).unwrap();
    assert_eq!(
        ro.status(Request::Write {
            handle: h,
            offset: 0,
            data: b"x"
        }),
        VFS_ACCESS_DENIED
    );
}

#[test]
fn a_file_unlinked_while_open_reads_until_its_last_close() {
    let mut server = fresh();
    let mut c = Client::on(&mut server, 0);
    let h = c.create("/tmp-obj");
    let data = pattern(300_000, 3);
    c.write_all(h, 0, &data);
    assert_eq!(c.status(Request::Flush { handle: h }), VFS_OK);
    let free_before = c.server.fs.allocator().free();
    assert_eq!(c.status(Request::Unlink { path: b"/tmp-obj" }), VFS_OK);
    assert_eq!(c.stat("/tmp-obj"), Err(VfsError::Status(VFS_NOT_FOUND)));
    assert_eq!(c.read_all(h), data, "the open handle still reads");
    assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
    assert!(
        c.server.fs.allocator().free() > free_before,
        "the last close released the node's blocks"
    );
}

#[test]
fn a_closed_session_releases_its_handles_and_locks() {
    let mut server = fresh();
    let h0 = {
        let mut a = Client::on(&mut server, 0);
        let h = a.create("/locked");
        assert_eq!(
            a.status(Request::Lock {
                handle: h,
                operation: LOCK_EX
            }),
            VFS_OK
        );
        h
    };
    {
        let mut b = Client::on(&mut server, 1);
        let (h, _) = b.open("/locked", OPEN_READ).unwrap();
        assert_eq!(
            b.status(Request::Lock {
                handle: h,
                operation: LOCK_SH
            }),
            VFS_WOULD_BLOCK
        );
    }
    {
        // The holder's own upgrade/downgrade never conflicts with itself.
        let mut a = Client::on(&mut server, 0);
        assert_eq!(
            a.status(Request::Lock {
                handle: h0,
                operation: LOCK_SH
            }),
            VFS_OK
        );
    }
    {
        let mut b = Client::on(&mut server, 1);
        assert_eq!(
            b.status(Request::Lock {
                handle: 1,
                operation: LOCK_SH
            }),
            VFS_OK,
            "two shared locks coexist"
        );
        assert_eq!(
            b.status(Request::Lock {
                handle: 1,
                operation: LOCK_EX
            }),
            VFS_WOULD_BLOCK
        );
    }
    server.close_session(0);
    let mut b = Client::on(&mut server, 1);
    assert_eq!(
        b.status(Request::Lock {
            handle: 1,
            operation: LOCK_EX
        }),
        VFS_OK,
        "the dead session's lock is gone"
    );
    // Session 0's table is empty again.
    let mut a = Client::on(b.server, 0);
    a.server.set_access(0, SESSION_READ_WRITE);
    assert_eq!(a.status(Request::Close { handle: h0 }), VFS_BAD_HANDLE);
}

#[test]
fn renames_follow_linux_where_it_matters_to_cargo() {
    let mut server = fresh();
    let mut c = Client::on(&mut server, 0);
    for d in [&b"/a"[..], b"/a/b", b"/t"] {
        assert_eq!(c.status(Request::Mkdir { path: d }), VFS_OK);
    }
    for f in ["/f", "/g"] {
        let h = c.create(f);
        c.write_all(h, 0, f.as_bytes());
        assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
    }
    let rename =
        |c: &mut Client<'_>, from: &[u8], to: &[u8]| c.status(Request::Rename { from, to });
    assert_eq!(
        rename(&mut c, b"/a", b"/a/b/c"),
        VFS_BAD_REQUEST,
        "not under itself"
    );
    assert_eq!(
        rename(&mut c, b"/a", b"/t"),
        VFS_EXISTS,
        "a directory needs a free name"
    );
    assert_eq!(rename(&mut c, b"/f", b"/t"), VFS_IS_DIR);
    assert_eq!(
        rename(&mut c, b"/f", b"/g"),
        VFS_OK,
        "a file replaces a file"
    );
    assert_eq!(c.read_file("/g"), b"/f");
    assert_eq!(c.stat("/f"), Err(VfsError::Status(VFS_NOT_FOUND)));
    assert_eq!(rename(&mut c, b"/a", b"/t/a2"), VFS_OK, "a directory moves");
    assert_eq!(c.stat("/t/a2/b").unwrap().kind, STAT_KIND_DIRECTORY);
    assert_eq!(rename(&mut c, b"/g", b"/g"), VFS_OK);
    assert_eq!(rename(&mut c, b"/missing", b"/x"), VFS_NOT_FOUND);
}

#[test]
fn paths_and_errors_answer_like_fatfs() {
    let mut server = fresh();
    let mut c = Client::on(&mut server, 0);
    assert_eq!(c.status(Request::Mkdir { path: b"/d" }), VFS_OK);
    assert_eq!(c.status(Request::Mkdir { path: b"/d" }), VFS_EXISTS);
    assert_eq!(c.status(Request::Mkdir { path: b"/" }), VFS_EXISTS);
    assert_eq!(c.status(Request::Mkdir { path: b"/no/d" }), VFS_NOT_FOUND);
    assert_eq!(
        c.open("/no/f", OPEN_WRITE | OPEN_CREATE),
        Err(VfsError::Status(VFS_NOT_FOUND)),
        "Cargo relies on ENOENT for a missing parent"
    );
    let h = c.create("/d/./x/../f");
    assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
    assert_eq!(c.stat("//d//f").unwrap().kind, STAT_KIND_FILE);
    assert_eq!(
        c.open("/d/f/g", OPEN_READ),
        Err(VfsError::Status(VFS_NOT_DIR))
    );
    assert_eq!(
        c.open("/d/f/g", OPEN_WRITE | OPEN_CREATE),
        Err(VfsError::Status(VFS_NOT_DIR))
    );
    assert_eq!(c.open("/", OPEN_READ), Err(VfsError::Status(VFS_IS_DIR)));
    assert_eq!(c.open("/d", OPEN_READ), Err(VfsError::Status(VFS_IS_DIR)));
    assert_eq!(c.status(Request::Rmdir { path: b"/d" }), VFS_NOT_EMPTY);
    assert_eq!(c.status(Request::Rmdir { path: b"/d/f" }), VFS_NOT_DIR);
    assert_eq!(c.status(Request::Unlink { path: b"/d" }), VFS_IS_DIR);
    assert_eq!(c.status(Request::Rmdir { path: b"/" }), VFS_ACCESS_DENIED);
    assert_eq!(c.status(Request::Unlink { path: b"/" }), VFS_IS_DIR);
    assert_eq!(
        c.status(Request::Mkdir { path: b"/bad\xff" }),
        VFS_BAD_REQUEST
    );
    assert_eq!(c.status(Request::Mkdir { path: b"/a:b" }), VFS_BAD_REQUEST);
    assert_eq!(
        c.status(Request::Chmod {
            path: b"/d/f",
            mode: 0o100_755
        }),
        VFS_OK
    );
    let st = c.stat("/d/f").unwrap();
    assert_eq!(
        (st.kind, st.mode),
        (STAT_KIND_FILE, 0o755),
        "type bits kept"
    );
    assert_eq!(
        c.status(Request::Link {
            from: b"/d/f",
            to: b"/d/g"
        }),
        VFS_NOT_SUPPORTED
    );
    assert_eq!(c.status(Request::Close { handle: 9 }), VFS_BAD_HANDLE);
    let mut reply = [0u8; 8];
    assert_eq!(
        c.server.dispatch(0, &[0xEE], 0, &mut NoWindow, &mut reply),
        1
    );
    assert_eq!(reply[0], VFS_BAD_REQUEST);
}

#[test]
fn a_directory_pages_through_many_entries() {
    let mut server = fresh();
    let mut c = Client::on(&mut server, 0);
    assert_eq!(c.status(Request::Mkdir { path: b"/many" }), VFS_OK);
    let mut names: Vec<String> = (0..150)
        .map(|i| std::format!("entry-with-a-longish-name-{i:03}.rs"))
        .collect();
    for name in &names {
        let h = c.create(&std::format!("/many/{name}"));
        assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
    }
    names.sort();
    let listed: Vec<String> = c.list("/many").into_iter().map(|e| e.2).collect();
    assert_eq!(listed, names);
}

#[test]
fn interleaved_writes_and_reads_see_each_other() {
    let mut server = fresh();
    let mut c = Client::on(&mut server, 0);
    let h = c.create("/rw");
    c.write_all(h, 0, &[b'a'; 1000]);
    // A read commits the coalesced run first, then fills read-ahead.
    let reply = c.call(Request::Read {
        handle: h,
        offset: 990,
        len: 256,
    });
    assert_eq!(read_payload(&reply).unwrap(), &[b'a'; 10]);
    c.write_all(h, 500, b"BBBB");
    let data = c.read_all(h);
    assert_eq!(&data[496..506], b"aaaaBBBBaa");
    assert_eq!(c.stat("/rw").unwrap().size, 1000);
    // A non-contiguous write starts a new run without losing the first.
    c.write_all(h, 2000, b"tail");
    c.write_all(h, 0, b"head");
    let data = c.read_all(h);
    assert_eq!(data.len(), 2004);
    assert_eq!(&data[..4], b"head");
    assert_eq!(&data[1000..2000], &[0u8; 1000][..], "the hole reads zeros");
    assert_eq!(&data[2000..], b"tail");
}

#[test]
fn the_block_cache_serves_rereads_and_forgets_failed_writes() {
    let mut disk = CachedDisk::new(BlockDisk::new(Memory::new(64), 64).unwrap(), 4);
    let block = vec![0x5Au8; BLOCK];
    unsafe { disk.write_at(1, &block).unwrap() };
    let mut out = vec![0u8; BLOCK];
    unsafe { disk.read_at(1, &mut out).unwrap() };
    assert_eq!(out, block);
    assert_eq!(
        (disk.hits, disk.misses),
        (1, 0),
        "a written block is cached"
    );
    // Slot 1 aliases block 5: a miss, read from the device, evicting block 1.
    unsafe { disk.read_at(5, &mut out).unwrap() };
    assert_eq!(disk.misses, 1);
    unsafe { disk.read_at(1, &mut out).unwrap() };
    assert_eq!((out[0], disk.misses), (0x5A, 2));
    disk.inner_mut().transport_mut().writes_left = Some(0);
    assert!(unsafe { disk.write_at(1, &vec![0x11u8; BLOCK]) }.is_err());
    unsafe { disk.read_at(1, &mut out).unwrap() };
    assert_eq!(disk.misses, 3, "a failed write forgot the block");
    // Only flushes reach stable bytes: caching changed no barrier.
    let flushes = disk.inner_mut().transport_mut().flushes;
    disk.sync().unwrap();
    assert_eq!(disk.inner_mut().transport_mut().flushes, flushes + 1);
}

struct BufWindow(Vec<u8>);

impl BulkWindow for BufWindow {
    fn fill(
        &mut self,
        _vmo: u32,
        vmo_offset: u64,
        len: usize,
        fill: &mut dyn FnMut(&mut [u8]) -> usize,
    ) -> Option<usize> {
        let start = vmo_offset as usize;
        Some(fill(self.0.get_mut(start..start + len)?))
    }
}

#[test]
fn read_vmo_lands_file_bytes_in_the_carried_window() {
    let mut server = fresh();
    let data = pattern(200_000, 9);
    {
        let mut c = Client::on(&mut server, 0);
        let h = c.create("/obj");
        c.write_all(h, 0, &data);
        assert_eq!(c.status(Request::Close { handle: h }), VFS_OK);
    }
    let mut c = Client::on(&mut server, 0);
    let (h, _) = c.open("/obj", OPEN_READ).unwrap();
    let mut window = BufWindow(vec![0; 300_000]);
    let mut frame = [0u8; REQUEST_BUF_BYTES];
    let n = Request::ReadVmo {
        handle: h,
        offset: 1000,
        len: 250_000,
        vmo_offset: 16,
    }
    .encode_into(&mut frame)
    .unwrap();
    let mut reply = [0u8; REPLY_BUF_BYTES];
    let len = server.dispatch(0, &frame[..n], 7, &mut window, &mut reply);
    assert_eq!(parse_read_vmo_reply(&reply[..len]), Ok(199_000));
    assert_eq!(&window.0[16..16 + 199_000], &data[1000..]);
    let len = server.dispatch(0, &frame[..n], 0, &mut window, &mut reply);
    assert_eq!(reply[..len], [VFS_BAD_REQUEST], "no VMO carried");
}
