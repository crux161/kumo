#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[used]
#[link_section = ".rodata.sora"]
static SORA_IDENTITY: &[u8] = sora::SORA_NAME.as_bytes();

core::arch::global_asm!(
    ".section .rodata.sora_msg, \"a\"",
    ".balign 4",
    "sora_msg:",
    "  .ascii \"hello from Sora via SVC\\n\"", // 24
    "sora_ack:",
    "  .ascii \"sora ack\\n\"", // 9
    ".section .text._start, \"ax\"",
    ".global _start",
    ".balign 4",
    "_start:",
    "  mov  x19, x0", // x0 at entry = bootstrap root-channel handle; stash it
    // Greeting.
    "  adr  x0, sora_msg",
    "  movz x1, #24",
    "  movz x8, #29", // DebugWrite
    "  svc  #0",
    // Read the kernel's boot message into a 64-byte stack scratch buffer.
    "  sub  sp, sp, #64",
    "  mov  x0, x19", // root handle
    "  mov  x1, sp",  // dst
    "  movz x2, #64", // capacity
    "  movz x8, #5",  // ChannelRead -> x0 = bytes read
    "  svc  #0",
    // Echo what we received (proves kernel -> Sora delivery).
    "  mov  x1, x0",  // len = bytes read
    "  mov  x0, sp",  // ptr
    "  movz x8, #29", // DebugWrite
    "  svc  #0",
    "  add  sp, sp, #64",
    // Reply down the root channel.
    "  mov  x0, x19",
    "  adr  x1, sora_ack",
    "  movz x2, #9", // "sora ack\n"
    "  movz x8, #4", // ChannelWrite
    "  svc  #0",
    // Exit.
    "  movz x0, #0",
    "  movz x8, #21", // ProcessExit
    "  svc  #0",
    "1: b 1b",
);

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
