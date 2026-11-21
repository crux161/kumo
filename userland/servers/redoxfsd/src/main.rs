#![no_std]
#![no_main]
//j928
//j929
//j930
//! The resident `redoxfsd`: the tested handler in this crate's library over a `drv-blk`
//! volume channel, serving `kumo-vfs` sessions exactly as fatfs's mount-scoped server does
//! (SH8-b).
//!
//! Bootstrap: one channel's first message is `drv_blk::encode_mount_bootstrap_flags` and
//! carries the volume's block channel. The bootstrap channel then stays open as the
//! session-control channel: each `[b'S'][access]` grant carries the server end of one client
//! channel. The server owns no device and no ambient authority, only the narrowed volume.

extern crate alloc;

use kumo_abi::{Handle, VmarFlags};
use kumo_redoxfs::redoxfs::FileSystem;
use kumo_redoxfs::{BlockDisk, BlockTransport, Bulk};
use kumo_rt::{
    channel_read, channel_read_with_handle, channel_write, handle_close, handle_koid, port_bind,
    port_create, port_unbind, port_wait, vmar_map, vmar_unmap,
};
use redoxfsd::{
    BulkWindow, CachedDisk, RedoxServer, CACHE_BLOCKS, MAX_SESSIONS, REPLY_BUF_BYTES,
    REQUEST_BUF_BYTES, SESSION_READ_ONLY, SESSION_READ_WRITE,
};

kumo_rt::entry!(main);

/// Diagnostics go to the kernel log (`dmesg`).
fn log_info(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::INFO, message);
}

/// What an operator must see at the default console level.
fn log_notice(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, message);
}

fn fail(message: &[u8]) -> ! {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::ERR, message);
    kumo_rt::process_exit(1)
}

/// Where one `ReadVmo` client window is mapped. RedoxFS's working set lives on the heap, so
/// nothing else is mapped at a fixed address here. Clear of the child stack (fatfs's reason).
const CLIENT_VA: u64 = kumo_rt::USER_WINDOW_BASE + 0x0000_0000_1100_0000;
/// One request's window: `READ_VMO_MAX` plus the page its offset may start inside.
const CLIENT_WINDOW_LEN: u64 = kumo_vfs::READ_VMO_MAX as u64 + 0x1000;

/// Monotonic time since boot stands in for a wall clock until KUMO has one; timestamps are
/// ordered within a boot and not visible through the VFS yet (stat carries none).
fn clock() -> (u64, u32) {
    let ns = kumo_rt::clock_get();
    (ns / 1_000_000_000, (ns % 1_000_000_000) as u32)
}

fn valid(raw: u64) -> Option<Handle> {
    (raw != 0 && raw != u64::MAX).then_some(Handle(raw as u32))
}

/// The volume channel's syscall boundary; framing lives in `kumo-redoxfs`.
struct ChannelBlocks {
    channel: Handle,
    /// j930: the bulk VMO (`BULK_BYTES`, mapped at [`BULK_VA`]); 0 when it could not be made,
    /// and then every transfer goes sector by sector.
    bulk: Handle,
}

/// Where the bulk VMO lives in this process, past the `ReadVmo` client window.
const BULK_VA: u64 = CLIENT_VA + CLIENT_WINDOW_LEN.next_multiple_of(0x10_0000);

impl ChannelBlocks {
    /// A block channel with the process's bulk VMO, made and mapped once (a format after a
    /// refused open builds a second transport over the same mapping).
    fn new(channel: Handle) -> Self {
        static BULK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(u32::MAX);
        let made = BULK.load(core::sync::atomic::Ordering::Relaxed);
        if made != u32::MAX {
            return Self {
                channel,
                bulk: Handle(made),
            };
        }
        let raw = kumo_rt::vmo_create(kumo_redoxfs::BULK_BYTES as u64);
        let bulk = match valid(raw) {
            Some(vmo)
                if vmar_map(
                    Handle(0),
                    vmo,
                    0,
                    BULK_VA,
                    kumo_redoxfs::BULK_BYTES as u64,
                    (VmarFlags::READ | VmarFlags::WRITE).0,
                ) == 0 =>
            {
                vmo
            }
            other => {
                if let Some(vmo) = other {
                    let _ = handle_close(vmo);
                }
                log_info(b"redoxfsd: no bulk VMO; transfers go sector by sector\n");
                Handle(0)
            }
        };
        BULK.store(bulk.0, core::sync::atomic::Ordering::Relaxed);
        Self { channel, bulk }
    }

    fn window(&mut self, len: usize) -> &'static mut [u8] {
        // SAFETY: BULK_VA maps BULK_BYTES of this process's private bulk VMO; `len` is bounded
        // by BlockDisk (one bulk request); only one exchange is ever in flight.
        unsafe { core::slice::from_raw_parts_mut(BULK_VA as *mut u8, len) }
    }
}

impl BlockTransport for ChannelBlocks {
    fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize> {
        if channel_write(self.channel, request.as_ptr(), request.len()) != 0 {
            return None;
        }
        let n = channel_read(self.channel, reply.as_mut_ptr(), reply.len());
        (n != 0 && n != u64::MAX && n <= reply.len() as u64).then_some(n as usize)
    }

    /// j930 (SH8-d): up to 64 KiB per request through the bulk VMO. The driver gets a copy of
    /// it for this request (closed by the driver), never this process's mapping.
    fn exchange_bulk(
        &mut self,
        request: &[u8],
        payload: Bulk<'_>,
        reply: &mut [u8],
    ) -> Option<usize> {
        if self.bulk.0 == 0 {
            return None;
        }
        let len = match &payload {
            Bulk::Read(out) => out.len(),
            Bulk::Write(data) => data.len(),
        };
        if len > kumo_redoxfs::BULK_BYTES {
            return None;
        }
        if let Bulk::Write(data) = &payload {
            self.window(len).copy_from_slice(data);
        }
        let copy = valid(kumo_rt::handle_duplicate(
            self.bulk,
            kumo_abi::Rights(
                kumo_abi::Rights::READ.0 | kumo_abi::Rights::WRITE.0 | kumo_abi::Rights::TRANSFER.0,
            ),
        ))?;
        if kumo_rt::channel_write_with_handle(self.channel, request.as_ptr(), request.len(), copy)
            != 0
        {
            let _ = handle_close(copy);
            return None;
        }
        let n = channel_read(self.channel, reply.as_mut_ptr(), reply.len());
        if n == 0 || n == u64::MAX || n > reply.len() as u64 {
            return None;
        }
        if let (Bulk::Read(out), Some(&drv_blk::STATUS_OK)) = (payload, reply.first()) {
            out.copy_from_slice(self.window(len));
        }
        Some(n as usize)
    }
}

/// The `ReadVmo` window: maps just the pages of the client's VMO that one request names, at a
/// fixed address, and unmaps them before the reply.
struct VmoWindow;

impl BulkWindow for VmoWindow {
    fn fill(
        &mut self,
        vmo: u32,
        vmo_offset: u64,
        len: usize,
        fill: &mut dyn FnMut(&mut [u8]) -> usize,
    ) -> Option<usize> {
        let page = vmo_offset & !0xFFF;
        let delta = vmo_offset - page;
        let map_len = (delta + len as u64).checked_add(0xFFF)? & !0xFFF;
        if map_len > CLIENT_WINDOW_LEN {
            return None;
        }
        let status = vmar_map(
            Handle(0),
            Handle(vmo),
            page,
            CLIENT_VA,
            map_len,
            (VmarFlags::READ | VmarFlags::WRITE).0,
        );
        if status != 0 {
            log_info(b"redoxfsd: ReadVmo window could not be mapped\n");
            return None;
        }
        // SAFETY: the kernel just mapped `map_len >= delta + len` writable bytes at CLIENT_VA,
        // and nothing else in this process refers to them.
        let window =
            unsafe { core::slice::from_raw_parts_mut((CLIENT_VA + delta) as *mut u8, len) };
        let landed = fill(window);
        let _ = vmar_unmap(CLIENT_VA, map_len);
        Some(landed)
    }
}

type Disk = CachedDisk<BlockDisk<ChannelBlocks>>;

/// Mount the volume, or (only when the launcher says it made it blank, and only when it holds
/// no RedoxFS yet) put a new filesystem on it.
/// j930 (SH8-d): the volume would not mount: no RedoxFS, a corrupt or forged header, or an
/// alloc log that fails replay (vendored RedoxFS refuses those with EIO). That is the disk's
/// problem, not a crash: say so and exit 0, without a TOWER. Sora's supervisor restarts the
/// server inside its intensity window like any exit, and then gives the mount up with a notice,
/// so a poisoned disk cannot crash-loop it. Never formatted. — TEAL
fn refuse() -> ! {
    log_notice(b"redoxfsd: volume refused (not a RedoxFS this server will mount)\n");
    kumo_rt::process_exit(0)
}

fn mount(blocks: Handle, block_count: u64, flags: u8) -> FileSystem<Disk> {
    let Ok(volume) = BlockDisk::new(ChannelBlocks::new(blocks), block_count) else {
        fail(b"redoxfsd: volume size is not whole RedoxFS blocks\n");
    };
    let disk = CachedDisk::new(volume, CACHE_BLOCKS);
    // `cleanup` frees nodes a crash left unlinked-but-open.
    let disk = match FileSystem::open(disk, None, Some(0), true) {
        Ok(fs) => {
            log_notice(b"redoxfsd: mounted an existing RedoxFS volume\n");
            return fs;
        }
        Err(_) if flags & drv_blk::MOUNT_FORMAT == 0 => refuse(),
        // `open` consumed the disk; build it again over the same channel.
        Err(_) => match BlockDisk::new(ChannelBlocks::new(blocks), block_count) {
            Ok(volume) => CachedDisk::new(volume, CACHE_BLOCKS),
            Err(_) => fail(b"redoxfsd: volume size is not whole RedoxFS blocks\n"),
        },
    };
    let (secs, nsec) = clock();
    let mut uuid = *b"KUMO-WORK-------";
    uuid[8..].copy_from_slice(&kumo_rt::clock_get().to_le_bytes());
    match FileSystem::create_unencrypted(disk, uuid, secs, nsec) {
        Ok(fs) => {
            log_notice(b"redoxfsd: formatted the blank volume as RedoxFS\n");
            fs
        }
        Err(_) => fail(b"redoxfsd: format failed\n"),
    }
}

/// Serve the mount until the control channel closes with no session left.
fn serve(control: Handle, fs: FileSystem<Disk>) -> ! {
    let Some(port) = valid(port_create()) else {
        fail(b"redoxfsd: port create failed\n");
    };
    let control_koid = handle_koid(control);
    if control_koid == u64::MAX || port_bind(port, control) != 0 {
        fail(b"redoxfsd: control bind failed\n");
    }
    let mut server = alloc::boxed::Box::new(RedoxServer::new(fs, clock));
    let mut sessions: [Option<(Handle, u64)>; MAX_SESSIONS] = [None; MAX_SESSIONS];
    let mut request_buf = [0u8; REQUEST_BUF_BYTES];
    let mut reply_buf = [0u8; REPLY_BUF_BYTES];
    log_info(b"redoxfsd: serving a mount\n");
    loop {
        let source = port_wait(port);
        if source == control_koid {
            let mut grant = [0u8; 2];
            let (n, raw) = channel_read_with_handle(control, grant.as_mut_ptr(), grant.len());
            if n == 0 {
                let _ = port_unbind(port, control);
                if sessions.iter().all(Option::is_none) {
                    kumo_rt::process_exit(0);
                }
                continue;
            }
            if n != 2 || grant[0] != kumo_vfs::SESSION_GRANT {
                if let Some(extra) = valid(raw) {
                    let _ = handle_close(extra);
                }
                continue;
            }
            let Some(channel) = valid(raw) else {
                continue;
            };
            let koid = handle_koid(channel);
            let Some(index) = sessions.iter().position(Option::is_none) else {
                log_info(b"redoxfsd: session full; grant refused\n");
                let _ = handle_close(channel);
                continue;
            };
            if koid == u64::MAX || port_bind(port, channel) != 0 {
                let _ = handle_close(channel);
                continue;
            }
            sessions[index] = Some((channel, koid));
            server.set_access(
                index,
                if grant[1] == kumo_vfs::SESSION_ACCESS_READ_WRITE {
                    SESSION_READ_WRITE
                } else {
                    SESSION_READ_ONLY
                },
            );
            log_info(b"redoxfsd: session granted\n");
        } else if let Some(index) = sessions
            .iter()
            .position(|entry| entry.is_some_and(|(_, koid)| koid == source))
        {
            let Some((channel, _)) = sessions[index] else {
                continue;
            };
            let (n, carried) =
                channel_read_with_handle(channel, request_buf.as_mut_ptr(), request_buf.len());
            let vmo = valid(carried);
            if n == 0 || n > request_buf.len() {
                if let Some(vmo) = vmo {
                    let _ = handle_close(vmo);
                }
                let _ = port_unbind(port, channel);
                let _ = handle_close(channel);
                sessions[index] = None;
                server.close_session(index);
                log_info(b"redoxfsd: session closed\n");
                continue;
            }
            let reply_len = server.dispatch(
                index,
                &request_buf[..n],
                vmo.map_or(0, |h| h.0),
                &mut VmoWindow,
                &mut reply_buf,
            );
            if let Some(vmo) = vmo {
                let _ = handle_close(vmo);
            }
            if reply_len != 0 {
                let _ = channel_write(channel, reply_buf.as_ptr(), reply_len);
            }
        }
    }
}

#[no_mangle]
extern "C" fn main(
    _arg0: u64,
    bootstrap_channel: u64,
    _arg2: u64,
    _arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
    _arg7: u64,
) -> ! {
    log_info(b"redoxfsd starting\n");
    let bootstrap = Handle(bootstrap_channel as u32);
    let mut header = [0u8; drv_blk::MOUNT_FLAGS_BOOTSTRAP_LEN];
    let (n, raw) = channel_read_with_handle(bootstrap, header.as_mut_ptr(), header.len());
    let Some(blocks) = valid(raw) else {
        fail(b"redoxfsd: bootstrap carried no volume channel\n");
    };
    let Some((block_count, flags)) =
        drv_blk::decode_mount_bootstrap_flags(&header[..n.min(header.len())])
    else {
        fail(b"redoxfsd: bootstrap is not a mount bootstrap\n");
    };
    let fs = mount(blocks, block_count, flags);
    // j929 (SH8-c): the launcher sends the control endpoint next, on this private bootstrap
    // channel, so a restarted instance never mistakes a client's queued grant for its bootstrap.
    let mut tag = [0u8; 1];
    let (n, raw) = channel_read_with_handle(bootstrap, tag.as_mut_ptr(), tag.len());
    let _ = handle_close(bootstrap);
    match valid(raw) {
        Some(endpoint) if n == 1 && tag[0] == drv_blk::MOUNT_CONTROL_TAG => serve(endpoint, fs),
        Some(extra) => {
            let _ = handle_close(extra);
            fail(b"redoxfsd: unexpected bootstrap message\n")
        }
        None => fail(b"redoxfsd: bootstrap carried no control endpoint\n"),
    }
}
