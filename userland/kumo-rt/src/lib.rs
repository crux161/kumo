#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use core::panic::PanicInfo;
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

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    sys::process_exit(1);
}

#[macro_export]
macro_rules! entry {
    ($path:ident) => {
        core::arch::global_asm!(
            ".section .text._start, \"ax\"",
            ".global _start",
            "_start:",
            concat!("  bl  ", stringify!($path)),
            "1: b 1b",
        );
    };
}

#[global_allocator]
static ALLOC: heap::KumoHeap = heap::KumoHeap::empty();

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
