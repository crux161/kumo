#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use kumo_abi::{Handle, Status};

mod sys;

pub use sys::*;

pub trait Server {
    fn name(&self) -> &'static str;
    fn dispatch(&mut self, channel: Handle, message: &[u8]) -> Status;
}

pub fn run_one<S: Server>(server: &mut S, channel: Handle, message: &[u8]) -> Status {
    server.dispatch(channel, message)
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
