#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[used]
#[link_section = ".rodata.sora"]
static SORA_IDENTITY: &[u8] = sora::SORA_NAME.as_bytes();

#[no_mangle]
pub extern "C" fn _start() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
