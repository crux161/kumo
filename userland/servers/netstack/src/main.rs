#![no_std]
#![no_main]
//j815
//j820

//! The resident netstack: one Port, one loopback interface, sessions over granted channels
//! (DESIGN/026). Sora transfers a control channel in `x1`; each control message is the tag `S`
//! carrying the server end of one new session channel. Everything protocol-shaped lives in the
//! host-tested library; this file is the syscall loop around it.
//!
//! Recovery class: stateless (discardable socket soft state); fresh Sora-granted channels reconnect.
//!
//! A resident child's `ChannelRead` parks on an empty channel and returns 0 only once the peer
//! has closed. So every wake reads exactly one message from the channel that woke (the kernel
//! re-arms the port while more is queued), and a 0-byte read is a closed session.

extern crate alloc;

use alloc::vec::Vec;

use kumo_abi::sys::log_level;
use kumo_abi::Handle;
use kumo_ipc::MAX_INLINE_BYTES;
use kumo_netstack::budget::SESSIONS;
use kumo_rt::{
    channel_read_with_handle, channel_write, clock_get, handle_close, handle_koid, port_bind,
    port_create, port_unbind, port_wait, timer_create,
};
use netstack::{After, Output, Server};

kumo_rt::entry!(main);

/// A control message: one tag byte, with the session channel attached.
const CONTROL_SESSION: u8 = b'S';
/// Timers shorter than this are rounded up, so a busy stack cannot spin the loop.
const MIN_TIMER_NS: u64 = 1_000_000;

fn log(level: u8, message: &[u8]) {
    let _ = kumo_rt::log_write(level, message);
}

fn fail(message: &[u8]) -> ! {
    log(log_level::ERR, message);
    kumo_rt::process_exit(1)
}

fn valid(raw: u64) -> Option<Handle> {
    (raw != 0 && raw != u64::MAX).then_some(Handle(raw as u32))
}

fn push_dec(line: &mut Vec<u8>, mut value: u64) {
    let mut digits = [0u8; 20];
    let mut len = 0;
    loop {
        digits[len] = b'0' + (value % 10) as u8;
        value /= 10;
        len += 1;
        if value == 0 {
            break;
        }
    }
    line.extend(digits[..len].iter().rev());
}

struct Channels {
    port: Handle,
    /// `(channel, koid)` per server session index.
    sessions: [Option<(Handle, u64)>; SESSIONS],
    timer: Option<(Handle, u64, u64)>,
}

impl Channels {
    fn session_of(&self, koid: u64) -> Option<usize> {
        self.sessions
            .iter()
            .position(|entry| matches!(entry, Some((_, k)) if *k == koid))
    }

    fn write(&self, out: &mut Vec<Output>) {
        for output in out.drain(..) {
            if let Some((channel, _)) = self.sessions[output.session] {
                let _ = channel_write(channel, output.bytes.as_ptr(), output.bytes.len());
            }
        }
    }

    fn close(&mut self, server: &mut Server, session: usize, why: &[u8]) {
        if let Some((channel, _)) = self.sessions[session].take() {
            let _ = port_unbind(self.port, channel);
            let _ = handle_close(channel);
        }
        let freed = server.close_session(session);
        let mut line = Vec::with_capacity(96);
        line.extend_from_slice(b"netstack: session ");
        push_dec(&mut line, session as u64);
        line.extend_from_slice(b" closed (");
        line.extend_from_slice(why);
        line.extend_from_slice(b") sockets freed=");
        push_dec(&mut line, freed as u64);
        line.extend_from_slice(b" live sessions=");
        push_dec(&mut line, server.live_sessions() as u64);
        line.extend_from_slice(b" sockets=");
        push_dec(&mut line, server.live_sockets() as u64);
        line.push(b'\n');
        log(log_level::NOTICE, &line);
    }

    /// Keep exactly one timer armed for the server's next deadline.
    fn arm(&mut self, server: &mut Server, now: u64) {
        let Some(delay) = server.next_wake_ns(now) else {
            return;
        };
        let due = now.saturating_add(delay.max(MIN_TIMER_NS));
        if matches!(self.timer, Some((_, _, armed)) if armed <= due) {
            return;
        }
        self.disarm();
        let Some(timer) = valid(timer_create(due - now)) else {
            return;
        };
        let koid = handle_koid(timer);
        if koid == u64::MAX || port_bind(self.port, timer) != 0 {
            let _ = handle_close(timer);
            return;
        }
        self.timer = Some((timer, koid, due));
    }

    fn disarm(&mut self) {
        if let Some((timer, _, _)) = self.timer.take() {
            let _ = port_unbind(self.port, timer);
            let _ = handle_close(timer);
        }
    }
}

#[no_mangle]
extern "C" fn main(
    _arg0: u64,
    control_raw: u64,
    _arg2: u64,
    _arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
    _arg7: u64,
) -> ! {
    let control = valid(control_raw).unwrap_or_else(|| fail(b"netstack: no control channel\n"));
    // The supervisor queues the instance epoch before admission. Read it before binding,
    // so its READABLE packet cannot survive as a stale wake in the ordinary receive loop.
    let mut bootstrap = [0u8; 3];
    let (n, raw) = channel_read_with_handle(control, bootstrap.as_mut_ptr(), bootstrap.len());
    if valid(raw).is_some() || n != 3 || bootstrap[0] != b'I' {
        if let Some(handle) = valid(raw) {
            let _ = handle_close(handle);
        }
        fail(b"netstack: invalid instance bootstrap\n");
    }
    let epoch = u16::from_le_bytes([bootstrap[1], bootstrap[2]]);
    let port = valid(port_create()).unwrap_or_else(|| fail(b"netstack: no port\n"));
    let control_koid = handle_koid(control);
    if control_koid == u64::MAX || port_bind(port, control) != 0 {
        fail(b"netstack: control bind failed\n");
    }
    // Heap, not stack: a native child's default stack is 16 KiB.
    let mut server = alloc::boxed::Box::new(
        Server::for_instance(clock_get(), epoch)
            .unwrap_or_else(|| fail(b"netstack: invalid instance epoch\n")),
    );
    let mut channels = Channels {
        port,
        sessions: [None; SESSIONS],
        timer: None,
    };
    let mut out: Vec<Output> = Vec::new();
    let mut message = alloc::vec![0u8; MAX_INLINE_BYTES];
    log(
        log_level::NOTICE,
        b"netstack: online, loopback 127.0.0.1/8, UDP/TCP; sessions granted by Sora\n",
    );

    loop {
        let source = port_wait(port);
        let now = clock_get();
        if source == control_koid {
            let mut tag = [0u8; 1];
            let (n, raw) = channel_read_with_handle(control, tag.as_mut_ptr(), tag.len());
            if n == 0 {
                // Sora is gone; serve the sessions that remain.
                let _ = port_unbind(port, control);
            } else if let Some(channel) = valid(raw) {
                let koid = handle_koid(channel);
                let admitted = (tag[0] == CONTROL_SESSION && koid != u64::MAX)
                    .then(|| server.open_session())
                    .flatten();
                match admitted {
                    Some(session) if port_bind(port, channel) == 0 => {
                        channels.sessions[session] = Some((channel, koid));
                        let mut line = Vec::with_capacity(64);
                        line.extend_from_slice(b"netstack: session ");
                        push_dec(&mut line, session as u64);
                        line.extend_from_slice(b" granted, live sessions=");
                        push_dec(&mut line, server.live_sessions() as u64);
                        line.push(b'\n');
                        log(log_level::NOTICE, &line);
                    }
                    Some(session) => {
                        server.close_session(session);
                        let _ = handle_close(channel);
                    }
                    None => {
                        log(
                            log_level::WARNING,
                            b"netstack: session refused (full or bad tag)\n",
                        );
                        let _ = handle_close(channel);
                    }
                }
            }
        } else if let Some(session) = channels.session_of(source) {
            let (channel, _) = channels.sessions[session].unwrap();
            let (n, raw) = channel_read_with_handle(channel, message.as_mut_ptr(), message.len());
            if n == 0 {
                channels.close(&mut server, session, b"peer closed");
            } else {
                let handles = usize::from(valid(raw).is_some());
                if let Some(extra) = valid(raw) {
                    let _ = handle_close(extra);
                }
                // The buffer holds the largest message the kernel carries, so `n` is the
                // whole message and the codec's exact-length check stays meaningful.
                let after = server.handle(session, &message[..n], handles, now, &mut out);
                server.poll(now, &mut out);
                channels.write(&mut out);
                if after == After::Close {
                    channels.close(&mut server, session, b"protocol");
                }
            }
        } else if matches!(channels.timer, Some((_, koid, _)) if koid == source) {
            channels.disarm();
        }
        server.poll(clock_get(), &mut out);
        channels.write(&mut out);
        channels.arm(&mut server, clock_get());
    }
}
