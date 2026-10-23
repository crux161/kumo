#![no_std]
#![no_main]
//j508
//j514
//j545
//j549
//j559
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
use fatfs::{FatServer, Transport};
use kumo_abi::{Handle, VmarFlags};
use kumo_fatfs::{FatVolume, SectorReader, SectorWriter, SECTOR_SIZE};
use kumo_rt::{channel_read, channel_read_with_handle, channel_write, vmar_map};

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
}

/// Serve a block-backed volume until the vfs peer closes (J545).
fn serve_blocks(channel: Handle, blocks: Handle, block_count: u64) -> ! {
    let mut disk = BlockSectors::new(ChannelBlocks { channel: blocks }, block_count);
    let Ok(volume) = FatVolume::mount(&mut disk) else {
        fail(b"fatfs: block mount failed\n");
    };
    log_info(b"fatfs: serving blocks\n");
    let mut transport = ChannelTransport { channel };
    FatServer::new().serve(&volume, &mut disk, &mut transport);
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
    FatServer::new().serve(&volume, &mut disk, &mut transport);

    // The peer closed its end: this instance's whole reason to exist is gone.
    kumo_rt::process_exit(0)
}
