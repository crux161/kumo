#![no_std]
#![no_main]
//j569
//j655
//j684

//! A separate-process presentation client (F1b): Sora transfers one presentation channel in
//! `x0` and its surface VMO in `x1`. The client writes the diagnostic image, presents, waits
//! for the matching reply, and exits. It never names a screen coordinate or framebuffer address.

use kumo_abi::Handle;
use kumo_graphics::{demo_pixel, DamageSet, Present, Reply, Status, DEMO, REPLY_LEN};
use kumo_rt::{channel_read, channel_write, log_write, process_exit, vmo_write};

kumo_rt::entry!(main);

fn log(message: &[u8]) {
    let _ = log_write(kumo_abi::sys::log_level::INFO, message);
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
    let mut row = [0u8; 256 * 4];
    for y in 0..DEMO.height {
        for x in 0..DEMO.width {
            let offset = x as usize * 4;
            row[offset..offset + 4].copy_from_slice(&demo_pixel(x, y).to_le_bytes());
        }
        for (chunk_index, chunk) in row.chunks(256).enumerate() {
            let offset = u64::from(y * DEMO.stride * 4) + (chunk_index * 256) as u64;
            if vmo_write(vmo, offset, chunk.as_ptr(), chunk.len()) != 0 {
                log(b"gfx-client: surface write failed\n");
                process_exit(1);
            }
        }
    }
    let sequence = 1;
    let frame = Present {
        sequence,
        surface: DEMO,
        damage: DamageSet::FULL,
    }
    .encode();
    if channel_write(channel, frame.as_ptr(), frame.len()) != 0 {
        log(b"gfx-client: present failed\n");
        process_exit(1);
    }
    let mut reply = [0u8; REPLY_LEN + 1];
    let len = channel_read(channel, reply.as_mut_ptr(), reply.len()) as usize;
    match reply.get(..len).and_then(Reply::decode) {
        Some(reply)
            if reply.sequence == sequence
                && matches!(reply.status, Status::Retired | Status::Copied) =>
        {
            // The status names exactly what the server proved: retirement on the glass, or
            // the honest copy fallback. The log line mirrors it. — TIMBERDOODLE
            if reply.status == Status::Retired {
                log(b"gfx-client: 256x128 surface retired; frame proven on the scanout\n");
            } else {
                log(b"gfx-client: 256x128 surface copied; buffer released (software GOP)\n");
            }
            process_exit(0)
        }
        _ => {
            log(b"gfx-client: presenter rejected surface\n");
            process_exit(1)
        }
    }
}
