#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use kumo_abi::{Errno, Handle, Rights, Status};
use kumo_ipc::Message;

pub mod sys {
    use super::*;

    pub fn handle_close(_handle: Handle) -> Status {
        Errno::NotSupported.status()
    }

    pub fn channel_write(_channel: Handle, _message: &Message<'_>) -> Status {
        Errno::NotSupported.status()
    }

    pub fn channel_call(_channel: Handle, _message: &Message<'_>) -> Status {
        Errno::NotSupported.status()
    }

    pub fn vmar_map(_vmar: Handle, _vmo: Handle, _rights: Rights) -> Result<usize, Errno> {
        Err(Errno::NotSupported)
    }
}

pub trait Server {
    fn name(&self) -> &'static str;
    fn dispatch(&mut self, message: Message<'_>) -> Status;
}

pub fn run_one<S: Server>(server: &mut S, message: Message<'_>) -> Status {
    server.dispatch(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoServer;

    impl Server for EchoServer {
        fn name(&self) -> &'static str {
            "echo"
        }

        fn dispatch(&mut self, _message: Message<'_>) -> Status {
            Errno::Ok.status()
        }
    }

    #[test]
    fn dispatches_one_message() {
        let msg = Message::new(1, b"hello", &[]).unwrap();
        let mut server = EchoServer;
        assert_eq!(run_one(&mut server, msg), Errno::Ok.status());
    }
}
