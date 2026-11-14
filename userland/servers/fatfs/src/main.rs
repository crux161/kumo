#![no_std]
#![no_main]
//j549
//j559
//j864
//j891
//j892
//! The resident `fatfs` server: the handler in this crate's library, wired to a
//! `Channel` transport and a sector reader, as its module docs anticipated.
//!
//! Bootstrap mirrors `drv-blk`'s: one channel carries a first message of
//! `[image_base: u64 LE][map_len: u64 LE]` plus a read-write VMO handle, and then
//! becomes the `kumo-vfs` request channel. The server owns no device and no
//! ambient authority — it can mutate exactly the interop-volume VMO it was
//! handed, and answer exactly the peer that holds the other end.
//!
//! J545 adds a second backing: a removable volume served by a block driver. Its bootstrap is
//! `drv_blk::encode_client_bootstrap` plus a `drv-blk`-protocol channel instead of the VMO. The
//! driver has already narrowed that channel to the volume, so block 0 is the volume's boot
//! sector and nothing outside the partition can be named.

use fatfs::block::{BlockSectors, BlockTransport};
use fatfs::{
    dispatch_shared_bulk, BulkWindow, FatServer, SharedServer, Transport, MAX_SESSIONS,
    REPLY_BUF_BYTES, REQUEST_BUF_BYTES, SESSION_READ_ONLY, SESSION_READ_WRITE,
};
use kumo_abi::{Handle, Rights, VmarFlags};
use kumo_fatfs::{FatVolume, SectorReader, SectorWriter, SECTOR_SIZE};
use kumo_rt::{
    channel_read, channel_read_with_handle, channel_write, channel_write_with_handle, handle_close,
    handle_duplicate, handle_koid, port_bind, port_create, port_unbind, port_wait, vmar_map,
    vmar_map_anywhere_at, vmar_unmap,
};

/// Diagnostics go to the kernel log (`dmesg`) since J559, shown on the console only when its level
/// allows.
fn log_info(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::INFO, message);
}

kumo_rt::entry!(main);

/// Largest backing image this server will map. Matches `drv-blk`'s ceiling for the
/// same reason: it covers the shipped FAT32 image with room to spare.
const MAX_VMO_MAP: u64 = 16 * 1024 * 1024;

/// Where the backing VMO lands in this process. `drv-blk` picks the same base, and
/// for the same reason: `run_elf` puts the child stack at [0x1000_C000, 0x1001_0000),
/// so a multi-MiB map at 0x1000_0000 would remap the stack read-only.
const VMO_VA: u64 = 0x0000_0000_1100_0000;

fn fail(message: &[u8]) -> ! {
    log_info(message);
    kumo_rt::process_exit(1)
}

/// The backing store as mapped bytes. `base` is the byte offset of the FAT image
/// inside that mapping, so the volume's LBA 0 is the image's first sector rather
/// than the container's.
struct MappedSectors {
    base: u64,
    len: u64,
}

impl SectorReader for MappedSectors {
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> bool {
        let sector = SECTOR_SIZE as u64;
        let Some(offset) = (lba as u64)
            .checked_mul(sector)
            .and_then(|o| o.checked_add(self.base))
        else {
            return false;
        };
        // Every read is bounded by the mapping, so a request past the image is a
        // clean failure the volume reports, never a fault.
        if offset.checked_add(sector).is_none_or(|end| end > self.len) {
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                (VMO_VA + offset) as *const u8,
                buf.as_mut_ptr(),
                SECTOR_SIZE,
            );
        }
        true
    }

    /// A run of sectors is one copy out of the mapped image (SH7-d3a).
    fn read_sectors(&mut self, lba: u32, out: &mut [u8]) -> bool {
        let Some(offset) = (lba as u64)
            .checked_mul(SECTOR_SIZE as u64)
            .and_then(|o| o.checked_add(self.base))
        else {
            return false;
        };
        if out.len() % SECTOR_SIZE != 0
            || offset
                .checked_add(out.len() as u64)
                .is_none_or(|end| end > self.len)
        {
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                (VMO_VA + offset) as *const u8,
                out.as_mut_ptr(),
                out.len(),
            );
        }
        true
    }
}

/// SH7-d3a: the `ReadVmo` window. Maps just the pages of the client's VMO that one request
/// names, wherever the kernel places them, and unmaps them before the reply. The client's VMO
/// handle is closed after every request that carried one.
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
        let (status, view) = vmar_map_anywhere_at(
            Handle(vmo),
            page,
            map_len,
            (VmarFlags::READ | VmarFlags::WRITE).0,
        );
        if status != 0 || view == 0 {
            log_info(b"fatfs: ReadVmo window could not be mapped\n");
            return None;
        }
        // SAFETY: the kernel just mapped `map_len >= delta + len` writable bytes at `view`,
        // and nothing else in this process refers to them.
        let window = unsafe { core::slice::from_raw_parts_mut((view + delta) as *mut u8, len) };
        let landed = fill(window);
        let _ = vmar_unmap(view, map_len);
        Some(landed)
    }

    fn release(&mut self, vmo: u32) {
        let _ = handle_close(Handle(vmo));
    }
}

impl SectorWriter for MappedSectors {
    fn write_sector(&mut self, lba: u32, buf: &[u8; SECTOR_SIZE]) -> bool {
        let sector = SECTOR_SIZE as u64;
        let Some(offset) = (lba as u64)
            .checked_mul(sector)
            .and_then(|value| value.checked_add(self.base))
        else {
            return false;
        };
        if offset.checked_add(sector).is_none_or(|end| end > self.len) {
            return false;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), (VMO_VA + offset) as *mut u8, SECTOR_SIZE);
        }
        true
    }
}

/// The block channel's syscall boundary; sector framing lives in the tested library.
struct ChannelBlocks {
    channel: Handle,
}

impl BlockTransport for ChannelBlocks {
    fn exchange(&mut self, request: &[u8], reply: &mut [u8]) -> Option<usize> {
        if channel_write(self.channel, request.as_ptr(), request.len()) != 0 {
            return None;
        }
        let n = channel_read(self.channel, reply.as_mut_ptr(), reply.len());
        (n != 0 && n != u64::MAX && n <= reply.len() as u64).then_some(n as usize)
    }

    /// SH7-d3b: the request carries a copy of the client's VMO, with the rights to write and
    /// map it and nothing more; the block driver closes it when the request is done.
    fn exchange_with_vmo(&mut self, request: &[u8], vmo: u32, reply: &mut [u8]) -> Option<usize> {
        let raw = handle_duplicate(
            Handle(vmo),
            Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::TRANSFER.0),
        );
        let copy = valid(raw)?;
        if channel_write_with_handle(self.channel, request.as_ptr(), request.len(), copy) != 0 {
            let _ = handle_close(copy);
            return None;
        }
        let n = channel_read(self.channel, reply.as_mut_ptr(), reply.len());
        (n != 0 && n != u64::MAX && n <= reply.len() as u64).then_some(n as usize)
    }
}

fn valid(raw: u64) -> Option<Handle> {
    (raw != 0 && raw != u64::MAX).then_some(Handle(raw as u32))
}

/// Serve a mount-scoped volume (SH3-c2): one mount, many client sessions. The bootstrap
/// channel stays open as the session-control channel; each `[b'S'][access]` grant carries
/// the server end of one client channel, and requests dispatch through the shared server
/// (per-session open tables, cross-session advisory locks). A closed session releases its
/// locks and table; a closed control channel with no sessions left ends the process.
fn serve_mount(control: Handle, blocks: Handle, block_count: u64) -> ! {
    let mut disk = BlockSectors::new(ChannelBlocks { channel: blocks }, block_count);
    let Ok(volume) = FatVolume::mount(&mut disk) else {
        fail(b"fatfs: mount-scoped block mount failed\n");
    };
    let Some(port) = valid(port_create()) else {
        fail(b"fatfs: mount-scoped port create failed\n");
    };
    let control_koid = handle_koid(control);
    if control_koid == u64::MAX || port_bind(port, control) != 0 {
        fail(b"fatfs: mount-scoped control bind failed\n");
    }
    log_info(b"fatfs: serving a mount\n");
    let mut server = SharedServer::new();
    let mut sessions: [Option<(Handle, u64)>; MAX_SESSIONS] = [None; MAX_SESSIONS];
    let mut request_buf = [0u8; REQUEST_BUF_BYTES];
    let mut reply_buf = [0u8; REPLY_BUF_BYTES];
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
            if n == 2 && grant[0] == b'S' {
                let Some(channel) = valid(raw) else {
                    continue;
                };
                let koid = handle_koid(channel);
                let Some(index) = sessions.iter().position(Option::is_none) else {
                    log_info(b"fatfs: session full; grant refused\n");
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
                    if grant[1] == 0 {
                        SESSION_READ_ONLY
                    } else {
                        SESSION_READ_WRITE
                    },
                );
                log_info(b"fatfs: session granted\n");
            } else if let Some(extra) = valid(raw) {
                let _ = handle_close(extra);
            }
        } else if let Some(index) = sessions
            .iter()
            .position(|entry| entry.is_some_and(|(_, koid)| koid == source))
        {
            let Some((channel, _)) = sessions[index] else {
                continue;
            };
            let (n, carried) =
                channel_read_with_handle(channel, request_buf.as_mut_ptr(), request_buf.len());
            let n = n as u64;
            if n == 0 || n > request_buf.len() as u64 {
                if carried != 0 && carried != u64::MAX {
                    let _ = handle_close(Handle(carried as u32));
                }
                let _ = port_unbind(port, channel);
                let _ = handle_close(channel);
                sessions[index] = None;
                server.close_session(index);
                log_info(b"fatfs: session closed\n");
            } else {
                let vmo = valid(carried).map_or(0, |handle| handle.0);
                let reply_len = dispatch_shared_bulk(
                    &volume,
                    &mut disk,
                    &mut server,
                    index,
                    &request_buf[..n as usize],
                    vmo,
                    &mut VmoWindow,
                    &mut reply_buf,
                );
                if vmo != 0 {
                    VmoWindow.release(vmo);
                }
                if reply_len != 0 {
                    let _ = channel_write(channel, reply_buf.as_ptr(), reply_len);
                }
            }
        }
    }
}

/// Serve a block-backed volume until the vfs peer closes (J545).
fn serve_blocks(channel: Handle, blocks: Handle, block_count: u64) -> ! {
    let mut disk = BlockSectors::new(ChannelBlocks { channel: blocks }, block_count);
    let Ok(volume) = FatVolume::mount(&mut disk) else {
        fail(b"fatfs: block mount failed\n");
    };
    log_info(b"fatfs: serving blocks\n");
    let mut transport = ChannelTransport { channel };
    FatServer::new().serve_bulk(&volume, &mut disk, &mut transport, &mut VmoWindow);
    kumo_rt::process_exit(0)
}

/// The vfs request channel. A zero-length read means the peer is gone, which is
/// the serve loop's cue to exit rather than spin.
struct ChannelTransport {
    channel: Handle,
}

impl Transport for ChannelTransport {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        let n = channel_read(self.channel, buf.as_mut_ptr(), buf.len());
        if n == 0 || n == u64::MAX {
            None
        } else {
            Some(n as usize)
        }
    }

    fn send(&mut self, frame: &[u8]) {
        let _ = channel_write(self.channel, frame.as_ptr(), frame.len());
    }

    fn recv_with_handle(&mut self, buf: &mut [u8]) -> Option<(usize, u32)> {
        let (n, raw) = channel_read_with_handle(self.channel, buf.as_mut_ptr(), buf.len());
        let handle = valid(raw).map_or(0, |handle| handle.0);
        if n == 0 {
            if handle != 0 {
                let _ = handle_close(Handle(handle));
            }
            return None;
        }
        Some((n, handle))
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
    log_info(b"fatfs starting\n");
    let channel = Handle(bootstrap_channel as u32);

    let mut header = [0u8; 16];
    let (n, vmo_raw) = channel_read_with_handle(channel, header.as_mut_ptr(), header.len());
    if vmo_raw != 0 && drv_blk::decode_mount_bootstrap(&header[..n.min(header.len())]).is_some() {
        let block_count = drv_blk::decode_mount_bootstrap(&header[..n.min(header.len())])
            .expect("checked mount bootstrap");
        serve_mount(channel, Handle(vmo_raw as u32), block_count);
    }
    if vmo_raw != 0 {
        if let Some(block_count) = drv_blk::decode_client_bootstrap(&header[..n.min(header.len())])
        {
            serve_blocks(channel, Handle(vmo_raw as u32), block_count);
        }
    }
    if vmo_raw == 0 || n < header.len() {
        fail(b"fatfs: bootstrap missing image\n");
    }
    let base = u64::from_le_bytes(header[..8].try_into().unwrap());
    let map_len = u64::from_le_bytes(header[8..].try_into().unwrap());
    // `vmar_map` requires a page-aligned length, and the initrd size the launcher
    // computes is not 4 KiB-aligned (the same rounding drv-blk does).
    let map_len = map_len.min(MAX_VMO_MAP) & !0xFFF;
    if map_len == 0 || base >= map_len {
        fail(b"fatfs: bootstrap image out of range\n");
    }
    if vmar_map(
        Handle(0),
        Handle(vmo_raw as u32),
        0,
        VMO_VA,
        map_len,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    ) != 0
    {
        fail(b"fatfs: image map failed\n");
    }

    let mut disk = MappedSectors { base, len: map_len };
    let Ok(volume) = FatVolume::mount(&mut disk) else {
        fail(b"fatfs: mount failed\n");
    };
    log_info(b"fatfs: serving\n");

    let mut transport = ChannelTransport { channel };
    FatServer::new().serve_bulk(&volume, &mut disk, &mut transport, &mut VmoWindow);

    // The peer closed its end: this instance's whole reason to exist is gone.
    kumo_rt::process_exit(0)
}
