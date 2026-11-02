#![no_std]
#![no_main]
//j709

//! QEMU-only fake client for G8d-a's contained-close smoke. It drives the fake server through
//! the real `kumo-gpu` request frames — two buffers, two bindings, one group and one armed
//! fence wait — then parks on the channel until Sora kills it. The kill is the point: the
//! kernel closes its endpoint, which is the only signal the server acts on. No DMA is mapped
//! and no VMO handle is expected back. — TIMBERDOODLE

use kumo_abi::Handle;
use kumo_gpu::{
    BindFlags, Binding, Command, Reply, Request, Response, PAGE_BYTES, REPLY_LEN, VA_START,
};

kumo_rt::entry!(main);

fn handle(raw: u64) -> Option<Handle> {
    (raw != 0 && (raw as i64) > 0).then_some(Handle(raw as u32))
}

fn binding(buffer: u32, va: u64) -> Binding {
    Binding {
        vm: 1,
        buffer,
        va,
        offset: 0,
        bytes: PAGE_BYTES,
        flags: BindFlags(BindFlags::READ_ONLY | BindFlags::NO_EXECUTE),
    }
}

/// One synchronous request/reply. The fake server never grants a handle, but a malformed or
/// foreign one is closed rather than leaked, and every reply must answer this exact ID.
fn request(channel: Handle, id: u32, command: Command) -> Option<Response> {
    let frame = Request { id, command }.encode();
    if kumo_rt::channel_write(channel, frame.as_ptr(), frame.len()) != 0 {
        return None;
    }
    let mut bytes = [0u8; REPLY_LEN + 1];
    let (len, raw_handle) =
        kumo_rt::channel_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len());
    if let Some(handle) = handle(raw_handle) {
        let _ = kumo_rt::handle_close(handle);
    }
    let reply = Reply::decode(bytes.get(..len)?)?;
    if reply.id != id {
        return None;
    }
    reply.result.ok()
}

fn setup(channel: Handle) -> bool {
    if !matches!(
        request(channel, 1, Command::VmCreate),
        Some(Response::VmCreated { vm: 1 })
    ) {
        return false;
    }
    if !matches!(
        request(channel, 2, Command::BufferCreate { bytes: PAGE_BYTES }),
        Some(Response::BufferCreated {
            buffer: 1,
            bytes: PAGE_BYTES
        })
    ) {
        return false;
    }
    if !matches!(
        request(
            channel,
            3,
            Command::BufferCreate {
                bytes: 2 * PAGE_BYTES
            }
        ),
        Some(Response::BufferCreated { buffer: 2, bytes }) if bytes == 2 * PAGE_BYTES
    ) {
        return false;
    }
    if !matches!(
        request(channel, 4, Command::VmBind(binding(1, VA_START))),
        Some(Response::VmBound)
    ) {
        return false;
    }
    if !matches!(
        request(
            channel,
            5,
            Command::VmBind(binding(2, VA_START + PAGE_BYTES))
        ),
        Some(Response::VmBound)
    ) {
        return false;
    }
    if !matches!(
        request(channel, 6, Command::GroupCreate { vm: 1 }),
        Some(Response::GroupCreated { group: 1 })
    ) {
        return false;
    }
    if !matches!(
        request(
            channel,
            7,
            Command::Submit(kumo_gpu::Submission {
                group: 1,
                stream_va: VA_START,
                stream_bytes: 48,
                signal_point: 1,
            })
        ),
        Some(Response::Submitted { point: 1 })
    ) {
        return false;
    }
    matches!(
        request(channel, 8, Command::FenceWait { group: 1, point: 1 }),
        Some(Response::WaitArmed { point: 1 })
    )
}

#[no_mangle]
extern "C" fn main(channel_raw: u64, _report: u64) -> ! {
    let Some(channel) = handle(channel_raw) else {
        kumo_rt::process_exit(1)
    };
    if !setup(channel) {
        let _ = kumo_rt::log_write(
            kumo_abi::sys::log_level::ERR,
            b"gpu-contain-client: setup failed\n",
        );
        kumo_rt::process_exit(1)
    }
    let _ = kumo_rt::log_write(
        kumo_abi::sys::log_level::NOTICE,
        b"gpu-contain-client: two buffers, two bindings, one wait armed; waiting for the supervisor's kill\n",
    );
    // No further traffic is expected. Blocking here is the point: the session is armed and
    // the supervisor must kill this process to make the server see a peer close.
    let mut bytes = [0u8; REPLY_LEN + 1];
    loop {
        let (len, raw_handle) =
            kumo_rt::channel_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len());
        if let Some(handle) = handle(raw_handle) {
            let _ = kumo_rt::handle_close(handle);
        }
        if len == 0 {
            break;
        }
    }
    let _ = kumo_rt::handle_close(channel);
    kumo_rt::process_exit(0)
}
