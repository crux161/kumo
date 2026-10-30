#![no_std]
#![no_main]
//j646
//j647
//j652
//j682
//j683
//j711
//j712
//j713
//j714

//! One cooperative session granted by Sora in x0, with no Resource, MMIO or firmware handle.
//! Recovery class: soft-state. On any error exit and let drv-gpu contain the session.

use core::fmt::{self, Write};
use gpu_client::{
    bindings, compute_data, compute_stream, proof_stream, retarget_paint, triangle_data,
    triangle_stream, BINARY_OFFSET, BUFFER_BYTES, CHURN_VA, DATA_OFFSET, FRAME_BYTES, PAINT_OFFSET,
    PIXEL_POISON, SCRATCH_OFFSET, TRIANGLE_BINARY_OFFSET, TRIANGLE_DATA_OFFSET, VALUES,
};
use kumo_abi::{Handle, VmarFlags};
use kumo_gpu::{
    client::{Client, Error as ClientError, FenceStatus, Transport, TransportError},
    BindFlags, Binding, Status as GpuStatus, Submission, Unbind, REPLY_LEN, REQUEST_LEN, VA_END,
    VA_START,
};
use kumo_graphics::{
    DamageSet, Present, Reply as PresentReply, Status as PresentStatus, Surface, XRGB8888,
};

use kumo_mali::{
    check_paint, check_triangle, paint_reference_checksum, triangle_reference_checksum, CsFeatures,
    KUMO_PAINT_V10, KUMO_TRIANGLE_V10, PAINT_BYTES, PAINT_HEIGHT, PAINT_WIDTH,
};

kumo_rt::entry!(main);

struct Text {
    bytes: [u8; 256],
    len: usize,
}
impl Write for Text {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self
            .len
            .checked_add(text.len())
            .filter(|end| *end <= self.bytes.len())
            .ok_or(fmt::Error)?;
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}
fn log_at(level: u8, args: fmt::Arguments<'_>) {
    let mut text = Text {
        bytes: [0; 256],
        len: 0,
    };
    let _ = text.write_fmt(args);
    let _ = kumo_rt::log_write(level, &text.bytes[..text.len]);
}
fn log(args: fmt::Arguments<'_>) {
    log_at(kumo_abi::sys::log_level::NOTICE, args);
}
fn handle(raw: u64) -> Option<Handle> {
    (raw != 0 && (raw as i64) > 0).then_some(Handle(raw as u32))
}

//j711
/// The lane Sora named on the bootstrap pair. `Native` is the J0c diagnostic channel (the
/// handle is 0 when the image has no presenter); `Churn` is view `GPU_VIEW`'s production
/// channel and surface, handed to this one cooperative client for the G8 loop.
//j714 A `Churn` lane's native producer may be handle zero: a session regranted after a
/// supervised exit runs no J0c diagnostic (DESIGN/024), and only the boot session's three
/// native presentations set drv-fb's diagnostic-completed flag.
#[derive(Clone, Copy)]
enum Lane {
    Native(Handle),
    Churn {
        channel: Handle,
        surface: Handle,
        presentation: Handle,
    },
}

//j711
/// Read the lane record: a leading tag, then that lane's handles. A closed bootstrap, an
/// invalid handle or a missing surface fails named; nothing else travels this pair.
/// — TIMBERDOODLE
fn select_lane(bootstrap: Handle) -> Result<Lane, ()> {
    let mut tag = [0u8; 1];
    let (len, raw) = kumo_rt::channel_read_with_handle(bootstrap, tag.as_mut_ptr(), 1);
    let fail = |cause: &str| -> Result<Lane, ()> {
        log_at(
            kumo_abi::sys::log_level::ERR,
            format_args!("gpu-client: lane selection failed: {}\n", cause),
        );
        Err(())
    };
    if len == 0 || (len as isize) < 0 {
        return fail("bootstrap closed before a tag");
    }
    if len != 1 {
        return fail("bootstrap tag was not one byte");
    }
    match tag[0] {
        b'N' => {
            // The normal lane may honestly have no presenter; that is today's J0c-less run.
            if raw == 0 {
                return Ok(Lane::Native(Handle(0)));
            }
            match handle(raw) {
                Some(native) => Ok(Lane::Native(native)),
                None => fail("native lane carried an invalid handle"),
            }
        }
        b'V' => {
            let Some(channel) = handle(raw) else {
                return fail("churn lane carried no view channel");
            };
            let (len, raw) = kumo_rt::channel_read_with_handle(bootstrap, tag.as_mut_ptr(), 1);
            if len == 0 || (len as isize) < 0 {
                let _ = kumo_rt::handle_close(channel);
                return fail("churn lane closed before its surface");
            }
            if len != 1 || tag[0] != b'S' {
                let _ = kumo_rt::handle_close(channel);
                return fail("churn lane surface tag missing");
            }
            let Some(surface) = handle(raw) else {
                let _ = kumo_rt::handle_close(channel);
                return fail("churn lane carried no view surface");
            };
            // j713: the churn lane also carries the native diagnostic producer; without it
            // the display diagnostic cannot complete and the gate can never arm. — TIMBERDOODLE
            let (len, raw) = kumo_rt::channel_read_with_handle(bootstrap, tag.as_mut_ptr(), 1);
            if len == 0 || (len as isize) < 0 {
                let _ = kumo_rt::handle_close(channel);
                let _ = kumo_rt::handle_close(surface);
                return fail("churn lane closed before its native producer");
            }
            if len != 1 || tag[0] != b'N' {
                let _ = kumo_rt::handle_close(channel);
                let _ = kumo_rt::handle_close(surface);
                return fail("churn lane native tag missing");
            }
            //j714 Handle zero is the honest no-producer churn lane (DESIGN/024): a
            // regranted session carries none. Only a malformed nonzero value fails.
            let presentation = if raw == 0 {
                Handle(0)
            } else {
                match handle(raw) {
                    Some(presentation) => presentation,
                    None => {
                        let _ = kumo_rt::handle_close(channel);
                        let _ = kumo_rt::handle_close(surface);
                        return fail("churn lane carried an invalid native producer");
                    }
                }
            };
            Ok(Lane::Churn {
                channel,
                surface,
                presentation,
            })
        }
        _ => fail("unknown lane tag"),
    }
}
fn barrier() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}
struct Channel {
    channel: Handle,
}
impl Transport for Channel {
    type Handle = Handle;
    fn send(&mut self, bytes: &[u8; REQUEST_LEN]) -> Result<(), TransportError> {
        let status = kumo_rt::channel_write(self.channel, bytes.as_ptr(), bytes.len());
        if status == 0 {
            Ok(())
        } else if status == kumo_abi::Errno::PeerClosed.status() {
            Err(TransportError::Closed)
        } else {
            Err(TransportError::Failed)
        }
    }
    fn receive(
        &mut self,
        bytes: &mut [u8; REPLY_LEN + 1],
    ) -> Result<(usize, Option<Handle>), TransportError> {
        let (len, raw) =
            kumo_rt::channel_read_with_handle(self.channel, bytes.as_mut_ptr(), bytes.len());
        let grant = handle(raw);
        if len == 0 || (len as isize) < 0 || (raw != 0 && grant.is_none()) {
            if let Some(grant) = grant {
                let _ = kumo_rt::handle_close(grant);
            }
            return Err(if len == 0 {
                TransportError::Closed
            } else {
                TransportError::Failed
            });
        }
        Ok((len, grant))
    }
    fn close_handle(&mut self, handle: Handle) {
        let _ = kumo_rt::handle_close(handle);
    }
}

/// All writes target the retained Normal-NC mapping returned for BUFFER_BYTES in run.
fn upload(cpu: u64, offset: u64, bytes: &[u8]) -> Result<(), ()> {
    if offset
        .checked_add(bytes.len() as u64)
        .is_none_or(|end| end > BUFFER_BYTES)
    {
        return Err(());
    }
    for (i, byte) in bytes.iter().enumerate() {
        // SAFETY: caller's mapped buffer, with the byte extent checked above.
        unsafe {
            ((cpu + offset + i as u64) as *mut u8).write_volatile(*byte);
        }
    }
    Ok(())
}

fn submit_wait(
    client: &mut Client<Channel>,
    cpu: u64,
    group: u32,
    point: u64,
    stream: &[u64],
) -> Result<(), Failure> {
    if stream.len() * 8 > kumo_gpu::PAGE_BYTES as usize {
        return Err(Failure::Failed);
    }
    for (i, word) in stream.iter().enumerate() {
        // SAFETY: the validated command page, reused only after the last completed point.
        unsafe {
            ((cpu + i as u64 * 8) as *mut u64).write_volatile(*word);
        }
    }
    barrier();
    // G8c-b: the first real fence consumer. The wait is registered on the gpu-done
    // timeline and this thread parks on its own wake channel until the driver's completion
    // path signals it; a Port timer bounds the wait so a dead producer cannot wedge the
    // client (DESIGN/023, ratified). SyncRead remains the library's fallback path.
    // — TIMBERDOODLE
    let fence = client.submit_fence(Submission {
        group,
        stream_va: VA_START,
        stream_bytes: (stream.len() * 8) as u32,
        signal_point: point,
    })?;
    match client.fence_wait(fence)? {
        FenceStatus::Done { gpu_status: 0 } => {
            barrier();
            Ok(())
        }
        FenceStatus::Done { .. } => Err(Failure::Failed),
        FenceStatus::Armed(wake) => {
            if wait_wake(wake) {
                barrier();
                Ok(())
            } else {
                Err(Failure::Failed)
            }
        }
    }
}

/// Park on an armed fence wake channel with a 5 s upper bound, driven by the Port/timer
/// wake the ratification chose. A closed channel without its byte is a failure, not a
/// wake: the driver closes armed ends it could not complete at teardown. The wake handle
/// is consumed either way. — TIMBERDOODLE
fn wait_wake(wake: Handle) -> bool {
    let Some(port) = handle(kumo_rt::port_create()) else {
        let _ = kumo_rt::handle_close(wake);
        return false;
    };
    let Some(timer) = handle(kumo_rt::timer_create(5_000_000_000)) else {
        let _ = kumo_rt::handle_close(wake);
        let _ = kumo_rt::handle_close(port);
        return false;
    };
    let wake_koid = kumo_rt::handle_koid(wake);
    let bound = kumo_rt::handle_koid(timer);
    let bound_ok = bound != u64::MAX
        && wake_koid != u64::MAX
        && kumo_rt::port_bind(port, wake) == 0
        && kumo_rt::port_bind(port, timer) == 0;
    let mut woke = false;
    if bound_ok && kumo_rt::port_wait(port) == wake_koid {
        let mut byte = [0u8; 1];
        let n = kumo_rt::channel_read(wake, byte.as_mut_ptr(), byte.len());
        woke = n == 1 && byte[0] == 1;
    }
    let _ = kumo_rt::port_unbind(port, wake);
    let _ = kumo_rt::port_unbind(port, timer);
    let _ = kumo_rt::handle_close(timer);
    let _ = kumo_rt::handle_close(wake);
    let _ = kumo_rt::handle_close(port);
    woke
}

//j711
/// The counted acceptance run: 100 frames, each a fresh 160 KiB buffer at [`CHURN_VA`].
const CHURN_FRAMES: u32 = 100;
//j714 The restarted churn lane ran no native frames, so its timeline resumes past the
/// bring-up paint at point 3: DESIGN/024 §4 names point 4 without the J0c three and point 7
/// with them (the session timeline requires exactly the next point). — TIMBERDOODLE
const CHURN_FIRST_POINT_NO_NATIVE: u64 = 4;
/// The view-class surface drv-gpu presents its G5b paint through, replicated exactly:
/// 256x128 linear XRGB8888 with stride 256 (`PAINT_SURFACE` in
/// userland/servers/drv-gpu/src/lib.rs). drv-fb refuses a mismatched surface, so a drift
/// here fails the first present loudly instead of painting wrong pixels. — TIMBERDOODLE
const CHURN_SURFACE: Surface = Surface {
    width: PAINT_WIDTH,
    height: PAINT_HEIGHT,
    stride: PAINT_WIDTH,
    format: XRGB8888,
};

/// One named churn abort: frame index, the display status when the failure carried one,
/// and the cause. This is the run's ERR summary on failure. — TIMBERDOODLE
fn churn_abort(frame: u32, status: Option<PresentStatus>, cause: &str) -> Failure {
    log_at(
        kumo_abi::sys::log_level::ERR,
        format_args!(
            "gpu-churn: failed frame={} status={:?} cause={}\n",
            frame, status, cause
        ),
    );
    Failure::Failed
}

/// The same record for a session-API failure, keeping the server status distinguishable.
fn churn_api_fail(frame: u32, cause: &str, error: ClientError) -> Failure {
    match error {
        ClientError::Server(status) => log_at(
            kumo_abi::sys::log_level::ERR,
            format_args!(
                "gpu-churn: failed frame={} status={:?} cause={}\n",
                frame, status, cause
            ),
        ),
        other => log_at(
            kumo_abi::sys::log_level::ERR,
            format_args!(
                "gpu-churn: failed frame={} status=none cause={} error={:?}\n",
                frame, cause, other
            ),
        ),
    }
    Failure::Failed
}

/// Fill the image extent with the poison word before every submission, exactly as the
/// bring-up and J0c lanes do. — TIMBERDOODLE
fn poison_churn(cpu: u64) {
    for offset in (0..PAINT_BYTES).step_by(4) {
        // SAFETY: the caller's churn mapping covers at least PAINT_BYTES.
        unsafe {
            ((cpu + offset) as *mut u32).write_volatile(PIXEL_POISON);
        }
    }
}

/// Volatile-copy the painted first PAINT_BYTES of the churn buffer into the view surface in
/// the VmoWrite syscall's 256-byte chunks (64 pixels, drv-gpu's `surface_chunks` shape).
/// — TIMBERDOODLE
fn copy_paint_to_surface(cpu: u64, surface: Handle) -> Result<(), &'static str> {
    for offset in (0..PAINT_BYTES).step_by(256) {
        let mut bytes = [0u8; 256];
        for (index, pixel) in bytes.chunks_exact_mut(4).enumerate() {
            // SAFETY: a word of the churn mapping; the loop stays inside PAINT_BYTES.
            let value =
                unsafe { ((cpu + offset + index as u64 * 4) as *const u32).read_volatile() };
            pixel.copy_from_slice(&value.to_le_bytes());
        }
        if kumo_rt::vmo_write(surface, offset, bytes.as_ptr(), bytes.len()) != 0 {
            return Err("view surface write failed");
        }
    }
    Ok(())
}

/// 64 evenly spaced pixels: the middle frames take this cheap sample while frames 1 and
/// 100 compare all 32768. Returns the first wrong (x, y, got, want). — TIMBERDOODLE
fn sample_mismatch(cpu: u64) -> Option<(u32, u32, u32, u32)> {
    const SAMPLES: u32 = 64;
    for index in 0..SAMPLES {
        let pixel_index = index * (PAINT_WIDTH * PAINT_HEIGHT / SAMPLES);
        let (x, y) = (pixel_index % PAINT_WIDTH, pixel_index / PAINT_WIDTH);
        // SAFETY: `pixel_index` lies inside the 256x128 image.
        let got =
            unsafe { ((cpu + u64::from(y * PAINT_WIDTH + x) * 4) as *const u32).read_volatile() };
        let want = kumo_mali::paint_pixel(x, y);
        if got != want {
            return Some((x, y, got, want));
        }
    }
    None
}

/// Open one reply port for `channel` with a one-shot timer bound. Only these two objects
/// are bound, so the wait can never mistake another source for this reply. — TIMBERDOODLE
fn open_reply_port(
    channel: Handle,
    timeout_ns: u64,
) -> Result<(Handle, Handle, u64, u64), &'static str> {
    let port = handle(kumo_rt::port_create()).ok_or("reply port unavailable")?;
    let Some(timer) = handle(kumo_rt::timer_create(timeout_ns)) else {
        let _ = kumo_rt::handle_close(port);
        return Err("reply timer unavailable");
    };
    let channel_koid = kumo_rt::handle_koid(channel);
    let timer_koid = kumo_rt::handle_koid(timer);
    if channel_koid == u64::MAX
        || timer_koid == u64::MAX
        || kumo_rt::port_bind(port, channel) != 0
        || kumo_rt::port_bind(port, timer) != 0
    {
        let _ = kumo_rt::handle_close(timer);
        let _ = kumo_rt::handle_close(port);
        return Err("reply port bind failed");
    }
    Ok((port, timer, channel_koid, timer_koid))
}

fn close_reply_port(port: Handle, timer: Handle, channel: Handle) {
    let _ = kumo_rt::port_unbind(port, channel);
    let _ = kumo_rt::port_unbind(port, timer);
    let _ = kumo_rt::handle_close(timer);
    let _ = kumo_rt::handle_close(port);
}

/// Wait for the reply carrying `sequence`; the timer bound is the port's second source.
/// A reply carries no handle, and any other wake is ignored because it cannot be ours.
fn await_present_reply(
    channel: Handle,
    port: Handle,
    channel_koid: u64,
    timer_koid: u64,
    sequence: u32,
) -> Result<PresentStatus, &'static str> {
    let mut bytes = [0u8; kumo_graphics::REPLY_LEN + 1];
    loop {
        let source = kumo_rt::port_wait(port);
        if source == channel_koid {
            let (len, extra) =
                kumo_rt::channel_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len());
            if extra != 0 {
                if let Some(extra) = handle(extra) {
                    let _ = kumo_rt::handle_close(extra);
                }
                return Err("present reply carried a handle");
            }
            if len == 0 {
                return Err("present channel closed");
            }
            if (len as isize) < 0 {
                return Err("present channel read failed");
            }
            let Some(reply) = bytes.get(..len).and_then(PresentReply::decode) else {
                return Err("malformed present reply");
            };
            if reply.sequence != sequence {
                return Err("present reply sequence mismatch");
            }
            return Ok(reply.status);
        }
        if source == timer_koid {
            return Err("present reply timed out");
        }
        // Any other source is not bound here; keep waiting for ours.
    }
}

/// One counted present: a fresh sequence with FULL damage, then the reply under a 5 s bound.
fn present_counted(channel: Handle, sequence: u32) -> Result<PresentStatus, &'static str> {
    let frame = Present {
        sequence,
        surface: CHURN_SURFACE,
        damage: DamageSet::FULL,
    }
    .encode();
    if kumo_rt::channel_write(channel, frame.as_ptr(), frame.len()) != 0 {
        return Err("present request write failed");
    }
    let (port, timer, channel_koid, timer_koid) = open_reply_port(channel, 5_000_000_000)?;
    let status = await_present_reply(channel, port, channel_koid, timer_koid, sequence);
    close_reply_port(port, timer, channel);
    status
}

//j711
/// The churn lane starts during Sora's boot, while drv-fb is still measuring D1b's vblank
/// cadence; a tight probe loop perturbs that measurement (metal 2026-10-30: median 61.120 Hz
/// against mode 60.000, so the diagnostic correctly refused to complete and the gate stayed
/// unarmed). Wait out the quiet window first, then present once per second so the probe is at
/// most a rounding error in any frame count. A run that never sees `Retired` aborts named
/// when the bound expires. — TIMBERDOODLE
const CHURN_READY_QUIET_NS: u64 = 20_000_000_000;
const CHURN_READY_PACE_NS: u64 = 1_000_000_000;
const CHURN_READY_BOUND_NS: u64 = 180_000_000_000;

/// The vblank gate arms only after drv-fb's start-up display diagnostic completes, so the
/// first replies may honestly be `Copied`; present the first frame with fresh sequences
/// until the first `Retired`. — TIMBERDOODLE
fn churn_readiness(channel: Handle, sequence: &mut u32, probes: &mut u32) -> Result<(), Failure> {
    let _ = kumo_rt::sleep_ns(CHURN_READY_QUIET_NS);
    let (port, timer, channel_koid, timer_koid) =
        match open_reply_port(channel, CHURN_READY_BOUND_NS) {
            Ok(parts) => parts,
            Err(cause) => return Err(churn_abort(0, None, cause)),
        };
    let result = loop {
        *sequence = (*sequence).wrapping_add(1);
        *probes = (*probes).saturating_add(1);
        let frame = Present {
            sequence: *sequence,
            surface: CHURN_SURFACE,
            damage: DamageSet::FULL,
        }
        .encode();
        if kumo_rt::channel_write(channel, frame.as_ptr(), frame.len()) != 0 {
            break Err(churn_abort(0, None, "readiness present write failed"));
        }
        match await_present_reply(channel, port, channel_koid, timer_koid, *sequence) {
            Ok(PresentStatus::Retired) => break Ok(()),
            Ok(PresentStatus::Copied) => {
                let _ = kumo_rt::sleep_ns(CHURN_READY_PACE_NS);
                continue;
            }
            Ok(status) => break Err(churn_abort(0, Some(status), "readiness reply")),
            Err(cause) => break Err(churn_abort(0, None, cause)),
        }
    };
    close_reply_port(port, timer, channel);
    result
}

//j711
/// The G8 acceptance loop on the production view path: create one buffer per frame, bind it
/// at [`CHURN_VA`], paint, wait on the session fence (never SyncRead), verify, copy into the
/// view surface, present with a fresh sequence and FULL damage, count only `Retired`, then
/// destroy the buffer. — TIMBERDOODLE
fn churn_run(
    client: &mut Client<Channel>,
    cpu: u64,
    vm: u32,
    group: u32,
    view: Handle,
    surface: Handle,
    compute: &[u64; 24],
    first_point: u64,
) -> Result<(), Failure> {
    // The bring-up paint (point 3) is the first frame's content: copy it once, probe until
    // the gate answers Retired, then count 100 fresh frames.
    copy_paint_to_surface(cpu + PAINT_OFFSET, surface)
        .map_err(|cause| churn_abort(0, None, cause))?;
    let mut sequence = 0u32;
    let mut probes = 0u32;
    churn_readiness(view, &mut sequence, &mut probes)?;
    log_at(
        kumo_abi::sys::log_level::INFO,
        format_args!(
            "gpu-churn: readiness retired probes={} sequence={}\n",
            probes, sequence
        ),
    );
    let mut retired = 0u32;
    let mut copied = 0u32;
    for frame in 1..=CHURN_FRAMES {
        let point = first_point + u64::from(frame) - 1;
        let buffer = client
            .create_buffer(FRAME_BYTES)
            .map_err(|error| churn_api_fail(frame, "buffer create", error))?;
        let (status, churn_cpu) = kumo_rt::vmar_map_anywhere(
            buffer.handle,
            buffer.bytes,
            (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
        );
        if status != 0 || churn_cpu == 0 {
            let _ = kumo_rt::handle_close(buffer.handle);
            return Err(churn_abort(frame, None, "churn buffer map failed"));
        }
        poison_churn(churn_cpu);
        barrier();
        client
            .bind(Binding {
                vm,
                buffer: buffer.id,
                va: CHURN_VA,
                offset: 0,
                bytes: FRAME_BYTES,
                // The paint target needs write access and no execute; this mirrors the
                // fixed PAINT_OFFSET binding's flags. — TIMBERDOODLE
                flags: BindFlags(BindFlags::NO_EXECUTE | BindFlags::UNCACHED),
            })
            .map_err(|error| churn_api_fail(frame, "churn bind", error))?;
        // Descriptors stay in the fixed data page the stream names; only the output word
        // moves, and the volatile upload plus barrier make it visible to the GPU.
        let mut data = compute_data()
            .map_err(|_| churn_abort(frame, None, "paint descriptor rebuild failed"))?;
        retarget_paint(&mut data, CHURN_VA)
            .map_err(|_| churn_abort(frame, None, "paint retarget refused"))?;
        upload(cpu, DATA_OFFSET, &data)
            .map_err(|()| churn_abort(frame, None, "paint descriptor upload failed"))?;
        submit_wait(client, cpu, group, point, compute)
            .map_err(|_| churn_abort(frame, None, "paint fence wait failed"))?;
        let read = |x, y| unsafe {
            ((churn_cpu + u64::from(y * PAINT_WIDTH + x) * 4) as *const u32).read_volatile()
        };
        let first = if frame == 1 || frame == CHURN_FRAMES {
            check_paint(read).first
        } else {
            sample_mismatch(churn_cpu)
        };
        if let Some((x, y, got, want)) = first {
            let _ = kumo_rt::handle_close(buffer.handle);
            log_at(
                kumo_abi::sys::log_level::ERR,
                format_args!(
                    "gpu-churn: failed frame={} status=none cause=pixel mismatch ({},{}) got0x{:08x} want0x{:08x}\n",
                    frame, x, y, got, want
                ),
            );
            return Err(Failure::Failed);
        }
        copy_paint_to_surface(churn_cpu, surface)
            .map_err(|cause| churn_abort(frame, None, cause))?;
        barrier();
        sequence = sequence.wrapping_add(1);
        let reply =
            present_counted(view, sequence).map_err(|cause| churn_abort(frame, None, cause))?;
        match reply {
            PresentStatus::Retired => retired += 1,
            PresentStatus::Copied => copied += 1,
            status => return Err(churn_abort(frame, Some(status), "present refused")),
        }
        if frame.is_multiple_of(25) {
            log_at(
                kumo_abi::sys::log_level::INFO,
                format_args!(
                    "gpu-churn: frame={} retired={} copied={} sequence={}\n",
                    frame, retired, copied, sequence
                ),
            );
        }
        // G8b is live (j711): even frames unbind through opcode 15 and destroy with an empty
        // batch, odd frames exercise destroy's implicit batch. Both are legal with the group
        // live, and the surface copy was already consumed above. — TIMBERDOODLE
        if frame % 2 == 0 {
            client
                .unbind(Unbind {
                    vm,
                    va: CHURN_VA,
                    bytes: FRAME_BYTES,
                })
                .map_err(|error| churn_api_fail(frame, "churn unbind", error))?;
        }
        client
            .destroy_buffer(&buffer)
            .map_err(|error| churn_api_fail(frame, "buffer destroy", error))?;
        let _ = kumo_rt::handle_close(buffer.handle);
    }
    log_at(
        kumo_abi::sys::log_level::NOTICE,
        format_args!(
            "gpu-churn: retired={} copied={} frames={} bytes={} probes={} va=0x{:x}\n",
            retired,
            copied,
            CHURN_FRAMES,
            u64::from(CHURN_FRAMES) * FRAME_BYTES,
            probes,
            CHURN_VA
        ),
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    Failed,
    Cancelled,
}
impl From<()> for Failure {
    fn from(_: ()) -> Self {
        Self::Failed
    }
}
impl From<ClientError> for Failure {
    fn from(error: ClientError) -> Self {
        log(format_args!(
            "gpu-client: native GPU API failed: {:?}\n",
            error
        ));
        Self::Failed
    }
}

fn present_image(
    channel: Handle,
    vmo: Handle,
    image: kumo_graphics::native::Image,
    lease: &mut kumo_graphics::native::Lease,
) -> Result<(), Failure> {
    if !lease.begin(image, kumo_rt::handle_koid(vmo)) {
        return Err(Failure::Failed);
    }
    let grant = handle(kumo_rt::handle_duplicate(
        vmo,
        kumo_abi::Rights::READ | kumo_abi::Rights::TRANSFER,
    ))
    .ok_or(())?;
    // G8c-b2: register the frame-retired wait for this sequence before publishing the
    // image, so the arm precedes the consumed load. The pair gate: the GPU point
    // (submit_wait) and this frame-retired wake both precede any reuse. — TIMBERDOODLE
    let wait = kumo_graphics::native::RetireWait {
        point: u64::from(image.sequence),
    };
    let wait_frame = wait.encode();
    if kumo_rt::channel_write(channel, wait_frame.as_ptr(), wait_frame.len()) != 0 {
        let _ = kumo_rt::handle_close(grant);
        return Err(Failure::Failed);
    }
    let bytes = image.encode();
    if kumo_rt::channel_write_with_handle(channel, bytes.as_ptr(), bytes.len(), grant) != 0 {
        let _ = kumo_rt::handle_close(grant);
        return Err(Failure::Failed);
    }
    log(format_args!("gpu-client: J0c sequence{} point{} leased READ-only; GPU/CPU writes blocked until matching retirement\n",image.sequence,image.gpu_point));
    let port = handle(kumo_rt::port_create()).ok_or(())?;
    let timer = handle(kumo_rt::timer_create(120_000_000_000)).ok_or(())?;
    if kumo_rt::port_bind(port, channel) != 0 || kumo_rt::port_bind(port, timer) != 0 {
        return Err(Failure::Failed);
    }
    let channel_koid = kumo_rt::handle_koid(channel);
    let mut bytes = [0; kumo_graphics::native::REQUEST_LEN + 1];
    let mut wake: Option<Handle> = None;
    let mut armed = false;
    let mut woken = false;
    let mut reply: Option<kumo_graphics::native::Reply> = None;
    // Both timeline signals arrive on this port: the wait reply and the retirement reply
    // on the native channel, the frame-retired wake byte on its own channel. Reuse is held
    // until the retirement reply AND the wake are both observed (the pair gate).
    loop {
        let source = kumo_rt::port_wait(port);
        if source == channel_koid {
            let (n, extra) =
                kumo_rt::channel_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len());
            let extra = handle(extra);
            if let Some(wait_reply) = bytes
                .get(..n)
                .and_then(kumo_graphics::native::WaitReply::decode)
            {
                if let Some(extra) = extra {
                    if wake.is_some() || kumo_rt::port_bind(port, extra) != 0 {
                        let _ = kumo_rt::handle_close(extra);
                        return Err(Failure::Failed);
                    }
                    wake = Some(extra);
                }
                if wait_reply.armed != wake.is_some() {
                    return Err(Failure::Failed);
                }
                armed = wait_reply.armed;
            } else if let Some(image_reply) = bytes
                .get(..n)
                .and_then(kumo_graphics::native::Reply::decode)
            {
                if extra.is_some() {
                    return Err(Failure::Failed);
                }
                reply = Some(image_reply);
                if !armed {
                    break;
                }
                if woken {
                    break;
                }
            } else {
                return Err(Failure::Failed);
            }
        } else if let Some(wake_handle) = wake.filter(|w| kumo_rt::handle_koid(*w) == source) {
            let mut byte = [0u8; 1];
            let n = kumo_rt::channel_read(wake_handle, byte.as_mut_ptr(), byte.len());
            woken = n == 1 && byte[0] == 1;
            if woken && reply.is_some() {
                break;
            }
        } else {
            return Err(Failure::Failed);
        }
    }
    let _ = kumo_rt::port_unbind(port, channel);
    if let Some(wake_handle) = wake {
        let _ = kumo_rt::port_unbind(port, wake_handle);
        let _ = kumo_rt::handle_close(wake_handle);
    }
    let _ = kumo_rt::port_unbind(port, timer);
    let _ = kumo_rt::handle_close(timer);
    let _ = kumo_rt::handle_close(port);
    if armed && !woken {
        return Err(Failure::Failed);
    }
    let reply = reply.ok_or(())?;
    if reply.is_cancelled() {
        return Err(Failure::Cancelled);
    }
    if !lease.retire(reply) {
        return Err(Failure::Failed);
    }
    log(format_args!("gpu-client: J0c sequence{} point{} retired after consumed GOP replacement/unmap; next GPU reuse permitted\n",image.sequence,image.gpu_point));
    Ok(())
}

fn run(channel: Handle, lane: Lane) -> Result<(), Failure> {
    let mut client = Client::new(Channel { channel });
    let info = client.info()?;
    if info.gpu_id & 0xffff_0000 != 0xa867_0000 {
        return Err(Failure::Failed);
    }
    let topology = client.topology()?;
    log(format_args!(
        "gpu-client: G7c attested topology: shader mask0x{:016x}, cores={}, core-ID range={}\n",
        topology.shader_present,
        topology.shader_present.count_ones(),
        64 - topology.shader_present.leading_zeros(),
    ));
    let mmu = client.mmu()?;
    // Derive the width exactly as the pinned Mesa helper does, then require this service's own
    // window to fit inside the attested hardware range. An attestation that could not hold the
    // window is a contradiction, not a usable property, so it fails the session. — SAKER
    let va_bits = mmu.mmu_features & 0xff;
    let Some(hardware_va_bytes) = 1u64.checked_shl(va_bits) else {
        return Err(Failure::Failed);
    };
    if hardware_va_bytes < VA_END {
        return Err(Failure::Failed);
    }
    log(format_args!(
        "gpu-client: G7d attested MMU: features0x{:08x}, va bits={}, hardware range0..0x{:x} holds service window0x{:x}..0x{:x}\n",
        mmu.mmu_features, va_bits, hardware_va_bytes, VA_START, VA_END
    ));
    // Tiler properties are optional. An unknown shape must not fail an otherwise healthy
    // session, so NotSupported is reported and the compute path continues. The two fields are
    // decoded exactly as upstream's pan_query_tiler_features does: [4:0] is a log2 bin size
    // and [11:8] the level count. — SAKER
    match client.tiler() {
        Ok(tiler) => {
            let raw = tiler.tiler_features;
            log(format_args!(
                "gpu-client: G7e attested tiler: features0x{:08x}, bin={} bytes, max levels={}\n",
                raw,
                1u32 << (raw & 0x1f),
                (raw >> 8) & 0xf
            ));
        }
        Err(ClientError::Server(GpuStatus::NotSupported)) => log(format_args!(
            "gpu-client: G7e tiler properties unavailable; compute path unaffected\n"
        )),
        Err(_) => return Err(Failure::Failed),
    }
    // Same optional rule: upstream reads this word whole as the compressed-format mask and
    // reads bit 25 as AFRC presence on arch 10, so KUMO reduces it to neither. — SAKER
    match client.texture() {
        Ok(texture) => log(format_args!(
            "gpu-client: G7f attested texture: features0=0x{:08x}, AFRC bit25={}\n",
            texture.texture_features[0],
            (texture.texture_features[0] >> 25) & 1
        )),
        Err(ClientError::Server(GpuStatus::NotSupported)) => log(format_args!(
            "gpu-client: G7f texture features unavailable; compute path unaffected\n"
        )),
        Err(_) => return Err(Failure::Failed),
    }
    let features = CsFeatures::decode(info.cs_features);
    let compute = compute_stream(features).map_err(|_| ())?;
    let triangle = triangle_stream(features).map_err(|_| ())?;
    log(format_args!(
        "gpu-client: GPU_ID=0x{:08x}, queue registers={} scoreboards={}\n",
        info.gpu_id, features.work_regs, features.scoreboards
    ));
    let vm = client.create_vm()?;
    // A normal server refusal must consume its request ID without killing the channel.
    // The next real query proves the shared API can continue after that refusal. — KESTREL
    if client.create_vm() != Err(ClientError::Server(GpuStatus::BadState)) || client.info()? != info
    {
        return Err(Failure::Failed);
    }
    log(format_args!("gpu-client: G7a shared API: duplicate VM refused, next query live; unique request IDs, no replay\n"));
    let buffer = client.create_buffer(BUFFER_BYTES)?;
    let (vmo, bytes) = (buffer.handle, buffer.bytes);
    let (status, cpu) = kumo_rt::vmar_map_anywhere(
        vmo,
        bytes,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    );
    if status != 0 || cpu == 0 {
        return Err(Failure::Failed);
    }
    upload(cpu, BINARY_OFFSET, KUMO_PAINT_V10.binary)?;
    upload(cpu, DATA_OFFSET, &compute_data().map_err(|_| ())?)?;
    upload(cpu, TRIANGLE_BINARY_OFFSET, KUMO_TRIANGLE_V10.binary)?;
    upload(cpu, TRIANGLE_DATA_OFFSET, &triangle_data().map_err(|_| ())?)?;
    for offset in (0..PAINT_BYTES).step_by(4) {
        // SAFETY: each word lies inside the image extent of this retained mapping.
        unsafe {
            ((cpu + PAINT_OFFSET + offset) as *mut u32).write_volatile(PIXEL_POISON);
        }
    }
    barrier();
    for binding in bindings(vm, buffer.id) {
        client.bind(binding)?;
    }
    let group = client.create_group(vm)?;
    for (index, value) in VALUES.into_iter().enumerate() {
        let point = index as u64 + 1;
        // SAFETY: the scratch word is in its own mapped page, outside the image and code.
        unsafe {
            ((cpu + SCRATCH_OFFSET) as *mut u32).write_volatile(0xfeed_face);
        }
        submit_wait(
            &mut client,
            cpu,
            group,
            point,
            &proof_stream(VA_START + SCRATCH_OFFSET, value),
        )?;
        // SAFETY: read only after the matching completion reply and barrier.
        let got = unsafe { ((cpu + SCRATCH_OFFSET) as *const u32).read_volatile() };
        if got != value {
            log(format_args!(
                "gpu-client: point {} wrong value 0x{:08x}, expected 0x{:08x}\n",
                point, got, value
            ));
            return Err(Failure::Failed);
        }
        log(format_args!(
            "gpu-client: point {} complete, GPU wrote 0x{:08x} into shared VMO\n",
            point, got
        ));
    }
    log(format_args!(
        "gpu-client: two submissions complete; shared values verified\n"
    ));
    log(format_args!(
        "gpu-client: dispatching kumo_paint: binary=0x{:x} data=0x{:x} output=0x{:x}\n",
        VA_START + BINARY_OFFSET,
        VA_START + DATA_OFFSET,
        VA_START + PAINT_OFFSET
    ));
    submit_wait(&mut client, cpu, group, 3, &compute)?;
    let check = check_paint(|x, y| {
        // SAFETY: check_paint visits exactly the 256x128 words of this mapped image.
        unsafe {
            ((cpu + PAINT_OFFSET + u64::from(y * PAINT_WIDTH + x) * 4) as *const u32)
                .read_volatile()
        }
    });
    log(format_args!(
        "gpu-client: compute painted {}x{}: mismatches={} checksum=0x{:08x} (reference 0x{:08x})\n",
        PAINT_WIDTH,
        PAINT_HEIGHT,
        check.mismatches,
        check.checksum,
        paint_reference_checksum()
    ));
    if let Some((x, y, got, want)) = check.first {
        log(format_args!(
            "gpu-client: first wrong pixel ({}, {}): got 0x{:08x}, want 0x{:08x}\n",
            x, y, got, want
        ));
        return Err(Failure::Failed);
    }
    log(format_args!(
        "gpu-client: client compute frame verified; 32768 pixels match\n"
    ));
    match lane {
        Lane::Native(presentation) => native_frames(
            &mut client,
            cpu,
            vmo,
            group,
            presentation,
            &triangle,
            &compute,
        )?,
        Lane::Churn {
            channel,
            surface,
            presentation,
        } => {
            // j713: the boot session's J0c native diagnostic runs first; its third retirement
            // sets drv-fb's diagnostic-completed flag, so the vblank gate arms before the
            // churn readiness probe. Points: 3 = bring-up paint, 4..=6 = native frames.
            // — TIMBERDOODLE
            //j714 A regranted session has no producer: it runs no native frames at all (not
            // the no-presenter single-frame path) and starts its churn timeline at point 5.
            let mut first_point = CHURN_FIRST_POINT_NO_NATIVE;
            if presentation.0 != 0 {
                native_frames(
                    &mut client,
                    cpu,
                    vmo,
                    group,
                    presentation,
                    &triangle,
                    &compute,
                )?;
                first_point = 4 + u64::from(kumo_graphics::native::PRESENTATIONS);
            }
            churn_run(
                &mut client,
                cpu,
                vm,
                group,
                channel,
                surface,
                &compute,
                first_point,
            )?
        }
    }
    let _ = kumo_rt::handle_close(vmo);
    Ok(())
}

/// The three-presentation J0c diagnostic, unchanged: the native presenter leases the same
/// VMO's image pages and each GPU reuse waits on the display pair gate. — TIMBERDOODLE
fn native_frames(
    client: &mut Client<Channel>,
    cpu: u64,
    vmo: Handle,
    group: u32,
    presentation: Handle,
    triangle: &[u64; 24],
    compute: &[u64; 24],
) -> Result<(), Failure> {
    let mut lease = kumo_graphics::native::Lease::default();
    let count = if presentation.0 == 0 {
        1
    } else {
        kumo_graphics::native::PRESENTATIONS
    };
    for sequence in 1..=count {
        // This check precedes EVERY poison/write and GPU submission. Only a verified
        // previous display retirement reply can reopen the image for production. — KESTREL
        if !lease.can_render() {
            return Err(Failure::Failed);
        }
        let point = u64::from(sequence) + 3;
        let is_triangle = sequence % 2 == 1;
        for offset in (0..PAINT_BYTES).step_by(4) {
            unsafe {
                ((cpu + PAINT_OFFSET + offset) as *mut u32).write_volatile(PIXEL_POISON);
            }
        }
        submit_wait(
            client,
            cpu,
            group,
            point,
            if is_triangle { triangle } else { compute },
        )?;
        let read = |x, y| unsafe {
            ((cpu + PAINT_OFFSET + u64::from(y * PAINT_WIDTH + x) * 4) as *const u32)
                .read_volatile()
        };
        let check = if is_triangle {
            check_triangle(read)
        } else {
            check_paint(read)
        };
        let reference = if is_triangle {
            triangle_reference_checksum()
        } else {
            paint_reference_checksum()
        };
        log(format_args!("gpu-client: J0c sequence{} point{} {}: VMO{} mismatches={} checksum=0x{:08x} (reference 0x{:08x})\n",sequence,point,if is_triangle {"triangle"}else{"bars/ramp"},kumo_rt::handle_koid(vmo),check.mismatches,check.checksum,reference));
        if let Some((x, y, got, want)) = check.first {
            log(format_args!(
                "gpu-client: J0c mismatch ({},{}): got0x{:08x} want0x{:08x}\n",
                x, y, got, want
            ));
            return Err(Failure::Failed);
        }
        if presentation.0 != 0 {
            present_image(
                presentation,
                vmo,
                kumo_graphics::native::Image {
                    sequence,
                    surface: kumo_graphics::DEMO,
                    offset: PAINT_OFFSET,
                    vmo_bytes: BUFFER_BYTES,
                    gpu_point: point,
                    checksum: check.checksum,
                },
                &mut lease,
            )?;
        }
    }
    if presentation.0 != 0 {
        if !lease.complete() {
            return Err(Failure::Failed);
        }
        log(format_args!("gpu-client: J0c complete: 3 native frames, same VMO/image pages, 2 GPU reuses only after display retirement\n"));
    }
    Ok(())
}

#[no_mangle]
extern "C" fn main(channel: u64, bootstrap: u64) -> ! {
    let Some(channel) = handle(channel) else {
        kumo_rt::process_exit(1)
    };
    log(format_args!(
        "gpu-client: separate process starting GPU session\n"
    ));
    // x1 is the lane bootstrap pair's child end, transferred on both lanes. — TIMBERDOODLE
    let Some(bootstrap) = handle(bootstrap) else {
        log_at(
            kumo_abi::sys::log_level::ERR,
            format_args!("gpu-client: lane bootstrap missing\n"),
        );
        let _ = kumo_rt::handle_close(channel);
        kumo_rt::process_exit(1)
    };
    let lane = match select_lane(bootstrap) {
        Ok(lane) => lane,
        Err(()) => {
            let _ = kumo_rt::handle_close(bootstrap);
            let _ = kumo_rt::handle_close(channel);
            kumo_rt::process_exit(1)
        }
    };
    let _ = kumo_rt::handle_close(bootstrap);
    let result = run(channel, lane);
    match lane {
        Lane::Native(presentation) => {
            if presentation.0 != 0 {
                let _ = kumo_rt::handle_close(presentation);
            }
        }
        Lane::Churn {
            channel: view,
            surface,
            presentation,
        } => {
            let _ = kumo_rt::handle_close(view);
            let _ = kumo_rt::handle_close(surface);
            //j714 A no-producer churn lane carries handle zero; never close it.
            if presentation.0 != 0 {
                let _ = kumo_rt::handle_close(presentation);
            }
        }
    }
    let failed = match result {
        Ok(()) => false,
        Err(Failure::Cancelled) => {
            log(format_args!("gpu-client: presenter cancelled diagnostic; stopping without image reuse or fabricated completion\n"));
            false
        }
        Err(Failure::Failed) => {
            log(format_args!("gpu-client: session failed\n"));
            true
        }
    };
    let _ = kumo_rt::handle_close(channel);
    kumo_rt::process_exit(u64::from(failed))
}
