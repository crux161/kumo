#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//! `persona-posix` - native POSIX personality scaffolding.
//!
//! This crate owns the POSIX-facing file-descriptor table shape. There is no
//! kernel fd: each integer fd indexes a userspace slot containing a typed
//! capability client. This first slice proves stdio slots route to a TTY
//! transport without granting ambient authority.

pub const STDIN_FILENO: i32 = 0;
pub const STDOUT_FILENO: i32 = 1;
pub const STDERR_FILENO: i32 = 2;
pub const MAX_FDS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PosixErrno {
    BadFd,
    NoDevice,
}

pub type PosixResult<T> = core::result::Result<T, PosixErrno>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TtyStream {
    pub handle: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fd {
    Closed,
    Null,
    Tty(TtyStream),
}

pub trait TtyWrite {
    fn write(&mut self, stream: TtyStream, bytes: &[u8]) -> PosixResult<usize>;
}

pub trait TtyRead {
    fn read(&mut self, stream: TtyStream, out: &mut [u8]) -> PosixResult<usize>;
}

pub trait TtyRpcTransport {
    fn call(&mut self, stream: TtyStream, request: &[u8], reply: &mut [u8]) -> PosixResult<usize>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TtyRpc<T> {
    transport: T,
}

impl<T> TtyRpc<T> {
    pub const fn new(transport: T) -> TtyRpc<T> {
        TtyRpc { transport }
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn into_inner(self) -> T {
        self.transport
    }
}

impl<T: TtyRpcTransport> TtyWrite for TtyRpc<T> {
    fn write(&mut self, stream: TtyStream, bytes: &[u8]) -> PosixResult<usize> {
        let mut request = [0u8; ttyd::REQUEST_BUF_BYTES];
        let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
        for &byte in bytes {
            let Some(request_len) = ttyd::Request::write(byte).encode_into(&mut request) else {
                return Err(PosixErrno::NoDevice);
            };
            let reply_len = self
                .transport
                .call(stream, &request[..request_len], &mut reply)?;
            let Some(parsed) = ttyd::Reply::parse(&reply[..reply_len]) else {
                return Err(PosixErrno::NoDevice);
            };
            if parsed.status != ttyd::TTY_OK || parsed.echo != [byte] || parsed.line.is_some() {
                return Err(PosixErrno::NoDevice);
            }
        }
        Ok(bytes.len())
    }
}

impl<T: TtyRpcTransport> TtyRead for TtyRpc<T> {
    fn read(&mut self, stream: TtyStream, out: &mut [u8]) -> PosixResult<usize> {
        let mut request = [0u8; ttyd::REQUEST_BUF_BYTES];
        let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
        let Some(request_len) = ttyd::Request::read().encode_into(&mut request) else {
            return Err(PosixErrno::NoDevice);
        };
        let reply_len = self
            .transport
            .call(stream, &request[..request_len], &mut reply)?;
        let Some(parsed) = ttyd::Reply::parse(&reply[..reply_len]) else {
            return Err(PosixErrno::NoDevice);
        };
        if parsed.status != ttyd::TTY_OK || !parsed.echo.is_empty() {
            return Err(PosixErrno::NoDevice);
        }
        let Some(line) = parsed.line else {
            return Ok(0);
        };
        let n = line.len().min(out.len());
        out[..n].copy_from_slice(&line[..n]);
        Ok(n)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FdTable {
    slots: [Fd; MAX_FDS],
}

impl FdTable {
    pub const fn empty() -> FdTable {
        FdTable {
            slots: [Fd::Closed; MAX_FDS],
        }
    }

    pub const fn with_stdio(tty: TtyStream) -> FdTable {
        let mut table = FdTable::empty();
        table.slots[STDIN_FILENO as usize] = Fd::Tty(tty);
        table.slots[STDOUT_FILENO as usize] = Fd::Tty(tty);
        table.slots[STDERR_FILENO as usize] = Fd::Tty(tty);
        table
    }

    pub fn get(&self, fd: i32) -> PosixResult<Fd> {
        let index = fd_index(fd)?;
        match self.slots[index] {
            Fd::Closed => Err(PosixErrno::BadFd),
            entry => Ok(entry),
        }
    }

    pub fn install(&mut self, fd: i32, entry: Fd) -> PosixResult<()> {
        let index = fd_index(fd)?;
        self.slots[index] = entry;
        Ok(())
    }

    pub fn close(&mut self, fd: i32) -> PosixResult<()> {
        let index = fd_index(fd)?;
        if self.slots[index] == Fd::Closed {
            return Err(PosixErrno::BadFd);
        }
        self.slots[index] = Fd::Closed;
        Ok(())
    }

    pub fn write(&mut self, fd: i32, bytes: &[u8], tty: &mut impl TtyWrite) -> PosixResult<usize> {
        match self.get(fd)? {
            Fd::Tty(stream) => tty.write(stream, bytes),
            Fd::Null => Ok(bytes.len()),
            Fd::Closed => Err(PosixErrno::BadFd),
        }
    }

    pub fn read(&mut self, fd: i32, out: &mut [u8], tty: &mut impl TtyRead) -> PosixResult<usize> {
        match self.get(fd)? {
            Fd::Tty(stream) => tty.read(stream, out),
            Fd::Null => Ok(0),
            Fd::Closed => Err(PosixErrno::BadFd),
        }
    }
}

fn fd_index(fd: i32) -> PosixResult<usize> {
    let Ok(index) = usize::try_from(fd) else {
        return Err(PosixErrno::BadFd);
    };
    if index < MAX_FDS {
        Ok(index)
    } else {
        Err(PosixErrno::BadFd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTty {
        stream: Option<TtyStream>,
        bytes: [u8; 32],
        len: usize,
    }

    impl TtyWrite for FakeTty {
        fn write(&mut self, stream: TtyStream, bytes: &[u8]) -> PosixResult<usize> {
            self.stream = Some(stream);
            let n = bytes.len().min(self.bytes.len());
            self.bytes[..n].copy_from_slice(&bytes[..n]);
            self.len = n;
            Ok(bytes.len())
        }
    }

    impl TtyRead for FakeTty {
        fn read(&mut self, stream: TtyStream, _out: &mut [u8]) -> PosixResult<usize> {
            self.stream = Some(stream);
            Ok(0)
        }
    }

    fn dispatch_request<const N: usize>(
        server: &mut ttyd::TtyServer<N>,
        request: ttyd::Request,
        reply: &mut [u8],
    ) {
        let mut frame = [0u8; ttyd::REQUEST_BUF_BYTES];
        let len = request.encode_into(&mut frame).unwrap();
        server.dispatch(&frame[..len], reply);
    }

    #[test]
    fn stdio_slots_are_tty_streams() {
        let tty = TtyStream { handle: 7 };
        let table = FdTable::with_stdio(tty);
        assert_eq!(table.get(STDIN_FILENO), Ok(Fd::Tty(tty)));
        assert_eq!(table.get(STDOUT_FILENO), Ok(Fd::Tty(tty)));
        assert_eq!(table.get(STDERR_FILENO), Ok(Fd::Tty(tty)));
    }

    #[test]
    fn write_stdout_uses_tty_transport() {
        let tty = TtyStream { handle: 11 };
        let mut table = FdTable::with_stdio(tty);
        let mut fake = FakeTty::default();
        assert_eq!(table.write(STDOUT_FILENO, b"hello", &mut fake), Ok(5));
        assert_eq!(fake.stream, Some(tty));
        assert_eq!(&fake.bytes[..fake.len], b"hello");
    }

    #[test]
    fn bad_fd_is_rejected() {
        let mut table = FdTable::empty();
        let mut fake = FakeTty::default();
        assert_eq!(table.get(-1), Err(PosixErrno::BadFd));
        assert_eq!(table.get(MAX_FDS as i32), Err(PosixErrno::BadFd));
        assert_eq!(
            table.write(STDOUT_FILENO, b"x", &mut fake),
            Err(PosixErrno::BadFd)
        );
    }

    #[test]
    fn close_drops_slot() {
        let tty = TtyStream { handle: 3 };
        let mut table = FdTable::with_stdio(tty);
        assert_eq!(table.close(STDOUT_FILENO), Ok(()));
        assert_eq!(table.get(STDOUT_FILENO), Err(PosixErrno::BadFd));
        assert_eq!(table.close(STDOUT_FILENO), Err(PosixErrno::BadFd));
    }

    #[test]
    fn null_fd_discards_successfully() {
        let mut table = FdTable::empty();
        let mut fake = FakeTty::default();
        assert_eq!(table.install(4, Fd::Null), Ok(()));
        assert_eq!(table.write(4, b"ignored", &mut fake), Ok(7));
        assert_eq!(fake.stream, None);

        let mut out = [0u8; 8];
        assert_eq!(table.read(4, &mut out, &mut fake), Ok(0));
    }

    struct FakeTtyRpc {
        server: ttyd::TtyServer<8>,
        last_stream: Option<TtyStream>,
        calls: usize,
        last_request: [u8; ttyd::REQUEST_BUF_BYTES],
        last_request_len: usize,
    }

    impl FakeTtyRpc {
        fn new() -> FakeTtyRpc {
            FakeTtyRpc {
                server: ttyd::TtyServer::new(),
                last_stream: None,
                calls: 0,
                last_request: [0; ttyd::REQUEST_BUF_BYTES],
                last_request_len: 0,
            }
        }
    }

    impl TtyRpcTransport for FakeTtyRpc {
        fn call(
            &mut self,
            stream: TtyStream,
            request: &[u8],
            reply: &mut [u8],
        ) -> PosixResult<usize> {
            self.last_stream = Some(stream);
            self.calls += 1;
            self.last_request_len = request.len();
            self.last_request[..request.len()].copy_from_slice(request);
            Ok(self.server.dispatch(request, reply))
        }
    }

    #[test]
    fn tty_rpc_write_sends_write_frames() {
        let stream = TtyStream { handle: 9 };
        let mut table = FdTable::with_stdio(stream);
        let mut tty = TtyRpc::new(FakeTtyRpc::new());

        assert_eq!(table.write(STDOUT_FILENO, b"ok", &mut tty), Ok(2));

        let inner = tty.transport();
        assert_eq!(inner.calls, 2);
        assert_eq!(inner.last_stream, Some(stream));
        assert_eq!(
            ttyd::Request::decode(&inner.last_request[..inner.last_request_len]),
            Some(ttyd::Request::Write(b'k'))
        );
        assert_eq!(inner.server.pending(), b"");
    }

    struct BadTtyRpc;

    impl TtyRpcTransport for BadTtyRpc {
        fn call(
            &mut self,
            _stream: TtyStream,
            _request: &[u8],
            reply: &mut [u8],
        ) -> PosixResult<usize> {
            reply[0] = ttyd::TTY_BAD_REQUEST;
            reply[1] = ttyd::EVENT_NONE;
            reply[2] = 0;
            reply[3] = 0;
            reply[4] = 0;
            Ok(ttyd::REPLY_HEADER_BYTES)
        }
    }

    #[test]
    fn tty_rpc_write_rejects_bad_reply() {
        let stream = TtyStream { handle: 4 };
        let mut table = FdTable::with_stdio(stream);
        let mut tty = TtyRpc::new(BadTtyRpc);

        assert_eq!(
            table.write(STDOUT_FILENO, b"x", &mut tty),
            Err(PosixErrno::NoDevice)
        );
    }

    #[test]
    fn tty_rpc_read_receives_submitted_line_once() {
        let stream = TtyStream { handle: 12 };
        let mut table = FdTable::with_stdio(stream);
        let mut tty = TtyRpc::new(FakeTtyRpc::new());
        let mut scratch = [0u8; ttyd::REPLY_BUF_BYTES];
        dispatch_request(
            &mut tty.transport_mut().server,
            ttyd::Request::input(b'o'),
            &mut scratch,
        );
        dispatch_request(
            &mut tty.transport_mut().server,
            ttyd::Request::input(b'k'),
            &mut scratch,
        );
        dispatch_request(
            &mut tty.transport_mut().server,
            ttyd::Request::input(b'\n'),
            &mut scratch,
        );

        let mut out = [0u8; 8];
        assert_eq!(table.read(STDIN_FILENO, &mut out, &mut tty), Ok(2));
        assert_eq!(&out[..2], b"ok");
        assert_eq!(table.read(STDIN_FILENO, &mut out, &mut tty), Ok(0));
    }

    #[test]
    fn tty_rpc_read_truncates_to_output_buffer() {
        let stream = TtyStream { handle: 13 };
        let mut table = FdTable::with_stdio(stream);
        let mut tty = TtyRpc::new(FakeTtyRpc::new());
        let mut scratch = [0u8; ttyd::REPLY_BUF_BYTES];
        for &byte in b"long\n" {
            dispatch_request(
                &mut tty.transport_mut().server,
                ttyd::Request::input(byte),
                &mut scratch,
            );
        }

        let mut out = [0u8; 2];
        assert_eq!(table.read(STDIN_FILENO, &mut out, &mut tty), Ok(2));
        assert_eq!(&out, b"lo");
    }
}
