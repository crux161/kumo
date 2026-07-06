#![no_std]
#![no_main]

use drv_blk::{BlockDevice, Request, CMD_READ, CMD_WRITE, STATUS_BAD_LBA, STATUS_OK};
use kumo_abi::{Handle, VmarFlags};
use kumo_rt::{channel_read, channel_read_with_handle, channel_write, debug_write, vmar_map};

kumo_rt::entry!(main);

/// Block I/O request from the channel (11 bytes):
///   cmd:    u8  — 0x00 = read, 0x01 = write
///   lba:    u64 LE
///   count:  u16 LE
/// Read response: status(1) + data(count*512)
/// Write response: status(1)
const REQ_LEN: usize = 11;

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

    let dev = BlockDevice::new(vmo_len);
    // The mapped VMO as a byte slice — the backing store the block-device logic reads from.
    // Read-only for now (the write path lands with a private RW backing in the next slice);
    // `read_blocks` bounds every access to `map_len`, so a request past the mapped extent is a
    // clean STATUS_BAD_LBA, never a fault.
    let store = unsafe { core::slice::from_raw_parts(vmo_va as *const u8, map_len as usize) };
    debug_write(b"drv-blk: initialized\n".as_ptr(), 20);

    // Serve loop: read request, perform block I/O, write response.
    let mut req = [0u8; REQ_LEN];
    let mut buf = [0u8; 4096]; // response buffer
    loop {
        let n = channel_read(ch, req.as_mut_ptr(), req.len()) as usize;
        let request = match Request::decode(&req[..n]) {
            Some(r) => r,
            None => continue, // empty/partial frame (e.g. a spurious wake)
        };

        match request.cmd {
            CMD_READ => match dev.read_blocks(store, request.lba, request.count) {
                Ok(data) => {
                    buf[0] = STATUS_OK;
                    let copy_len = data.len().min(buf.len() - 1);
                    buf[1..1 + copy_len].copy_from_slice(&data[..copy_len]);
                    channel_write(ch, buf.as_ptr(), 1 + copy_len);
                }
                Err(status) => {
                    buf[0] = status;
                    channel_write(ch, buf.as_ptr(), 1);
                }
            },
            CMD_WRITE => {
                // Read-only ramdisk for now: acknowledge in-range writes but ignore data.
                // The real write path (a private RW backing) lands in the next slice; until
                // then this preserves the historical "ack but discard" behavior.
                buf[0] = if dev.check_bounds(request.lba, request.count as u64) {
                    STATUS_OK
                } else {
                    STATUS_BAD_LBA
                };
                channel_write(ch, buf.as_ptr(), 1);
            }
            _ => {
                buf[0] = STATUS_BAD_LBA;
                channel_write(ch, buf.as_ptr(), 1);
            }
        }
    }
}
