//j747
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
/// `value`, then status 0 in the next word. The group is created on the service VM at first
/// use; points run 1, 2, ... Returns 0 on completion with GPU status 0. — TIMBERDOODLE
#[no_mangle]
pub extern "C" fn kumo_metal_store(
    context: *mut c_void,
    stream_cpu: *mut u64,
    stream_va: u64,
    target: u64,
    value: u32,
) -> u32 {
    if context.is_null() || stream_cpu.is_null() || stream_va % 64 != 0 {
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
    let words = [
        (1u64 << 56) | (target & 0x0000_ffff_ffff_ffff), // MOVE48 r0, target
        (2u64 << 56) | (2u64 << 48) | u64::from(value),  // MOVE32 r2, value
        (38u64 << 56) | (2u64 << 32),                    // SYNC_SET32 slot0, [r0], r2
        (3u64 << 56) | (1u64 << 16),                     // WAIT slot0
    ];
    for (i, word) in words.iter().enumerate() {
        // SAFETY: four words inside the caller's mapped, line-aligned stream slot.
        unsafe { stream_cpu.add(i).write_volatile(*word) };
    }
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
    let point = NEXT_POINT.fetch_add(1, Ordering::Relaxed);
    let submission = Submission {
        group,
        stream_va,
        stream_bytes: (words.len() * 8) as u32,
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
