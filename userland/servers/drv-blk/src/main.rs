#![no_std]
#![no_main]

//j892
//j913
//j690
//j928
//j930

use drv_blk::{
    split_write_frame, BlockDevice, Request, CMD_FLUSH, CMD_READ, CMD_READ_VMO, CMD_WRITE,
    STATUS_BAD_LBA, STATUS_OK, STATUS_UNSUPPORTED,
};
use kumo_abi::{Handle, VmarFlags};
use kumo_rt::{
    channel_read_with_handle, channel_write, handle_close, handle_koid, port_bind, port_create,
    port_unbind, port_wait, vmar_map, vmo_create,
};

/// Diagnostics go to the kernel log (`dmesg`) since J559: shown on the console only when its level
/// allows. Failures are ERR, so a default boot still shows them.
fn log_info(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::INFO, message);
}

fn log_err(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::ERR, message);
}

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
    log_info(b"drv-blk starting\n");

    let ch = Handle(bootstrap_channel as u32);

    // Read the initrd VMO handle + 8-byte size from the bootstrap channel. SH8-b: or a
    // device-mode bootstrap, whose VMO is the volume itself.
    let mut boot = [0u8; 16];
    let (n, vmo_raw) = channel_read_with_handle(ch, boot.as_mut_ptr(), boot.len());
    let vmo = Handle(vmo_raw as u32);
    if vmo_raw == 0 || vmo_raw == u64::MAX {
        log_err(b"drv-blk: missing vmo\n");
        kumo_rt::process_exit(1);
    }
    if let Some(blocks) = drv_blk::decode_device_bootstrap(&boot[..n.min(boot.len())]) {
        serve_device(ch, vmo, blocks);
    }
    let vmo_len = if n == 8 {
        u64::from_le_bytes(boot[..8].try_into().unwrap())
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
    // j913: offset into this architecture's user window (x86 programs live at 0x80_0000_0000).
    let vmo_va = kumo_rt::USER_WINDOW_BASE + 0x0000_0000_1100_0000;
    if vmar_map(Handle(0), vmo, 0, vmo_va, map_len, (VmarFlags::READ).0) != 0 {
        log_err(b"drv-blk: map failed\n");
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
        log_err(b"drv-blk: rw vmo create failed\n");
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
        log_err(b"drv-blk: rw map failed\n");
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
    log_info(b"drv-blk: initialized (writable)\n");
    kumo_rt::boot_ok(&[b"drv-blk: online, initrd block device\n"]);

    // Serve loop: read the request frame, perform block I/O, write the response. One buffer
    // holds either an inbound write frame (`[header][data]`) or an outbound read response;
    // they are never live at the same time.
    let mut buf = [0u8; 4096];
    loop {
        let (n, carried) = channel_read_with_handle(ch, buf.as_mut_ptr(), buf.len());
        // SH7-d3b: this RAM disk serves CMD_READ_VMO as unsupported; never keep a VMO.
        if carried != 0 && carried != u64::MAX {
            let _ = kumo_rt::handle_close(Handle(carried as u32));
        }
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
            // j930: neither bulk command on the legacy ramdisk; clients fall back to blocks.
            CMD_READ_VMO | drv_blk::CMD_WRITE_VMO => {
                buf[0] = STATUS_UNSUPPORTED;
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

/// The largest volume device mode maps: it must fit below the child-VMAR boundary at
/// 0x2000_0000 above the 0x1100_0000 base.
const MAX_DEVICE_BYTES: u64 = 192 * 1024 * 1024;
/// Clients one device serves at once (a mount-scoped filesystem server holds one).
const MAX_DEVICE_CLIENTS: usize = 4;

/// SH8-b device mode: serve the VMO in place as an offered volume. The bootstrap channel is the
/// control channel: drv-blk announces the volume there and Sora connects clients through it.
/// The VMO is the volume, so the bytes outlive every client and this driver. — TEAL
fn serve_device(control: Handle, volume: Handle, blocks: u64) -> ! {
    let Some(bytes) = blocks
        .checked_mul(drv_blk::BLOCK_SIZE)
        .filter(|b| *b <= MAX_DEVICE_BYTES && b % 0x1000 == 0)
    else {
        log_err(b"drv-blk: device volume size refused\n");
        kumo_rt::process_exit(1);
    };
    let va = kumo_rt::USER_WINDOW_BASE + 0x0000_0000_1100_0000;
    if vmar_map(
        Handle(0),
        volume,
        0,
        va,
        bytes,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    ) != 0
    {
        log_err(b"drv-blk: device volume map failed\n");
        kumo_rt::process_exit(1);
    }
    // SAFETY: the kernel mapped `bytes` writable bytes at `va` for this process alone.
    let store = unsafe { core::slice::from_raw_parts_mut(va as *mut u8, bytes as usize) };
    let port = port_create();
    let control_koid = handle_koid(control);
    if port == 0 || port == u64::MAX || control_koid == u64::MAX {
        log_err(b"drv-blk: device port failed\n");
        kumo_rt::process_exit(1);
    }
    let port = Handle(port as u32);
    if port_bind(port, control) != 0 {
        log_err(b"drv-blk: device control bind failed\n");
        kumo_rt::process_exit(1);
    }
    let ready = drv_blk::MediaReady {
        block_count: blocks,
        serial: drv_blk::WORK_SERIAL,
        label: Some(drv_blk::WORK_LABEL),
        read_only: false,
    }
    .encode();
    if channel_write(control, ready.as_ptr(), ready.len()) != 0 {
        log_err(b"drv-blk: device announce failed\n");
        kumo_rt::process_exit(1);
    }
    log_info(b"drv-blk: device volume offered\n");
    // (channel, koid, read_only)
    let mut clients: [Option<(Handle, u64, bool)>; MAX_DEVICE_CLIENTS] = [None; MAX_DEVICE_CLIENTS];
    let mut frame = [0u8; drv_blk::REQUEST_LEN + drv_blk::BLOCK_SIZE as usize + 16];
    let mut reply = [0u8; 1 + drv_blk::BLOCK_SIZE as usize];
    loop {
        let source = port_wait(port);
        if source == control_koid {
            let mut msg = [0u8; drv_blk::CONTROL_CONNECT_LEN];
            let (n, raw) = channel_read_with_handle(control, msg.as_mut_ptr(), msg.len());
            let carried = (raw != 0 && raw != u64::MAX).then_some(Handle(raw as u32));
            if n == 0 {
                // Sora is gone. The clients keep their channels until they close them.
                let _ = port_unbind(port, control);
                continue;
            }
            let (Some(access), Some(channel)) =
                (drv_blk::decode_connect(&msg[..n.min(msg.len())]), carried)
            else {
                if let Some(extra) = carried {
                    let _ = handle_close(extra);
                }
                continue;
            };
            let koid = handle_koid(channel);
            let slot = clients.iter().position(Option::is_none);
            match slot {
                Some(slot) if koid != u64::MAX && port_bind(port, channel) == 0 => {
                    clients[slot] = Some((channel, koid, access == drv_blk::BlockAccess::ReadOnly));
                }
                _ => {
                    log_err(b"drv-blk: device client refused\n");
                    let _ = handle_close(channel);
                }
            }
        } else if let Some(slot) = clients
            .iter()
            .position(|c| c.is_some_and(|(_, koid, _)| koid == source))
        {
            let Some((channel, _, read_only)) = clients[slot] else {
                continue;
            };
            let (n, raw) = channel_read_with_handle(channel, frame.as_mut_ptr(), frame.len());
            if raw != 0 && raw != u64::MAX {
                let _ = handle_close(Handle(raw as u32));
            }
            if n == 0 || n > frame.len() {
                let _ = port_unbind(port, channel);
                let _ = handle_close(channel);
                clients[slot] = None;
                continue;
            }
            let len = drv_blk::serve_device_frame(store, &frame[..n], read_only, &mut reply);
            if len != 0 {
                let _ = channel_write(channel, reply.as_ptr(), len);
            }
        }
    }
}
