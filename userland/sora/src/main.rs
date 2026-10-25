#![no_std]
#![no_main]

//j606
//j607
//j609
//j611
//j614

extern crate alloc;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use drv_blk::BlockAccess;
use kumo_abi::{
    BootInfo, Errno, Framebuffer, Handle, ProcessRunFlags, Rights, VmarFlags, AUTOEXEC_PATH,
    CAT_PATH, DRV_GPU_PATH, DRV_XHCI_PATH, FAT32_IMG_PATH, FATFS_PATH, GFX_CLIENT_PATH, GROUP_PATH,
    HOSTNAME_PATH, IDENTITYD_PATH, KILO_PATH, LS_PATH, PASSWD_PATH, PERSONA_LINUX_HELLO_PATH,
    RELIBC_SMOKE_PATH, SHADOW_PATH, SVC_HEALTH_PATH, TTYD_PATH, WC_PATH,
};
use kumo_fatfs::{FatVolume, SectorReader};
use kumo_i2c_hid::{HidDeviceKind, I2cHidBusTopology};
use kumo_identity::protocol::{
    bundle_header, Reply as IdentityReply, Request as IdentityRequest, BUNDLE_HEADER_LEN,
    REPLY_MAX as IDENTITY_REPLY_MAX, REQUEST_MAX as IDENTITY_REQUEST_MAX,
};
use kumo_rt::{
    address_space_create, channel_create, channel_create_pair, channel_read,
    channel_read_with_handle, channel_write, channel_write_with_handle, handle_close,
    handle_duplicate, handle_koid, interrupt_create, port_bind, port_create, port_unbind,
    port_wait, process_create, process_kill, process_run, process_wait, resource_create_child,
    syscall_handle, thread_create, thread_start, timer_create, vmar_map, vmo_create, vmo_read,
    vmo_write, STARTUP_TAG_ARGV, STARTUP_TAG_CAP0, STARTUP_TAG_FS, STARTUP_TAG_STDIN,
    STARTUP_TAG_STDOUT, STARTUP_TAG_TTY,
};
use kumoza::{parse, DirectoryError, WorkingDirectory};
use persona_linux::{arm64 as linux_arm64, elf as linux_elf};
use persona_posix::{FdTable, TtyRpc, TtyRpcTransport, TtyStream, STDIN_FILENO, STDOUT_FILENO};
use svc_health::{Request as HealthRequest, Response as HealthResponse};
use ttyd::{Reply as TtyReply, Request as TtyRequest};

/// The BootInfo VMO handle the kernel hands Sora in **x8** — the 9th bootstrap value, beyond
/// the C-ABI argument registers x0–x7 (J159). x8 is caller-saved, so reading it inside
/// `sora_main` via inline asm is unsound: the compiler may clobber x8 before the read. It did
/// exactly that on the live X13s boot — reusing x8 as the `DebugWrite` syscall number (29) for
/// the greeting and reordering the `options(nomem)` read after it — so Sora saw handle 29 and
/// the BootInfo self-map returned `BadHandle` (`st=0xfffffffe`). Capture x8 in `_start`, the
/// one place it is still guaranteed live, before any other instruction can touch it.
static BOOTINFO_VMO_HANDLE: AtomicU64 = AtomicU64::new(0);

/// Read-only DTB VMO handle supplied in x9. `_start` must preserve it before using x9 as scratch.
static DTB_VMO_HANDLE: AtomicU64 = AtomicU64::new(0);

/// A reusable scratch VMO for packing a program's argv before launch. Created once and
/// rewritten per spawn; `ARGV_VMO` is Sora's full-rights write handle. Each launch mints a
/// fresh one-shot read+transfer alias from it for the bootstrap startup message (least
/// privilege — the child reads its args, can't mutate the shared buffer). Runs are
/// synchronous, so one buffer is safe to reuse. The static avoids threading the handle
/// through the shell dispatcher.
static ARGV_VMO: AtomicU64 = AtomicU64::new(0);

/// Supervisor-owned writable copy of `bin/fat32.img`. A `fatfs` instance receives
/// only a narrowed duplicate, so its restart drops open-table soft state while the
/// interop-volume bytes survive for the rest of this boot. The immutable initrd is
/// never writable. This is the RAM-backed bridge before PLAN/008 S3 supplies a
/// persistent block device, not the DESIGN/010 RedoxFS root. — KESTREL
static FAT_IMAGE_VMO: AtomicU64 = AtomicU64::new(0);
static FAT_IMAGE_LEN: AtomicU64 = AtomicU64::new(0);

/// The presentation clients are supervisor-owned demo views. No app receives hardware
/// authority. A failed round trip disables that view until the service is re-established.
/// The arrays are written for the two-view contract; this keeps them in step.
const _: () = assert!(kumo_graphics::VIEW_COUNT == 2);
static GRAPHICS_CHANNEL: [AtomicU32; kumo_graphics::VIEW_COUNT] =
    [AtomicU32::new(0), AtomicU32::new(0)];
static GRAPHICS_VMO: [AtomicU32; kumo_graphics::VIEW_COUNT] =
    [AtomicU32::new(0), AtomicU32::new(0)];
static GRAPHICS_SEQUENCE: [AtomicU32; kumo_graphics::VIEW_COUNT] =
    [AtomicU32::new(1), AtomicU32::new(1)];

/// G5b: the sending end of drv-gpu's bootstrap channel. It is held only on a board with the
/// GPU's power unit, from drv-gpu's launch until drv-fb's views exist and view
/// `drv_gpu::GPU_VIEW` has been handed over; otherwise it is closed so drv-gpu stops waiting.
static GPU_VIEW_BOOTSTRAP: AtomicU32 = AtomicU32::new(0);
/// One cooperative G6b lease per boot. It is transferred once and never re-granted after
/// client/driver failure; a new boot resets the device before any fresh grant. — TIMBERDOODLE
static GPU_CLIENT_CHANNEL: AtomicU32 = AtomicU32::new(0);

fn launch_gpu_client(initrd: Handle) {
    let channel = GPU_CLIENT_CHANNEL.swap(0, Ordering::Relaxed);
    if channel == 0 {
        return;
    }
    let flags = (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG).bits();
    if run_elf(
        initrd,
        kumo_abi::initrd::GPU_CLIENT_PATH.as_bytes(),
        u64::from(channel),
        0,
        flags,
        b"gpu-client",
    ) {
        log(b"gpu-client: session handed to a separate process\n");
    } else {
        let _ = handle_close(Handle(channel));
        log(b"gpu-client: launch failed; session retired\n");
    }
}

/// Hand one view's grant (its presentation channel and writable surface) to drv-gpu, which
/// paints and presents it. Returns false, leaving the grant with Sora, when no GPU driver is
/// waiting or the first send fails; a refused send leaves the handle in Sora's table.
/// — TIMBERDOODLE
fn hand_view_to_gpu(view: usize, channel: Handle, vmo: Handle) -> bool {
    let sender = Handle(GPU_VIEW_BOOTSTRAP.swap(0, Ordering::Relaxed));
    if sender.0 == 0 {
        return false;
    }
    let tag = [drv_gpu::BOOTSTRAP_VIEW_CHANNEL];
    if channel_write_with_handle(sender, tag.as_ptr(), 1, channel) != 0 {
        let _ = handle_close(sender);
        log(b"drv-gpu: view handoff refused; Sora keeps the view\n");
        return false;
    }
    let tag = [drv_gpu::BOOTSTRAP_VIEW_SURFACE];
    if channel_write_with_handle(sender, tag.as_ptr(), 1, vmo) != 0 {
        // The channel already moved; without its surface the view cannot be painted.
        let _ = handle_close(vmo);
        log(b"drv-gpu: view surface handoff failed\n");
    } else {
        log(b"drv-gpu: view ");
        log(&[b'0' + view as u8]);
        log(b" handed to the GPU driver\n");
    }
    let _ = handle_close(sender);
    true
}

/// Close drv-gpu's bootstrap sender when no view went to it (no framebuffer, no band), so its
/// read ends with PeerClosed instead of waiting for the rest of the boot.
fn release_gpu_bootstrap() {
    let sender = GPU_VIEW_BOOTSTRAP.swap(0, Ordering::Relaxed);
    if sender != 0 {
        let _ = handle_close(Handle(sender));
    }
}

/// Queue each view's surface grant on its own graphics channel before handing the server
/// ends to drv-fb. READ permits a read-only VmarMap in the current ABI; TRANSFER delivers
/// the grant. A view that fails setup still sends its G record with Handle(0), keeping the
/// bootstrap stream aligned; drv-fb reads one G per view.
fn bootstrap_graphics(
    bootstrap: Handle,
    enabled: bool,
) -> [Option<(Handle, Handle)>; kumo_graphics::VIEW_COUNT] {
    let mut grants: [Option<(Handle, Handle)>; kumo_graphics::VIEW_COUNT] =
        core::array::from_fn(|_| None);
    for grant in grants.iter_mut() {
        if !enabled {
            let _ = channel_write_with_handle(bootstrap, b"G".as_ptr(), 1, Handle(0));
            continue;
        }
        let (client, server) = channel_create_pair();
        let vmo = vmo_create(kumo_graphics::SURFACE_BYTES as u64);
        let valid = |raw| raw != 0 && raw != u64::MAX;
        let surface = if valid(vmo) {
            handle_duplicate(Handle(vmo as u32), Rights::READ | Rights::TRANSFER)
        } else {
            0
        };
        let mut sent_surface = false;
        if [client, server, vmo, surface].into_iter().all(valid) {
            sent_surface = channel_write_with_handle(
                Handle(client as u32),
                b"S".as_ptr(),
                1,
                Handle(surface as u32),
            ) == 0;
            if sent_surface
                && channel_write_with_handle(bootstrap, b"G".as_ptr(), 1, Handle(server as u32))
                    == 0
            {
                *grant = Some((Handle(client as u32), Handle(vmo as u32)));
                continue;
            }
        }
        for raw in [client, server, vmo] {
            if valid(raw) {
                let _ = handle_close(Handle(raw as u32));
            }
        }
        if valid(surface) && !sent_surface {
            let _ = handle_close(Handle(surface as u32));
        }
        // Keep the stream aligned even when this view failed.
        let _ = channel_write_with_handle(bootstrap, b"G".as_ptr(), 1, Handle(0));
    }
    grants
}

fn graphics_demo(cmd: &kumoza::Command) {
    use kumo_graphics::{
        clip_demo_pixel, demo_pixel, Present, Reply, Status, Surface, CLIP_DEMO, DEMO, REPLY_LEN,
        VIEW_COUNT,
    };
    if !cmd.args.is_empty() {
        usage(b"usage: gfx-demo\n");
        return;
    }
    let mut copied = [false; VIEW_COUNT];
    let mut failed = false;
    for view in 0..VIEW_COUNT {
        let channel = Handle(GRAPHICS_CHANNEL[view].swap(0, Ordering::Relaxed));
        let vmo = Handle(GRAPHICS_VMO[view].load(Ordering::Relaxed));
        if channel.0 == 0 {
            // A separate client owns this view now (F1b); Sora paints only what it holds.
            continue;
        }
        if vmo.0 == 0 {
            let _ = handle_close(channel);
            failed = true;
            continue;
        }
        let (surface, pixel): (Surface, fn(u32, u32) -> u32) = if view == 0 {
            (DEMO, demo_pixel)
        } else {
            (CLIP_DEMO, clip_demo_pixel)
        };
        let result = (|| {
            // Let drv-fb drain the command echo before enqueueing the present request.
            if !child_parked(process_wait()) {
                return None;
            }
            let mut row = [0u8; 320 * 4];
            let row_bytes = surface.stride as usize * 4;
            for y in 0..surface.height {
                for x in 0..surface.width {
                    let offset = x as usize * 4;
                    row[offset..offset + 4].copy_from_slice(&pixel(x, y).to_le_bytes());
                }
                for (chunk_index, chunk) in row[..row_bytes].chunks(256).enumerate() {
                    let offset = u64::from(y * surface.stride * 4) + (chunk_index * 256) as u64;
                    if vmo_write(vmo, offset, chunk.as_ptr(), chunk.len()) != 0 {
                        return None;
                    }
                }
            }
            let sequence = GRAPHICS_SEQUENCE[view].fetch_add(1, Ordering::Relaxed);
            let frame = Present { sequence, surface }.encode();
            if channel_write(channel, frame.as_ptr(), frame.len()) != 0
                || !child_parked(process_wait())
            {
                return None;
            }
            let mut reply = [0u8; REPLY_LEN + 1];
            let len = channel_read(channel, reply.as_mut_ptr(), reply.len()) as usize;
            let reply = Reply::decode(reply.get(..len)?)?;
            (reply.sequence == sequence).then_some(reply.status)
        })();
        match result {
            Some(status) => {
                // A valid reply releases this surface even when its view rejected it.
                GRAPHICS_CHANNEL[view].store(channel.0, Ordering::Relaxed);
                if status == Status::Copied {
                    copied[view] = true;
                    log(b"gfx-demo: view ");
                    log(&[b'0' + view as u8]);
                    log(b" copied; buffer released (software GOP)\n");
                } else {
                    failed = true;
                }
            }
            None => {
                let _ = handle_close(channel);
                failed = true;
            }
        }
    }
    if failed {
        log(b"gfx-demo: one or more views failed; those views disabled\n");
        fail_status(1);
    } else if !copied.iter().any(|value| *value) {
        log(b"gfx-demo: no graphics view held\n");
        fail_status(1);
    }
}

/// Hand one supervisor-held presentation grant to a separate process (F1b). The child receives
/// the client end of the channel in x0 and the surface VMO in x1; both handles move into the
/// child's table. A grant that is not admitted stays with Sora and is closed.
fn graphics_client(cmd: &kumoza::Command, initrd: Handle) {
    use kumo_graphics::VIEW_COUNT;
    if !cmd.args.is_empty() {
        usage(b"usage: gfx-client\n");
        return;
    }
    let mut selected = None;
    for view in 0..VIEW_COUNT {
        let channel = Handle(GRAPHICS_CHANNEL[view].swap(0, Ordering::Relaxed));
        if channel.0 == 0 {
            continue;
        }
        let vmo = Handle(GRAPHICS_VMO[view].swap(0, Ordering::Relaxed));
        selected = Some((view, channel, vmo));
        break;
    }
    let Some((view, channel, vmo)) = selected else {
        log(b"gfx-client: no graphics view available\n");
        fail_status(1);
        return;
    };
    if vmo.0 == 0 {
        let _ = handle_close(channel);
        log(b"gfx-client: no surface available\n");
        fail_status(1);
        return;
    }
    log(b"gfx-client: view ");
    log(&[b'0' + view as u8]);
    log(b" handed to a client process\n");
    let flags = (ProcessRunFlags::TRANSFER_ARG | ProcessRunFlags::TRANSFER_ARG2).bits();
    let ran = run_elf_threads(
        initrd,
        GFX_CLIENT_PATH.as_bytes(),
        channel.0 as u64,
        vmo.0 as u64,
        flags,
        b"gfx-client",
        0,
        None,
    );
    if !ran {
        // An admitted child owns the handles even if it exited non-zero; only an admission
        // failure leaves them with Sora.
        if handle_koid(channel) != u64::MAX {
            let _ = handle_close(channel);
            let _ = handle_close(vmo);
        }
        fail_status(1);
    }
}

// Sora's entry. Mirrors `kumo_rt::entry!` (a bare `bl sora_main`) but first stashes x8 into
// `BOOTINFO_VMO_HANDLE`. x9 is scratch; sora_main is reached with x0–x7 (its C-ABI args)
// untouched. `_start` runs at EL0 with Sora's address space live, so the static (.bss, mapped
// RW) is writable here.
core::arch::global_asm!(
    ".section .text._start, \"ax\"",
    ".global _start",
    "_start:",
    "  adrp x10, {dtb_slot}",
    "  add  x10, x10, :lo12:{dtb_slot}",
    "  str  x9, [x10]",
    "  adrp x9, {slot}",
    "  add  x9, x9, :lo12:{slot}",
    "  str  x8, [x9]",
    "  bl   sora_main",
    "1: b 1b",
    slot = sym BOOTINFO_VMO_HANDLE,
    dtb_slot = sym DTB_VMO_HANDLE,
);

fn log(msg: &[u8]) {
    debug_write(msg.as_ptr(), msg.len());
}

/// Set when the serve loop is about to draw its first prompt (J559). Until then everything Sora
/// prints is a boot record: its own diagnostics and the output of autoexec's smoke programs go
/// to the kernel log, where `dmesg` shows them, instead of across the console. From then on
/// `debug_write` is the terminal again, and serve-loop diagnostics say so with [`diag`].
/// — TIMBERDOODLE
static SERVING: AtomicBool = AtomicBool::new(false);

/// Sora's console write: the kernel log while booting, the terminal once serving.
fn debug_write(ptr: *const u8, len: usize) -> u64 {
    if SERVING.load(Ordering::Relaxed) {
        kumo_rt::debug_write(ptr, len)
    } else {
        let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
        kumo_rt::log_write(kumo_abi::sys::log_level::INFO, bytes)
    }
}

/// A boot milestone: one short line the console's default level shows, written in one call so
/// no other source can land inside it (J559).
fn notice(parts: &[&[u8]]) {
    let mut line = alloc::vec::Vec::new();
    for part in parts {
        line.extend_from_slice(part);
    }
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, &line);
}

/// The longest the first prompt waits for boot to settle (J559). The opi5's USB keyboard and
/// stick finish enumerating about 1.5 s after the serve loop starts; their milestone lines belong
/// above the prompt, not after it.
const BOOT_SETTLE_NS: u64 = 3_000_000_000;

/// Arm the first-prompt timer on `port`; `None` when no timer could be bound.
fn arm_first_prompt(port: Handle) -> Option<(Handle, Handle)> {
    let raw = timer_create(BOOT_SETTLE_NS);
    if raw == u64::MAX {
        return None;
    }
    let timer = Handle(raw as u32);
    let koid = handle_koid(timer);
    if koid == u64::MAX || port_bind(port, timer) != 0 {
        let _ = handle_close(timer);
        return None;
    }
    Some((timer, Handle(koid as u32)))
}

/// Draw the first prompt now if it is still waiting: boot has settled.
fn settle_first_prompt(
    pending: &mut Option<(Handle, Handle)>,
    port: Handle,
    cwd: &WorkingDirectory,
    reason: &[u8],
) {
    if let Some((timer, _)) = pending.take() {
        let _ = port_unbind(port, timer);
        let _ = handle_close(timer);
        diag(b"sora: boot settled by ");
        diag(reason);
        diag(b"\n");
        write_shell_prompt(cwd);
    }
}

/// A diagnostic from the serve loop: kernel log only, at INFO.
fn diag(msg: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::INFO, msg);
}

fn diag_hex(v: u64) {
    let mut buf = [0u8; 18];
    buf[..2].copy_from_slice(b"0x");
    let mut digits = [0u8; 16];
    let mut n = 0;
    let mut v = v;
    loop {
        let d = (v & 0xf) as u8;
        digits[n] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        n += 1;
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    for i in 0..n {
        buf[2 + i] = digits[n - 1 - i];
    }
    diag(&buf[..2 + n]);
}

/// The root resource, kept for `dmesg`: reading the kernel log takes MANAGE (J559).
static ROOT_RESOURCE: AtomicU64 = AtomicU64::new(0);

fn write_shell_prompt(cwd: &WorkingDirectory) {
    let Some(user) = session() else {
        write_login_prompt();
        return;
    };
    let host = HOSTNAME.get();
    let prompt = kumoza::Prompt {
        user: user.name.as_str(),
        host: host.as_str(),
        root: user.root,
        cwd: cwd.path(),
        home: user.home.as_str(),
        status: LAST_STATUS.load(Ordering::Relaxed).min(255) as u8,
    }
    .render();
    // Rendered whole and written back to back, so a driver line cannot split it except at a
    // 256-byte DebugWrite boundary.
    let _ = sora::relay_console_output(&prompt, |chunk| debug_write(chunk.as_ptr(), chunk.len()));
}

/// Log a `u64` as `0x…` hex on the live serial. `no_std` Sora has no formatter, so a
/// failing syscall's raw status is otherwise invisible; this surfaces the kernel `Errno`
/// behind a collapsed "fail" so a hardware boot localises the cause (mirrors the inline
/// child-handle dump already used below).
fn log_hex(mut v: u64) {
    debug_write(b"0x".as_ptr(), 2);
    let mut buf = [0u8; 16];
    let mut hi = 16;
    loop {
        hi -= 1;
        let d = (v & 0xF) as u8;
        buf[hi] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    debug_write(buf[hi..].as_ptr(), 16 - hi);
}

/// Dump framebuffer geometry as `phys=… len=… w=… h=… stride=…`. Used by both drv-fb
/// spawn failure paths so a corrupt or rejected geometry is always fully visible.
fn log_fb_geometry(fb: &Framebuffer) {
    log(b"phys=");
    log_hex(fb.phys);
    log(b" len=");
    log_hex(fb.len);
    log(b" w=");
    log_hex(fb.width as u64);
    log(b" h=");
    log_hex(fb.height as u64);
    log(b" stride=");
    log_hex(fb.stride as u64);
    log(b"\n");
}

fn log_process_koid(name: &[u8], process: Handle) {
    diag(name);
    diag(b": proc koid=");
    let koid = handle_koid(process);
    if koid == u64::MAX {
        diag(b"fail\n");
    } else {
        diag_hex(koid);
        diag(b"\n");
    }
}

/// Map the read-only DTB capability in two stages, validate its declared extent, and discover the
/// X13s i2c21 HID children plus pinctrl plan before any device Resource is minted or controller
/// MMIO is touched.
fn i2c_hid_runtime_topology_from_dtb(
    dtb_vmo: Handle,
    dtb_phys: u64,
) -> Option<sora::I2cHidRuntimeTopology> {
    const PAGE_SIZE: u64 = 4096;
    const HEADER_VA: u64 = 0x0000_0000_3100_0000;
    const DTB_VA: u64 = 0x0000_0000_3200_0000;
    const FDT_MAGIC: u32 = 0xd00d_feed;
    const MAX_DTB_BYTES: usize = 16 * 1024 * 1024;

    if dtb_vmo.0 == 0 || dtb_phys == 0 {
        return None;
    }
    let offset = (dtb_phys & (PAGE_SIZE - 1)) as usize;
    if offset > PAGE_SIZE as usize - 8
        || vmar_map(
            Handle(0),
            dtb_vmo,
            0,
            HEADER_VA,
            PAGE_SIZE,
            VmarFlags::READ.0,
        ) != 0
    {
        return None;
    }
    let header =
        unsafe { core::slice::from_raw_parts((HEADER_VA as usize + offset) as *const u8, 8) };
    if u32::from_be_bytes(header[..4].try_into().ok()?) != FDT_MAGIC {
        return None;
    }
    let total = u32::from_be_bytes(header[4..8].try_into().ok()?) as usize;
    if !(40..=MAX_DTB_BYTES).contains(&total) {
        return None;
    }
    let mapped_len = (offset as u64)
        .checked_add(total as u64)?
        .checked_add(PAGE_SIZE - 1)?
        & !(PAGE_SIZE - 1);
    if vmar_map(Handle(0), dtb_vmo, 0, DTB_VA, mapped_len, VmarFlags::READ.0) != 0 {
        return None;
    }
    let bytes =
        unsafe { core::slice::from_raw_parts((DTB_VA as usize + offset) as *const u8, total) };
    sora::discover_i2c_hid_runtime_topology(bytes)
}

fn xhci_usb0_probe_from_dtb(dtb_vmo: Handle, dtb_phys: u64) -> Option<kumo_xhci::XhciProbeConfig> {
    const PAGE_SIZE: u64 = 4096;
    const HEADER_VA: u64 = 0x0000_0000_3300_0000;
    const DTB_VA: u64 = 0x0000_0000_3400_0000;
    const FDT_MAGIC: u32 = 0xd00d_feed;
    const MAX_DTB_BYTES: usize = 16 * 1024 * 1024;

    if dtb_vmo.0 == 0 || dtb_phys == 0 {
        return None;
    }
    let offset = (dtb_phys & (PAGE_SIZE - 1)) as usize;
    if offset > PAGE_SIZE as usize - 8
        || vmar_map(
            Handle(0),
            dtb_vmo,
            0,
            HEADER_VA,
            PAGE_SIZE,
            VmarFlags::READ.0,
        ) != 0
    {
        return None;
    }
    let header =
        unsafe { core::slice::from_raw_parts((HEADER_VA as usize + offset) as *const u8, 8) };
    if u32::from_be_bytes(header[..4].try_into().ok()?) != FDT_MAGIC {
        return None;
    }
    let total = u32::from_be_bytes(header[4..8].try_into().ok()?) as usize;
    if !(40..=MAX_DTB_BYTES).contains(&total) {
        return None;
    }
    let mapped_len = (offset as u64)
        .checked_add(total as u64)?
        .checked_add(PAGE_SIZE - 1)?
        & !(PAGE_SIZE - 1);
    if vmar_map(Handle(0), dtb_vmo, 0, DTB_VA, mapped_len, VmarFlags::READ.0) != 0 {
        return None;
    }
    let bytes =
        unsafe { core::slice::from_raw_parts((DTB_VA as usize + offset) as *const u8, total) };
    kumo_xhci::discover_primary_xhci(bytes)?.probe_config()
}

/// RK3588 has three xHCI controllers. usb0 @ 0xfc000000 is the USB-C role-switch port; usb1 and
/// host2 front the USB-A ports where a keyboard is likely attached. A USB-A device is invisible to
/// a driver bound only to usb0, so spawn a drv-xhci per controller and let each enumerate its own.
/// The register window is 0x8000; enumeration polls, so a placeholder IRQ is fine for usb1/host2.
const RK3588_USB1_XHCI_MMIO_BASE: u64 = 0xfc40_0000;
const RK3588_HOST2_XHCI_MMIO_BASE: u64 = 0xfcd0_0000;
const RK3588_XHCI_REGISTER_MMIO_LEN: u64 = 0x8000;
/// GIC INTIDs for the other two xHCI controllers. `rk3588-base.dtsi`/`rk3588-extra.dtsi` list
/// usb_host0/1/2 as GIC_SPI 220/221/222, and an SPI's INTID is its number + 32 (usb0's 220 → 252
/// matches the DTB-derived config). Without a real IRQ a driver has to busy-poll for reports, and
/// at driver priority that starves the whole system — every controller gets its line.
const RK3588_USB1_XHCI_IRQ: u32 = 253;
const RK3588_HOST2_XHCI_IRQ: u32 = 254;

/// U5: bootstrap tag preceding the keyboard-channel writer handed to a drv-xhci instance.
const XHCI_KEYBOARD_BOOTSTRAP_TAG: u8 = b'k';
/// ...and the pointer channel. A USB boot mouse emits the same 3-byte `[buttons, dx, dy]` frame
/// the i2c-hid path does, so it joins the existing `PointerState` pipeline unchanged.
const XHCI_MOUSE_BOOTSTRAP_TAG: u8 = b'm';
/// J545: ...and the storage control channel. A driver that finds a USB stick offers its volume
/// on it, and Sora answers with one block channel per `fatfs` instance it starts for the volume.
const XHCI_STORAGE_BOOTSTRAP_TAG: u8 = b'b';
/// Sora's ends of the drivers' storage control channels, one per controller (usb0, usb1,
/// host2); 0 = none.
static XHCI_STORAGE_CONTROLS: [AtomicU32; 3] =
    [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];

/// The removable volume Sora knows, mounted at `/media/<name>` (DESIGN/012 §2.1, J545).
#[derive(Clone, Copy)]
struct Media {
    /// The `/media/<name>` component the shell reaches it by.
    name: sora::MediaName,
    ready: drv_blk::MediaReady,
    /// The offering driver's control channel: where a new client's block channel goes.
    control: Handle,
}

struct MediaCell(core::cell::UnsafeCell<Option<Media>>);

// SAFETY: Sora is single-threaded, and `media`/`set_media` copy in and out without keeping a
// reference.
unsafe impl Sync for MediaCell {}

static MEDIA: MediaCell = MediaCell(core::cell::UnsafeCell::new(None));

fn media() -> Option<Media> {
    // SAFETY: see `MediaCell`.
    unsafe { *MEDIA.0.get() }
}

fn set_media(media: Option<Media>) {
    // SAFETY: see `MediaCell`.
    unsafe { *MEDIA.0.get() = media }
}

/// Spawn a drv-xhci per xHCI controller and return the keyboard-channel reader (bound to the serve
/// port → shell). The writer goes to the usb0 driver — where the keyboard enumerates on a root port
/// today; usb1/host2 get none yet (they reach a keyboard via the hub path, U2).
fn launch_xhci_first_light(
    initrd: Handle,
    root_resource: Handle,
    dtb_vmo: Handle,
    dtb_phys: u64,
) -> (Option<Handle>, Option<Handle>) {
    let (kbd_reader, kbd_writer) = channel_create_pair();
    let have_channel = kbd_reader != u64::MAX && kbd_writer != u64::MAX;
    if !have_channel {
        if kbd_reader != u64::MAX {
            let _ = handle_close(Handle(kbd_reader as u32));
        }
        if kbd_writer != u64::MAX {
            let _ = handle_close(Handle(kbd_writer as u32));
        }
    }
    let (mouse_reader, mouse_writer) = channel_create_pair();
    let have_mouse = mouse_reader != u64::MAX && mouse_writer != u64::MAX;
    if !have_mouse {
        if mouse_reader != u64::MAX {
            let _ = handle_close(Handle(mouse_reader as u32));
        }
        if mouse_writer != u64::MAX {
            let _ = handle_close(Handle(mouse_writer as u32));
        }
    }

    // Every controller gets its own writer for the one keyboard channel: a keyboard may enumerate
    // on any root port OR behind a hub on any controller, and whichever driver finds it delivers
    // to the same reader Sora keeps. Each spawn needs a distinct handle, so duplicate per child
    // (the engine requires Rights::TRANSFER on a transferred handle).
    let dup_writer = |have: bool, writer: u64| -> u64 {
        if !have {
            return u64::MAX;
        }
        handle_duplicate(
            Handle(writer as u32),
            Rights::READ | Rights::WRITE | Rights::TRANSFER,
        )
    };
    let writer_for = |have: bool| -> u64 { dup_writer(have, kbd_writer) };
    let mouse_for = |have: bool| -> u64 { dup_writer(have, mouse_writer) };
    // J545: a storage control channel per controller. Sora keeps controller `index`'s end and
    // the driver gets the other; a failed pair just means that controller offers no storage.
    let storage_for = |index: usize| -> u64 {
        let (sora_end, driver_end) = channel_create_pair();
        if sora_end == u64::MAX || driver_end == u64::MAX {
            for end in [sora_end, driver_end] {
                if end != u64::MAX {
                    let _ = handle_close(Handle(end as u32));
                }
            }
            log(b"drv-xhci: storage control unavailable\n");
            return u64::MAX;
        }
        XHCI_STORAGE_CONTROLS[index].store(sora_end as u32, Ordering::Relaxed);
        driver_end
    };

    match xhci_usb0_probe_from_dtb(dtb_vmo, dtb_phys) {
        Some(config) => spawn_xhci(
            initrd,
            root_resource,
            config,
            writer_for(have_channel),
            mouse_for(have_mouse),
            storage_for(0),
        ),
        None => log(b"drv-xhci: no supported usb0 xhci\n"),
    }
    spawn_xhci(
        initrd,
        root_resource,
        kumo_xhci::XhciProbeConfig::new(
            RK3588_USB1_XHCI_MMIO_BASE,
            RK3588_XHCI_REGISTER_MMIO_LEN,
            RK3588_USB1_XHCI_IRQ,
            kumo_xhci::XHCI_NO_STREAM_ID,
        ),
        writer_for(have_channel),
        mouse_for(have_mouse),
        storage_for(1),
    );
    spawn_xhci(
        initrd,
        root_resource,
        kumo_xhci::XhciProbeConfig::new(
            RK3588_HOST2_XHCI_MMIO_BASE,
            RK3588_XHCI_REGISTER_MMIO_LEN,
            RK3588_HOST2_XHCI_IRQ,
            kumo_xhci::XHCI_NO_STREAM_ID,
        ),
        writer_for(have_channel),
        mouse_for(have_mouse),
        storage_for(2),
    );
    // The originals stay with Sora only as duplication sources; close them.
    if have_channel {
        let _ = handle_close(Handle(kbd_writer as u32));
    }
    if have_mouse {
        let _ = handle_close(Handle(mouse_writer as u32));
    }

    (
        have_channel.then(|| Handle(kbd_reader as u32)),
        have_mouse.then(|| Handle(mouse_reader as u32)),
    )
}

/// DESIGN/020 D0-1: a display probe grant carries READ (to mint a read-only VMO), DUPLICATE
/// (D0b mints the VOP a second time) and TRANSFER, never WRITE. The writable child is closed,
/// so drv-fb cannot map these registers writable even by mistake. — TIMBERDOODLE
fn read_only_grant(raw: u64) -> Handle {
    let Some(writable) = syscall_handle(raw) else {
        return Handle(0);
    };
    let narrowed = handle_duplicate(
        writable,
        Rights::READ | Rights::DUPLICATE | Rights::TRANSFER,
    );
    let _ = handle_close(writable);
    syscall_handle(narrowed).unwrap_or(Handle(0))
}

/// D0a: observe shared PMU status in the supervisor, then pass data and one VOP Resource.
/// No PMU/CRU capability or IRQ authority reaches the display driver. Only the exact Orange
/// Pi board with a plausible existing GOP image is probed. — TIMBERDOODLE
fn bootstrap_display_probe(bootstrap: Handle, root: Handle, board: &str, framebuffer_valid: bool) {
    use drv_fb::rk3588::{eligible, Power, PMU_BASE, PMU_LEN, VOP_BASE, VOP_LEN};
    use drv_fb::rk3588_clock::{
        Grant, CLOCK_PROBE_IRQ_BASE, CLOCK_PROBE_IRQ_COUNT, CRU_BASE, CRU_LEN, GRANT_LEN,
    };
    use drv_fb::rk3588_hdmi::{
        hdmi_accessible, HDMI_BASE, HDMI_LEN, PHY_BASE, PHY_GRF_BASE, PHY_GRF_LEN, PHY_LEN,
    };
    use drv_fb::rk3588_shared::{
        SYS_GRF_BASE, SYS_GRF_LEN, VO1_GRF_BASE, VO1_GRF_LEN, VOP_GRF_BASE, VOP_GRF_LEN,
    };
    let mut power = None;
    // D0d: PMU_BIU_IDLE_SFTCON1 bit 0 is VO1's idle request enable, which D0a's fixed
    // 16-byte record does not carry. Sora observes it read-only for the HDMI gate.
    // — TIMBERDOODLE
    let mut vo1_request = 0u32;
    if eligible(board, framebuffer_valid) {
        // Sora already owns root hardware authority. Its sole PMU mapping is read-only;
        // D0a leaves shared power/clock mutations with DESIGN/020's kernel mechanism.
        // — TIMBERDOODLE
        const PMU_VA: u64 = 0x3500_0000;
        let raw = kumo_rt::resource_mint_mmio(root, PMU_BASE, PMU_LEN);
        if let Some(vmo) = syscall_handle(raw) {
            if vmar_map(Handle(0), vmo, 0, PMU_VA, PMU_LEN, VmarFlags::READ.0) == 0 {
                vo1_request = unsafe { core::ptr::read_volatile((PMU_VA + 0x110) as *const u32) };
                power = Some(Power::read(|offset| unsafe {
                    core::ptr::read_volatile((PMU_VA + u64::from(offset)) as *const u32)
                }));
            }
            let _ = handle_close(vmo);
        }
    }
    let record = power.map(Power::encode).unwrap_or([0; 16]);
    let len = if power.is_some() { record.len() } else { 1 };
    let _ = channel_write(bootstrap, record.as_ptr(), len);
    let raw = if power.is_some_and(Power::vop_accessible) {
        resource_create_child(root, VOP_BASE, VOP_LEN, 0, 0)
    } else {
        0
    };
    let grant = read_only_grant(raw);
    if channel_write_with_handle(bootstrap, b"D".as_ptr(), 1, grant) != 0 && grant.0 != 0 {
        let _ = handle_close(grant);
    }

    // D0c: one read-only CRU child Resource, no IRQ authority. Data record first
    // (12 bytes, or one zero byte when absent), then the 'K' handle message, so
    // the bootstrap order stays F,C,B,power,D,K. The span covers CRU, PHPTOPCRU's
    // PPLL and PMU1CRU's PHY reference selector, within the board DTB's 0x5c000
    // CRU window. The grant is narrowed to READ (J605), so the wider span gives no write
    // reach: DESIGN/020's concern is write authority. — TIMBERDOODLE
    let cru_raw = if power.is_some_and(Power::vop_accessible) {
        resource_create_child(
            root,
            CRU_BASE,
            CRU_LEN,
            CLOCK_PROBE_IRQ_BASE,
            CLOCK_PROBE_IRQ_COUNT,
        )
    } else {
        0
    };
    let cru = read_only_grant(cru_raw);
    let mut record = [0u8; GRANT_LEN];
    if cru.0 != 0 {
        record = Grant::expected().encode();
    }
    let len = if cru.0 != 0 { record.len() } else { 1 };
    let _ = channel_write(bootstrap, record.as_ptr(), len);
    if channel_write_with_handle(bootstrap, b"K".as_ptr(), 1, cru) != 0 && cru.0 != 0 {
        let _ = handle_close(cru);
    }

    // D0d: HDMI0 TX, combo PHY and HDPTXPHY0_GRF as three read-only child Resources.
    // Every board receives the same three tagged records; an ineligible, powered-down or
    // idle-requested display path transfers Handle(0) instead of a capability. The PHY GRF
    // span is the DTB's 0x100 bytes. — TIMBERDOODLE
    let accessible = power.is_some_and(|power| hdmi_accessible(power) && vo1_request & 1 == 0);
    let mut grants = [Handle(0); 3];
    if accessible {
        let apertures = [
            (HDMI_BASE, HDMI_LEN),
            (PHY_BASE, PHY_LEN),
            (PHY_GRF_BASE, PHY_GRF_LEN),
        ];
        for (slot, (base, len)) in grants.iter_mut().zip(apertures) {
            *slot = read_only_grant(resource_create_child(root, base, len, 0, 0));
        }
    }
    for (tag, grant) in [b"H", b"P", b"R"].iter().zip(grants) {
        if channel_write_with_handle(bootstrap, tag.as_ptr(), 1, grant) != 0 && grant.0 != 0 {
            let _ = handle_close(grant);
        }
    }

    // R0 (DESIGN/020 Amendment D0): one read-only page each of VOP_GRF, VO1_GRF and SYS_GRF,
    // always sent as V/W/Y. VOP_GRF follows PD_VOP; VO1_GRF follows the HDMI gate above
    // (drv-fb also proves VO1's bus clocks open before it reads); SYS_GRF is always on. Every
    // grant is narrowed to READ. — TIMBERDOODLE
    let shared = [
        (
            power.is_some_and(Power::vop_accessible),
            VOP_GRF_BASE,
            VOP_GRF_LEN,
        ),
        (accessible, VO1_GRF_BASE, VO1_GRF_LEN),
        (power.is_some(), SYS_GRF_BASE, SYS_GRF_LEN),
    ];
    for (tag, (allowed, base, len)) in [b"V", b"W", b"Y"].iter().zip(shared) {
        let grant = if allowed {
            read_only_grant(resource_create_child(root, base, len, 0, 0))
        } else {
            Handle(0)
        };
        if channel_write_with_handle(bootstrap, tag.as_ptr(), 1, grant) != 0 && grant.0 != 0 {
            let _ = handle_close(grant);
        }
    }

    // DESIGN/022 D1d-a: VOP WRITE authorizes checked register calls and its IRQ; every raw
    // VOP/MMU mapping is read-only in the kernel. TX has no writers, so narrow its grant.
    // CRU/PMU/GRF diagnostic authority stays read-only. — KESTREL
    // D1b (J611): the VOP window also carries the VOP's interrupt, INTID 188 (SPI 156), which
    // it shares with the display IOMMU; drv-fb refuses to arm it unless every other source on
    // that line is off. The TX page carries no interrupt. — TIMBERDOODLE
    let windows = [
        (
            power.is_some_and(Power::vop_accessible),
            VOP_BASE,
            drv_fb::rk3588_mode::VOP_WINDOW_LEN,
            drv_fb::rk3588::VOP_INTID,
            1,
        ),
        (
            accessible,
            HDMI_BASE,
            drv_fb::rk3588_mode::TX_WINDOW_LEN,
            0,
            0,
        ),
    ];
    for (tag, (allowed, base, len, irq_base, irq_count)) in [b"U", b"X"].iter().zip(windows) {
        let grant = if allowed {
            let raw = resource_create_child(root, base, len, irq_base, irq_count);
            if base == HDMI_BASE {
                read_only_grant(raw)
            } else {
                syscall_handle(raw).unwrap_or(Handle(0))
            }
        } else {
            Handle(0)
        };
        if channel_write_with_handle(bootstrap, tag.as_ptr(), 1, grant) != 0 && grant.0 != 0 {
            let _ = handle_close(grant);
        }
    }
    // R2: writable PHY0/PHY0-GRF windows only. The PHY capability authorizes four indexed
    // reset lines; CRU and PMU mappings remain read-only. Send both tags on every board.
    // — KESTREL
    for (tag, base, len) in [(b"E", PHY_BASE, PHY_LEN), (b"Q", PHY_GRF_BASE, PHY_GRF_LEN)] {
        let grant = if accessible {
            syscall_handle(resource_create_child(root, base, len, 0, 0)).unwrap_or(Handle(0))
        } else {
            Handle(0)
        };
        if channel_write_with_handle(bootstrap, tag.as_ptr(), 1, grant) != 0 && grant.0 != 0 {
            let _ = handle_close(grant);
        }
    }
}

/// D0b: forward the *live* boot DTB's display topology to drv-fb. The kernel's x9 capability
/// carries READ|DUPLICATE but not TRANSFER, so the capability itself cannot cross a channel;
/// Sora maps it read-only, validates magic and extent, copies the blob into a fresh VMO, and
/// transfers a read-only alias. Every board receives the same 13-byte record, with Handle(0)
/// when ineligible, so the bootstrap stream stays aligned. Sora keeps its own live handle for
/// the i2c-hid and xHCI users. — TIMBERDOODLE
fn bootstrap_display_topology(
    bootstrap: Handle,
    dtb_vmo: Handle,
    dtb_phys: u64,
    board: &str,
    framebuffer_valid: bool,
) {
    let mut transfer = drv_fb::dtb_topology::Transfer::NONE;
    let mut grant = Handle(0);
    if drv_fb::rk3588::eligible(board, framebuffer_valid) {
        if let Some((copied, handle)) = copy_live_dtb(dtb_vmo, dtb_phys) {
            transfer = copied;
            grant = handle;
        }
    }
    let record = transfer.encode();
    if channel_write_with_handle(bootstrap, record.as_ptr(), record.len(), grant) != 0
        && grant.0 != 0
    {
        let _ = handle_close(grant);
    }
}

/// Copy the validated live DTB into a private VMO and return its transfer record plus a
/// read+transfer alias. `None` leaves the original capability untouched and sends no data.
/// — TIMBERDOODLE
fn copy_live_dtb(
    dtb_vmo: Handle,
    dtb_phys: u64,
) -> Option<(drv_fb::dtb_topology::Transfer, Handle)> {
    use drv_fb::dtb_topology::{checksum, Transfer, MAX_DTB_BYTES};
    const PAGE_SIZE: u64 = 4096;
    const HEADER_VA: u64 = 0x3600_0000;
    const DTB_VA: u64 = 0x3700_0000;
    const FDT_MAGIC: u32 = 0xd00d_feed;

    if dtb_vmo.0 == 0 || dtb_phys == 0 {
        return None;
    }
    let offset = (dtb_phys & (PAGE_SIZE - 1)) as usize;
    if offset > PAGE_SIZE as usize - 8
        || vmar_map(
            Handle(0),
            dtb_vmo,
            0,
            HEADER_VA,
            PAGE_SIZE,
            VmarFlags::READ.0,
        ) != 0
    {
        return None;
    }
    let header =
        unsafe { core::slice::from_raw_parts((HEADER_VA as usize + offset) as *const u8, 8) };
    if u32::from_be_bytes(header[..4].try_into().ok()?) != FDT_MAGIC {
        return None;
    }
    let total = u32::from_be_bytes(header[4..8].try_into().ok()?) as usize;
    if !(40..=MAX_DTB_BYTES).contains(&total) {
        return None;
    }
    let mapped_len = (offset as u64)
        .checked_add(total as u64)?
        .checked_add(PAGE_SIZE - 1)?
        & !(PAGE_SIZE - 1);
    if vmar_map(Handle(0), dtb_vmo, 0, DTB_VA, mapped_len, VmarFlags::READ.0) != 0 {
        return None;
    }
    let bytes =
        unsafe { core::slice::from_raw_parts((DTB_VA as usize + offset) as *const u8, total) };
    // The kernel moves at most `VMO_TRANSFER_MAX` bytes per VmoWrite and refuses more
    // (J601), so chunk at the ABI cap rather than the page size.
    let copy_handle = syscall_handle(vmo_create(total as u64))?;
    let mut written = 0usize;
    while written < total {
        let chunk = (total - written).min(kumo_abi::sys::VMO_TRANSFER_MAX);
        if vmo_write(
            copy_handle,
            written as u64,
            bytes[written..].as_ptr(),
            chunk,
        ) != 0
        {
            let _ = handle_close(copy_handle);
            return None;
        }
        written += chunk;
    }
    let alias = handle_duplicate(copy_handle, Rights::READ | Rights::TRANSFER);
    let _ = handle_close(copy_handle);
    let alias = syscall_handle(alias)?;
    Some((
        Transfer {
            total: total as u32,
            checksum: checksum(bytes),
        },
        alias,
    ))
}

/// G1b grants only the GPU child Resource. The kernel's live-DTB power-unit lookup rejects
/// absent hardware before touching registers, including on QEMU. No initrd, PMU or CRU grant
/// reaches the child. The one-shot identity probe exits after reporting. — KESTREL
fn launch_gpu_identity(initrd: Handle, root_resource: Handle) {
    let resource = resource_create_child(
        root_resource,
        drv_gpu::APERTURE_BASE,
        drv_gpu::APERTURE_LEN,
        drv_gpu::IRQ_BASE,
        drv_gpu::IRQ_COUNT,
    );
    if resource == u64::MAX {
        let _ = kumo_rt::log_write(
            kumo_abi::sys::log_level::ERR,
            b"drv-gpu: resource creation failed\n",
        );
        return;
    }
    // G5b: a view goes to the GPU driver only where the GPU's power unit exists. QEMU has none,
    // and its drv-gpu exits as soon as DevicePowerUp answers NotSupported, so a view handed to it
    // could die with it.
    let mut power = kumo_abi::DevicePowerStatus::default();
    let has_gpu =
        kumo_rt::device_power_status(Handle(resource as u32), drv_gpu::APERTURE_BASE, &mut power)
            == 0;
    // G3b: the driver reads the vendored firmware from the initrd itself, read-only;
    // `find_entry` needs only the header and entry table, and the payload is copied in 256-byte
    // VmoRead chunks. G5b: x1 is now a bootstrap channel, the initrd grant its first message, so
    // the view can follow on the same channel once drv-fb has made it.
    let firmware = handle_duplicate(initrd, Rights::READ | Rights::TRANSFER);
    let (sender, bootstrap) = channel_create_pair();
    let valid = |raw| raw != 0 && raw != u64::MAX;
    let tag = [drv_gpu::BOOTSTRAP_FIRMWARE];
    if ![firmware, sender, bootstrap].into_iter().all(valid)
        || channel_write_with_handle(
            Handle(sender as u32),
            tag.as_ptr(),
            1,
            Handle(firmware as u32),
        ) != 0
    {
        for raw in [resource, firmware, sender, bootstrap] {
            if valid(raw) {
                let _ = handle_close(Handle(raw as u32));
            }
        }
        let _ = kumo_rt::log_write(
            kumo_abi::sys::log_level::ERR,
            b"drv-gpu: bootstrap setup failed\n",
        );
        return;
    }
    let mut client_channel = 0;
    if has_gpu {
        let (client, server) = channel_create_pair();
        let tag = [drv_gpu::BOOTSTRAP_CLIENT];
        if ![client, server].into_iter().all(valid)
            || channel_write_with_handle(
                Handle(sender as u32),
                tag.as_ptr(),
                1,
                Handle(server as u32),
            ) != 0
        {
            for raw in [client, server, resource, sender, bootstrap] {
                if valid(raw) {
                    let _ = handle_close(Handle(raw as u32));
                }
            }
            log(b"drv-gpu: client bootstrap failed\n");
            return;
        }
        client_channel = client;
    }
    if !run_elf(
        initrd,
        DRV_GPU_PATH.as_bytes(),
        resource,
        bootstrap,
        (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG | ProcessRunFlags::TRANSFER_ARG2)
            .bits(),
        b"drv-gpu",
    ) {
        // The firmware grant sits queued in the channel and goes with it.
        for raw in [resource, sender, bootstrap, client_channel] {
            if valid(raw) {
                let _ = handle_close(Handle(raw as u32));
            }
        }
        let _ = kumo_rt::log_write(kumo_abi::sys::log_level::ERR, b"drv-gpu: launch failed\n");
        return;
    }
    if has_gpu {
        GPU_VIEW_BOOTSTRAP.store(sender as u32, Ordering::Relaxed);
        GPU_CLIENT_CHANNEL.store(client_channel as u32, Ordering::Relaxed);
    } else {
        let _ = handle_close(Handle(sender as u32));
    }
}

fn spawn_xhci(
    initrd: Handle,
    root_resource: Handle,
    config: kumo_xhci::XhciProbeConfig,
    keyboard_writer: u64,
    mouse_writer: u64,
    storage_writer: u64,
) {
    log(b"drv-xhci: topology mmio=");
    log_hex(config.mmio_base);
    log(b" len=");
    log_hex(config.mmio_length);
    log(b" irq=");
    log_hex(config.irq as u64);
    log(b" stream=");
    if config.stream_id == kumo_xhci::XHCI_NO_STREAM_ID {
        log(b"none");
    } else {
        log_hex(config.stream_id as u64);
    }
    log(b"\n");

    // Grant the controller's IRQ alongside its MMIO so the driver can bind it (interrupt-driven
    // HID). usb0 carries a real IRQ from the DTB; usb1/host2 are placeholders (0) and stay polled.
    let irq_count = if config.irq != 0 { 1 } else { 0 };
    let device_resource = resource_create_child(
        root_resource,
        config.mmio_base,
        config.mmio_length,
        config.irq,
        irq_count,
    );
    if device_resource == u64::MAX {
        log(b"drv-xhci: resource fail\n");
        return;
    }
    let (sender, bootstrap) = channel_create_pair();
    if sender == u64::MAX || bootstrap == u64::MAX {
        log(b"drv-xhci: channel fail\n");
        let _ = handle_close(Handle(device_resource as u32));
        if sender != u64::MAX {
            let _ = handle_close(Handle(sender as u32));
        }
        if bootstrap != u64::MAX {
            let _ = handle_close(Handle(bootstrap as u32));
        }
        return;
    }
    let encoded = config.encode();
    let sent = channel_write_with_handle(
        Handle(sender as u32),
        encoded.as_ptr(),
        encoded.len(),
        Handle(device_resource as u32),
    );
    if sent != 0 {
        log(b"drv-xhci: bootstrap send fail\n");
        let _ = handle_close(Handle(device_resource as u32));
        let _ = handle_close(Handle(sender as u32));
        let _ = handle_close(Handle(bootstrap as u32));
        if keyboard_writer != u64::MAX {
            let _ = handle_close(Handle(keyboard_writer as u32));
        }
        if mouse_writer != u64::MAX {
            let _ = handle_close(Handle(mouse_writer as u32));
        }
        if storage_writer != u64::MAX {
            let _ = handle_close(Handle(storage_writer as u32));
        }
        return;
    }
    // Hand the keyboard-channel writer to the driver as a tagged second bootstrap message. The
    // driver writes decoded key bytes here; the reader Sora kept feeds the shell.
    if keyboard_writer != u64::MAX {
        let tag = [XHCI_KEYBOARD_BOOTSTRAP_TAG];
        let _ = channel_write_with_handle(
            Handle(sender as u32),
            tag.as_ptr(),
            1,
            Handle(keyboard_writer as u32),
        );
    }
    if mouse_writer != u64::MAX {
        let tag = [XHCI_MOUSE_BOOTSTRAP_TAG];
        let _ = channel_write_with_handle(
            Handle(sender as u32),
            tag.as_ptr(),
            1,
            Handle(mouse_writer as u32),
        );
    }
    if storage_writer != u64::MAX {
        let tag = [XHCI_STORAGE_BOOTSTRAP_TAG];
        let _ = channel_write_with_handle(
            Handle(sender as u32),
            tag.as_ptr(),
            1,
            Handle(storage_writer as u32),
        );
    }
    if run_elf(
        initrd,
        DRV_XHCI_PATH.as_bytes(),
        0,
        bootstrap,
        ProcessRunFlags::ASYNC.bits(),
        b"drv-xhci",
    ) {
        log(b"drv-xhci: run ok\n");
        let _ = handle_close(Handle(sender as u32));
        let _ = handle_close(Handle(bootstrap as u32));
    } else {
        log(b"drv-xhci: run fail\n");
        let _ = handle_close(Handle(sender as u32));
        let _ = handle_close(Handle(bootstrap as u32));
    }
}

fn log_i2c_hid_kind(kind: HidDeviceKind) {
    match kind {
        HidDeviceKind::Keyboard => log(b"keyboard"),
        HidDeviceKind::Touchpad => log(b"touchpad"),
        HidDeviceKind::Unknown => log(b"unknown"),
    }
}

fn log_i2c_hid_topology(topology: &I2cHidBusTopology) {
    let summary = sora::summarize_i2c_hid_bus(topology);
    log(b"drv-i2c-hid: topology total=");
    log_hex(summary.total as u64);
    log(b" keyboard=");
    log_hex(summary.keyboards as u64);
    log(b" touchpad=");
    log_hex(summary.touchpads as u64);
    log(b" unknown=");
    log_hex(summary.unknown as u64);
    log(b"\n");
    for device in topology.devices() {
        log(b"drv-i2c-hid: child ");
        log_i2c_hid_kind(device.kind);
        log(b" addr=");
        log_hex(device.i2c_address as u64);
        log(b" hid=");
        log_hex(device.hid_descriptor_register as u64);
        log(b" gpio=");
        log_hex(device.interrupt.pin as u64);
        log(b" flags=");
        log_hex(device.interrupt.flags as u64);
        log(b"\n");
    }
}

fn log_i2c_hid_pinctrl_plan(discovery: &sora::I2cHidRuntimeTopology) {
    match discovery.pinctrl_plan {
        Some(plan) => {
            log(b"drv-i2c-hid: pinctrl plan updates=");
            log_hex(plan.updates().len() as u64);
            log(b"\n");
        }
        None => log(b"drv-i2c-hid: pinctrl plan missing\n"),
    }
}

fn log_i2c_hid_failure_policy(policy: drv_i2c_hid::ProbeFailurePolicy) {
    match policy {
        drv_i2c_hid::ProbeFailurePolicy::Required => log(b"required"),
        drv_i2c_hid::ProbeFailurePolicy::Optional => log(b"optional"),
    }
}

fn log_i2c_hid_probe_plan(discovery: &sora::I2cHidRuntimeTopology) {
    match discovery.probe_plan.as_ref() {
        Some(plan) => {
            log(b"drv-i2c-hid: probe plan total=");
            log_hex(plan.devices().len() as u64);
            log(b" required=");
            log_hex(plan.required_count() as u64);
            log(b" optional=");
            log_hex(plan.optional_count() as u64);
            log(b"\n");
            for device in plan.devices() {
                log(b"drv-i2c-hid: probe ");
                log_i2c_hid_kind(device.kind);
                log(b" addr=");
                log_hex(device.config.i2c_address as u64);
                log(b" hid=");
                log_hex(device.config.hid_descriptor_register as u64);
                log(b" irq=");
                log_hex(device.config.attention_irq as u64);
                log(b" policy=");
                log_i2c_hid_failure_policy(device.failure_policy);
                log(b"\n");
            }
        }
        None => log(b"drv-i2c-hid: probe plan missing\n"),
    }
}

/// Locate a file in the initrd by path, returning its `(offset, len)`. Sora
/// holds the initrd as a VMO handle (not a mapped slice), so it walks the entry
/// table with `vmo_read` rather than `kumo_abi::initrd::find_file`.
fn initrd_find(initrd: Handle, want: &[u8]) -> Option<(u64, u64)> {
    let mut hdr = [0u8; 16];
    if vmo_read(initrd, 0, hdr.as_mut_ptr(), 16) != 0 || &hdr[..8] != b"KUMORD01" {
        return None;
    }
    let entry_count = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as u64;
    let mut name = [0u8; 64];
    let mut tail = [0u8; 16]; // offset(8) + len(8)
    for i in 0..entry_count {
        let base = 16 + i * 80;
        if vmo_read(initrd, base, name.as_mut_ptr(), 64) != 0
            || vmo_read(initrd, base + 64, tail.as_mut_ptr(), 16) != 0
        {
            return None;
        }
        let nlen = name.iter().position(|&b| b == 0).unwrap_or(64);
        if &name[..nlen] == want {
            let off = u64::from_le_bytes(tail[..8].try_into().unwrap());
            let len = u64::from_le_bytes(tail[8..16].try_into().unwrap());
            return Some((off, len));
        }
    }
    None
}

/// A [`SectorReader`] serving 512-byte sectors from a region of the initrd VMO
/// (the FAT32 image lives as a file inside the initrd). This is the interim
/// block source until `drv-blk` serves the same image over IPC.
struct InitrdSectors {
    vmo: Handle,
    base: u64,
    len: u64,
}

/// The kernel's `VmoRead` syscall copies at most this many bytes per call
/// (`usermode.rs`), so a 512-byte sector must be read in chunks.
const VMO_READ_MAX: usize = 256;

impl SectorReader for InitrdSectors {
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; kumo_fatfs::SECTOR_SIZE]) -> bool {
        let off = lba as u64 * kumo_fatfs::SECTOR_SIZE as u64;
        if off + kumo_fatfs::SECTOR_SIZE as u64 > self.len {
            return false;
        }
        let mut done = 0usize;
        while done < kumo_fatfs::SECTOR_SIZE {
            let chunk = (kumo_fatfs::SECTOR_SIZE - done).min(VMO_READ_MAX);
            if vmo_read(
                self.vmo,
                self.base + off + done as u64,
                buf[done..].as_mut_ptr(),
                chunk,
            ) != 0
            {
                return false;
            }
            done += chunk;
        }
        true
    }
}

/// A [`SectorReader`] that reads 512-byte sectors **through `drv-blk` over IPC** rather than
/// touching the initrd VMO directly — the `fatfs`-server path (the trait's intended client).
/// `base` is the FS image's byte offset within `drv-blk`'s block space (the whole initrd, so
/// `LBA·512 = initrd offset`). The image is not 512-aligned inside the initrd, so a logical
/// sector generally straddles two `drv-blk` blocks; this splices them. `client` is the serve
/// channel's client end Sora already holds.
struct BlkSectors {
    client: Handle,
    base: u64,
}

impl BlkSectors {
    /// Read one whole `drv-blk` block at absolute `lba` into `out` (write → pump → read →
    /// `read_payload`, mirroring `request_reply`). Returns `false` on any failure.
    fn read_block(&mut self, lba: u64, out: &mut [u8; kumo_fatfs::SECTOR_SIZE]) -> bool {
        let req = drv_blk::Request::read(lba, 1).encode();
        if channel_write(self.client, req.as_ptr(), req.len()) != 0 {
            return false;
        }
        if !child_parked(process_wait()) {
            return false;
        }
        let mut reply = [0u8; 1 + kumo_fatfs::SECTOR_SIZE];
        let n = channel_read(self.client, reply.as_mut_ptr(), reply.len()) as usize;
        match drv_blk::read_payload(&reply[..n]) {
            Ok(data) if data.len() >= kumo_fatfs::SECTOR_SIZE => {
                out.copy_from_slice(&data[..kumo_fatfs::SECTOR_SIZE]);
                true
            }
            _ => false,
        }
    }
}

impl SectorReader for BlkSectors {
    fn read_sector(&mut self, lba: u32, buf: &mut [u8; kumo_fatfs::SECTOR_SIZE]) -> bool {
        const S: u64 = kumo_fatfs::SECTOR_SIZE as u64;
        let byte_off = self.base + lba as u64 * S;
        let first = byte_off / S;
        let skew = (byte_off % S) as usize;
        let mut tmp = [0u8; kumo_fatfs::SECTOR_SIZE];
        if !self.read_block(first, &mut tmp) {
            return false;
        }
        if skew == 0 {
            buf.copy_from_slice(&tmp);
            return true;
        }
        let head = kumo_fatfs::SECTOR_SIZE - skew;
        buf[..head].copy_from_slice(&tmp[skew..]);
        if !self.read_block(first + 1, &mut tmp) {
            return false;
        }
        buf[head..].copy_from_slice(&tmp[..skew]);
        true
    }
}

/// Send a `drv-blk` request frame, pump the driver child to service it, and read the reply.
/// Returns the number of reply bytes (0 on any failure). Mirrors the inline blk round-trip
/// (write → `process_wait` → read), shared by the boot read/write-back proof.
fn blk_request(client: Handle, frame: &[u8], reply: &mut [u8]) -> usize {
    if channel_write(client, frame.as_ptr(), frame.len()) != 0 {
        return 0;
    }
    if !child_parked(process_wait()) {
        return 0;
    }
    channel_read(client, reply.as_mut_ptr(), reply.len()) as usize
}

/// Prove `drv-blk`'s writable private backing end to end: write a recognizable pattern to
/// `lba`, read it back, and confirm the bytes survived — then restore `original` so the block
/// in drv-blk's private copy is left as it was. Returns true only if the round-trip AND the
/// restore succeeded. Non-destructive by construction. — KESTREL
fn blk_write_read_back_proof(client: Handle, lba: u64, original: &[u8]) -> bool {
    let block = drv_blk::BLOCK_SIZE as usize;
    if original.len() != block {
        return false;
    }
    let mut pattern = [0u8; drv_blk::BLOCK_SIZE as usize];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i as u8) ^ 0xA5;
    }
    let mut frame = [0u8; drv_blk::REQUEST_LEN + drv_blk::BLOCK_SIZE as usize];
    let mut reply = [0u8; 1 + drv_blk::BLOCK_SIZE as usize];

    // Write the pattern.
    frame[..drv_blk::REQUEST_LEN].copy_from_slice(&drv_blk::Request::write(lba, 1).encode());
    frame[drv_blk::REQUEST_LEN..].copy_from_slice(&pattern);
    let wrote = blk_request(client, &frame, &mut reply) >= 1 && reply[0] == drv_blk::STATUS_OK;

    // Read it back and compare.
    let read_ok = wrote && {
        let n = blk_request(client, &drv_blk::Request::read(lba, 1).encode(), &mut reply);
        matches!(drv_blk::read_payload(&reply[..n]), Ok(data) if data == &pattern[..])
    };

    // Always restore the original bytes, even if the check failed, so the FAT image is intact.
    frame[drv_blk::REQUEST_LEN..].copy_from_slice(original);
    let restored = blk_request(client, &frame, &mut reply) >= 1 && reply[0] == drv_blk::STATUS_OK;

    read_ok && restored
}

const QEMU_PL011_MMIO_BASE: u64 = 0x0900_0000;
const QEMU_PL011_MMIO_SIZE: u64 = 0x1000;
const QEMU_PL011_IRQ: u32 = 33; // SPI 1 = 33 on QEMU ARM virt
const TIMER_IRQ: u32 = 27; // EL1 physical timer PPI
/// Gate for the legacy Journal-134..137 dev boot-smokes that run *synchronously* during
/// boot: the EL0-fault crash-containment self-test and the svc-health pair/restart demos.
/// Default **off** — they gate the real boot from reaching the serve loop, the crash-test
/// injects a fault that now (correctly) raises a reversed TOWER and reads like a real
/// failure, and the svc-health pair smoke's `process_wait` pump wedges on the X13s with the
/// full resident set live. The TOWER channel (`kernel/src/tower.rs`) now demonstrates fault
/// containment loudly on every *real* fault, so the injected self-test is redundant. Flip to
/// `true` to run the demos when iterating on supervision.
const RUN_LEGACY_SVC_HEALTH_BOOT_SMOKES: bool = false;

/// Bootstrap args (arrive in x0-x7, aarch64 calling convention):
///   x0: root-channel handle
///   x1: framebuffer virtual address (0 if no FB)
///   x2: console channel handle
///   x3: initrd VMO handle
///   x4: block-server channel handle (P7-g)
///   x5: root Resource handle (P9-b)
///   x6: network channel handle (P9-c)
///   x7: keyboard channel handle (P8-a restoration)
#[no_mangle]
extern "C" fn sora_main(
    root_handle: u64,
    fb_va: u64,
    console_handle: u64,
    initrd_vmo: u64,
    block_handle: u64,
    resource_handle: u64,
    net_handle: u64,
    kbd_handle: u64,
) -> ! {
    let root = Handle(root_handle as u32);
    let console = Handle(console_handle as u32);
    let initrd = Handle(initrd_vmo as u32);
    let block = Handle(block_handle as u32);
    let res = Handle(resource_handle as u32);
    ROOT_RESOURCE.store(res.0 as u64, Ordering::Relaxed);
    let net = Handle(net_handle as u32);
    let kbd = Handle(kbd_handle as u32);
    let mut hid_keyboard_input: Option<Handle> = None;
    let mut mouse_input: Option<Handle> = None;
    let mut pointer_state = sora::PointerState::default();

    // J159: BootInfo VMO handle arrives in x8 (beyond the C-ABI first-8). `_start` captured it
    // into `BOOTINFO_VMO_HANDLE` at entry — reading x8 here would race the compiler's reuse of
    // it (see the static's doc-comment).
    let bootinfo_vmo: u64 = BOOTINFO_VMO_HANDLE.load(Ordering::Relaxed);
    let dtb_vmo: u64 = DTB_VMO_HANDLE.load(Ordering::Relaxed);

    // Greeting.
    debug_write(b"hello from Sora via SVC\n".as_ptr(), 24);
    notice(&[b"sora: supervisor online\n"]);

    // Read the kernel's boot message from the root channel.
    let mut buf = [0u8; 64];
    let n = channel_read(root, buf.as_mut_ptr(), 64) as usize;
    if n > 0 {
        debug_write(buf.as_ptr(), n);
    }

    // Acknowledge — Sora is alive and will serve the console channel.
    channel_write(root, b"sora ack\n".as_ptr(), 9);

    // P7-a: acknowledge the initrd VMO handle.
    debug_write(b"initrd vmo h0\n".as_ptr(), 14);

    // P7-b: read the initrd magic (KUMORD01) and echo it.
    let mut magic = [0u8; 8];
    if vmo_read(initrd, 0, magic.as_mut_ptr(), 8) == 0 {
        debug_write(magic.as_ptr(), 8);
        debug_write(b"\n".as_ptr(), 1);
    }

    // P7-f: walk all initrd entries to find a file by name (userspace find_file).
    // Initrd layout: header(16) + entries(N*80) + data.
    let mut header = [0u8; 16];
    if vmo_read(initrd, 0, header.as_mut_ptr(), 16) == 0 {
        let entry_count = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        let target = b"bin/sora";
        let mut entry = [0u8; 80];
        let mut found = false;
        for i in 0..entry_count {
            let offset = 16u64 + (i as u64) * 80;
            if vmo_read(initrd, offset, entry.as_mut_ptr(), 80) != 0 {
                break;
            }
            let path_len = entry[..64].iter().position(|b| *b == 0).unwrap_or(64);
            if path_len == 0 || path_len != target.len() {
                continue;
            }
            if entry[..path_len] != target[..] {
                continue;
            }
            // Found the target file.
            let file_off = u64::from_le_bytes(entry[64..72].try_into().unwrap());
            let file_len = u64::from_le_bytes(entry[72..80].try_into().unwrap());
            debug_write(b"found: ".as_ptr(), 7);
            debug_write(entry.as_ptr(), path_len);
            debug_write(b"\n".as_ptr(), 1);

            // Read and echo the first 4 bytes of the file (ELF magic).
            let mut head = [0u8; 4];
            let n = (file_len as usize).min(4);
            if vmo_read(initrd, file_off, head.as_mut_ptr(), n) == 0 {
                debug_write(head.as_ptr(), n);
                debug_write(b"\n".as_ptr(), 1);
            }
            found = true;
            break;
        }
        if !found {
            debug_write(b"file not found\n".as_ptr(), 15);
        }
    }

    // P7-i: locate the FAT32 disk image, parse the BPB, and walk the root directory.
    // Prove userspace can read a real filesystem: compute the data-region layout from
    // the BPB, read the root-directory sector, and list every 8.3 entry.
    {
        let fat_path = b"bin/fat32.img";
        let mut header = [0u8; 16];
        if vmo_read(initrd, 0, header.as_mut_ptr(), 16) == 0 {
            let entry_count = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
            let mut entry = [0u8; 80];
            let mut fat_off = 0u64;
            for i in 0..entry_count {
                let off = 16u64 + (i as u64) * 80;
                if vmo_read(initrd, off, entry.as_mut_ptr(), 80) != 0 {
                    break;
                }
                let plen = entry[..64].iter().position(|b| *b == 0).unwrap_or(64);
                if plen == fat_path.len() && entry[..plen] == fat_path[..] {
                    fat_off = u64::from_le_bytes(entry[64..72].try_into().unwrap());
                    break;
                }
            }
            if fat_off != 0 {
                let mut bpb = [0u8; 512];
                if vmo_read(initrd, fat_off, bpb.as_mut_ptr(), 512) == 0 {
                    let bps = u16::from_le_bytes(bpb[0x0B..0x0D].try_into().unwrap()) as u64;
                    let spc = bpb[0x0D] as u64;
                    let rsvd = u16::from_le_bytes(bpb[0x0E..0x10].try_into().unwrap()) as u64;
                    let nfat = bpb[0x10] as u64;
                    let spf = u32::from_le_bytes(bpb[0x24..0x28].try_into().unwrap()) as u64;
                    let root = u32::from_le_bytes(bpb[0x2C..0x30].try_into().unwrap()) as u64;
                    let data_start = rsvd + nfat * spf;
                    let root_sec = data_start + (root - 2) * spc;
                    let root_off = fat_off + root_sec * bps;

                    let mut dir = [0u8; 512];
                    if vmo_read(initrd, root_off, dir.as_mut_ptr(), 512) == 0 {
                        let mut pos = 0;
                        while pos + 32 <= 512 && dir[pos] != 0x00 {
                            let attr = dir[pos + 11];
                            if attr != 0x0F && attr != 0x08 {
                                // 8.3 filename: 8-char name + 3-char extension
                                let name = &dir[pos..pos + 8];
                                let ext = &dir[pos + 8..pos + 11];
                                let name_end = name.iter().position(|b| *b == b' ');
                                let ext_end = ext.iter().position(|b| *b == b' ');
                                let nl = name_end.unwrap_or(8);
                                let el = ext_end.unwrap_or(3);
                                if nl > 0 {
                                    debug_write(name.as_ptr(), nl);
                                    if el > 0 {
                                        debug_write(b".".as_ptr(), 1);
                                        debug_write(ext.as_ptr(), el);
                                    }
                                    debug_write(b"\n".as_ptr(), 1);
                                }
                            }
                            pos += 32;
                        }
                    }
                }
            }
        }
    }

    // P8-g/h: demo ProcessCreate + VmarMap — create a child process, map the initrd
    // VMO into it, then print both handles. The child is still an empty shell (no
    // threads, no page tables); ThreadCreate + ThreadStart follow in P8-i.
    // VMAR must cover the address range the child actually uses (shared TTBR0).
    // Sora's code/data/stack live in 0x0..0x2000_0000; use a generous 512 MiB.
    let child_h = process_create(0x0000_0000_0000_0000, 0x0000_0000_2000_0000);
    if child_h != u64::MAX {
        let child = Handle(child_h as u32);
        // Map one page of the initrd VMO at the child's USER_IMAGE_BASE.
        let map_status = vmar_map(
            child,
            initrd,
            0,                     // vmo_offset
            0x0000_0000_1000_0000, // virt (within child VMAR)
            0x1000,                // 4 KiB
            1,                     // READ flag
        );
        debug_write(b"child process h".as_ptr(), 15);
        let mut h = child_h;
        let mut hex = [0u8; 16];
        let mut hi = 16;
        loop {
            hi -= 1;
            let d = (h & 0xF) as u8;
            hex[hi] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
            h >>= 4;
            if h == 0 && hi <= 16 - 4 {
                break;
            }
        }
        debug_write(hex[hi..].as_ptr(), 16 - hi);
        debug_write(b" map=".as_ptr(), 5);
        if map_status == 0 {
            debug_write(b"ok".as_ptr(), 2);
        } else {
            debug_write(b"fail".as_ptr(), 4);
        }
        // P8-i: create a thread in the child and start it (scaffold — kernel thread,
        // no user-mode page tables yet; won't actually run).
        let th_h = thread_create(child);
        if th_h != u64::MAX {
            let th = Handle(th_h as u32);
            let start_ok = thread_start(th, 0x4000_0000, 0x5000_0000, 0);
            debug_write(b" thread=".as_ptr(), 8);
            if start_ok == 0 {
                debug_write(b"ok\n".as_ptr(), 3);
            } else {
                debug_write(b"fail\n".as_ptr(), 5);
            }
        } else {
            debug_write(b" thread=fail\n".as_ptr(), 13);
        }
        // P9-h (retired): a cross-process pipe was demonstrated here by `process_run`ing
        // `child_payload` in Sora's *shared* address space. P10 replaced that fiction with
        // real per-process address spaces, and `ProcessRun` now requires one (built via
        // AddressSpaceCreate) and runs the child as a scheduled thread on its own TTBR0 —
        // so a shared-AS "child" is no longer valid. The P10 child-AS demo below is the
        // successor; cross-process IPC from a real child returns once a child process may
        // issue channel syscalls (today the SVC hook limits non-Sora processes to
        // DebugWrite).
    }

    // P9-a: create an Interrupt object bound to the timer IRQ (27). The handle
    // proves interrupt infrastructure works. InterruptWait is not called here —
    // it would park Sora before the serve loop, breaking channel dispatch.
    // IRQ authority is gated by `res` (the root Resource covers every IRQ line).
    let timer_irq = interrupt_create(res, TIMER_IRQ);
    if timer_irq != u64::MAX {
        debug_write(b"timer irq ok\n".as_ptr(), 13);
    }

    // §5.6 crash containment (Journal 137): prove the kernel contains an EL0 fault from a
    // child process without halting Sora. Gated off by default: it *deliberately* faults a
    // child at BOGUS_ENTRY, which now (correctly) raises a reversed TOWER (kernel/src/tower.rs)
    // and reads like a real microservice failure on every boot. The TOWER channel already
    // proves containment loudly on real faults, so this injected self-test is redundant on the
    // boot path. Run it (with the rest of the legacy demos) by flipping the flag. Must run
    // before persistent services so `process_wait()` sees no resident children and
    // `all_residents_gone` can succeed.
    if RUN_LEGACY_SVC_HEALTH_BOOT_SMOKES {
        if run_crash_containment_smoke(initrd) {
            log(b"svc-health: crash contained\n");
        } else {
            log(b"svc-health: crash escaped\n");
        }
    }

    // Spawn drv-serial only on QEMU's no-framebuffer serial path.
    //
    // drv-serial is the QEMU PL011 input driver: it owns the UART at 0x0900_0000 /
    // IRQ 33 and forwards RX bytes to the console channel. It is not the owner of the kernel's
    // selected early console route. On a framebuffer path, the target may be the X13s with no
    // PL011 or a dual-sink Pi 5 using uart10/RP1; in either case the QEMU base/IRQ pair is wrong.
    // The mint+map still SUCCEED (the root Resource spans all of phys, so
    // resource_create_child/ResourceMintMmio range-checks pass), but drv-serial's first
    // MMIO access — the RXIM unmask write at drv-serial/main.rs — stalls the interconnect
    // on the absent/unclocked peripheral and hard-hangs the core. That is the boot freeze
    // whose last visible line is "drv-serial starting" (printed just before that write).
    // Keep this IRQ-driven driver gated on `fb_va == 0`. On a dual-sink Pi the kernel retains and
    // polls the already-selected PL011 directly, which needs no guessed RP1 interrupt and keeps
    // both TX diagnostics and shell RX alive after POST. — KESTREL 2026-07-18
    if fb_va == 0 {
        let serial_res_h = resource_create_child(
            res,
            QEMU_PL011_MMIO_BASE,
            QEMU_PL011_MMIO_SIZE,
            QEMU_PL011_IRQ,
            1,
        );
        if serial_res_h == u64::MAX {
            log(b"drv-serial: resource fail\n");
        } else if run_elf(
            initrd,
            kumo_abi::DRV_SERIAL_PATH.as_bytes(),
            serial_res_h,
            console_handle,
            1, // async
            b"drv-serial",
        ) {
            log(b"drv-serial: run ok\n");
        } else {
            log(b"drv-serial: run fail\n");
        }
    } else {
        log(b"drv-serial: skipped (QEMU PL011 driver not applicable)\n");
    }

    // J160/J161: spawn drv-blk — the ramdisk block driver (M6/P7).
    // Duplicate the initrd VMO with read-only rights, compute the total
    // initrd size from the file table, and pass both (size + handle) to
    // drv-blk over a bootstrap channel.
    let mut blk_serve: Option<Handle> = None; // serve-channel client end, kept for fatfs-over-blk
    {
        // READ to map the VMO; TRANSFER so channel_write may move the handle to
        // drv-blk (the engine requires Rights::TRANSFER on a transferred handle).
        // Without TRANSFER the write fails AccessDenied and drv-blk sees no VMO (J167).
        let blk_vmo_h = handle_duplicate(initrd, Rights::READ | Rights::TRANSFER);
        if blk_vmo_h != 0 && blk_vmo_h != u64::MAX {
            // Compute the total initrd data size from the header + entry table.
            let mut initrd_size: u64 = 0;
            let mut hdr = [0u8; 16];
            if vmo_read(initrd, 0, hdr.as_mut_ptr(), 16) == 0 {
                let entry_count = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as u64;
                let mut entry = [0u8; 80];
                for i in 0..entry_count {
                    let off = 16u64 + i * 80;
                    if vmo_read(initrd, off, entry.as_mut_ptr(), 80) != 0 {
                        break;
                    }
                    let file_off = u64::from_le_bytes(entry[64..72].try_into().unwrap());
                    let file_len = u64::from_le_bytes(entry[72..80].try_into().unwrap());
                    let end = file_off.saturating_add(file_len);
                    if end > initrd_size {
                        initrd_size = end;
                    }
                }
                // Add header + table overhead. The data region starts after
                // header(16) + table(entry_count * 80).
                initrd_size = initrd_size
                    .saturating_add(16)
                    .saturating_add(entry_count.saturating_mul(80));
            }
            let (server_chan, client_chan) = channel_create_pair();
            if server_chan != u64::MAX && client_chan != u64::MAX {
                // Send the size as 8 LE bytes alongside the VMO handle.
                let size_bytes = initrd_size.to_le_bytes();
                channel_write_with_handle(
                    Handle(server_chan as u32),
                    size_bytes.as_ptr(),
                    size_bytes.len(),
                    Handle(blk_vmo_h as u32),
                );
                if run_elf(
                    initrd,
                    kumo_abi::DRV_BLK_PATH.as_bytes(),
                    0,           // arg (x0) = unused
                    client_chan, // arg2 (x1) = bootstrap channel
                    1,           // async
                    b"drv-blk",
                ) {
                    log(b"drv-blk: run ok\n");
                    // M6/P7 round-trip: drv-blk now serves the initrd VMO as a block
                    // device over IPC. Sora holds the serve channel's client end
                    // (`server_chan`). Pump once so drv-blk consumes its bootstrap
                    // (VMO + size) and parks on the serve channel, then read LBA 0 — the
                    // initrd header — and verify a FULL 512-byte sector survives the
                    // client → drv-blk → client path (the J166 codec end to end; mirrors
                    // the svc-health `request_reply` pump pattern). A whole sector only fits
                    // now that the channel cap is `MAX_INLINE_BYTES`, not 256 (J173 → J174);
                    // checking `data.len() == BLOCK_SIZE` is the proof it is not truncated.
                    let blk_client = Handle(server_chan as u32);
                    blk_serve = Some(blk_client); // reuse for the fatfs-over-drv-blk mount
                    let _ = process_wait(); // let drv-blk (and drv-serial) reach their park
                    let req = drv_blk::Request::read(0, 1).encode();
                    let mut reply = [0u8; 1 + drv_blk::BLOCK_SIZE as usize];
                    let rt_ok = channel_write(blk_client, req.as_ptr(), req.len()) == 0
                        && child_parked(process_wait())
                        && {
                            let n =
                                channel_read(blk_client, reply.as_mut_ptr(), reply.len()) as usize;
                            matches!(
                                drv_blk::read_payload(&reply[..n]),
                                Ok(data)
                                    if data.len() == drv_blk::BLOCK_SIZE as usize
                                        && &data[..8] == b"KUMORD01"
                            )
                        };
                    if rt_ok {
                        log(b"blk-rt: ok\n");
                        // Write-then-read-back proof over the block protocol (PLAN/008 S2):
                        // LBA 0's original bytes are still in `reply[1..]` from the read above.
                        // Prove a write persists in drv-blk's private copy, then restore LBA 0
                        // so later blk-backed initrd reads still see the original KUMORD01 header. — KESTREL
                        let mut original = [0u8; drv_blk::BLOCK_SIZE as usize];
                        original.copy_from_slice(&reply[1..1 + drv_blk::BLOCK_SIZE as usize]);
                        if blk_write_read_back_proof(blk_client, 0, &original) {
                            log(b"blk-wr: ok\n");
                        } else {
                            log(b"blk-wr: fail\n");
                        }
                    } else {
                        log(b"blk-rt: fail\n");
                    }
                } else {
                    log(b"drv-blk: run fail\n");
                }
            }
        }
    }

    // Live proof that kumo-fatfs reads the real bin/fat32.img shipped in the
    // initrd: mount it, resolve HELLO.TXT, read its bytes, and log them. The
    // image is read straight from the initrd VMO (the interim block source until
    // drv-blk serves it over IPC). This exercises the whole kumo-fatfs read
    // engine against reality. Verified by the local UEFI/AAVMF smoke (xtask
    // qemu-smoke boots only the raw Stage-A stub, not the kernel/sora); see
    // JOURNAL/165 for the "fatfs: HELLO.TXT = hello!" serial capture.
    if let Some((base, len)) = initrd_find(initrd, FAT32_IMG_PATH.as_bytes()) {
        let mut disk = InitrdSectors {
            vmo: initrd,
            base,
            len,
        };
        match FatVolume::mount(&mut disk) {
            Ok(vol) => {
                match vol.find_in_root(&mut disk, b"HELLO   TXT") {
                    Some(entry) => {
                        let mut data = [0u8; 16];
                        let n = vol.read_file(&mut disk, &entry, &mut data);
                        log(b"fatfs: HELLO.TXT = ");
                        debug_write(data.as_ptr(), n);
                        log(b"\n");
                    }
                    None => log(b"fatfs: HELLO.TXT missing\n"),
                }
                // J179 consumer: resolve a file in a real SUBDIRECTORY by path
                // (kumo-fatfs find_in_dir + resolve_path, J177/J178). The image
                // now ships an ESP-shaped /EFI/BOOT/BOOTAA64.EFI subtree.
                match vol.resolve_path(&mut disk, b"/EFI/BOOT/BOOTAA64.EFI") {
                    Some(entry) => {
                        let mut data = [0u8; 16];
                        let n = vol.read_file(&mut disk, &entry, &mut data);
                        log(b"fatfs-path: /EFI/BOOT/BOOTAA64.EFI = ");
                        debug_write(data.as_ptr(), n);
                        log(b"\n");
                    }
                    None => log(b"fatfs-path: /EFI/BOOT/BOOTAA64.EFI missing\n"),
                }
            }
            Err(_) => log(b"fatfs: mount failed\n"),
        }
    } else {
        log(b"fatfs: image not found\n");
    }

    // M6/P7 fatfs-server gate: mount the SAME bin/fat32.img and read HELLO.TXT, but drive
    // kumo-fatfs through `drv-blk` over IPC (a `BlkSectors` SectorReader) instead of reading
    // the initrd VMO directly. Same engine, same image, sectors now served by the block
    // driver — the end-to-end storage path. (drv-blk serves the whole initrd by LBA, so the
    // image's byte offset is the SectorReader `base`.)
    if let Some(blk) = blk_serve {
        if let Some((fat_base, _len)) = initrd_find(initrd, FAT32_IMG_PATH.as_bytes()) {
            let mut disk = BlkSectors {
                client: blk,
                base: fat_base,
            };
            match FatVolume::mount(&mut disk) {
                Ok(vol) => match vol.find_in_root(&mut disk, b"HELLO   TXT") {
                    Some(entry) => {
                        let mut data = [0u8; 16];
                        let n = vol.read_file(&mut disk, &entry, &mut data);
                        log(b"fatfs-blk: HELLO.TXT = ");
                        debug_write(data.as_ptr(), n);
                        log(b"\n");
                    }
                    None => log(b"fatfs-blk: HELLO.TXT missing\n"),
                },
                Err(_) => log(b"fatfs-blk: mount failed\n"),
            }
        } else {
            log(b"fatfs-blk: image not found\n");
        }
    }

    launch_gpu_identity(initrd, res);

    // J159: spawn drv-fb when a framebuffer is present.
    // Map the BootInfo VMO (populated by the kernel), read framebuffer geometry,
    // create a narrowed child Resource for the framebuffer MMIO range, and hand
    // fb_res, the console channel, and the bootinfo VMO to drv-fb over a
    // bootstrap channel pair. drv-fb reads these and paints the text console.
    if fb_va != 0 && bootinfo_vmo != 0 {
        let bi_vmo = Handle(bootinfo_vmo as u32);
        let bi_va = 0x0000_0000_3000_0000;
        let bi_map_st = vmar_map(Handle(0), bi_vmo, 0, bi_va, 4096, (VmarFlags::READ).0);
        if bi_map_st == 0 {
            let bootinfo = unsafe { &*(bi_va as *const BootInfo) };
            if bootinfo.has_framebuffer() {
                let fb = bootinfo.framebuffer;
                // Unconditional pre-gate dump of the geometry read from the BootInfo snapshot, so a
                // blank boot reveals whether the snapshot is stale/garbage (a coherency bug upstream)
                // or sane-but-rejected (the is_plausible gate too strict). Fires every boot — unlike
                // the failure-branch dump below, which never prints when the gate itself is the cause.
                log(b"drv-fb: snapshot fb geometry ");
                log_fb_geometry(&fb);
                if fb.is_plausible() {
                    pointer_state = sora::PointerState::new(fb.width, fb.height);
                }
                // Refuse to mint an MMIO Resource over implausibly-shaped geometry: a corrupt
                // snapshot read must fail here as a clear diagnostic, not reach the kernel's
                // range check disguised as a rights problem.
                let fb_res_h = match (fb.is_plausible(), fb.mapping_len()) {
                    (true, Some(mapping_len)) => {
                        // VMAR mappings are page-granular. Grant the final partial page while
                        // keeping `fb.len` as the exact scanout/ownership extent; the opi5's
                        // 1600×900×4 GOP length is not page-aligned. — KESTREL 2026-07-24
                        resource_create_child(res, fb.phys, mapping_len, 0, 0)
                    }
                    _ => u64::MAX,
                };
                if fb_res_h != u64::MAX {
                    let (server_chan, client_chan) = channel_create_pair();
                    if server_chan != u64::MAX && client_chan != u64::MAX {
                        let srv = Handle(server_chan as u32);
                        // Write the three handles drv-fb expects.
                        channel_write_with_handle(srv, b"F".as_ptr(), 1, Handle(fb_res_h as u32));
                        channel_write_with_handle(srv, b"C".as_ptr(), 1, console);
                        channel_write_with_handle(srv, b"B".as_ptr(), 1, bi_vmo);
                        bootstrap_display_probe(srv, res, bootinfo.board_id(), fb.is_plausible());
                        bootstrap_display_topology(
                            srv,
                            Handle(dtb_vmo as u32),
                            bootinfo.platform.dtb,
                            bootinfo.board_id(),
                            fb.is_plausible(),
                        );
                        let graphics_enabled = kumo_graphics::band(fb.width, fb.height).is_some();
                        let graphics = bootstrap_graphics(srv, graphics_enabled);
                        if run_elf(
                            initrd,
                            kumo_abi::DRV_FB_PATH.as_bytes(),
                            0,           // arg (x0) = unused
                            client_chan, // arg2 (x1) = bootstrap channel
                            1,           // async
                            b"drv-fb",
                        ) {
                            log(b"drv-fb: run ok\n");
                            for (view, grant) in graphics.iter().enumerate() {
                                if let Some((channel, vmo)) = grant {
                                    if view == drv_gpu::GPU_VIEW
                                        && hand_view_to_gpu(view, *channel, *vmo)
                                    {
                                        continue;
                                    }
                                    GRAPHICS_CHANNEL[view].store(channel.0, Ordering::Relaxed);
                                    GRAPHICS_VMO[view].store(vmo.0, Ordering::Relaxed);
                                }
                            }
                            if let Some(size) = sora::console_window(fb.width, fb.height) {
                                CONSOLE_WINDOW.store(
                                    (u32::from(size.rows) << 16) | u32::from(size.columns),
                                    Ordering::Relaxed,
                                );
                            }
                        } else {
                            log(b"drv-fb: run fail\n");
                            for grant in &graphics {
                                if let Some((channel, vmo)) = grant {
                                    let _ = handle_close(*channel);
                                    let _ = handle_close(*vmo);
                                }
                            }
                        }
                    } else {
                        log(b"drv-fb: channel fail\n");
                    }
                } else {
                    // Two failure modes, now separated by the plausibility gate above.
                    // Implausible geometry is a corrupt BootInfo snapshot read (the kernel
                    // cleans the snapshot to coherency as of J219; if this still fires the read
                    // is wrong further upstream). Plausible geometry the kernel still rejects is
                    // a real Resource-rights / range-check issue — and the Sora syscall path
                    // collapses the kernel errno to u64::MAX, so surface the geometry either way.
                    if fb.is_plausible() {
                        log(b"drv-fb: resource fail ");
                    } else {
                        log(b"drv-fb: implausible framebuffer geometry ");
                    }
                    log_fb_geometry(&fb);
                }

                // The xHCI keyboard reader feeds the shell like the i2c-hid one; the i2c-hid path
                // below only overrides it on a board that actually has an i2c-hid keyboard.
                let (xhci_kbd, xhci_mouse) = launch_xhci_first_light(
                    initrd,
                    res,
                    Handle(dtb_vmo as u32),
                    bootinfo.platform.dtb,
                );
                hid_keyboard_input = xhci_kbd;
                if xhci_mouse.is_some() {
                    mouse_input = xhci_mouse;
                }

                // Match HID children from the read-only DTB capability before granting hardware
                // authority. QEMU and unrelated framebuffer boards stop here without ever mapping
                // or touching the X13s GENI address. This slice logs all Linux-described children
                // but still launches only the keyboard until shared-controller probing lands.
                let discovery = i2c_hid_runtime_topology_from_dtb(
                    Handle(dtb_vmo as u32),
                    bootinfo.platform.dtb,
                );
                if let Some(discovery) = discovery {
                    log_i2c_hid_topology(&discovery.bus);
                    log_i2c_hid_pinctrl_plan(&discovery);
                    log_i2c_hid_probe_plan(&discovery);
                    let config = sora::select_i2c_hid_keyboard_probe(&discovery);
                    if let Some(config) = config {
                        let device_resource = resource_create_child(
                            res,
                            config.mmio_base,
                            config.mmio_length,
                            kumo_abi::interrupt_authority_key(config.attention_irq),
                            1,
                        );
                        let (sender, bootstrap) = channel_create_pair();
                        if device_resource == u64::MAX {
                            log(b"drv-i2c-hid: resource fail\n");
                        } else if sender == u64::MAX || bootstrap == u64::MAX {
                            log(b"drv-i2c-hid: channel fail\n");
                        } else {
                            let (keyboard_reader, keyboard_writer) = channel_create_pair();
                            let (mouse_reader, mouse_writer) = channel_create_pair();
                            if keyboard_reader == u64::MAX
                                || keyboard_writer == u64::MAX
                                || mouse_reader == u64::MAX
                                || mouse_writer == u64::MAX
                            {
                                if keyboard_reader != u64::MAX {
                                    let _ = handle_close(Handle(keyboard_reader as u32));
                                }
                                if keyboard_writer != u64::MAX {
                                    let _ = handle_close(Handle(keyboard_writer as u32));
                                }
                                if mouse_reader != u64::MAX {
                                    let _ = handle_close(Handle(mouse_reader as u32));
                                }
                                if mouse_writer != u64::MAX {
                                    let _ = handle_close(Handle(mouse_writer as u32));
                                }
                                log(b"drv-i2c-hid: input channel fail\n");
                            } else {
                                let encoded = config.encode();
                                let sent = channel_write_with_handle(
                                    Handle(sender as u32),
                                    encoded.as_ptr(),
                                    encoded.len(),
                                    Handle(device_resource as u32),
                                );
                                let tag = drv_i2c_hid::KEYBOARD_BOOTSTRAP_TAG;
                                let keyboard_sent = if sent == 0 {
                                    channel_write_with_handle(
                                        Handle(sender as u32),
                                        &tag,
                                        1,
                                        Handle(keyboard_writer as u32),
                                    )
                                } else {
                                    Errno::Ok.status()
                                };
                                let mouse_tag = drv_i2c_hid::MOUSE_BOOTSTRAP_TAG;
                                let mouse_sent = if keyboard_sent == 0 {
                                    channel_write_with_handle(
                                        Handle(sender as u32),
                                        &mouse_tag,
                                        1,
                                        Handle(mouse_writer as u32),
                                    )
                                } else {
                                    Errno::Ok.status()
                                };
                                // PLAN_V Slice 8: hand the driver the DT-derived optional
                                // (skippable) bus siblings plus the narrow shared touchpad
                                // attention Resource. The encoded IRQ names intent; this handle is
                                // the actual authority to bind gpio182 later. — KESTREL
                                let probe_candidates =
                                    sora::select_i2c_hid_optional_probe_candidates(&discovery);
                                let probe_attention_irq =
                                    sora::select_i2c_hid_optional_attention_irq(&discovery);
                                let probe_encoded = probe_candidates.encode();
                                let mut probe_resource = 0;
                                let probe_sent = if sent == 0 && mouse_sent == 0 {
                                    if !probe_candidates.candidates().is_empty()
                                        && probe_attention_irq.is_none()
                                    {
                                        Errno::InvalidArgs.status()
                                    } else {
                                        if let Some(attention_irq) = probe_attention_irq {
                                            probe_resource = resource_create_child(
                                                res,
                                                config.mmio_base,
                                                config.mmio_length,
                                                kumo_abi::interrupt_authority_key(attention_irq),
                                                1,
                                            );
                                        }
                                        if probe_resource == u64::MAX {
                                            Errno::BadHandle.status()
                                        } else {
                                            channel_write_with_handle(
                                                Handle(sender as u32),
                                                probe_encoded.as_ptr(),
                                                probe_encoded.len(),
                                                Handle(probe_resource as u32),
                                            )
                                        }
                                    }
                                } else {
                                    Errno::Ok.status()
                                };
                                if sent != 0 {
                                    let _ = handle_close(Handle(keyboard_reader as u32));
                                    let _ = handle_close(Handle(keyboard_writer as u32));
                                    let _ = handle_close(Handle(mouse_reader as u32));
                                    let _ = handle_close(Handle(mouse_writer as u32));
                                    log(b"drv-i2c-hid: bootstrap send fail\n");
                                } else if keyboard_sent != 0 {
                                    let _ = handle_close(Handle(keyboard_reader as u32));
                                    let _ = handle_close(Handle(keyboard_writer as u32));
                                    let _ = handle_close(Handle(mouse_reader as u32));
                                    let _ = handle_close(Handle(mouse_writer as u32));
                                    log(b"drv-i2c-hid: keyboard bootstrap fail\n");
                                } else if mouse_sent != 0 {
                                    let _ = handle_close(Handle(keyboard_reader as u32));
                                    let _ = handle_close(Handle(mouse_reader as u32));
                                    let _ = handle_close(Handle(mouse_writer as u32));
                                    log(b"drv-i2c-hid: mouse bootstrap fail\n");
                                } else if probe_sent != 0 {
                                    if probe_resource != 0 && probe_resource != u64::MAX {
                                        let _ = handle_close(Handle(probe_resource as u32));
                                    }
                                    let _ = handle_close(Handle(keyboard_reader as u32));
                                    let _ = handle_close(Handle(mouse_reader as u32));
                                    log(b"drv-i2c-hid: probe bootstrap fail\n");
                                } else if run_elf(
                                    initrd,
                                    kumo_abi::DRV_I2C_HID_PATH.as_bytes(),
                                    0,
                                    bootstrap,
                                    ProcessRunFlags::ASYNC.bits(),
                                    b"drv-i2c-hid",
                                ) {
                                    hid_keyboard_input = Some(Handle(keyboard_reader as u32));
                                    mouse_input = Some(Handle(mouse_reader as u32));
                                    log(b"drv-i2c-hid: run ok\n");
                                } else {
                                    let _ = handle_close(Handle(keyboard_reader as u32));
                                    let _ = handle_close(Handle(mouse_reader as u32));
                                    log(b"drv-i2c-hid: run fail\n");
                                }
                            }
                        }
                    } else {
                        log(b"drv-i2c-hid: no matched keyboard\n");
                    }
                } else {
                    log(b"drv-i2c-hid: no matched i2c21 hid bus\n");
                }
            }
        } else {
            // Static analysis says this self-map (in-bounds of Sora's root VMAR, READ right
            // present, physical-backed BootInfo VMO, identity-map switch in the kernel
            // VmarMap path) should succeed — yet it fails on X13s. Surface the real Errno so
            // the next boot localises it (NoMemory vs InvalidArgs vs BadHandle) instead of a
            // collapsed "fail".
            log(b"drv-fb: bootinfo map fail st=");
            log_hex(bi_map_st);
            log(b"\n");
        }
    }

    // P9-e: handle transfer test. Create a channel pair, transfer one handle
    // to the kernel via the net channel. The kernel reads it and verifies.
    let ch = channel_create();
    if ch != u64::MAX {
        let h0 = Handle(ch as u32);
        channel_write_with_handle(net, b"h".as_ptr(), 1, h0);
    }

    // P10-b (process model): anonymous VMO + child separate address space.
    // Write a child asm payload to a fresh VMO, map it executable into the child,
    // build page tables, and run with the child's own TTBR0. Proves child code no
    // longer mutates the shared initrd VMO.
    //
    // Payload (8 aarch64 instructions, 32 bytes at offset 0x1100), verified with llvm-mc:
    //   movz x0, #0x1000, lsl #16  → x0 = 0x1000_0000
    //   movk x0, #0x1000           → x0 = 0x1000_1000 (string at child VA+0x1000)
    //   movz x1, #20               → len = 20
    //   movz x8, #29               → DebugWrite
    //   svc  #0
    //   movz x0, #0                → exit code 0
    //   movz x8, #21               → ProcessExit
    //   svc  #0
    // P10-f: verify ChannelWrite + handle transfer across a channel pair.
    // The channel_create_pair asm used `in(reg)` which could alias x8 with
    // the output register, causing ChannelCreate to execute as VmoCreate
    // (handle-is-Vmo). Fixed by switching to explicit `in("x8")`.
    {
        let (a0, a1) = channel_create_pair();
        let (b0, b1) = channel_create_pair();
        if a0 != u64::MAX && a1 != u64::MAX && b0 != u64::MAX && b1 != u64::MAX {
            let st = channel_write(Handle(a0 as u32), b"T".as_ptr(), 1);
            let st2 =
                channel_write_with_handle(Handle(a0 as u32), b"x".as_ptr(), 1, Handle(b1 as u32));
            if st == 0 && st2 == 0 {
                log(b"P10-f: ch-write+transfer ok\n");
            } else {
                log(b"P10-f: ch-write fail\n");
            }
        }
    }

    // String at VMO offset 0x1000: "hello from child ch\n" (20 bytes).
    // String at VMO offset 0x1020: "child xfer ok!\n" (16 bytes).
    const RUN_BLOCKING_CHILD_XFER_DEMO: bool = false;
    if RUN_BLOCKING_CHILD_XFER_DEMO {
        // P10-f: channel-based cross-process handle transfer. Sora writes to
        // ch0 WITH xfer1 as a transferred handle BEFORE process_run, so the
        // message waits in ch1's inbox. The child's first act is ChannelRead
        // on ch1, which installs xfer1 in its own handle table. The child
        // then writes to xfer1 and exits. Sora reads from xfer0 to verify.
        //
        // Payload (22 aarch64 instructions, 88 bytes at VMO offset 0x1100),
        // verified with llvm-mc -triple=aarch64 --show-encoding:
        let code: [u32; 22] = [
            0xaa0003f3, // mov x19, x0              (save bootstrap handle)
            // ChannelRead(x19, buf=0x1000FE00, cap=32) — buf on RW stack
            0xaa1303e0, // mov x0, x19              (channel = bootstrap)
            0xd2a20001, // movz x1, #0x1000, lsl #16
            0xf29fc001, // movk x1, #0xFE00          (buf = 0x1000FE00)
            0xd2800402, // movz x2, #32              (cap = 32)
            0xd28000a8, // movz x8, #5               (ChannelRead)
            0xd4000001, // svc #0
            0xaa0103f4, // mov x20, x1               (save transferred handle)
            // ChannelWrite(x20, string_xfer, 16)
            0xaa1403e0, // mov x0, x20              (channel = received)
            0xd2a20001, // movz x1, #0x1000, lsl #16
            0xf2820401, // movk x1, #0x1020          (ptr = 0x10001020)
            0xd2800202, // movz x2, #16              (len = 16)
            0xd2800088, // movz x8, #4               (ChannelWrite)
            0xd4000001, // svc #0
            // DebugWrite(string_hello, 20)
            0xd2a20000, // movz x0, #0x1000, lsl #16
            0xf2820000, // movk x0, #0x1000          (ptr = 0x10001000)
            0xd2800281, // movz x1, #20              (len = 20)
            0xd28003a8, // movz x8, #29              (DebugWrite)
            0xd4000001, // svc #0
            // ProcessExit(0)
            0xd2800000, // movz x0, #0
            0xd28002a8, // movz x8, #21              (ProcessExit)
            0xd4000001, // svc #0
        ];
        let mut code_bytes = [0u8; 88];
        for (i, w) in code.iter().enumerate() {
            code_bytes[i * 4..(i + 1) * 4].copy_from_slice(&w.to_le_bytes());
        }
        let child_vmo_h = vmo_create(0x2000);
        if child_vmo_h == u64::MAX {
            log(b" child as=nop (vmo create fail)\n");
        } else {
            let child_vmo = Handle(child_vmo_h as u32);
            let s1 = vmo_write(child_vmo, 0x1000, b"hello from child ch\n".as_ptr(), 20) == 0;
            let s2 = vmo_write(child_vmo, 0x1020, b"child xfer ok!\n".as_ptr(), 16) == 0;
            let c_ok = vmo_write(child_vmo, 0x1100, code_bytes.as_ptr(), 88) == 0;
            if !s1 || !s2 {
                log(b" child as=nop (string write fail)\n");
            } else if !c_ok {
                log(b" child as=nop (code write fail)\n");
            } else {
                let child_as_h = process_create(0x0000_0000_0000_0000, 0x0000_0000_2000_0000);
                if child_as_h != u64::MAX {
                    let child_as = Handle(child_as_h as u32);
                    // Map the 3 pages holding strings (0x1000, 0x1020) and code
                    // (0x1100) at child VA 0x10000000 as RX (READ|EXECUTE = 5).
                    if vmar_map(child_as, child_vmo, 0, 0x10000000, 0x2000, 5) != 0 {
                        log(b" child as=nop (map fail)\n");
                    } else if address_space_create(child_as, 0x10010000, 0x4000) == u64::MAX {
                        log(b" child as=nop (addr space fail)\n");
                    } else {
                        // P10-f: channel-based handle transfer. Create two pairs.
                        // Sora writes to ch0 with xfer1 as transfer BEFORE
                        // process_run; the child reads from ch1 (passed as the
                        // bootstrap arg) and receives xfer1 via install_transfers.
                        let (ch0, ch1) = channel_create_pair();
                        let (xfer0, xfer1) = channel_create_pair();
                        let run_as = if ch0 != u64::MAX
                            && ch1 != u64::MAX
                            && xfer0 != u64::MAX
                            && xfer1 != u64::MAX
                        {
                            // Queue the handle transfer before the child starts.
                            let xfer_st = channel_write_with_handle(
                                Handle(ch0 as u32),
                                b"x".as_ptr(),
                                1,
                                Handle(xfer1 as u32),
                            );
                            let ok = process_run(child_as, 0x10001100, 0x1000FFF0, ch1, 0, 0);
                            // Read from xfer0: the child wrote to xfer1.
                            let mut xfer_buf = [0u8; 32];
                            let xn = channel_read(Handle(xfer0 as u32), xfer_buf.as_mut_ptr(), 32)
                                as usize;
                            if xn > 0 {
                                log(b"child xfer: ");
                                debug_write(xfer_buf.as_ptr(), xn);
                            } else {
                                log(b"child xfer: (none) st=");
                                let mut hx = [0u8; 4];
                                let mut v = xfer_st as u64;
                                let mut i = 4;
                                loop {
                                    i -= 1;
                                    let d = (v & 0xF) as u8;
                                    hx[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
                                    v >>= 4;
                                    if v == 0 && i <= 1 {
                                        break;
                                    }
                                }
                                debug_write(hx[i..].as_ptr(), 4 - i);
                                debug_write(b"\n".as_ptr(), 1);
                            }
                            ok
                        } else {
                            u64::MAX
                        };
                        log(b" child as run=");
                        if run_as == 0 {
                            log(b"ok\n");
                        } else {
                            log(b"fail\n");
                        }
                    }
                } else {
                    log(b" child as=nop (process create fail)\n");
                }
            }
        }
    } // P10-g: async child demo — non-blocking ProcessRun (flags=1).
      // Sora spawns the child, writes "go" to the bootstrap channel,
      // calls process_wait (child preempts, reads "go", writes "world!\n",
      // DebugWrites, exits). Sora resumes, reads "world!\n" from c0.
    const RUN_ASYNC_CHILD_DEMO: bool = false;
    if RUN_ASYNC_CHILD_DEMO {
        let child_vmo_h = vmo_create(0x2000);
        if child_vmo_h != u64::MAX {
            let child_vmo = Handle(child_vmo_h as u32);
            // Code: save handle + ChannelRead + ChannelWrite + DebugWrite + ProcessExit
            // String at 0x1000: "async child ok\n" (15 bytes)
            // String at 0x1020: "world!\n" (6 bytes)
            let code: [u32; 21] = [
                0xaa0003f3, // mov x19, x0              (save bootstrap handle)
                // ChannelRead(x19, buf=0x1000FE00, cap=32)
                0xaa1303e0, // mov x0, x19
                0xd2a20001, // movz x1, #0x1000, lsl #16
                0xf29fc001, // movk x1, #0xFE00
                0xd2800402, // movz x2, #32
                0xd28000a8, // movz x8, #5               (ChannelRead)
                0xd4000001, // svc #0
                // ChannelWrite(x19, reply, 6)
                0xaa1303e0, // mov x0, x19
                0xd2a20001, // movz x1, #0x1000, lsl #16
                0xf2820401, // movk x1, #0x1020
                0xd28000c2, // movz x2, #6
                0xd2800088, // movz x8, #4               (ChannelWrite)
                0xd4000001, // svc #0
                // DebugWrite(string, 15)
                0xd2a20000, // movz x0, #0x1000, lsl #16
                0xf2820000, // movk x0, #0x1000
                0xd28001e1, // movz x1, #15
                0xd28003a8, // movz x8, #29              (DebugWrite)
                0xd4000001, // svc #0
                // ProcessExit(0)
                0xd2800000, // movz x0, #0
                0xd28002a8, // movz x8, #21              (ProcessExit)
                0xd4000001, // svc #0
            ];
            let mut cb = [0u8; 84];
            for (i, w) in code.iter().enumerate() {
                cb[i * 4..(i + 1) * 4].copy_from_slice(&w.to_le_bytes());
            }
            let s1 = vmo_write(child_vmo, 0x1000, b"async child ok\n".as_ptr(), 15) == 0;
            let s2 = vmo_write(child_vmo, 0x1020, b"world!\n".as_ptr(), 6) == 0;
            let c_ok = vmo_write(child_vmo, 0x1100, cb.as_ptr(), 84) == 0;
            if s1 && s2 && c_ok {
                let child_as_h = process_create(0, 0x2000_0000);
                if child_as_h != u64::MAX {
                    let child_as = Handle(child_as_h as u32);
                    if vmar_map(child_as, child_vmo, 0, 0x10000000, 0x2000, 5) == 0
                        && address_space_create(child_as, 0x10010000, 0x4000) != u64::MAX
                    {
                        let (c0, c1) = channel_create_pair();
                        if c0 != u64::MAX && c1 != u64::MAX {
                            // flags=1 → async
                            let st = process_run(child_as, 0x10001100, 0x1000FFF0, c1, 0, 1);
                            if st == 0 {
                                channel_write(Handle(c0 as u32), b"go".as_ptr(), 2);
                                process_wait();
                                let mut rbuf = [0u8; 16];
                                let rn =
                                    channel_read(Handle(c0 as u32), rbuf.as_mut_ptr(), 16) as usize;
                                if rn > 0 {
                                    log(b"async reply: ");
                                    debug_write(rbuf.as_ptr(), rn);
                                }
                                log(b"async run=ok\n");
                            } else {
                                log(b"async run=fail\n");
                            }
                        }
                    }
                }
            }
        }
    }

    // M10-a: first `persona-linux` smoke. This is not a native KUMO child: the
    // payload uses ARM64 Linux syscall numbers (`write` = 64, `exit_group` = 94).
    // The temporary Stage-A bridge in the kernel translates those numbers only for
    // non-Sora children until the userspace persona runner owns the trap path.
    {
        let _ = linux_arm64::WRITE;
        let _ = linux_arm64::EXIT_GROUP;
        let linux_msg = b"M10 linux hello\n";
        let child_vmo_h = vmo_create(0x2000);
        if child_vmo_h == u64::MAX {
            log(b"M10 linux: vmo fail\n");
        } else if linux_msg.len() != 16 {
            log(b"M10 linux: len fail\n");
        } else {
            let child_vmo = Handle(child_vmo_h as u32);
            // Payload (9 aarch64 instructions, 36 bytes at VMO offset 0x1100),
            // verified with `llvm-mc -triple=aarch64 --show-encoding`:
            //   movz x0, #1                → fd = stdout
            //   movz x1, #0x1000, lsl #16 → x1 = 0x1000_0000
            //   movk x1, #0x1000          → buf = 0x1000_1000
            //   movz x2, #16              → len
            //   movz x8, #64              → Linux arm64 write
            //   svc  #0
            //   movz x0, #0               → status
            //   movz x8, #94              → Linux arm64 exit_group
            //   svc  #0
            let code: [u32; 9] = [
                0xd2800020, // movz x0, #1
                0xd2a20001, // movz x1, #0x1000, lsl #16
                0xf2820001, // movk x1, #0x1000
                0xd2800202, // movz x2, #16
                0xd2800808, // movz x8, #64
                0xd4000001, // svc #0
                0xd2800000, // movz x0, #0
                0xd2800bc8, // movz x8, #94
                0xd4000001, // svc #0
            ];
            let mut cb = [0u8; 36];
            for (i, w) in code.iter().enumerate() {
                cb[i * 4..(i + 1) * 4].copy_from_slice(&w.to_le_bytes());
            }
            let msg_ok = vmo_write(child_vmo, 0x1000, linux_msg.as_ptr(), linux_msg.len()) == 0;
            let code_ok = vmo_write(child_vmo, 0x1100, cb.as_ptr(), cb.len()) == 0;
            if !msg_ok || !code_ok {
                log(b"M10 linux: write fail\n");
            } else {
                let child_as_h = process_create(0, 0x2000_0000);
                if child_as_h == u64::MAX {
                    log(b"M10 linux: process fail\n");
                } else {
                    let child_as = Handle(child_as_h as u32);
                    if vmar_map(child_as, child_vmo, 0, 0x10000000, 0x2000, 5) != 0 {
                        log(b"M10 linux: map fail\n");
                    } else if address_space_create(child_as, 0x10010000, 0x4000) == u64::MAX {
                        log(b"M10 linux: as fail\n");
                    } else {
                        let st = process_run(child_as, 0x10001100, 0x1000FFF0, 0, 0, 0);
                        if st == 0 {
                            log(b"M10 linux: run ok\n");
                        } else {
                            log(b"M10 linux: run fail\n");
                        }
                    }
                }
            }
        }
    }

    // M10-b: load the same hello path as an initrd-resident ARM64 Linux ELF.
    // This is the first step from handmade payload toward "one static ELF".
    if run_elf(
        initrd,
        PERSONA_LINUX_HELLO_PATH.as_bytes(),
        0,
        0,
        0,
        b"M10 elf",
    ) {
        debug_write(b"M10-c Check ok\n".as_ptr(), 15);
    } else {
        log(b"M10 elf: run fail\n");
    }

    // Narrow the initrd handle before handing it to user programs. `ls` only needs READ
    // authority; Sora keeps the full bootstrap handle for its own loader duties.
    let prog_initrd = handle_duplicate(initrd, Rights::READ | Rights::DUPLICATE);
    if prog_initrd == 0 || prog_initrd == u64::MAX {
        log(b"initrd read-only dup fail\n");
    }

    // Create the reusable argv scratch VMO (see the static). Each launch packs the program's
    // argv into it and mints a fresh one-shot read+transfer alias for the bootstrap startup
    // message; failure here just means programs run with no argv (degrade, don't abort the boot).
    let argv_vmo = vmo_create(256);
    if argv_vmo != u64::MAX {
        ARGV_VMO.store(argv_vmo, Ordering::Relaxed);
    } else {
        log(b"argv vmo create fail\n");
    }

    // Live Sora-side HandleClose proof: close a temporary read-only duplicate, then
    // HandleKoid must reject that process-local number. The original initrd handle is
    // untouched and remains the source for program loading.
    let close_probe = handle_duplicate(initrd, Rights::READ);
    if close_probe != 0
        && close_probe != u64::MAX
        && handle_close(Handle(close_probe as u32)) == Errno::Ok.status()
        && handle_koid(Handle(close_probe as u32)) == u64::MAX
    {
        log(b"handle close: sora ok\n");
    } else {
        log(b"handle close: sora fail\n");
    }

    let mut cwd = WorkingDirectory::default();

    // Autoexec (input-less): run the boot script shipped at `etc/autoexec` in the initrd —
    // one shell command per line, `#` comments and blanks skipped (`kumoza::autoexec_lines`).
    // Each line is parsed and dispatched through the SAME `eval_command` as interactive input,
    // so `run <prog>` launches a program (the J181 synchronous path) and `echo …` prints — the
    // manifest speaks the shell's own syntax. This is the X13s shell-vertical stopgap: commands
    // you add to the manifest run at boot and paint to the framebuffer with NO keyboard. Manifest
    // is small; a fixed buffer avoids the heap.
    let mut manifest = [0u8; 256];
    match find_initrd_file(initrd, AUTOEXEC_PATH.as_bytes()) {
        Some((off, len)) => {
            let n = (len as usize).min(manifest.len());
            if vmo_read(initrd, off, manifest.as_mut_ptr(), n) == 0 {
                for line in kumoza::autoexec_lines(&manifest[..n]) {
                    if let Ok(ls) = core::str::from_utf8(line) {
                        if let Some(stmt) = parse(ls) {
                            run_statement(&stmt, line, initrd, prog_initrd, root, &mut cwd);
                        }
                    }
                }
                log(b"autoexec: done\n");
            } else {
                log(b"autoexec: read fail\n");
            }
        }
        None => log(b"autoexec: no manifest\n"),
    }

    // Run the fixed Piccolo evaluator before the console-backlog flush. The serial-only QEMU
    // vertical can park in that legacy flush on an empty console channel, while the evaluator's
    // finite stdout channel is independently pumpable and must remain an observable boot gate.
    // — KESTREL
    launch_lua_repl(initrd);
    log(b"sora: lua returned\n");

    // Flush the kernel console backlog accumulated during autoexec into drv-fb, so boot
    // progress is visible immediately rather than trickling through the serve loop where it
    // must compete with keyboard and other port sources.
    //
    // Hardened over MERLIN's J402 first cut. On the claimed-framebuffer path `debug_write`
    // routes through `queue_framebuffer_console`, which re-enqueues into the *same*
    // kernel->Sora console inbox this code reads from (kernel end = Left; Sora's `console`
    // handle = Right). So a plain read->debug_write loop feeds itself: it only terminates once
    // drv-fb (prio 63) preempts Sora (prio 64) to drain the inbox — an unbounded spin, with no
    // yield, that hangs boot if drv-fb ever stalls, and doubles lines when it re-reads its own
    // re-queue. Instead: drain the inbox into a fixed staging buffer FIRST (no debug_write, so
    // nothing is re-queued and no line is doubled), then emit the staged bytes once in
    // DebugWrite-sized (<=256B) chunks. The fixed buffer bounds the work absolutely: a backlog
    // larger than it — or a stalled drv-fb — costs a truncated or late flush that the serve
    // loop still drains one message per wake, never a boot hang. — CORVUS
    //
    // J510: gated on a claimed framebuffer, because "on the serial path the inbox is empty,
    // so this is a clean no-op" was not true. An empty inbox is not a zero-length read: for
    // Sora, `channel_read` parks the user thread once and waits for `wake_user`, and on a
    // framebuffer-less serial console nothing ever calls it — `klog!` goes straight to the
    // HAL rather than through `deliver_to_sora`. Boot stalled here, before the serve loop,
    // so the shell was unreachable on exactly the configuration QEMU presents. There is no
    // backlog to flush without drv-fb, so the honest fix is not to ask. — TIMBERDOODLE
    if fb_va != 0 {
        const CONSOLE_FLUSH_STAGE_BYTES: usize = 1024;
        const CONSOLE_CHUNK_BYTES: usize = 256;
        let mut flush_stage = [0u8; CONSOLE_FLUSH_STAGE_BYTES];
        let mut staged = 0usize;
        while staged + CONSOLE_CHUNK_BYTES <= flush_stage.len() {
            let n = channel_read(
                console,
                flush_stage[staged..].as_mut_ptr(),
                CONSOLE_CHUNK_BYTES,
            ) as usize;
            if n == 0 || n == usize::MAX {
                break;
            }
            staged += n;
        }
        let mut emitted = 0usize;
        while emitted < staged {
            let end = (emitted + CONSOLE_CHUNK_BYTES).min(staged);
            debug_write(flush_stage[emitted..end].as_ptr(), end - emitted);
            emitted = end;
        }
    }

    if run_ttyd_smoke(initrd) {
        log(b"ttyd: serve ok\n");
    } else {
        log(b"ttyd: serve fail\n");
    }

    if run_persona_posix_tty_write_smoke(initrd) {
        log(b"persona-posix: tty write ok\n");
    } else {
        log(b"persona-posix: tty write fail\n");
    }

    if run_persona_posix_tty_read_smoke(initrd) {
        log(b"persona-posix: tty read ok\n");
    } else {
        log(b"persona-posix: tty read fail\n");
    }

    if RUN_LEGACY_SVC_HEALTH_BOOT_SMOKES {
        // Stage-C: spawn TWO real servers as independent resident processes, each with its own
        // request channel and port. Proves the kernel's per-thread wait queue routes each
        // client's traffic to exactly the server that owns its port — no cross-wake (Journal 134).
        if run_svc_health_pair_smoke(initrd) {
            log(b"svc-health: serve ok\n");
        } else {
            log(b"svc-health: serve fail\n");
        }

        // §5.6 supervised restart: a server is shut down and reaped, then Sora rebuilds it from
        // its recipe; the fresh instance serves with reset state — the first restart (Journal 136).
        if run_svc_health_restart_smoke(initrd) {
            log(b"svc-health: restart ok\n");
        } else {
            log(b"svc-health: restart fail\n");
        }
    } else {
        log(b"svc-health: legacy pair/restart smokes skipped\n");
    }

    // P10-b: VmoWrite/VmoRead demo on anonymous backing.
    let scratch_h = vmo_create(0x1000);
    if scratch_h != u64::MAX {
        let scratch = Handle(scratch_h as u32);
        let mut vbuf = [0u8; 8];
        if vmo_write(scratch, 8, b"VMO_OK\n".as_ptr(), 6) == 0
            && vmo_read(scratch, 8, vbuf.as_mut_ptr(), 6) == 0
            && &vbuf[..6] == b"VMO_OK"
        {
            log(b"anon vmo write ok\n");
        }
    }

    // Port/wait-many serve loop. A single PortWait parks once and wakes when
    // either console or block has data. Source koid matching dispatches to
    // the right handler. This replaces the per-channel-park loops and lifts
    // the 2-channel limit.
    release_gpu_bootstrap();
    launch_gpu_client(initrd);
    log(b"sora: serve setup\n");
    let port_h = port_create();
    let console_koid = handle_koid(console);
    let block_koid = handle_koid(block);
    let net_koid = handle_koid(net);
    let kbd_koid = handle_koid(kbd);
    let hid_kbd_koid = hid_keyboard_input.map(handle_koid).unwrap_or(u64::MAX);
    let mouse_koid = mouse_input.map(handle_koid).unwrap_or(u64::MAX);
    // Breadcrumb each unavailable handle so a metal boot that wedges here (the X13s "halts
    // right after `anon vmo write ok`" report) names exactly what is missing on the next flash.
    if port_h == u64::MAX {
        log(b"sora: port create fail\n");
    }
    if console_koid == u64::MAX {
        log(b"sora: console koid fail\n");
    }
    if block_koid == u64::MAX {
        log(b"sora: block koid fail\n");
    }
    if net_koid == u64::MAX {
        log(b"sora: net koid fail\n");
    }
    if kbd_koid == u64::MAX {
        log(b"sora: kbd koid fail\n");
    }
    if hid_keyboard_input.is_some() && hid_kbd_koid == u64::MAX {
        log(b"sora: hid kbd koid fail\n");
    }
    if mouse_input.is_some() && mouse_koid == u64::MAX {
        log(b"sora: mouse koid fail\n");
    }
    // The serve loop needs only the PORT; every source (console, block, net, keyboard, mouse) is
    // bound only when its koid is live. On the X13s framebuffer path the console handle has
    // already been moved to drv-fb, so `console_koid` legitimately fails — Sora must still enter
    // the loop to serve the keyboard/mouse rather than drop into the idle busy-spin. This is the
    // J407 repair, restored after the J405 reset re-required the console and re-broke X13s input
    // (the "serve setup / console koid fail / no port; idle" symptom). Each per-source handler
    // below gates on a koid match, so an unbound channel simply never wakes the port.
    if port_h != u64::MAX {
        let port = Handle(port_h as u32);
        if console_koid != u64::MAX {
            port_bind(port, console);
        }
        if block_koid != u64::MAX {
            port_bind(port, block);
        }
        if net_koid != u64::MAX {
            port_bind(port, net);
        }
        if kbd_koid != u64::MAX {
            port_bind(port, kbd);
        }
        if let Some(hid_kbd) = hid_keyboard_input {
            if hid_kbd_koid != u64::MAX {
                port_bind(port, hid_kbd);
            }
        }
        if let Some(mouse) = mouse_input {
            if mouse_koid != u64::MAX {
                port_bind(port, mouse);
            }
        }
        SERVE_PORT.store(u64::from(port.0), Ordering::Relaxed);
        log(b"sora: ports bound\n");
        // J545: each controller's storage control channel. `PortBind` replays a pending offer,
        // so a stick probed before this point still wakes the loop below.
        let mut storage_koids: [Option<Handle>; 3] = [None; 3];
        for (koid, control) in storage_koids.iter_mut().zip(&XHCI_STORAGE_CONTROLS) {
            let control = Handle(control.load(Ordering::Relaxed));
            let raw_koid = handle_koid(control);
            if control.0 != 0 && raw_koid != u64::MAX && port_bind(port, control) == 0 {
                *koid = Some(Handle(raw_koid as u32));
            }
        }
        let cons_koid = Handle(console_koid as u32);
        let blk_koid = Handle(block_koid as u32);
        let net_k = Handle(net_koid as u32);
        let kbd_k = Handle(kbd_koid as u32);
        let hid_kbd_k = Handle(hid_kbd_koid as u32);
        let mouse_k = Handle(mouse_koid as u32);
        let mut serve_buf = [0u8; 256];
        let mut block_buf = [0u8; 512];
        let mut mouse_events_seen: u32 = 0;
        let mut malformed_mouse_events_seen: u32 = 0;

        if let Some(client) = spawn_identityd(initrd) {
            IDENTITYD.store(client.0, Ordering::Relaxed);
        }
        read_hostname(initrd);

        let ttyd_recipe = sora::ServerRecipe {
            name: "ttyd",
            image_path: TTYD_PATH,
            restart: sora::RestartPolicy::OnFailure,
        };
        let mut interactive_ttyd = spawn_ttyd_session(initrd, ttyd_recipe);
        let mut interactive_ttyd_koid = interactive_ttyd.and_then(|service| {
            arm_supervised_process(port, service).inspect(|_| {
                log(b"ttyd: supervision armed\n");
            })
        });
        if interactive_ttyd_koid.is_none() {
            log(b"ttyd: supervision arm fail\n");
        }

        // Bound how many times the interactive service may be respawned before Sora gives
        // up on it (DESIGN/002 §5 give-up ladder floor): a crash-looping instance must not
        // be restarted forever.
        const TTYD_MAX_RESTARTS: u32 = 3;
        let mut ttyd_restart_budget = sora::RestartBudget::new(TTYD_MAX_RESTARTS);
        const TTYD_INITIAL_BACKOFF_NS: u64 = 50_000_000;
        const TTYD_MAX_BACKOFF_NS: u64 = 200_000_000;
        let mut ttyd_restart_backoff =
            sora::RestartBackoff::new(TTYD_INITIAL_BACKOFF_NS, TTYD_MAX_BACKOFF_NS);
        let mut pending_ttyd_restart: Option<(sora::ServerRecipe<'static>, Handle)> = None;
        let mut ttyd_restart_timer: Option<(Handle, Handle)> = None;

        log(b"sora: entering serve loop\n");
        SERVING.store(true, Ordering::Relaxed);
        // First light of the interactive shell: render the prompt once so the settled framebuffer
        // console invites input instead of sitting silent after boot. Gated on a live ttyd session —
        // without one there is no line editor to receive the command the prompt would solicit.
        // It waits for boot to settle (J559): the stick mounting, a key, or `BOOT_SETTLE_NS`.
        let mut first_prompt = None;
        if interactive_ttyd.is_some() {
            first_prompt = arm_first_prompt(port);
            if first_prompt.is_none() {
                diag(b"sora: no settle timer; prompt now\n");
                write_shell_prompt(&cwd);
            }
        }
        // `PortBind` replays an endpoint's current READABLE state, so keys queued before setup
        // arrive through the ordinary loop below. Do not probe the channel with blocking
        // `channel_read`: an empty HID channel would park Sora inside the syscall's SoraState
        // borrow, leaving the boot floor alive but unable to forward any ordinary console input.
        loop {
            let source = Handle(port_wait(port) as u32);
            if let Some(index) = storage_koids.iter().position(|koid| *koid == Some(source)) {
                if media_offered(initrd, port, index, &mut storage_koids) {
                    settle_first_prompt(&mut first_prompt, port, &cwd, b"media");
                }
                continue;
            }
            if first_prompt.map(|(_, koid)| koid) == Some(source) {
                settle_first_prompt(&mut first_prompt, port, &cwd, b"timeout");
                continue;
            }
            if login_pause_koid() == Some(source) {
                end_login_pause();
                continue;
            }
            if ttyd_restart_timer.map(|(_, koid)| koid) == Some(source) {
                let Some((timer, _)) = ttyd_restart_timer.take() else {
                    continue;
                };
                if port_unbind(port, timer) != 0 {
                    log(b"ttyd: restart timer unbind fail\n");
                }
                if handle_close(timer) != 0 {
                    log(b"ttyd: restart timer cleanup fail\n");
                }
                let Some((recipe, dead_source)) = pending_ttyd_restart.take() else {
                    log(b"ttyd: restart timer orphaned\n");
                    continue;
                };
                log(b"ttyd: restart required\n");
                if let Some(replacement) = spawn_ttyd_session(initrd, recipe) {
                    let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
                    let serves = ttyd_request_reply(
                        replacement.instance.client,
                        TtyRequest::clear(),
                        &mut reply,
                    )
                    .and_then(|n| TtyReply::parse(&reply[..n]))
                    .map(|reply| reply.status == ttyd::TTY_OK)
                    .unwrap_or(false);
                    if serves {
                        if let Some(koid) = arm_supervised_process(port, replacement) {
                            if koid != dead_source {
                                interactive_ttyd = Some(replacement);
                                interactive_ttyd_koid = Some(koid);
                                log(b"ttyd: restart ok\n");
                                // A fresh ttyd echoes: never carry on into a password prompt.
                                if session().is_none() {
                                    let mut login = LOGIN.get();
                                    login.restart();
                                    LOGIN.set(login);
                                    log(b"\n");
                                    write_login_prompt();
                                }
                            } else {
                                let _ = close_supervised_service(replacement);
                                log(b"ttyd: restart identity fail\n");
                            }
                        } else {
                            let _ = close_supervised_service(replacement);
                            log(b"ttyd: restart rearm fail\n");
                        }
                    } else {
                        let _ = close_supervised_service(replacement);
                        log(b"ttyd: restart serve fail\n");
                    }
                } else {
                    log(b"ttyd: restart spawn fail\n");
                }
                continue;
            }
            if Some(source) == interactive_ttyd_koid {
                let Some(dead) = interactive_ttyd.take() else {
                    interactive_ttyd_koid = None;
                    continue;
                };
                interactive_ttyd_koid = None;
                let recipe = dead.recipe;
                let restart = recipe.restart.should_restart(true);
                // Drop this instance's port watch before closing its handles. Teardown only
                // reclaims bindings keyed on the dead process's *own* handles, never Sora's
                // watch on it, so without this the permanent port would retain one inert
                // binding per restart. Idempotent in the kernel; a real failure is loud.
                if port_unbind(port, dead.instance.process) != 0 {
                    log(b"ttyd: dead watch unbind fail\n");
                }
                if !close_supervised_service(dead) {
                    log(b"ttyd: dead instance cleanup fail\n");
                }
                // Consume one unit of the give-up budget only when policy actually wants a
                // restart — short-circuit means an intentional or Never termination spends
                // nothing, and an exhausted budget gives the service up instead of looping.
                let allowed = restart && ttyd_restart_budget.try_consume();
                if allowed {
                    let delay_ns = ttyd_restart_backoff.next_delay_ns();
                    let timer = Handle(timer_create(delay_ns) as u32);
                    let timer_koid = handle_koid(timer);
                    if timer_koid != u64::MAX && port_bind(port, timer) == 0 {
                        pending_ttyd_restart = Some((recipe, source));
                        ttyd_restart_timer = Some((timer, Handle(timer_koid as u32)));
                        log(b"ttyd: restart delayed\n");
                    } else {
                        let _ = handle_close(timer);
                        log(b"ttyd: restart timer fail\n");
                    }
                } else if restart {
                    log(b"ttyd: restart budget exhausted\n");
                } else {
                    log(b"ttyd: terminated\n");
                }
                continue;
            }
            // console handler — output only (klog! → DebugWrite)
            if source == cons_koid {
                let n = channel_read(console, serve_buf.as_mut_ptr(), 256) as usize;
                if n > 0 {
                    debug_write(serve_buf.as_ptr(), n);
                }
            }
            if source == blk_koid {
                let n = channel_read(block, serve_buf.as_mut_ptr(), 256) as usize;
                if n == 0 {
                    // Spurious wake: our own reply re-signalled this port (the engine signals
                    // the writer's bound port on every ChannelWrite). The request was already
                    // consumed, so the inbox is empty — do NOT reply, or each empty read writes
                    // another reply that re-signals again, spinning the serve loop until the
                    // kernel heap is exhausted.
                } else if n == 16 {
                    let offset = u64::from_le_bytes(serve_buf[..8].try_into().unwrap());
                    let len = u64::from_le_bytes(serve_buf[8..16].try_into().unwrap());
                    let len = (len as usize).min(block_buf.len());
                    if len > 0 && vmo_read(initrd, offset, block_buf.as_mut_ptr(), len) == 0 {
                        channel_write(block, block_buf.as_ptr(), len);
                    } else {
                        channel_write(block, b"".as_ptr(), 0);
                    }
                } else {
                    let served = if serve_buf[0] == 0x01 && n >= 17 {
                        let file_off = u64::from_le_bytes(serve_buf[1..9].try_into().unwrap());
                        let req_len = u64::from_le_bytes(serve_buf[9..17].try_into().unwrap());
                        serve_file_read_at(
                            initrd,
                            &serve_buf[17..n],
                            file_off,
                            req_len,
                            &mut block_buf,
                        )
                    } else {
                        serve_file_read(initrd, &serve_buf[..n], &mut block_buf)
                    };
                    channel_write(block, block_buf.as_ptr(), served);
                }
            }
            // net handler — loopback echo + lightweight control acks.
            if source == net_k {
                let n = channel_read(net, serve_buf.as_mut_ptr(), 256) as usize;
                if n == 4 && &serve_buf[..4] == b"conn" {
                    // The kernel POST cannot receive Sora-process handles as capabilities yet.
                    // Return a protocol token instead of allocating ephemeral local channels.
                    channel_write(net, b"conn".as_ptr(), 4);
                } else if n > 5 && &serve_buf[..5] == b"pipe:" {
                    channel_write(net, b"pipe".as_ptr(), 4);
                } else if n > 0 {
                    channel_write(net, serve_buf.as_ptr(), n);
                }
            }
            // mouse handler — drain fixed boot-mouse records until a UI consumer exists.
            if mouse_koid != u64::MAX && source == mouse_k {
                if let Some(mouse) = mouse_input {
                    let n = channel_read(mouse, serve_buf.as_mut_ptr(), 256) as usize;
                    if n == drv_i2c_hid::MOUSE_EVENT_BYTES {
                        if let Some(report) = drv_i2c_hid::decode_mouse_event(&serve_buf[..n]) {
                            mouse_events_seen = mouse_events_seen.saturating_add(1);
                            pointer_state.apply_boot_mouse(report);
                            if mouse_events_seen <= 8 {
                                let position = pointer_state.position();
                                diag(b"sora: mouse pointer x=");
                                diag_hex(position.x as u64);
                                diag(b" y=");
                                diag_hex(position.y as u64);
                                diag(b" buttons=");
                                diag_hex(pointer_state.buttons() as u64);
                                diag(b"\n");
                            }
                        }
                    } else if n > 0 {
                        malformed_mouse_events_seen = malformed_mouse_events_seen.saturating_add(1);
                        if malformed_mouse_events_seen <= 8 {
                            diag(b"sora: mouse event malformed\n");
                        }
                    }
                }
            }
            // editor repaint handler — a live full-screen program's stdout is just another
            // port source, so its output reaches the console without Sora ever blocking on it.
            if kilo_session_active() && source.0 as u64 == KILO_STDOUT_KOID.load(Ordering::Relaxed)
            {
                let stdout = Handle(KILO_STDOUT_READ.load(Ordering::Relaxed) as u32);
                if stdout.0 != 0 && !kilo_relay_paint(stdout) {
                    write_shell_prompt(&cwd);
                }
                continue;
            }
            // keyboard handler — input only (keystrokes → ttyd → parsed command line)
            if source == kbd_k || (hid_kbd_koid != u64::MAX && source == hid_kbd_k) {
                let input = if source == kbd_k {
                    kbd
                } else {
                    match hid_keyboard_input {
                        Some(handle) => handle,
                        None => continue,
                    }
                };
                let n = channel_read(input, serve_buf.as_mut_ptr(), 256) as usize;
                // Binding replays the channel's state, so the first packet can carry no key:
                // only bytes settle the boot (J559).
                if n > 0 {
                    settle_first_prompt(&mut first_prompt, port, &cwd, b"a key");
                }
                if n > 0 && kilo_session_active() {
                    // A full-screen editor owns the keyboard while it runs: each byte goes
                    // straight through, unedited and unechoed. When the session ends the
                    // shell prompt returns.
                    for i in 0..n {
                        if !kilo_session_key(serve_buf[i]) {
                            write_shell_prompt(&cwd);
                            break;
                        }
                    }
                } else if n > 0 && interactive_ttyd.is_some() {
                    let tty = interactive_ttyd.unwrap().instance;
                    let mut tty_reply = [0u8; ttyd::REPLY_BUF_BYTES];
                    for i in 0..n {
                        dispatch_ttyd_key(
                            tty.client,
                            serve_buf[i],
                            &mut tty_reply,
                            initrd,
                            prog_initrd,
                            root,
                            &mut cwd,
                        );
                    }
                }
            }
        }
    } else {
        // Reached only when the serve PORT itself could not be created — Sora cannot wait on any
        // source, so it idles. Console absence no longer lands here: it is expected on the
        // framebuffer path and must not stop keyboard serving. The marker keeps this state honest
        // on the framebuffer instead of looking like a halt after `anon vmo write ok`.
        log(b"sora: no serve port; idle\n");
        loop {
            core::hint::spin_loop();
        }
    }
}

fn arm_supervised_process(port: Handle, service: sora::SupervisedService<'_>) -> Option<Handle> {
    let koid = handle_koid(service.instance.process);
    if koid != u64::MAX && port_bind(port, service.instance.process) == 0 {
        Some(Handle(koid as u32))
    } else {
        None
    }
}

fn close_supervised_service(service: sora::SupervisedService<'_>) -> bool {
    sora::close_handles(
        &[
            Some(service.instance.process),
            Some(service.instance.client),
        ],
        handle_close,
    )
}

/// Convert a path like "HELLO.TXT" to an 11-byte 8.3 FAT directory entry name.
/// Returns `None` if the name or extension is too long for 8.3 format.
fn path_to_8_3(path: &[u8]) -> Option<[u8; 11]> {
    let mut name = [b' '; 11];
    let dot = path.iter().position(|b| *b == b'.');
    let (base, ext) = match dot {
        Some(d) => (&path[..d], &path[d + 1..]),
        None => (path, &[][..]),
    };
    if base.len() > 8 || ext.len() > 3 || base.is_empty() {
        return None;
    }
    name[..base.len()].copy_from_slice(base);
    name[8..8 + ext.len()].copy_from_slice(ext);
    Some(name)
}

#[derive(Clone, Copy)]
struct PersonaLoadSegment {
    file_offset: u64,
    file_size: u64,
    virt_addr: u64,
    mem_size: u64,
    flags: u64,
}

fn find_initrd_file(initrd: Handle, target: &[u8]) -> Option<(u64, u64)> {
    let mut header = [0u8; 16];
    if vmo_read(initrd, 0, header.as_mut_ptr(), 16) != 0 {
        return None;
    }
    let entry_count = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    let mut entry = [0u8; 80];
    for i in 0..entry_count {
        let off = 16u64 + (i as u64) * 80;
        if vmo_read(initrd, off, entry.as_mut_ptr(), 80) != 0 {
            return None;
        }
        let plen = entry[..64].iter().position(|b| *b == 0).unwrap_or(64);
        if plen == target.len() && entry[..plen] == target[..] {
            let file_off = u64::from_le_bytes(entry[64..72].try_into().unwrap());
            let file_len = u64::from_le_bytes(entry[72..80].try_into().unwrap());
            return Some((file_off, file_len));
        }
    }
    None
}

/// Run a parsed `Statement`. A single command dispatches through [`eval_command`] (shared by
/// the interactive editor and the boot autoexec). A multi-stage pipeline has no execution model
/// yet — that needs concurrent spawn plus a stdin/stdout channel wired between stages — so it is
/// rejected honestly rather than running its stages disconnected (which would run stage 1 and
/// then forward the whole line to the kernel shell).
fn run_statement(
    stmt: &kumoza::Statement,
    raw: &[u8],
    initrd: Handle,
    prog_initrd: u64,
    root: Handle,
    cwd: &mut WorkingDirectory,
) {
    if stmt.pipeline.commands.len() > 1 {
        let msg = b"sora: pipelines not yet supported\n";
        debug_write(msg.as_ptr(), msg.len());
        return;
    }
    kumoza::evaluate(stmt, |cmd| {
        eval_command(cmd, raw, initrd, prog_initrd, root, cwd)
    });
}

/// Dispatch one parsed shell command. Shared by the interactive line editor and the
/// boot autoexec (J183), so both speak the same syntax: `echo`/`help`/`ls`/`run` are
/// handled by this dispatcher; any other non-empty command is forwarded verbatim (`raw`, the
/// original line bytes) to the kernel shell over the `root` channel. `prog_initrd` is a
/// read-only initrd handle granted to capability-aware coreutils (`ls`/`cat`). Always returns
/// `true` — a builtin failure logs but never aborts the surrounding `kumoza::evaluate`.
fn eval_command(
    cmd: &kumoza::Command,
    raw: &[u8],
    initrd: Handle,
    prog_initrd: u64,
    root: Handle,
    cwd: &mut WorkingDirectory,
) -> bool {
    LAST_STATUS.store(0, Ordering::Relaxed);
    if cmd.name == "gfx-demo" {
        graphics_demo(cmd);
        return true;
    }
    if cmd.name == "gfx-client" {
        graphics_client(cmd, initrd);
        return true;
    }
    if cmd.name == "dmesg" {
        dmesg(cmd);
        return true;
    }
    if cmd.name == "id" {
        id(cmd);
        return true;
    }
    if cmd.name == "whoami" {
        match (cmd.args.is_empty(), session()) {
            (true, Some(user)) => {
                log(user.name.as_str().as_bytes());
                log(b"\n");
            }
            (true, None) => {
                log(b"whoami: not logged in\n");
                fail_status(1);
            }
            (false, _) => usage(b"usage: whoami\n"),
        }
        return true;
    }
    if cmd.name == "hostname" {
        if cmd.args.is_empty() {
            log(HOSTNAME.get().as_str().as_bytes());
            log(b"\n");
        } else {
            usage(b"usage: hostname\n");
        }
        return true;
    }
    if cmd.name == "logout" || cmd.name == "exit" {
        if session().is_some() {
            end_session(cwd);
        }
        return true;
    }
    if cmd.name == "pwd" {
        if cmd.args.is_empty() {
            log(cwd.path().as_bytes());
            log(b"\n");
        } else {
            usage(b"usage: pwd\n");
        }
        return true;
    }
    if cmd.name == "cd" {
        if cmd.args.len() > 1 {
            usage(b"usage: cd [path]\n");
        } else if let Err(error) = cwd
            .change(cmd.args.first().map_or("/", |path| path.as_str()), |path| {
                check_directory(initrd, path)
            })
        {
            log_directory_error(b"cd", error);
        }
        return true;
    }
    let mut resolved;
    let cmd = if matches!(cmd.name.as_str(), "cat" | "wc" | "kilo") && cmd.args.len() == 1 {
        let path = match cwd.resolve(&cmd.args[0]) {
            Ok(path) => path,
            Err(error) => {
                log_directory_error(cmd.name.as_bytes(), error);
                return true;
            }
        };
        if matches!(cmd.name.as_str(), "cat" | "wc") && !sora::shell_may_read(path.as_bytes()) {
            log(cmd.name.as_bytes());
            log(b": ");
            log(path.as_bytes());
            log(b": permission denied\n");
            fail_status(1);
            return true;
        }
        resolved = cmd.clone();
        resolved.args[0] = path;
        &resolved
    } else {
        cmd
    };
    if kumoza::write_builtin_output(cmd, |bytes| {
        debug_write(bytes.as_ptr(), bytes.len());
    }) {
        if cmd.name == "false" {
            fail_status(1);
        }
        return true;
    } else if cmd.name == "heap" {
        // S1's on-target proof: allocate past the 256 KiB bootstrap floor and show the heap
        // actually acquired a second region. Sora is itself a `kumo-rt` heap user, so this
        // exercises the real growth path in a real process rather than a contrived one.
        let before = kumo_rt::heap_region_count();
        // Well past the floor, in chunks large enough that the size-class cache cannot serve them.
        let mut held: alloc::vec::Vec<alloc::vec::Vec<u8>> = alloc::vec::Vec::new();
        for _ in 0..8 {
            let mut chunk = alloc::vec::Vec::new();
            chunk.resize(64 * 1024, 0u8);
            held.push(chunk);
        }
        let after = kumo_rt::heap_region_count();
        // Touch every byte so a mapping that was recorded but never backed would fault here
        // rather than pass silently.
        let mut sum = 0u64;
        for chunk in &held {
            sum = sum.wrapping_add(chunk.len() as u64);
        }
        drop(held);
        let settled = kumo_rt::heap_region_count();
        debug_write(b"heap: regions ".as_ptr(), 14);
        log_dec(before as u64);
        debug_write(b" -> ".as_ptr(), 4);
        log_dec(after as u64);
        debug_write(b" (after free ".as_ptr(), 13);
        log_dec(settled as u64);
        debug_write(b") bytes=".as_ptr(), 8);
        log_dec(sum);
        if after > before {
            debug_write(b" -> OK: heap grew on target\n".as_ptr(), 28);
        } else {
            debug_write(b" -> FAIL: no growth\n".as_ptr(), 21);
        }
        return true;
    } else if cmd.name == "kilo" {
        // The first third-party C program on KUMO, byte-identical to antirez's upstream
        // source. Everything it needs was built by J509/J511-J514 for its own reasons; this
        // launcher only grants and gets out of the way. — TIMBERDOODLE
        start_kilo_session(initrd, cmd);
        return true;
    } else if cmd.name == "relibc" {
        // I-a2: run the static C smoke with one finite stdout capability and its own argv
        // VMO — the same two grants any other argv-taking program gets. `_start` hands both
        // to relibc, which builds the SysV startup view from them, activates TLS, and only
        // then proves printf/argv/malloc/exit. Anything the user types after `relibc`
        // arrives as C `argv`, so the bridge carries real input rather than a fixed
        // string. — TIMBERDOODLE
        debug_write(b"relibc: C runtime smoke\n".as_ptr(), 24);
        let mut slots: [&[u8]; MAX_ARGV] = [b""; MAX_ARGV];
        let count = command_argv_slots(cmd, &mut slots);
        // The finite stdin form closes its writer before launch, so the child reads its one
        // line and then observes end-of-input without parking on a peer that will never
        // write again — the bounded shape a boot proof needs. — TIMBERDOODLE
        run_with_startup_input(
            initrd,
            RELIBC_SMOKE_PATH.as_bytes(),
            b"relibc",
            Some(&slots[..count]),
            false,
            FsGrant::Image,
            true,
            false,
            StdinMode::Finite(b"a line from sora\n"),
        );
        return true;
    } else if cmd.name == "threads" {
        // D2: two threads in one address space, handing off through a futex. Launched on demand
        // from the shell rather than at boot — a proof you can re-run is worth more than one that
        // scrolls past once during startup.
        debug_write(b"threads: two threads, one address space\n".as_ptr(), 40);
        if !run_elf_threads(
            initrd,
            b"bin/threads",
            0,
            0,
            ProcessRunFlags::ASYNC.bits(),
            b"threads",
            1,
            None,
        ) {
            debug_write(b"threads: launch failed\n".as_ptr(), 23);
        }
        return true;
    } else if cmd.name == "lua" {
        // Bare `lua` opens an interactive session; every following line goes to the child until it
        // exits. `lua <expression>` stays the one-shot form.
        if cmd.args.is_empty() {
            start_lua_session(initrd);
        } else {
            let mut argv = alloc::vec::Vec::with_capacity(cmd.args.len() + 1);
            argv.push(alloc::string::String::from("lua-repl"));
            argv.extend(cmd.args.iter().cloned());
            run_program(initrd, &argv);
        }
        return true;
    } else if cmd.name == "cat" {
        // cat receives stdout, argv, and read-only initrd in one finite bootstrap message.
        // Only this explicit shell launcher grants the filesystem image; plain `run cat ...`
        // remains authority-free.
        if cmd.args.len() != 1 {
            usage(b"usage: cat <path>\n");
        } else if run_on_filesystem(initrd, b"cat", CAT_PATH, &[], cmd.args[0].as_bytes()) {
            // The selected scratch or removable volume supplies the file.
        } else if prog_initrd == 0 || prog_initrd == u64::MAX {
            debug_write(b"cat: no initrd handle\n".as_ptr(), 22);
        } else {
            let slots = [
                b"cat".as_slice(),
                cmd.args[0].trim_start_matches('/').as_bytes(),
            ];
            run_with_startup(initrd, CAT_PATH.as_bytes(), b"cat", Some(&slots), true);
        }
    } else if cmd.name == "wc" {
        // wc: like cat, granted stdout, its argv VMO, and the read-only initrd capability in
        // one finite bootstrap startup message; it counts the named file. Only this explicit
        // launcher grants the initrd — plain `run wc ...` stays authority-free.
        if cmd.args.len() != 1 {
            usage(b"usage: wc <path>\n");
        } else if prog_initrd == 0 || prog_initrd == u64::MAX {
            debug_write(b"wc: no initrd handle\n".as_ptr(), 21);
        } else {
            let slots = [
                b"wc".as_slice(),
                cmd.args[0].trim_start_matches('/').as_bytes(),
            ];
            run_with_startup(initrd, WC_PATH.as_bytes(), b"wc", Some(&slots), true);
        }
    } else if cmd.name == "ls" {
        // ls: one bootstrap channel is moved into x0. Its finite startup message carries
        // the read-only initrd and stdout capabilities; no authority is inherited.
        // Parse once with the child's rules so routing grants the filesystem it will read.
        // — KESTREL
        let Some(options) = ls::Options::parse(cmd.args.iter().map(|arg| arg.as_bytes())) else {
            usage(b"usage: ls [-al] [--] [path]\n");
            return true;
        };
        let path = match core::str::from_utf8(options.path)
            .ok()
            .and_then(|path| cwd.resolve(path).ok())
        {
            Some(path) => path,
            None => {
                log_directory_error(b"ls", DirectoryError::InvalidPath);
                return true;
            }
        };
        let flags: &[&[u8]] = match (options.show_all, options.long) {
            (true, true) => &[b"-al"],
            (true, false) => &[b"-a"],
            (false, true) => &[b"-l"],
            (false, false) => &[],
        };
        if run_on_filesystem(initrd, b"ls", LS_PATH, flags, path.as_bytes()) {
            // The selected scratch or removable volume supplies the listing.
        } else if prog_initrd == 0 || prog_initrd == u64::MAX {
            debug_write(b"ls: no initrd handle\n".as_ptr(), 20);
        } else {
            let mut slots = alloc::vec![b"ls".as_slice()];
            slots.extend_from_slice(flags);
            slots.push(path.as_bytes());
            run_with_startup(initrd, LS_PATH.as_bytes(), b"ls", Some(&slots), true);
            if path == "/" {
                for name in [b"media".as_slice(), b"tmp"] {
                    ls::write_entry(options, name, ls::NodeKind::Directory, log);
                }
            }
        }
    } else if cmd.name == "run" {
        // run <program> [args...]: spawn bin/<program> and run it to completion, handing it
        // stdout + its arguments through the bootstrap startup message (x0) — no initrd, so a
        // plain `run` stays authority-free. `cmd.args` is already [program, args…], i.e. the
        // program's argv with argv[0] = its name. The interactive twin of the boot autoexec —
        // both drive the same `run_program`.
        if cmd.args.is_empty() {
            usage(b"usage: run <program> [args...]\n");
        } else {
            run_program(initrd, &cmd.args);
        }
    } else if !cmd.name.is_empty() {
        if !kumo_abi::KERNEL_SHELL_COMMANDS.contains(&cmd.name.as_str()) {
            // The kernel shell prints `unknown command`; the status says so too.
            fail_status(127);
        }
        channel_write(root, raw.as_ptr(), raw.len());
    }
    true
}

/// Run a program with one self-terminating bootstrap startup message. stdout (a channel Sora
/// pumps to the console after the child exits) is always transferred; `argv` (when `Some`)
/// attaches a one-shot read-only argv VMO built from its slots, and `grant_initrd` additionally
/// attaches a read-only initrd alias as `cap0`. Sora closes the sender before launch so the
/// child drains the finite set to peer-closed without parking, then moves the bootstrap receiver
/// into the child's `x0`.
///
/// This one launcher serves every spawn: the capability-aware coreutils (`cat`/`wc` with argv +
/// initrd; `ls` with initrd, no argv) and generic `run <program>` (argv, **no** initrd — a plain
/// `run` is authority-free). Every minted capability is reclaimed on any construction or
/// admission failure — no authority is stranded and none is inherited ambiently (DESIGN/004).
/// The set is queued strictly: if any tagged handle fails to enqueue, the launch is abandoned
/// rather than started with a missing capability.
fn run_with_startup(
    initrd: Handle,
    path: &[u8],
    name: &[u8],
    argv: Option<&[&[u8]]>,
    grant_initrd: bool,
) {
    run_with_startup_input(
        initrd,
        path,
        name,
        argv,
        grant_initrd,
        FsGrant::None,
        false,
        false,
        StdinMode::None,
    );
}

/// Spawn a private `fatfs` instance and return the client end of its `kumo-vfs` channel.
///
/// One server per launch, deliberately. Its whole storage authority is a read-write alias of
/// Sora's FAT-image VMO, not the initrd. The VMO persists across launches while each server's
/// open-file table remains disposable soft state. The bootstrap message rides the same channel
/// that afterwards carries vfs frames — the shape `drv-blk` established — so the child needs
/// exactly one handle.
fn spawn_fatfs(initrd: Handle) -> Option<Handle> {
    let (client_raw, server_raw) = channel_create_pair();
    if client_raw == u64::MAX || server_raw == u64::MAX {
        log(b"fatfs: channel unavailable\n");
        return None;
    }
    let client = Handle(client_raw as u32);
    let server = Handle(server_raw as u32);
    let Some((image_vmo, map_len)) = fat_image_alias(initrd) else {
        let _ = handle_close(client);
        let _ = handle_close(server);
        return None;
    };

    let mut header = [0u8; 16];
    header[..8].copy_from_slice(&0u64.to_le_bytes());
    header[8..].copy_from_slice(&map_len.to_le_bytes());
    if channel_write_with_handle(
        client,
        header.as_ptr(),
        header.len(),
        Handle(image_vmo as u32),
    ) != Errno::Ok.status()
    {
        let _ = handle_close(Handle(image_vmo as u32));
        let _ = handle_close(client);
        let _ = handle_close(server);
        log(b"fatfs: bootstrap queue failed\n");
        return None;
    }

    let flags = (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG2).bits();
    if !run_elf(
        initrd,
        FATFS_PATH.as_bytes(),
        0,
        server_raw,
        flags,
        b"fatfs",
    ) {
        let _ = handle_close(client);
        let _ = handle_close(server);
        return None;
    }
    Some(client)
}

/// J545: a storage control channel woke the serve loop: its driver offered a stick's volume, or
/// the driver is gone and its volume with it.
/// Returns whether a volume was mounted: a closed or empty control channel is a driver leaving.
fn media_offered(
    initrd: Handle,
    port: Handle,
    index: usize,
    koids: &mut [Option<Handle>; 3],
) -> bool {
    let control = Handle(XHCI_STORAGE_CONTROLS[index].load(Ordering::Relaxed));
    let mut frame = [0u8; drv_blk::MEDIA_READY_LEN];
    let n = channel_read(control, frame.as_mut_ptr(), frame.len());
    if n == 0 || n == u64::MAX {
        let _ = port_unbind(port, control);
        koids[index] = None;
        if media().is_some_and(|media| media.control == control) {
            set_media(None);
            log(b"media: driver gone; volume removed\n");
        }
        return false;
    }
    let Some(ready) = drv_blk::MediaReady::decode(&frame[..n as usize]) else {
        log(b"media: bad offer\n");
        return false;
    };
    let name = sora::MediaName::new(ready.label, ready.serial);
    set_media(Some(Media {
        name,
        ready,
        control,
    }));
    diag(b"media: /media/");
    diag(name.as_bytes());
    diag(b" usb fat32 blocks=");
    diag_hex(ready.block_count);
    diag(b" serial=");
    diag(sora::MediaName::new(None, ready.serial).as_bytes());
    diag(if ready.read_only {
        b" read-only\n" as &[u8]
    } else {
        b"\n"
    });
    notice(&[
        b"media: /media/",
        name.as_bytes(),
        if ready.read_only {
            b" mounted (fat32, read-only)\n" as &[u8]
        } else {
            b" mounted (fat32)\n"
        },
    ]);
    // J545's end-to-end check: mount the volume once through a block-backed `fatfs`, then let
    // that instance go. Its mount crosses every new hop (Sora's connect, the driver's resident
    // loop, the block protocol), and its exit returns the driver's client slot. — TIMBERDOODLE
    match spawn_media_fatfs(initrd, BlockAccess::ReadOnly) {
        Some(vfs) => {
            let _ = handle_close(vfs);
            diag(b"media: mount check started\n");
        }
        None => log(b"media: mount check spawn fail\n"),
    }
    true
}

/// Grant the selected scratch or USB filesystem and a volume-relative path. Return false
/// outside those mounts, where the caller instead grants the read-only initrd. — KESTREL
fn run_on_filesystem(
    initrd: Handle,
    program: &[u8],
    image: &str,
    flags: &[&[u8]],
    path: &[u8],
) -> bool {
    if let Some(path) = sora::scratch_path(path) {
        run_on_volume(initrd, program, image, flags, path, FsGrant::Image);
        return true;
    }
    let (name, path) = match sora::media_route(path) {
        sora::MediaRoute::Elsewhere => return false,
        sora::MediaRoute::Root => {
            // `/media` is Sora's own namespace: the volumes it has mounted.
            match (program, media()) {
                (b"ls", Some(media)) => {
                    let options = ls::Options::parse(flags.iter().copied()).unwrap();
                    ls::write_entry(options, media.name.as_bytes(), ls::NodeKind::Directory, log);
                }
                (b"ls", None) => {}
                _ => {
                    log(program);
                    log(b": /media: is a directory\n");
                }
            }
            return true;
        }
        sora::MediaRoute::Volume { name, path } => (name, path),
    };
    if !media().is_some_and(|media| media.name.as_bytes() == name) {
        log(program);
        log(b": /media/");
        log(name);
        log(b": no such volume\n");
        return true;
    }
    run_on_volume(
        initrd,
        program,
        image,
        flags,
        path,
        FsGrant::Media(BlockAccess::ReadOnly),
    );
    true
}

fn run_on_volume(
    initrd: Handle,
    program: &[u8],
    image: &str,
    flags: &[&[u8]],
    path: &[u8],
    fs: FsGrant,
) {
    let mut slots: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::with_capacity(flags.len() + 3);
    slots.push(program);
    slots.extend_from_slice(flags);
    if program == b"ls" {
        slots.push(b"--");
    }
    slots.push(path);
    run_with_startup_input(
        initrd,
        image.as_bytes(),
        program,
        Some(&slots),
        false,
        fs,
        false,
        false,
        StdinMode::None,
    );
}

/// The identity service's client end (PLAN/012), or 0 when it did not start.
static IDENTITYD: AtomicU32 = AtomicU32::new(0);

/// The last command's status, for the prompt's `✖` block (PLAN/012 item 5): 0 success, 1 a
/// failure (a child's non-zero exit included, until the kernel reports exit codes), 2 a usage
/// error, 127 a command or program that does not exist.
static LAST_STATUS: AtomicU32 = AtomicU32::new(0);

/// Record a failure. The first one in a command wins, so the most specific report stands.
fn fail_status(code: u8) {
    let _ = LAST_STATUS.compare_exchange(0, u32::from(code), Ordering::Relaxed, Ordering::Relaxed);
}

/// A usage message: status 2, as POSIX utilities and shell builtins report it.
fn usage(message: &[u8]) {
    log(message);
    fail_status(2);
}

/// Sora's single-threaded session state: copied in and out, never borrowed across a call.
struct Shared<T: Copy>(core::cell::UnsafeCell<T>);

// SAFETY: Sora is single-threaded, and `get`/`set` copy without keeping a reference.
unsafe impl<T: Copy> Sync for Shared<T> {}

impl<T: Copy> Shared<T> {
    fn get(&self) -> T {
        // SAFETY: see `Shared`.
        unsafe { *self.0.get() }
    }

    fn set(&self, value: T) {
        // SAFETY: see `Shared`.
        unsafe { *self.0.get() = value }
    }
}

/// Who is logged in on the console (PLAN/012 item 4); `None` shows the login prompt.
static SESSION: Shared<Option<sora::SessionUser>> = Shared(core::cell::UnsafeCell::new(None));
static LOGIN: Shared<sora::Login> = Shared(core::cell::UnsafeCell::new(sora::Login::new()));
/// `/etc/hostname`, read once at boot.
static HOSTNAME: Shared<sora::Text<64>> = Shared(core::cell::UnsafeCell::new(sora::Text::empty()));
/// The pause after the last failed try: its timer and koid, 0 when not paused.
static LOGIN_PAUSE: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
/// How long the console ignores input after [`sora::LOGIN_TRIES`] failures.
const LOGIN_PAUSE_NS: u64 = 5_000_000_000;

fn session() -> Option<sora::SessionUser> {
    SESSION.get()
}

fn read_hostname(initrd: Handle) {
    let mut file = [0u8; 64];
    let n = initrd_find(initrd, HOSTNAME_PATH.as_bytes())
        .map(|(base, len)| (base, (len as usize).min(file.len())))
        .filter(|&(base, n)| vmo_read(initrd, base, file.as_mut_ptr(), n) == 0)
        .map_or(0, |(_, n)| n);
    HOSTNAME.set(sora::hostname(&file[..n]));
}

fn write_login_prompt() {
    match LOGIN.get().stage() {
        sora::LoginStage::Name => {
            log(HOSTNAME.get().as_str().as_bytes());
            log(b" login: ");
        }
        sora::LoginStage::Password => log(b"Password: "),
        sora::LoginStage::Paused => {}
    }
}

/// Canonical input with echo on or off on the console's ttyd, dropping anything typed ahead
/// (`TCSAFLUSH`, as `login` and `getpass` use it).
fn set_console_echo(tty: Handle, echo: bool) -> bool {
    let mode = ttyd::TerminalMode {
        canonical: true,
        echo,
    };
    let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
    ttyd_request_reply(
        tty,
        TtyRequest::set_mode(mode, ttyd::SetModeAction::Flush),
        &mut reply,
    )
    .and_then(|n| TtyReply::parse(&reply[..n]))
    .is_some_and(|reply| reply.status == ttyd::TTY_OK)
}

/// Drop the console's line history: each session starts with none, so nobody recalls the last
/// user's commands, or a password typed at `login:` by mistake.
fn forget_console_history(tty: Handle) {
    let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
    let forgot = ttyd_request_reply(tty, TtyRequest::forget_history(), &mut reply)
        .and_then(|n| TtyReply::parse(&reply[..n]))
        .is_some_and(|reply| reply.status == ttyd::TTY_OK);
    if !forgot {
        diag(b"login: console history not cleared\n");
    }
}

/// `identityd`'s answer to a login. Anything but a match is `None`, the same for every reason.
fn check_login(name: &str, password: &[u8]) -> Option<sora::SessionUser> {
    if !kumo_identity::valid_name(name) {
        return None;
    }
    let mut reply = [0u8; IDENTITY_REPLY_MAX];
    let n = identity_request_reply(IdentityRequest::Verify { name, password }, &mut reply)?;
    match IdentityReply::decode(&reply[..n]) {
        Some(IdentityReply::Ok(subject)) => sora::SessionUser::new(&subject),
        _ => None,
    }
}

/// A line submitted while nobody is logged in.
fn login_line(tty: Handle, line: &[u8], initrd: Handle, cwd: &mut WorkingDirectory) {
    let mut login = LOGIN.get();
    match login.line(line) {
        sora::LoginStep::Ignore => {}
        sora::LoginStep::AskName => {
            LOGIN.set(login);
            write_login_prompt();
        }
        sora::LoginStep::AskPassword => {
            LOGIN.set(login);
            if !set_console_echo(tty, false) {
                // Never ask for a password the terminal would show.
                log(b"login: cannot turn echo off\n");
                let mut login = LOGIN.get();
                login.restart();
                LOGIN.set(login);
            }
            write_login_prompt();
        }
        sora::LoginStep::Check { name, password } => {
            let user = check_login(name, password);
            let _ = set_console_echo(tty, true);
            match user {
                Some(user) => {
                    LOGIN.set(sora::Login::default());
                    forget_console_history(tty);
                    start_session(user, initrd, cwd);
                    write_shell_prompt(cwd);
                }
                None => {
                    diag(b"login: failed on the console\n");
                    log(b"\nLogin incorrect\n");
                    let paused = login.failed();
                    LOGIN.set(login);
                    if paused {
                        log(b"Maximum number of tries exceeded (3)\n");
                        if !arm_login_pause() {
                            end_login_pause();
                        }
                    } else {
                        write_login_prompt();
                    }
                }
            }
        }
    }
}

fn start_session(mut user: sora::SessionUser, initrd: Handle, cwd: &mut WorkingDirectory) {
    *cwd = WorkingDirectory::default();
    if cwd
        .change(user.home.as_str(), |path| check_directory(initrd, path))
        .is_err()
    {
        // As `login(1)` does when the home directory is missing, which it is until RedoxFS.
        log(b"No directory, logging in with HOME=/\n");
        user.home = sora::Text::new("/").unwrap_or(user.home);
    }
    SESSION.set(Some(user));
    LAST_STATUS.store(0, Ordering::Relaxed);
    diag(b"login: ");
    diag(user.name.as_str().as_bytes());
    diag(b" on the console\n");
}

/// `logout`/`exit`: back to the login prompt.
fn end_session(cwd: &mut WorkingDirectory) {
    if let Some(user) = session() {
        diag(b"login: ");
        diag(user.name.as_str().as_bytes());
        diag(b" logged out\n");
    }
    SESSION.set(None);
    LOGIN.set(sora::Login::default());
    *cwd = WorkingDirectory::default();
    log(b"\n");
}

fn arm_login_pause() -> bool {
    let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
    let raw = timer_create(LOGIN_PAUSE_NS);
    if port.0 == 0 || raw == u64::MAX {
        return false;
    }
    let timer = Handle(raw as u32);
    let koid = handle_koid(timer);
    if koid == u64::MAX || port_bind(port, timer) != 0 {
        let _ = handle_close(timer);
        return false;
    }
    LOGIN_PAUSE[0].store(raw, Ordering::Relaxed);
    LOGIN_PAUSE[1].store(koid, Ordering::Relaxed);
    true
}

fn login_pause_koid() -> Option<Handle> {
    let koid = LOGIN_PAUSE[1].load(Ordering::Relaxed);
    (koid != 0).then_some(Handle(koid as u32))
}

fn end_login_pause() {
    let timer = LOGIN_PAUSE[0].swap(0, Ordering::Relaxed);
    LOGIN_PAUSE[1].store(0, Ordering::Relaxed);
    if timer != 0 {
        let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
        let _ = port_unbind(port, Handle(timer as u32));
        let _ = handle_close(Handle(timer as u32));
    }
    let mut login = LOGIN.get();
    login.resume();
    LOGIN.set(login);
    log(b"\n");
    write_login_prompt();
}

/// Start `identityd`. Its whole world is one read-only VMO holding `/etc/passwd`, `/etc/group`
/// and `/etc/shadow`, copied out of the initrd (`kumo_identity::protocol::bundle_files`); it
/// gets no initrd, no filesystem and no device. It parks in `PortWait` before this returns.
fn spawn_identityd(initrd: Handle) -> Option<Handle> {
    let files =
        [PASSWD_PATH, GROUP_PATH, SHADOW_PATH].map(|path| initrd_find(initrd, path.as_bytes()));
    let [Some(passwd), Some(group), Some(shadow)] = files else {
        log(b"identityd: identity files missing\n");
        return None;
    };
    let Some(header) = bundle_header(passwd.1 as usize, group.1 as usize, shadow.1 as usize) else {
        log(b"identityd: identity files too large\n");
        return None;
    };
    let len = BUNDLE_HEADER_LEN as u64 + passwd.1 + group.1 + shadow.1;
    let raw = vmo_create(len);
    if raw == 0 || raw == u64::MAX {
        log(b"identityd: bundle vmo unavailable\n");
        return None;
    }
    let bundle = Handle(raw as u32);
    let mut at = BUNDLE_HEADER_LEN as u64;
    let mut copied = vmo_write(bundle, 0, header.as_ptr(), header.len()) == 0;
    for (base, file_len) in [passwd, group, shadow] {
        copied = copied && copy_from_initrd(initrd, base, file_len, bundle, at);
        at += file_len;
    }
    let grant = handle_duplicate(bundle, Rights::READ | Rights::TRANSFER);
    let _ = handle_close(bundle);
    if !copied || grant == 0 || grant == u64::MAX {
        if grant != 0 && grant != u64::MAX {
            let _ = handle_close(Handle(grant as u32));
        }
        log(b"identityd: bundle copy failed\n");
        return None;
    }

    let (client_raw, server_raw) = channel_create_pair();
    if client_raw == u64::MAX || server_raw == u64::MAX {
        let _ = handle_close(Handle(grant as u32));
        log(b"identityd: channel unavailable\n");
        return None;
    }
    let client = Handle(client_raw as u32);
    let close_all = || {
        let _ = handle_close(client);
        let _ = handle_close(Handle(server_raw as u32));
    };
    if channel_write_with_handle(client, len.to_le_bytes().as_ptr(), 8, Handle(grant as u32))
        != Errno::Ok.status()
    {
        let _ = handle_close(Handle(grant as u32));
        close_all();
        log(b"identityd: bootstrap queue failed\n");
        return None;
    }
    let flags = (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG2).bits();
    if !run_elf(
        initrd,
        IDENTITYD_PATH.as_bytes(),
        0,
        server_raw,
        flags,
        b"identityd",
    ) {
        close_all();
        return None;
    }
    if !child_parked(process_wait()) {
        let _ = handle_close(client);
        log(b"identityd: did not start\n");
        return None;
    }
    Some(client)
}

/// Copy `len` bytes at `base` in the initrd to `offset` in `dst`. VMO copies are capped at 256
/// bytes a call (J166), as `fat_image_alias` does it.
fn copy_from_initrd(initrd: Handle, base: u64, len: u64, dst: Handle, offset: u64) -> bool {
    let mut chunk = [0u8; 256];
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(chunk.len() as u64) as usize;
        if vmo_read(initrd, base + done, chunk.as_mut_ptr(), n) != 0
            || vmo_write(dst, offset + done, chunk.as_ptr(), n) != 0
        {
            return false;
        }
        done += n as u64;
    }
    true
}

/// One request to `identityd` and its reply, pumped the way `ttyd_request_reply` is.
fn identity_request_reply(request: IdentityRequest<'_>, reply: &mut [u8]) -> Option<usize> {
    let client = Handle(IDENTITYD.load(Ordering::Relaxed));
    if client.0 == 0 {
        return None;
    }
    let mut frame = [0u8; IDENTITY_REQUEST_MAX];
    let len = request.encode_into(&mut frame)?;
    let written = channel_write(client, frame.as_ptr(), len) == 0;
    frame.fill(0);
    if !written || !child_parked(process_wait()) {
        return None;
    }
    let n = channel_read(client, reply.as_mut_ptr(), reply.len());
    (n != 0 && n != u64::MAX).then_some(n as usize)
}

/// `id [USER]`: who an account is, as `identityd` knows it; with no name, the session's user.
fn id(cmd: &kumoza::Command) {
    let [name] = cmd.args.as_slice() else {
        match (cmd.args.is_empty(), session()) {
            (true, Some(user)) => {
                let mut line = alloc::string::String::new();
                if sora::write_id(&user.subject(), &mut line).is_ok() {
                    log(line.as_bytes());
                }
            }
            _ => usage(b"usage: id [user]\n"),
        }
        return;
    };
    let no_such_user = || {
        log(b"id: '");
        log(name.as_bytes());
        log(b"': no such user\n");
        fail_status(1);
    };
    if !kumo_identity::valid_name(name) {
        no_such_user();
        return;
    }
    let mut reply = [0u8; IDENTITY_REPLY_MAX];
    let answer = identity_request_reply(IdentityRequest::Lookup { name }, &mut reply)
        .and_then(|n| IdentityReply::decode(&reply[..n]));
    match answer {
        Some(IdentityReply::Ok(subject)) => {
            let mut line = alloc::string::String::new();
            if sora::write_id(&subject, &mut line).is_ok() {
                log(line.as_bytes());
            }
        }
        Some(IdentityReply::NotFound) => no_such_user(),
        _ => {
            log(b"id: identity service unavailable\n");
            fail_status(1);
        }
    }
}

/// `dmesg` prints the kernel log; `dmesg -n LEVEL` sets which levels the console also shows,
/// as Linux's does (J559). LEVEL is 0-7 or a name from `emerg` to `debug`; `dmesg -n` alone
/// reports the current one.
fn dmesg(cmd: &kumoza::Command) {
    let resource = Handle(ROOT_RESOURCE.load(Ordering::Relaxed) as u32);
    match cmd.args.as_slice() {
        [] => {
            let mut buf = [0u8; 1024];
            let mut offset = 0;
            loop {
                let (n, next) = kumo_rt::log_read(resource, offset, &mut buf);
                if n == u64::MAX {
                    {
                        log(b"dmesg: kernel log unavailable\n");
                        fail_status(1);
                    }
                    return;
                }
                if n == 0 {
                    return;
                }
                if !sora::relay_console_output(&buf[..n as usize], |chunk| {
                    debug_write(chunk.as_ptr(), chunk.len())
                }) {
                    return;
                }
                offset = next;
            }
        }
        [flag] if flag == "-n" => {
            let level = kumo_rt::log_level(resource, kumo_abi::sys::LOG_LEVEL_QUERY);
            match sora::log_level_name(level) {
                Some(name) => {
                    log(b"console level ");
                    log(name);
                    log(b"\n");
                }
                None => {
                    log(b"dmesg: kernel log unavailable\n");
                    fail_status(1);
                }
            }
        }
        [flag, level] if flag == "-n" => match sora::parse_log_level(level.as_bytes()) {
            Some(level) => {
                if kumo_rt::log_level(resource, u64::from(level)) == u64::MAX {
                    {
                        log(b"dmesg: kernel log unavailable\n");
                        fail_status(1);
                    }
                }
            }
            None => usage(
                b"dmesg: level is 0-7 or emerg, alert, crit, err, warn, notice, info, debug\n",
            ),
        },
        _ => usage(b"usage: dmesg [-n level]\n"),
    }
}

fn log_directory_error(command: &[u8], error: DirectoryError) {
    fail_status(1);
    log(command);
    log(match error {
        DirectoryError::InvalidPath => b": invalid or too long path\n",
        DirectoryError::NotFound => b": no such directory\n",
        DirectoryError::NotDirectory => b": not a directory\n",
        DirectoryError::Unavailable => b": filesystem unavailable\n",
    });
}

/// Validate against the same granted namespace used by the file launchers. A failed probe
/// leaves the shell's directory untouched, and closing its channel disposes the server.
/// No directory state or pathname authority moves into the kernel. — KESTREL
fn check_directory(initrd: Handle, path: &str) -> Result<(), DirectoryError> {
    if let Some(path) = sora::scratch_path(path.as_bytes()) {
        return check_volume_directory(spawn_fatfs(initrd), path);
    }
    match sora::media_route(path.as_bytes()) {
        sora::MediaRoute::Root => return Ok(()),
        sora::MediaRoute::Volume { name, path } => {
            if !media().is_some_and(|volume| volume.name.as_bytes() == name) {
                return Err(DirectoryError::NotFound);
            }
            return check_volume_directory(spawn_media_fatfs(initrd, BlockAccess::ReadOnly), path);
        }
        sora::MediaRoute::Elsewhere => {}
    }
    let mut table = [0u8; kumo_abi::INITRD_TABLE_MAX];
    if vmo_read(initrd, 0, table.as_mut_ptr(), kumo_abi::INITRD_HEADER_LEN) != 0 {
        return Err(DirectoryError::Unavailable);
    }
    let len = kumo_abi::entry_table_bytes(&table[..kumo_abi::INITRD_HEADER_LEN])
        .map_err(|_| DirectoryError::Unavailable)?;
    if len > table.len() {
        return Err(DirectoryError::Unavailable);
    }
    let mut offset = kumo_abi::INITRD_HEADER_LEN;
    while offset < len {
        let count = (len - offset).min(256);
        if vmo_read(initrd, offset as u64, table[offset..].as_mut_ptr(), count) != 0 {
            return Err(DirectoryError::Unavailable);
        }
        offset += count;
    }
    match ls::initrd_kind(path, kumo_abi::entries(&table[..len])) {
        Some(ls::NodeKind::Directory) => Ok(()),
        Some(ls::NodeKind::File(_)) => Err(DirectoryError::NotDirectory),
        None => Err(DirectoryError::NotFound),
    }
}

fn check_volume_directory(fs: Option<Handle>, path: &[u8]) -> Result<(), DirectoryError> {
    let fs = fs.ok_or(DirectoryError::Unavailable)?;
    let mut request = [0u8; 7 + kumo_vfs::MAX_PATH];
    let mut reply = [0u8; 1024];
    let result = (|| {
        let len = kumo_vfs::Request::ReadDir { path, start: 0 }
            .encode_into(&mut request)
            .ok_or(DirectoryError::InvalidPath)?;
        if channel_write(fs, request.as_ptr(), len) != 0 {
            return Err(DirectoryError::Unavailable);
        }
        let n = channel_read(fs, reply.as_mut_ptr(), reply.len());
        if n == 0 || n > reply.len() as u64 {
            return Err(DirectoryError::Unavailable);
        }
        match kumo_vfs::parse_dir_reply(&reply[..n as usize]) {
            Ok(_) => Ok(()),
            Err(kumo_vfs::VfsError::Status(kumo_vfs::VFS_NOT_FOUND)) => {
                Err(DirectoryError::NotFound)
            }
            Err(kumo_vfs::VfsError::Status(kumo_vfs::VFS_NOT_DIR)) => {
                Err(DirectoryError::NotDirectory)
            }
            Err(_) => Err(DirectoryError::Unavailable),
        }
    })();
    let _ = handle_close(fs);
    // Pump the disposable probe's exit and its block peer's close notification before
    // drawing the next prompt, just as ordinary synchronous launches do. — KESTREL
    let _ = process_wait();
    result
}

/// J545: start a `fatfs` instance over the registered stick and return its vfs client channel.
/// The instance gets its own block channel, whose other end goes to the stick's driver first;
/// when the instance exits, the driver sees that channel close and frees the client slot.
fn spawn_media_fatfs(initrd: Handle, access: BlockAccess) -> Option<Handle> {
    let media = media()?;
    let (block_client_raw, block_server_raw) = channel_create_pair();
    if block_client_raw == u64::MAX || block_server_raw == u64::MAX {
        for end in [block_client_raw, block_server_raw] {
            if end != u64::MAX {
                let _ = handle_close(Handle(end as u32));
            }
        }
        log(b"media: block channel unavailable\n");
        return None;
    }
    let block_client = Handle(block_client_raw as u32);
    let block_server = Handle(block_server_raw as u32);
    let connect = drv_blk::encode_connect(access);
    if channel_write_with_handle(media.control, connect.as_ptr(), connect.len(), block_server)
        != Errno::Ok.status()
    {
        let _ = handle_close(block_client);
        let _ = handle_close(block_server);
        log(b"media: driver connect failed\n");
        return None;
    }

    let (vfs_client_raw, fatfs_end_raw) = channel_create_pair();
    if vfs_client_raw == u64::MAX || fatfs_end_raw == u64::MAX {
        for end in [vfs_client_raw, fatfs_end_raw] {
            if end != u64::MAX {
                let _ = handle_close(Handle(end as u32));
            }
        }
        let _ = handle_close(block_client);
        log(b"media: vfs channel unavailable\n");
        return None;
    }
    let vfs_client = Handle(vfs_client_raw as u32);
    let bootstrap = drv_blk::encode_client_bootstrap(media.ready.block_count);
    if channel_write_with_handle(
        vfs_client,
        bootstrap.as_ptr(),
        bootstrap.len(),
        block_client,
    ) != Errno::Ok.status()
    {
        let _ = handle_close(block_client);
        let _ = handle_close(vfs_client);
        let _ = handle_close(Handle(fatfs_end_raw as u32));
        log(b"media: fatfs bootstrap failed\n");
        return None;
    }
    let flags = (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG2).bits();
    if !run_elf(
        initrd,
        FATFS_PATH.as_bytes(),
        0,
        fatfs_end_raw,
        flags,
        b"fatfs",
    ) {
        // Closing the client drops the queued bootstrap, and with it the block channel: the
        // driver sees it close and frees the slot it had bound.
        let _ = handle_close(vfs_client);
        let _ = handle_close(Handle(fatfs_end_raw as u32));
        return None;
    }
    Some(vfs_client)
}

fn fat_image_alias(initrd: Handle) -> Option<(u64, u64)> {
    let mut backing = FAT_IMAGE_VMO.load(Ordering::Acquire);
    let mut map_len = FAT_IMAGE_LEN.load(Ordering::Relaxed);
    if backing == 0 {
        let Some((image_base, image_len)) = initrd_find(initrd, FAT32_IMG_PATH.as_bytes()) else {
            log(b"fatfs: image not found\n");
            return None;
        };
        map_len = image_len.saturating_add(0xFFF) & !0xFFF;
        backing = vmo_create(map_len);
        if backing == 0 || backing == u64::MAX {
            log(b"fatfs: backing vmo unavailable\n");
            return None;
        }

        // Child VMO copy syscalls are deliberately capped at 256 bytes (J166), so copy
        // the exact FAT payload in bounded pieces. Padding stays zero from VmoCreate.
        let backing_handle = Handle(backing as u32);
        let mut offset = 0u64;
        let mut chunk = [0u8; 256];
        while offset < image_len {
            let n = (image_len - offset).min(chunk.len() as u64) as usize;
            if vmo_read(initrd, image_base + offset, chunk.as_mut_ptr(), n) != 0
                || vmo_write(backing_handle, offset, chunk.as_ptr(), n) != 0
            {
                let _ = handle_close(backing_handle);
                log(b"fatfs: backing copy failed\n");
                return None;
            }
            offset += n as u64;
        }
        FAT_IMAGE_LEN.store(map_len, Ordering::Relaxed);
        FAT_IMAGE_VMO.store(backing, Ordering::Release);
    }

    let alias = handle_duplicate(
        Handle(backing as u32),
        Rights::READ | Rights::WRITE | Rights::TRANSFER,
    );
    if alias == 0 || alias == u64::MAX {
        log(b"fatfs: image grant unavailable\n");
        None
    } else {
        Some((alias, map_len))
    }
}

/// The framebuffer console's grid once drv-fb is drawing it, `rows << 16 | columns`; 0 while
/// there is none, and sessions keep ttyd's 80x24 (J557).
static CONSOLE_WINDOW: AtomicU32 = AtomicU32::new(0);

fn console_window_size() -> Option<ttyd::WindowSize> {
    let packed = CONSOLE_WINDOW.load(Ordering::Relaxed);
    let size = ttyd::WindowSize {
        rows: (packed >> 16) as u16,
        columns: packed as u16,
    };
    (size.rows != 0 && size.columns != 0).then_some(size)
}

/// Spawn a private ttyd control channel, distinct from the child's stream channels.
fn spawn_ttyd_control(initrd: Handle) -> Option<Handle> {
    let recipe = SvcRecipe {
        initrd,
        path: TTYD_PATH,
    };
    let instance = spawn_from_recipe(&recipe)?;
    if !child_parked(process_wait()) {
        let _ = handle_close(instance.client);
        let _ = handle_close(instance.process);
        log(b"ttyd: control park fail\n");
        return None;
    }
    // The session draws on the framebuffer console, so its terminal is that size. Sora owns the
    // display and sets it before the client end goes to the program, the way a terminal sets
    // `TIOCSWINSZ` on its pty. Kilo reads it once, at start. — TIMBERDOODLE
    if let Some(size) = console_window_size() {
        let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
        let set = ttyd_request_reply(
            instance.client,
            TtyRequest::set_window_size(size),
            &mut reply,
        )
        .and_then(|n| TtyReply::parse(&reply[..n]))
        .is_some_and(|reply| reply.window_size == Some(size));
        if !set {
            diag(b"ttyd: window size not set; 80x24\n");
        }
    }
    // Closing Sora's process handle does not terminate the admitted resident. Its client
    // channel is the lifetime authority; once the launched child exits, peer-closed lets
    // `ttyd` leave its serve loop and exit too. — KESTREL
    let _ = handle_close(instance.process);
    Some(instance.client)
}

/// Which filesystem, if any, a launch is granted as its `STARTUP_TAG_FS` channel. Each grant is
/// a private `fatfs` instance started for that launch alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FsGrant {
    None,
    /// Over Sora's RAM FAT image (J508): what kilo and the relibc smoke write to.
    Image,
    /// Over the USB volume with only the access this launch needs (J550). — KESTREL
    Media(BlockAccess),
}

/// How a spawned child gets its standard input.
enum StdinMode<'a> {
    /// No stdin at all — the child is granted no input capability.
    None,
    /// One finite message, with the writer closed before launch, so a one-shot child reads its
    /// submitted line and then observes peer-closed without parking. — KESTREL
    Finite(&'a [u8]),
    /// Sora keeps the writer and the child's stdout reader, and does **not** drain to EOF. The
    /// child parks on its first read; the caller drives it line by line for as long as it lives.
    Interactive,
}

/// `run_with_startup` plus stdin and optional filesystem/terminal-control grants. Returns
/// `Some((stdin_write, stdout_read))` only for [`StdinMode::Interactive`], where the
/// caller owns the running child's two ends.
///
/// `fs` spawns a private `fatfs` instance for this launch and hands the child the
/// client end of its `kumo-vfs` channel. Like `grant_initrd`, only an explicit launcher
/// grants it: a program that was not given the channel cannot name a file at all.
/// `grant_tty` similarly spawns a private `ttyd` control endpoint, kept separate from
/// the child's byte-stream handles so terminal replies cannot be consumed as input.
fn run_with_startup_input(
    initrd: Handle,
    path: &[u8],
    name: &[u8],
    argv: Option<&[&[u8]]>,
    grant_initrd: bool,
    fs: FsGrant,
    grant_tty: bool,
    bind_stdout: bool,
    stdin: StdinMode<'_>,
) -> Option<InteractiveChild> {
    let (stdout_read_raw, stdout_write_raw) = channel_create_pair();
    if stdout_read_raw == u64::MAX || stdout_write_raw == u64::MAX {
        log(name);
        log(b": stdout unavailable\n");
        return None;
    }
    let stdout_read = Handle(stdout_read_raw as u32);
    let stdout_write = Handle(stdout_write_raw as u32);

    let (bootstrap_send_raw, bootstrap_child_raw) = channel_create_pair();
    if bootstrap_send_raw == u64::MAX || bootstrap_child_raw == u64::MAX {
        let _ = handle_close(stdout_read);
        let _ = handle_close(stdout_write);
        log(name);
        log(b": bootstrap unavailable\n");
        return None;
    }
    let bootstrap_send = Handle(bootstrap_send_raw as u32);
    let bootstrap_child = Handle(bootstrap_child_raw as u32);

    // `stdin_writer` is retained only for an interactive session; the finite form closes its
    // writer here so the child sees peer-closed after its one line.
    let mut stdin_writer: Option<Handle> = None;
    let stdin_read = match stdin {
        StdinMode::None => None,
        StdinMode::Finite(bytes) => {
            if bytes.is_empty() || bytes.len() > 256 {
                for handle in [stdout_read, stdout_write, bootstrap_send, bootstrap_child] {
                    let _ = handle_close(handle);
                }
                log(name);
                log(b": stdin too long\n");
                return None;
            }
            let (read_raw, write_raw) = channel_create_pair();
            if read_raw == u64::MAX || write_raw == u64::MAX {
                for handle in [stdout_read, stdout_write, bootstrap_send, bootstrap_child] {
                    let _ = handle_close(handle);
                }
                log(name);
                log(b": stdin unavailable\n");
                return None;
            }
            let read = Handle(read_raw as u32);
            let write = Handle(write_raw as u32);
            let written = channel_write(write, bytes.as_ptr(), bytes.len()) == Errno::Ok.status();
            let writer_closed = handle_close(write) == Errno::Ok.status();
            if !written || !writer_closed {
                let _ = handle_close(read);
                for handle in [stdout_read, stdout_write, bootstrap_send, bootstrap_child] {
                    let _ = handle_close(handle);
                }
                log(name);
                log(b": stdin queue failed\n");
                return None;
            }
            Some(read)
        }
        StdinMode::Interactive => {
            let (read_raw, write_raw) = channel_create_pair();
            if read_raw == u64::MAX || write_raw == u64::MAX {
                for handle in [stdout_read, stdout_write, bootstrap_send, bootstrap_child] {
                    let _ = handle_close(handle);
                }
                log(name);
                log(b": stdin unavailable\n");
                return None;
            }
            stdin_writer = Some(Handle(write_raw as u32));
            Some(Handle(read_raw as u32))
        }
    };

    // Mint the requested per-child capabilities: a one-shot read-only argv VMO when the program
    // takes arguments, and a fresh read-only initrd alias only when the caller grants it.
    let argv_child_raw = match argv {
        Some(slots) => argv_transfer_handle(slots),
        None => 0,
    };
    let initrd_child_raw = if grant_initrd {
        handle_duplicate(initrd, Rights::READ | Rights::TRANSFER)
    } else {
        0
    };
    // The filesystem grant is a whole server, not a handle duplicate: spawn it before the
    // child so the vfs channel is live the moment the child's first request lands.
    let fs_child = match fs {
        FsGrant::None => None,
        FsGrant::Image => spawn_fatfs(initrd),
        FsGrant::Media(access) => spawn_media_fatfs(initrd, access),
    };
    let tty_child = if grant_tty {
        spawn_ttyd_control(initrd)
    } else {
        None
    };
    let argv_bad = argv.is_some() && (argv_child_raw == 0 || argv_child_raw == u64::MAX);
    let initrd_bad = grant_initrd && (initrd_child_raw == 0 || initrd_child_raw == u64::MAX);
    let fs_bad = fs != FsGrant::None && fs_child.is_none();
    let tty_bad = grant_tty && tty_child.is_none();
    if argv_bad || initrd_bad || fs_bad || tty_bad {
        for handle in [stdout_read, stdout_write, bootstrap_send, bootstrap_child] {
            let _ = handle_close(handle);
        }
        if let Some(stdin_read) = stdin_read {
            let _ = handle_close(stdin_read);
        }
        if argv.is_some() && !argv_bad {
            let _ = handle_close(Handle(argv_child_raw as u32));
        }
        if grant_initrd && !initrd_bad {
            let _ = handle_close(Handle(initrd_child_raw as u32));
        }
        if let Some(fs) = fs_child {
            let _ = handle_close(fs);
        }
        if let Some(tty) = tty_child {
            let _ = handle_close(tty);
        }
        if let Some(writer) = stdin_writer {
            let _ = handle_close(writer);
        }
        log(name);
        log(b": startup grant unavailable\n");
        return None;
    }

    // Assemble the tagged set: [stdin], stdout, [argv], [cap0=initrd], [fs], [tty]. Order is
    // immaterial — the child installs by tag — but kept deterministic for serial diagnosis.
    let mut tags: [(u8, Handle); 6] = [(0, Handle(0)); 6];
    let mut expected = 0usize;
    if let Some(stdin_read) = stdin_read {
        tags[expected] = (STARTUP_TAG_STDIN, stdin_read);
        expected += 1;
    }
    tags[expected] = (STARTUP_TAG_STDOUT, stdout_write);
    expected += 1;
    if argv.is_some() {
        tags[expected] = (STARTUP_TAG_ARGV, Handle(argv_child_raw as u32));
        expected += 1;
    }
    if grant_initrd {
        tags[expected] = (STARTUP_TAG_CAP0, Handle(initrd_child_raw as u32));
        expected += 1;
    }
    if let Some(fs) = fs_child {
        tags[expected] = (STARTUP_TAG_FS, fs);
        expected += 1;
    }
    if let Some(tty) = tty_child {
        tags[expected] = (STARTUP_TAG_TTY, tty);
        expected += 1;
    }

    let mut queued = 0usize;
    for &(tag, handle) in &tags[..expected] {
        if channel_write_with_handle(bootstrap_send, &tag, 1, handle) == Errno::Ok.status() {
            queued += 1;
        } else {
            let _ = handle_close(handle);
        }
    }

    let sender_closed = handle_close(bootstrap_send) == Errno::Ok.status();
    if queued != expected || !sender_closed {
        reclaim_startup_handles(bootstrap_child, Some(queued));
        let _ = handle_close(bootstrap_child);
        let _ = handle_close(stdout_read);
        if let Some(writer) = stdin_writer {
            let _ = handle_close(writer);
        }
        log(name);
        log(b": startup queue failed\n");
        return None;
    }

    // Bind the child's stdout to the serve port BEFORE admitting it. `port_bind` only wakes
    // on NEW signals, so a child that writes and exits between admission and a later bind
    // strands its output in a channel nobody is listening to — and, worse, leaves a session
    // that captured the keyboard with nothing left to ever release it. On a fast board the
    // child reliably wins that race. Bound first, neither the output nor the closure can be
    // missed. — TIMBERDOODLE
    if bind_stdout {
        let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
        let koid = handle_koid(stdout_read);
        if port.0 == 0 || koid == u64::MAX || port_bind(port, stdout_read) != 0 {
            for handle in [stdout_read, stdout_write, bootstrap_child] {
                let _ = handle_close(handle);
            }
            if let Some(writer) = stdin_writer {
                let _ = handle_close(writer);
            }
            log(name);
            log(b": stdout bind failed\n");
            return None;
        }
        KILO_STDOUT_KOID.store(koid, Ordering::Relaxed);
    }

    let flags = ProcessRunFlags::TRANSFER_ARG.bits();
    let mut process = None;
    let keep = if stdin_writer.is_some() {
        Some(&mut process)
    } else {
        None
    };
    let ran = run_elf_threads(initrd, path, bootstrap_child_raw, 0, flags, name, 0, keep);

    if handle_koid(bootstrap_child) == u64::MAX {
        if !ran && stdin_writer.is_none() {
            // A finite child ran to completion and exited non-zero.
            fail_status(1);
        }
        // The child is admitted. An interactive session hands both ends back and returns *without
        // draining: the child is parked on its first stdin read, and pumping to EOF here would
        // wait for a child that is waiting for us.
        if let Some(writer) = stdin_writer {
            return Some(InteractiveChild {
                stdin: writer,
                stdout: stdout_read,
                process,
            });
        }
        pump_to_console(stdout_read);
    } else {
        // Admission failed before the bootstrap transfer committed. Recover and close
        // every capability queued in its message so no authority is stranded.
        fail_status(1);
        reclaim_startup_handles(bootstrap_child, None);
        let _ = handle_close(bootstrap_child);
        if let Some(writer) = stdin_writer {
            let _ = handle_close(writer);
        }
    }
    if let Some(process) = process {
        let _ = handle_close(process);
    }
    let _ = handle_close(stdout_read);
    None
}

/// A running interactive child's ends: the writer for its stdin, the reader for its stdout, and
/// the process handle that can end it when it stops answering (J556).
struct InteractiveChild {
    stdin: Handle,
    stdout: Handle,
    process: Option<Handle>,
}

/// Recover handles from a startup message that could not be delivered. `Some(n)` drains
/// exactly the known queued messages without risking a park (including partial construction).
/// With a closed sender, `None` safely drains to EOF.
fn reclaim_startup_handles(bootstrap: Handle, count: Option<usize>) {
    let mut tag = [0u8; 1];
    let limit = count.unwrap_or(usize::MAX);
    for _ in 0..limit {
        let (n, raw_handle) = channel_read_with_handle(bootstrap, tag.as_mut_ptr(), tag.len());
        if n == 0 {
            break;
        }
        if raw_handle != 0 {
            let _ = handle_close(Handle(raw_handle as u32));
        }
    }
}

fn pump_to_console(read_end: Handle) {
    let mut buf = [0u8; kumo_ipc::MAX_INLINE_BYTES];
    loop {
        let n = channel_read(read_end, buf.as_mut_ptr(), buf.len());
        if n == 0 || n == u64::MAX {
            break;
        }
        if !sora::relay_console_output(&buf[..n as usize], |chunk| {
            debug_write(chunk.as_ptr(), chunk.len())
        }) {
            break;
        }
    }
}

/// The live interactive Lua session's two ends, or 0 when none is running.
///
/// Globals for the same reason the boot VMO handles are: the shell's line dispatch runs several
/// frames below where the session is created, and Sora is single-threaded.
static LUA_STDIN_WRITE: AtomicU64 = AtomicU64::new(0);
static LUA_STDOUT_READ: AtomicU64 = AtomicU64::new(0);

/// Largest reply Sora will relay from the Lua child in one read. The child's own bounds (print
/// budget + displayed value + prompt) sit comfortably inside this.
const LUA_REPLY_BYTES: usize = 1024;

fn lua_session_active() -> bool {
    LUA_STDIN_WRITE.load(Ordering::Relaxed) != 0
}

/// Close and forget the live session's ends.
fn end_lua_session() {
    let stdin = LUA_STDIN_WRITE.swap(0, Ordering::Relaxed);
    let stdout = LUA_STDOUT_READ.swap(0, Ordering::Relaxed);
    if stdin != 0 {
        let _ = handle_close(Handle(stdin as u32));
    }
    if stdout != 0 {
        let _ = handle_close(Handle(stdout as u32));
    }
}

/// Read one reply from the Lua child and render it. Returns false once the child is gone — its
/// stdout closing is the only end-of-session signal, which is why `exit` needs no reply.
fn lua_relay_reply(stdout: Handle) -> bool {
    let mut buf = [0u8; LUA_REPLY_BYTES];
    let n = channel_read(stdout, buf.as_mut_ptr(), buf.len());
    if n == 0 || n == u64::MAX {
        end_lua_session();
        return false;
    }
    debug_write(buf.as_ptr(), n as usize);
    true
}

/// Start an interactive Lua session: spawn the child holding both ends, then relay its greeting.
fn start_lua_session(initrd: Handle) -> bool {
    if lua_session_active() {
        debug_write(b"lua: already running\n".as_ptr(), 21);
        return true;
    }
    let slots: [&[u8]; 2] = [b"lua-repl", b"-i"];
    let Some(child) = run_with_startup_input(
        initrd,
        b"bin/lua-repl",
        b"lua-repl",
        Some(&slots),
        false,
        FsGrant::None,
        false,
        false,
        StdinMode::Interactive,
    ) else {
        debug_write(b"lua: session unavailable\n".as_ptr(), 25);
        return false;
    };
    // A Lua session ends when the REPL itself exits and its stdout closes; Sora keeps no handle
    // to end it, as before.
    if let Some(process) = child.process {
        let _ = handle_close(process);
    }
    let stdout_read = child.stdout;
    LUA_STDIN_WRITE.store(child.stdin.0 as u64, Ordering::Relaxed);
    LUA_STDOUT_READ.store(stdout_read.0 as u64, Ordering::Relaxed);
    // The child writes its banner and prompt before its first read, so this relay both renders the
    // greeting and proves the session is actually serving.
    lua_relay_reply(stdout_read)
}

/// The live `kilo` session's two ends, or 0 when none is running. Globals for the same
/// reason the Lua session's are: the session outlives the shell frame that created it.
static KILO_STDIN_WRITE: AtomicU64 = AtomicU64::new(0);
static KILO_STDOUT_READ: AtomicU64 = AtomicU64::new(0);
/// The live editor's process handle (with `MANAGE`), kept so Sora can end an editor it gives
/// up on (J556). 0 when there is none.
static KILO_PROCESS: AtomicU64 = AtomicU64::new(0);

/// The key that ends a live editor from the keyboard: Ctrl-\, the serial console's escape
/// prefix. The kernel keeps that byte on serial, so this reaches Sora only from a USB or I2C
/// keyboard, which until now had no way out of an editor that stopped answering.
const KILO_ESCAPE_KEY: u8 = 0x1c;

fn kilo_session_active() -> bool {
    KILO_STDIN_WRITE.load(Ordering::Relaxed) != 0
}

/// End the editor session. `kill` also ends the editor: Sora is giving up on a child that may
/// still be alive. Closing its stdin alone is not enough. relibc reads end-of-file as 0, and
/// Kilo's `while ((nread = read(fd,&c,1)) == 0);` then spins at child priority, above Sora's,
/// for good. That is how J555's watchdog left the board needing a reset. An editor that
/// already exited only needs its handle closed. — TIMBERDOODLE
fn end_kilo_session(kill: bool) {
    let process = KILO_PROCESS.swap(0, Ordering::Relaxed);
    if process != 0 {
        if kill {
            // Name Sora's end of the editor's key channel. The kernel's kill report names
            // the far end of whatever channel the editor was parked on, so the two match
            // exactly when the editor was waiting for keys.
            let keys = KILO_STDIN_WRITE.load(Ordering::Relaxed);
            diag(b"kilo: keys channel ");
            diag_hex(handle_koid(Handle(keys as u32)));
            diag(b"\n");
            let _ = process_kill(Handle(process as u32));
        }
        let _ = handle_close(Handle(process as u32));
    }
    let stdin = KILO_STDIN_WRITE.swap(0, Ordering::Relaxed);
    let stdout = KILO_STDOUT_READ.swap(0, Ordering::Relaxed);
    KILO_STDOUT_KOID.store(0, Ordering::Relaxed);
    KILO_SILENT_KEYS.store(0, Ordering::Relaxed);
    let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
    if port.0 != 0 && stdout != 0 {
        let _ = port_unbind(port, Handle(stdout as u32));
    }
    if stdin != 0 {
        let _ = handle_close(Handle(stdin as u32));
    }
    if stdout != 0 {
        let _ = handle_close(Handle(stdout as u32));
    }
    // Clear after every exit, including forced exits, before the caller redraws
    // the shell prompt. No newline should move that prompt off row one. — KESTREL
    debug_write(
        sora::CONSOLE_SESSION_RESET.as_ptr(),
        sora::CONSOLE_SESSION_RESET.len(),
    );
}

/// Sora's serve port, published so a session started deep in the shell can bind its
/// child's stdout to the same loop that already multiplexes console, block, net and keys.
static SERVE_PORT: AtomicU64 = AtomicU64::new(0);

/// The koid of the live editor's stdout, matched against the serve port's packet source.
static KILO_STDOUT_KOID: AtomicU64 = AtomicU64::new(0);

/// Keystrokes forwarded since the editor last painted anything.
static KILO_SILENT_KEYS: AtomicU64 = AtomicU64::new(0);

/// How many keys a live editor may swallow without drawing before Sora takes the keyboard
/// back. Kilo repaints on every key, so any run this long means the far end is gone or
/// wedged. The bound exists because a session owns the keyboard: without it, one dead child
/// locks the console, and the kernel's `~~~` escape hatch watches only the serial UART — so
/// a user typing on USB has no way out at all. — TIMBERDOODLE
const KILO_SILENT_KEY_LIMIT: u64 = 64;

/// Relay one repaint. The serve port fires once per child write, so this reads exactly
/// once: a second read would find the channel empty and park Sora with nothing left to
/// wake it — the same hazard J510 fixed one layer down. Returns false when the editor's
/// stdout is gone, which is the only end-of-session signal an editor gives.
fn kilo_relay_paint(stdout: Handle) -> bool {
    // ChannelRead consumes one whole message, discarding any bytes that do not
    // fit. A TTY control-reply buffer is not a stdout-message bound. — KESTREL
    let mut buf = [0u8; kumo_ipc::MAX_INLINE_BYTES];
    let n = channel_read(stdout, buf.as_mut_ptr(), buf.len());
    if n == 0 || n == u64::MAX {
        // 0 is the editor's stdout closing: it exited.
        end_kilo_session(n == u64::MAX);
        return false;
    }
    if sora::relay_console_output(&buf[..n as usize], |chunk| {
        debug_write(chunk.as_ptr(), chunk.len())
    }) {
        // A complete repaint is editor progress, not a lifetime key budget.
        // Lua output must not reset this editor's liveness counter. — KESTREL
        KILO_SILENT_KEYS.store(0, Ordering::Relaxed);
        true
    } else {
        end_kilo_session(true);
        false
    }
}

/// Forward one raw keystroke to the editor and render what it draws in response.
///
/// Deliberately byte-at-a-time and unedited: `kilo` put its terminal in noncanonical mode
/// through its own ttyd control capability (J512), so the shell's line editor must not
/// stand between the key and the program. Returns false when the session has ended.
fn kilo_session_key(byte: u8) -> bool {
    let stdin = Handle(KILO_STDIN_WRITE.load(Ordering::Relaxed) as u32);
    let stdout = Handle(KILO_STDOUT_READ.load(Ordering::Relaxed) as u32);
    if stdin.0 == 0 || stdout.0 == 0 {
        return false;
    }
    let _ = stdout;
    if byte == KILO_ESCAPE_KEY {
        const MESSAGE: &[u8] = b"\r\nkilo: ended by Ctrl-\\\r\n";
        debug_write(MESSAGE.as_ptr(), MESSAGE.len());
        end_kilo_session(true);
        return false;
    }
    if channel_write(stdin, &byte, 1) != Errno::Ok.status() {
        // Its stdin's read end is gone: the editor exited.
        end_kilo_session(false);
        return false;
    }
    if KILO_SILENT_KEYS.fetch_add(1, Ordering::Relaxed) + 1 >= KILO_SILENT_KEY_LIMIT {
        const MESSAGE: &[u8] = b"\r\nkilo: not responding; ended\r\n";
        debug_write(MESSAGE.as_ptr(), MESSAGE.len());
        end_kilo_session(true);
        return false;
    }
    true
}

/// Start `kilo` on an optional file.
///
/// It receives stdin, stdout, its argv, a filesystem channel, and a private ttyd control
/// endpoint — and nothing else. No initrd handle, no resource, no device: a text editor
/// has no business with any of them.
fn start_kilo_session(initrd: Handle, cmd: &kumoza::Command) -> bool {
    if kilo_session_active() {
        debug_write(b"kilo: already running\n".as_ptr(), 22);
        return true;
    }
    if cmd.args.len() > 1 {
        usage(b"usage: kilo [path]\n");
        return true;
    }
    let mut slots: [&[u8]; MAX_ARGV] = [b""; MAX_ARGV];
    let count = command_argv_slots(cmd, &mut slots);
    let mut fs = FsGrant::Image;
    if let Some(path) = cmd.args.first() {
        if let Some(path) = sora::scratch_path(path.as_bytes()) {
            if path == b"/" {
                log(b"kilo: is a directory\n");
                return true;
            }
            slots[1] = path;
        } else {
            match sora::media_route(path.as_bytes()) {
                sora::MediaRoute::Elsewhere => {
                    log(b"kilo: root filesystem is read-only; use /tmp or /media/<volume>\n");
                    return true;
                }
                sora::MediaRoute::Root | sora::MediaRoute::Volume { path: b"/", .. } => {
                    log(b"kilo: is a directory\n");
                    return true;
                }
                sora::MediaRoute::Volume { name, path } => {
                    let Some(volume) = media().filter(|volume| volume.name.as_bytes() == name)
                    else {
                        log(b"kilo: no such volume\n");
                        return true;
                    };
                    if volume.ready.read_only {
                        log(b"kilo: volume is read-only\n");
                        return true;
                    }
                    // The editor sees only its granted volume and a path from that volume's root.
                    // — KESTREL
                    slots[1] = path;
                    fs = FsGrant::Media(BlockAccess::ReadWrite);
                }
            }
        }
    }
    let Some(child) = run_with_startup_input(
        initrd,
        KILO_PATH.as_bytes(),
        b"kilo",
        Some(&slots[..count]),
        false,
        fs,
        true,
        true,
        StdinMode::Interactive,
    ) else {
        debug_write(b"kilo: session unavailable\n".as_ptr(), 26);
        return false;
    };
    KILO_SILENT_KEYS.store(0, Ordering::Relaxed);
    KILO_PROCESS.store(child.process.map_or(0, |p| p.0 as u64), Ordering::Relaxed);
    KILO_STDIN_WRITE.store(child.stdin.0 as u64, Ordering::Relaxed);
    KILO_STDOUT_READ.store(child.stdout.0 as u64, Ordering::Relaxed);
    debug_write(
        sora::CONSOLE_SESSION_RESET.as_ptr(),
        sora::CONSOLE_SESSION_RESET.len(),
    );
    // The editor queries the window size and paints its first screen before reading a key,
    // so this relay both renders the opening frame and proves the child is serving.
    // Stdout was bound to the serve port before the child was admitted, so its first screen
    // — or its exit — arrives as an ordinary port event. Sora must NOT block-read it here:
    // `channel_read` parks the user thread, and a child's write signals the port but never
    // calls `wake_user`, which is the J510 hazard one layer up.
    true
}

/// Hand one submitted line to the live session and render its single reply. Returns false when the
/// session has ended, so the caller restores the shell prompt.
fn lua_session_submit(line: &[u8]) -> bool {
    let stdin = Handle(LUA_STDIN_WRITE.load(Ordering::Relaxed) as u32);
    let stdout = Handle(LUA_STDOUT_READ.load(Ordering::Relaxed) as u32);
    if stdin.0 == 0 || stdout.0 == 0 {
        return false;
    }
    if channel_write(stdin, line.as_ptr(), line.len()) != Errno::Ok.status() {
        end_lua_session();
        return false;
    }
    lua_relay_reply(stdout)
}

/// Write a decimal number to the console. `log_hex` exists but a region count in hex reads badly.
fn log_dec(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    debug_write(buf[i..].as_ptr(), buf.len() - i);
}

/// Maximum argv entries Sora forwards (program name + 15 args), bounding the heap-free slot array.
const MAX_ARGV: usize = 16;

/// Pack `slots` (each element one argv entry, `slots[0]` = program name) into the shared scratch
/// VMO and mint a one-shot read+transfer handle suitable for a bootstrap startup message; returns
/// `0` if the VMO is unavailable or argv overflows the 256-byte buffer. The child `vmo_read`s the
/// handle and walks it with `kumo_abi::unpack_argv`.
fn argv_transfer_handle(slots: &[&[u8]]) -> u64 {
    let Some(vmo) = write_argv(slots) else {
        return 0;
    };
    handle_duplicate(vmo, Rights::READ | Rights::TRANSFER)
}

/// Fill `slots` from a parsed command (`argv[0]` = the command name) and return the entry count.
fn command_argv_slots<'a>(cmd: &'a kumoza::Command, slots: &mut [&'a [u8]; MAX_ARGV]) -> usize {
    slots[0] = cmd.name.as_bytes();
    let mut count = 1;
    for arg in cmd.args.iter().take(MAX_ARGV - 1) {
        slots[count] = arg.as_bytes();
        count += 1;
    }
    count
}

/// Fill `slots` from a generic `run` argv (`argv[0]` already the program name) and return the count.
fn string_argv_slots<'a>(
    argv: &'a [alloc::string::String],
    slots: &mut [&'a [u8]; MAX_ARGV],
) -> usize {
    let mut count = 0;
    for arg in argv.iter().take(MAX_ARGV) {
        slots[count] = arg.as_bytes();
        count += 1;
    }
    count
}

fn write_argv(argv: &[&[u8]]) -> Option<Handle> {
    let vmo = ARGV_VMO.load(Ordering::Relaxed);
    if vmo == 0 || vmo == u64::MAX {
        return None;
    }
    let mut buf = [0u8; 256];
    let written = kumo_abi::pack_argv(argv, &mut buf)?;
    // VmoWrite is capped at 256/call; `written <= 256`, so one write suffices. The VMO is
    // reused across runs — a partial write leaves stale tail bytes, but `unpack_argv`
    // bounds its walk by the fresh `argc`, so they are never read.
    if vmo_write(Handle(vmo as u32), 0, buf.as_ptr(), written) != 0 {
        return None;
    }
    Some(Handle(vmo as u32))
}

/// Run an initrd program: spawn `bin/<argv[0]>` to completion via the same finite bootstrap
/// startup message every program now receives. The child gets stdout + a one-shot read-only argv
/// VMO; it is **not** granted the initrd, so a plain `run` stays authority-free (the `cat`/`wc`/
/// `ls` builtins are the capability-granting launchers). The `bin/` prefix is built into a fixed
/// buffer (no heap). Returns whether the spawn was attempted (false also logs the reason).
fn run_program(initrd: Handle, argv: &[alloc::string::String]) -> bool {
    let name = match argv.first() {
        Some(n) => n.as_bytes(),
        None => return false,
    };
    const PREFIX: &[u8] = b"bin/";
    let mut path = [0u8; 64];
    if PREFIX.len() + name.len() > path.len() {
        debug_write(b"run: name too long\n".as_ptr(), 19);
        return false;
    }
    path[..PREFIX.len()].copy_from_slice(PREFIX);
    path[PREFIX.len()..PREFIX.len() + name.len()].copy_from_slice(name);
    let full = &path[..PREFIX.len() + name.len()];
    let mut slots: [&[u8]; MAX_ARGV] = [b""; MAX_ARGV];
    let count = string_argv_slots(argv, &mut slots);
    if name == b"lua-repl" {
        if argv.len() == 1 {
            usage(b"usage: run lua-repl <expression>\n");
            return false;
        }
        let mut input = [0u8; 256];
        let mut input_len = 0usize;
        for arg in &argv[1..] {
            let bytes = arg.as_bytes();
            let separator = usize::from(input_len != 0);
            if input_len + separator + bytes.len() + 1 > input.len() {
                debug_write(b"lua-repl: input too long\n".as_ptr(), 25);
                return false;
            }
            if separator != 0 {
                input[input_len] = b' ';
                input_len += 1;
            }
            input[input_len..input_len + bytes.len()].copy_from_slice(bytes);
            input_len += bytes.len();
        }
        input[input_len] = b'\n';
        input_len += 1;
        run_with_startup_input(
            initrd,
            full,
            full,
            Some(&slots[..count]),
            false,
            FsGrant::None,
            false,
            false,
            StdinMode::Finite(&input[..input_len]),
        );
    } else {
        run_with_startup(initrd, full, full, Some(&slots[..count]), false);
    }
    true
}

/// Load an ELF into a fresh address space and start it.
///
/// `extra_threads` starts additional residents **in the same process**: each gets its own stack and
/// receives its index in `x0`, while sharing one image, one `ttbr0` and one set of statics. That is
/// what makes them threads rather than processes — `ResidentChild` already keeps `process` separate
/// from its thread, so two residents naming the same process koid *are* two threads, and
/// `ProcessRun`'s ASYNC arm resolves that identity from the handle. No kernel change was needed.
/// Every existing caller passes 0 and is unaffected.
#[allow(clippy::too_many_arguments)]
fn run_elf_threads(
    initrd: Handle,
    path: &[u8],
    arg: u64,
    arg2: u64,
    flags: u64,
    name: &[u8],
    extra_threads: u32,
    kept: Option<&mut Option<Handle>>,
) -> bool {
    const PAGE_SIZE: u64 = 4096;
    const MAX_SEGMENTS: usize = 8;

    fn align_down(value: u64) -> u64 {
        value & !(PAGE_SIZE - 1)
    }

    fn align_up(value: u64) -> Option<u64> {
        value
            .checked_add(PAGE_SIZE - 1)
            .map(|value| value & !(PAGE_SIZE - 1))
    }

    fn page_flags(elf_flags: u32) -> u64 {
        let mut flags = 0u64;
        if elf_flags & linux_elf::PF_R != 0 {
            flags |= 1;
        }
        if elf_flags & linux_elf::PF_W != 0 {
            flags |= 2;
        }
        if elf_flags & linux_elf::PF_X != 0 {
            flags |= 4;
        }
        flags
    }

    let (elf_off, elf_len) = match find_initrd_file(initrd, path) {
        Some(file) => file,
        None => {
            log(name);
            log(b": missing\n");
            fail_status(127);
            return false;
        }
    };

    let mut header = [0u8; linux_elf::ELF_HEADER_LEN];
    if vmo_read(initrd, elf_off, header.as_mut_ptr(), header.len()) != 0 {
        log(name);
        log(b": hdr read fail\n");
        return false;
    }
    let elf = match linux_elf::parse_header(&header) {
        Ok(elf) => elf,
        Err(_) => {
            log(name);
            log(b": bad hdr\n");
            return false;
        }
    };

    let ph_table_len = (elf.phnum as u64).saturating_mul(elf.phentsize as u64);
    if elf.phoff.saturating_add(ph_table_len) > elf_len || elf.phnum as usize > MAX_SEGMENTS {
        log(name);
        log(b": ph range fail\n");
        return false;
    }

    let empty = PersonaLoadSegment {
        file_offset: 0,
        file_size: 0,
        virt_addr: 0,
        mem_size: 0,
        flags: 0,
    };
    let mut segments = [empty; MAX_SEGMENTS];
    let mut segment_count = 0usize;
    let mut vmo_len = 0u64;

    for index in 0..elf.phnum as usize {
        let ph_off = elf_off + elf.phoff + (index as u64) * (elf.phentsize as u64);
        let mut ph_buf = [0u8; linux_elf::ELF_PHDR_LEN];
        if vmo_read(initrd, ph_off, ph_buf.as_mut_ptr(), ph_buf.len()) != 0 {
            log(name);
            log(b": ph read fail\n");
            return false;
        }
        let ph = match linux_elf::parse_program_header(&ph_buf) {
            Ok(ph) => ph,
            Err(_) => {
                log(name);
                log(b": bad ph\n");
                return false;
            }
        };
        if ph.kind != linux_elf::PT_LOAD {
            continue;
        }
        if segment_count >= MAX_SEGMENTS
            || ph.file_offset.saturating_add(ph.file_size) > elf_len
            || ph.file_offset.saturating_add(ph.mem_size) < ph.file_offset
        {
            log(name);
            log(b": segment range fail\n");
            return false;
        }
        vmo_len = vmo_len.max(ph.file_offset.saturating_add(ph.mem_size));
        segments[segment_count] = PersonaLoadSegment {
            file_offset: ph.file_offset,
            file_size: ph.file_size,
            virt_addr: ph.virt_addr,
            mem_size: ph.mem_size,
            flags: page_flags(ph.flags),
        };
        segment_count += 1;
    }

    if segment_count == 0 {
        log(name);
        log(b": no load\n");
        return false;
    }
    let vmo_len = match align_up(vmo_len) {
        Some(len) if len > 0 => len,
        _ => {
            log(name);
            log(b": size fail\n");
            return false;
        }
    };

    let child_vmo_h = vmo_create(vmo_len);
    if child_vmo_h == u64::MAX {
        log(name);
        log(b": vmo fail\n");
        return false;
    }
    let child_vmo = Handle(child_vmo_h as u32);

    // From the first successful allocation onward, all exits break through the one
    // cleanup point below. Slot 0 owns the loader VMO; slot 1 begins empty and takes
    // ownership of the process handle if ProcessCreate succeeds.
    let mut loader_handles = [Some(child_vmo), None];
    let loaded = 'load: {
        let mut chunk = [0u8; 256];
        for segment in segments.iter().take(segment_count) {
            let mut copied = 0u64;
            while copied < segment.file_size {
                let n = ((segment.file_size - copied) as usize).min(chunk.len());
                if vmo_read(
                    initrd,
                    elf_off + segment.file_offset + copied,
                    chunk.as_mut_ptr(),
                    n,
                ) != 0
                    || vmo_write(child_vmo, segment.file_offset + copied, chunk.as_ptr(), n) != 0
                {
                    log(name);
                    log(b": copy fail\n");
                    break 'load false;
                }
                copied += n as u64;
            }
        }

        let child_as_h = process_create(0, 0x2000_0000);
        if child_as_h == u64::MAX {
            log(name);
            log(b": process fail\n");
            break 'load false;
        }
        let child_as = Handle(child_as_h as u32);
        loader_handles[1] = Some(child_as);
        log_process_koid(name, child_as);

        for segment in segments.iter().take(segment_count) {
            let page_delta = segment.virt_addr & (PAGE_SIZE - 1);
            let virt = align_down(segment.virt_addr);
            let vmo_offset = align_down(segment.file_offset);
            let len = match align_up(page_delta.saturating_add(segment.mem_size)) {
                Some(len) => len,
                None => {
                    log(name);
                    log(b": map len fail\n");
                    break 'load false;
                }
            };
            if vmar_map(child_as, child_vmo, vmo_offset, virt, len, segment.flags) != 0 {
                log(name);
                log(b": map fail\n");
                break 'load false;
            }
        }

        // Give every thread a disjoint 16 KiB slice, 64 KiB apart, inside one stack arena.
        // `AddressSpaceCreate` currently owns one stack range per process and rebuilding it
        // replaces the prior user page table, so calling it once per peer would leave only the
        // last peer's stack mapped. Build the whole arena in one call instead. The unused space
        // between slices is deliberate headroom; the canaries in `threads` prove that the active
        // slices remain independent across context switches.
        //
        // `AddressSpaceCreate` takes the arena **top** and maps downward. Thread zero's slice ends
        // at `STACK_TOP`; each peer's slice ends one stride above that.
        // — CORVUS / KESTREL 2026-07-26
        const STACK_TOP: u64 = 0x1001_0000;
        const STACK_STRIDE: u64 = 0x1_0000;
        const STACK_SIZE: u64 = 0x4000;
        let arena_top = STACK_TOP + u64::from(extra_threads) * STACK_STRIDE;
        let arena_size = arena_top - (STACK_TOP - STACK_SIZE);
        if address_space_create(child_as, arena_top, arena_size) == u64::MAX {
            log(name);
            log(b": as fail\n");
            break 'load false;
        }

        let sp = |i: u64| STACK_TOP + i * STACK_STRIDE - 0x10;
        if process_run(child_as, elf.entry, sp(0), arg, arg2, flags) != 0 {
            break 'load false;
        }
        // Peers enter the same code with their index in x0. ASYNC because a second thread that
        // blocked its launcher would defeat the point.
        let mut ok = true;
        for i in 1..=u64::from(extra_threads) {
            if process_run(
                child_as,
                elf.entry,
                sp(i),
                i,
                0,
                ProcessRunFlags::ASYNC.bits(),
            ) != 0
            {
                log(name);
                log(b": peer thread fail\n");
                ok = false;
                break;
            }
        }
        ok
    };

    // `VmarMap`/`AddressSpaceCreate` copied everything an admitted child needs
    // into its process record. Sora does not supervise this path by handle, so
    // both success and every post-allocation failure surrender all acquired handles,
    // except the process handle of an admitted child whose caller asked to keep it: an
    // interactive session needs it to end a child that stops answering (J556).
    if loaded {
        if let Some(kept) = kept {
            *kept = loader_handles[1].take();
        }
    }
    let cleanup_ok = sora::close_handles(&loader_handles, handle_close);
    if !cleanup_ok {
        log(name);
        log(b": handle cleanup fail\n");
    }
    loaded && cleanup_ok
}

fn run_elf(initrd: Handle, path: &[u8], arg: u64, arg2: u64, flags: u64, name: &[u8]) -> bool {
    run_elf_threads(initrd, path, arg, arg2, flags, name, 0, None)
}

/// Stage-C milestone (Journal 134): two `svc-health` servers run as **independent**
/// resident processes at the same time. Each binds its own request channel to its own port
/// and parks in `PortWait`; the kernel's per-thread wait queue routes each client write to
/// exactly the server that owns that port, so the two never cross-wake. Independence is
/// proved by each server's own `served` counter reaching 2 from its own Ping + Status —
/// neither sees the other's traffic. (This subsumes the former single-resident smoke.)
/// A server's **construction recipe** (DESIGN/002 §5.6): everything Sora needs to (re)build
/// the server from scratch. For `svc-health` — a *stateless* recovery-class server — that is
/// just where to find its ELF; a fresh instance gets a fresh address space and channel, and
/// its advisory counters reset. Respawning after a crash/stop is simply calling
/// [`spawn_from_recipe`] again.
struct SvcRecipe {
    initrd: Handle,
    path: &'static str,
}

/// Spawn one fresh server instance from its recipe: reload the ELF into a new address space,
/// grant it a fresh request channel, and start it as a resident (async) process. The server
/// endpoint is moved into the child; Sora retains only the process handle for supervision
/// and the client endpoint for RPC. The caller pumps (`process_wait`) to let the server reach
/// `PortWait`.
fn spawn_from_recipe(recipe: &SvcRecipe) -> Option<sora::SupervisedServer> {
    let (child_as, entry, stack_top, server_chan, client_chan) =
        run_initrd_elf_with_channel(recipe.initrd, recipe.path.as_bytes())?;
    let flags = (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG).bits();
    if process_run(child_as, entry, stack_top, server_chan.0 as u64, 0, flags) != 0 {
        if !sora::close_handles(
            &[Some(child_as), Some(server_chan), Some(client_chan)],
            handle_close,
        ) {
            log(b"spawn: failure cleanup fail\n");
        }
        return None;
    }
    if handle_koid(server_chan) != u64::MAX {
        log(b"spawn: bootstrap transfer fail\n");
        return None;
    }
    diag(b"spawn: bootstrap transfer ok\n");
    Some(sora::SupervisedServer {
        process: child_as,
        client: client_chan,
    })
}

fn run_svc_health_pair_smoke(initrd: Handle) -> bool {
    let recipe = SvcRecipe {
        initrd,
        path: SVC_HEALTH_PATH,
    };
    // Spawn both as resident (async) servers; one pump drains both to their own PortWait.
    let cli1 = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"svc-pair: run1 fail\n");
            return false;
        }
    };
    let cli2 = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"svc-pair: run2 fail\n");
            return false;
        }
    };
    if !child_parked(process_wait()) {
        log(b"svc-pair: park fail\n");
        return false;
    }

    // Ping each resident on its own channel: each must reply Pong while the other stays
    // asleep (no cross-wake).
    if request_reply(cli1, HealthRequest::Ping) != Some(HealthResponse::Pong) {
        log(b"svc-pair: ping1 fail\n");
        return false;
    }
    if request_reply(cli2, HealthRequest::Ping) != Some(HealthResponse::Pong) {
        log(b"svc-pair: ping2 fail\n");
        return false;
    }

    // Status each: served == 2 (its own Ping + Status) proves separate per-process state —
    // neither counted the other's Ping.
    let want = Some(HealthResponse::Status {
        uptime_ticks: 0,
        served: 2,
    });
    if request_reply(cli1, HealthRequest::Status) != want {
        log(b"svc-pair: status1 fail\n");
        return false;
    }
    if request_reply(cli2, HealthRequest::Status) != want {
        log(b"svc-pair: status2 fail\n");
        return false;
    }

    // Termination + detection (Journal 135): shut down resident #1. Its serve loop exits
    // and it process-exits; the harness reaps it. Resident #2 must keep serving — proving
    // an *independent lifecycle*, not just independent wakeups — so ProcessWait still
    // reports a resident remaining.
    if !child_parked(shutdown_resident(cli1)) {
        log(b"svc-pair: shutdown1 fail\n");
        return false;
    }
    if request_reply(cli2, HealthRequest::Ping) != Some(HealthResponse::Pong) {
        log(b"svc-pair: survivor fail\n");
        return false;
    }
    // Shut down the survivor too; persistent background services keep ProcessWait
    // from returning Ok (ShouldWait means the child parked/exited, which is correct).
    if !child_parked(shutdown_resident(cli2)) {
        log(b"svc-pair: shutdown2 fail\n");
        return false;
    }
    true
}

/// First **supervised restart** (Journal 136, DESIGN/002): a server dies and Sora rebuilds
/// it from its recipe. Instance A serves until its counter reaches 2, then is shut down and
/// reaped. Sora respawns from the *same* recipe; the fresh instance B must serve, and its
/// first `Status` reports `served: 1` — not 3 — proving it is a **new process with reset
/// state** (the stateless recovery class), not a revived A.
fn run_svc_health_restart_smoke(initrd: Handle) -> bool {
    let recipe = SvcRecipe {
        initrd,
        path: SVC_HEALTH_PATH,
    };

    // Instance A: serve Ping + Status (counter climbs to 2), then "die" via Shutdown.
    let a = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"svc-restart: a spawn fail\n");
            return false;
        }
    };
    if !child_parked(process_wait()) {
        log(b"svc-restart: a park fail\n");
        return false;
    }
    if request_reply(a, HealthRequest::Ping) != Some(HealthResponse::Pong) {
        log(b"svc-restart: a ping fail\n");
        return false;
    }
    if request_reply(a, HealthRequest::Status)
        != Some(HealthResponse::Status {
            uptime_ticks: 0,
            served: 2,
        })
    {
        log(b"svc-restart: a status fail\n");
        return false;
    }
    if !child_parked(shutdown_resident(a)) {
        log(b"svc-restart: a shutdown fail\n");
        return false;
    }

    // Supervised restart from the same recipe. Instance B is a fresh process: its first
    // Status reports served:1 (not 3), proving reset state — the restart actually worked.
    let b = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"svc-restart: respawn fail\n");
            return false;
        }
    };
    if !child_parked(process_wait()) {
        log(b"svc-restart: b park fail\n");
        return false;
    }
    if request_reply(b, HealthRequest::Status)
        != Some(HealthResponse::Status {
            uptime_ticks: 0,
            served: 1,
        })
    {
        log(b"svc-restart: b state fail\n");
        return false;
    }
    if !child_parked(shutdown_resident(b)) {
        log(b"svc-restart: b shutdown fail\n");
        return false;
    }
    true
}

/// §5.6 crash containment (Journal 137): a child that *faults* must be terminated by the
/// kernel — never halt it. Spawn a child at a bogus (unmapped) entry so it instruction-aborts
/// on its first fetch. If the kernel contains the fault and reaps the child, Sora keeps
/// running and `ProcessWait` reports no resident remaining; before this slice the boot would
/// have stopped at the Tower ("Ziwei seizes the wheel") and never reached this line.
fn run_crash_containment_smoke(initrd: Handle) -> bool {
    // Build a valid child address space + stack + channel, then run it at an unmapped entry.
    let (child_as, _entry, stack_top, server_chan, _client) =
        match run_initrd_elf_with_channel(initrd, SVC_HEALTH_PATH.as_bytes()) {
            Some(t) => t,
            None => return false,
        };
    const BOGUS_ENTRY: u64 = 0xDEAD_0000; // unmapped: instruction abort on first fetch
    if process_run(child_as, BOGUS_ENTRY, stack_top, server_chan.0 as u64, 0, 1) != 0 {
        log(b"svc-crash: run fail\n");
        return false;
    }
    // The child faults immediately; the kernel contains it and reaps it. Reaching here at all
    // proves the kernel survived; `Ok` proves the crashed child was reaped (no resident left).
    all_residents_gone(process_wait())
}

/// Send one request to a resident server on `client`, pump the scheduler so that server
/// wakes, serves, and re-parks, then read and decode its reply.
fn request_reply(client: sora::SupervisedServer, request: HealthRequest) -> Option<HealthResponse> {
    let bytes = request.encode();
    if channel_write(client.client, bytes.as_ptr(), bytes.len()) != 0 {
        return None;
    }
    if !child_parked(process_wait()) {
        return None;
    }
    let mut reply = [0u8; 32];
    let n = channel_read(client.client, reply.as_mut_ptr(), reply.len()) as usize;
    HealthResponse::decode(&reply[..n])
}

/// Send `Shutdown` to a resident on `client` and pump: the server's serve loop exits and it
/// process-exits, so the harness reaps it. Returns the `ProcessWait` status — `ShouldWait`
/// while another resident remains, `Ok` once none do.
fn shutdown_resident(client: sora::SupervisedServer) -> u64 {
    let bytes = HealthRequest::Shutdown.encode();
    let _ = channel_write(client.client, bytes.as_ptr(), bytes.len());
    process_wait()
}

fn spawn_ttyd_session(
    initrd: Handle,
    recipe: sora::ServerRecipe<'static>,
) -> Option<sora::SupervisedService<'static>> {
    let spawn_recipe = SvcRecipe {
        initrd,
        path: recipe.image_path,
    };
    let instance = spawn_from_recipe(&spawn_recipe)?;
    if !child_parked(process_wait()) {
        log(b"ttyd: interactive park fail\n");
        return None;
    }
    log(b"ttyd: interactive ok\n");
    notice(&[b"ttyd: online, console line discipline\n"]);
    Some(sora::SupervisedService { recipe, instance })
}

fn dispatch_ttyd_key(
    tty: Handle,
    byte: u8,
    reply: &mut [u8],
    initrd: Handle,
    prog_initrd: u64,
    root: Handle,
    cwd: &mut WorkingDirectory,
) -> bool {
    if session().is_none() && LOGIN.get().stage() == sora::LoginStage::Paused {
        return true;
    }
    let Some(n) = ttyd_request_reply(tty, TtyRequest::input(byte), reply) else {
        log(b"ttyd: interactive input fail\n");
        return false;
    };
    let Some(parsed) = TtyReply::parse(&reply[..n]) else {
        log(b"ttyd: interactive parse fail\n");
        return false;
    };
    if parsed.status != ttyd::TTY_OK && parsed.status != ttyd::TTY_OVERFLOW {
        log(b"ttyd: interactive status fail\n");
        return false;
    }
    if !parsed.echo.is_empty() {
        debug_write(parsed.echo.as_ptr(), parsed.echo.len());
    }
    let Some(line) = parsed.line else {
        return true;
    };
    if session().is_none() {
        login_line(tty, line, initrd, cwd);
        // The line may have been a password.
        reply.fill(0);
        return true;
    }
    // While a Lua session is live it owns every submitted line — including ones that look like
    // shell commands, which in Lua they are not. Its reply carries its own prompt, so the shell
    // prompt returns only once the child is gone.
    if lua_session_active() {
        if lua_session_submit(line) {
            return true;
        }
        write_shell_prompt(cwd);
        return true;
    }
    if let Ok(ls) = core::str::from_utf8(line) {
        if let Some(stmt) = parse(ls) {
            run_statement(&stmt, line, initrd, prog_initrd, root, cwd);
        }
    }
    if session().is_none() {
        // That line was `logout` or `exit`.
        forget_console_history(tty);
    }
    // A line was submitted (Enter): the command (if any) has finished writing its output, so render
    // the next prompt. Command output ends in a newline, so the prompt lands on a fresh line; an
    // empty line just re-prompts after ttyd's CRLF echo. This is the loop that makes it a shell.
    // An interactive program owns the screen until its session ends. — KESTREL
    if !lua_session_active() && !kilo_session_active() {
        write_shell_prompt(cwd);
    }
    true
}

/// First live P8 `ttyd` proof: spawn a private `ttyd` server, send `h`, `i`, Enter,
/// and require the submitted line `hi` to come back over the request channel. This keeps
/// Sora's existing interactive path untouched while proving the real server binary/loop.
fn run_ttyd_smoke(initrd: Handle) -> bool {
    let recipe = SvcRecipe {
        initrd,
        path: TTYD_PATH,
    };
    let client = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"ttyd: spawn fail\n");
            return false;
        }
    };
    if handle_koid(client.process) == u64::MAX {
        log(b"ttyd: process handle fail\n");
        return false;
    }
    log(b"ttyd: process handle ok\n");
    if !child_parked(process_wait()) {
        log(b"ttyd: park fail\n");
        return false;
    }

    let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
    let Some(n) = ttyd_request_reply(client.client, TtyRequest::input(b'x'), &mut reply) else {
        log(b"ttyd: cancel seed fail\n");
        return false;
    };
    let Some(parsed) = TtyReply::parse(&reply[..n]) else {
        log(b"ttyd: cancel seed parse fail\n");
        return false;
    };
    if parsed.status != ttyd::TTY_OK || parsed.echo != b"x" || parsed.line.is_some() {
        log(b"ttyd: cancel seed reply fail\n");
        return false;
    }

    let Some(n) = ttyd_request_reply(client.client, TtyRequest::input(0x03), &mut reply) else {
        log(b"ttyd: ctrl-c fail\n");
        return false;
    };
    let Some(parsed) = TtyReply::parse(&reply[..n]) else {
        log(b"ttyd: ctrl-c parse fail\n");
        return false;
    };
    if parsed.status != ttyd::TTY_OK || parsed.echo != b"^C\r\n" || parsed.line.is_some() {
        log(b"ttyd: ctrl-c reply fail\n");
        return false;
    }

    let Some(n) = ttyd_request_reply(client.client, TtyRequest::input(b'h'), &mut reply) else {
        log(b"ttyd: h fail\n");
        return false;
    };
    let Some(parsed) = TtyReply::parse(&reply[..n]) else {
        log(b"ttyd: h parse fail\n");
        return false;
    };
    if parsed.status != ttyd::TTY_OK || parsed.echo != b"h" || parsed.line.is_some() {
        log(b"ttyd: h reply fail\n");
        return false;
    }

    let Some(n) = ttyd_request_reply(client.client, TtyRequest::input(b'i'), &mut reply) else {
        log(b"ttyd: i fail\n");
        return false;
    };
    let Some(parsed) = TtyReply::parse(&reply[..n]) else {
        log(b"ttyd: i parse fail\n");
        return false;
    };
    if parsed.status != ttyd::TTY_OK || parsed.echo != b"i" || parsed.line.is_some() {
        log(b"ttyd: i reply fail\n");
        return false;
    }

    let Some(n) = ttyd_request_reply(client.client, TtyRequest::input(b'\n'), &mut reply) else {
        log(b"ttyd: enter fail\n");
        return false;
    };
    let Some(parsed) = TtyReply::parse(&reply[..n]) else {
        log(b"ttyd: enter parse fail\n");
        return false;
    };
    if parsed.status != ttyd::TTY_OK || parsed.echo != b"\r\n" || parsed.line != Some(&b"hi"[..]) {
        log(b"ttyd: line fail\n");
        return false;
    }

    let mut req = [0u8; ttyd::REQUEST_BUF_BYTES];
    let Some(req_len) = TtyRequest::shutdown().encode_into(&mut req) else {
        return false;
    };
    if channel_write(client.client, req.as_ptr(), req_len) != 0 {
        return false;
    }
    let status = process_wait();
    child_parked(status) || all_residents_gone(status)
}

struct ChannelTtyTransport {
    output: [u8; 16],
    len: usize,
}

impl ChannelTtyTransport {
    const fn new() -> ChannelTtyTransport {
        ChannelTtyTransport {
            output: [0; 16],
            len: 0,
        }
    }

    fn output(&self) -> &[u8] {
        &self.output[..self.len]
    }
}

impl TtyRpcTransport for ChannelTtyTransport {
    fn call(
        &mut self,
        stream: TtyStream,
        request: &[u8],
        reply: &mut [u8],
    ) -> persona_posix::PosixResult<usize> {
        let client = Handle(stream.handle);
        if channel_write(client, request.as_ptr(), request.len()) != 0 {
            return Err(persona_posix::PosixErrno::NoDevice);
        }
        if !child_parked(process_wait()) {
            return Err(persona_posix::PosixErrno::NoDevice);
        }
        let n = channel_read(client, reply.as_mut_ptr(), reply.len()) as usize;
        if let Some(parsed) = TtyReply::parse(&reply[..n]) {
            let room = self.output.len().saturating_sub(self.len);
            let take = parsed.echo.len().min(room);
            self.output[self.len..self.len + take].copy_from_slice(&parsed.echo[..take]);
            self.len += take;
        }
        Ok(n)
    }
}

fn run_persona_posix_tty_write_smoke(initrd: Handle) -> bool {
    let recipe = SvcRecipe {
        initrd,
        path: TTYD_PATH,
    };
    let client = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"persona-posix: tty spawn fail\n");
            return false;
        }
    };
    if !child_parked(process_wait()) {
        log(b"persona-posix: tty park fail\n");
        return false;
    }

    let mut table = FdTable::with_stdio(TtyStream {
        handle: client.client.0,
    });
    let mut tty = TtyRpc::new(ChannelTtyTransport::new());
    if table.write(STDOUT_FILENO, b"ok\n", &mut tty) != Ok(3) {
        log(b"persona-posix: tty write call fail\n");
        return false;
    }
    if tty.transport().output() != b"ok\n" {
        log(b"persona-posix: tty output fail\n");
        return false;
    }

    let mut req = [0u8; ttyd::REQUEST_BUF_BYTES];
    let Some(req_len) = TtyRequest::shutdown().encode_into(&mut req) else {
        return false;
    };
    if channel_write(client.client, req.as_ptr(), req_len) != 0 {
        return false;
    }
    let status = process_wait();
    child_parked(status) || all_residents_gone(status)
}

fn run_persona_posix_tty_read_smoke(initrd: Handle) -> bool {
    let recipe = SvcRecipe {
        initrd,
        path: TTYD_PATH,
    };
    let client = match spawn_from_recipe(&recipe) {
        Some(c) => c,
        None => {
            log(b"persona-posix: tty read spawn fail\n");
            return false;
        }
    };
    if !child_parked(process_wait()) {
        log(b"persona-posix: tty read park fail\n");
        return false;
    }

    let mut reply = [0u8; ttyd::REPLY_BUF_BYTES];
    for &byte in b"ok\n" {
        if ttyd_request_reply(client.client, TtyRequest::input(byte), &mut reply).is_none() {
            log(b"persona-posix: tty read feed fail\n");
            return false;
        }
    }

    let mut table = FdTable::with_stdio(TtyStream {
        handle: client.client.0,
    });
    let mut tty = TtyRpc::new(ChannelTtyTransport::new());
    let mut out = [0u8; 8];
    if table.read(STDIN_FILENO, &mut out, &mut tty) != Ok(2) || &out[..2] != b"ok" {
        log(b"persona-posix: tty read call fail\n");
        return false;
    }
    if table.read(STDIN_FILENO, &mut out, &mut tty) != Ok(0) {
        log(b"persona-posix: tty read drain fail\n");
        return false;
    }

    let mut req = [0u8; ttyd::REQUEST_BUF_BYTES];
    let Some(req_len) = TtyRequest::shutdown().encode_into(&mut req) else {
        return false;
    };
    if channel_write(client.client, req.as_ptr(), req_len) != 0 {
        return false;
    }
    let status = process_wait();
    child_parked(status) || all_residents_gone(status)
}

fn ttyd_request_reply(client: Handle, request: TtyRequest, reply: &mut [u8]) -> Option<usize> {
    let mut req = [0u8; ttyd::REQUEST_BUF_BYTES];
    let req_len = request.encode_into(&mut req)?;
    if channel_write(client, req.as_ptr(), req_len) != 0 {
        return None;
    }
    if !child_parked(process_wait()) {
        return None;
    }
    let n = channel_read(client, reply.as_mut_ptr(), reply.len()) as usize;
    Some(n)
}

fn child_parked(status: u64) -> bool {
    status == Errno::ShouldWait.status() as u32 as u64
}

fn all_residents_gone(status: u64) -> bool {
    status == Errno::Ok.status() as u32 as u64
}

fn run_initrd_elf_with_channel(
    initrd: Handle,
    path: &[u8],
) -> Option<(Handle, u64, u64, Handle, Handle)> {
    const PAGE_SIZE: u64 = 4096;
    const MAX_SEGMENTS: usize = 8;
    const STACK_TOP: u64 = 0x10010000;
    const STACK_SIZE: u64 = 0x4000;

    fn align_down(value: u64) -> u64 {
        value & !(PAGE_SIZE - 1)
    }

    fn align_up(value: u64) -> Option<u64> {
        value
            .checked_add(PAGE_SIZE - 1)
            .map(|value| value & !(PAGE_SIZE - 1))
    }

    fn page_flags(elf_flags: u32) -> u64 {
        let mut flags = 0u64;
        if elf_flags & linux_elf::PF_R != 0 {
            flags |= 1;
        }
        if elf_flags & linux_elf::PF_W != 0 {
            flags |= 2;
        }
        if elf_flags & linux_elf::PF_X != 0 {
            flags |= 4;
        }
        flags
    }

    let (elf_off, elf_len) = match find_initrd_file(initrd, path) {
        Some(file) => file,
        None => {
            log(b"svc-health: missing\n");
            return None;
        }
    };

    let mut header = [0u8; linux_elf::ELF_HEADER_LEN];
    if vmo_read(initrd, elf_off, header.as_mut_ptr(), header.len()) != 0 {
        log(b"svc-health: hdr read fail\n");
        return None;
    }
    let elf = match linux_elf::parse_header(&header) {
        Ok(elf) => elf,
        Err(_) => {
            log(b"svc-health: bad hdr\n");
            return None;
        }
    };

    let ph_table_len = (elf.phnum as u64).saturating_mul(elf.phentsize as u64);
    if elf.phoff.saturating_add(ph_table_len) > elf_len || elf.phnum as usize > MAX_SEGMENTS {
        log(b"svc-health: ph range fail\n");
        return None;
    }

    let empty = PersonaLoadSegment {
        file_offset: 0,
        file_size: 0,
        virt_addr: 0,
        mem_size: 0,
        flags: 0,
    };
    let mut segments = [empty; MAX_SEGMENTS];
    let mut segment_count = 0usize;
    let mut vmo_len = 0u64;

    for index in 0..elf.phnum as usize {
        let ph_off = elf_off + elf.phoff + (index as u64) * (elf.phentsize as u64);
        let mut ph_buf = [0u8; linux_elf::ELF_PHDR_LEN];
        if vmo_read(initrd, ph_off, ph_buf.as_mut_ptr(), ph_buf.len()) != 0 {
            log(b"svc-health: ph read fail\n");
            return None;
        }
        let ph = match linux_elf::parse_program_header(&ph_buf) {
            Ok(ph) => ph,
            Err(_) => {
                log(b"svc-health: bad ph\n");
                return None;
            }
        };
        if ph.kind != linux_elf::PT_LOAD {
            continue;
        }
        if segment_count >= MAX_SEGMENTS
            || ph.file_offset.saturating_add(ph.file_size) > elf_len
            || ph.file_offset.saturating_add(ph.mem_size) < ph.file_offset
        {
            log(b"svc-health: segment range fail\n");
            return None;
        }
        vmo_len = vmo_len.max(ph.file_offset.saturating_add(ph.mem_size));
        segments[segment_count] = PersonaLoadSegment {
            file_offset: ph.file_offset,
            file_size: ph.file_size,
            virt_addr: ph.virt_addr,
            mem_size: ph.mem_size,
            flags: page_flags(ph.flags),
        };
        segment_count += 1;
    }

    if segment_count == 0 {
        log(b"svc-health: no load\n");
        return None;
    }

    let child_vmo_h = vmo_create(align_up(vmo_len)?);
    if child_vmo_h == u64::MAX {
        log(b"svc-health: vmo fail\n");
        return None;
    }
    let child_vmo = Handle(child_vmo_h as u32);

    // The loader VMO is always temporary. The process and channel handles are
    // tracked as they are acquired, then preserved only when the full recipe is
    // returned to the supervisor. Every post-allocation failure keeps nothing.
    let mut loader_handles = [Some(child_vmo), None, None, None];
    let loaded = 'load: {
        let mut chunk = [0u8; 256];
        for segment in segments.iter().take(segment_count) {
            let mut copied = 0u64;
            while copied < segment.file_size {
                let n = ((segment.file_size - copied) as usize).min(chunk.len());
                if vmo_read(
                    initrd,
                    elf_off + segment.file_offset + copied,
                    chunk.as_mut_ptr(),
                    n,
                ) != 0
                    || vmo_write(child_vmo, segment.file_offset + copied, chunk.as_ptr(), n) != 0
                {
                    log(b"svc-health: copy fail\n");
                    break 'load None;
                }
                copied += n as u64;
            }
        }

        let child_as_h = process_create(0, 0x2000_0000);
        if child_as_h == u64::MAX {
            log(b"svc-health: process fail\n");
            break 'load None;
        }
        let child_as = Handle(child_as_h as u32);
        loader_handles[1] = Some(child_as);

        for segment in segments.iter().take(segment_count) {
            let page_delta = segment.virt_addr & (PAGE_SIZE - 1);
            let virt = align_down(segment.virt_addr);
            let vmo_offset = align_down(segment.file_offset);
            let len = match align_up(page_delta.saturating_add(segment.mem_size)) {
                Some(len) => len,
                None => {
                    log(b"svc-health: map len fail\n");
                    break 'load None;
                }
            };
            if vmar_map(child_as, child_vmo, vmo_offset, virt, len, segment.flags) != 0 {
                log(b"svc-health: map fail\n");
                break 'load None;
            }
        }

        if address_space_create(child_as, STACK_TOP, STACK_SIZE) == u64::MAX {
            log(b"svc-health: as fail\n");
            break 'load None;
        }

        let (client_h, server_h) = channel_create_pair();
        if client_h != u64::MAX {
            loader_handles[2] = Some(Handle(client_h as u32));
        }
        if server_h != u64::MAX {
            loader_handles[3] = Some(Handle(server_h as u32));
        }
        if client_h == u64::MAX || server_h == u64::MAX {
            log(b"svc-health: channel fail\n");
            break 'load None;
        }

        break 'load Some((
            child_as,
            elf.entry,
            STACK_TOP - 0x10,
            Handle(server_h as u32),
            Handle(client_h as u32),
        ));
    };

    let cleanup_ok = match loaded {
        Some((child_as, _, _, server, client)) => {
            sora::close_handles_except(&loader_handles, &[child_as, server, client], handle_close)
        }
        None => sora::close_handles(&loader_handles, handle_close),
    };
    if !cleanup_ok {
        log(b"svc-health: loader cleanup fail\n");
        return None;
    }
    loaded
}

/// Resolve a file path against the FAT32 root directory and return up to `req_len` bytes
/// starting from byte `file_off`. Writes into `out` and returns bytes written (0 = error).
fn serve_file_read_at(
    initrd: Handle,
    path: &[u8],
    file_off: u64,
    req_len: u64,
    out: &mut [u8; 512],
) -> usize {
    let target = match path_to_8_3(path) {
        Some(n) => n,
        None => return 0,
    };

    // Locate bin/fat32.img.
    let mut header = [0u8; 16];
    if vmo_read(initrd, 0, header.as_mut_ptr(), 16) != 0 {
        return 0;
    }
    let entry_count = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    let mut entry = [0u8; 80];
    let fat_path = b"bin/fat32.img";
    let mut fat_off = 0u64;
    for i in 0..entry_count {
        let off = 16u64 + (i as u64) * 80;
        if vmo_read(initrd, off, entry.as_mut_ptr(), 80) != 0 {
            break;
        }
        let plen = entry[..64].iter().position(|b| *b == 0).unwrap_or(64);
        if plen == fat_path.len() && entry[..plen] == fat_path[..] {
            fat_off = u64::from_le_bytes(entry[64..72].try_into().unwrap());
            break;
        }
    }
    if fat_off == 0 {
        return 0;
    }

    // Read BPB.
    let mut bpb = [0u8; 512];
    if vmo_read(initrd, fat_off, bpb.as_mut_ptr(), 512) != 0 {
        return 0;
    }
    let bps = u16::from_le_bytes(bpb[0x0B..0x0D].try_into().unwrap()) as u64;
    let spc = bpb[0x0D] as u64;
    let rsvd = u16::from_le_bytes(bpb[0x0E..0x10].try_into().unwrap()) as u64;
    let nfat = bpb[0x10] as u64;
    let spf = u32::from_le_bytes(bpb[0x24..0x28].try_into().unwrap()) as u64;
    let data_start = rsvd + nfat * spf;
    let root_sec = data_start;
    let root_off = fat_off + root_sec * bps;

    // Walk root directory.
    let mut dir = [0u8; 512];
    if vmo_read(initrd, root_off, dir.as_mut_ptr(), 512) != 0 {
        return 0;
    }
    let mut pos = 0;
    let mut file_cluster = 0u32;
    let mut file_size = 0u32;
    while pos + 32 <= 512 && dir[pos] != 0x00 {
        let attr = dir[pos + 11];
        if attr != 0x0F && attr != 0x08 && dir[pos..pos + 11] == target[..] {
            let c_hi = u16::from_le_bytes(dir[pos + 20..pos + 22].try_into().unwrap()) as u32;
            let c_lo = u16::from_le_bytes(dir[pos + 26..pos + 28].try_into().unwrap()) as u32;
            file_cluster = (c_hi << 16) | c_lo;
            file_size = u32::from_le_bytes(dir[pos + 28..pos + 32].try_into().unwrap());
            break;
        }
        pos += 32;
    }
    if file_cluster == 0 || file_off >= file_size as u64 {
        return 0;
    }

    let max_len = (req_len as usize)
        .min(out.len())
        .min((file_size as u64 - file_off) as usize);

    // Seek to file_off within the cluster chain.
    let fat_abs = fat_off + rsvd * bps;
    let mut cluster = file_cluster;
    let mut skip = file_off;
    let bytes_per_cluster = (spc * bps) as u64;
    while skip >= bytes_per_cluster && cluster >= 2 && cluster < 0x0FFFFFF8 {
        skip -= bytes_per_cluster;
        let fat_entry_off = cluster as u64 * 4;
        let fat_sec = (fat_entry_off / bps) as usize;
        let fat_idx = (fat_entry_off % bps) as usize;
        let mut fat_buf = [0u8; 512];
        if vmo_read(
            initrd,
            fat_abs + fat_sec as u64 * bps,
            fat_buf.as_mut_ptr(),
            512,
        ) != 0
        {
            return 0;
        }
        cluster = u32::from_le_bytes(fat_buf[fat_idx..fat_idx + 4].try_into().unwrap());
    }

    // Read clusters starting from `cluster` at offset `skip`.
    let mut written = 0usize;
    let mut fat_buf = [0u8; 512];
    while cluster >= 2 && cluster < 0x0FFFFFF8 && written < max_len {
        let sec = data_start + (cluster as u64 - 2) * spc;
        let abs_off = fat_off + sec * bps + skip;
        let to_read = ((bps as u64 - skip) as usize).min(max_len - written);
        if vmo_read(initrd, abs_off, out[written..].as_mut_ptr(), to_read) != 0 {
            break;
        }
        written += to_read;
        skip = 0;

        let fat_entry_off = cluster as u64 * 4;
        let fat_sec = (fat_entry_off / bps) as usize;
        let fat_idx = (fat_entry_off % bps) as usize;
        if vmo_read(
            initrd,
            fat_abs + fat_sec as u64 * bps,
            fat_buf.as_mut_ptr(),
            512,
        ) != 0
        {
            break;
        }
        cluster = u32::from_le_bytes(fat_buf[fat_idx..fat_idx + 4].try_into().unwrap());
        if cluster >= 0x0FFFFFF8 {
            cluster = 0;
        }
    }
    written
}

/// P7-j simple path read: entire file from offset 0 (backward compatible).
fn serve_file_read(initrd: Handle, path: &[u8], out: &mut [u8; 512]) -> usize {
    serve_file_read_at(initrd, path, 0, out.len() as u64, out)
}

fn launch_lua_repl(initrd: Handle) {
    debug_write(b"Launching Lua REPL...\n".as_ptr(), 22);

    // Exercise `print` through the same finite stdin transfer used by the interactive command,
    // while preserving the fixed returned `42` acceptance gate. — KESTREL
    run_with_startup_input(
        initrd,
        b"bin/lua-repl",
        b"lua-repl",
        None,
        false,
        FsGrant::None,
        false,
        false,
        StdinMode::Finite(
            b"local answer = math.floor(41.75) + 1; print('lua-print', answer); return answer\n",
        ),
    );
}
