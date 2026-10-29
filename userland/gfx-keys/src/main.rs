#![no_std]
#![no_main]
//j669
//j677

//! An input consumer for the focused view (F2b focus routing). Sora transfers one input
//! channel in `x0` and names the view as the channel's first record; this program blocks on
//! the channel, logs each later byte with its view so focus routing is attributable, and
//! exits when the channel closes. It presents nothing: the view's chrome is drv-fb's to
//! paint.

use kumo_abi::Handle;
use kumo_rt::{channel_read, log_write, process_exit};

kumo_rt::entry!(main);

fn hex_digit(value: u8) -> u8 {
    if value < 10 {
        b'0' + value
    } else {
        b'a' + value - 10
    }
}

#[no_mangle]
extern "C" fn main(
    channel: u64,
    _a1: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
) -> ! {
    let channel = Handle(channel as u32);
    let _ = log_write(kumo_abi::sys::log_level::NOTICE, b"gfx-keys: online\n");
    // Sora names the view as the first record on the channel; a plain ProcessRun argument
    // cannot carry it safely (a value colliding with a live handle is staged as a handle
    // and arrives as the child-side value). — TIMBERDOODLE
    let mut first = [0u8; 16];
    let n = channel_read(channel, first.as_mut_ptr(), first.len()) as usize;
    if n == 0 {
        process_exit(0);
    }
    let view = u64::from(first[0]);
    let mut buf = [0u8; 16];
    let mut logged = 0u32;
    loop {
        let n = channel_read(channel, buf.as_mut_ptr(), buf.len()) as usize;
        if n == 0 {
            process_exit(0);
        }
        for byte in buf.get(..n).unwrap_or(&[]) {
            if logged < 64 {
                let mut line = *b"gfx-keys: view 0 key 0x00\n";
                if view < 10 {
                    line[15] = b'0' + view as u8;
                }
                line[23] = hex_digit(byte >> 4);
                line[24] = hex_digit(byte & 0x0f);
                let _ = log_write(kumo_abi::sys::log_level::INFO, &line);
                logged += 1;
            }
        }
    }
}
