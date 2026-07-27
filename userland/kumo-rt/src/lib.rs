#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use kumo_abi::{Handle, Status};

pub mod heap;
pub mod sys;

pub use sys::*;

pub trait Server {
    fn name(&self) -> &'static str;
    fn dispatch(&mut self, channel: Handle, message: &[u8]) -> Status;
}

pub fn run_one<S: Server>(server: &mut S, channel: Handle, message: &[u8]) -> Status {
    server.dispatch(channel, message)
}

pub fn init() {
    // KumoHeap auto-initializes on first allocation, so this is a no-op marker
    // for future explicit initialization if we switch to VMO-backed heaps.
}

#[cfg(all(not(test), target_os = "none"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    sys::process_exit(1);
}

#[macro_export]
macro_rules! entry {
    ($path:ident) => {
        #[cfg(target_arch = "aarch64")]
        core::arch::global_asm!(
            ".section .text._start, \"ax\"",
            ".global _start",
            "_start:",
            concat!("  bl  ", stringify!($path)),
            "1: b 1b",
        );

        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        core::arch::global_asm!(
            ".section .text._start, \"ax\"",
            ".global _start",
            "_start:",
            // The architecture-neutral program ABI supplies eight bootstrap words. AMD64
            // carries the first six in registers and finds the final two on the stack.
            "  subq $16, %rsp",
            "  movq $0, 0(%rsp)",
            "  movq $0, 8(%rsp)",
            concat!("  call ", stringify!($path)),
            "1: jmp 1b",
            options(att_syntax),
        );
    };
}

#[global_allocator]
static ALLOC: heap::KumoHeap = heap::KumoHeap::empty();

/// How many regions back this process's heap. `1` is the bootstrap floor alone; anything more means
/// the growth path ran, which is the only thing that distinguishes a heap that *can* grow from one
/// that merely says so.
pub fn heap_region_count() -> usize {
    ALLOC.region_count()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoServer;

    impl Server for EchoServer {
        fn name(&self) -> &'static str {
            "echo"
        }

        fn dispatch(&mut self, _channel: Handle, _message: &[u8]) -> Status {
            kumo_abi::Errno::Ok.status()
        }
    }

    #[test]
    fn dispatches_one_message() {
        let mut server = EchoServer;
        assert_eq!(
            run_one(&mut server, Handle(0), b"hello"),
            kumo_abi::Errno::Ok.status()
        );
    }
}
