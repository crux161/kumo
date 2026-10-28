#![no_std]
#![no_main]
//j648
//j649
//j655

//! Sora transfers a finite bootstrap channel (x0) and a writable surface VMO (x1).
//! Bootstrap grants one presenter and, optionally, the selected file as a read-only VMO.

use bmp_view::{file_length, Bitmap, FILE_HEADER_LEN, PRESENTER_TAG, ROW_BYTES, SAMPLE};
use kumo_abi::{Handle, VmarFlags};
use kumo_graphics::{DamageSet, Present, Reply, Status, DEMO, REPLY_LEN};
use kumo_rt::{
    channel_read, channel_read_with_handle, channel_write, handle_close, log_write, process_exit,
    vmar_map_anywhere, vmo_write,
};

kumo_rt::entry!(main);

fn fail(message: &[u8]) -> ! {
    let _ = log_write(kumo_abi::sys::log_level::ERR, message);
    process_exit(1)
}

#[no_mangle]
extern "C" fn main(
    channel: u64,
    surface: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
) -> ! {
    let bootstrap = Handle(channel as u32);
    let mut header = [0; FILE_HEADER_LEN + 1];
    let (n, file) = channel_read_with_handle(bootstrap, header.as_mut_ptr(), header.len());
    let len = header
        .get(..n)
        .and_then(file_length)
        .unwrap_or_else(|| fail(b"bmp-view: bad file grant\n"));
    let source = if len == 0 && file == 0 {
        SAMPLE
    } else if len != 0 && file != 0 && file != u64::MAX {
        let map_len = (len + 4095) & !4095;
        let (status, address) = vmar_map_anywhere(Handle(file as u32), map_len, VmarFlags::READ.0);
        if status != 0 || address == 0 {
            fail(b"bmp-view: file map failed\n");
        }
        // The selected file has no remaining writer; the read-only mapping lives until exit. — SAKER
        unsafe { core::slice::from_raw_parts(address as *const u8, len as usize) }
    } else {
        fail(b"bmp-view: bad file handle\n");
    };
    let (n, presenter) = channel_read_with_handle(bootstrap, header.as_mut_ptr(), header.len());
    if header.get(..n) != Some(PRESENTER_TAG) || presenter == 0 || presenter == u64::MAX {
        fail(b"bmp-view: no presentation channel\n");
    }
    let _ = handle_close(bootstrap);
    let channel = Handle(presenter as u32);
    let vmo = Handle(surface as u32);
    let bitmap = Bitmap::new(source).unwrap_or_else(|_| fail(b"bmp-view: invalid BMP\n"));
    let mut row = [0u8; ROW_BYTES];
    for y in 0..DEMO.height {
        bitmap
            .row(y, &mut row)
            .unwrap_or_else(|_| fail(b"bmp-view: decode failed\n"));
        for (index, chunk) in row.chunks(256).enumerate() {
            let offset = u64::from(y) * ROW_BYTES as u64 + (index * 256) as u64;
            if vmo_write(vmo, offset, chunk.as_ptr(), chunk.len()) != 0 {
                fail(b"bmp-view: surface write failed\n");
            }
        }
    }
    let sequence = 1;
    let request = Present {
        sequence,
        surface: DEMO,
        damage: DamageSet::FULL,
    }
    .encode();
    if channel_write(channel, request.as_ptr(), request.len()) != 0 {
        fail(b"bmp-view: present failed\n");
    }
    let mut bytes = [0u8; REPLY_LEN + 1];
    let len = channel_read(channel, bytes.as_mut_ptr(), bytes.len()) as usize;
    match bytes.get(..len).and_then(Reply::decode) {
        Some(reply) if reply.sequence == sequence && reply.status == Status::Copied => {
            let message = b"bmp-view: bitmap copied; buffer released (software GOP)\n";
            let _ = log_write(kumo_abi::sys::log_level::INFO, message);
            process_exit(0)
        }
        _ => fail(b"bmp-view: presenter rejected surface\n"),
    }
}
