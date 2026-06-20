#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//! `ttyd` - the P8 TTY line-discipline core.
//!
//! This crate is the host-testable center of the future userspace terminal server.
//! It owns no device and touches no framebuffer: drivers feed it input bytes over
//! IPC, and it returns the bytes that should be echoed plus completed command
//! lines for Kumoza/persona-posix clients.
//!
//! Recovery class: stateless / soft-state (`DESIGN/002`). The edit buffer is not
//! durable truth; on restart, clients reconnect and any half-typed line is lost.

#[cfg(test)]
extern crate alloc;

/// Default cooked-line capacity, matching the current Sora scaffold buffer.
pub const DEFAULT_LINE_BYTES: usize = 256;

/// One-byte request: clear the current edit line.
pub const OP_CLEAR: u8 = 0x00;
/// Two-byte request: feed one input byte (`[OP_INPUT, byte]`).
pub const OP_INPUT: u8 = 0x01;
/// Two-byte request: write one output byte (`[OP_WRITE, byte]`).
pub const OP_WRITE: u8 = 0x02;
/// One-byte request: read the last submitted line, if any.
pub const OP_READ: u8 = 0x03;
/// One-byte request: terminate the serve loop without a reply.
pub const OP_SHUTDOWN: u8 = 0xff;

/// Reply status: request handled.
pub const TTY_OK: u8 = 0x00;
/// Reply status: malformed request frame.
pub const TTY_BAD_REQUEST: u8 = 0x01;
/// Reply status: the line or reply buffer was full.
pub const TTY_OVERFLOW: u8 = 0x02;

/// Reply event: no completed line in this reply.
pub const EVENT_NONE: u8 = 0x00;
/// Reply event: the reply carries one submitted line.
pub const EVENT_LINE: u8 = 0x01;

/// Request buffer size for the current protocol.
pub const REQUEST_BUF_BYTES: usize = 2;
/// Reply header: status, event, echo_len, line_len_le.
pub const REPLY_HEADER_BYTES: usize = 5;
/// Reply buffer size for a default-sized TTY session.
pub const REPLY_BUF_BYTES: usize = REPLY_HEADER_BYTES + 3 + DEFAULT_LINE_BYTES;

const BACKSPACE_ECHO: &[u8] = b"\x08 \x08";
const ENTER_ECHO: &[u8] = b"\r\n";
const BELL: &[u8] = b"\x07";

/// A decoded ttyd request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    /// Drop the current edit line.
    Clear,
    /// Feed one raw input byte.
    Input(u8),
    /// Write one byte to the terminal output stream.
    Write(u8),
    /// Read one submitted line.
    Read,
    /// Gracefully stop the server.
    Shutdown,
}

impl Request {
    pub const fn input(byte: u8) -> Request {
        Request::Input(byte)
    }

    pub const fn clear() -> Request {
        Request::Clear
    }

    pub const fn write(byte: u8) -> Request {
        Request::Write(byte)
    }

    pub const fn read() -> Request {
        Request::Read
    }

    pub const fn shutdown() -> Request {
        Request::Shutdown
    }

    pub fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        match *self {
            Request::Clear | Request::Read | Request::Shutdown => {
                let frame = out.get_mut(..1)?;
                frame[0] = match *self {
                    Request::Clear => OP_CLEAR,
                    Request::Read => OP_READ,
                    Request::Shutdown => OP_SHUTDOWN,
                    Request::Input(_) | Request::Write(_) => unreachable!(),
                };
                Some(1)
            }
            Request::Input(byte) | Request::Write(byte) => {
                let frame = out.get_mut(..2)?;
                frame[0] = match *self {
                    Request::Input(_) => OP_INPUT,
                    Request::Write(_) => OP_WRITE,
                    Request::Clear | Request::Read | Request::Shutdown => {
                        unreachable!()
                    }
                };
                frame[1] = byte;
                Some(2)
            }
        }
    }

    pub fn decode(raw: &[u8]) -> Option<Request> {
        match raw {
            [OP_CLEAR] => Some(Request::Clear),
            [OP_INPUT, byte] => Some(Request::Input(*byte)),
            [OP_WRITE, byte] => Some(Request::Write(*byte)),
            [OP_READ] => Some(Request::Read),
            [OP_SHUTDOWN] => Some(Request::Shutdown),
            _ => None,
        }
    }
}

/// Parsed view of a ttyd reply frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reply<'a> {
    pub status: u8,
    pub echo: &'a [u8],
    pub line: Option<&'a [u8]>,
}

impl<'a> Reply<'a> {
    pub fn parse(raw: &'a [u8]) -> Option<Reply<'a>> {
        let header = raw.get(..REPLY_HEADER_BYTES)?;
        let status = header[0];
        let event = header[1];
        let echo_len = header[2] as usize;
        let line_len = u16::from_le_bytes([header[3], header[4]]) as usize;
        let echo_start = REPLY_HEADER_BYTES;
        let line_start = echo_start.checked_add(echo_len)?;
        let line_end = line_start.checked_add(line_len)?;
        let echo = raw.get(echo_start..line_start)?;
        let line_bytes = raw.get(line_start..line_end)?;
        let line = match event {
            EVENT_NONE if line_len == 0 => None,
            EVENT_LINE => Some(line_bytes),
            _ => return None,
        };
        Some(Reply { status, echo, line })
    }
}

/// Echo bytes emitted in response to one input byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Echo {
    bytes: [u8; 3],
    len: u8,
}

impl Echo {
    pub const fn none() -> Echo {
        Echo {
            bytes: [0; 3],
            len: 0,
        }
    }

    pub const fn byte(byte: u8) -> Echo {
        Echo {
            bytes: [byte, 0, 0],
            len: 1,
        }
    }

    pub fn bytes(bytes: &[u8]) -> Echo {
        let mut out = [0u8; 3];
        let mut i = 0;
        while i < bytes.len() && i < out.len() {
            out[i] = bytes[i];
            i += 1;
        }
        Echo {
            bytes: out,
            len: i as u8,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Line-level event produced by one input byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineEvent {
    /// No completed line yet.
    None,
    /// A line was copied into the caller-provided output buffer.
    Submitted { len: usize },
    /// The input or output buffer was full, so the byte/line was not accepted.
    Overflow,
}

/// Result of feeding one byte through the line discipline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputResult {
    pub echo: Echo,
    pub event: LineEvent,
}

impl InputResult {
    pub const fn none() -> InputResult {
        InputResult {
            echo: Echo::none(),
            event: LineEvent::None,
        }
    }
}

/// Cooked line discipline for one TTY session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineDiscipline<const N: usize = DEFAULT_LINE_BYTES> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> LineDiscipline<N> {
    pub const fn new() -> LineDiscipline<N> {
        LineDiscipline {
            buf: [0; N],
            len: 0,
        }
    }

    /// Bytes currently typed but not yet submitted.
    pub fn pending(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Drop the current edit line.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Feed one raw input byte.
    ///
    /// Printable ASCII is appended and echoed. Backspace/Delete erase one byte.
    /// Enter submits the current line into `submitted`, echoes CRLF, and resets the
    /// edit buffer. Other control bytes are ignored.
    pub fn feed_byte(&mut self, byte: u8, submitted: &mut [u8]) -> InputResult {
        match byte {
            b'\r' | b'\n' => self.submit(submitted),
            0x08 | 0x7f => self.backspace(),
            0x20..=0x7e => self.printable(byte),
            _ => InputResult::none(),
        }
    }

    fn printable(&mut self, byte: u8) -> InputResult {
        if self.len >= self.buf.len() {
            return InputResult {
                echo: Echo::bytes(BELL),
                event: LineEvent::Overflow,
            };
        }
        self.buf[self.len] = byte;
        self.len += 1;
        InputResult {
            echo: Echo::byte(byte),
            event: LineEvent::None,
        }
    }

    fn backspace(&mut self) -> InputResult {
        if self.len == 0 {
            return InputResult::none();
        }
        self.len -= 1;
        InputResult {
            echo: Echo::bytes(BACKSPACE_ECHO),
            event: LineEvent::None,
        }
    }

    fn submit(&mut self, submitted: &mut [u8]) -> InputResult {
        if submitted.len() < self.len {
            return InputResult {
                echo: Echo::bytes(BELL),
                event: LineEvent::Overflow,
            };
        }
        let len = self.len;
        submitted[..len].copy_from_slice(&self.buf[..len]);
        self.len = 0;
        InputResult {
            echo: Echo::bytes(ENTER_ECHO),
            event: LineEvent::Submitted { len },
        }
    }
}

impl<const N: usize> Default for LineDiscipline<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The request/reply transport the server runs over. The real implementation will
/// wrap KUMO channels; tests use an in-memory fake.
pub trait Transport {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
    fn send(&mut self, frame: &[u8]);
}

/// Server state for one tty session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TtyServer<const N: usize = DEFAULT_LINE_BYTES> {
    line: LineDiscipline<N>,
    submitted: [u8; N],
    submitted_len: usize,
    submitted_ready: bool,
}

impl<const N: usize> TtyServer<N> {
    pub const fn new() -> TtyServer<N> {
        TtyServer {
            line: LineDiscipline::new(),
            submitted: [0; N],
            submitted_len: 0,
            submitted_ready: false,
        }
    }

    pub fn pending(&self) -> &[u8] {
        self.line.pending()
    }

    /// Handle one request frame, writing a reply frame.
    pub fn dispatch(&mut self, request: &[u8], reply: &mut [u8]) -> usize {
        let Some(request) = Request::decode(request) else {
            return write_reply(reply, TTY_BAD_REQUEST, Echo::none(), None);
        };
        match request {
            Request::Clear => {
                self.line.clear();
                write_reply(reply, TTY_OK, Echo::none(), None)
            }
            Request::Shutdown => 0,
            Request::Write(byte) => write_reply(reply, TTY_OK, Echo::byte(byte), None),
            Request::Read => {
                if self.submitted_ready {
                    let n = self.submitted_len;
                    self.submitted_ready = false;
                    self.submitted_len = 0;
                    write_reply(reply, TTY_OK, Echo::none(), Some(&self.submitted[..n]))
                } else {
                    write_reply(reply, TTY_OK, Echo::none(), None)
                }
            }
            Request::Input(byte) => {
                let mut submitted = [0u8; N];
                let result = self.line.feed_byte(byte, &mut submitted);
                let line = match result.event {
                    LineEvent::None => None,
                    LineEvent::Submitted { len } => {
                        self.submitted[..len].copy_from_slice(&submitted[..len]);
                        self.submitted_len = len;
                        self.submitted_ready = true;
                        Some(&submitted[..len])
                    }
                    LineEvent::Overflow => {
                        return write_reply(reply, TTY_OVERFLOW, result.echo, None);
                    }
                };
                write_reply(reply, TTY_OK, result.echo, line)
            }
        }
    }

    /// Handle exactly one transport request/reply exchange. Returns `false` when
    /// the transport closes before yielding a request.
    pub fn serve_once<T: Transport>(
        &mut self,
        transport: &mut T,
        request_buf: &mut [u8],
        reply_buf: &mut [u8],
    ) -> bool {
        let Some(n) = transport.recv(request_buf) else {
            return false;
        };
        if Request::decode(&request_buf[..n]) == Some(Request::Shutdown) {
            return false;
        }
        let reply_len = self.dispatch(&request_buf[..n], reply_buf);
        if reply_len != 0 {
            transport.send(&reply_buf[..reply_len]);
        }
        true
    }
}

impl TtyServer<DEFAULT_LINE_BYTES> {
    /// Run request/reply exchanges until the transport closes.
    pub fn serve<T: Transport>(&mut self, transport: &mut T) {
        let mut request_buf = [0u8; REQUEST_BUF_BYTES];
        let mut reply_buf = [0u8; REPLY_BUF_BYTES];
        while self.serve_once(transport, &mut request_buf, &mut reply_buf) {}
    }
}

impl<const N: usize> Default for TtyServer<N> {
    fn default() -> Self {
        Self::new()
    }
}

fn write_reply(reply: &mut [u8], status: u8, echo: Echo, line: Option<&[u8]>) -> usize {
    let echo = echo.as_bytes();
    let line = line.unwrap_or(&[]);
    let Some(total) = REPLY_HEADER_BYTES
        .checked_add(echo.len())
        .and_then(|n| n.checked_add(line.len()))
    else {
        return 0;
    };
    let Some(out) = reply.get_mut(..total) else {
        return 0;
    };
    if line.len() > u16::MAX as usize {
        return 0;
    }
    out[0] = status;
    out[1] = if line.is_empty() {
        EVENT_NONE
    } else {
        EVENT_LINE
    };
    out[2] = echo.len() as u8;
    out[3..5].copy_from_slice(&(line.len() as u16).to_le_bytes());
    out[REPLY_HEADER_BYTES..REPLY_HEADER_BYTES + echo.len()].copy_from_slice(echo);
    out[REPLY_HEADER_BYTES + echo.len()..total].copy_from_slice(line);
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;

    fn feed<const N: usize>(tty: &mut LineDiscipline<N>, bytes: &[u8]) -> ([u8; 16], usize) {
        let mut submitted = [0u8; 16];
        let mut echo = [0u8; 16];
        let mut pos = 0;
        for &byte in bytes {
            let result = tty.feed_byte(byte, &mut submitted);
            let e = result.echo.as_bytes();
            echo[pos..pos + e.len()].copy_from_slice(e);
            pos += e.len();
        }
        (echo, pos)
    }

    #[test]
    fn printable_bytes_echo_and_buffer() {
        let mut tty = LineDiscipline::<8>::new();
        let (echo, n) = feed(&mut tty, b"abc");

        assert_eq!(&echo[..n], b"abc");
        assert_eq!(tty.pending(), b"abc");
    }

    #[test]
    fn enter_submits_line_and_resets_buffer() {
        let mut tty = LineDiscipline::<8>::new();
        let mut submitted = [0u8; 8];
        feed(&mut tty, b"run");

        let result = tty.feed_byte(b'\n', &mut submitted);

        assert_eq!(result.echo.as_bytes(), b"\r\n");
        assert_eq!(result.event, LineEvent::Submitted { len: 3 });
        assert_eq!(&submitted[..3], b"run");
        assert_eq!(tty.pending(), b"");
    }

    #[test]
    fn carriage_return_also_submits() {
        let mut tty = LineDiscipline::<8>::new();
        let mut submitted = [0u8; 8];
        feed(&mut tty, b"ls");

        let result = tty.feed_byte(b'\r', &mut submitted);

        assert_eq!(result.event, LineEvent::Submitted { len: 2 });
        assert_eq!(&submitted[..2], b"ls");
    }

    #[test]
    fn backspace_erases_one_byte_and_echoes_erase_sequence() {
        let mut tty = LineDiscipline::<8>::new();
        let (echo, n) = feed(&mut tty, b"ab\x08c");

        assert_eq!(&echo[..n], b"ab\x08 \x08c");
        assert_eq!(tty.pending(), b"ac");
    }

    #[test]
    fn delete_is_treated_as_backspace() {
        let mut tty = LineDiscipline::<8>::new();
        feed(&mut tty, b"xy");

        let result = tty.feed_byte(0x7f, &mut [0u8; 8]);

        assert_eq!(result.echo.as_bytes(), b"\x08 \x08");
        assert_eq!(tty.pending(), b"x");
    }

    #[test]
    fn empty_backspace_and_other_controls_are_ignored() {
        let mut tty = LineDiscipline::<8>::new();
        let mut submitted = [0u8; 8];

        assert_eq!(tty.feed_byte(0x08, &mut submitted), InputResult::none());
        assert_eq!(tty.feed_byte(0x01, &mut submitted), InputResult::none());
        assert_eq!(tty.pending(), b"");
    }

    #[test]
    fn full_line_rejects_more_input_with_bell() {
        let mut tty = LineDiscipline::<3>::new();
        feed(&mut tty, b"abc");

        let result = tty.feed_byte(b'd', &mut [0u8; 8]);

        assert_eq!(result.echo.as_bytes(), b"\x07");
        assert_eq!(result.event, LineEvent::Overflow);
        assert_eq!(tty.pending(), b"abc");
    }

    #[test]
    fn too_small_submission_buffer_preserves_line() {
        let mut tty = LineDiscipline::<8>::new();
        feed(&mut tty, b"abcd");

        let result = tty.feed_byte(b'\n', &mut [0u8; 2]);

        assert_eq!(result.echo.as_bytes(), b"\x07");
        assert_eq!(result.event, LineEvent::Overflow);
        assert_eq!(tty.pending(), b"abcd");
    }

    fn encode_request(request: Request) -> Vec<u8> {
        let mut buf = [0u8; REQUEST_BUF_BYTES];
        let n = request.encode_into(&mut buf).expect("encode tty request");
        buf[..n].to_vec()
    }

    struct MockTransport {
        incoming: VecDeque<Vec<u8>>,
        sent: Vec<Vec<u8>>,
    }

    impl Transport for MockTransport {
        fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
            let frame = self.incoming.pop_front()?;
            buf[..frame.len()].copy_from_slice(&frame);
            Some(frame.len())
        }

        fn send(&mut self, frame: &[u8]) {
            self.sent.push(frame.to_vec());
        }
    }

    #[test]
    fn request_codec_round_trips() {
        let mut buf = [0u8; REQUEST_BUF_BYTES];

        let n = Request::input(b'a').encode_into(&mut buf).unwrap();
        assert_eq!(Request::decode(&buf[..n]), Some(Request::Input(b'a')));
        let n = Request::write(b'\n').encode_into(&mut buf).unwrap();
        assert_eq!(Request::decode(&buf[..n]), Some(Request::Write(b'\n')));

        let n = Request::clear().encode_into(&mut buf).unwrap();
        assert_eq!(Request::decode(&buf[..n]), Some(Request::Clear));
        let n = Request::read().encode_into(&mut buf).unwrap();
        assert_eq!(Request::decode(&buf[..n]), Some(Request::Read));
        let n = Request::shutdown().encode_into(&mut buf).unwrap();
        assert_eq!(Request::decode(&buf[..n]), Some(Request::Shutdown));
        assert_eq!(Request::decode(&[OP_INPUT]), None);
    }

    #[test]
    fn dispatch_input_replies_with_echo() {
        let mut server = TtyServer::<8>::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];

        let n = server.dispatch(&encode_request(Request::input(b'x')), &mut reply);
        let parsed = Reply::parse(&reply[..n]).unwrap();

        assert_eq!(parsed.status, TTY_OK);
        assert_eq!(parsed.echo, b"x");
        assert_eq!(parsed.line, None);
        assert_eq!(server.pending(), b"x");
    }

    #[test]
    fn dispatch_write_replies_with_output_without_touching_line() {
        let mut server = TtyServer::<8>::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        server.dispatch(&encode_request(Request::input(b'x')), &mut reply);

        let n = server.dispatch(&encode_request(Request::write(b'\n')), &mut reply);
        let parsed = Reply::parse(&reply[..n]).unwrap();

        assert_eq!(parsed.status, TTY_OK);
        assert_eq!(parsed.echo, b"\n");
        assert_eq!(parsed.line, None);
        assert_eq!(server.pending(), b"x");
    }

    #[test]
    fn dispatch_enter_replies_with_submitted_line() {
        let mut server = TtyServer::<8>::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        server.dispatch(&encode_request(Request::input(b'l')), &mut reply);
        server.dispatch(&encode_request(Request::input(b's')), &mut reply);

        let n = server.dispatch(&encode_request(Request::input(b'\n')), &mut reply);
        let parsed = Reply::parse(&reply[..n]).unwrap();

        assert_eq!(parsed.status, TTY_OK);
        assert_eq!(parsed.echo, b"\r\n");
        assert_eq!(parsed.line, Some(&b"ls"[..]));
        assert_eq!(server.pending(), b"");
    }

    #[test]
    fn dispatch_read_returns_submitted_line_once() {
        let mut server = TtyServer::<8>::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        server.dispatch(&encode_request(Request::input(b'l')), &mut reply);
        server.dispatch(&encode_request(Request::input(b's')), &mut reply);
        server.dispatch(&encode_request(Request::input(b'\n')), &mut reply);

        let n = server.dispatch(&encode_request(Request::read()), &mut reply);
        let parsed = Reply::parse(&reply[..n]).unwrap();

        assert_eq!(parsed.status, TTY_OK);
        assert_eq!(parsed.echo, b"");
        assert_eq!(parsed.line, Some(&b"ls"[..]));

        let n = server.dispatch(&encode_request(Request::read()), &mut reply);
        let parsed = Reply::parse(&reply[..n]).unwrap();

        assert_eq!(parsed.status, TTY_OK);
        assert_eq!(parsed.line, None);
    }

    #[test]
    fn dispatch_read_before_submit_is_empty() {
        let mut server = TtyServer::<8>::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];

        let n = server.dispatch(&encode_request(Request::read()), &mut reply);
        let parsed = Reply::parse(&reply[..n]).unwrap();

        assert_eq!(parsed.status, TTY_OK);
        assert_eq!(parsed.echo, b"");
        assert_eq!(parsed.line, None);
    }

    #[test]
    fn dispatch_bad_request_reports_status() {
        let mut server = TtyServer::<8>::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];

        let n = server.dispatch(&[0xfd], &mut reply);
        let parsed = Reply::parse(&reply[..n]).unwrap();

        assert_eq!(parsed.status, TTY_BAD_REQUEST);
        assert_eq!(parsed.echo, b"");
        assert_eq!(parsed.line, None);
    }

    #[test]
    fn clear_request_drops_pending_line() {
        let mut server = TtyServer::<8>::new();
        let mut reply = [0u8; REPLY_BUF_BYTES];
        server.dispatch(&encode_request(Request::input(b'a')), &mut reply);

        let n = server.dispatch(&encode_request(Request::clear()), &mut reply);
        let parsed = Reply::parse(&reply[..n]).unwrap();

        assert_eq!(parsed.status, TTY_OK);
        assert_eq!(server.pending(), b"");
    }

    #[test]
    fn serve_preserves_line_across_transport_frames() {
        let mut server = TtyServer::<DEFAULT_LINE_BYTES>::new();
        let mut transport = MockTransport {
            incoming: VecDeque::from([
                encode_request(Request::input(b'h')),
                encode_request(Request::input(b'i')),
                encode_request(Request::input(b'\n')),
            ]),
            sent: Vec::new(),
        };

        server.serve(&mut transport);

        assert_eq!(transport.sent.len(), 3);
        assert_eq!(Reply::parse(&transport.sent[0]).unwrap().echo, b"h");
        assert_eq!(Reply::parse(&transport.sent[1]).unwrap().echo, b"i");
        let third = Reply::parse(&transport.sent[2]).unwrap();
        assert_eq!(third.echo, b"\r\n");
        assert_eq!(third.line, Some(&b"hi"[..]));
    }

    #[test]
    fn serve_once_stops_when_transport_closes() {
        let mut server = TtyServer::<DEFAULT_LINE_BYTES>::new();
        let mut transport = MockTransport {
            incoming: VecDeque::new(),
            sent: Vec::new(),
        };
        let mut request_buf = [0u8; REQUEST_BUF_BYTES];
        let mut reply_buf = [0u8; REPLY_BUF_BYTES];

        assert!(!server.serve_once(&mut transport, &mut request_buf, &mut reply_buf));
        assert!(transport.sent.is_empty());
    }

    #[test]
    fn shutdown_request_stops_without_reply() {
        let mut server = TtyServer::<DEFAULT_LINE_BYTES>::new();
        let mut transport = MockTransport {
            incoming: VecDeque::from([encode_request(Request::shutdown())]),
            sent: Vec::new(),
        };
        let mut request_buf = [0u8; REQUEST_BUF_BYTES];
        let mut reply_buf = [0u8; REPLY_BUF_BYTES];

        assert!(!server.serve_once(&mut transport, &mut request_buf, &mut reply_buf));
        assert!(transport.sent.is_empty());
    }
}
