#![no_std]
#![no_main]

use core::arch::asm;

fn exit(code: usize) -> ! {
    unsafe { asm!("svc #0", in("x8") 21usize, in("x0") code, options(noreturn)) }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let line = b"SH7-f hello from KUMO\n";
    unsafe {
        asm!("svc #0", in("x8") 30usize, inout("x0") line.as_ptr() => _,
             in("x1") line.len(), options(nostack));
    }
    exit(0)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    exit(1)
}
