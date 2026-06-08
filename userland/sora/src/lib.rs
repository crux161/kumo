#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use kumo_abi::{Errno, Status};
use kumo_ipc::{Message, MessageError};

pub const SORA_NAME: &str = "Sora";
pub const ROOT_SERVER_ORDINAL_ECHO: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartPolicy {
    Never,
    OnFailure,
    Always,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerRecipe<'a> {
    pub name: &'a str,
    pub image_path: &'a str,
    pub restart: RestartPolicy,
}

pub struct Sora;

impl Sora {
    pub const fn new() -> Self {
        Self
    }

    pub fn echo<'a>(&self, request: Message<'a>) -> Result<Message<'a>, MessageError> {
        Message::new(ROOT_SERVER_ORDINAL_ECHO, request.bytes, request.handles)
    }
}

impl kumo_rt::Server for Sora {
    fn name(&self) -> &'static str {
        SORA_NAME
    }

    fn dispatch(&mut self, _message: Message<'_>) -> Status {
        Errno::Ok.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_message_payload_shape() {
        let sora = Sora::new();
        let request = Message::call(ROOT_SERVER_ORDINAL_ECHO, b"ping", &[]).unwrap();
        let reply = sora.echo(request).unwrap();
        assert_eq!(reply.bytes, b"ping");
        assert_eq!(reply.header.ordinal, ROOT_SERVER_ORDINAL_ECHO);
    }
}
