#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//! `svc-health` — the first real userspace server, and the template every Siyu
//! server clones (PLAN §6/§12, Appendix Γ; the migration is Stage C of PLAN §5.4).
//!
//! ## The Arcana
//! This is the seed of the **Siyu** service plane — the first service to leave the
//! kernel/Sora bootstrap and stand as its own crate with its own capability surface.
//! It holds no ambient authority; it answers exactly one channel.
//!
//! ## What this crate is (and is not)
//! Per PLAN §9, *server logic is arch-neutral and host-testable before it ever runs on
//! metal.* So the load-bearing content here is **pure, fully-tested request logic**: a
//! typed protocol, a byte wire-format (IPC messages are bytes — PLAN §19.2), and a
//! [`Health::dispatch`] that maps request bytes to reply bytes. That path is unit tested
//! (`cargo test -p svc-health`) and Sora now runs the freestanding binary for a
//! process-isolated, resident `Ping` → `Pong`, `Status` smoke through the same serve loop.
//!
//! The serve loop itself ([`serve`]) is generic over a [`Transport`], so the whole
//! request→dispatch→reply cycle is **host-tested end-to-end** (see `end_to_end_*` in the
//! tests). Only the ~10-line `kumo-rt` `Transport` impl + the Sora spawn need the image
//! pipeline (nightly + build-std); the current image path grants the binary one `Channel`
//! and proves child-side `PortWait` plus later wake. DESIGN/002 supervised restart
//! applies unchanged — it is stateless.
//!
//! ## Reusing this as a template
//! A new server (drv-serial, an fsd front, …) copies this shape: a `Request`/`Response`
//! enum, `decode`/`encode`, a state struct, and `dispatch`. The serve loop is identical;
//! only the protocol and the granted capabilities change.

extern crate alloc;

use alloc::vec::Vec;

/// Wire opcodes. The first byte of every message is one of these.
mod op {
    pub const PING: u8 = 0x01;
    pub const STATUS: u8 = 0x02;
    pub const SHUTDOWN: u8 = 0x03;
    pub const PONG: u8 = 0x81;
    pub const STATUS_OK: u8 = 0x82;
    pub const ERROR: u8 = 0xEE;
}

/// A request a client sends to the health server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    /// Liveness probe; expects [`Response::Pong`].
    Ping,
    /// Ask for the server's running counters; expects [`Response::Status`].
    Status,
    /// Graceful stop: the serve loop exits (no reply) and the server process terminates,
    /// so its supervisor observes the termination (DESIGN/002 §1). Intercepted in
    /// [`serve_once`] before the reply path.
    Shutdown,
}

impl Request {
    /// Decode a request from a channel message. Returns `None` on a malformed frame so
    /// the server replies with an error rather than faulting (no panic on bad input).
    pub fn decode(raw: &[u8]) -> Option<Request> {
        match raw.first().copied()? {
            op::PING => Some(Request::Ping),
            op::STATUS => Some(Request::Status),
            op::SHUTDOWN => Some(Request::Shutdown),
            _ => None,
        }
    }

    /// Encode a request (used by clients and tests).
    pub fn encode(self) -> Vec<u8> {
        let code = match self {
            Request::Ping => op::PING,
            Request::Status => op::STATUS,
            Request::Shutdown => op::SHUTDOWN,
        };
        alloc::vec![code]
    }
}

/// A reply the health server sends back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Response {
    /// Answer to [`Request::Ping`].
    Pong,
    /// Answer to [`Request::Status`]: uptime in scheduler ticks and total requests served.
    Status { uptime_ticks: u64, served: u64 },
    /// The request frame was malformed.
    Error,
}

impl Response {
    /// Encode a reply into channel bytes.
    pub fn encode(self) -> Vec<u8> {
        match self {
            Response::Pong => alloc::vec![op::PONG],
            Response::Status {
                uptime_ticks,
                served,
            } => {
                let mut out = Vec::with_capacity(1 + 8 + 8);
                out.push(op::STATUS_OK);
                out.extend_from_slice(&uptime_ticks.to_le_bytes());
                out.extend_from_slice(&served.to_le_bytes());
                out
            }
            Response::Error => alloc::vec![op::ERROR],
        }
    }

    /// Decode a reply (used by clients and tests).
    pub fn decode(raw: &[u8]) -> Option<Response> {
        match raw.first().copied()? {
            op::PONG => Some(Response::Pong),
            op::ERROR => Some(Response::Error),
            op::STATUS_OK if raw.len() >= 17 => {
                let uptime_ticks = u64::from_le_bytes(raw[1..9].try_into().ok()?);
                let served = u64::from_le_bytes(raw[9..17].try_into().ok()?);
                Some(Response::Status {
                    uptime_ticks,
                    served,
                })
            }
            _ => None,
        }
    }
}

/// The server's recoverable state. Per DESIGN/002 this server is the *stateless* recovery
/// class: counters are advisory, so a supervised restart simply resets them — no critical
/// state lives in private RAM.
#[derive(Clone, Copy, Debug, Default)]
pub struct Health {
    uptime_ticks: u64,
    served: u64,
}

impl Health {
    pub const fn new() -> Health {
        Health {
            uptime_ticks: 0,
            served: 0,
        }
    }

    /// Advance the uptime counter (called from the server's timer wakeups).
    pub fn tick(&mut self, ticks: u64) {
        self.uptime_ticks = self.uptime_ticks.saturating_add(ticks);
    }

    /// Handle one typed request, advancing the served counter for replied requests.
    pub fn handle(&mut self, req: Request) -> Response {
        match req {
            Request::Ping => {
                self.served = self.served.saturating_add(1);
                Response::Pong
            }
            Request::Status => {
                self.served = self.served.saturating_add(1);
                Response::Status {
                    uptime_ticks: self.uptime_ticks,
                    served: self.served,
                }
            }
            // A control request with no reply; `serve_once` intercepts it before this path,
            // so this arm only keeps a direct `handle`/`dispatch` call total.
            Request::Shutdown => Response::Error,
        }
    }

    /// The whole request path in one call: raw request bytes -> raw reply bytes. A
    /// malformed frame yields [`Response::Error`]; it never panics on client input.
    pub fn dispatch(&mut self, raw: &[u8]) -> Vec<u8> {
        match Request::decode(raw) {
            Some(req) => self.handle(req).encode(),
            None => Response::Error.encode(),
        }
    }
}

/// The request/reply transport the server runs over. The real implementation wraps a KUMO
/// `Channel` via `kumo-rt` (see [`entry`]); tests use an in-memory fake. Inverting the
/// transport this way keeps the **whole serve loop** ([`serve`]) host-testable end-to-end,
/// not just the per-request dispatch.
pub trait Transport {
    /// Block for the next request frame, returning its length written into `buf`, or `None`
    /// when the peer has closed — at which point the serve loop exits cleanly.
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
    /// Send one reply frame.
    fn send(&mut self, frame: &[u8]);
}

/// Handle exactly one request/reply exchange. Returns `false` — ending the serve loop —
/// when the transport closes **or** a [`Request::Shutdown`] arrives (a graceful stop that
/// sends no reply; the server process then exits, DESIGN/002 §1).
pub fn serve_once<T: Transport>(t: &mut T, state: &mut Health, buf: &mut [u8]) -> bool {
    match t.recv(buf) {
        Some(n) => {
            if Request::decode(&buf[..n]) == Some(Request::Shutdown) {
                return false;
            }
            let reply = state.dispatch(&buf[..n]);
            t.send(&reply);
            true
        }
        None => false,
    }
}

/// The entire serve loop: run request/reply exchanges until the transport closes. This is
/// the shell every Siyu server shares; on real hardware only the `Transport` impl differs.
pub fn serve<T: Transport>(t: &mut T) {
    let mut state = Health::new();
    let mut buf = [0u8; 64];
    while serve_once(t, &mut state, &mut buf) {}
}

/// The real process entry — illustrative, built only by the image pipeline (nightly +
/// build-std). A `kumo-rt` `Channel` becomes a [`Transport`]; everything above is the
/// shared, host-tested code. Today Sora queues a finite Ping/Status batch on the same
/// channel; true child-side blocking/ports will let this loop stay resident.
///
/// ```ignore
/// struct ChannelTransport { chan: kumo_rt::Handle, port: kumo_rt::Handle }
/// impl Transport for ChannelTransport {
///     fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
///         let src = kumo_rt::port_wait(self.port);          // park until readable / closed
///         if src == kumo_rt::PEER_CLOSED { return None; }
///         Some(kumo_rt::channel_read(self.chan, buf))
///     }
///     fn send(&mut self, frame: &[u8]) { kumo_rt::channel_write(self.chan, frame, &[]); }
/// }
///
/// #[no_mangle]
/// extern "C" fn svc_health_main(request_channel: u64) -> ! {
///     let chan = kumo_rt::Handle(request_channel as u32);
///     let port = kumo_rt::port_create();
///     kumo_rt::port_bind_channel(port, chan);
///     serve(&mut ChannelTransport { chan, port });          // <- the shared loop above
///     kumo_rt::process_exit(0);
/// }
/// ```
pub mod entry {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_round_trips() {
        let mut h = Health::new();
        let reply = h.dispatch(&Request::Ping.encode());
        assert_eq!(Response::decode(&reply), Some(Response::Pong));
    }

    #[test]
    fn status_reports_and_increments_served() {
        let mut h = Health::new();
        h.tick(42);
        let _ = h.dispatch(&Request::Ping.encode()); // served -> 1
        let reply = h.dispatch(&Request::Status.encode()); // served -> 2
        assert_eq!(
            Response::decode(&reply),
            Some(Response::Status {
                uptime_ticks: 42,
                served: 2,
            })
        );
    }

    #[test]
    fn request_opcodes_round_trip() {
        for req in [Request::Ping, Request::Status, Request::Shutdown] {
            assert_eq!(Request::decode(&req.encode()), Some(req));
        }
    }

    #[test]
    fn shutdown_ends_serve_loop_without_replying() {
        use alloc::collections::VecDeque;
        use alloc::vec::Vec;

        struct Mock {
            incoming: VecDeque<Vec<u8>>,
            sent: Vec<Vec<u8>>,
        }
        impl Transport for Mock {
            fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
                let frame = self.incoming.pop_front()?;
                buf[..frame.len()].copy_from_slice(&frame);
                Some(frame.len())
            }
            fn send(&mut self, frame: &[u8]) {
                self.sent.push(frame.to_vec());
            }
        }

        // Ping is served; then Shutdown stops the loop. The Status queued *after* Shutdown
        // must never be served — the loop exits on Shutdown, not on transport drain.
        let mut m = Mock {
            incoming: VecDeque::from([
                Request::Ping.encode(),
                Request::Shutdown.encode(),
                Request::Status.encode(),
            ]),
            sent: Vec::new(),
        };
        serve(&mut m);

        assert_eq!(m.sent.len(), 1, "only Ping replies; Shutdown sends nothing");
        assert_eq!(Response::decode(&m.sent[0]), Some(Response::Pong));
    }

    #[test]
    fn malformed_request_yields_error_not_panic() {
        let mut h = Health::new();
        assert_eq!(Response::decode(&h.dispatch(&[])), Some(Response::Error));
        assert_eq!(
            Response::decode(&h.dispatch(&[0x77])),
            Some(Response::Error)
        );
    }

    #[test]
    fn status_wire_format_round_trips() {
        let r = Response::Status {
            uptime_ticks: 0xDEAD_BEEF,
            served: 7,
        };
        assert_eq!(Response::decode(&r.encode()), Some(r));
    }

    #[test]
    fn truncated_status_decodes_to_none() {
        // A STATUS_OK opcode without the full 16-byte payload must not panic.
        assert_eq!(Response::decode(&[op::STATUS_OK, 0, 0]), None);
    }

    #[test]
    fn end_to_end_ping_then_status_over_transport() {
        use alloc::collections::VecDeque;
        use alloc::vec::Vec;

        // A two-party in-memory transport: requests are dequeued, replies captured. This is
        // the end-to-end smoke minus the OS process boundary — the very `serve` loop that
        // will run over a real kumo-rt Channel once Sora spawns this server.
        struct Mock {
            incoming: VecDeque<Vec<u8>>,
            sent: Vec<Vec<u8>>,
        }
        impl Transport for Mock {
            fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
                let frame = self.incoming.pop_front()?;
                buf[..frame.len()].copy_from_slice(&frame);
                Some(frame.len())
            }
            fn send(&mut self, frame: &[u8]) {
                self.sent.push(frame.to_vec());
            }
        }

        let mut m = Mock {
            incoming: VecDeque::from([Request::Ping.encode(), Request::Status.encode()]),
            sent: Vec::new(),
        };
        serve(&mut m); // runs until the transport drains (recv -> None)

        assert_eq!(m.sent.len(), 2);
        assert_eq!(Response::decode(&m.sent[0]), Some(Response::Pong));
        assert_eq!(
            Response::decode(&m.sent[1]),
            Some(Response::Status {
                uptime_ticks: 0,
                served: 2,
            })
        );
    }
}
