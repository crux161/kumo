#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use kumo_abi::Handle;

pub const MAX_INLINE_BYTES: usize = 4096;
pub const MAX_MESSAGE_HANDLES: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MessageHeader {
    pub ordinal: u32,
    pub flags: u32,
    pub bytes_len: u32,
    pub handles_len: u32,
}

impl MessageHeader {
    pub const FLAG_EXPECTS_REPLY: u32 = 1 << 0;
    pub const FLAG_IS_REPLY: u32 = 1 << 1;

    pub const fn new(ordinal: u32, bytes_len: u32, handles_len: u32) -> Self {
        Self {
            ordinal,
            flags: 0,
            bytes_len,
            handles_len,
        }
    }

    pub const fn expects_reply(mut self) -> Self {
        self.flags |= Self::FLAG_EXPECTS_REPLY;
        self
    }

    pub const fn is_reply(mut self) -> Self {
        self.flags |= Self::FLAG_IS_REPLY;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageError {
    TooManyBytes,
    TooManyHandles,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Message<'a> {
    pub header: MessageHeader,
    pub bytes: &'a [u8],
    pub handles: &'a [Handle],
}

impl<'a> Message<'a> {
    pub fn new(ordinal: u32, bytes: &'a [u8], handles: &'a [Handle]) -> Result<Self, MessageError> {
        if bytes.len() > MAX_INLINE_BYTES {
            return Err(MessageError::TooManyBytes);
        }

        if handles.len() > MAX_MESSAGE_HANDLES {
            return Err(MessageError::TooManyHandles);
        }

        Ok(Self {
            header: MessageHeader::new(ordinal, bytes.len() as u32, handles.len() as u32),
            bytes,
            handles,
        })
    }

    pub fn call(
        ordinal: u32,
        bytes: &'a [u8],
        handles: &'a [Handle],
    ) -> Result<Self, MessageError> {
        let mut msg = Self::new(ordinal, bytes, handles)?;
        msg.header = msg.header.expects_reply();
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_bytes_and_handles() {
        let handles = [Handle(7), Handle(9)];
        let msg = Message::call(42, b"ping", &handles).unwrap();
        assert_eq!(msg.header.ordinal, 42);
        assert_eq!(msg.header.bytes_len, 4);
        assert_eq!(msg.header.handles_len, 2);
        assert_eq!(msg.header.flags & MessageHeader::FLAG_EXPECTS_REPLY, 1);
    }
}
