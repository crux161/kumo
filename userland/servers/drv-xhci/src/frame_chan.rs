//j843
//! The frame provider: KNF1 over the channel Sora granted (DESIGN/026 §3, NET7b-2b).
//!
//! Sora's admin channel (bootstrap tag `f`) delivers one frame-channel end per netstack
//! instance. The provider answers RX_FRAMEs within its granted window, queues TX_FRAMEs for
//! the ax88 bulk OUT lane and answers each completion with an in-order TX_DONE, reports
//! LINK, and counts what it drops. Every rule lives in `kumo_netstack::provider`; this file
//! is the driver-shaped shell around it. The rings live in their own statics: a Service
//! built on the stack would cost 24 KiB of the driver's 32 KiB.

use core::cell::UnsafeCell;

use kumo_abi::Handle;
use kumo_netstack::budget::{FRAME_MAX, TX_CREDIT};
use kumo_netstack::frame::{decode, encode, Body, Frame, TxStatus};
use kumo_netstack::provider::Provider;
use kumo_rt::{
    channel_read_with_handle, channel_write, handle_close, handle_koid, port_bind, port_unbind,
};

/// The bootstrap tag bearing Sora's end of the admin channel.
pub(super) const FRAME_BOOTSTRAP_TAG: u8 = b'f';
/// The admin message tag carrying a fresh frame-channel end.
const ADMIN_FRAME_TAG: u8 = b'F';
/// One frame per TX credit may wait for the lane; the provider never accepts more.
const QUEUE: usize = TX_CREDIT;
/// A KNF1 header plus the largest frame.
const MESSAGE_MAX: usize = FRAME_MAX + 32;

struct Service {
    admin: Handle,
    admin_koid: u64,
    channel: Option<Handle>,
    channel_koid: u64,
    provider: Provider,
    mac: Option<[u8; 6]>,
    hello_sent: bool,
    head: usize,
    count: usize,
    /// A submitted frame waits on the bulk OUT lane; its completion answers the TX_DONE.
    in_flight: bool,
    /// The encoded transfer length of the in-flight submission (prefix included).
    transfer: usize,
    /// No bulk OUT before this instant: the reference engine always let the adapter settle
    /// for two seconds after the MAC read before its first frame.
    ready_ns: u64,
}

struct Cell(UnsafeCell<Option<Service>>);
// SAFETY: drv-xhci is single-threaded; `service` is the only way in.
unsafe impl Sync for Cell {}
static SERVICE: Cell = Cell(UnsafeCell::new(None));

fn service() -> &'static mut Option<Service> {
    // SAFETY: single-threaded; no earlier borrow survives a call boundary.
    unsafe { &mut *SERVICE.0.get() }
}

struct QueueCell(UnsafeCell<[[u8; FRAME_MAX]; QUEUE]>);
// SAFETY: drv-xhci is single-threaded; `queue`/`lens` are the only ways in.
unsafe impl Sync for QueueCell {}
static QUEUE_BUF: QueueCell = QueueCell(UnsafeCell::new([[0; FRAME_MAX]; QUEUE]));

struct LensCell(UnsafeCell<[u16; QUEUE]>);
unsafe impl Sync for LensCell {}
static LENS_BUF: LensCell = LensCell(UnsafeCell::new([0; QUEUE]));

fn queue() -> &'static mut [[u8; FRAME_MAX]; QUEUE] {
    // SAFETY: single-threaded; callers finish with one slice before the next call.
    unsafe { &mut *QUEUE_BUF.0.get() }
}

fn lens() -> &'static mut [u16; QUEUE] {
    // SAFETY: single-threaded; callers finish with one slice before the next call.
    unsafe { &mut *LENS_BUF.0.get() }
}

/// Bind the admin channel to the resident port. False when the driver was given none.
pub(super) fn offer_admin(port: Handle, raw: u64) -> bool {
    if raw == 0 || raw == u64::MAX {
        crate::ax88_record(format_args!("NET7b admin absent (raw={:#x})\n", raw));
        return false;
    }
    let admin = Handle(raw as u32);
    let koid = handle_koid(admin);
    if koid == u64::MAX || port_bind(port, admin) != 0 {
        crate::ax88_record(format_args!("NET7b admin bind failed koid={:#x}\n", koid));
        let _ = handle_close(admin);
        return false;
    }
    *service() = Some(Service {
        admin,
        admin_koid: koid,
        channel: None,
        channel_koid: 0,
        provider: Provider::new(),
        mac: None,
        hello_sent: false,
        head: 0,
        count: 0,
        in_flight: false,
        transfer: 0,
        ready_ns: 0,
    });
    crate::ax88_record(format_args!("NET7b admin channel bound koid={:#x}\n", koid));
    true
}

/// The provider needs the adapter's MAC before HELLO can open the generation.
pub(super) fn needs_mac() -> bool {
    service()
        .as_ref()
        .is_some_and(|s| s.channel.is_some() && s.mac.is_none())
}

/// The provider has an open generation: frames may cross.
pub(super) fn active() -> bool {
    service()
        .as_ref()
        .is_some_and(|s| s.channel.is_some() && s.hello_sent)
}

/// Record the adapter's MAC (from the link goal) and open HELLO once it is known.
pub(super) fn set_mac(mac: [u8; 6]) {
    let Some(s) = service().as_mut() else {
        return;
    };
    if s.mac == Some(mac) {
        return;
    }
    s.mac = Some(mac);
    try_hello(s);
}

fn try_hello(s: &mut Service) {
    let (Some(channel), Some(mac), false) = (s.channel, s.mac, s.hello_sent) else {
        return;
    };
    let hello = s.provider.hello(mac);
    if write_message(channel, &hello) {
        s.hello_sent = true;
        s.ready_ns = kumo_rt::clock_get().saturating_add(2_000_000_000);
        crate::ax88_record(format_args!(
            "NET7b frame provider hello gen={} mac={:02x}{:02x}{:02x}{:02x}{:02x}{:02x}\n",
            s.provider.generation(),
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        ));
    }
}

/// One resident-loop wake that is not the controller's interrupt: the admin channel (a grant)
/// or the frame channel (netstack traffic). False when the wake is neither.
pub(super) fn serve_wake(port: Handle, source: u64) -> bool {
    let Some(s) = service().as_mut() else {
        return false;
    };
    if s.admin_koid != 0 && source == s.admin_koid {
        let mut tag = [0u8; 1];
        let (n, raw) = channel_read_with_handle(s.admin, tag.as_mut_ptr(), tag.len());
        if n == 0 {
            // Sora closed the admin link: no further grants will come.
            let _ = port_unbind(port, s.admin);
            let _ = handle_close(s.admin);
            s.admin = Handle(0);
            s.admin_koid = 0;
            return true;
        }
        if raw == 0 || raw == u64::MAX {
            return true;
        }
        let channel = Handle(raw as u32);
        if tag[0] != ADMIN_FRAME_TAG {
            let _ = handle_close(channel);
            return true;
        }
        install(port, s, channel);
        return true;
    }
    if s.channel.is_some() && source == s.channel_koid {
        serve_frame(port, s);
        return true;
    }
    false
}

/// A fresh frame channel replaces the old one: a new channel is a new generation
/// (DESIGN/026 §7), so every queued frame and the old provider state go with it.
fn install(port: Handle, s: &mut Service, channel: Handle) {
    if let Some(old) = s.channel.take() {
        let _ = port_unbind(port, old);
        let _ = handle_close(old);
    }
    let koid = handle_koid(channel);
    if koid == u64::MAX || port_bind(port, channel) != 0 {
        let _ = handle_close(channel);
        return;
    }
    s.channel = Some(channel);
    s.channel_koid = koid;
    s.provider = Provider::new();
    s.head = 0;
    s.count = 0;
    s.in_flight = false;
    s.hello_sent = false;
    crate::ax88_record(format_args!(
        "NET7b frame channel installed koid={:#x}\n",
        koid
    ));
    try_hello(s);
}

fn close_channel(port: Handle, s: &mut Service) {
    if let Some(channel) = s.channel.take() {
        let _ = port_unbind(port, channel);
        let _ = handle_close(channel);
    }
    s.channel_koid = 0;
    s.head = 0;
    s.count = 0;
    s.in_flight = false;
    s.hello_sent = false;
}

fn write_message(channel: Handle, frame: &Frame<'_>) -> bool {
    let mut wire = [0u8; MESSAGE_MAX];
    let Some(n) = encode(frame, &mut wire) else {
        return false;
    };
    channel_write(channel, wire.as_ptr(), n) == 0
}

fn serve_frame(port: Handle, s: &mut Service) {
    let Some(channel) = s.channel else {
        return;
    };
    let mut bytes = [0u8; MESSAGE_MAX];
    let (n, raw) = channel_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len());
    if n == 0 {
        close_channel(port, s);
        return;
    }
    if raw != 0 && raw != u64::MAX {
        let _ = handle_close(Handle(raw as u32));
    }
    let Ok(message) = decode(&bytes[..n], 0) else {
        crate::ax88_record(format_args!("NET7b tx decode drop len={}\n", n));
        return;
    };
    if let Err(violation) = s.provider.accept(&message) {
        // A violation closes the channel; Sora grants a fresh one and the provider re-HELLOs.
        crate::ax88_record(format_args!("NET7b tx violation {violation:?}\n"));
        close_channel(port, s);
        return;
    }
    match message.body {
        Body::TxFrame(frame) => {
            crate::ax88_record(format_args!("NET7b tx frame len={}\n", frame.len()));
            enqueue(s, frame)
        }
        Body::QueryCounters => {
            if let Some(answer) = s.provider.counters_frame() {
                let _ = write_message(channel, &answer);
            }
        }
        _ => {}
    }
}

fn enqueue(s: &mut Service, frame: &[u8]) {
    let Some(channel) = s.channel else {
        return;
    };
    if s.count == QUEUE {
        // Credit bounds the queue depth; a full queue answers DeviceError at once so the
        // provider's in-flight accounting stays exact.
        if let Some(done) = s.provider.tx_done(TxStatus::DeviceError) {
            let _ = write_message(channel, &done);
        }
        return;
    }
    let slot = (s.head + s.count) % QUEUE;
    queue()[slot][..frame.len()].copy_from_slice(frame);
    lens()[slot] = frame.len() as u16;
    s.count += 1;
}

/// The oldest queued frame, copied out for the bulk OUT lane. `None` while empty, before
/// HELLO, or while a completion is still pending.
pub(super) fn next_tx(out: &mut [u8; FRAME_MAX]) -> Option<usize> {
    let s = service().as_mut()?;
    if s.channel.is_none() || !s.hello_sent || s.in_flight || s.count == 0 {
        return None;
    }
    if kumo_rt::clock_get() < s.ready_ns {
        return None;
    }
    let len = usize::from(lens()[s.head]);
    out[..len].copy_from_slice(&queue()[s.head][..len]);
    Some(len)
}

/// Mark the frame copied by `next_tx` as handed to the lane.
pub(super) fn note_submitted(transfer: usize) {
    if let Some(s) = service().as_mut() {
        if s.count > 0 {
            s.in_flight = true;
            s.transfer = transfer;
        }
    }
}

/// One bulk OUT completion: answer the in-flight frame's TX_DONE. False when the completion
/// belongs to the proof schedule instead.
pub(super) fn note_complete(actual: usize) -> bool {
    let Some(s) = service().as_mut() else {
        return false;
    };
    if !s.in_flight {
        return false;
    }
    let status = if actual == s.transfer {
        TxStatus::HandedToBus
    } else {
        TxStatus::DeviceError
    };
    crate::ax88_record(format_args!(
        "NET7b tx done actual={} transfer={} {}\n",
        actual,
        s.transfer,
        if actual == s.transfer {
            "handed"
        } else {
            "error"
        }
    ));
    s.in_flight = false;
    s.head = (s.head + 1) % QUEUE;
    s.count -= 1;
    if let Some(channel) = s.channel {
        if let Some(done) = s.provider.tx_done(status) {
            let answers = match done.body {
                kumo_netstack::frame::Body::TxDone { answers, .. } => answers,
                _ => 0,
            };
            crate::ax88_record(format_args!("NET7b tx answer={}\n", answers));
            let _ = write_message(channel, &done);
        }
    }
    kumo_ax88::link::request_rearm();
    true
}

/// Forward one RX aggregate to netstack as RX_FRAMEs within the granted window.
pub(super) fn forward_rx(bytes: &[u8]) {
    let Some(s) = service().as_mut() else {
        return;
    };
    let Some(channel) = s.channel else {
        return;
    };
    if !s.hello_sent {
        return;
    }
    let Ok(aggregate) = kumo_ax88::rx::Aggregate::parse(bytes) else {
        s.provider.note_rx_bad();
        return;
    };
    for packet in aggregate.packets() {
        match packet {
            kumo_ax88::rx::Packet::Frame { bytes: frame, .. } => {
                if let Some(rx) = s.provider.send_rx(frame) {
                    let _ = write_message(channel, &rx);
                }
            }
            kumo_ax88::rx::Packet::Dropped { .. } => s.provider.note_rx_bad(),
            kumo_ax88::rx::Packet::Dummy => {}
        }
    }
}

/// A link-status change reached the driver; report it without polling.
pub(super) fn link(up: bool) {
    let Some(s) = service().as_mut() else {
        return;
    };
    let Some(channel) = s.channel else {
        return;
    };
    if !s.hello_sent {
        return;
    }
    // The ax88 autonegotiates full duplex on this link; the status word's speed is not
    // decoded yet, so the rate is reported as unknown.
    if let Some(frame) = s.provider.link(up, true, 0) {
        let _ = write_message(channel, &frame);
    }
}
