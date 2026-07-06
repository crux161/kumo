#![no_std]
#![no_main]

//j166
//j168
//j230
//j413
//j414

use drv_blk::{
    split_write_frame, BlockDevice, Request, CMD_FLUSH, CMD_READ, CMD_WRITE, STATUS_BAD_LBA,
    STATUS_OK,
};
use kumo_abi::{Handle, VmarFlags};
use kumo_rt::{
    channel_read, channel_read_with_handle, channel_write, debug_write, vmar_map, vmo_create,
};

kumo_rt::entry!(main);

/// Block I/O request frame on the channel: `[cmd: u8][lba: u64 LE][count: u16 LE]`, followed
/// for a write by `count * 512` data bytes. Read response: `status(1) + data(count*512)`;
/// write/flush response: `status(1)`.
///
/// Maximum initrd size we'll try to map (16 MiB — covers the test FAT32 image).
const MAX_VMO_MAP: u64 = 16 * 1024 * 1024;

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
    debug_write(b"drv-blk starting\n".as_ptr(), 16);

    let ch = Handle(bootstrap_channel as u32);

    // Read the initrd VMO handle + 8-byte size from the bootstrap channel.
    let mut size_buf = [0u8; 8];
    let (n, vmo_raw) = channel_read_with_handle(ch, size_buf.as_mut_ptr(), size_buf.len());
    let vmo = Handle(vmo_raw as u32);
    if vmo_raw == 0 {
        debug_write(b"drv-blk: missing vmo\n".as_ptr(), 20);
        kumo_rt::process_exit(1);
    }
    let vmo_len = if n >= 8 {
        u64::from_le_bytes(size_buf)
    } else {
        MAX_VMO_MAP // fallback: map up to 16 MiB
    };
    let map_len = if vmo_len < MAX_VMO_MAP {
        vmo_len
    } else {
        MAX_VMO_MAP
    };
    // Vmar::map requires a page-aligned length; round down so we never exceed
    // the VMO. The initrd size Sora computes is not 4 KiB-aligned (J168).
    let map_len = map_len & !0xFFF;

    // Map the readable portion of the VMO into our address space. Use a base
    // clear of the child stack — run_elf puts the stack at [0x1000_C000,
    // 0x1001_0000), so mapping a multi-MiB VMO at 0x1000_0000 would remap the
    // stack READ-only and fault on the next push (J168). 0x1100_0000 leaves the
    // full 16 MiB below the 0x2000_0000 child-VMAR boundary.
    let vmo_va = 0x0000_0000_1100_0000;
    if vmar_map(Handle(0), vmo, 0, vmo_va, map_len, (VmarFlags::READ).0) != 0 {
        debug_write(b"drv-blk: map failed\n".as_ptr(), 20);
        kumo_rt::process_exit(1);
    }

    // Back the ramdisk with a PRIVATE read-write copy, not the shared initrd. The initrd VMO
    // is aliased by Sora (which loads programs from it) and by this driver's read consumers
    // (the blk-rt boot proof, the fatfs mount); writing through it would corrupt the live boot
    // image. Instead: create an anonymous RW VMO of the same mapped length, map it R/W (the
    // kernel eagerly allocates+zeroes every page at map time — no demand-fault on first
    // touch), and copy the initrd content in. Reads and writes then hit this private copy, so
    // a write persists in RAM and reads see it, while the initrd stays pristine. — KESTREL
    let rw_vmo_raw = vmo_create(map_len);
    if rw_vmo_raw == 0 || rw_vmo_raw == u64::MAX {
        debug_write(b"drv-blk: rw vmo create failed\n".as_ptr(), 30);
        kumo_rt::process_exit(1);
    }
    let rw_vmo = Handle(rw_vmo_raw as u32);
    // Place the RW copy immediately above the read-only initrd map. `map_len` is page-aligned
    // and <= 16 MiB, so `rw_va + map_len` stays below the 0x2000_0000 child-VMAR boundary.
    let rw_va = vmo_va + map_len;
    if vmar_map(
        Handle(0),
        rw_vmo,
        0,
        rw_va,
        map_len,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    ) != 0
    {
        debug_write(b"drv-blk: rw map failed\n".as_ptr(), 23);
        kumo_rt::process_exit(1);
    }
    // Seed the private copy from the initrd. Both mappings are Normal-WB RAM, so a plain
    // byte copy is coherent.
    unsafe {
        core::ptr::copy_nonoverlapping(vmo_va as *const u8, rw_va as *mut u8, map_len as usize);
    }

    let dev = BlockDevice::new(vmo_len);
    // The private RW copy as a mutable byte slice — the backing store block I/O operates on.
    // `read_blocks`/`write_blocks` bound every access to `map_len`, so a request past the
    // mapped extent is a clean STATUS_BAD_LBA, never a fault.
    let store = unsafe { core::slice::from_raw_parts_mut(rw_va as *mut u8, map_len as usize) };
    debug_write(b"drv-blk: initialized (writable)\n".as_ptr(), 32);

    // Serve loop: read the request frame, perform block I/O, write the response. One buffer
    // holds either an inbound write frame (`[header][data]`) or an outbound read response;
    // they are never live at the same time.
    let mut buf = [0u8; 4096];
    loop {
        let n = channel_read(ch, buf.as_mut_ptr(), buf.len()) as usize;
        let request = match Request::decode(&buf[..n]) {
            Some(r) => r,
            None => continue, // empty/partial frame (e.g. a spurious wake)
        };

        match request.cmd {
            CMD_READ => match dev.read_blocks(store, request.lba, request.count) {
                Ok(data) => {
                    // `data` borrows `store`, which is disjoint from `buf`, so the response
                    // (status byte + block bytes) assembles into `buf` directly.
                    let copy_len = data.len().min(buf.len() - 1);
                    buf[1..1 + copy_len].copy_from_slice(&data[..copy_len]);
                    buf[0] = STATUS_OK;
                    channel_write(ch, buf.as_ptr(), 1 + copy_len);
                }
                Err(status) => {
                    buf[0] = status;
                    channel_write(ch, buf.as_ptr(), 1);
                }
            },
            CMD_WRITE => {
                // The data rides after the header in the same frame; apply it to the private
                // copy. `split_write_frame` borrows `buf`; the borrow ends before we write the
                // status byte back into `buf`.
                let status = match split_write_frame(&buf[..n]) {
                    Ok((req, data)) => dev.write_blocks(store, req.lba, req.count, data),
                    Err(s) => s,
                };
                buf[0] = status;
                channel_write(ch, buf.as_ptr(), 1);
            }
            CMD_FLUSH => {
                // RAM-backed: nothing to flush. Ack so a client barrier is a no-op.
                buf[0] = STATUS_OK;
                channel_write(ch, buf.as_ptr(), 1);
            }
            _ => {
                buf[0] = STATUS_BAD_LBA;
                channel_write(ch, buf.as_ptr(), 1);
            }
        }
    }
}
