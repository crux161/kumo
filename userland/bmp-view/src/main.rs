#![no_std]
#![no_main]
//j648

//! Sora transfers only a presentation channel (x0) and its writable surface VMO (x1).

use bmp_view::{Bitmap, ROW_BYTES, SAMPLE};
use kumo_abi::Handle;
use kumo_graphics::{Present, Reply, Status, DEMO, REPLY_LEN};
use kumo_rt::{channel_read, channel_write, log_write, process_exit, vmo_write};

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
    let channel = Handle(channel as u32);
    let vmo = Handle(surface as u32);
    let bitmap = Bitmap::new(SAMPLE).unwrap_or_else(|_| fail(b"bmp-view: invalid BMP\n"));
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
