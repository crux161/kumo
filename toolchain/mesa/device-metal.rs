//j774
//j777
//j778
//j785
//j788
//! G7o-f: the native Mesa device, BOs and VM over the live drv-gpu session that Sora grants
//! the GPU client, on real hardware. The C cases (device-metal.c) drive the real bridge and
//! Mesa lifecycle; this side owns the session, logs through the kernel and runs the native
//! group/submit/sync operations the bridge does not model yet (G7p). — TIMBERDOODLE
#![no_std]
extern crate alloc;
use core::{
    alloc::{GlobalAlloc, Layout},
    ffi::{c_char, c_void},
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};
use kumo_abi::{
    sys::{log_level, Syscall},
    Handle,
};
use kumo_gpu::{
    bo::Buffers,
    c::{with_device_api, Api},
    client::Client,
    Submission, SyncState,
};
use kumo_graphics::lane;
mod native;

type Main = unsafe extern "C" fn(isize, *mut *mut c_char, *mut *mut c_char) -> i32;
extern "C" {
    fn posix_memalign(out: *mut *mut c_void, alignment: usize, size: usize) -> i32;
    fn free(p: *mut c_void);
    fn abort() -> !;
    fn __relibc_kumo_start(bootstrap: u64, main: Main) -> !;
    fn kumo_metal_cases(api: *const Api) -> u32;
}
struct LibcAllocator;
unsafe impl GlobalAlloc for LibcAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut p = core::ptr::null_mut();
        if posix_memalign(
            &mut p,
            layout.align().max(core::mem::size_of::<usize>()),
            layout.size(),
        ) != 0
        {
            return core::ptr::null_mut();
        }
        p.cast()
    }
    unsafe fn dealloc(&self, p: *mut u8, _: Layout) {
        free(p.cast());
    }
}
#[global_allocator]
static ALLOCATOR: LibcAllocator = LibcAllocator;

/// Written by device-metal-crt0.S before anything else runs.
#[no_mangle]
#[allow(non_upper_case_globals)]
static kumo_metal_session: AtomicU64 = AtomicU64::new(0);
#[no_mangle]
#[allow(non_upper_case_globals)]
static kumo_metal_lane: AtomicU64 = AtomicU64::new(0);
static GROUP: AtomicU32 = AtomicU32::new(0);
static NEXT_POINT: AtomicU64 = AtomicU64::new(1);

fn log(bytes: &[u8]) {
    let len = bytes.len().min(256);
    native::call(
        Syscall::LogWrite,
        [
            u64::from(log_level::NOTICE),
            bytes.as_ptr() as u64,
            len as u64,
            0,
            0,
            0,
        ],
    );
}
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let mut line = *b"G7o-f metal device FAILED Rust line=00000\n";
    let mut n = info.location().map_or(0, |p| p.line());
    let end = line.len() - 1;
    for digit in line[end - 5..end].iter_mut().rev() {
        *digit = b'0' + (n % 10) as u8;
        n /= 10;
    }
    log(&line);
    unsafe { abort() }
}

/// Runs before relibc: no TLS, allocator or stdio yet. A pair whose peer is already closed
/// reads as an empty startup channel, so relibc grants no streams and no argv. — TIMBERDOODLE
#[no_mangle]
pub extern "C" fn kumo_metal_start(main: Main) -> ! {
    let (closed, empty) = native::call(Syscall::ChannelCreate, [0; 6]);
    native::call(Syscall::HandleClose, [closed, 0, 0, 0, 0, 0]);
    unsafe { __relibc_kumo_start(empty, main) }
}

/// Monotonic nanoseconds, so the G7p-e timeout proof can time its wait. — TIMBERDOODLE
#[no_mangle]
pub extern "C" fn kumo_metal_now_ns() -> u64 {
    native::call(Syscall::ClockGet, [0; 6]).0
}

#[no_mangle]
pub extern "C" fn kumo_metal_log(line: *const c_char) {
    if line.is_null() {
        return;
    }
    let mut len = 0;
    // SAFETY: a NUL-terminated C string from device-metal.c, read up to the log's limit.
    while len < 255 && unsafe { *line.add(len) } != 0 {
        len += 1;
    }
    let mut bytes = [0u8; 256];
    // SAFETY: the `len` bytes just scanned.
    unsafe { core::ptr::copy_nonoverlapping(line.cast::<u8>(), bytes.as_mut_ptr(), len) };
    bytes[len] = b'\n';
    log(&bytes[..=len]);
}

type Store<'a> = Buffers<'a, native::Channel, native::Memory>;

/// One CS SYNC_SET32 of `value` at GPU VA `target`, from a fresh 64-byte line of a Mesa BO
/// whose binding is read-only (KUMO's Submit requires it). It writes a 32-bit sync object:
/// `value`, then status 0 in the next word. Returns 0 on completion with GPU status 0.
/// — TIMBERDOODLE
#[no_mangle]
pub extern "C" fn kumo_metal_store(
    context: *mut c_void,
    stream_cpu: *mut u64,
    stream_va: u64,
    target: u64,
    value: u32,
) -> u32 {
    let words = [
        (1u64 << 56) | (target & 0x0000_ffff_ffff_ffff), // MOVE48 r0, target
        (2u64 << 56) | (2u64 << 48) | u64::from(value),  // MOVE32 r2, value
        (38u64 << 56) | (2u64 << 32),                    // SYNC_SET32 slot0, [r0], r2
        (3u64 << 56) | (1u64 << 16),                     // WAIT slot0
    ];
    kumo_metal_run(
        context,
        stream_cpu,
        stream_va,
        words.as_ptr(),
        words.len() as u32,
    )
}

//j754
/// Run `count` CS words (at most one 64-byte line) as one job on the session's own group,
/// created on the service VM at first use; points run 1, 2, ... Returns 0 on completion with
/// GPU status 0. G7q-a's heap walk shares it with the store above. — TIMBERDOODLE
#[no_mangle]
pub extern "C" fn kumo_metal_run(
    context: *mut c_void,
    stream_cpu: *mut u64,
    stream_va: u64,
    words: *const u64,
    count: u32,
) -> u32 {
    if context.is_null() || stream_cpu.is_null() || words.is_null() || stream_va % 64 != 0 {
        return 1;
    }
    if count == 0 || count > 8 {
        return 1;
    }
    // SAFETY: `context` is the Buffers store with_device_api lent the C cases; calls are
    // serial and none of its other callbacks is live during this one.
    let client: &mut Client<native::Channel> =
        unsafe { &mut *context.cast::<Store<'_>>() }.native_client();
    if GROUP.load(Ordering::Relaxed) == 0 {
        match client.create_group(1) {
            Ok(group) => GROUP.store(group, Ordering::Relaxed),
            Err(_) => return 2,
        }
    }
    let group = GROUP.load(Ordering::Relaxed);
    for i in 0..count as usize {
        // SAFETY: `count` caller words, copied into the caller's mapped, line-aligned stream
        // slot.
        unsafe { stream_cpu.add(i).write_volatile(*words.add(i)) };
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
    let point = NEXT_POINT.fetch_add(1, Ordering::Relaxed);
    let submission = Submission {
        group,
        stream_va,
        stream_bytes: count * 8,
        signal_point: point,
    };
    if client.submit(submission).is_err() {
        return 3;
    }
    // drv-gpu waits for the job before reading the next request, so the first SyncRead
    // normally answers Complete; Pending is bounded rather than spun forever.
    for _ in 0..1000 {
        match client.sync_read(group, point) {
            Ok(SyncState::Complete { gpu_status: 0 }) => {
                unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
                return 0;
            }
            Ok(SyncState::Complete { .. }) => return 4,
            Ok(SyncState::Pending) => continue,
            Err(_) => return 5,
        }
    }
    6
}

/// VK1-c: the session's one group of three queues, created at first use.
static QUEUE_GROUP: AtomicU32 = AtomicU32::new(0);

/// VK1-c (PLAN/015 §8.2): one SubmitStreams carrying `count` (at most three) subqueue streams,
/// stream `i` on queue `i`, under one point of the three-queue group. Returns 0 when the point
/// completes with GPU status 0. The streams order themselves in memory, as panvk's do.
#[no_mangle]
pub extern "C" fn kumo_metal_queues(
    context: *mut c_void,
    streams_va: *const u64,
    streams_bytes: *const u32,
    count: u32,
) -> u32 {
    if context.is_null() || streams_va.is_null() || streams_bytes.is_null() {
        return 1;
    }
    if count == 0 || count > u32::from(kumo_gpu::MAX_GROUP_QUEUES) {
        return 1;
    }
    // SAFETY: as in kumo_metal_run.
    let client: &mut Client<native::Channel> =
        unsafe { &mut *context.cast::<Store<'_>>() }.native_client();
    if QUEUE_GROUP.load(Ordering::Relaxed) == 0 {
        match client.create_group_queues(1, kumo_gpu::MAX_GROUP_QUEUES) {
            Ok(group) => QUEUE_GROUP.store(group, Ordering::Relaxed),
            Err(_) => return 2,
        }
    }
    let group = QUEUE_GROUP.load(Ordering::Relaxed);
    let mut submission = kumo_gpu::MultiSubmission {
        group,
        signal_point: NEXT_POINT.fetch_add(1, Ordering::Relaxed),
        count: count as u8,
        ..Default::default()
    };
    for queue in 0..count as usize {
        // SAFETY: the caller's `count`-entry arrays.
        submission.streams[queue] = kumo_gpu::StreamRef {
            queue: queue as u8,
            stream_va: unsafe { *streams_va.add(queue) },
            stream_bytes: unsafe { *streams_bytes.add(queue) },
        };
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
    let Ok(fence) = client.submit_streams_fence(submission) else {
        return 3;
    };
    for _ in 0..1000 {
        match client.sync_read(group, fence.point) {
            Ok(SyncState::Complete { gpu_status: 0 }) => {
                unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
                return 0;
            }
            Ok(SyncState::Complete { .. }) => return 4,
            Ok(SyncState::Pending) => continue,
            Err(_) => return 5,
        }
    }
    6
}

/// G7q-a: one tiler heap on the service VM. On success `out` holds the heap ID, the context VA
/// and the first chunk's VA, and the result is 0. — TIMBERDOODLE
#[no_mangle]
pub extern "C" fn kumo_metal_heap_create(
    context: *mut c_void,
    chunk_bytes: u32,
    initial_chunks: u32,
    max_chunks: u32,
    out: *mut u64,
) -> u32 {
    if context.is_null() || out.is_null() {
        return 1;
    }
    // SAFETY: as in kumo_metal_run.
    let client: &mut Client<native::Channel> =
        unsafe { &mut *context.cast::<Store<'_>>() }.native_client();
    let spec = kumo_gpu::HeapSpec {
        vm: 1,
        chunk_bytes,
        initial_chunks,
        max_chunks,
        target_in_flight: 65535,
    };
    match client.create_heap(spec) {
        Ok((heap, placement)) => {
            // SAFETY: the caller's three-word output array.
            unsafe {
                out.write(u64::from(heap));
                out.add(1).write(placement.context_va);
                out.add(2).write(placement.first_chunk_va);
            }
            0
        }
        Err(_) => 2,
    }
}

/// Destroy heap `heap`; `bytes` receives what returned to the budget.
#[no_mangle]
pub extern "C" fn kumo_metal_heap_destroy(context: *mut c_void, heap: u32, bytes: *mut u64) -> u32 {
    if context.is_null() || bytes.is_null() {
        return 1;
    }
    // SAFETY: as in kumo_metal_run.
    let client: &mut Client<native::Channel> =
        unsafe { &mut *context.cast::<Store<'_>>() }.native_client();
    match client.destroy_heap(heap) {
        Ok(released) => {
            // SAFETY: the caller's output word.
            unsafe { bytes.write(released) };
            0
        }
        Err(_) => 2,
    }
}

fn close(raw: u64) {
    native::call(Syscall::HandleClose, [raw, 0, 0, 0, 0, 0]);
}

/// One NOTICE line from parts, truncated at the log's 256-byte limit.
fn log_parts(parts: &[&[u8]]) {
    let mut line = [0u8; 256];
    let mut len = 0;
    for part in parts {
        let take = part.len().min(line.len() - len);
        line[len..len + take].copy_from_slice(&part[..take]);
        len += take;
    }
    log(&line[..len]);
}

fn valid(raw: u64) -> Option<u64> {
    lane::valid_handle(raw).then_some(raw)
}

/// GL3-b (PLAN/015 §8.32): view `GPU_VIEW`'s presentation channel and surface, taken from the
/// lane before the GL proof runs. Zero until `kumo_metal_view_take` succeeds; the present
/// takes and closes them.
static VIEW_CHANNEL: AtomicU64 = AtomicU64::new(0);
static VIEW_SURFACE: AtomicU64 = AtomicU64::new(0);
/// Which client drives the view: 0 is a GL client (GL3-b, GL4: bottom-up `glReadPixels`
/// readbacks, flipped upright), 1 is a Vulkan client (VK4-b, VK5: top-down images kept in
/// Vulkan's orientation, PLAN/015 §8.43 D2 and §8.46 D3). It names the view and churn log
/// lines and picks the row order; the GL clients' lines are unchanged.
static VIEW_PROOF: AtomicU32 = AtomicU32::new(0);

fn vulkan_client() -> bool {
    VIEW_PROOF.load(Ordering::Relaxed) == 1
}

/// The churn proof's name in its lines: GL4, or VK5 for the Vulkan client.
fn churn_name() -> &'static [u8] {
    if vulkan_client() {
        b"VK5"
    } else {
        b"GL4"
    }
}

fn churn_str() -> &'static str {
    if vulkan_client() {
        "VK5"
    } else {
        "GL4"
    }
}

/// The API that rendered the frames J0c leases.
fn churn_api() -> &'static str {
    if vulkan_client() {
        "Vulkan"
    } else {
        "GL"
    }
}

/// GL readbacks arrive bottom-up and are flipped upright; Vulkan images are copied top-down.
fn client_rows() -> lane::RowOrder {
    if vulkan_client() {
        lane::RowOrder::TopDown
    } else {
        lane::RowOrder::BottomUp
    }
}

fn view_name() -> &'static [u8] {
    if vulkan_client() {
        b"VK4-b"
    } else {
        b"GL3-b"
    }
}

/// Select the client that drives the view before `kumo_metal_view_take` or
/// `kumo_metal_churn_take` (VK4-b and VK5 pass 1).
/// Returns nonzero for an unknown proof, which leaves GL3-b's selection in place.
#[no_mangle]
pub extern "C" fn kumo_metal_view_proof(proof: u32) -> u32 {
    if proof > 1 {
        return 1;
    }
    VIEW_PROOF.store(proof, Ordering::Relaxed);
    0
}
/// The hold: fifteen presents paced one second apart keep the triangle on HDMI for about
/// fifteen seconds, at a rate that cannot crowd drv-fb's serve loop.
const VIEW_PRESENTS: u32 = 15;
const VIEW_PACE_NS: u64 = 1_000_000_000;
/// A serving drv-fb answers at once; the bound only stops a wedged presenter hanging the
/// client.
const VIEW_REPLY_BOUND_NS: u64 = 10_000_000_000;

/// Decode Sora's lane (D1's `view` marker sends `V`, `S`, then `N`) before Mesa starts. The
/// native producer is closed unused: this image runs no J0c diagnostic, so drv-fb's present
/// gate stays unarmed and replies are honestly `Copied` (CRUX, 2026-11-03). A native-only
/// lane means the image lacks its `etc/gpu-lane` marker and fails here, before any GPU work.
#[no_mangle]
pub extern "C" fn kumo_metal_view_take() -> u32 {
    let raw = kumo_metal_lane.swap(0, Ordering::Relaxed);
    if raw == 0 {
        log_parts(&[view_name(), b" lane: no lane bootstrap was transferred\n"]);
        return 1;
    }
    let read = || {
        let mut bytes = [0u8; 2];
        let (len, handle) = native::call(
            Syscall::ChannelRead,
            [raw, bytes.as_mut_ptr() as u64, bytes.len() as u64, 0, 0, 0],
        );
        lane::LaneRead {
            len,
            tag: bytes[0],
            handle,
        }
    };
    let decoded = lane::decode_lane(read, close);
    close(raw);
    match decoded {
        Ok(lane::Lane::View {
            channel,
            surface,
            producer,
        }) => {
            if producer != 0 {
                close(producer);
            }
            VIEW_CHANNEL.store(channel, Ordering::Relaxed);
            VIEW_SURFACE.store(surface, Ordering::Relaxed);
            log_parts(&[
                view_name(),
                b" lane: view 1 channel and surface received over V, S, N; the native producer is not used\n",
            ]);
            0
        }
        Ok(lane::Lane::Native { producer }) => {
            if producer != 0 {
                close(producer);
            }
            log_parts(&[
                view_name(),
                b" lane: Sora named the native lane; this image has no etc/gpu-lane view marker\n",
            ]);
            2
        }
        Err(cause) => {
            log_parts(&[view_name(), b" lane: ", cause.as_bytes(), b"\n"]);
            3
        }
    }
}

/// One sleep on a private port and one-shot timer, as kumo-rt's `sleep_ns` does.
fn sleep(ns: u64) {
    let Some(port) = valid(native::call(Syscall::PortCreate, [0; 6]).0) else {
        return;
    };
    if let Some(timer) = valid(native::call(Syscall::TimerCreate, [ns, 0, 0, 0, 0, 0]).0) {
        if native::call(Syscall::PortBind, [port, timer, 0, 0, 0, 0]).0 == 0 {
            native::call(Syscall::PortWait, [port, 0, 0, 0, 0, 0]);
        }
        close(timer);
    }
    close(port);
}

/// View `GPU_VIEW`'s presentation channel, as the hold drives it. A reply wait binds only
/// the channel and a one-shot timer, so no other source can be mistaken for the reply
/// (the Rust gpu-client's reply-port shape).
struct ViewLink {
    channel: u64,
}
impl lane::PresentLink for ViewLink {
    fn send(&mut self, request: &[u8; kumo_graphics::REQUEST_LEN]) -> Result<(), &'static str> {
        let status = native::call(
            Syscall::ChannelWrite,
            [
                self.channel,
                request.as_ptr() as u64,
                request.len() as u64,
                0,
                0,
                0,
            ],
        )
        .0;
        if status == 0 {
            Ok(())
        } else {
            Err("present request write failed")
        }
    }
    fn receive(
        &mut self,
        reply: &mut [u8; kumo_graphics::REPLY_LEN + 1],
    ) -> Result<usize, &'static str> {
        let port =
            valid(native::call(Syscall::PortCreate, [0; 6]).0).ok_or("reply port unavailable")?;
        let Some(timer) =
            valid(native::call(Syscall::TimerCreate, [VIEW_REPLY_BOUND_NS, 0, 0, 0, 0, 0]).0)
        else {
            close(port);
            return Err("reply timer unavailable");
        };
        let koid = |raw| native::call(Syscall::HandleKoid, [raw, 0, 0, 0, 0, 0]).0;
        let (channel_koid, timer_koid) = (koid(self.channel), koid(timer));
        let result = if channel_koid == u64::MAX
            || timer_koid == u64::MAX
            || native::call(Syscall::PortBind, [port, self.channel, 0, 0, 0, 0]).0 != 0
            || native::call(Syscall::PortBind, [port, timer, 0, 0, 0, 0]).0 != 0
        {
            Err("reply port bind failed")
        } else {
            loop {
                let source = native::call(Syscall::PortWait, [port, 0, 0, 0, 0, 0]).0;
                if source == channel_koid {
                    let (len, extra) = native::call(
                        Syscall::ChannelRead,
                        [
                            self.channel,
                            reply.as_mut_ptr() as u64,
                            reply.len() as u64,
                            0,
                            0,
                            0,
                        ],
                    );
                    if extra != 0 {
                        if valid(extra).is_some() {
                            close(extra);
                        }
                        break Err("present reply carried a handle");
                    }
                    if len == 0 {
                        break Err("present channel closed");
                    }
                    if (len as i64) < 0 {
                        break Err("present channel read failed");
                    }
                    break Ok(len as usize);
                }
                if source == timer_koid {
                    break Err("present reply timed out");
                }
            }
        };
        native::call(Syscall::PortUnbind, [port, self.channel, 0, 0, 0, 0]);
        native::call(Syscall::PortUnbind, [port, timer, 0, 0, 0, 0]);
        close(timer);
        close(port);
        result
    }
    fn pause(&mut self) {
        sleep(VIEW_PACE_NS);
    }
}

/// Copy GL3-a's verified readback into view `GPU_VIEW`'s surface as upright XRGB8888 (D2:
/// public glReadPixels, a CPU swizzle and VmoWrite), then hold it on HDMI with fresh FULL
/// presents. With `kumo_metal_view_proof(1)` the pixels are VK4-a's top-down Vulkan image,
/// copied as they are (PLAN/015 §8.43 D2). The view's channel and surface are closed either way, so drv-fb reports the view
/// released and Sora takes its lease back when the client exits.
#[no_mangle]
pub extern "C" fn kumo_metal_view_present(pixels: *const u32, width: u32, height: u32) -> u32 {
    use alloc::format;
    let channel = VIEW_CHANNEL.swap(0, Ordering::Relaxed);
    let surface = VIEW_SURFACE.swap(0, Ordering::Relaxed);
    let result = (|| {
        if channel == 0 || surface == 0 {
            log_parts(&[view_name(), b" copy: no view was taken from the lane\n"]);
            return 1;
        }
        let view = lane::VIEW_SURFACE;
        if pixels.is_null() || width != view.width || height != view.height {
            log_parts(&[
                view_name(),
                b" copy: the readback is not the 256x128 view's size\n",
            ]);
            return 2;
        }
        // SAFETY: the C caller's readback array of width * height packed RGBA8 words, alive
        // and unaliased for this call.
        let pixels =
            unsafe { core::slice::from_raw_parts(pixels, width as usize * height as usize) };
        let name = core::str::from_utf8(view_name()).unwrap_or("view");
        let (order, shape) = if vulkan_client() {
            (
                lane::RowOrder::TopDown,
                "top-down XRGB8888 in Vulkan's orientation",
            )
        } else {
            (lane::RowOrder::BottomUp, "upright XRGB8888")
        };
        let written = lane::copy_rows(pixels, view, order, |offset, bytes| {
            native::call(
                Syscall::VmoWrite,
                [
                    surface,
                    offset,
                    bytes.as_ptr() as u64,
                    bytes.len() as u64,
                    0,
                    0,
                ],
            )
            .0 == 0
        });
        match written {
            Ok(chunks) => log(format!(
                "{} copy: {} RGBA8 pixels written to view 1's surface as {} in {} VmoWrite chunks\n",
                name,
                pixels.len(),
                shape,
                chunks
            )
            .as_bytes()),
            Err(error) => {
                log(format!("{} copy: surface write refused: {:?}\n", name, error).as_bytes());
                return 3;
            }
        }
        let mut link = ViewLink { channel };
        match lane::present_hold(&mut link, view, 1, VIEW_PRESENTS) {
            Ok(tally) => {
                log(format!(
                    "{} present: {} of {} presents accepted with FULL damage; retired={} copied={}\n",
                    name, tally.presents, VIEW_PRESENTS, tally.retired, tally.copied
                )
                .as_bytes());
                0
            }
            Err(error) => {
                log(format!("{} present: hold ended: {:?}\n", name, error).as_bytes());
                4
            }
        }
    })();
    if channel != 0 {
        close(channel);
    }
    if surface != 0 {
        close(surface);
    }
    log_parts(&[
        view_name(),
        b" release: view 1 channel and surface closed\n",
    ]);
    result
}

/// The client is single-threaded; this holds GL4's J0c lease between C calls.
struct Single<T>(core::cell::UnsafeCell<T>);
// SAFETY: one thread calls these entry points, serially.
unsafe impl<T> Sync for Single<T> {}
static LEASE: Single<Option<kumo_graphics::native::Lease>> =
    Single(core::cell::UnsafeCell::new(None));
/// GL4: the boot session's native producer (0 on a regranted session), the one VMO every
/// J0c image is leased from, the view's present sequence and the frame tally.
static PRODUCER: AtomicU64 = AtomicU64::new(0);
static J0C_VMO: AtomicU64 = AtomicU64::new(0);
static VIEW_SEQUENCE: AtomicU32 = AtomicU32::new(0);
static RETIRED: AtomicU32 = AtomicU32::new(0);
static COPIED: AtomicU32 = AtomicU32::new(0);
/// G8's readiness bound (`CHURN_READY_*` in the Rust gpu-client): a quiet window while
/// drv-fb measures the vblank cadence, then one probe per second for up to 180 s.
const READY_QUIET_NS: u64 = 20_000_000_000;
const READY_PROBES: u32 = 180;
/// drv-fb holds each J0c image 5 s; this bounds one presentation.
const J0C_BOUND_NS: u64 = 120_000_000_000;

/// GL4 (PLAN/015 §8.32): decode the churn lane (`etc/gpu-lane` = `churn`). The boot session
/// carries drv-fb's native producer for J0c; a regranted one carries handle 0 and skips it.
#[no_mangle]
pub extern "C" fn kumo_metal_churn_take() -> u32 {
    let raw = kumo_metal_lane.swap(0, Ordering::Relaxed);
    if raw == 0 {
        log_parts(&[churn_name(), b" lane: no lane bootstrap was transferred\n"]);
        return 1;
    }
    let read = || {
        let mut bytes = [0u8; 2];
        let (len, handle) = native::call(
            Syscall::ChannelRead,
            [raw, bytes.as_mut_ptr() as u64, bytes.len() as u64, 0, 0, 0],
        );
        lane::LaneRead {
            len,
            tag: bytes[0],
            handle,
        }
    };
    let decoded = lane::decode_lane(read, close);
    close(raw);
    match decoded {
        Ok(lane::Lane::View {
            channel,
            surface,
            producer,
        }) => {
            VIEW_CHANNEL.store(channel, Ordering::Relaxed);
            VIEW_SURFACE.store(surface, Ordering::Relaxed);
            PRODUCER.store(producer, Ordering::Relaxed);
            if producer != 0 {
                log_parts(&[
                    churn_name(),
                    b" lane: view 1 over V, S, N with the boot session's native producer\n",
                ]);
            } else {
                log_parts(&[
                    churn_name(),
                    b" lane: view 1 over V, S, N; a regranted session with no native producer\n",
                ]);
            }
            0
        }
        Ok(lane::Lane::Native { producer }) => {
            if producer != 0 {
                close(producer);
            }
            log_parts(&[
                churn_name(),
                b" lane: Sora named the native lane; this image has no etc/gpu-lane churn marker\n",
            ]);
            2
        }
        Err(cause) => {
            log_parts(&[churn_name(), b" lane: ", cause.as_bytes(), b"\n"]);
            3
        }
    }
}

/// 1 when this session must run J0c (it carries the native producer), else 0.
#[no_mangle]
pub extern "C" fn kumo_metal_churn_native() -> u32 {
    u32::from(PRODUCER.load(Ordering::Relaxed) != 0)
}

/// GL-fed J0c (CRUX, 2026-11-03): one GL frame, read back and copied upright into the one
/// J0c VMO, presented through drv-fb's unchanged native lease protocol. A Vulkan client's
/// frame (VK5, PLAN/015 §8.46 D2a) is copied top-down instead. The VMO is rewritten
/// only after the previous sequence's retirement, as the lease requires. drv-fb checks the
/// lease, the checksum and the display mapping, not who wrote the pixels; the pixels here
/// are GL-rendered and CPU-copied, not written by hand-built GPU jobs.
#[no_mangle]
pub extern "C" fn kumo_metal_j0c(
    pixels: *const u32,
    width: u32,
    height: u32,
    sequence: u32,
) -> u32 {
    use alloc::{format, vec::Vec};
    use kumo_graphics::native::{Image, RetireWait, WaitReply};
    let producer = PRODUCER.load(Ordering::Relaxed);
    let view = lane::VIEW_SURFACE;
    if producer == 0 || pixels.is_null() || width != view.width || height < view.height {
        return 1;
    }
    let image_bytes = Image {
        sequence: 1,
        surface: view,
        offset: 0,
        vmo_bytes: 4096,
        gpu_point: 1,
        checksum: 0,
    }
    .bytes();
    if J0C_VMO.load(Ordering::Relaxed) == 0 {
        // RE4 (metal): drv-fb maps the leased image READ|UNCACHED before checking it, and the
        // kernel refuses Normal-NC on anonymous VMOs, so a VmoCreate image is "no image
        // reachable". A contiguous DMA VMO is NC- and display-mappable, like the Rust
        // client's drv-gpu buffer; the pixels are still GL-rendered and CPU-copied.
        let Some(vmo) =
            valid(native::call(Syscall::VmoCreateContiguous, [image_bytes, 0, 0, 0, 0, 0]).0)
        else {
            return 2;
        };
        J0C_VMO.store(vmo, Ordering::Relaxed);
    }
    let vmo = J0C_VMO.load(Ordering::Relaxed);
    // SAFETY: the C caller's readback of width x height packed RGBA8 words, rows bottom-up for
    // GL (the view takes its bottom 256 x 128) or top-down for Vulkan (its top 256 x 128).
    let rows = unsafe { core::slice::from_raw_parts(pixels, (view.width * view.height) as usize) };
    let mut words: Vec<u32> = Vec::new();
    if words.try_reserve_exact(rows.len()).is_err() {
        return 3;
    }
    let copied = lane::copy_rows(rows, view, client_rows(), |offset, bytes| {
        words.extend(
            bytes
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        );
        native::call(
            Syscall::VmoWrite,
            [vmo, offset, bytes.as_ptr() as u64, bytes.len() as u64, 0, 0],
        )
        .0 == 0
    });
    if copied.is_err() {
        return 4;
    }
    let mut image = Image {
        sequence,
        surface: view,
        offset: 0,
        vmo_bytes: image_bytes,
        gpu_point: u64::from(sequence),
        checksum: 0,
    };
    image.checksum = image.checksum(|n| words[n as usize]);
    let koid = native::call(Syscall::HandleKoid, [vmo, 0, 0, 0, 0, 0]).0;
    // SAFETY: single-threaded, and no other borrow of the lease is live.
    let lease = unsafe { &mut *LEASE.0.get() }.get_or_insert_with(Default::default);
    if !lease.begin(image, koid) {
        log_parts(&[churn_name(), b" J0c: the lease refused the image\n"]);
        return 5;
    }
    let rights = (kumo_abi::Rights::READ | kumo_abi::Rights::TRANSFER).0 as u64;
    let Some(grant) = valid(native::call(Syscall::HandleDuplicate, [vmo, rights, 0, 0, 0, 0]).0)
    else {
        return 6;
    };
    // Register the frame-retired wait before publishing the image (G8c-b2).
    let wait = RetireWait {
        point: u64::from(sequence),
    }
    .encode();
    let request = image.encode();
    let write = |bytes: &[u8], handle: u64| {
        native::call(
            Syscall::ChannelWrite,
            [
                producer,
                bytes.as_ptr() as u64,
                bytes.len() as u64,
                handle,
                0,
                0,
            ],
        )
        .0 == 0
    };
    if !write(&wait, 0) || !write(&request, grant) {
        close(grant);
        return 7;
    }
    log(format!(
        "{} J0c sequence{}: {} frame leased READ-only on the native lane, checksum 0x{:08x}\n",
        churn_str(),
        sequence,
        churn_api(),
        image.checksum
    )
    .as_bytes());
    let Some(port) = valid(native::call(Syscall::PortCreate, [0; 6]).0) else {
        return 8;
    };
    let Some(timer) = valid(native::call(Syscall::TimerCreate, [J0C_BOUND_NS, 0, 0, 0, 0, 0]).0)
    else {
        close(port);
        return 8;
    };
    let koid_of = |raw| native::call(Syscall::HandleKoid, [raw, 0, 0, 0, 0, 0]).0;
    let (producer_koid, timer_koid) = (koid_of(producer), koid_of(timer));
    let mut wake = 0u64;
    let mut exchange = lane::NativeExchange::default();
    let mut step = if native::call(Syscall::PortBind, [port, producer, 0, 0, 0, 0]).0 != 0
        || native::call(Syscall::PortBind, [port, timer, 0, 0, 0, 0]).0 != 0
    {
        lane::NativeStep::Failed
    } else {
        lane::NativeStep::Continue
    };
    while step == lane::NativeStep::Continue {
        let source = native::call(Syscall::PortWait, [port, 0, 0, 0, 0, 0]).0;
        let event = if source == producer_koid {
            let mut bytes = [0u8; kumo_graphics::native::REQUEST_LEN + 1];
            let (n, extra) = native::call(
                Syscall::ChannelRead,
                [
                    producer,
                    bytes.as_mut_ptr() as u64,
                    bytes.len() as u64,
                    0,
                    0,
                    0,
                ],
            );
            let extra = valid(extra);
            let body = bytes.get(..n as usize).unwrap_or(&[]);
            if let Some(reply) = WaitReply::decode(body) {
                if let Some(handle) = extra {
                    if wake != 0
                        || native::call(Syscall::PortBind, [port, handle, 0, 0, 0, 0]).0 != 0
                    {
                        close(handle);
                        lane::NativeEvent::Unexpected
                    } else {
                        wake = handle;
                        lane::NativeEvent::WaitReply {
                            armed: reply.armed,
                            wake: true,
                        }
                    }
                } else {
                    lane::NativeEvent::WaitReply {
                        armed: reply.armed,
                        wake: false,
                    }
                }
            } else if let Some(reply) = kumo_graphics::native::Reply::decode(body) {
                if let Some(handle) = extra {
                    close(handle);
                }
                lane::NativeEvent::ImageReply {
                    reply,
                    handle: extra.is_some(),
                }
            } else {
                if let Some(handle) = extra {
                    close(handle);
                }
                lane::NativeEvent::Unexpected
            }
        } else if wake != 0 && source == koid_of(wake) {
            let mut byte = [0u8; 1];
            let n = native::call(
                Syscall::ChannelRead,
                [wake, byte.as_mut_ptr() as u64, 1, 0, 0, 0],
            )
            .0;
            lane::NativeEvent::WakeByte(if n == 1 { byte[0] } else { 0 })
        } else {
            if source == timer_koid {
                log_parts(&[churn_name(), b" J0c: no retirement within the bound\n"]);
            }
            lane::NativeEvent::Unexpected
        };
        step = exchange.on(event);
    }
    native::call(Syscall::PortUnbind, [port, producer, 0, 0, 0, 0]);
    native::call(Syscall::PortUnbind, [port, timer, 0, 0, 0, 0]);
    if wake != 0 {
        native::call(Syscall::PortUnbind, [port, wake, 0, 0, 0, 0]);
        close(wake);
    }
    close(timer);
    close(port);
    if step != lane::NativeStep::Done {
        return 9;
    }
    match exchange.finish() {
        Ok(reply) if lease.retire(reply) => {}
        Ok(_) => return 10,
        Err(true) => {
            log_parts(&[
                churn_name(),
                b" J0c: drv-fb cancelled the diagnostic; no reuse, no fabricated completion\n",
            ]);
            return 11;
        }
        Err(false) => return 12,
    }
    log(format!(
        "{} J0c sequence{} retired after drv-fb's consumed load; the VMO may be rewritten\n",
        churn_str(),
        sequence
    )
    .as_bytes());
    if lease.complete() {
        log(format!(
            "{} J0c complete: 3 {}-rendered frames through one VMO, each rewritten only after its retirement\n",
            churn_str(),
            churn_api()
        )
        .as_bytes());
    }
    0
}

/// G8's readiness on view 1: wait out drv-fb's vblank measurement, then probe once per
/// second until the first `Retired` (bounded).
#[no_mangle]
pub extern "C" fn kumo_metal_churn_ready() -> u32 {
    use alloc::format;
    let channel = VIEW_CHANNEL.load(Ordering::Relaxed);
    if channel == 0 {
        return 1;
    }
    sleep(READY_QUIET_NS);
    let mut sequence = VIEW_SEQUENCE.load(Ordering::Relaxed);
    let result = lane::present_until_retired(
        &mut ViewLink { channel },
        lane::VIEW_SURFACE,
        &mut sequence,
        READY_PROBES,
    );
    VIEW_SEQUENCE.store(sequence, Ordering::Relaxed);
    match result {
        Ok(probes) => {
            log(format!(
                "{} readiness: the first Retired after {} probes\n",
                churn_str(),
                probes
            )
            .as_bytes());
            0
        }
        Err(error) => {
            log(format!("{} readiness failed: {:?}\n", churn_str(), error).as_bytes());
            2
        }
    }
}

/// One churn frame: the readback's bottom 256 x 128 copied upright into view 1, then one
/// fresh FULL present. Retired and Copied are tallied separately; anything else fails.
#[no_mangle]
pub extern "C" fn kumo_metal_churn_frame(pixels: *const u32, width: u32, height: u32) -> u32 {
    use alloc::format;
    let (channel, surface) = (
        VIEW_CHANNEL.load(Ordering::Relaxed),
        VIEW_SURFACE.load(Ordering::Relaxed),
    );
    let view = lane::VIEW_SURFACE;
    if channel == 0
        || surface == 0
        || pixels.is_null()
        || width != view.width
        || height < view.height
    {
        return 1;
    }
    // SAFETY: as in kumo_metal_j0c.
    let rows = unsafe { core::slice::from_raw_parts(pixels, (view.width * view.height) as usize) };
    let copied = lane::copy_rows(rows, view, client_rows(), |offset, bytes| {
        native::call(
            Syscall::VmoWrite,
            [
                surface,
                offset,
                bytes.as_ptr() as u64,
                bytes.len() as u64,
                0,
                0,
            ],
        )
        .0 == 0
    });
    if copied.is_err() {
        return 2;
    }
    let sequence = VIEW_SEQUENCE
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    match lane::present_frame(&mut ViewLink { channel }, view, sequence, sequence) {
        Ok(kumo_graphics::Status::Retired) => {
            RETIRED.fetch_add(1, Ordering::Relaxed);
            0
        }
        Ok(_) => {
            COPIED.fetch_add(1, Ordering::Relaxed);
            0
        }
        Err(error) => {
            log(format!("{} frame present failed: {:?}\n", churn_str(), error).as_bytes());
            3
        }
    }
}

/// The session's presentation record, then the view and producer handles are closed.
#[no_mangle]
pub extern "C" fn kumo_metal_churn_summary(frames: u32) {
    use alloc::format;
    log(format!(
        "{} presents: {} frames on view 1, retired={} copied={}\n",
        churn_str(),
        frames,
        RETIRED.load(Ordering::Relaxed),
        COPIED.load(Ordering::Relaxed)
    )
    .as_bytes());
    for slot in [&VIEW_CHANNEL, &VIEW_SURFACE, &PRODUCER, &J0C_VMO] {
        let raw = slot.swap(0, Ordering::Relaxed);
        if raw != 0 {
            close(raw);
        }
    }
}

#[no_mangle]
pub extern "C" fn kumo_metal_proof() -> u32 {
    let session = kumo_metal_session.load(Ordering::Relaxed);
    let lane = kumo_metal_lane.load(Ordering::Relaxed);
    if session == 0 || session > u32::MAX as u64 {
        return 1000;
    }
    // The lane pair names a presentation lane this proof does not run; closing it drops
    // Sora's `N` and its native producer handle.
    if lane != 0 {
        native::call(Syscall::HandleClose, [lane, 0, 0, 0, 0, 0]);
    }
    let mut client = Client::new(native::Channel::new(Handle(session as u32)));
    let mut memory = native::Memory;
    let mut buffers = Buffers::new(&mut client, &mut memory);
    let failed = with_device_api(&mut buffers, |api| unsafe { kumo_metal_cases(api) });
    if buffers.live_count() != 0 {
        return 1001;
    }
    failed
}
