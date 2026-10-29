#![no_std]
#![no_main]
//j669

//! An input consumer for the focused view (F2b focus routing). Sora transfers one input
//! channel in `x0`; this program blocks on it, logs each received byte, and exits when the
//! channel closes. It presents nothing: the view's chrome is drv-fb's to paint.

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
    let mut buf = [0u8; 16];
    let mut logged = 0u32;
    loop {
        let n = channel_read(channel, buf.as_mut_ptr(), buf.len()) as usize;
        if n == 0 {
            process_exit(0);
        }
        for byte in buf.get(..n).unwrap_or(&[]) {
            if logged < 64 {
                let mut line = *b"gfx-keys: key 0x00\n";
                line[16] = hex_digit(byte >> 4);
                line[17] = hex_digit(byte & 0x0f);
                let _ = log_write(kumo_abi::sys::log_level::INFO, &line);
                logged += 1;
            }
        }
    }
}
