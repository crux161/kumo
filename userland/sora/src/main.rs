#![no_std]
#![no_main]

//j820
//j822
//j824
//j825
//j829

extern crate alloc;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use drv_blk::BlockAccess;
use kumo_abi::{
    BootInfo, Errno, Framebuffer, Handle, ProcessRunFlags, Rights, VmarFlags, AUTOEXEC_PATH,
    BMP_VIEW_PATH, CAT_PATH, DRV_GPU_PATH, DRV_XHCI_PATH, FAT32_IMG_PATH, FATFS_PATH,
    GFX_CLIENT_PATH, GPU_CONTAIN_CLIENT_PATH, GPU_CONTAIN_SERVER_PATH, GPU_LANE_PATH, GROUP_PATH,
    HOSTNAME_PATH, IDENTITYD_PATH, KILO_PATH, LS_PATH, NETSTACK_PATH, PASSWD_PATH,
    PERSONA_LINUX_HELLO_PATH, RELIBC_SMOKE_PATH, SHADOW_PATH, SVC_HEALTH_PATH, TTYD_PATH, WC_PATH,
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
    resource_mint_mmio, syscall_handle, thread_create, thread_start, timer_create, vmar_map,
    vmo_create, vmo_read, vmo_write, STARTUP_TAG_ARGV, STARTUP_TAG_CAP0, STARTUP_TAG_FS,
    STARTUP_TAG_NET, STARTUP_TAG_STDIN, STARTUP_TAG_STDOUT, STARTUP_TAG_TTY,
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

/// Root Job MANAGE capability supplied in x10 and preserved before scratch use.
static ROOT_JOB_HANDLE: AtomicU64 = AtomicU64::new(0);

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

/// C3 slice-2: the six row hashes of the last presented log view; zero means "never
/// presented". The incremental view rewrites and damages only rows whose hash moved.
const LOG_ROWS: usize = 6;
const LOG_COLS: usize = 25;
const _: () = assert!(kumo_graphics::DEMO.width as usize / drv_fb::GLYPH_W == LOG_COLS);
const _: () = assert!(kumo_graphics::DEMO.height as usize >= LOG_ROWS * drv_fb::GLYPH_H);
/// C3 slice-4: per-view console row hashes; each console view owns its own state.
static LOG_ROW_HASH: [[AtomicU64; LOG_ROWS]; kumo_graphics::VIEW_COUNT] = [
    [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ],
    [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ],
];
/// Koid of each view's surface; a regranted view starts from a blank VMO.
static LOG_VMO_KOID: [AtomicU64; kumo_graphics::VIEW_COUNT] =
    [AtomicU64::new(0), AtomicU64::new(0)];
/// C3 slice-3: while set, the serve loop refreshes the console views on a cadence so the
/// surfaces follow their streams without anyone typing `gfx-log`.
static LOG_MIRROR: AtomicBool = AtomicBool::new(false);
/// C3 S3-a: the interactive terminal's stream, a single-writer byte ring feeding the
/// terminal view. Only shell output enters it — prompts, ttyd echo and command output via
/// `shell_write`; Sora's own notices never do. — TIMBERDOODLE
static SHELL_RING: [AtomicU8; 1024] = [const { AtomicU8::new(0) }; 1024];
static SHELL_RING_SEQ: AtomicU32 = AtomicU32::new(0);
/// C3 S3-b: while set, the terminal view owns the keyboard like a shell itself. Keys typed
/// while it holds focus go to ttyd even if an input consumer was admitted, and no consumer
/// may be admitted on it. Default off, so the F2 acceptance proof's two consumer views
/// stay valid. — TIMBERDOODLE
static TERMINAL_MODE: AtomicBool = AtomicBool::new(false);
const TERMINAL_VIEW: usize = 1;
/// Which stream each console view shows: view 0 the kernel log ring, view 1 the interactive
/// terminal's shell stream. — TIMBERDOODLE
const CONSOLE_SOURCES: [u8; kumo_graphics::VIEW_COUNT] = [0, 2];
const CONSOLE_KERNEL_RING: u8 = 0;

/// Bounded reply reads for the supervisor's demo presents. Sora's channel read parks once
/// per call and returns an empty read on any unrelated wake, and the production vblank gate
/// may hold a reply for two frame periods while it proves the frame start — so one attempt
/// can honestly miss. Each attempt survives exactly one unrelated wake; the reply logs land
/// with the reply itself, so the next wake after them carries it. — KINGFISHER
const REPLY_READ_ATTEMPTS: usize = 4;

/// Sora's write end of drv-fb's bootstrap channel, retained after startup so a released view
/// can be regranted with a fresh presentation channel. — TIMBERDOODLE
static DRV_FB_BOOTSTRAP: AtomicU32 = AtomicU32::new(0);
/// Views whose synchronous client exited (or whose taken grant failed), so a later
/// gfx-client/bmp-view may be given a fresh channel and surface for the same view.
/// — TIMBERDOODLE
static GRAPHICS_RELEASED: [AtomicBool; kumo_graphics::VIEW_COUNT] =
    [AtomicBool::new(false), AtomicBool::new(false)];
/// Views Sora may grant or regrant: the ones it kept at bootstrap. A view handed to
/// drv-gpu is never granted by Sora. — TIMBERDOODLE
static GRAPHICS_GRANTABLE: [AtomicBool; kumo_graphics::VIEW_COUNT] =
    [AtomicBool::new(false), AtomicBool::new(false)];
/// Views currently leased to an ASYNC client. A later synchronous client waits for these
/// to be reported released before regranting. — TIMBERDOODLE
static GRAPHICS_ASYNC: [AtomicBool; kumo_graphics::VIEW_COUNT] =
    [AtomicBool::new(false), AtomicBool::new(false)];
/// The retained process handle of each view's ASYNC client, or 0. It stays open until drv-fb
/// reports the view released, so a second wait can bind it again and the kernel replays the
/// retained exit. — TIMBERDOODLE
static GRAPHICS_ASYNC_PROCESS: [AtomicU32; kumo_graphics::VIEW_COUNT] =
    [AtomicU32::new(0), AtomicU32::new(0)];
/// The server end of each view's input channel (F2b focus routing), or 0. Created when a
/// `gfx-keys` consumer is admitted and retired when the consumer's channel closes.
/// — TIMBERDOODLE
static GRAPHICS_INPUT: [AtomicU32; kumo_graphics::VIEW_COUNT] =
    [AtomicU32::new(0), AtomicU32::new(0)];

/// G5b/G8d-b: the sending end of drv-gpu's bootstrap channel. On a board with the GPU's
/// power unit it is the driver's long-lived session handoff (DESIGN/024): the firmware
/// grant and the first session's `g` ride it at launch, the boot view's `v`/`s` — or a
/// bare `v` carrying no view — follow once drv-fb's grants exist, and every later
/// session's `g` reuses the same pair. Sora never closes it on a GPU board; on any other
/// board it was closed at spawn, so this slot stays zero and nothing waits on it.
/// — TIMBERDOODLE
static GPU_VIEW_BOOTSTRAP: AtomicU32 = AtomicU32::new(0);
//j714 The retained session handoff once the boot view's records have been queued: the
/// serve loop's reachable copy of the drv-gpu pair, so a regrant can offer `g` without the
/// pair ever being closed. Zero on a non-GPU board and after a driver end that closed.
static GPU_SESSION_HANDOFF: AtomicU32 = AtomicU32::new(0);
/// The first session's client end of the GPU wire channel, staged by drv-gpu's launcher and
/// transferred once by the boot launch. Later sessions mint their own pair in the regrant
/// path, so this slot stays zero after the boot transfer. — TIMBERDOODLE
static GPU_CLIENT_CHANNEL: AtomicU32 = AtomicU32::new(0);
static GPU_NATIVE_CHANNEL: AtomicU32 = AtomicU32::new(0);
//j711
/// The sending end of one launch's lane bootstrap pair. Every launch mints a fresh pair —
/// the boot one and each G8d-b regrant — and names its lane before dropping the sender:
/// `N` + the native producer on a normal image, or `V` + view channel, `S` + view surface
/// and then `N` on a churn image. The boot churn lane still runs the J0c native diagnostic
/// first (j713); a regranted churn lane has no native producer, so its `N` carries handle
/// zero and the client resumes the churn timeline without native frames (DESIGN/024). The
/// client reads this pair as its second transferred argument. — TIMBERDOODLE
/// A `view` image (`etc/gpu-lane`, PLAN/015 §8.32 D1) sends the boot churn records, `V`,
/// `S`, then `N`, and is never regranted.
static GPU_LANE_BOOTSTRAP: AtomicU32 = AtomicU32::new(0);

/// Boot launch: take the one session wire channel and native producer drv-gpu's launcher
/// staged, and run the cooperative client through the shared lane path. The retained child
/// process handle (the gfx-hold `kept` precedent) goes back to the caller so the serve loop
/// can supervise it; `None` when this board has no GPU lane to run.
fn launch_gpu_client(initrd: Handle) -> Option<Handle> {
    let channel = GPU_CLIENT_CHANNEL.swap(0, Ordering::Relaxed);
    if channel == 0 {
        return None;
    }
    let native = GPU_NATIVE_CHANNEL.swap(0, Ordering::Relaxed);
    launch_gpu_client_session(
        initrd,
        u64::from(channel),
        u64::from(native),
        if gpu_lane().keeps_view() {
            LaneGrant::SoraView
        } else {
            LaneGrant::Normal
        },
    )
}

/// Which lane a launch names, and where its view grant comes from.
#[derive(Clone, Copy)]
enum LaneGrant {
    /// A normal image: `N` plus the native producer (handle zero when there is none).
    Normal,
    /// The boot launch on a `view` or `churn` image: swap view `GPU_VIEW`'s grant out of the
    /// statics.
    SoraView,
    /// A regrant on a churn image: the grant drv-fb issued for this session, or zeros when
    /// no view released in time (the client then fails honestly).
    Regranted(Handle, Handle),
}

/// Where a churn view grant no client consumed goes next.
#[derive(Clone, Copy)]
enum GrantReturn {
    /// A boot grant: back into the statics, so a shell client can still use the view.
    ToSora,
    /// A regrant: close it and let drv-fb report the release afresh for the next attempt.
    Released,
}

//j714 One launch of the cooperative GPU client through the lane path — shared by the boot
/// launch and every supervised regrant, so both speak the same protocol. It mints the
/// one-shot lane pair, transfers the session channel in x0 and the pair's child end in x1
/// (ASYNC|TRANSFER_ARG|TRANSFER_ARG2), then names the lane before dropping the lane sender:
/// `N` + native producer on a normal image, `V` + view channel, `S` + view surface and then
/// `N` on a churn image (DESIGN/024). `session` is the client end of the session wire
/// channel; its server end has already been offered to drv-gpu as `g`.
fn launch_gpu_client_session(
    initrd: Handle,
    session: u64,
    native: u64,
    lane_grant: LaneGrant,
) -> Option<Handle> {
    let valid = |raw: u64| raw != 0 && raw != u64::MAX;
    // Both lanes need one bootstrap pair: the session channel rides x0, and the lane
    // selector rides this pair's other end. The child end is transferred as x1; Sora keeps
    // the sending end until it has named the lane. — TIMBERDOODLE
    let (sender, lane_child) = channel_create_pair();
    if !valid(sender) || !valid(lane_child) {
        if valid(session) {
            let _ = handle_close(Handle(session as u32));
        }
        if valid(native) {
            let _ = handle_close(Handle(native as u32));
        }
        if let LaneGrant::Regranted(channel, vmo) = lane_grant {
            return_gpu_view_grant(channel, vmo, GrantReturn::Released);
        }
        log(b"gpu-client: lane bootstrap unavailable; session retired\n");
        return None;
    }
    // On the churn lane view `GPU_VIEW`'s boot grant leaves the regrant statics only after
    // the pair exists, and returns to them if the launch never reaches the send, so a shell
    // client can still use the view afterwards. — TIMBERDOODLE
    let (view_grant, homing) = match lane_grant {
        LaneGrant::Normal => (None, GrantReturn::ToSora),
        LaneGrant::SoraView => {
            let view_channel =
                Handle(GRAPHICS_CHANNEL[drv_gpu::GPU_VIEW].swap(0, Ordering::Relaxed));
            let view_vmo = Handle(GRAPHICS_VMO[drv_gpu::GPU_VIEW].swap(0, Ordering::Relaxed));
            GRAPHICS_GRANTABLE[drv_gpu::GPU_VIEW].store(false, Ordering::Relaxed);
            (Some((view_channel, view_vmo)), GrantReturn::ToSora)
        }
        LaneGrant::Regranted(channel, vmo) => (Some((channel, vmo)), GrantReturn::Released),
    };
    GPU_LANE_BOOTSTRAP.store(sender as u32, Ordering::Relaxed);
    let flags =
        (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG | ProcessRunFlags::TRANSFER_ARG2)
            .bits();
    let mut kept = None;
    let ran = run_elf_threads(
        initrd,
        kumo_abi::initrd::GPU_CLIENT_PATH.as_bytes(),
        session,
        lane_child,
        flags,
        b"gpu-client",
        0,
        Some(&mut kept),
    );
    if ran {
        let sender = Handle(GPU_LANE_BOOTSTRAP.swap(0, Ordering::Relaxed));
        match view_grant {
            Some((view_channel, view_vmo)) => {
                if view_channel.0 == 0 || view_vmo.0 == 0 {
                    // Honest absence: the client refuses a V lane without a usable grant.
                    let _ = channel_write_with_handle(sender, b"V".as_ptr(), 1, Handle(0));
                    let _ = channel_write_with_handle(sender, b"S".as_ptr(), 1, Handle(0));
                    log_lane(b" lane has no view grant\n");
                } else if channel_write_with_handle(sender, b"V".as_ptr(), 1, view_channel) != 0 {
                    // A refused send leaves the handles in Sora's table.
                    match homing {
                        GrantReturn::ToSora => {
                            GRAPHICS_CHANNEL[drv_gpu::GPU_VIEW]
                                .store(view_channel.0, Ordering::Relaxed);
                            GRAPHICS_VMO[drv_gpu::GPU_VIEW].store(view_vmo.0, Ordering::Relaxed);
                            GRAPHICS_GRANTABLE[drv_gpu::GPU_VIEW].store(true, Ordering::Relaxed);
                            log_lane(b" lane view channel refused; view restored\n");
                        }
                        GrantReturn::Released => {
                            return_gpu_view_grant(view_channel, view_vmo, GrantReturn::Released);
                            log_lane(b" lane view channel refused; grant closed\n");
                        }
                    }
                } else if channel_write_with_handle(sender, b"S".as_ptr(), 1, view_vmo) != 0 {
                    // The channel already moved; without its surface the view cannot be
                    // painted, so the client closes it and drv-fb reports the release.
                    let _ = handle_close(view_vmo);
                    log_lane(b" lane view surface refused\n");
                }
                // j713: the boot churn lane also runs the J0c native diagnostic first, so
                // the native producer travels after the view pair. A regranted lane has no
                // producer and sends handle zero, which the client accepts (DESIGN/024).
                if channel_write_with_handle(sender, b"N".as_ptr(), 1, Handle(native as u32)) != 0
                    && native != 0
                {
                    let _ = handle_close(Handle(native as u32));
                }
                let _ = handle_close(sender);
            }
            None => {
                if channel_write_with_handle(sender, b"N".as_ptr(), 1, Handle(native as u32)) != 0
                    && native != 0
                {
                    let _ = handle_close(Handle(native as u32));
                }
                let _ = handle_close(sender);
            }
        }
        log(b"gpu-client: session handed to a separate process\n");
        kept
    } else {
        let sender = Handle(GPU_LANE_BOOTSTRAP.swap(0, Ordering::Relaxed));
        let _ = handle_close(sender);
        let _ = handle_close(Handle(lane_child as u32));
        let _ = handle_close(Handle(session as u32));
        if native != 0 {
            let _ = handle_close(Handle(native as u32));
        }
        if let Some((view_channel, view_vmo)) = view_grant {
            if view_channel.0 != 0 || view_vmo.0 != 0 {
                return_gpu_view_grant(view_channel, view_vmo, homing);
            }
        }
        log(b"gpu-client: launch failed; session retired\n");
        None
    }
}

/// One `gpu-client: <lane> lane ...` line, naming the lane this image runs, so a `view`
/// lane's failure never reads as a churn one; a churn image's lines are unchanged.
fn log_lane(rest: &[u8]) {
    log(b"gpu-client: ");
    log(gpu_lane().name());
    log(rest);
}

/// Return a churn view grant no client consumed: a boot grant goes back to the statics (a
/// shell client may still use it), while a regrant is closed so drv-fb reports the release
/// afresh and the next attempt waits on real evidence, not a cached flag.
fn return_gpu_view_grant(channel: Handle, vmo: Handle, homing: GrantReturn) {
    match homing {
        GrantReturn::ToSora => {
            GRAPHICS_CHANNEL[drv_gpu::GPU_VIEW].store(channel.0, Ordering::Relaxed);
            GRAPHICS_VMO[drv_gpu::GPU_VIEW].store(vmo.0, Ordering::Relaxed);
            GRAPHICS_GRANTABLE[drv_gpu::GPU_VIEW].store(true, Ordering::Relaxed);
        }
        GrantReturn::Released => {
            if channel.0 != 0 {
                let _ = handle_close(channel);
            }
            if vmo.0 != 0 {
                let _ = handle_close(vmo);
            }
            // Still grantable: the release query re-confirms drv-fb's side before regrant.
            GRAPHICS_GRANTABLE[drv_gpu::GPU_VIEW].store(true, Ordering::Relaxed);
        }
    }
}

//j714 One supervised regrant: a fresh session channel whose server end rides the retained
/// drv-gpu handoff as `g` (DESIGN/024's once-per-session record), then the shared lane path.
/// On a churn image the client also needs view `GPU_VIEW` back: the shell gfx-recover
/// machinery (`wait_for_released` + `regrant_view`) is reused for exactly that view, and an
/// unreleased view becomes the lane's honest zero grant rather than a fabricated one.
//j715
/// Bounded release observation for a churn regrant: twenty control-lane queries paced at
/// 100 ms (2 s) cover the multi-second observation delay seen on metal without letting a
/// wedged control lane hold the serve loop indefinitely. — TIMBERDOODLE
const GPU_REGRANT_OBSERVE_ROUNDS: u32 = 20;
const GPU_REGRANT_OBSERVE_PACE_NS: u64 = 100_000_000;

fn regrant_gpu_session(initrd: Handle) -> Option<Handle> {
    let handoff = Handle(GPU_SESSION_HANDOFF.load(Ordering::Relaxed));
    if handoff.0 == 0 {
        log(b"gpu-client: no session handoff; session regrant unavailable\n");
        return None;
    }
    let valid = |raw: u64| raw != 0 && raw != u64::MAX;
    let (client, server) = channel_create_pair();
    if !valid(client) || !valid(server) {
        for raw in [client, server] {
            if valid(raw) {
                let _ = handle_close(Handle(raw as u32));
            }
        }
        log(b"gpu-client: session pair unavailable; regrant refused\n");
        return None;
    }
    if channel_write_with_handle(handoff, b"g".as_ptr(), 1, Handle(server as u32)) != 0 {
        // Refused: the driver's receiver is gone, and neither end moved.
        let _ = handle_close(Handle(server as u32));
        let _ = handle_close(Handle(client as u32));
        log(b"gpu-client: session regrant refused; grant closed\n");
        return None;
    }
    let lane_grant = if gpu_lane().keeps_view() {
        // j715: observe the release with a bounded poll of the control lane. The first metal
        // regrant queried 34 ms after drv-fb recorded `view 1 recovered` and still saw no
        // release bit, while a later attempt observed it only after ~1.8 s, so the shell's
        // wait helpers are replaced here by direct queries the caller can time, and the mask
        // is logged to make a miss diagnosable. This runs on the serve loop, hence bounded
        // rounds and pacing rather than an unbounded wait. — TIMBERDOODLE
        GRAPHICS_GRANTABLE[drv_gpu::GPU_VIEW].store(true, Ordering::Relaxed);
        let mut last_mask = None;
        let mut observed = false;
        for _round in 0..GPU_REGRANT_OBSERVE_ROUNDS {
            last_mask = query_released();
            if last_mask.is_some_and(|mask| mask & (1u8 << drv_gpu::GPU_VIEW) != 0) {
                observed = true;
                break;
            }
            let _ = kumo_rt::sleep_ns(GPU_REGRANT_OBSERVE_PACE_NS);
        }
        log(b"gpu-client: release observation mask=");
        match last_mask {
            Some(mask) => log_hex(u64::from(mask)),
            None => log(b"none"),
        }
        if observed {
            log(b" observed=yes\n");
        } else {
            log(b" observed=no\n");
        }
        let grant = if observed {
            regrant_view(drv_gpu::GPU_VIEW)
        } else {
            None
        };
        match grant {
            Some((view_channel, view_vmo)) => {
                GRAPHICS_GRANTABLE[drv_gpu::GPU_VIEW].store(false, Ordering::Relaxed);
                GRAPHICS_RELEASED[drv_gpu::GPU_VIEW].store(false, Ordering::Relaxed);
                LaneGrant::Regranted(view_channel, view_vmo)
            }
            None => {
                log(b"gpu-client: view regrant unavailable; sending an honest absence\n");
                LaneGrant::Regranted(Handle(0), Handle(0))
            }
        }
    } else {
        LaneGrant::Normal
    };
    launch_gpu_client_session(initrd, client, 0, lane_grant)
}

/// Hand one view's grant (its presentation channel and writable surface) to drv-gpu, which
/// paints and presents it, then keep the bootstrap pair as the retained session handoff —
/// the same pair carries every later session's `g` (DESIGN/024). Returns false, leaving the
/// grant with Sora, when no GPU driver is waiting or the first send fails; a refused send
/// leaves the handle in Sora's table.
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
    //j714 The pair outlives the boot view grant: it is the driver's session handoff now,
    // so the next session's `g` has somewhere to go (DESIGN/024).
    GPU_SESSION_HANDOFF.store(sender.0, Ordering::Relaxed);
    true
}

/// Retire the pre-grant bootstrap slot once the boot view has had its chance. On a GPU
/// board the sender is not closed — it becomes the retained session handoff — and a board
/// that handed no view to drv-gpu (a churn image keeps `GPU_VIEW`, and a framebuffer-less
/// board has none) records the absence honestly with a bare `v` whose handle is zero, so
/// the driver's first-session view read ends instead of waiting on a pair Sora will never
/// close (DESIGN/024). Non-GPU boards never stored a sender here, so nothing changes.
fn release_gpu_bootstrap() {
    let sender = GPU_VIEW_BOOTSTRAP.swap(0, Ordering::Relaxed);
    if sender == 0 {
        return;
    }
    let handoff = Handle(sender);
    if channel_write_with_handle(handoff, b"v".as_ptr(), 1, Handle(0)) != 0 {
        // The driver closed its end; nothing can serve sessions over this pair.
        let _ = handle_close(handoff);
        log(b"drv-gpu: boot view absence refused; handoff closed\n");
        return;
    }
    GPU_SESSION_HANDOFF.store(sender, Ordering::Relaxed);
    log(b"drv-gpu: boot view absence recorded for the first session\n");
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
        clip_demo_pixel, demo_pixel, DamageSet, Present, Reply, Status, Surface, CLIP_DEMO, DEMO,
        REPLY_LEN, VIEW_COUNT,
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
                log(b"gfx-demo: pre-present pump failed\n");
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
            let frame = Present {
                sequence,
                surface,
                damage: DamageSet::FULL,
            }
            .encode();
            if channel_write(channel, frame.as_ptr(), frame.len()) != 0
                || !child_parked(process_wait())
            {
                log(b"gfx-demo: present queue failed\n");
                return None;
            }
            let mut reply = [0u8; REPLY_LEN + 1];
            // The gate may hold the reply behind its bounded frame-start wait; a single
            // read can be released empty by an unrelated wake first. — KINGFISHER
            let mut len = 0;
            for _ in 0..REPLY_READ_ATTEMPTS {
                len = channel_read(channel, reply.as_mut_ptr(), reply.len()) as usize;
                if len != 0 {
                    break;
                }
            }
            let decoded = reply.get(..len).and_then(Reply::decode);
            let Some(reply) = decoded else {
                log(if len == 0 {
                    b"gfx-demo: reply missing\n" as &[u8]
                } else if len < REPLY_LEN {
                    b"gfx-demo: reply short\n"
                } else {
                    b"gfx-demo: reply undecodable\n"
                });
                return None;
            };
            if reply.sequence != sequence {
                log(b"gfx-demo: reply sequence mismatch\n");
                return None;
            }
            Some(reply.status)
        })();
        match result {
            Some(status) => {
                // A valid reply releases this surface even when its view rejected it.
                GRAPHICS_CHANNEL[view].store(channel.0, Ordering::Relaxed);
                if matches!(status, Status::Retired | Status::Copied) {
                    copied[view] = true;
                    log(b"gfx-demo: view ");
                    log(&[b'0' + view as u8]);
                    if status == Status::Retired {
                        log(b" retired; frame proven on the scanout\n");
                    } else {
                        log(b" copied; buffer released (software GOP)\n");
                    }
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

/// Write one whole surface in 256-byte VMO chunks. Shared by the damage proof; the syscall's
/// per-write cap is 256 bytes. — TIMBERDOODLE
fn write_surface(vmo: Handle, surface: kumo_graphics::Surface, pixel: fn(u32, u32) -> u32) -> bool {
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
                return false;
            }
        }
    }
    true
}

/// Send one Present and wait for the reply with the same sequence. — TIMBERDOODLE
fn present_and_wait(
    channel: Handle,
    present: kumo_graphics::Present,
) -> Option<kumo_graphics::Status> {
    if !child_parked(process_wait()) {
        return None;
    }
    let frame = present.encode();
    if channel_write(channel, frame.as_ptr(), frame.len()) != 0 || !child_parked(process_wait()) {
        return None;
    }
    let mut reply = [0u8; kumo_graphics::REPLY_LEN + 1];
    let len = channel_read(channel, reply.as_mut_ptr(), reply.len()) as usize;
    let reply = kumo_graphics::Reply::decode(reply.get(..len)?)?;
    (reply.sequence == present.sequence).then_some(reply.status)
}

/// F2a-2 damage proof: present a complete demo image, then poison every undeclared pixel and
/// present exactly one damaged rectangle. A presenter that honours the set leaves the first
/// image outside the rectangle and shows no poison. — TIMBERDOODLE
fn graphics_damage(cmd: &kumoza::Command) {
    use kumo_graphics::{
        damage_poison_pixel, demo_pixel, DamageSet, Present, Status, DAMAGE_RECT, DEMO,
    };
    if !cmd.args.is_empty() {
        usage(b"usage: gfx-damage\n");
        return;
    }
    let mut selected = None;
    for view in 0..kumo_graphics::VIEW_COUNT {
        let channel = Handle(GRAPHICS_CHANNEL[view].swap(0, Ordering::Relaxed));
        if channel.0 == 0 {
            continue;
        }
        selected = Some((
            view,
            channel,
            Handle(GRAPHICS_VMO[view].load(Ordering::Relaxed)),
        ));
        break;
    }
    let Some((view, channel, vmo)) = selected else {
        log(b"gfx-damage: no graphics view available\n");
        fail_status(1);
        return;
    };
    if vmo.0 == 0 {
        let _ = handle_close(channel);
        log(b"gfx-damage: no surface available\n");
        fail_status(1);
        return;
    }
    let Some(damage) = DamageSet::single(DAMAGE_RECT) else {
        let _ = handle_close(channel);
        log(b"gfx-damage: invalid proof rectangle\n");
        fail_status(1);
        return;
    };
    let result = (|| {
        if !write_surface(vmo, DEMO, demo_pixel) {
            return None;
        }
        let sequence = GRAPHICS_SEQUENCE[view].fetch_add(1, Ordering::Relaxed);
        if present_and_wait(
            channel,
            Present {
                sequence,
                surface: DEMO,
                damage: DamageSet::FULL,
            },
        ) != Some(Status::Copied)
        {
            return None;
        }
        if !write_surface(vmo, DEMO, damage_poison_pixel) {
            return None;
        }
        let sequence = GRAPHICS_SEQUENCE[view].fetch_add(1, Ordering::Relaxed);
        if present_and_wait(
            channel,
            Present {
                sequence,
                surface: DEMO,
                damage,
            },
        ) != Some(Status::Copied)
        {
            return None;
        }
        Some(())
    })();
    match result {
        Some(()) => {
            GRAPHICS_CHANNEL[view].store(channel.0, Ordering::Relaxed);
            notice(&[b"gfx-damage: full then partial damage copied\n"]);
        }
        None => {
            let _ = handle_close(channel);
            log(b"gfx-damage: damage proof failed\n");
            fail_status(1);
        }
    }
}

/// Snapshot one selected file into its own immutable VMO; the viewer gets no namespace. — SAKER
fn bitmap_snapshot(
    len: u64,
    read: impl FnMut(u64, &mut [u8]) -> Option<usize>,
) -> Result<(Handle, u64), &'static [u8]> {
    if len == 0 || len > bmp_view::MAX_FILE_BYTES {
        return Err(b"bmp-view: file must be 1..16777216 bytes\n");
    }
    let raw = vmo_create((len + 4095) & !4095);
    if raw == 0 || raw == u64::MAX {
        return Err(b"bmp-view: file VMO unavailable\n");
    }
    let writer = Handle(raw as u32);
    let copied = bmp_view::copy_file(len, read, |offset, bytes| {
        vmo_write(writer, offset, bytes.as_ptr(), bytes.len()) == 0
    });
    let grant = if copied {
        handle_duplicate(writer, Rights::READ | Rights::TRANSFER)
    } else {
        0
    };
    let _ = handle_close(writer);
    if grant == 0 || grant == u64::MAX {
        return Err(b"bmp-view: file read failed\n");
    }
    Ok((Handle(grant as u32), len))
}

fn bitmap_volume_file(fs: Handle, path: &[u8]) -> Result<(Handle, u64), &'static [u8]> {
    let result = (|| {
        let mut request = [0; 4 + kumo_vfs::MAX_PATH];
        let mut reply = [0; 257];
        let mut call = |request: &[u8]| -> Result<usize, &'static [u8]> {
            if channel_write(fs, request.as_ptr(), request.len()) != 0 {
                return Err(b"bmp-view: filesystem unavailable\n");
            }
            let n = read_channel_ready(fs, &mut reply)
                .ok_or(b"bmp-view: filesystem read failed\n".as_slice())?;
            if n == 0 {
                return Err(b"bmp-view: filesystem read failed\n");
            }
            Ok(n)
        };
        let n = kumo_vfs::Request::open(path)
            .encode_into(&mut request)
            .ok_or(b"bmp-view: path too long\n".as_slice())?;
        let n = call(&request[..n])?;
        drop(call);
        let (file, len) = kumo_vfs::parse_open_reply(&reply[..n]).map_err(|error| match error {
            kumo_vfs::VfsError::Status(kumo_vfs::VFS_NOT_FOUND) => {
                b"bmp-view: not found\n".as_slice()
            }
            kumo_vfs::VfsError::Status(kumo_vfs::VFS_IS_DIR) => {
                b"bmp-view: is a directory\n".as_slice()
            }
            _ => b"bmp-view: open failed\n".as_slice(),
        })?;
        bitmap_snapshot(len, |offset, bytes| {
            let n = kumo_vfs::Request::read(file, offset, bytes.len() as u32)
                .encode_into(&mut request)?;
            if channel_write(fs, request.as_ptr(), n) != 0 {
                return None;
            }
            let n = read_channel_ready(fs, &mut reply)?;
            if n == 0 {
                return None;
            }
            let data = kumo_vfs::read_payload(&reply[..n]).ok()?;
            if data.len() > bytes.len() {
                return None;
            }
            bytes[..data.len()].copy_from_slice(data);
            Some(data.len())
        })
    })();
    let _ = handle_close(fs);
    let _ = process_wait();
    result
}

fn bitmap_file(initrd: Handle, path: &[u8]) -> Result<(Handle, u64), &'static [u8]> {
    if !sora::shell_may_read(path) {
        return Err(b"bmp-view: permission denied\n");
    }
    if let Some(path) = sora::scratch_path(path) {
        let fs = spawn_fatfs(initrd).ok_or(b"bmp-view: filesystem unavailable\n".as_slice())?;
        return bitmap_volume_file(fs, path);
    }
    match sora::media_route(path) {
        sora::MediaRoute::Root => return Err(b"bmp-view: is a directory\n"),
        sora::MediaRoute::Volume { name, path } => {
            if !media().is_some_and(|media| media.name.as_bytes() == name) {
                return Err(b"bmp-view: no such volume\n");
            }
            let fs = spawn_media_fatfs(initrd, BlockAccess::ReadOnly)
                .ok_or(b"bmp-view: filesystem unavailable\n".as_slice())?;
            return bitmap_volume_file(fs, path);
        }
        sora::MediaRoute::Elsewhere => {}
    }
    let path = path.strip_prefix(b"/").unwrap_or(path);
    let (base, len) = initrd_find(initrd, path).ok_or(b"bmp-view: not found\n".as_slice())?;
    bitmap_snapshot(len, |offset, bytes| {
        (vmo_read(initrd, base + offset, bytes.as_mut_ptr(), bytes.len()) == 0)
            .then_some(bytes.len())
    })
}

/// Hand one supervisor-held presentation grant to a separate process (F1b). gfx-client
/// receives the presenter directly; bmp-view receives it through its finite bootstrap,
/// alongside an optional read-only file snapshot. Both receive the surface VMO in x1.
/// Re-grant a released view: a fresh channel pair and surface, announced on the retained
/// drv-fb bootstrap channel. Returns the client end and its VMO, or None after closing every
/// handle when the request cannot be staged. — TIMBERDOODLE
fn regrant_view(view: usize) -> Option<(Handle, Handle)> {
    let control = Handle(DRV_FB_BOOTSTRAP.load(Ordering::Relaxed));
    let frame = kumo_graphics::regrant_request(view)?;
    if control.0 == 0 {
        log(b"gfx: regrant without a control channel\n");
        return None;
    }
    let valid = |raw: u64| raw != 0 && raw != u64::MAX;
    let (client, server) = channel_create_pair();
    let vmo = vmo_create(kumo_graphics::SURFACE_BYTES as u64);
    let surface = if valid(vmo) {
        handle_duplicate(Handle(vmo as u32), Rights::READ | Rights::TRANSFER)
    } else {
        0
    };
    let queued = valid(client)
        && valid(server)
        && valid(surface)
        && channel_write_with_handle(
            Handle(client as u32),
            b"S".as_ptr(),
            1,
            Handle(surface as u32),
        ) == 0;
    if !queued {
        log(b"gfx: regrant surface queue failed\n");
    }
    let sent = queued
        && channel_write_with_handle(control, frame.as_ptr(), frame.len(), Handle(server as u32))
            == 0;
    if queued && !sent {
        log(b"gfx: regrant control write failed\n");
    }
    if sent {
        return Some((Handle(client as u32), Handle(vmo as u32)));
    }
    for raw in [client, server, vmo] {
        if valid(raw) {
            let _ = handle_close(Handle(raw as u32));
        }
    }
    if valid(surface) && !queued {
        let _ = handle_close(Handle(surface as u32));
    }
    None
}

/// Ask drv-fb which views' clients have closed. The control lane is the retained drv-fb
/// bootstrap channel; drv-fb answers from its lane state. — TIMBERDOODLE
fn query_released() -> Option<u8> {
    let control = Handle(DRV_FB_BOOTSTRAP.load(Ordering::Relaxed));
    if control.0 == 0 {
        return None;
    }
    if channel_write(
        control,
        kumo_graphics::RELEASE_QUERY.as_ptr(),
        kumo_graphics::RELEASE_QUERY.len(),
    ) != 0
    {
        return None;
    }
    let mut reply = [0u8; 8];
    let n = channel_read(control, reply.as_mut_ptr(), reply.len()) as usize;
    kumo_graphics::parse_release_reply(reply.get(..n)?)
}

/// Ask drv-fb which views hold a recorded frame, so a supervisor can observe that a
/// presenter has come up before acting on the grant. — TIMBERDOODLE
fn query_presented() -> Option<u8> {
    let control = Handle(DRV_FB_BOOTSTRAP.load(Ordering::Relaxed));
    if control.0 == 0 {
        return None;
    }
    if channel_write(
        control,
        kumo_graphics::PRESENTED_QUERY.as_ptr(),
        kumo_graphics::PRESENTED_QUERY.len(),
    ) != 0
    {
        return None;
    }
    let mut reply = [0u8; 8];
    let n = channel_read(control, reply.as_mut_ptr(), reply.len()) as usize;
    kumo_graphics::parse_presented_reply(reply.get(..n)?)
}

/// Fold drv-fb's release mask into Sora's released views, logging each newly observed one.
fn refresh_released() {
    let Some(mask) = query_released() else {
        return;
    };
    for view in 0..kumo_graphics::VIEW_COUNT {
        if !GRAPHICS_GRANTABLE[view].load(Ordering::Relaxed)
            || mask & (1u8 << view) == 0
            || GRAPHICS_RELEASED[view].load(Ordering::Relaxed)
        {
            continue;
        }
        GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
        GRAPHICS_ASYNC[view].store(false, Ordering::Relaxed);
        let process = GRAPHICS_ASYNC_PROCESS[view].swap(0, Ordering::Relaxed);
        if process != 0 {
            let _ = handle_close(Handle(process));
        }
        notice(&[
            b"sora: view ",
            &[b'0' + view as u8],
            b" released by drv-fb\n",
        ]);
    }
}

/// F2b focus: Ctrl-] cycles the keyboard owner among Sora-held views without reaching the
/// shell. A Kilo session keeps the key; a full-screen program owns the keyboard.
/// — TIMBERDOODLE
const FOCUS_KEY: u8 = 0x1d;

/// The next view with a held grant after `current`. The cycle walks forward only and passes
/// through none after the last grantable view, so the shell regains the keyboard.
/// — TIMBERDOODLE
fn next_focus(current: Option<usize>) -> Option<usize> {
    let start = match current {
        Some(view) => view + 1,
        None => 0,
    };
    (start..kumo_graphics::VIEW_COUNT)
        .find(|view| GRAPHICS_GRANTABLE[*view].load(Ordering::Relaxed))
}

/// Tell drv-fb which view owns the keyboard and record the change. — TIMBERDOODLE
fn announce_focus(view: Option<usize>) {
    let control = Handle(DRV_FB_BOOTSTRAP.load(Ordering::Relaxed));
    if control.0 != 0 {
        if let Some(record) = kumo_graphics::focus_record(view) {
            let _ = channel_write(control, record.as_ptr(), record.len());
        }
    }
    match view {
        Some(view) => notice(&[b"sora: focus view ", &[b'0' + view as u8], b"\n"]),
        None => notice(&[b"sora: focus none\n"]),
    }
}

/// How long Sora waits for one ASYNC client to exit before giving up on its view. The old
/// pump bound was 64 scheduler calls; a wall-clock bound does not depend on scheduler speed.
const ASYNC_CLIENT_WAIT_NS: u64 = 5_000_000_000;

/// Wait for every view still leased to an ASYNC client to signal its process `TERMINATED`.
/// One private port carries the retained process signals and a one-shot timeout; a packet's
/// source koid names the view. A late bind is safe: the kernel replays a process's retained
/// `TERMINATED` level, as it already does for a fired timer. — TIMBERDOODLE
fn wait_for_async_clients() -> bool {
    use kumo_graphics::VIEW_COUNT;
    let mut pending = [false; VIEW_COUNT];
    let mut any = false;
    for view in 0..VIEW_COUNT {
        pending[view] = GRAPHICS_GRANTABLE[view].load(Ordering::Relaxed)
            && GRAPHICS_ASYNC[view].load(Ordering::Relaxed)
            && !GRAPHICS_RELEASED[view].load(Ordering::Relaxed);
        any |= pending[view];
    }
    if !any {
        return true;
    }

    let Some(port) = syscall_handle(port_create()) else {
        log(b"sora: async client wait port unavailable\n");
        return false;
    };
    let Some(timer) = syscall_handle(timer_create(ASYNC_CLIENT_WAIT_NS)) else {
        let _ = handle_close(port);
        log(b"sora: async client wait timer unavailable\n");
        return false;
    };
    let timer_koid = handle_koid(timer);
    if timer_koid == u64::MAX || port_bind(port, timer) != 0 {
        let _ = handle_close(timer);
        let _ = handle_close(port);
        log(b"sora: async client wait timer bind failed\n");
        return false;
    }

    let mut bound = [false; VIEW_COUNT];
    let mut sources = [0u64; VIEW_COUNT];
    let mut remaining = 0usize;
    let mut ok = true;
    for view in 0..VIEW_COUNT {
        if !pending[view] {
            continue;
        }
        let Some(process) =
            syscall_handle(GRAPHICS_ASYNC_PROCESS[view].load(Ordering::Relaxed) as u64)
        else {
            log(b"sora: async client has no retained process handle\n");
            ok = false;
            break;
        };
        let koid = handle_koid(process);
        if koid == u64::MAX || port_bind(port, process) != 0 {
            log(b"sora: async client process bind failed\n");
            ok = false;
            break;
        }
        bound[view] = true;
        sources[view] = koid;
        remaining += 1;
    }

    while ok && remaining > 0 {
        let source = port_wait(port);
        if source == 0 {
            log(b"sora: async client wait failed\n");
            ok = false;
            break;
        }
        if source == timer_koid {
            for view in 0..VIEW_COUNT {
                if pending[view] {
                    notice(&[
                        b"sora: view ",
                        &[b'0' + view as u8],
                        b" client process wait timed out\n",
                    ]);
                }
            }
            ok = false;
            break;
        }
        let Some(view) = (0..VIEW_COUNT).find(|&view| bound[view] && sources[view] == source)
        else {
            continue;
        };
        if !pending[view] {
            continue;
        }
        pending[view] = false;
        remaining -= 1;
        notice(&[
            b"sora: view ",
            &[b'0' + view as u8],
            b" client process exited (TERMINATED)\n",
        ]);
    }

    for view in 0..VIEW_COUNT {
        if bound[view] {
            if let Some(process) =
                syscall_handle(GRAPHICS_ASYNC_PROCESS[view].load(Ordering::Relaxed) as u64)
            {
                let _ = port_unbind(port, process);
            }
        }
    }
    let _ = port_unbind(port, timer);
    let _ = handle_close(timer);
    let _ = handle_close(port);
    ok
}

/// Wait for a released view. A leased ASYNC view is waited on by its client's process object,
/// one exit at a time; then one scheduler pump lets drv-fb drain every closed presentation lane
/// and park before the release mask is read. The result is the lowest released grantable view.
/// — TIMBERDOODLE
fn wait_for_released() -> Option<usize> {
    if !wait_for_async_clients() {
        refresh_released();
        return None;
    }
    // One pump, not a spin: this is what lets drv-fb observe each closed lane before its
    // release query is answered. The per-process waits above are what establish the exits.
    let _ = process_wait();
    refresh_released();
    (0..kumo_graphics::VIEW_COUNT).find(|view| {
        GRAPHICS_GRANTABLE[*view].load(Ordering::Relaxed)
            && GRAPHICS_RELEASED[*view].load(Ordering::Relaxed)
    })
}

/// Admit one input consumer for a grantable view (F2b focus routing): Sora keeps the server
/// end of the input channel, transfers the client end in `x0`, and names the view as the
/// channel's first record so every consumed key is attributable (J677's acceptance gate).
/// While that view holds focus, keyboard bytes go to the consumer; a view without one keeps
/// the shell. — TIMBERDOODLE
fn gfx_keys(cmd: &kumoza::Command, initrd: Handle) {
    if !cmd.args.is_empty() {
        usage(b"usage: gfx-keys\n");
        return;
    }
    let view = (0..kumo_graphics::VIEW_COUNT).find(|view| {
        GRAPHICS_GRANTABLE[*view].load(Ordering::Relaxed)
            && GRAPHICS_INPUT[*view].load(Ordering::Relaxed) == 0
            && !(TERMINAL_MODE.load(Ordering::Relaxed) && *view == TERMINAL_VIEW)
    });
    let Some(view) = view else {
        log(b"gfx-keys: no view free for an input consumer\n");
        fail_status(1);
        return;
    };
    let (server, client) = channel_create_pair();
    if server == u64::MAX || client == u64::MAX {
        log(b"gfx-keys: channel unavailable\n");
        fail_status(1);
        return;
    }
    GRAPHICS_INPUT[view].store(server as u32, Ordering::Relaxed);
    let flags = ProcessRunFlags::TRANSFER_ARG.bits();
    let ran = run_elf_threads(
        initrd,
        kumo_abi::initrd::GFX_KEYS_PATH.as_bytes(),
        client,
        0,
        flags,
        b"gfx-keys",
        0,
        None,
    );
    if !ran {
        GRAPHICS_INPUT[view].store(0, Ordering::Relaxed);
        let _ = handle_close(Handle(server as u32));
        fail_status(1);
        return;
    }
    // The view index rides the channel, not a register: ProcessRun's plain argument slot
    // silently stages any value that collides with a live handle in Sora's table (J677 saw
    // `view 2` arrive for a passed 1), so the first channel record is the safe carrier.
    // — TIMBERDOODLE
    let index = [view as u8];
    let _ = channel_write(Handle(server as u32), index.as_ptr(), index.len());
    notice(&[b"sora: keys view ", &[b'0' + view as u8], b"\n"]);
}

/// Admit a presenter, kill it mid-grant, and recover the view for a fresh presenter (F2c
/// slice-6). The hold client presents one full frame and parks; once drv-fb records that
/// frame, Sora kills the child, waits for the release record, regrants the view and runs the
/// reference client to prove presentation survives a presenter's death. — TIMBERDOODLE
fn gfx_recover(cmd: &kumoza::Command, initrd: Handle) {
    use kumo_graphics::VIEW_COUNT;
    if !cmd.args.is_empty() {
        usage(b"usage: gfx-recover\n");
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
    // The metal case: an earlier presenter consumed the bootstrap grant and exited, so the
    // view is released rather than pristine. Recover it the same way a first client would be
    // regranted, instead of refusing. — TIMBERDOODLE
    if selected.is_none() {
        if let Some(view) = wait_for_released() {
            if let Some((channel, vmo)) = regrant_view(view) {
                GRAPHICS_RELEASED[view].store(false, Ordering::Relaxed);
                GRAPHICS_ASYNC[view].store(false, Ordering::Relaxed);
                selected = Some((view, channel, vmo));
            }
        }
    }
    let Some((view, channel, vmo)) = selected else {
        log(b"gfx-recover: no graphics view available\n");
        fail_status(1);
        return;
    };
    if vmo.0 == 0 {
        let _ = handle_close(channel);
        GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
        log(b"gfx-recover: no surface available\n");
        fail_status(1);
        return;
    }
    notice(&[
        b"sora: gfx-recover: hold admitted for view ",
        &[b'0' + view as u8],
        b"\n",
    ]);
    let flags =
        (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG | ProcessRunFlags::TRANSFER_ARG2)
            .bits();
    let mut kept = None;
    let ran = run_elf_threads(
        initrd,
        kumo_abi::initrd::GFX_HOLD_PATH.as_bytes(),
        channel.0 as u64,
        vmo.0 as u64,
        flags,
        b"gfx-hold",
        0,
        Some(&mut kept),
    );
    let Some(child) = kept.filter(|_| ran) else {
        // An admitted child owns the handles; only an admission failure leaves them here.
        if handle_koid(channel) != u64::MAX {
            let _ = handle_close(channel);
            let _ = handle_close(vmo);
        }
        GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
        log(b"gfx-recover: hold admission failed\n");
        fail_status(1);
        return;
    };
    // Wait (bounded) until drv-fb records the presenter's first frame: recovery of a live
    // grant is only meaningful once the child has actually presented.
    let mut presented = false;
    for _ in 0..4000 {
        if query_presented().is_some_and(|mask| mask & (1u8 << view) != 0) {
            presented = true;
            break;
        }
        let _ = process_wait();
    }
    if !presented {
        log(b"gfx-recover: presenter never presented\n");
        let _ = process_kill(child);
        let _ = handle_close(child);
        GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
        fail_status(1);
        return;
    }
    notice(&[
        b"sora: view ",
        &[b'0' + view as u8],
        b" presented before the kill\n",
    ]);
    // Kill the presenter mid-grant: the view must release through the normal lane-close
    // path, exactly as a crashed presenter would. — TIMBERDOODLE
    let _ = process_kill(child);
    let _ = handle_close(child);
    let mut released = false;
    for _ in 0..4000 {
        let _ = process_wait();
        if query_released().is_some_and(|mask| mask & (1u8 << view) != 0) {
            released = true;
            break;
        }
    }
    if !released {
        log(b"gfx-recover: killed presenter never released the view\n");
        fail_status(1);
        return;
    }
    notice(&[
        b"sora: view ",
        &[b'0' + view as u8],
        b" recovered after the kill\n",
    ]);
    GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
    // Restart the presenter on a fresh grant: the recovery half of the proof.
    let Some((channel, vmo)) = regrant_view(view) else {
        log(b"gfx-recover: regrant failed\n");
        fail_status(1);
        return;
    };
    GRAPHICS_RELEASED[view].store(false, Ordering::Relaxed);
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
        if handle_koid(channel) != u64::MAX {
            let _ = handle_close(channel);
            let _ = handle_close(vmo);
        }
        GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
        log(b"gfx-recover: restarted presenter failed\n");
        fail_status(1);
        return;
    }
    GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
    notice(&[
        b"sora: gfx-recover: view ",
        &[b'0' + view as u8],
        b" recovered and regranted\n",
    ]);
}

/// Toggle the server's own band labels (F2c's first visible glyph-atlas consumer): the record
/// rides the retained drv-fb bootstrap lane. `gfx-titles on|off`. — TIMBERDOODLE
fn gfx_titles(cmd: &kumoza::Command) {
    let on = match cmd.args.first().map(|arg| arg.as_str()) {
        Some("on") => true,
        Some("off") => false,
        _ => {
            usage(b"usage: gfx-titles on|off\n");
            return;
        }
    };
    let control = Handle(DRV_FB_BOOTSTRAP.load(Ordering::Relaxed));
    if control.0 == 0 {
        log(b"gfx-titles: no display server\n");
        fail_status(1);
        return;
    }
    let record = kumo_graphics::encode_titles(on);
    if channel_write(control, record.as_ptr(), record.len()) != 0 {
        log(b"gfx-titles: control write failed\n");
        fail_status(1);
        return;
    }
    notice(&[if on {
        b"sora: titles on\n" as &[u8]
    } else {
        b"sora: titles off\n"
    }]);
}

/// C3 slices 1-2 (PLAN/016 C3): present the kernel log as a client surface, rewriting and
/// damaging only the rows that changed. Sora reads the log ring's tail, keeps the last six
/// padded rows, compares their hashes against the last presented rows, renders changed rows
/// with the console's own coverage renderer over the retained surface, and presents a
/// bounding damage set of just those rows. An unchanged tail skips the present entirely.
/// — TIMBERDOODLE
fn log_row_hash(row: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in row {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// One console-view refresh: read the view's stream tail (the kernel log ring for view 0,
/// Sora's own userland console stream for view 1), render changed rows over the retained
/// surface, present a bounding damage set, and decode the reply. `quiet` keeps the mirror's
/// idle and unavailable ticks silent; one-shot runs announce them. Returns the pass outcome:
/// DEAD when the view is unavailable (the caller decides whether that is fatal), RETIRED when
/// its present consumed a frame start, ALIVE otherwise. — TIMBERDOODLE
fn console_refresh(view: usize, quiet: bool) -> sora::RefreshOutcome {
    use kumo_graphics::{DamageSet, Present, Reply, Status, DEMO, REPLY_LEN};
    const LOG_VA: u64 = 0x3800_0000;
    let channel = Handle(GRAPHICS_CHANNEL[view].swap(0, Ordering::Relaxed));
    let vmo = Handle(GRAPHICS_VMO[view].load(Ordering::Relaxed));
    if channel.0 == 0 || vmo.0 == 0 {
        if channel.0 != 0 {
            GRAPHICS_CHANNEL[view].store(channel.0, Ordering::Relaxed);
        }
        if !quiet {
            log(b"gfx-log: no graphics view held\n");
        }
        return sora::RefreshOutcome::DEAD;
    }
    let map = vmar_map(
        Handle(0),
        vmo,
        0,
        LOG_VA,
        kumo_graphics::SURFACE_BYTES as u64,
        (VmarFlags::READ | VmarFlags::WRITE).0,
    );
    if map != 0 {
        GRAPHICS_CHANNEL[view].store(channel.0, Ordering::Relaxed);
        log(b"gfx-log: surface map failed\n");
        return sora::RefreshOutcome::DEAD;
    }
    let result = (|| -> Option<(Status, Option<([u64; LOG_ROWS], (usize, usize))>)> {
        if !child_parked(process_wait()) {
            log(b"gfx-log: pre-present pump failed\n");
            return None;
        }
        // Fetch the view's stream tail; `seen` counts bytes ever, `tail` keeps the last
        // window in overwrite order (the shared linearize below restores line order).
        let mut tail = [0u8; 1024];
        let seen;
        if CONSOLE_SOURCES[view] == CONSOLE_KERNEL_RING {
            let resource = Handle(ROOT_RESOURCE.load(Ordering::Relaxed) as u32);
            let mut chunk = [0u8; 1024];
            let mut offset = 0u64;
            let mut total = 0usize;
            for _ in 0..64 {
                let (n, next) = kumo_rt::log_read(resource, offset, &mut chunk);
                if n == u64::MAX || n == 0 {
                    break;
                }
                for &byte in &chunk[..n as usize] {
                    tail[total % tail.len()] = byte;
                    total += 1;
                }
                offset = next;
            }
            seen = total;
        } else {
            // The interactive terminal's shell stream (not Sora's notices).
            let seq = SHELL_RING_SEQ.load(Ordering::Relaxed) as usize;
            let len = seq.min(tail.len());
            for (offset, index) in (seq - len..seq).enumerate() {
                tail[offset] = SHELL_RING[index % tail.len()].load(Ordering::Relaxed);
            }
            seen = seq;
        }
        // Linearize the tail once: the overwrite ring may have wrapped.
        let start = seen.saturating_sub(tail.len());
        let mut linear = [0u8; 1024];
        let len = seen - start;
        for (offset, index) in (start..seen).enumerate() {
            linear[offset] = tail[index % tail.len()];
        }
        // Split into lines and keep the last LOG_ROWS, each padded to the row width.
        // Present-traffic records and the view's own records are dropped: the display
        // server writes two lines for every present, so a view that shows them can never
        // converge on its own tail, and failure-path `gfx-log:` records ride the userland
        // stream too. — TIMBERDOODLE
        const PRESENT_RECORD: &[u8] = b"display: present ";
        const SELF_RECORD: &[u8] = b"gfx-log: ";
        let mut lines = [[b' '; LOG_COLS]; LOG_ROWS];
        let mut row = 0usize;
        let mut cursor = 0usize;
        while cursor < len {
            let end = linear[cursor..len]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(len, |rel| cursor + rel);
            let line = &linear[cursor..end];
            // Ring lines carry a `[ timestamp] ` stamp; match the records after it.
            let text = match line.iter().position(|&byte| byte == b']') {
                Some(close) if line.first() == Some(&b'[') => {
                    let rest = &line[close + 1..];
                    let skip = rest
                        .iter()
                        .position(|&byte| byte != b' ')
                        .unwrap_or(rest.len());
                    &rest[skip..]
                }
                _ => line,
            };
            if !text.starts_with(PRESENT_RECORD) && !text.starts_with(SELF_RECORD) {
                let mut col = 0usize;
                for &byte in line {
                    // A plain text surface: printable ASCII inside the row width.
                    if (0x20..=0x7e).contains(&byte) && col < LOG_COLS {
                        lines[row][col] = byte;
                        col += 1;
                    }
                }
                row += 1;
                if row == LOG_ROWS {
                    lines.rotate_left(1);
                    row -= 1;
                }
            }
            cursor = end + 1;
        }
        // A regranted view starts from a blank VMO; never trust the old row hashes there.
        let vmo_koid = handle_koid(vmo);
        let fresh_surface = LOG_VMO_KOID[view].swap(vmo_koid, Ordering::Relaxed) != vmo_koid;
        if fresh_surface {
            for slot in &LOG_ROW_HASH[view] {
                slot.store(0, Ordering::Relaxed);
            }
        }
        let mut hashes = [0u64; LOG_ROWS];
        let mut changed: Option<(usize, usize)> = None;
        for (index, line) in lines.iter().enumerate() {
            hashes[index] = log_row_hash(line);
            if LOG_ROW_HASH[view][index].load(Ordering::Relaxed) != hashes[index] {
                changed = Some(match changed {
                    Some((first, _)) => (first, index),
                    None => (index, index),
                });
            }
        }
        let Some((first, last)) = changed else {
            return Some((Status::Copied, None));
        };
        // Repaint only the changed rows over the retained picture.
        let mut console = unsafe {
            drv_fb::Console::new_preserving(
                LOG_VA as *mut u32,
                DEMO.width as usize,
                DEMO.height as usize,
                DEMO.stride as usize,
            )
        };
        for index in first..=last {
            if LOG_ROW_HASH[view][index].load(Ordering::Relaxed) == hashes[index] {
                continue;
            }
            console.write(&[0x1b, b'[']);
            console.write(&[b'0' + (index as u8 + 1)]);
            console.write(b";1H");
            console.write(&lines[index]);
        }
        // A pristine lane refuses a partial first present: the server must see every pixel
        // declared once. The first frame on a fresh surface claims the whole of it.
        let damage = if fresh_surface {
            DamageSet::FULL
        } else {
            let damage = kumo_graphics::Damage {
                x: 0,
                y: (first * drv_fb::GLYPH_H) as u32,
                width: DEMO.width,
                height: ((last - first + 1) * drv_fb::GLYPH_H) as u32,
            };
            DamageSet::single(damage).unwrap_or(DamageSet::FULL)
        };
        let sequence = GRAPHICS_SEQUENCE[view].fetch_add(1, Ordering::Relaxed);
        let frame = Present {
            sequence,
            surface: DEMO,
            damage,
        }
        .encode();
        if channel_write(channel, frame.as_ptr(), frame.len()) != 0 || !child_parked(process_wait())
        {
            log(b"gfx-log: present queue failed\n");
            return None;
        }
        let mut reply = [0u8; REPLY_LEN + 1];
        let len = channel_read(channel, reply.as_mut_ptr(), reply.len()) as usize;
        let reply = Reply::decode(reply.get(..len)?)?;
        (reply.sequence == sequence).then_some((reply.status, Some((hashes, (first, last)))))
    })();
    let _ = kumo_rt::vmar_unmap(LOG_VA, kumo_graphics::SURFACE_BYTES as u64);
    match result {
        Some((Status::Copied | Status::Retired, None)) => {
            // A valid reply with no changed rows: the retained surface is current.
            GRAPHICS_CHANNEL[view].store(channel.0, Ordering::Relaxed);
            if !quiet {
                term(b"gfx-log: view ");
                term(&[b'0' + view as u8]);
                term(b" unchanged; no present\n");
            }
            sora::RefreshOutcome::ALIVE
        }
        Some((status, Some((hashes, (first, last)))))
            if matches!(status, Status::Copied | Status::Retired) =>
        {
            GRAPHICS_CHANNEL[view].store(channel.0, Ordering::Relaxed);
            for (slot, hash) in LOG_ROW_HASH[view].iter().zip(hashes.iter()) {
                slot.store(*hash, Ordering::Relaxed);
            }
            term(b"gfx-log: view ");
            term(&[b'0' + view as u8]);
            term(b" ");
            term(if status == Status::Retired {
                b"retired" as &[u8]
            } else {
                b"copied"
            });
            term(b"; rows ");
            term(&[b'0' + first as u8, b'-', b'0' + last as u8]);
            term(b" damaged\n");
            if status == Status::Retired {
                sora::RefreshOutcome::RETIRED
            } else {
                sora::RefreshOutcome::ALIVE
            }
        }
        Some(_) => {
            let _ = handle_close(channel);
            log(b"gfx-log: view rejected; view disabled\n");
            sora::RefreshOutcome::DEAD
        }
        None => {
            let _ = handle_close(channel);
            log(b"gfx-log: view failed; view disabled\n");
            sora::RefreshOutcome::DEAD
        }
    }
}

/// Refresh every console view once; the pass stays alive when at least one view answered,
/// and names a retirement when any present retired. The mirror disables only when every
/// view is gone. — TIMBERDOODLE
fn refresh_all(quiet: bool) -> sora::RefreshOutcome {
    let mut outcome = sora::RefreshOutcome::DEAD;
    for view in 0..kumo_graphics::VIEW_COUNT {
        let view_outcome = console_refresh(view, quiet);
        outcome.alive |= view_outcome.alive;
        outcome.retired |= view_outcome.retired;
    }
    outcome
}

fn gfx_log(cmd: &kumoza::Command) {
    match cmd.args.first().map(|arg| arg.as_str()) {
        None => {
            if !refresh_all(false).alive {
                fail_status(1);
            }
        }
        Some("on") => {
            LOG_MIRROR.store(true, Ordering::Relaxed);
            term(b"gfx-log: mirror on\n");
            let _ = refresh_all(false);
        }
        Some("off") => {
            LOG_MIRROR.store(false, Ordering::Relaxed);
            term(b"gfx-log: mirror off\n");
        }
        _ => usage(b"usage: gfx-log [on|off]\n"),
    }
}

/// C3 S3-b: hand the keyboard to the terminal view while it holds focus. Flag-gated so the
/// F2 acceptance proof (two input consumers) keeps working unchanged. — TIMBERDOODLE
fn gfx_term(cmd: &kumoza::Command) {
    match cmd.args.first().map(|arg| arg.as_str()) {
        Some("on") => {
            TERMINAL_MODE.store(true, Ordering::Relaxed);
            term(b"gfx-term: on\n");
        }
        Some("off") => {
            TERMINAL_MODE.store(false, Ordering::Relaxed);
            term(b"gfx-term: off\n");
        }
        _ => usage(b"usage: gfx-term on|off\n"),
    }
}

fn graphics_client(cmd: &kumoza::Command, initrd: Handle, cwd: &WorkingDirectory) {
    use kumo_graphics::VIEW_COUNT;
    let bitmap = cmd.name == "bmp-view";
    let name = cmd.name.as_bytes();
    if cmd.args.len() > usize::from(bitmap) {
        usage(if bitmap {
            b"usage: bmp-view [path]\n"
        } else {
            b"usage: gfx-client\n"
        });
        return;
    }
    let file = if let Some(path) = cmd.args.first() {
        let path = match cwd.resolve(path) {
            Ok(path) => path,
            Err(_) => {
                usage(b"bmp-view: invalid path\n");
                return;
            }
        };
        match bitmap_file(initrd, path.as_bytes()) {
            Ok(file) => Some(file),
            Err(message) => {
                log(message);
                fail_status(1);
                return;
            }
        }
    } else {
        None
    };
    let bootstrap = if bitmap {
        let (send, child) = channel_create_pair();
        if send == u64::MAX || child == u64::MAX {
            for raw in [send, child] {
                if raw != u64::MAX {
                    let _ = handle_close(Handle(raw as u32));
                }
            }
            if let Some((file, _)) = file {
                let _ = handle_close(file);
            }
            log(b"bmp-view: bootstrap unavailable\n");
            fail_status(1);
            return;
        }
        Some((Handle(send as u32), Handle(child as u32)))
    } else {
        None
    };
    let close_inputs = || {
        if let Some((file, _)) = file {
            let _ = handle_close(file);
        }
        if let Some((send, child)) = bootstrap {
            let _ = handle_close(send);
            let _ = handle_close(child);
        }
    };
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
    if selected.is_none() {
        if let Some(view) = wait_for_released() {
            if let Some((channel, vmo)) = regrant_view(view) {
                GRAPHICS_RELEASED[view].store(false, Ordering::Relaxed);
                GRAPHICS_ASYNC[view].store(false, Ordering::Relaxed);
                selected = Some((view, channel, vmo));
            }
        }
    }
    let Some((view, channel, vmo)) = selected else {
        log(name);
        log(b": no graphics view available\n");
        close_inputs();
        fail_status(1);
        return;
    };
    if vmo.0 == 0 {
        let _ = handle_close(channel);
        GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
        log(name);
        log(b": no surface available\n");
        close_inputs();
        fail_status(1);
        return;
    }
    log(name);
    log(b": view ");
    log(&[b'0' + view as u8]);
    log(b" handed to a client process\n");
    let argument = if let Some((send, child)) = bootstrap {
        let header = bmp_view::file_header(file.map_or(0, |(_, len)| len)).unwrap();
        let queued = if let Some((file, _)) = file {
            channel_write_with_handle(send, header.as_ptr(), header.len(), file) == 0
        } else {
            channel_write(send, header.as_ptr(), header.len()) == 0
        };
        let presenter_queued = queued
            && channel_write_with_handle(
                send,
                bmp_view::PRESENTER_TAG.as_ptr(),
                bmp_view::PRESENTER_TAG.len(),
                channel,
            ) == 0;
        if !presenter_queued {
            if !queued {
                if let Some((file, _)) = file {
                    let _ = handle_close(file);
                }
            }
            let _ = handle_close(send);
            let _ = handle_close(child);
            let _ = handle_close(channel);
            let _ = handle_close(vmo);
            log(b"bmp-view: bootstrap queue failed\n");
            GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
            fail_status(1);
            return;
        }
        let _ = handle_close(send);
        child
    } else {
        channel
    };
    let flags = (ProcessRunFlags::TRANSFER_ARG | ProcessRunFlags::TRANSFER_ARG2).bits();
    let ran = run_elf_threads(
        initrd,
        if bitmap {
            BMP_VIEW_PATH
        } else {
            GFX_CLIENT_PATH
        }
        .as_bytes(),
        argument.0 as u64,
        vmo.0 as u64,
        flags,
        name,
        0,
        None,
    );
    if !ran {
        // An admitted child owns the handles even if it exited non-zero; only an admission
        // failure leaves them with Sora.
        if handle_koid(argument) != u64::MAX {
            let _ = handle_close(argument);
            let _ = handle_close(vmo);
        }
        fail_status(1);
    }
    // The synchronous child has exited (or admission failed); the grant is gone, so a later
    // invocation may regrant this view. — TIMBERDOODLE
    GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
}

/// Admit one presentation client per supervisor-held view, concurrently (F2a-1). Each child
/// is launched ASYNC, so the builtin returns only after every admission is staged; the
/// children then paint their own server-owned views and exit on their own. This reports
/// admission only: a child's later failure is its own log line, not this command's status.
/// Clients never choose a view and never name coordinates. On a board where drv-gpu owns one
/// view, only the remaining view is admitted; the QEMU proof uses both. — TIMBERDOODLE
fn graphics_clients(cmd: &kumoza::Command, initrd: Handle) {
    use kumo_graphics::VIEW_COUNT;
    if !cmd.args.is_empty() {
        usage(b"usage: gfx-clients\n");
        return;
    }
    let mut launched = 0usize;
    let mut failed = false;
    for view in 0..VIEW_COUNT {
        let channel = Handle(GRAPHICS_CHANNEL[view].swap(0, Ordering::Relaxed));
        if channel.0 == 0 {
            continue;
        }
        let vmo = Handle(GRAPHICS_VMO[view].swap(0, Ordering::Relaxed));
        if vmo.0 == 0 {
            let _ = handle_close(channel);
            log(b"gfx-clients: a view has no surface\n");
            failed = true;
            continue;
        }
        let flags = (ProcessRunFlags::ASYNC
            | ProcessRunFlags::TRANSFER_ARG
            | ProcessRunFlags::TRANSFER_ARG2)
            .bits();
        let mut kept = None;
        let ran = run_elf_threads(
            initrd,
            GFX_CLIENT_PATH.as_bytes(),
            channel.0 as u64,
            vmo.0 as u64,
            flags,
            b"gfx-client",
            0,
            Some(&mut kept),
        );
        if ran {
            launched += 1;
            GRAPHICS_ASYNC[view].store(true, Ordering::Relaxed);
            if let Some(process) = kept {
                GRAPHICS_ASYNC_PROCESS[view].store(process.0, Ordering::Relaxed);
                // One log record, so the handoff line is contiguous in the console transcript
                // and the smoke's exact-marker check can see it. — TIMBERDOODLE
                notice(&[
                    b"gfx-clients: view ",
                    &[b'0' + view as u8],
                    b" handed to an async client process\n",
                ]);
            } else {
                // The child is admitted but Sora has no handle to wait on. Keep the view
                // leased rather than stealing a live grant; the command reports the failure.
                log(b"gfx-clients: async client handle unavailable\n");
                failed = true;
            }
        } else {
            // An admitted child owns the handles even if it exits non-zero; only an admission
            // failure leaves them with Sora.
            if handle_koid(channel) != u64::MAX {
                let _ = handle_close(channel);
                let _ = handle_close(vmo);
            }
            GRAPHICS_RELEASED[view].store(true, Ordering::Relaxed);
            failed = true;
        }
    }
    if launched == 0 {
        log(b"gfx-clients: no graphics view available\n");
        fail_status(1);
    } else if failed {
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
    "  adrp x11, {job_slot}",
    "  add  x11, x11, :lo12:{job_slot}",
    "  str  x10, [x11]",
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
    job_slot = sym ROOT_JOB_HANDLE,
);

fn log(msg: &[u8]) {
    debug_write(msg.as_ptr(), msg.len());
}

/// The log view's own records: terminal output, not kernel-log entries. The view reads the
/// same ring it would otherwise narrate into, so its bookkeeping must not feed itself.
/// — TIMBERDOODLE
fn term(msg: &[u8]) {
    let _ = kumo_rt::debug_write(msg.as_ptr(), msg.len());
}

/// Set when the serve loop is about to draw its first prompt (J559). Until then everything Sora
/// prints is a boot record: its own diagnostics and the output of autoexec's smoke programs go
/// to the kernel log, where `dmesg` shows them, instead of across the console. From then on
/// `debug_write` is the terminal again, and serve-loop diagnostics say so with [`diag`].
/// — TIMBERDOODLE
static SERVING: AtomicBool = AtomicBool::new(false);

/// Sora's console write: the kernel log while booting, the terminal once serving.
fn debug_write(ptr: *const u8, len: usize) -> u64 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    if SERVING.load(Ordering::Relaxed) {
        kumo_rt::debug_write(ptr, len)
    } else {
        kumo_rt::log_write(kumo_abi::sys::log_level::INFO, bytes)
    }
}

/// C3 S3-a: write shell output — prompts, ttyd echo and command output — to the console and
/// to the terminal view's ring. Sora's own records (`log`/`notice`/`diag`) stay out of the
/// terminal by construction. — TIMBERDOODLE
fn shell_write(ptr: *const u8, len: usize) -> u64 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let mut seq = SHELL_RING_SEQ.load(Ordering::Relaxed);
    for &byte in bytes {
        SHELL_RING[(seq as usize) % SHELL_RING.len()].store(byte, Ordering::Relaxed);
        seq = seq.wrapping_add(1);
    }
    SHELL_RING_SEQ.store(seq, Ordering::Relaxed);
    debug_write(ptr, len)
}

fn shell_output(message: &[u8]) {
    let _ = shell_write(message.as_ptr(), message.len());
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

/// Arm one delayed GPU session-regrant attempt on the serve port, the same one-shot timer
/// shape the ttyd restart and first-prompt paths use. — TIMBERDOODLE
//j720
/// Arm the next clock-watch sample with the same one-shot timer shape as the other serve
/// timers. — TIMBERDOODLE
fn arm_clock_watch(port: Handle) -> Option<(Handle, Handle)> {
    if !GPU_CLOCK_WATCH_ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    //j721
    GPU_CLOCK_WATCH_DUE_NS.store(
        kumo_rt::clock_get() + GPU_CLOCK_WATCH_PERIOD_NS,
        Ordering::Relaxed,
    );
    //j728
    GPU_CLOCK_POPS_AT_ARM.store(GPU_CLOCK_POPS.load(Ordering::Relaxed), Ordering::Relaxed);
    arm_gpu_restart(port, GPU_CLOCK_WATCH_PERIOD_NS)
}

//j720
/// Read and log one GPU clock sample from the read-only CRU mapping: the source/divider
/// decode plus the raw select, gate and GPLL words, so a moving clock is visible in the
/// same log as the JOB completion times. — TIMBERDOODLE
fn log_gpu_clock_sample(pops: u64, prep_ns: u64) {
    if !GPU_CLOCK_WATCH_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    //j728
    let log_start = kumo_rt::clock_get();
    let read = |offset: u64| unsafe { ((GPU_CLOCK_CRU_VA + offset) as *const u32).read_volatile() };
    let r0 = kumo_rt::clock_get();
    let clksel158 = read(GPU_CLOCK_O_CLKSEL158);
    let clksel161 = read(GPU_CLOCK_O_CLKSEL161);
    let gate66 = read(GPU_CLOCK_O_GATE66);
    let gate67 = read(GPU_CLOCK_O_GATE67);
    let gpll0 = read(GPU_CLOCK_O_GPLL0);
    let r1 = kumo_rt::clock_get();
    let clock = sora::decode_gpu_clock(clksel158, clksel161);
    log(b"clk-gpu: clksel158=");
    log_hex(u64::from(clksel158));
    log(b" gate66=");
    log_hex(u64::from(gate66));
    log(b" gate67=");
    log_hex(u64::from(gate67));
    log(b" gpll0=");
    log_hex(u64::from(gpll0));
    log(b" src=");
    log(clock.parent_name().as_bytes());
    log(b" div=");
    log_hex(u64::from(clock.divider));
    match clock.rate_mhz(GPU_CLOCK_GPLL_MHZ) {
        Some(mhz) => {
            log(b" rate=");
            log_hex(u64::from(mhz));
            log(b"MHz");
        }
        None => log(b" rate=unknown"),
    }
    //j728
    // Decompose the sample's own cycle: pops on the serve port since the arm, the
    // unbind/close prep, the CRU register reads, and the logging so far, so a late
    // sample names its hop. — TIMBERDOODLE
    let now = kumo_rt::clock_get();
    let due = GPU_CLOCK_WATCH_DUE_NS.load(Ordering::Relaxed);
    let wake = GPU_CLOCK_WAKE_NS.load(Ordering::Relaxed);
    log(b" pops=");
    log_hex(pops);
    log(b" prep_ns=");
    log_hex(prep_ns);
    log(b" read_ns=");
    log_hex(r1.saturating_sub(r0));
    log(b" log_ns=");
    log_hex(now.saturating_sub(log_start));
    log(b" late_ns=");
    log_hex(now.saturating_sub(due));
    log(b" wake_ns=");
    log_hex(wake.saturating_sub(due));
    log(b"\n");
}

fn arm_gpu_restart(port: Handle, delay_ns: u64) -> Option<(Handle, Handle)> {
    let raw = timer_create(delay_ns);
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

/// Arm the next mirror wake on `port` with the pass's cadence: a retired present chains
/// through the gate's own frame wait, and the idle poll covers quiet tails. — TIMBERDOODLE
fn arm_log_mirror(port: Handle, delay_ns: u64) -> Option<(Handle, Handle)> {
    let raw = timer_create(delay_ns);
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
    let _ = sora::relay_console_output(&prompt, |chunk| shell_write(chunk.as_ptr(), chunk.len()));
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
/// The register window covers the xHCI block and the DWC3 globals at +0xC100..0xC6FF, so host
/// mode and PHY policy can be programmed; enumeration polls, and every controller now has its
/// real GIC line. — TIMBERDOODLE
const RK3588_USB1_XHCI_MMIO_BASE: u64 = 0xfc40_0000;
const RK3588_HOST2_XHCI_MMIO_BASE: u64 = 0xfcd0_0000;
const RK3588_XHCI_REGISTER_MMIO_LEN: u64 = 0xD000;
/// GIC INTIDs for the other two xHCI controllers. `rk3588-base.dtsi`/`rk3588-extra.dtsi` list
/// usb_host0/1/2 as GIC_SPI 220/221/222, and an SPI's INTID is its number + 32 (usb0's 220 → 252
/// matches the DTB-derived config). Without a real IRQ a driver has to busy-poll for reports, and
/// at driver priority that starves the whole system — every controller gets its line.
const RK3588_USB1_XHCI_IRQ: u32 = 253;
const RK3588_HOST2_XHCI_IRQ: u32 = 254;

/// The live opi5 DTB's snps quirks, per controller. Host-mode init must honor them; the
/// u1/u2-entry pair is carried for the record even though mainline host ignores it.
/// — TIMBERDOODLE
const RK3588_USB0_DWC3_FLAGS: u32 = kumo_xhci::FLAG_DWC3
    | kumo_xhci::QUIRK_DIS_ENBLSLPM
    | kumo_xhci::QUIRK_DIS_U1_ENTRY
    | kumo_xhci::QUIRK_DIS_U2_ENTRY
    | kumo_xhci::QUIRK_DIS_U2_FREECLK_EXISTS
    | kumo_xhci::QUIRK_DIS_DEL_PHY_POWER_CHG
    | kumo_xhci::QUIRK_DIS_TX_IPGAP_LINECHECK;
const RK3588_USB1_DWC3_FLAGS: u32 = kumo_xhci::FLAG_DWC3
    | kumo_xhci::QUIRK_DIS_ENBLSLPM
    | kumo_xhci::QUIRK_DIS_U2_FREECLK_EXISTS
    | kumo_xhci::QUIRK_DIS_DEL_PHY_POWER_CHG
    | kumo_xhci::QUIRK_DIS_TX_IPGAP_LINECHECK;
const RK3588_HOST2_DWC3_FLAGS: u32 = RK3588_USB1_DWC3_FLAGS | kumo_xhci::QUIRK_DIS_RXDET_INP3;

/// U5: bootstrap tag preceding the keyboard-channel writer handed to a drv-xhci instance.
const XHCI_KEYBOARD_BOOTSTRAP_TAG: u8 = b'k';
/// ...and the pointer channel. A USB boot mouse emits the same 3-byte `[buttons, dx, dy]` frame
/// the i2c-hid path does, so it joins the existing `PointerState` pipeline unchanged.
const XHCI_MOUSE_BOOTSTRAP_TAG: u8 = b'm';
/// J545: ...and the storage control channel. A driver that finds a USB stick offers its volume
/// on it, and Sora answers with one block channel per `fatfs` instance it starts for the volume.
const XHCI_STORAGE_BOOTSTRAP_TAG: u8 = b'b';
/// Sora's ends of the drivers' storage control channels, one per storage slot
/// (`sora::StorageTransport`: usb0, usb1, host2, then NVMe since J803); 0 = none.
static STORAGE_CONTROLS: [AtomicU32; sora::STORAGE_DEVICES] =
    [const { AtomicU32::new(0) }; sora::STORAGE_DEVICES];
// Private USB parent custody uses the existing per-controller control channels.
// Ledger aliases stay in Sora's handle table until reset, including peer death.
// — TIMBERDOODLE
struct UsbDmaCustodyCell(core::cell::UnsafeCell<[kumo_xhci::DmaCustodyLedger; sora::NVME_STORAGE]>);
// SAFETY: Sora has one receive thread; ledger borrows end before any syscall.
unsafe impl Sync for UsbDmaCustodyCell {}
static USB_DMA_CUSTODY: UsbDmaCustodyCell = UsbDmaCustodyCell(core::cell::UnsafeCell::new(
    [const { kumo_xhci::DmaCustodyLedger::new() }; sora::NVME_STORAGE],
));
static USB_CONTROL_KOIDS: [AtomicU64; sora::NVME_STORAGE] =
    [const { AtomicU64::new(0) }; sora::NVME_STORAGE];
static USB_CUSTODY_INITRD: AtomicU32 = AtomicU32::new(0);
fn configure_usb_custody(index: usize, controller: u64) -> bool {
    if index >= sora::NVME_STORAGE {
        return false;
    }
    // SAFETY: no reference escapes this startup-only operation.
    unsafe {
        (&mut *USB_DMA_CUSTODY.0.get())[index]
            .configure(controller)
            .is_ok()
    }
}
/// Rejected/unacknowledged imports may close; an alias already accepted by any
/// USB ledger never does. Keep even an unexpected duplicate handle safe. — TIMBERDOODLE
fn close_unheld_usb_handle(raw: u64) {
    if raw == 0 || raw > u32::MAX as u64 {
        return;
    }
    // SAFETY: one Sora receiver; the shared borrow ends before handle_close.
    let held = unsafe {
        (&*USB_DMA_CUSTODY.0.get())
            .iter()
            .any(|l| l.contains_handle(raw as u32))
    };
    if !held {
        let _ = handle_close(Handle(raw as u32));
    }
}

fn accept_usb_custody(index: usize, frame: &[u8], raw: u64, control: Handle) {
    use kumo_xhci::{CustodyError, DmaMemory};
    let memory = DmaMemory::decode(frame);
    let result =
        if index >= sora::NVME_STORAGE || raw == 0 || raw == u64::MAX || raw > u32::MAX as u64 {
            Err(CustodyError::Handle)
        } else {
            memory.and_then(|memory| {
                // SAFETY: single receive thread; store the strong handle before ACK.
                let ledger = unsafe { &mut (&mut *USB_DMA_CUSTODY.0.get())[index] };
                let ack = ledger.accept(memory, raw as u32)?;
                Ok((ack, memory, ledger.count()))
            })
        };
    match result {
        Ok((ack, memory, count)) => {
            let message = alloc::format!(
                "XHCI DMA HOLD controller={:#x} seq={} bytes={} held={}\n",
                memory.controller,
                memory.sequence,
                memory.bytes,
                count
            );
            notice(&[message.as_bytes()]);
            if channel_write(control, ack.as_ptr(), ack.len()) != 0 {
                notice(&[b"xhci: custody ACK failed; parent VMO alias retained\n"]);
            }
        }
        Err(_) => {
            // This allocation was not acknowledged/published. Existing aliases stay held.
            close_unheld_usb_handle(raw);
            let mut nack = *b"XDN1\0\0\0\0";
            if frame.len() >= 8 {
                nack[4..].copy_from_slice(&frame[4..8]);
            }
            let _ = channel_write(control, nack.as_ptr(), nack.len());
            notice(&[b"xhci: DMA custody refused; no publication ACK\n"]);
        }
    }
}

static NVME_CONTROL_KOID: AtomicU64 = AtomicU64::new(0);
static NVME_UNPROVEN: AtomicBool = AtomicBool::new(false);
static NVME_WATCH_PORT: AtomicU32 = AtomicU32::new(0);

/// The removable volume Sora knows, mounted at `/media/<name>` (DESIGN/012 §2.1, J545).
#[derive(Clone, Copy)]
struct Media {
    /// The `/media/<name>` component the shell reaches it by.
    name: sora::MediaName,
    ready: drv_blk::MediaReady,
    /// The offering driver's control channel: where a new client's block channel goes.
    control: Handle,
    /// Its storage controller, the `N` of `/dev/disk<N>s1` (J766).
    device: usize,
}

/// J766: the volume each storage controller's driver offers, mounted or not. `mount` mounts one
/// at a time at `/media/<name>`, because the shell's routes serve one volume.
#[derive(Clone, Copy)]
struct Device {
    ready: drv_blk::MediaReady,
    control: Handle,
}

static DEVICES: Shared<[Option<Device>; sora::STORAGE_DEVICES]> =
    Shared(core::cell::UnsafeCell::new([None; sora::STORAGE_DEVICES]));

fn set_device(index: usize, device: Option<Device>) {
    let mut devices = DEVICES.get();
    devices[index] = device;
    DEVICES.set(devices);
}

/// Mount the offered volume of controller `index` as `/media/<name>`, and say so.
fn mount_device(index: usize, device: Device, name: sora::MediaName) {
    set_media(Some(Media {
        name,
        ready: device.ready,
        control: device.control,
        device: index,
    }));
    notice(&[
        b"media: ",
        &sora::disk_path(index),
        b" mounted on /media/",
        name.as_bytes(),
        if device.ready.read_only {
            b" (fat32, read-only)\n" as &[u8]
        } else {
            b" (fat32)\n"
        },
    ]);
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
    USB_CUSTODY_INITRD.store(initrd.0, Ordering::Relaxed);
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
        STORAGE_CONTROLS[index].store(sora_end as u32, Ordering::Relaxed);
        driver_end
    };

    match xhci_usb0_probe_from_dtb(dtb_vmo, dtb_phys) {
        Some(config) => spawn_xhci(
            initrd,
            root_resource,
            config.with_flags(RK3588_USB0_DWC3_FLAGS),
            writer_for(have_channel),
            mouse_for(have_mouse),
            storage_for(0),
            0,
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
        )
        .with_flags(RK3588_USB1_DWC3_FLAGS),
        writer_for(have_channel),
        mouse_for(have_mouse),
        storage_for(1),
        1,
    );
    spawn_xhci(
        initrd,
        root_resource,
        kumo_xhci::XhciProbeConfig::new(
            RK3588_HOST2_XHCI_MMIO_BASE,
            RK3588_XHCI_REGISTER_MMIO_LEN,
            RK3588_HOST2_XHCI_IRQ,
            kumo_xhci::XHCI_NO_STREAM_ID,
        )
        .with_flags(RK3588_HOST2_DWC3_FLAGS),
        writer_for(have_channel),
        mouse_for(have_mouse),
        storage_for(2),
        2,
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
/// Exercise real offline DeviceCtx syscalls before the trusted display diagnostic.
/// This prepares no live domain and grants no extra authority to drv-fb. — KESTREL
fn verify_offline_display_context(root: Handle, boot: &BootInfo) -> Option<Handle> {
    use kumo_abi::sys::IoMmuKind;
    use kumo_rt::{device_ctx_create, device_vmar_map, device_vmar_unmap, iommu_from_resource};
    if boot.board_id() != "orange-pi-5-plus" {
        return None;
    }
    let result = (|| {
        let base = 0xfdd9_7e00;
        let len = 0x200;
        if syscall_handle(iommu_from_resource(root, IoMmuKind::SmmuV3, base, len)).is_some() {
            return None;
        }
        let iommu = syscall_handle(iommu_from_resource(root, IoMmuKind::RockchipV2, base, len))?;
        if syscall_handle(iommu_from_resource(root, IoMmuKind::RockchipV2, base, len)).is_some()
            || syscall_handle(device_ctx_create(iommu, 1)).is_some()
        {
            return None;
        }
        let ctx = syscall_handle(device_ctx_create(iommu, 0))?;
        if syscall_handle(device_ctx_create(iommu, 0)).is_some() {
            return None;
        }
        let span = boot.framebuffer.mapping_len()?;
        let source = syscall_handle(kumo_rt::resource_mint_mmio(
            root,
            boot.framebuffer.phys,
            span,
        ))?;
        let iova = 0x6000_0000;
        if device_vmar_map(ctx, source, 0, span, iova, Rights::READ | Rights::WRITE) == 0
            || device_vmar_map(ctx, source, 0, span - 4096, iova, Rights::READ) == 0
            || device_vmar_map(ctx, source, 0, span, iova, Rights::READ) != 0
        {
            return None;
        }
        let duplicate = syscall_handle(handle_duplicate(ctx, Rights::MAP | Rights::WAIT))?;
        if handle_close(source) != 0
            || handle_close(ctx) != 0
            || device_vmar_unmap(duplicate, iova, span - 4096) == 0
            || device_vmar_unmap(duplicate, iova, span) != 0
        {
            return None;
        }
        // Last close must discard even a still-mapped offline owner, permitting a
        // new sole context on the same IOMMU capability. — KESTREL
        let source = syscall_handle(kumo_rt::resource_mint_mmio(
            root,
            boot.framebuffer.phys,
            span,
        ))?;
        if device_vmar_map(duplicate, source, 0, span, iova, Rights::READ) != 0
            || handle_close(source) != 0
            || handle_close(duplicate) != 0
        {
            return None;
        }
        let again = syscall_handle(device_ctx_create(iommu, 0))?;
        if handle_close(again) != 0 {
            return None;
        }
        Some(iommu)
    })();
    if let Some(iommu) = result {
        notice(&[b"display: D1d-b offline DeviceCtx: READ/GOP maps, authority/range denials, closed source, duplicate survival, final-close/recreate OK; no paging\n"]);
        Some(iommu)
    } else {
        notice(&[b"display: D1d-b offline DeviceCtx refused; no paging proof\n"]);
        None
    }
}

/// Prove the successful activation SVC keeps live EL0 vectors across table/hardware
/// work, not just across a validation refusal. — KESTREL
#[cfg(target_arch = "aarch64")]
fn activate_display_with_vectors(ctx: Handle) -> bool {
    let mut status = u64::from(ctx.0);
    let (a, b, c, d): (u64, u64, u64, u64);
    unsafe {
        core::arch::asm!(
            "movi v0.16b, #0x5a", "movi v31.16b, #0xa5", "svc #0",
            "umov x9, v0.d[0]", "umov x10, v0.d[1]", "umov x11, v31.d[0]", "umov x12, v31.d[1]",
            inlateout("x0")status,in("x8")kumo_abi::Syscall::DeviceCtxActivate as u64,
            lateout("x9")a,lateout("x10")b,lateout("x11")c,lateout("x12")d,
            clobber_abi("C"),options(nostack),
        );
    }
    status as u32 as i32 == 0
        && a == 0x5a5a5a5a5a5a5a5a
        && b == a
        && c == 0xa5a5a5a5a5a5a5a5
        && d == c
}
#[cfg(not(target_arch = "aarch64"))]
fn activate_display_with_vectors(_ctx: Handle) -> bool {
    false
}

/// Sora's recovery recipe retains IOMMU/GOP authority, never a spare context handle.
/// The driver chooses presentation; this broker reconstructs only approved GOP mappings. — KESTREL
struct DisplayBroker {
    channel: Handle,
    iommu: Handle,
    gop: Handle,
    phys: u64,
    span: u64,
}
impl DisplayBroker {
    fn new(iommu: Handle, gop: Handle, boot: &BootInfo) -> Option<(Self, Handle)> {
        let gop = syscall_handle(handle_duplicate(
            gop,
            Rights::READ | Rights::DUPLICATE | Rights::TRANSFER,
        ))?;
        let (server, client) = channel_create_pair();
        let channel = syscall_handle(server)?;
        let client = syscall_handle(client)?;
        Some((
            Self {
                channel,
                iommu,
                gop,
                phys: boot.framebuffer.phys,
                span: boot.framebuffer.mapping_len()?,
            },
            client,
        ))
    }
    fn recover_gop(&self, front: u64) -> Option<Handle> {
        let raw = kumo_rt::device_ctx_create(self.iommu, 0);
        let Some(ctx) = syscall_handle(raw) else {
            notice(&[alloc::format!(
                "display: GOP recovery context create refused {}\n",
                raw as u32 as i32
            )
            .as_bytes()]);
            return None;
        };
        for iova in [self.phys, front] {
            let status = kumo_rt::device_vmar_map(ctx, self.gop, 0, self.span, iova, Rights::READ);
            if status != 0 {
                notice(&[alloc::format!(
                    "display: GOP recovery map {:#x} refused {}\n",
                    iova,
                    status
                )
                .as_bytes()]);
                let _ = handle_close(ctx);
                return None;
            }
        }
        let status = kumo_rt::device_ctx_activate(ctx);
        if status != 0 {
            notice(&[
                alloc::format!("display: GOP recovery activation refused {}\n", status).as_bytes(),
            ]);
            let _ = handle_close(ctx);
            return None;
        }
        Some(ctx)
    }
    fn serve(&self, initrd: Handle) {
        let mut request = [0u8; 8];
        let (n, raw) =
            kumo_rt::channel_read_with_handle(self.channel, request.as_mut_ptr(), request.len());
        let Some(ctx) = syscall_handle(raw).filter(|h| h.0 != 0) else {
            return;
        };
        let next = if n == 1 && request[0] == b'K' {
            self.kill_holder(initrd, ctx)
                .and_then(|()| self.recover_gop(0x60000000))
        } else if n == 5 && request[0] == b'F' {
            let front = u64::from(u32::from_le_bytes(request[1..5].try_into().unwrap()));
            let closed = handle_close(ctx) == 0;
            if closed
                && front != 0
                && front & 4095 == 0
                && front
                    .checked_add(self.span)
                    .is_some_and(|end| end <= 1 << 32)
            {
                self.recover_gop(front)
            } else {
                None
            }
        } else {
            let _ = handle_close(ctx);
            None
        };
        let (tag, grant) = if let Some(next) = next {
            (b"R", next)
        } else {
            (b"E", Handle(0))
        };
        if channel_write_with_handle(self.channel, tag.as_ptr(), 1, grant) != 0 && grant.0 != 0 {
            let _ = handle_close(grant);
        }
    }
    fn kill_holder(&self, initrd: Handle, ctx: Handle) -> Option<()> {
        let (send, child) = channel_create_pair();
        let send = syscall_handle(send)?;
        let child = syscall_handle(child)?;
        if channel_write_with_handle(send, b"H".as_ptr(), 1, ctx) != 0 {
            let _ = handle_close(ctx);
            return None;
        }
        let mut process = None;
        let flags = (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG2).bits();
        if !run_elf_threads(
            initrd,
            kumo_abi::DRV_FB_PATH.as_bytes(),
            drv_fb::rk3588_scanout::HOLDER_ARG,
            u64::from(child.0),
            flags,
            b"display-holder",
            0,
            Some(&mut process),
        ) {
            return None;
        }
        let process = process?;
        let mut ack = [0u8; 1];
        let n = channel_read(send, ack.as_mut_ptr(), 1);
        if n != 1 || ack[0] != b'H' || handle_koid(ctx) != u64::MAX {
            let _ = process_kill(process);
            return None;
        }
        // Receipt proves the queued grant is installed in the child. No MAP/WAIT duplicate
        // remains in Sora or drv-fb; ProcessKill must trigger actual final-holder teardown. — KESTREL
        if syscall_handle(kumo_rt::device_ctx_create(self.iommu, 0)).is_some() {
            let _ = process_kill(process);
            return None;
        }
        if process_kill(process) != 0 {
            return None;
        }
        let _ = handle_close(process);
        let _ = handle_close(send);
        notice(&[b"display: D1d actual sole context holder killed during DMA; old lineage gone, requesting GOP-only recovery\n"]);
        Some(())
    }
}

/// Prepare the exact three DMA ranges before granting any checked VOP WRITE.
/// Sora retains the IOMMU; the sole context and actual mapped VMOs transfer to drv-fb.
/// The two owned device addresses deliberately differ from their physical backing. — KESTREL
fn prepare_display_context(
    root: Handle,
    boot: &BootInfo,
    iommu: Option<Handle>,
) -> Option<[Handle; 4]> {
    let iommu = iommu?;
    let ctx = syscall_handle(kumo_rt::device_ctx_create(iommu, 0))?;
    let span = boot.framebuffer.mapping_len()?;
    let mut grants = [ctx, Handle(0), Handle(0), Handle(0)];
    let result = (|| {
        grants[1] = syscall_handle(kumo_rt::resource_mint_mmio(
            root,
            boot.framebuffer.phys,
            span,
        ))?;
        let (a, pa) = kumo_rt::vmo_create_contiguous(span);
        grants[2] = syscall_handle(a)?;
        let (b, pb) = kumo_rt::vmo_create_contiguous(span);
        grants[3] = syscall_handle(b)?;
        if pa == 0x60000000 || pb == 0x61000000 {
            return None;
        }
        for (vmo, iova) in [
            (grants[1], boot.framebuffer.phys),
            (grants[2], 0x60000000),
            (grants[3], 0x61000000),
        ] {
            if kumo_rt::device_vmar_map(ctx, vmo, 0, span, iova, Rights::READ) != 0 {
                return None;
            }
        }
        if kumo_rt::device_ctx_activate(ctx) != 0 {
            return None;
        }
        // Exercise the sole holder's checked teardown with live GOP DMA before any
        // driver grant. The physical controller must keep VP2 held across recreation. — KESTREL
        if handle_close(ctx) != 0 {
            return None;
        }
        let ctx = syscall_handle(kumo_rt::device_ctx_create(iommu, 0))?;
        grants[0] = ctx;
        for (vmo, iova) in [
            (grants[1], boot.framebuffer.phys),
            (grants[2], 0x60000000),
            (grants[3], 0x61000000),
        ] {
            if kumo_rt::device_vmar_map(ctx, vmo, 0, span, iova, Rights::READ) != 0 {
                return None;
            }
        }
        if !activate_display_with_vectors(ctx) {
            return None;
        }
        if kumo_rt::device_vmar_map(ctx, grants[1], 0, span, 0x62000000, Rights::READ) != 0
            || kumo_rt::device_vmar_unmap(ctx, 0x62000000, span) != 0
        {
            return None;
        }
        notice(&[b"display: D1d live GOP alias map/unmap passed; both replacements checked while GOP scans out\n"]);
        let mut info = kumo_abi::sys::DeviceCtxInfo {
            fault_record: 0,
            fault_addr: 0,
        };
        if kumo_rt::device_ctx_info(ctx, &mut info) != 0
            || info.fault_record
                != kumo_abi::sys::device_ctx_state::RECORD | kumo_abi::sys::device_ctx_state::LIVE
            || info.fault_addr == 0
        {
            return None;
        }
        Some(())
    })();
    if result.is_none() {
        for h in grants {
            if h.0 != 0 {
                let _ = handle_close(h);
            }
        }
        notice(&[b"display: D1d activation refused; checked VOP WRITE withheld; uncertain backing retained\n"]);
        return None;
    }
    notice(&[b"display: D1d live final-close/recreate and activated READ-only GOP/A/B, A IOVA 0x60000000 and B 0x61000000 differ from backing; SVC SIMD preserved; VOP WRITE grant follows kernel ownership\n"]);
    Some(grants)
}

fn bootstrap_display_probe(
    bootstrap: Handle,
    root: Handle,
    board: &str,
    framebuffer_valid: bool,
    display_live: bool,
) {
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
            power.is_some_and(Power::vop_accessible) && display_live,
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

struct NvmeReadMap {
    physical: u64,
    address: u64,
    len: u64,
    vmo: Handle,
    owned: bool,
}

impl NvmeReadMap {
    fn map(vmo: Handle, physical: u64, address: u64, len: u64, owned: bool) -> Option<Self> {
        // Sora's boot-loaded image is not recorded in the automatic allocator.
        // Use its dedicated temporary slots, as the other live-DTB observers do;
        // automatic placement can overwrite and then unmap the image. — TIMBERDOODLE
        let status = vmar_map(Handle(0), vmo, 0, address, len, VmarFlags::READ.0);
        if status != 0 {
            if owned {
                let _ = handle_close(vmo);
            }
            return None;
        }
        Some(Self {
            physical,
            address,
            len,
            vmo,
            owned,
        })
    }
    fn mmio(root: Handle, physical: u64, address: u64, len: u64) -> Option<Self> {
        let resource = read_only_grant(resource_create_child(root, physical, len, 0, 0));
        if resource.0 == 0 {
            return None;
        }
        let raw = resource_mint_mmio(resource, physical, len);
        let _ = handle_close(resource);
        Self::map(syscall_handle(raw)?, physical, address, len, true)
    }
    fn read(&self, offset: u16) -> u32 {
        if offset & 3 != 0 || u64::from(offset) + 4 > self.len {
            return u32::MAX;
        }
        unsafe { core::ptr::read_volatile((self.address + u64::from(offset)) as *const u32) }
    }
}
impl Drop for NvmeReadMap {
    fn drop(&mut self) {
        let removed = kumo_rt::vmar_unmap(self.address, self.len) == 0;
        let closed = !self.owned || handle_close(self.vmo) == 0;
        if !removed || !closed {
            notice(&[b"nvme: observer mapping cleanup failed\n"]);
        }
    }
}
struct NvmeRootRead(alloc::vec::Vec<NvmeReadMap>);
impl kumo_pcie::RegisterRead for NvmeRootRead {
    fn read32(&mut self, physical: u64) -> u32 {
        self.0
            .iter()
            .find_map(|p| {
                let offset = physical.checked_sub(p.physical)?;
                (offset + 4 <= p.len).then(|| p.read(offset as u16))
            })
            .unwrap_or(u32::MAX)
    }
}
struct NvmeConfigRead<'a>(&'a NvmeReadMap);
impl kumo_pcie::endpoint::ConfigRead for NvmeConfigRead<'_> {
    fn read32(&mut self, offset: u16) -> u32 {
        self.0.read(offset)
    }
}

/// Broker discovery is read-only; the child gets only one BAR Resource, no shared
/// control, root/config or IRQ capability. Every controller mint goes through the
/// live HAL guard. The live DTB selects the board, including headless boots.
/// — TIMBERDOODLE
fn nvme_startup(root: Handle, dtb_vmo: Handle, bootinfo: Handle) -> Option<drv_nvme::Startup> {
    if dtb_vmo.0 == 0 || bootinfo.0 == 0 {
        return None;
    }
    let mut field = [0u8; 8];
    let offset = core::mem::offset_of!(BootInfo, platform.dtb) as u64;
    if vmo_read(bootinfo, offset, field.as_mut_ptr(), field.len()) != 0 {
        return None;
    }
    let dtb_phys = u64::from_le_bytes(field);
    if dtb_phys == 0 {
        return None;
    }
    let offset = dtb_phys & 4095;
    if offset > 4088 {
        return None;
    }
    let header_map = NvmeReadMap::map(dtb_vmo, 0, 0x4000_0000, 4096, false)?;
    let header =
        unsafe { core::ptr::read_unaligned((header_map.address + offset) as *const [u8; 8]) };
    drop(header_map);
    if u32::from_be_bytes(header[..4].try_into().ok()?) != 0xd00d_feed {
        return None;
    }
    let total = u32::from_be_bytes(header[4..].try_into().ok()?) as u64;
    if !(40..=16 * 1024 * 1024).contains(&total) {
        return None;
    }
    let bytes = NvmeReadMap::map(
        dtb_vmo,
        0,
        0x4000_0000,
        (offset + total + 4095) & !4095,
        false,
    )?;
    let dtb = unsafe {
        core::slice::from_raw_parts((bytes.address + offset) as *const u8, total as usize)
    };
    let topology = kumo_pcie::discover_rk3588_pcie30x4(dtb)?;
    drop(bytes);
    let mut root_io = NvmeRootRead(alloc::vec::Vec::new());
    for (slot, (base, len)) in kumo_pcie::STATUS_WINDOWS.into_iter().enumerate() {
        root_io.0.push(NvmeReadMap::mmio(
            root,
            base,
            0x4200_0000 + slot as u64 * 0x1_0000,
            len,
        )?);
    }
    let report = kumo_pcie::probe(topology, &mut root_io);
    let page = kumo_pcie::endpoint::select_config_page(&report).ok()?;
    let config = NvmeReadMap::mmio(root, page.physical, 0x4207_0000, 4096)?;
    let endpoint = kumo_pcie::endpoint::probe(&report, &mut NvmeConfigRead(&config)).ok()?;
    let physical = kumo_pcie::nvme::select_register_page(&report, &endpoint).ok()?;
    let bar = NvmeReadMap::mmio(root, physical, 0x4208_0000, 4096)?;
    let snapshot = kumo_pcie::nvme::probe(&report, &endpoint, &mut |off| bar.read(off)).ok()?;
    let startup = drv_nvme::Startup {
        session: 1,
        bar: physical,
        cap: snapshot.cap,
        vs: snapshot.vs,
        id: endpoint.id,
        class_revision: endpoint.class_revision,
        root_command: report.dbi.as_ref()?.command as u16,
        endpoint_command: endpoint.command,
        msix_control: endpoint.msix.map_or(0, |m| m.control),
        // NV6: the DTB's legacy SPI as a GIC INTID, the function's Interrupt Pin and
        // the client's inherited INTx mask. Validation refuses a closed INTA; Sora
        // never writes the client mask. — TIMBERDOODLE
        irq: report.topology.spis.legacy.checked_add(32)?,
        pin: endpoint.interrupt_pin(),
        legacy_mask: report.client.as_ref()?.intr_mask_legacy,
        mode: drv_nvme::MODE_PROOF,
    };
    startup.validate().ok()?;
    Some(startup)
}

fn nvme_wait_source(port: Handle, wanted: u64) -> bool {
    for _ in 0..8 {
        let got = port_wait(port);
        if got == wanted {
            return true;
        }
        if got == 0 {
            return false;
        }
    }
    false
}
fn nvme_receive(port: Handle, channel: Handle, frame: &mut [u8]) -> Option<(usize, u64)> {
    if !nvme_wait_source(port, handle_koid(channel)) {
        return None;
    }
    let (n, h) = channel_read_with_handle(channel, frame.as_mut_ptr(), frame.len());
    (n > 0 && n <= frame.len()).then_some((n, h))
}

/// Parent custody precedes ACK and hence every controller write. After ACK an
/// unproven owner keeps its process/channel/VMO handles in Sora; do not recycle
/// them merely because the child failed or timed out. — TIMBERDOODLE
fn run_nvme_admin_owner(initrd: Handle, root: Handle, startup: drv_nvme::Startup) -> Option<u64> {
    if NVME_UNPROVEN.load(Ordering::Relaxed) {
        notice(&[b"nvme: unproven owner; replacement refused\n"]);
        return None;
    }
    // The narrowed Resource now carries exactly one IRQ: INTx (INTID 292 on this board).
    let resource = syscall_handle(resource_create_child(
        root,
        startup.bar,
        drv_nvme::MMIO_BYTES,
        startup.irq,
        1,
    ))?;
    let recipe = SvcRecipe {
        initrd,
        path: kumo_abi::DRV_NVME_PATH,
    };
    let Some(owner) = spawn_from_recipe(&recipe) else {
        let _ = handle_close(resource);
        return None;
    };
    let Some(port) = syscall_handle(port_create()) else {
        let _ = process_kill(owner.process);
        for h in [resource, owner.client, owner.process] {
            let _ = handle_close(h);
        }
        return None;
    };
    let boot = startup.encode();
    if port_bind(port, owner.client) != 0
        || channel_write_with_handle(owner.client, boot.as_ptr(), boot.len(), resource) != 0
    {
        let _ = process_kill(owner.process);
        for h in [resource, owner.client, owner.process, port] {
            let _ = handle_close(h);
        }
        return None;
    }
    // Resource moved: never close its former handle number after receiving a new
    // handle, because that slot may already have been reused. — TIMBERDOODLE
    let mut frame = [0u8; drv_nvme::PROOF_LEN];
    let first = nvme_receive(port, owner.client, &mut frame);
    let memory = first.and_then(|(n, h)| {
        drv_nvme::Memory::decode(&frame[..n])
            .filter(|m| m.session == startup.session && h != 0)
            .map(|m| (m, Handle(h as u32)))
    });
    let Some((memory, custody)) = memory else {
        let _ = process_kill(owner.process); // No ACK: this child cannot have touched CC.
        if let Some((_, h)) = first {
            if h != 0 {
                let _ = handle_close(Handle(h as u32));
            }
        }
        for h in [owner.client, owner.process, port] {
            let _ = handle_close(h);
        }
        notice(&[b"nvme: startup/custody negotiation refused; no ACK sent\n"]);
        return None;
    };
    NVME_UNPROVEN.store(true, Ordering::Relaxed);
    let ack = [drv_nvme::ACK];
    if channel_write(owner.client, ack.as_ptr(), ack.len()) != 0 {
        notice(&[b"nvme: custody ACK failed; DMA and owner handles retained\n"]);
        return None;
    }
    let Some((n, h)) = nvme_receive(port, owner.client, &mut frame) else {
        notice(&[b"nvme: owner stopped answering; DMA and owner handles retained\n"]);
        return None;
    };
    // NV7: a serve owner answers with its volume and stays resident. Its bootstrap channel
    // becomes NVMe's storage control; custody, process and DMA handles stay with Sora for as
    // long as it serves (a later exit without proof leaves them retained). — TIMBERDOODLE
    if matches!(
        startup.mode,
        drv_nvme::MODE_SERVE | drv_nvme::MODE_SERVE_WRITABLE
    ) && h == 0
    {
        if let Some(ready) = drv_blk::MediaReady::decode(&frame[..n]) {
            let _ = port_unbind(port, owner.client);
            let _ = handle_close(port);
            STORAGE_CONTROLS[sora::NVME_STORAGE].store(owner.client.0, Ordering::Relaxed);
            let process = handle_koid(owner.process);
            notice(&[alloc::format!(
                "NV7 OWNER session={} process={} resident; serving {} {}; DMA custody held while it serves\n",
                startup.session,
                process,
                core::str::from_utf8(&sora::disk_path(sora::NVME_STORAGE)).unwrap_or("?"),
                if ready.read_only {"read-only"} else {"read-write"}
            )
            .as_bytes()]);
            let _ = media_ready(initrd, sora::NVME_STORAGE, owner.client, ready);
            NVME_CONTROL_KOID.store(handle_koid(owner.client), Ordering::Relaxed);
            let watch = NVME_WATCH_PORT.load(Ordering::Relaxed);
            if watch != 0 {
                let _ = port_bind(Handle(watch), owner.client);
            }
            set_nvme_owner(Some(NvmeOwner {
                process: owner.process,
                control: owner.client,
                custody,
                #[cfg(feature = "nvme-write-proof")]
                root_resource: root,
                startup,
                memory,
                stopping: false,
                shutdown_ready: false,
            }));
            NVME_UNPROVEN.store(false, Ordering::Relaxed);
            return Some(process);
        }
    }
    let proof = drv_nvme::Proof::decode(&frame[..n]);
    if h != 0 || !proof.is_some_and(|p| p.releases(memory) && p.session == startup.session) {
        notice(&[b"nvme: quiescence unproven; DMA and owner handles retained\n"]);
        return None;
    }
    let process = handle_koid(owner.process);
    let _ = port_unbind(port, owner.client);
    if port_bind(port, owner.process) != 0 || !nvme_wait_source(port, process) {
        notice(&[b"nvme: termination unproven; custody retained\n"]);
        return None;
    }
    let _ = port_unbind(port, owner.process);
    let mut closed = true;
    for handle in [custody, owner.client, owner.process, port] {
        closed &= handle_close(handle) == 0;
    }
    if !closed {
        notice(&[b"nvme: owner cleanup refused\n"]);
        return None;
    }
    notice(&[alloc::format!(
        "NV6 OWNER session={} process={} quiescent; DMA custody released after termination\n",
        startup.session,
        process
    )
    .as_bytes()]);
    NVME_UNPROVEN.store(false, Ordering::Relaxed);
    Some(process)
}

#[derive(Clone, Copy)]
struct NvmeOwner {
    process: Handle,
    control: Handle,
    custody: Handle,
    #[cfg(feature = "nvme-write-proof")]
    root_resource: Handle,
    startup: drv_nvme::Startup,
    memory: drv_nvme::Memory,
    stopping: bool,
    shutdown_ready: bool,
}
struct NvmeOwnerCell(core::cell::UnsafeCell<Option<NvmeOwner>>);
// Single-threaded Sora; copy ownership records out before any kernel re-entry.
// Keeping these handles named enables proved teardown instead of losing their
// numbers while retaining DMA custody. — TIMBERDOODLE
unsafe impl Sync for NvmeOwnerCell {}
static NVME_OWNER: NvmeOwnerCell = NvmeOwnerCell(core::cell::UnsafeCell::new(None));
fn nvme_owner() -> Option<NvmeOwner> {
    unsafe { *NVME_OWNER.0.get() }
}
fn set_nvme_owner(owner: Option<NvmeOwner>) {
    unsafe { *NVME_OWNER.0.get() = owner };
}

fn launch_nvme_service(initrd: Handle, root: Handle, dtb: Handle, bootinfo: Handle) {
    let Some(mut startup) = nvme_startup(root, dtb, bootinfo) else {
        return;
    };
    startup.session = 1;
    startup.mode = drv_nvme::MODE_SERVE_WRITABLE;
    let _ = run_nvme_admin_owner(initrd, root, startup);
}

#[cfg(feature = "nvme-write-proof")]
fn nvme_stop_owner() -> bool {
    let Some(mut owner) = nvme_owner() else {
        return false;
    };
    if owner.stopping {
        notice(&[b"NV8 owner already stopping/unproven; reset required\n"]);
        return false;
    }
    owner.stopping = true;
    NVME_UNPROVEN.store(true, Ordering::Relaxed);
    set_nvme_owner(Some(owner));
    set_device(sora::NVME_STORAGE, None);
    NVME_CONTROL_KOID.store(0, Ordering::Relaxed);
    let watch = NVME_WATCH_PORT.load(Ordering::Relaxed);
    if watch != 0 {
        let _ = port_unbind(Handle(watch), owner.control);
    }
    let Some(port) = syscall_handle(port_create()) else {
        return false;
    };
    let stop = [drv_nvme::CONTROL_STOP];
    let mut frame = [0; drv_nvme::PROOF_LEN];
    if port_bind(port, owner.control) != 0
        || channel_write(owner.control, stop.as_ptr(), stop.len()) != 0
    {
        return false;
    }
    let proof = nvme_receive(port, owner.control, &mut frame).and_then(|(n, h)| {
        if h != 0 {
            return None;
        }
        drv_nvme::Proof::decode(&frame[..n])
    });
    if !proof.is_some_and(|p| p.releases(owner.memory) && p.session == owner.startup.session) {
        notice(&[b"NV8 stop unproven; owner/DMA retained; no replacement\n"]);
        return false;
    }
    let _ = port_unbind(port, owner.control);
    if port_bind(port, owner.process) != 0 || !nvme_wait_source(port, handle_koid(owner.process)) {
        return false;
    }
    let process = handle_koid(owner.process);
    let _ = port_unbind(port, owner.process);
    let mut closed = true;
    for h in [owner.control, owner.custody, owner.process, port] {
        closed &= handle_close(h) == 0;
    }
    if !closed {
        return false;
    }
    STORAGE_CONTROLS[sora::NVME_STORAGE].store(0, Ordering::Relaxed);
    set_nvme_owner(None);
    NVME_UNPROVEN.store(false, Ordering::Relaxed);
    notice(&[alloc::format!(
        "NV8 STOP owner={} quiescent; DMA custody released after termination\n",
        process
    )
    .as_bytes()]);
    true
}

/// Shutdown readiness is distinct from controller-disable proof: keep the driver,
/// process and both DMA owners live until reset. Never reclaim on SHST2 alone.
/// An unproven outcome blocks normal power; emergency SysRq is separate. — TIMBERDOODLE
fn nvme_shutdown_owner() -> bool {
    if NVME_UNPROVEN.load(Ordering::Relaxed) {
        return false;
    }
    let Some(mut owner) = nvme_owner() else {
        return true;
    };
    if owner.stopping {
        return owner.shutdown_ready;
    }
    if !owner.memory.valid()
        || owner.memory.session != owner.startup.session
        || handle_koid(owner.custody) == u64::MAX
        || handle_koid(owner.process) == u64::MAX
    {
        return false;
    }
    owner.stopping = true;
    set_nvme_owner(Some(owner));
    NVME_UNPROVEN.store(true, Ordering::Relaxed);
    set_device(sora::NVME_STORAGE, None);
    if media().is_some_and(|m| m.device == sora::NVME_STORAGE) {
        set_media(None);
    }
    NVME_CONTROL_KOID.store(0, Ordering::Relaxed);
    let watch = NVME_WATCH_PORT.load(Ordering::Relaxed);
    if watch != 0 {
        let _ = port_unbind(Handle(watch), owner.control);
    }
    let Some(port) = syscall_handle(port_create()) else {
        return false;
    };
    let request = [drv_nvme::CONTROL_SHUTDOWN];
    let mut frame = [0; drv_nvme::shutdown::LEN];
    if port_bind(port, owner.control) != 0
        || channel_write(owner.control, request.as_ptr(), request.len()) != 0
    {
        return false;
    }
    let ready = nvme_receive(port, owner.control, &mut frame).and_then(|(n, h)| {
        if h != 0 {
            return None;
        }
        drv_nvme::shutdown::Ready::decode(&frame[..n])
    });
    if !ready.is_some_and(|r| r.valid_for(owner.startup.session)) {
        notice(&[b"nvme: shutdown unproven; normal power blocked, owner/DMA retained\n"]);
        return false;
    }
    let _ = port_unbind(port, owner.control);
    let _ = handle_close(port);
    owner.shutdown_ready = true;
    set_nvme_owner(Some(owner));
    NVME_UNPROVEN.store(false, Ordering::Relaxed);
    notice(&[alloc::format!(
        "NV8 SHUTDOWN owner={} ready; process and DMA custody retained through reset\n",
        handle_koid(owner.process)
    )
    .as_bytes()]);
    true
}

#[cfg(feature = "nvme-write-proof")]
fn nvme_write_proof_command(initrd: Handle) -> bool {
    if media().is_some_and(|m| m.device == sora::NVME_STORAGE) {
        notice(&[b"nvme-write-proof: unmount NVMe first\n"]);
        return false;
    }
    let Some(owner) = nvme_owner() else {
        return false;
    };
    let Some(session) = owner.startup.session.checked_add(1) else {
        return false;
    };
    if !nvme_stop_owner() {
        return false;
    }
    let mut startup = owner.startup;
    startup.session = session;
    startup.mode = drv_nvme::MODE_WRITE_PROOF;
    if run_nvme_admin_owner(initrd, owner.root_resource, startup).is_none() {
        return false;
    }
    let Some(session) = startup.session.checked_add(1) else {
        return false;
    };
    startup.session = session;
    startup.mode = drv_nvme::MODE_SERVE;
    if run_nvme_admin_owner(initrd, owner.root_resource, startup).is_none() {
        return false;
    }
    let ok = nvme_alias_proof(false);
    notice(&[if ok {
        b"NV8 proof command complete; read-only service restored\n"
    } else {
        b"NV8 proof command FAILED\n"
    }]);
    ok
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
    controller_index: usize,
) {
    if !configure_usb_custody(controller_index, config.mmio_base) {
        log(b"drv-xhci: duplicate or invalid custody owner\n");
        return;
    }
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

/// Gate for G8d-a's contained-close integration smoke: a fake GPU server and a fake client on
/// QEMU. Default **off** — it launches two short-lived processes before the resident set. The
/// same path is reachable on demand with the `gpu-contain` shell builtin, which is how
/// `scripts/graphics-smoke.py --gpu-session-kill` drives it. It never touches the GPU, so it
/// proves the contained-close ordering, not the real reset path on metal. — TIMBERDOODLE
const RUN_GPU_CONTAINMENT_SMOKE: bool = false;
/// Wall-clock budget for the fake client to arm the session, and then for the fake server to
/// report after the kill. — TIMBERDOODLE
const GPU_CONTAIN_WAIT_NS: u64 = 5_000_000_000;

//j711
/// The GPU client's lane, read once from the initrd's `etc/gpu-lane` marker before any
/// view is handed out (PLAN/015 §8.32 D1). It replaces G8's hand-flipped compile-time churn
/// selector. With no marker the lane is `Default`: drv-gpu keeps its G5b boot paint and
/// the J0c diagnostic runs unchanged. `view` and `churn` hand view `GPU_VIEW`'s channel
/// and surface to the one cooperative client over the lane bootstrap pair; only `churn`
/// regrants a session after the client exits.
static GPU_LANE: AtomicU8 = AtomicU8::new(GPU_LANE_DEFAULT);
const GPU_LANE_DEFAULT: u8 = 0;
const GPU_LANE_VIEW: u8 = 1;
const GPU_LANE_CHURN: u8 = 2;

fn gpu_lane() -> sora::GpuLane {
    match GPU_LANE.load(Ordering::Relaxed) {
        GPU_LANE_VIEW => sora::GpuLane::View,
        GPU_LANE_CHURN => sora::GpuLane::Churn,
        _ => sora::GpuLane::Default,
    }
}

/// Read `etc/gpu-lane` once at boot. An absent marker is the default image and logs
/// nothing; an unreadable or unrecognized one is refused aloud and keeps the default lane,
/// so a proof image whose marker went wrong fails named instead of running the wrong lane.
fn read_gpu_lane(initrd: Handle) {
    let Some((base, len)) = initrd_find(initrd, GPU_LANE_PATH.as_bytes()) else {
        return;
    };
    let mut file = [0u8; 16];
    let n = (len as usize).min(file.len());
    let lane = if vmo_read(initrd, base, file.as_mut_ptr(), n) == 0 {
        sora::gpu_lane(&file[..n])
    } else {
        None
    };
    let Some(lane) = lane else {
        notice(&[b"sora: gpu-lane marker refused; default lane kept\n"]);
        return;
    };
    let raw = match lane {
        sora::GpuLane::Default => GPU_LANE_DEFAULT,
        sora::GpuLane::View => GPU_LANE_VIEW,
        sora::GpuLane::Churn => GPU_LANE_CHURN,
    };
    GPU_LANE.store(raw, Ordering::Relaxed);
    notice(&[
        b"sora: gpu-lane marker selects the ",
        lane.name(),
        b" lane\n",
    ]);
}

//j721
/// The clock-watch lane is independent of the churn lane so the same sampler can run as an
/// idle control: with the watch on and churn off, its samples measure serve-loop timer
/// lateness with no sustained GPU load. Committed off; acceptance builds flip either or
/// both. — TIMBERDOODLE
const GPU_CLOCK_WATCH: bool = false;

//j720
/// The GPU clock watch: a read-only CRU mapping minted from the root Resource and sampled
/// on the serve port every ten seconds while the churn lane is on. Its purpose is to
/// explain the elapsed-time JOB slowdown with evidence: if a CRU word moves, the clock or a
/// gate moved with it; if every word holds, the drift is elsewhere. Offsets are
/// `CRU_CLKSEL_CON158/161`, `GATE_CON66/67` and `GPLL_CON0` from
/// hal/kumo-hal-aarch64/src/mali.rs and power_unit.rs; the span is drv-fb's read-only
/// clock-probe window. — TIMBERDOODLE
const CLOCK_CRU_BASE: u64 = drv_fb::rk3588_clock::CRU_BASE;
const CLOCK_CRU_LEN: u64 = drv_fb::rk3588_clock::CRU_LEN;
const GPU_CLOCK_WATCH_PERIOD_NS: u64 = 10_000_000_000;
const GPU_CLOCK_CRU_VA: u64 = 0x3502_0000;
const GPU_CLOCK_O_CLKSEL158: u64 = 0x578;
const GPU_CLOCK_O_CLKSEL161: u64 = 0x584;
const GPU_CLOCK_O_GATE66: u64 = 0x908;
const GPU_CLOCK_O_GATE67: u64 = 0x90c;
const GPU_CLOCK_O_GPLL0: u64 = 0x1c0;
/// DESIGN/020: GPLL is 1188 MHz, so GPLL/6 is the ratified 198 MHz boot rate.
const GPU_CLOCK_GPLL_MHZ: u32 = 1188;
static GPU_CLOCK_WATCH_ACTIVE: AtomicBool = AtomicBool::new(false);
//j721
/// Monotonic time the next sample is due, recorded when the timer is armed so the sample
/// can report its own lateness in-guest. — TIMBERDOODLE
static GPU_CLOCK_WATCH_DUE_NS: AtomicU64 = AtomicU64::new(0);
//j723
/// Monotonic time the serve loop's most recent port wake returned, so a sample can split
/// its lateness into wake-side (timer delivery and scheduling) and log-side (sampling
/// cost). — TIMBERDOODLE
static GPU_CLOCK_WAKE_NS: AtomicU64 = AtomicU64::new(0);
//j728
static GPU_CLOCK_POPS: AtomicU64 = AtomicU64::new(0);
static GPU_CLOCK_POPS_AT_ARM: AtomicU64 = AtomicU64::new(0);

/// Keep full live vectors across the VMO factory syscall while its private-table guard
/// runs. Use a successful anonymous creation; the smoke allocates no backing pages. — KESTREL
#[cfg(target_arch = "aarch64")]
#[inline(never)]
fn verify_vmo_factory_vectors() {
    let mut raw = 4096u64;
    let (a, b, c, d): (u64, u64, u64, u64);
    unsafe {
        core::arch::asm!(
            "movi v0.16b, #0x5a", "movi v31.16b, #0xa5", "svc #0",
            "umov x9, v0.d[0]", "umov x10, v0.d[1]",
            "umov x11, v31.d[0]", "umov x12, v31.d[1]",
            inlateout("x0") raw, in("x1") 0u64, in("x2") 0u64, in("x3") 0u64,
            in("x8") kumo_abi::Syscall::VmoCreate as u64,
            lateout("x9") a, lateout("x10") b, lateout("x11") c, lateout("x12") d,
            clobber_abi("C"), options(nostack),
        );
    }
    let handle = syscall_handle(raw).expect("VMO vector smoke creation");
    let _ = handle_close(handle);
    assert!(
        a == 0x5a5a5a5a5a5a5a5a && b == a && c == 0xa5a5a5a5a5a5a5a5 && d == c,
        "VMO SVC vector preservation"
    );
    notice(&[b"sora: VmoCreate SIMD preserved\n"]);
}

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
    // F2b focus: the framebuffer extent for click hit-testing, and the view Sora last named
    // as the keyboard owner. Focus changes are announced to drv-fb on the retained lane.
    let mut fb_extent: (u32, u32) = (0, 0);
    let mut focus_view: Option<usize> = None;
    let mut display_broker: Option<DisplayBroker> = None;

    // J159: BootInfo VMO handle arrives in x8 (beyond the C-ABI first-8). `_start` captured it
    // into `BOOTINFO_VMO_HANDLE` at entry — reading x8 here would race the compiler's reuse of
    // it (see the static's doc-comment).
    let bootinfo_vmo: u64 = BOOTINFO_VMO_HANDLE.load(Ordering::Relaxed);
    let dtb_vmo: u64 = DTB_VMO_HANDLE.load(Ordering::Relaxed);

    // Greeting.
    debug_write(b"hello from Sora via SVC\n".as_ptr(), 24);
    kumo_rt::boot_ok(&[b"sora: supervisor online\n"]);
    #[cfg(target_arch = "aarch64")]
    verify_vmo_factory_vectors();

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
    // The GPU lane is fixed before drv-fb's views or the GPU client exist (PLAN/015 §8.32 D1).
    read_gpu_lane(initrd);

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

    // G8d-a: the contained-close integration smoke. Default off; it costs two short-lived
    // processes and never touches the GPU. Enable it at boot or run `gpu-contain` from the
    // shell/autoexec. — TIMBERDOODLE
    if RUN_GPU_CONTAINMENT_SMOKE {
        if run_gpu_session_kill_smoke(initrd) {
            log(b"gpu-contain: ok\n");
        } else {
            log(b"gpu-contain: fail\n");
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
    launch_nvme_service(
        initrd,
        res,
        Handle(dtb_vmo as u32),
        Handle(bootinfo_vmo as u32),
    );

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
                let display_iommu = verify_offline_display_context(res, bootinfo);
                let prepared = prepare_display_context(res, bootinfo, display_iommu);
                let mut display_control = Handle(0);
                if let (Some(iommu), Some(grants)) = (display_iommu, prepared) {
                    if let Some((broker, client)) = DisplayBroker::new(iommu, grants[1], bootinfo) {
                        display_broker = Some(broker);
                        display_control = client;
                    }
                }

                // Unconditional pre-gate dump of the geometry read from the BootInfo snapshot, so a
                // blank boot reveals whether the snapshot is stale/garbage (a coherency bug upstream)
                // or sane-but-rejected (the is_plausible gate too strict). Fires every boot — unlike
                // the failure-branch dump below, which never prints when the gate itself is the cause.
                log(b"drv-fb: snapshot fb geometry ");
                log_fb_geometry(&fb);
                if fb.is_plausible() {
                    pointer_state = sora::PointerState::new(fb.width, fb.height);
                    fb_extent = (fb.width, fb.height);
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
                        let mut transferred = prepared.is_some();
                        for (tag, handle) in [b"D", b"O", b"A", b"Z"]
                            .into_iter()
                            .zip(prepared.unwrap_or([Handle(0); 4]))
                        {
                            if channel_write_with_handle(srv, tag.as_ptr(), 1, handle) != 0 {
                                if handle.0 != 0 {
                                    let _ = handle_close(handle);
                                }
                                transferred = false;
                                let _ = channel_write_with_handle(srv, tag.as_ptr(), 1, Handle(0));
                                notice(&[b"display: D1d bootstrap transfer refused; absent record sent, WRITE withheld\n"]);
                            }
                        }
                        if channel_write_with_handle(srv, b"M".as_ptr(), 1, display_control) != 0 {
                            let _ = channel_write_with_handle(srv, b"M".as_ptr(), 1, Handle(0));
                            transferred = false;
                        }
                        // The first native-image lane links only the prepared display and
                        // the one cooperative GPU client. No Resource reaches that client. — KESTREL
                        let mut native = Handle(0);
                        let mut producer = Handle(0);
                        // The churn lane keeps the pair too (j713): its J0c diagnostic is what
                        // sets drv-fb's diagnostic-completed flag, so the vblank gate arms
                        // before the churn loop's readiness probe. — TIMBERDOODLE
                        if transferred
                            && display_broker.is_some()
                            && GPU_CLIENT_CHANNEL.load(Ordering::Relaxed) != 0
                        {
                            let (a, b) = channel_create_pair();
                            if let (Some(a), Some(b)) = (syscall_handle(a), syscall_handle(b)) {
                                native = a;
                                producer = b;
                            }
                        }
                        if channel_write_with_handle(srv, b"J".as_ptr(), 1, native) == 0 {
                            GPU_NATIVE_CHANNEL.store(producer.0, Ordering::Relaxed);
                        } else {
                            if native.0 != 0 {
                                let _ = handle_close(native);
                            }
                            if producer.0 != 0 {
                                let _ = handle_close(producer);
                            }
                            let _ = channel_write_with_handle(srv, b"J".as_ptr(), 1, Handle(0));
                        }
                        // A queued context keeps DMA backing alive but conveys no MAP
                        // authority to its old process, even with the root Resource. — KESTREL
                        let display_live = transferred
                            && display_broker.is_some()
                            && kumo_rt::device_register_write(
                                res,
                                drv_fb::rk3588::VOP_BASE,
                                kumo_abi::sys::DisplayRegister::Commit as u32,
                                drv_fb::rk3588_scanout::COMMIT_VP2,
                            ) as u32 as i32
                                == kumo_abi::Errno::BadState.status();
                        if display_live {
                            notice(&[b"display: D1d queued context retains DMA; old holder checked write denied; driver WRITE grant follows\n"]);
                        }
                        bootstrap_display_probe(
                            srv,
                            res,
                            bootinfo.board_id(),
                            fb.is_plausible(),
                            display_live,
                        );
                        bootstrap_display_topology(
                            srv,
                            Handle(dtb_vmo as u32),
                            bootinfo.platform.dtb,
                            bootinfo.board_id(),
                            fb.is_plausible(),
                        );
                        let graphics_enabled = kumo_graphics::band(fb.width, fb.height).is_some();
                        let graphics = bootstrap_graphics(srv, graphics_enabled);
                        DRV_FB_BOOTSTRAP.store(srv.0, Ordering::Relaxed);
                        notice(&[b"sora: drv-fb bootstrap retained for view regrant\n"]);
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
                                    //j711
                                    // The churn lane keeps the view with Sora so the GPU
                                    // client can receive it over the lane bootstrap after
                                    // launch, instead of drv-gpu taking it now.
                                    if view == drv_gpu::GPU_VIEW
                                        && !gpu_lane().keeps_view()
                                        && hand_view_to_gpu(view, *channel, *vmo)
                                    {
                                        continue;
                                    }
                                    GRAPHICS_CHANNEL[view].store(channel.0, Ordering::Relaxed);
                                    GRAPHICS_VMO[view].store(vmo.0, Ordering::Relaxed);
                                    GRAPHICS_GRANTABLE[view].store(true, Ordering::Relaxed);
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
                            let _ = handle_close(srv);
                            DRV_FB_BOOTSTRAP.store(0, Ordering::Relaxed);
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
    //j720
    // Mint one read-only CRU window from the root Resource, exactly as the display probe's
    // PMU lane does, and let the serve-loop sampler read the GPU clock words from it.
    // — TIMBERDOODLE
    if GPU_CLOCK_WATCH {
        let raw = kumo_rt::resource_mint_mmio(res, CLOCK_CRU_BASE, CLOCK_CRU_LEN);
        if let Some(vmo) = syscall_handle(raw) {
            if vmar_map(
                Handle(0),
                vmo,
                0,
                GPU_CLOCK_CRU_VA,
                CLOCK_CRU_LEN,
                VmarFlags::READ.0,
            ) == 0
            {
                GPU_CLOCK_WATCH_ACTIVE.store(true, Ordering::Relaxed);
            }
            let _ = handle_close(vmo);
        }
    }
    release_gpu_bootstrap();
    let mut gpu_client_process = launch_gpu_client(initrd);
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
        let display_koid = display_broker.as_ref().and_then(|b| {
            let k = handle_koid(b.channel);
            (k != u64::MAX && port_bind(port, b.channel) == 0).then_some(Handle(k as u32))
        });
        SERVE_PORT.store(u64::from(port.0), Ordering::Relaxed);
        netstack_attach_serve(port);
        log(b"sora: ports bound\n");
        // J545: each controller's storage control channel. `PortBind` replays a pending offer,
        // so a stick probed before this point still wakes the loop below.
        let mut storage_koids: [Option<Handle>; sora::STORAGE_DEVICES] =
            [None; sora::STORAGE_DEVICES];
        for (index, (koid, control)) in storage_koids.iter_mut().zip(&STORAGE_CONTROLS).enumerate()
        {
            let control = Handle(control.load(Ordering::Relaxed));
            let raw_koid = handle_koid(control);
            if control.0 != 0 && raw_koid != u64::MAX && port_bind(port, control) == 0 {
                *koid = Some(Handle(raw_koid as u32));
                if index < sora::NVME_STORAGE {
                    USB_CONTROL_KOIDS[index].store(raw_koid, Ordering::Relaxed);
                }
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
        let mut log_mirror_timer: Option<(Handle, Handle)> = None;

        // G8d-b: gpu-client supervision. The retained child is bound exactly like a
        // supervised server, and on the churn image each termination schedules a session
        // regrant inside the DESIGN/002 give-up budget.
        const GPU_MAX_RESTARTS: u32 = 3;
        const GPU_INITIAL_BACKOFF_NS: u64 = 50_000_000;
        const GPU_MAX_BACKOFF_NS: u64 = 200_000_000;
        let mut gpu_restart_budget = sora::RestartBudget::new(GPU_MAX_RESTARTS);
        let mut gpu_restart_backoff =
            sora::RestartBackoff::new(GPU_INITIAL_BACKOFF_NS, GPU_MAX_BACKOFF_NS);
        let mut gpu_restart_timer: Option<(Handle, Handle)> = None;
        //j720
        let mut clock_watch_timer: Option<(Handle, Handle)> = None;
        let mut gpu_client_process = gpu_client_process.take();
        let mut gpu_client_koid = None;
        if let Some(process) = gpu_client_process {
            let koid = handle_koid(process);
            if koid != u64::MAX && port_bind(port, process) == 0 {
                gpu_client_koid = Some(Handle(koid as u32));
                log(b"gpu-client: supervision armed\n");
            } else {
                let _ = handle_close(process);
                gpu_client_process = None;
                log(b"gpu-client: supervision bind failed\n");
            }
        }

        actor_arm(port, initrd, root);
        ax2_arm(port, initrd, root);
        ax4_arm(
            port,
            initrd,
            root,
            &AX4_PENDING,
            &AX4_INITRD,
            &AX4_ROOT,
            &AX4_TIMER,
        );
        ax4_gate_arm(port, initrd, root);
        log(b"sora: entering serve loop\n");
        //j720
        clock_watch_timer = arm_clock_watch(port);
        NVME_WATCH_PORT.store(port.0, Ordering::Relaxed);
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
            // A typed `gfx-log on` lands mid-loop; arm the mirror cadence on the next pass.
            if LOG_MIRROR.load(Ordering::Relaxed) && log_mirror_timer.is_none() {
                log_mirror_timer = arm_log_mirror(port, sora::LOG_MIRROR_IDLE_NS);
                if log_mirror_timer.is_none() {
                    log(b"gfx-log: mirror timer unavailable\n");
                    LOG_MIRROR.store(false, Ordering::Relaxed);
                }
            }
            let raw_source = port_wait(port);
            if actor_event(raw_source)
                || ax2_event(raw_source)
                || ax4_event(
                    raw_source,
                    gpu_client_process.is_some() || gpu_restart_timer.is_some(),
                )
                || ax4_gate_event(
                    raw_source,
                    gpu_client_process.is_some() || gpu_restart_timer.is_some(),
                )
                || netstack_event(raw_source)
            {
                continue;
            }
            let source = Handle(raw_source as u32);
            //j723
            // Capture the wake instant before any dispatch, so the clock sampler can tell a
            // late wake from slow logging. — TIMBERDOODLE
            GPU_CLOCK_WAKE_NS.store(kumo_rt::clock_get(), Ordering::Relaxed);
            //j728
            GPU_CLOCK_POPS.fetch_add(1, Ordering::Relaxed);
            if display_koid == Some(source) {
                if let Some(b) = display_broker.as_ref() {
                    b.serve(initrd);
                }
                continue;
            }
            // An explicit NVMe owner replacement changes control identity. Old queued
            // packets must never be read from the new endpoint. USB watches stay fixed.
            // — TIMBERDOODLE
            let current = NVME_CONTROL_KOID.load(Ordering::Relaxed);
            let storage_index = if current != 0 && current == u64::from(source.0) {
                Some(sora::NVME_STORAGE)
            } else {
                storage_koids
                    .iter()
                    .enumerate()
                    .find(|(i, koid)| *i != sora::NVME_STORAGE && **koid == Some(source))
                    .map(|(i, _)| i)
            };
            if let Some(index) = storage_index {
                if media_offered(initrd, port, index, &mut storage_koids) {
                    settle_first_prompt(&mut first_prompt, port, &cwd, b"media");
                }
                continue;
            }
            if first_prompt.map(|(_, koid)| koid) == Some(source) {
                settle_first_prompt(&mut first_prompt, port, &cwd, b"timeout");
                continue;
            }
            if log_mirror_timer.map(|(_, koid)| koid) == Some(source) {
                let Some((timer, _)) = log_mirror_timer.take() else {
                    continue;
                };
                let _ = port_unbind(port, timer);
                let _ = handle_close(timer);
                if LOG_MIRROR.load(Ordering::Relaxed) {
                    // A retired present names the frame this pass reached; the next pass is
                    // paced by the gate's frame wait, not by a fixed timer cadence.
                    // — TIMBERDOODLE
                    let outcome = refresh_all(true);
                    if outcome.alive {
                        log_mirror_timer =
                            arm_log_mirror(port, sora::log_mirror_delay(outcome.retired));
                    } else {
                        LOG_MIRROR.store(false, Ordering::Relaxed);
                        log(b"gfx-log: mirror disabled; view unavailable\n");
                    }
                }
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
            //j714 gpu-client supervision. A termination is the session's end; on the churn
            // image one regrant attempt is armed per give-up budget unit, paced by the same
            // capped backoff the supervised servers use.
            if gpu_restart_timer.map(|(_, koid)| koid) == Some(source) {
                let Some((timer, _)) = gpu_restart_timer.take() else {
                    continue;
                };
                let _ = port_unbind(port, timer);
                let _ = handle_close(timer);
                if !gpu_restart_budget.try_consume() {
                    log(b"gpu-client: restart budget exhausted; giving up\n");
                    continue;
                }
                match regrant_gpu_session(initrd) {
                    Some(process) => {
                        let koid = handle_koid(process);
                        if koid != u64::MAX && port_bind(port, process) == 0 {
                            gpu_client_process = Some(process);
                            gpu_client_koid = Some(Handle(koid as u32));
                            log(b"gpu-client: session regranted\n");
                        } else {
                            let _ = handle_close(process);
                            log(b"gpu-client: regrant supervision bind failed\n");
                        }
                    }
                    None => {
                        if gpu_restart_budget.exhausted() {
                            log(b"gpu-client: restart budget exhausted; giving up\n");
                        } else {
                            let delay_ns = gpu_restart_backoff.next_delay_ns();
                            match arm_gpu_restart(port, delay_ns) {
                                Some(timer) => {
                                    gpu_restart_timer = Some(timer);
                                    log(b"gpu-client: session regrant delayed\n");
                                }
                                None => {
                                    log(b"gpu-client: restart timer unavailable; giving up\n");
                                }
                            }
                        }
                    }
                }
                continue;
            }
            //j720
            // One clock sample per period, chained on the serve port like the log mirror.
            // — TIMBERDOODLE
            if clock_watch_timer.map(|(_, koid)| koid) == Some(source) {
                let Some((timer, _)) = clock_watch_timer.take() else {
                    continue;
                };
                //j728
                let pops = GPU_CLOCK_POPS
                    .load(Ordering::Relaxed)
                    .wrapping_sub(GPU_CLOCK_POPS_AT_ARM.load(Ordering::Relaxed));
                let t0 = kumo_rt::clock_get();
                let _ = port_unbind(port, timer);
                let _ = handle_close(timer);
                let t1 = kumo_rt::clock_get();
                log_gpu_clock_sample(pops, t1.saturating_sub(t0));
                match arm_clock_watch(port) {
                    Some(timer) => clock_watch_timer = Some(timer),
                    None => log(b"clk-gpu: watch disarmed\n"),
                }
                continue;
            }
            if Some(source) == gpu_client_koid {
                gpu_client_koid = None;
                if let Some(process) = gpu_client_process.take() {
                    // Teardown reclaims only bindings keyed on the dead process's own
                    // handles: drop Sora's watch before closing it (the ttyd pattern).
                    let _ = port_unbind(port, process);
                    let _ = handle_close(process);
                }
                match gpu_lane() {
                    sora::GpuLane::Default => {
                        diag(b"gpu-client: client exited; restart gated off on this image\n");
                        continue;
                    }
                    // A `view` lane is one launch (PLAN/015 §8.32 D1): the client's exit
                    // returns the view's lease. Marking the view grantable lets the release
                    // query observe it, so a shell client can be granted it again; no session
                    // is regranted.
                    sora::GpuLane::View => {
                        GRAPHICS_GRANTABLE[drv_gpu::GPU_VIEW].store(true, Ordering::Relaxed);
                        log(b"gpu-client: client exited; view lane returns view 1 to Sora, no regrant\n");
                        continue;
                    }
                    sora::GpuLane::Churn => {}
                }
                // The view's lease died with the client; make it eligible for the release
                // query so the regrant can ask drv-fb for it again (churn lane only).
                GRAPHICS_GRANTABLE[drv_gpu::GPU_VIEW].store(true, Ordering::Relaxed);
                if GPU_SESSION_HANDOFF.load(Ordering::Relaxed) == 0 {
                    log(b"gpu-client: no session handoff; supervision stops\n");
                    continue;
                }
                log(b"gpu-client: client exited; session regrant scheduled\n");
                let delay_ns = gpu_restart_backoff.next_delay_ns();
                match arm_gpu_restart(port, delay_ns) {
                    Some(timer) => gpu_restart_timer = Some(timer),
                    None => log(b"gpu-client: restart timer unavailable; giving up\n"),
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
                            // F2b focus: a left-button press focuses the view under the
                            // pointer, so detect the edge before the new state is applied.
                            let was_left = pointer_state.buttons() & 1 != 0;
                            pointer_state.apply_boot_mouse(report);
                            let left_pressed = !was_left && pointer_state.buttons() & 1 != 0;
                            // F2b slice-1: forward the clamped position to drv-fb, which owns
                            // the cursor sprite over the composed scene. — TIMBERDOODLE
                            let control = Handle(DRV_FB_BOOTSTRAP.load(Ordering::Relaxed));
                            if control.0 != 0 {
                                let position = pointer_state.position();
                                let record = kumo_graphics::encode_pointer(
                                    position.x as u16,
                                    position.y as u16,
                                    pointer_state.buttons(),
                                );
                                let _ = channel_write(control, record.as_ptr(), record.len());
                            }
                            if left_pressed {
                                let position = pointer_state.position();
                                let target = kumo_graphics::band(fb_extent.0, fb_extent.1)
                                    .and_then(|band| {
                                        kumo_graphics::band_view_at(&band, position.x, position.y)
                                    });
                                if let Some(view) = target {
                                    if GRAPHICS_GRANTABLE[view].load(Ordering::Relaxed)
                                        && focus_view != Some(view)
                                    {
                                        focus_view = Some(view);
                                        announce_focus(focus_view);
                                    }
                                }
                            }
                            // One NOTICE record proves the physical pointer path on metal
                            // without needing dmesg's INFO level. — TIMBERDOODLE
                            if mouse_events_seen == 1 {
                                notice(&[b"sora: pointer ready\n"]);
                            }
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
                } else if n > 0
                    && focus_view
                        .is_some_and(|view| GRAPHICS_INPUT[view].load(Ordering::Relaxed) != 0)
                    && !(TERMINAL_MODE.load(Ordering::Relaxed) && focus_view == Some(TERMINAL_VIEW))
                {
                    // F2b focus routing: the focused view's input consumer owns the keyboard.
                    // A write that finds the consumer gone retires the channel and hands the
                    // keyboard back to the shell. Ctrl-] still cycles. — TIMBERDOODLE
                    let view = focus_view.expect("focused view");
                    let consumer = Handle(GRAPHICS_INPUT[view].load(Ordering::Relaxed));
                    let mut delivered = true;
                    for i in 0..n {
                        if serve_buf[i] == FOCUS_KEY {
                            let next = next_focus(focus_view);
                            if next != focus_view {
                                focus_view = next;
                                announce_focus(next);
                            }
                            continue;
                        }
                        if channel_write(consumer, serve_buf[i..i + 1].as_ptr(), 1) != 0 {
                            delivered = false;
                            break;
                        }
                    }
                    if !delivered {
                        GRAPHICS_INPUT[view].store(0, Ordering::Relaxed);
                        let _ = handle_close(consumer);
                        focus_view = None;
                        announce_focus(None);
                    }
                } else if n > 0 && interactive_ttyd.is_some() {
                    let tty = interactive_ttyd.unwrap().instance;
                    let mut tty_reply = [0u8; ttyd::REPLY_BUF_BYTES];
                    for i in 0..n {
                        if serve_buf[i] == FOCUS_KEY {
                            // A cycle that lands on the same view (for example when only one
                            // view is grantable) changes nothing and announces nothing.
                            // — TIMBERDOODLE
                            let next = next_focus(focus_view);
                            if next != focus_view {
                                focus_view = next;
                                announce_focus(next);
                            }
                            continue;
                        }
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
    if cmd.name == "actorbench" {
        if !cmd.args.is_empty() {
            usage(b"usage: actorbench\n");
        } else if SERVE_PORT.load(Ordering::Relaxed) == 0 {
            ACTOR_PENDING.store(true, Ordering::Relaxed);
        } else {
            actor_benchmark(initrd, root);
        }
        return true;
    }
    if cmd.name == "ax2bench" {
        if !cmd.args.is_empty() {
            usage(b"usage: ax2bench\n");
        } else if SERVE_PORT.load(Ordering::Relaxed) == 0 {
            AX2_PENDING.store(true, Ordering::Relaxed);
        } else {
            ax2_benchmark(initrd, root);
        }
        return true;
    }
    if cmd.name == "ax4bench" {
        let boundary_runs = if cmd.args.is_empty() {
            0
        } else if cmd.args.len() == 1 && cmd.args[0] == "fill" {
            1
        } else if cmd.args.len() == 1 && cmd.args[0] == "fill2" {
            2
        } else {
            usage(b"usage: ax4bench [fill|fill2]\n");
            return true;
        };
        AX4_BOUNDARY_RUNS.store(boundary_runs, Ordering::Relaxed);
        AX4_PENDING.store(true, Ordering::Relaxed);
        let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
        if port.0 != 0 {
            ax4_arm(
                port,
                initrd,
                root,
                &AX4_PENDING,
                &AX4_INITRD,
                &AX4_ROOT,
                &AX4_TIMER,
            );
        }
        return true;
    }
    if cmd.name == "ax4gate" {
        if !cmd.args.is_empty() {
            usage(b"usage: ax4gate\n");
        } else {
            AX4_GATE_PENDING.store(true, Ordering::Relaxed);
            let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
            if port.0 != 0 {
                ax4_gate_arm(port, initrd, root);
            }
        }
        return true;
    }
    #[cfg(feature = "netstack-restart-proof")]
    if cmd.name == "netstack-crash-proof" {
        if cmd.args.first().is_some_and(|a| a == "idle") {
            netstack_idle_crash_proof(initrd);
        } else {
            netstack_crash_proof(initrd, cmd.args.first().is_some_and(|a| a == "burst"));
        }
        return true;
    }
    if cmd.name == "gfx-demo" {
        graphics_demo(cmd);
        return true;
    }
    if matches!(cmd.name.as_str(), "gfx-client" | "bmp-view") {
        graphics_client(cmd, initrd, cwd);
        return true;
    }
    if cmd.name == "gfx-clients" {
        graphics_clients(cmd, initrd);
        return true;
    }
    if cmd.name == "gfx-keys" {
        gfx_keys(cmd, initrd);
        return true;
    }
    if cmd.name == "gfx-recover" {
        gfx_recover(cmd, initrd);
        return true;
    }
    if cmd.name == "gpu-contain" {
        if !cmd.args.is_empty() {
            usage(b"usage: gpu-contain\n");
        } else if run_gpu_session_kill_smoke(initrd) {
            log(b"gpu-contain: ok\n");
        } else {
            log(b"gpu-contain: fail\n");
            fail_status(1);
        }
        return true;
    }
    if cmd.name == "gfx-titles" {
        gfx_titles(cmd);
        return true;
    }
    if cmd.name == "gfx-log" {
        gfx_log(cmd);
        return true;
    }
    if cmd.name == "gfx-term" {
        gfx_term(cmd);
        return true;
    }
    if cmd.name == "gfx-damage" {
        graphics_damage(cmd);
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
    #[cfg(feature = "nvme-write-proof")]
    if cmd.name == "nvme-write-proof" {
        if !cmd.args.is_empty() {
            usage(b"usage: nvme-write-proof\n");
        } else if !nvme_write_proof_command(initrd) {
            fail_status(1);
        }
        return true;
    }
    if matches!(
        cmd.name.as_str(),
        "reboot" | "reset" | "shutdown" | "poweroff" | "halt"
    ) {
        if nvme_shutdown_owner() {
            if channel_write(root, raw.as_ptr(), raw.len()) != 0 {
                fail_status(1);
            }
        } else {
            log(b"power: NVMe shutdown failed; power command refused\n");
            fail_status(1);
        }
        return true;
    }
    #[cfg(feature = "nvme-write-proof")]
    if cmd.name == "nvme-alias-proof" {
        let writable = DEVICES.get()[sora::NVME_STORAGE].is_some_and(|d| !d.ready.read_only);
        if !cmd.args.is_empty() {
            usage(b"usage: nvme-alias-proof\n");
        } else if !nvme_alias_proof(writable) {
            fail_status(1);
        }
        return true;
    }
    if cmd.name == "mount" {
        mount_command(cmd, cwd);
        return true;
    }
    if cmd.name == "umount" {
        umount_command(cmd, cwd);
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
            false,
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
    } else if cmd.name == "bg" {
        // bg <run|net> <program> [args...]: start one program without waiting for it; `wait`
        // later drains its output. One job at a time (J816, NET2b-2). — TIMBERDOODLE
        match cmd.args.split_first() {
            Some((launcher, program)) if !program.is_empty() && launcher == "run" => {
                run_program_background(initrd, program, false)
            }
            Some((launcher, program)) if !program.is_empty() && launcher == "net" => {
                run_program_background(initrd, program, true)
            }
            _ => usage(b"usage: bg run|net <program> [args...]\n"),
        }
    } else if cmd.name == "wait" {
        if !cmd.args.is_empty() {
            usage(b"usage: wait\n");
        } else {
            wait_background();
        }
    } else if cmd.name == "net" {
        // net <program> [args...]: `run`, plus one netstack session channel. Network access is
        // granted by this launcher word and nothing else (DESIGN/026 §4). — TIMBERDOODLE
        if cmd.args.is_empty() {
            usage(b"usage: net <program> [args...]\n");
        } else {
            run_program_granted(initrd, &cmd.args, true);
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
        false,
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

fn nvme_unavailable() {
    NVME_UNPROVEN.store(true, Ordering::Relaxed);
    NVME_CONTROL_KOID.store(0, Ordering::Relaxed);
    set_device(sora::NVME_STORAGE, None);
    if media().is_some_and(|m| m.device == sora::NVME_STORAGE) {
        set_media(None);
    }
    if let Some(mut owner) = nvme_owner() {
        owner.stopping = true;
        set_nvme_owner(Some(owner));
    }
}

/// J545: a storage control channel woke the serve loop: its driver offered a volume, or the
/// driver is gone and its volume with it.
/// Returns whether a volume was mounted: a closed or empty control channel is a driver leaving.
fn media_offered(
    initrd: Handle,
    port: Handle,
    index: usize,
    koids: &mut [Option<Handle>; sora::STORAGE_DEVICES],
) -> bool {
    let control = Handle(STORAGE_CONTROLS[index].load(Ordering::Relaxed));
    let mut frame = [0u8; kumo_xhci::DMA_CUSTODY_LEN];
    let (n, raw) = channel_read_with_handle(control, frame.as_mut_ptr(), frame.len());
    if n == 0 || n > frame.len() {
        close_unheld_usb_handle(raw);
        if index < sora::NVME_STORAGE {
            USB_CONTROL_KOIDS[index].store(0, Ordering::Relaxed);
        }
        let _ = port_unbind(port, control);
        koids[index] = None;
        set_device(index, None);
        if index == sora::NVME_STORAGE {
            nvme_unavailable();
        }
        if media().is_some_and(|media| media.control == control) {
            set_media(None);
            log(b"media: driver gone; volume removed\n");
        }
        return false;
    }
    if frame.get(..4) == Some(&b"XDM1"[..]) {
        accept_usb_custody(index, &frame[..n], raw, control);
        return false;
    }
    if raw != 0 || n != drv_blk::MEDIA_READY_LEN {
        close_unheld_usb_handle(raw);
        log(b"media: malformed offer; no DMA publication ACK\n");
        return false;
    }
    let Some(ready) = drv_blk::MediaReady::decode(&frame[..n as usize]) else {
        if index == sora::NVME_STORAGE {
            let _ = port_unbind(port, control);
            nvme_unavailable();
        }
        log(b"media: bad offer\n");
        return false;
    };
    media_ready(initrd, index, control, ready)
}

/// Register the volume storage slot `index` offers and, for a transport that mounts by itself,
/// mount it when `/media` is free (J545/J766). Returns whether it was mounted. — TIMBERDOODLE
fn media_ready(initrd: Handle, index: usize, control: Handle, ready: drv_blk::MediaReady) -> bool {
    let transport = sora::StorageTransport::of(index);
    let name = sora::MediaName::new(ready.label, ready.serial);
    let device = Device { ready, control };
    set_device(index, Some(device));
    if !transport.auto_mounts() {
        notice(&[
            b"media: ",
            &sora::disk_path(index),
            b" ",
            transport.name(),
            b" fat32 ",
            name.as_bytes(),
            if ready.read_only {
                b" (read-only)" as &[u8]
            } else {
                b""
            },
            b" ready; `mount ",
            &sora::disk_path(index),
            b"` to use\n",
        ]);
        return false;
    }
    // J766: a volume mounts by itself only when `/media` is free or already holds it; a second
    // stick waits for `mount`.
    if media().is_some_and(|media| media.device != index) {
        notice(&[
            b"media: ",
            &sora::disk_path(index),
            b" ready; another volume is mounted, so `mount ",
            &sora::disk_path(index),
            b"` waits for `umount`\n",
        ]);
        return false;
    }
    diag(b"media: /media/");
    diag(name.as_bytes());
    diag(b" ");
    diag(transport.name());
    diag(b" fat32 blocks=");
    diag_hex(ready.block_count);
    diag(b" serial=");
    diag(sora::MediaName::new(None, ready.serial).as_bytes());
    diag(if ready.read_only {
        b" read-only\n" as &[u8]
    } else {
        b"\n"
    });
    mount_device(index, device, name);
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
        false,
    );
}

/// The identity service's client end (PLAN/012), or 0 when it did not start.
static IDENTITYD: AtomicU32 = AtomicU32::new(0);

/// J816: the one background job (`bg`): its stdout reader and process handle, or 0 for none.
static BG_STDOUT: AtomicU32 = AtomicU32::new(0);
static BG_PROCESS: AtomicU32 = AtomicU32::new(0);

/// Start `argv` as the background job. It reuses the interactive launch, which returns as soon
/// as the child parks or exits and hands back its ends without draining them; the job reads no
/// input, so its stdin writer is closed at once. — TIMBERDOODLE
fn run_program_background(initrd: Handle, argv: &[alloc::string::String], grant_net: bool) {
    if BG_STDOUT.load(Ordering::Relaxed) != 0 {
        log(b"bg: a background job is still running; `wait` for it first\n");
        fail_status(1);
        return;
    }
    let Some(name) = argv.first().map(|n| n.as_bytes()) else {
        return;
    };
    if name == b"lua-repl" || name == b"kilo" {
        usage(b"bg: interactive programs cannot run in the background\n");
        return;
    }
    let mut path = [0u8; 64];
    let Some(full) = program_path(name, &mut path) else {
        fail_status(1);
        return;
    };
    let mut slots: [&[u8]; MAX_ARGV] = [b""; MAX_ARGV];
    let count = string_argv_slots(argv, &mut slots);
    let Some(child) = run_with_startup_input(
        initrd,
        full,
        full,
        Some(&slots[..count]),
        false,
        FsGrant::None,
        false,
        false,
        StdinMode::Interactive,
        grant_net,
    ) else {
        fail_status(1);
        return;
    };
    let _ = handle_close(child.stdin);
    BG_STDOUT.store(child.stdout.0, Ordering::Relaxed);
    BG_PROCESS.store(child.process.map_or(0, |p| p.0), Ordering::Relaxed);
    log(b"bg: started ");
    log(name);
    log(b"\n");
}

/// `wait`: drain the background job's output to the console until it exits.
fn wait_background() {
    let stdout = BG_STDOUT.swap(0, Ordering::Relaxed);
    let process = BG_PROCESS.swap(0, Ordering::Relaxed);
    if stdout == 0 {
        log(b"wait: no background job\n");
        fail_status(1);
        return;
    }
    pump_to_console(Handle(stdout));
    let _ = handle_close(Handle(stdout));
    if process != 0 {
        let _ = handle_close(Handle(process));
    }
    log(b"wait: background job finished\n");
}

/// Netstack is a soft-state actor: Sora retains its process watch and grants fresh sessions,
/// never old sockets, after replacement. Three restart attempts in any ten seconds, including
/// failed spawns; exhaustion is sticky until reboot. — TIMBERDOODLE
#[derive(Clone, Copy)]
struct NetInstance {
    process: Handle,
    control: Handle,
    koid: u64,
}

#[derive(Clone, Copy)]
struct NetSupervisor {
    instance: Option<NetInstance>,
    timer: Option<(Handle, u64)>,
    initrd: Handle,
    epoch: u16,
    intensity: sora::RestartIntensity<3>,
    backoff: sora::RestartBackoff,
    gave_up: bool,
}

static NET_SUPERVISOR: Shared<NetSupervisor> = Shared(core::cell::UnsafeCell::new(NetSupervisor {
    instance: None,
    timer: None,
    initrd: Handle(0),
    epoch: 0,
    intensity: sora::RestartIntensity::new(10_000_000_000),
    backoff: sora::RestartBackoff::new(50_000_000, 200_000_000),
    gave_up: false,
}));

// Sora still has the pre-AX9 synchronous reply context as well as its serve loop. Both dispatch
// the same death/timer transition; the first consumes it and stale koids are ignored. This does
// not make arbitrary child output nonblocking (DEFERRED/015). — TIMBERDOODLE
fn netstack_ports() -> [Handle; 2] {
    [
        Handle(REPLY_PORT.load(Ordering::Relaxed)),
        Handle(SERVE_PORT.load(Ordering::Relaxed) as u32),
    ]
}

fn netstack_watch(object: Handle, bind: bool) -> bool {
    let ports = netstack_ports();
    let mut ok = ports[0].0 != 0;
    for (i, port) in ports.iter().copied().enumerate() {
        if port.0 != 0 && (i == 0 || port != ports[0]) {
            let status = if bind {
                port_bind(port, object)
            } else {
                port_unbind(port, object)
            };
            ok &= status == 0;
        }
    }
    ok
}

fn netstack_attach_serve(port: Handle) {
    #[cfg(feature = "netstack-restart-proof")]
    if let Some((timer, _)) = NET_PROOF_TIMER.get() {
        let _ = port_bind(port, timer);
    }
    let state = NET_SUPERVISOR.get();
    for object in [state.instance.map(|s| s.process), state.timer.map(|t| t.0)]
        .into_iter()
        .flatten()
    {
        if port_bind(port, object) != 0 {
            log(b"netstack: serve watch unavailable; reply watch retained\n");
        }
    }
}

fn netstack_give_up() {
    let mut state = NET_SUPERVISOR.get();
    state.gave_up = true;
    NET_SUPERVISOR.set(state);
    log(b"netstack: giving up (restart intensity or resources exhausted)\n");
}

fn spawn_netstack(initrd: Handle, epoch: u16) -> Option<NetInstance> {
    let (control_raw, server_raw) = channel_create_pair();
    if control_raw == u64::MAX || server_raw == u64::MAX {
        return None;
    }
    let control = Handle(control_raw as u32);
    // ProcessRun's historical ABI auto-copies integers that name live handles. Bootstrap
    // the epoch over the control channel instead of risking an accidental grant. — TIMBERDOODLE
    let epoch_bytes = epoch.to_le_bytes();
    let bootstrap = [b'I', epoch_bytes[0], epoch_bytes[1]];
    if channel_write(control, bootstrap.as_ptr(), bootstrap.len()) != 0 {
        let _ = handle_close(control);
        let _ = handle_close(Handle(server_raw as u32));
        return None;
    }
    let mut kept = None;
    let flags = (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG2).bits();
    if !run_elf_threads(
        initrd,
        NETSTACK_PATH.as_bytes(),
        0,
        server_raw,
        flags,
        b"netstack",
        0,
        Some(&mut kept),
    ) {
        if let Some(process) = kept {
            let _ = process_kill(process);
            let _ = handle_close(process);
        }
        let _ = handle_close(control);
        let _ = handle_close(Handle(server_raw as u32));
        return None;
    }
    let Some(process) = kept else {
        let _ = handle_close(control);
        return None;
    };
    let parked = child_parked(process_wait());
    let koid = handle_koid(process);
    if !parked || koid == u64::MAX || !netstack_watch(process, true) {
        let _ = process_kill(process);
        netstack_watch(process, false);
        let _ = handle_close(process);
        let _ = handle_close(control);
        return None;
    }
    Some(NetInstance {
        process,
        control,
        koid,
    })
}

fn netstack_start() -> bool {
    let mut state = NET_SUPERVISOR.get();
    let Some(epoch) = state.epoch.checked_add(1) else {
        netstack_give_up();
        return false;
    };
    // Reserve the epoch even if loading fails: a partly started instance may have observed it.
    state.epoch = epoch;
    NET_SUPERVISOR.set(state);
    let Some(instance) = spawn_netstack(state.initrd, epoch) else {
        return false;
    };
    state = NET_SUPERVISOR.get();
    state.instance = Some(instance);
    NET_SUPERVISOR.set(state);
    log(b"netstack: supervised instance ");
    log_dec(u64::from(epoch));
    log(b" ready\n");
    true
}

fn netstack_schedule_restart() {
    let mut state = NET_SUPERVISOR.get();
    if state.gave_up || !state.intensity.try_consume(kumo_rt::clock_get()) {
        NET_SUPERVISOR.set(state);
        netstack_give_up();
        return;
    }
    let delay = state.backoff.next_delay_ns();
    NET_SUPERVISOR.set(state);
    let Some(timer) = syscall_handle(timer_create(delay)) else {
        netstack_give_up();
        return;
    };
    let koid = handle_koid(timer);
    if koid == u64::MAX || !netstack_watch(timer, true) {
        netstack_watch(timer, false);
        let _ = handle_close(timer);
        netstack_give_up();
        return;
    }
    state = NET_SUPERVISOR.get();
    state.timer = Some((timer, koid));
    NET_SUPERVISOR.set(state);
    log(b"netstack: restart delayed\n");
}

/// Called from every existing Sora wait context; no reference is held across a syscall.
/// Nested client waits must acknowledge USB custody too: a login/stdout wait may
/// otherwise depend on the very keyboard whose driver is waiting for ACK.
/// Per-wake one message; no draining ChannelRead on an empty peer. — TIMBERDOODLE
fn dispatch_service_event(source: u64) -> bool {
    if source != 0 {
        if let Some(index) = USB_CONTROL_KOIDS
            .iter()
            .position(|k| k.load(Ordering::Relaxed) == source)
        {
            let initrd = Handle(USB_CUSTODY_INITRD.load(Ordering::Relaxed));
            let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
            let mut watches = [None; sora::STORAGE_DEVICES];
            let _ = media_offered(initrd, port, index, &mut watches);
            return true;
        }
    }
    netstack_event(source)
}

fn netstack_event(source: u64) -> bool {
    #[cfg(feature = "netstack-restart-proof")]
    if let Some((timer, _)) = NET_PROOF_TIMER.get().filter(|t| t.1 == source) {
        NET_PROOF_TIMER.set(None);
        netstack_watch(timer, false);
        let _ = handle_close(timer);
        if let Some(instance) = NET_SUPERVISOR.get().instance {
            if process_kill(instance.process) == 0 {
                log(b"netstack-proof: idle kill sent\n");
            }
        }
        return true;
    }
    let mut state = NET_SUPERVISOR.get();
    if let Some(dead) = state.instance.filter(|s| s.koid == source) {
        state.instance = None;
        NET_SUPERVISOR.set(state);
        netstack_watch(dead.process, false);
        let _ = handle_close(dead.process);
        let _ = handle_close(dead.control);
        log(b"netstack: terminated; old sessions invalidated\n");
        netstack_schedule_restart();
        return true;
    }
    if let Some((timer, _)) = state.timer.filter(|t| t.1 == source) {
        state.timer = None;
        NET_SUPERVISOR.set(state);
        netstack_watch(timer, false);
        let _ = handle_close(timer);
        if !netstack_start() {
            log(b"netstack: replacement spawn failed\n");
            netstack_schedule_restart();
        }
        return true;
    }
    false
}

fn netstack_ready(initrd: Handle) -> Option<NetInstance> {
    let port = reply_port()?;
    let mut state = NET_SUPERVISOR.get();
    if state.initrd.0 == 0 {
        state.initrd = initrd;
        NET_SUPERVISOR.set(state);
        if !netstack_start() {
            netstack_schedule_restart();
        }
    }
    loop {
        state = NET_SUPERVISOR.get();
        if state.gave_up {
            log(b"netstack: unavailable after give-up; session not granted\n");
            return None;
        }
        if let Some(instance) = state.instance {
            return Some(instance);
        }
        if state.timer.is_none() {
            return None;
        }
        let source = port_wait(port);
        if source == u64::MAX {
            return None;
        }
        dispatch_service_event(source);
    }
}

/// Mint a new capability only on explicit launch. Failure never resets restart intensity;
/// Sora's TERMINATED watch, rather than a failed send, drives replacement. — TIMBERDOODLE
fn netstack_session(initrd: Handle) -> Option<Handle> {
    let control = netstack_ready(initrd)?.control;
    let (client_raw, server_raw) = channel_create_pair();
    if client_raw == u64::MAX || server_raw == u64::MAX {
        return None;
    }
    let server = Handle(server_raw as u32);
    if channel_write_with_handle(control, b"S".as_ptr(), 1, server) != Errno::Ok.status() {
        let _ = handle_close(server);
        let _ = handle_close(Handle(client_raw as u32));
        log(b"netstack: control channel refused; session not granted\n");
        return None;
    }
    Some(Handle(client_raw as u32))
}

#[cfg(feature = "netstack-restart-proof")]
static NET_PROOF_TIMER: Shared<Option<(Handle, u64)>> = Shared(core::cell::UnsafeCell::new(None));

#[cfg(feature = "netstack-restart-proof")]
fn netstack_idle_crash_proof(initrd: Handle) {
    if NET_PROOF_TIMER.get().is_some() || netstack_ready(initrd).is_none() {
        return;
    }
    let Some(timer) = syscall_handle(timer_create(5_000_000_000)) else {
        return;
    };
    let koid = handle_koid(timer);
    if koid == u64::MAX || !netstack_watch(timer, true) {
        netstack_watch(timer, false);
        let _ = handle_close(timer);
        return;
    }
    NET_PROOF_TIMER.set(Some((timer, koid)));
    log(b"netstack-proof: idle kill armed\n");
}

/// Private fault injection: Sora kills only the process it supervises. No application gains
/// kill authority, and this command is absent from default images. — TIMBERDOODLE
#[cfg(feature = "netstack-restart-proof")]
fn netstack_crash_proof(initrd: Handle, burst: bool) {
    let count = if burst { 4 } else { 1 };
    for _ in 0..count {
        let Some(instance) = netstack_ready(initrd) else {
            break;
        };
        if process_kill(instance.process) != 0 {
            log(b"netstack-proof: FAIL kill\n");
            return;
        }
        let Some(port) = reply_port() else {
            return;
        };
        let Some(deadline) = syscall_handle(timer_create(1_000_000_000)) else {
            return;
        };
        let deadline_koid = handle_koid(deadline);
        if port_bind(port, deadline) != 0 {
            let _ = handle_close(deadline);
            return;
        }
        while NET_SUPERVISOR
            .get()
            .instance
            .is_some_and(|s| s.koid == instance.koid)
        {
            let source = port_wait(port);
            if source == deadline_koid || source == u64::MAX {
                break;
            }
            dispatch_service_event(source);
        }
        let _ = port_unbind(port, deadline);
        let _ = handle_close(deadline);
        if NET_SUPERVISOR
            .get()
            .instance
            .is_some_and(|s| s.koid == instance.koid)
        {
            log(b"netstack-proof: FAIL termination watch\n");
            return;
        }
    }
    log(b"netstack-proof: kill observed\n");
}

// AX0 is opt-in. An autoexec request runs only after the ordinary login services settle.
static ACTOR_PENDING: AtomicBool = AtomicBool::new(false);
static ACTOR_INITRD: AtomicU32 = AtomicU32::new(0);
static ACTOR_ROOT: AtomicU32 = AtomicU32::new(0);
static ACTOR_TIMER: Shared<Option<(Handle, u64)>> = Shared(core::cell::UnsafeCell::new(None));

fn actor_arm(port: Handle, initrd: Handle, root: Handle) {
    if !ACTOR_PENDING.swap(false, Ordering::Relaxed) {
        return;
    }
    ACTOR_INITRD.store(initrd.0, Ordering::Relaxed);
    ACTOR_ROOT.store(root.0, Ordering::Relaxed);
    if let Some(timer) = syscall_handle(timer_create(5_000_000_000)) {
        if port_bind(port, timer) == 0 {
            ACTOR_TIMER.set(Some((timer, handle_koid(timer))));
        } else {
            let _ = handle_close(timer);
        }
    }
}
fn actor_event(source: u64) -> bool {
    if let Some((timer, _)) = ACTOR_TIMER.get().filter(|t| t.1 == source) {
        ACTOR_TIMER.set(None);
        let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
        let _ = port_unbind(port, timer);
        let _ = handle_close(timer);
        actor_benchmark(
            Handle(ACTOR_INITRD.load(Ordering::Relaxed)),
            Handle(ACTOR_ROOT.load(Ordering::Relaxed)),
        );
        return true;
    }
    false
}

/// Bounded diagnostic read on the already shared reply Port. A dead or silent child cannot
/// strand the root supervisor. No reference survives a syscall. — TIMBERDOODLE
fn actor_read(channel: Handle, buf: &mut [u8]) -> Option<usize> {
    let port = reply_port()?;
    let timer = syscall_handle(timer_create(15_000_000_000))?;
    let tk = handle_koid(timer);
    let ck = handle_koid(channel);
    let bound = port_bind(port, timer) == 0 && port_bind(port, channel) == 0;
    let result = if bound {
        loop {
            let source = port_wait(port);
            if dispatch_service_event(source) {
                continue;
            }
            if source == tk || source == u64::MAX {
                break None;
            }
            if source == ck {
                let n = channel_read(channel, buf.as_mut_ptr(), buf.len()) as usize;
                break (n <= buf.len()).then_some(n);
            }
        }
    } else {
        None
    };
    let _ = port_unbind(port, channel);
    let _ = port_unbind(port, timer);
    let _ = handle_close(timer);
    result
}
fn actor_snapshot(root: Handle, op: u8) -> Option<kumo_abi::actor::Snapshot> {
    let mut query = [0; 5];
    query[..4].copy_from_slice(&kumo_abi::actor::QUERY);
    query[4] = op;
    if channel_write(root, query.as_ptr(), 5) != 0 {
        return None;
    }
    let mut b = [0; kumo_abi::actor::SNAPSHOT_LEN];
    let n = actor_read(root, &mut b)?;
    kumo_abi::actor::Snapshot::decode(&b[..n])
}
fn actor_print(label: &str, s: kumo_abi::actor::Snapshot) {
    log(alloc::format!("AX0 census {} capacity={} live={} stored={} blocked={} ready={} running={} free={} frames={} heap={}\n",label,s.capacity,s.live,s.stored,s.blocked,s.ready,s.running,s.capacity.saturating_sub(s.live),s.frames_issued,s.heap_used).as_bytes());
    log(alloc::format!("AX0 processes {} {:?}\n", label, s.processes).as_bytes());
}
#[derive(Clone, Copy)]
struct ActorChild {
    process: Handle,
    control: Handle,
}
fn actor_stop(child: ActorChild) {
    let _ = process_kill(child.process);
    let _ = handle_close(child.process);
    let _ = handle_close(child.control);
}
fn actor_spawn(initrd: Handle, peer: Handle) -> Option<(ActorChild, u64)> {
    let (control, server) = channel_create_pair();
    if control == u64::MAX || server == u64::MAX {
        return None;
    }
    let control = Handle(control as u32);
    let mut process = None;
    let start = kumo_rt::clock_get();
    let flags =
        (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG | ProcessRunFlags::TRANSFER_ARG2)
            .bits();
    let loaded = run_elf_threads(
        initrd,
        b"bin/actorprobe",
        server,
        u64::from(peer.0),
        flags,
        b"actorprobe",
        0,
        Some(&mut process),
    );
    let child = process.map(|process| ActorChild { process, control });
    if !loaded {
        if let Some(child) = child {
            actor_stop(child);
        } else {
            let _ = handle_close(control);
        }
        let _ = handle_close(Handle(server as u32));
        return None;
    }
    let Some(child) = child else {
        let _ = handle_close(control);
        return None;
    };
    let mut b = [0; 32];
    let ready = actor_read(control, &mut b) == Some(32) && &b[..4] == b"AXP1" && b[4] == b'R';
    let elapsed = kumo_rt::clock_get().saturating_sub(start);
    if ready {
        Some((child, elapsed))
    } else {
        actor_stop(child);
        None
    }
}
fn actor_result(child: ActorChild) -> Option<u64> {
    let mut b = [0; 32];
    if actor_read(child.control, &mut b) != Some(32)
        || &b[..4] != b"AXP1"
        || b[4] != b'D'
        || u32::from_le_bytes(b[8..12].try_into().ok()?) != kumo_abi::actor::ROUNDS
        || b[24..32] != [0; 8]
    {
        return None;
    }
    Some(u64::from_le_bytes(b[16..24].try_into().ok()?))
}
fn actor_trial(initrd: Handle, root: Handle, index: u32) -> bool {
    let Some(base) = actor_snapshot(root, 1) else {
        return false;
    };
    let (a, b) = channel_create_pair();
    if a == u64::MAX || b == u64::MAX {
        return false;
    }
    let Some((echo, spawn_a)) = actor_spawn(initrd, Handle(a as u32)) else {
        let _ = handle_close(Handle(a as u32));
        let _ = handle_close(Handle(b as u32));
        return false;
    };
    let one = actor_snapshot(root, 0);
    let client = actor_spawn(initrd, Handle(b as u32));
    let Some((client, spawn_b)) = client else {
        actor_stop(echo);
        let _ = handle_close(Handle(b as u32));
        return false;
    };
    let two = actor_snapshot(root, 0);
    let ok = one.is_some_and(|s| s.live == base.live + 1)
        && two.is_some_and(|s| s.live == base.live + 2);
    let ping_base = actor_snapshot(root, 1);
    let sent = channel_write(client.control, b"G".as_ptr(), 1) == 0;
    let result = if sent { actor_result(client) } else { None };
    let peer = if result.is_some() {
        actor_result(echo)
    } else {
        None
    };
    let costs = actor_snapshot(root, 2);
    // Each probe sends at most Ready + Done. Drain a possibly unanswered Done, then require
    // EOF; a silent child is killed through our retained handle before it is closed.
    // — TIMBERDOODLE
    for child in [client, echo] {
        let _ = channel_write(child.control, b"Q".as_ptr(), 1);
    }
    for child in [client, echo] {
        let mut closed = false;
        let mut tail = [0; 32];
        for _ in 0..2 {
            match actor_read(child.control, &mut tail) {
                Some(0) => {
                    closed = true;
                    break;
                }
                Some(32) => {}
                _ => break,
            }
        }
        if !closed {
            let _ = process_kill(child.process);
        }
        let _ = handle_close(child.process);
        let _ = handle_close(child.control);
    }
    let clean = actor_snapshot(root, 0);
    if let (Some(one), Some(two), Some(ns), Some(_), Some(costs), Some(clean), Some(ping_base)) =
        (one, two, result, peer, costs, clean, ping_base)
    {
        let good = ok && clean.live == base.live && ns > 0 && costs.roots > 0;
        log(alloc::format!(
            "AX0 trial={} rounds={} bytes={} rtt-total-ns={} spawn-a-ns={} spawn-b-ns={} {}\n",
            index,
            kumo_abi::actor::ROUNDS,
            kumo_abi::actor::PAYLOAD,
            ns,
            spawn_a,
            spawn_b,
            if good { "PASS" } else { "FAIL" }
        )
        .as_bytes());
        log(alloc::format!(
            "AX0 memory trial={} frames-a={} frames-b={} heap-a={} heap-b={} clean={}\n",
            index,
            one.frames_issued.saturating_sub(base.frames_issued),
            two.frames_issued.saturating_sub(one.frames_issued),
            one.heap_used as i128 - base.heap_used as i128,
            two.heap_used as i128 - one.heap_used as i128,
            clean.live
        )
        .as_bytes());
        log(alloc::format!("AX0 switch trial={} syscalls={} switches={} root-count={} root-total-ns={} root-min-ns={} root-max-ns={} simd-errors=0\n",
            index,
            (costs.syscalls as u32).wrapping_sub(ping_base.syscalls as u32),
            costs.switches.saturating_sub(ping_base.switches),
            costs.roots, costs.root_ns, costs.root_min_ns, costs.root_max_ns,
        ).as_bytes());
        good
    } else {
        false
    }
}
fn actor_benchmark(initrd: Handle, root: Handle) {
    if BG_STDOUT.load(Ordering::Relaxed) != 0 || NET_SUPERVISOR.get().initrd.0 != 0 {
        log(b"AX0 FAIL needs a fresh boot and no background job\n");
        return;
    }
    log(b"AX0 begins: diagnostic costs; no policy change\n");
    let Some(base) = actor_snapshot(root, 0) else {
        log(b"AX0 FAIL snapshot\n");
        return;
    };
    actor_print("default-login", base);
    let mut ok = true;
    for n in 1..=5 {
        if !actor_trial(initrd, root, n) {
            ok = false;
            break;
        }
    }
    if let Some(s) = actor_snapshot(root, 8) {
        log(alloc::format!(
            "AX0 kernel-clock-pair samples={} total-ns={}\n",
            s.roots,
            s.root_ns
        )
        .as_bytes());
    }
    let mut timer_total = 0;
    for _ in 0..1024 {
        let a = kumo_rt::clock_get();
        timer_total += kumo_rt::clock_get().saturating_sub(a);
    }
    log(alloc::format!("AX0 clock-pair samples=1024 total-ns={}\n", timer_total).as_bytes());
    for (index, n) in [1, 2, 4, 8, 12].into_iter().enumerate() {
        if let Some(s) = actor_snapshot(root, 3 + index as u8) {
            log(alloc::format!(
                "AX0 scheduler runnable={} decisions={} total-ns={}\n",
                n,
                s.roots,
                s.root_ns
            )
            .as_bytes());
            if s.root_ns == u64::MAX {
                ok = false;
            }
        } else {
            ok = false;
        }
    }
    if ok {
        let args = [
            alloc::string::String::from("netprobe"),
            alloc::string::String::from("hold"),
        ];
        run_program_background(initrd, &args, true);
        if let Some(s) = actor_snapshot(root, 0) {
            actor_print("netstack-client", s);
            ok &= s.live == base.live + 2;
            log(alloc::format!(
                "AX0 capacity-gate free={} requires=2 {}\n",
                s.capacity.saturating_sub(s.live),
                if s.capacity.saturating_sub(s.live) >= 2 {
                    "PASS"
                } else {
                    "AX4-REQUIRED"
                }
            )
            .as_bytes());
        } else {
            ok = false;
        }
        if BG_STDOUT.load(Ordering::Relaxed) != 0 {
            wait_background();
        }
    }
    let _ = actor_snapshot(root, 2);
    log(if ok { b"AX0 PASS\n" } else { b"AX0 FAIL\n" });
}

// AX2 is opt-in, like AX0, but has its own pending flag and timer: the AX0 command and its
// statics are deliberately untouched. One benchmark per boot: `actorbench` and `ax2bench`
// in the same autoexec would arm two five-second timers and put four probe children on the
// machine, against the metal two-child rule.
static AX2_PENDING: AtomicBool = AtomicBool::new(false);
static AX2_INITRD: AtomicU32 = AtomicU32::new(0);
static AX2_ROOT: AtomicU32 = AtomicU32::new(0);
static AX2_TIMER: Shared<Option<(Handle, u64)>> = Shared(core::cell::UnsafeCell::new(None));

/// The AX2 call path's cost per round trip: caller ChannelCall + server ChannelRead +
/// server ChannelReply, and the two scheduler switches they cause. The trial bracket also
/// contains Sora's bounded reads and snapshots; AX2_SLACK pays for those fixed observer
/// costs so the line measures the mechanism, not the supervisor. The harness applies the
/// same arithmetic; `docs/actor-call.md` records the convention.
const AX2_ROUNDS: u32 = 1_024;
const AX2_CALL_SYSCALLS: u64 = 3;
const AX2_CALL_SWITCHES: u64 = 2;
const AX2_SLACK: u64 = 64;
const AX2_REPEAT_ROUNDS: u32 = 100;
const AX2_NEVER_DEADLINE_MS: u64 = 100;
const AX2_TRIAL_DEADLINE_MS: u64 = 5_000;
/// One 20 Hz tick is 50 ms; 75 ms guarantees at least one tick inside each `hold` reply.
const AX2_HOLD_MS: u64 = 75;
/// Quiescence gate before the recorded series (metal boot 1's first trial absorbed background
/// boot work): two samples one settle interval apart must differ by at most this many IPC
/// syscalls, or the boot is still busy. Retry for at most AX2_SETTLE_TRIES intervals (10 s).
const AX2_SETTLE_SYSCALLS: u64 = 100;
const AX2_SETTLE_SAMPLE_NS: u64 = 100_000_000;
const AX2_SETTLE_TRIES: u32 = 100;

fn ax2_arm(port: Handle, initrd: Handle, root: Handle) {
    if !AX2_PENDING.swap(false, Ordering::Relaxed) {
        return;
    }
    AX2_INITRD.store(initrd.0, Ordering::Relaxed);
    AX2_ROOT.store(root.0, Ordering::Relaxed);
    if let Some(timer) = syscall_handle(timer_create(5_000_000_000)) {
        if port_bind(port, timer) == 0 {
            AX2_TIMER.set(Some((timer, handle_koid(timer))));
        } else {
            let _ = handle_close(timer);
        }
    }
}
fn ax2_event(source: u64) -> bool {
    if let Some((timer, _)) = AX2_TIMER.get().filter(|t| t.1 == source) {
        AX2_TIMER.set(None);
        let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
        let _ = port_unbind(port, timer);
        let _ = handle_close(timer);
        ax2_benchmark(
            Handle(AX2_INITRD.load(Ordering::Relaxed)),
            Handle(AX2_ROOT.load(Ordering::Relaxed)),
        );
        return true;
    }
    false
}

/// The 32-byte AXP2 control frame actorcall writes before it exits. Layout and transform
/// are documented in `userland/actorcall/src/main.rs` and `docs/actor-call.md`.
#[derive(Clone, Copy)]
struct Ax2Frame {
    role: u8,
    rounds: u32,
    status: i32,
    errors: u32,
    simd_errors: u32,
    total_ns: u64,
}
impl Ax2Frame {
    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 32 || &bytes[..4] != b"AXP2" {
            return None;
        }
        Some(Self {
            role: bytes[4],
            rounds: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
            status: i32::from_le_bytes(bytes[12..16].try_into().ok()?),
            errors: u32::from_le_bytes(bytes[16..20].try_into().ok()?),
            simd_errors: u32::from_le_bytes(bytes[20..24].try_into().ok()?),
            total_ns: u64::from_le_bytes(bytes[24..32].try_into().ok()?),
        })
    }
}
fn ax2_read(control: Handle) -> Option<Ax2Frame> {
    let mut bytes = [0u8; 32];
    if actor_read(control, &mut bytes) != Some(32) {
        return None;
    }
    Ax2Frame::decode(&bytes)
}
fn ax2_status_name(status: i32) -> &'static str {
    match status {
        0 => "Ok",
        s if s == Errno::PeerClosed.status() => "PeerClosed",
        s if s == Errno::TimedOut.status() => "TimedOut",
        s if s == Errno::BadState.status() => "BadState",
        s if s == Errno::InvalidArgs.status() => "InvalidArgs",
        _ => "Status",
    }
}

#[derive(Clone, Copy)]
struct Ax2Child {
    process: Handle,
    control: Handle,
}
fn ax2_stop(child: Ax2Child) {
    let _ = process_kill(child.process);
    let _ = handle_close(child.process);
    let _ = handle_close(child.control);
}

/// Wait for a child's ready frame (role `R`), proving it was admitted and entered its role.
fn ax2_ready(child: Ax2Child) -> bool {
    ax2_read(child.control).is_some_and(|f| f.role == b'R' && f.rounds == 0 && f.status == 0)
}

/// Release the caller past its go gate. Sora sends this only after the baseline snapshot.
fn ax2_go(caller: Ax2Child) -> bool {
    channel_write(caller.control, b"G".as_ptr(), 1) == 0
}

/// Spawn one actorcall child. The bootstrap channel carries the argv VMO (`a`) and the
/// child's own control endpoint (`c`); the call endpoint travels in ProcessRun's second
/// argument, so each child gets exactly two channels and no ambient authority.
fn ax2_spawn(initrd: Handle, args: &[&[u8]], peer: Handle) -> Option<Ax2Child> {
    let (control_raw, control_child_raw) = channel_create_pair();
    if control_raw == u64::MAX || control_child_raw == u64::MAX {
        let _ = handle_close(Handle(control_raw as u32));
        let _ = handle_close(Handle(control_child_raw as u32));
        return None;
    }
    let (boot_raw, boot_child_raw) = channel_create_pair();
    if boot_raw == u64::MAX || boot_child_raw == u64::MAX {
        let _ = handle_close(Handle(control_raw as u32));
        let _ = handle_close(Handle(control_child_raw as u32));
        let _ = handle_close(Handle(boot_raw as u32));
        let _ = handle_close(Handle(boot_child_raw as u32));
        return None;
    }
    let control = Handle(control_raw as u32);
    let boot = Handle(boot_raw as u32);
    let argv_raw = argv_transfer_handle(args);
    if argv_raw == 0 || argv_raw == u64::MAX {
        let _ = handle_close(control);
        let _ = handle_close(boot);
        let _ = handle_close(Handle(boot_child_raw as u32));
        let _ = handle_close(Handle(control_child_raw as u32));
        return None;
    }
    let argv = Handle(argv_raw as u32);
    let argv_queued =
        channel_write_with_handle(boot, &STARTUP_TAG_ARGV, 1, argv) == Errno::Ok.status();
    if !argv_queued {
        let _ = handle_close(argv);
    }
    let control_child = Handle(control_child_raw as u32);
    let control_queued = argv_queued
        && channel_write_with_handle(boot, &STARTUP_TAG_CAP0, 1, control_child)
            == Errno::Ok.status();
    if !control_queued {
        let _ = handle_close(control_child);
    }
    let _ = handle_close(boot);
    if !control_queued {
        let _ = handle_close(Handle(boot_child_raw as u32));
        let _ = handle_close(control);
        return None;
    }
    let flags =
        (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG | ProcessRunFlags::TRANSFER_ARG2)
            .bits();
    let mut process = None;
    let loaded = run_elf_threads(
        initrd,
        kumo_abi::initrd::ACTORCALL_PATH.as_bytes(),
        boot_child_raw,
        u64::from(peer.0),
        flags,
        b"actorcall",
        0,
        Some(&mut process),
    );
    let child = process.map(|process| Ax2Child { process, control });
    if loaded {
        if let Some(child) = child {
            return Some(child);
        }
        let _ = handle_close(control);
        return None;
    }
    if let Some(child) = child {
        ax2_stop(child);
    } else {
        let _ = handle_close(control);
    }
    let _ = handle_close(Handle(boot_child_raw as u32));
    None
}

/// Create the two channel ends and spawn the server first, the caller second. Both ends
/// are transferred; Sora retains no call-channel authority. On failure the still-owned
/// end is closed, mirroring `actor_spawn`/`actor_trial`.
fn ax2_pair(
    initrd: Handle,
    server_args: &[&[u8]],
    client_args: &[&[u8]],
) -> Option<(Ax2Child, Ax2Child)> {
    let (a, b) = channel_create_pair();
    if a == u64::MAX || b == u64::MAX {
        let _ = handle_close(Handle(a as u32));
        let _ = handle_close(Handle(b as u32));
        return None;
    }
    let Some(server) = ax2_spawn(initrd, server_args, Handle(a as u32)) else {
        let _ = handle_close(Handle(a as u32));
        let _ = handle_close(Handle(b as u32));
        return None;
    };
    let Some(caller) = ax2_spawn(initrd, client_args, Handle(b as u32)) else {
        ax2_stop(server);
        let _ = handle_close(Handle(b as u32));
        return None;
    };
    // Both children announce themselves before the baseline snapshot; the caller then
    // blocks on the go byte, so neither can exit between the snapshot and the calls.
    if !ax2_ready(server) || !ax2_ready(caller) {
        ax2_stop(caller);
        ax2_stop(server);
        return None;
    }
    Some((server, caller))
}

fn ax2_trial(initrd: Handle, root: Handle, kind: u32, index: u32) -> bool {
    let Some(before) = actor_snapshot(root, 1) else {
        return false;
    };
    let rounds_text = alloc::format!("{}", AX2_ROUNDS);
    let bytes_text = alloc::format!("{}", kind);
    let deadline_text = alloc::format!("{}", AX2_TRIAL_DEADLINE_MS);
    let ping_args = [
        &b"actorcall"[..],
        &b"ping"[..],
        rounds_text.as_bytes(),
        bytes_text.as_bytes(),
        deadline_text.as_bytes(),
    ];
    let server_args = [&b"actorcall"[..], &b"pong"[..]];
    let Some((server, caller)) = ax2_pair(initrd, &server_args, &ping_args) else {
        return false;
    };
    let Some(base) = actor_snapshot(root, 1) else {
        ax2_stop(caller);
        ax2_stop(server);
        return false;
    };
    let two_ok = base.live == before.live + 2;
    if !ax2_go(caller) {
        ax2_stop(caller);
        ax2_stop(server);
        return false;
    }
    let ping = ax2_read(caller.control);
    let pong = ax2_read(server.control);
    let costs = actor_snapshot(root, 2);
    ax2_stop(caller);
    ax2_stop(server);
    let clean = actor_snapshot(root, 0);
    let (syscalls, switches, frames) = match costs {
        Some(costs) => (
            costs.syscalls.saturating_sub(base.syscalls),
            costs.switches.saturating_sub(base.switches),
            costs.frames_issued.saturating_sub(base.frames_issued),
        ),
        None => (u64::MAX, u64::MAX, u64::MAX),
    };
    let simd_errors = ping
        .map(|f| f.simd_errors)
        .unwrap_or(0)
        .saturating_add(pong.map(|f| f.simd_errors).unwrap_or(0));
    let rounds = ping.map(|f| f.rounds).unwrap_or(0);
    let total_ns = ping.map(|f| f.total_ns).unwrap_or(0);
    let good = two_ok
        && clean.is_some_and(|s| s.live == before.live)
        && ping.is_some_and(|f| {
            f.role == b'p'
                && f.rounds == AX2_ROUNDS
                && f.status == 0
                && f.errors == 0
                && f.simd_errors == 0
                && f.total_ns > 0
        })
        && pong.is_some_and(|f| {
            f.role == b'o'
                && f.rounds == AX2_ROUNDS
                && f.status == 0
                && f.errors == 0
                && f.simd_errors == 0
        })
        && frames == 0
        && simd_errors == 0
        && syscalls <= AX2_CALL_SYSCALLS * u64::from(AX2_ROUNDS) + AX2_SLACK
        && switches <= AX2_CALL_SWITCHES * u64::from(AX2_ROUNDS) + AX2_SLACK;
    log(alloc::format!(
        "AX2 trial={} kind={} rounds={} rtt-total-ns={} syscalls={} switches={} frames={} simd-errors={} {}\n",
        index,
        kind,
        rounds,
        total_ns,
        syscalls,
        switches,
        frames,
        simd_errors,
        if good { "PASS" } else { "FAIL" }
    )
    .as_bytes());
    good
}

fn ax2_matrix_die(initrd: Handle, root: Handle) -> bool {
    const ROUNDS: &str = "1";
    const BYTES: &str = "8";
    const DEADLINE: &str = "5000";
    let Some(before) = actor_snapshot(root, 1) else {
        log(b"AX2 matrix=die result=Status FAIL\n");
        return false;
    };
    let server_args = [&b"actorcall"[..], &b"die"[..]];
    let client_args = [
        &b"actorcall"[..],
        &b"ping"[..],
        ROUNDS.as_bytes(),
        BYTES.as_bytes(),
        DEADLINE.as_bytes(),
    ];
    let Some((server, caller)) = ax2_pair(initrd, &server_args, &client_args) else {
        log(b"AX2 matrix=die result=Status FAIL\n");
        return false;
    };
    let Some(base) = actor_snapshot(root, 1) else {
        ax2_stop(caller);
        ax2_stop(server);
        log(b"AX2 matrix=die result=Status FAIL\n");
        return false;
    };
    let two_ok = base.live == before.live + 2;
    let go = ax2_go(caller);
    let ping = ax2_read(caller.control);
    let died = ax2_read(server.control);
    ax2_stop(caller);
    ax2_stop(server);
    let clean = actor_snapshot(root, 0);
    let peer_closed = Errno::PeerClosed.status();
    let good = two_ok
        && go
        && clean.is_some_and(|s| s.live == before.live)
        && ping.is_some_and(|f| {
            f.role == b'p'
                && f.rounds == 1
                && f.status == peer_closed
                && f.errors == 0
                && f.simd_errors == 0
        })
        && died.is_some_and(|f| f.role == b'd' && f.rounds == 1 && f.status == 0 && f.errors == 0);
    let result = ping.map(|f| f.status).unwrap_or(0);
    log(alloc::format!(
        "AX2 matrix=die result={} {}\n",
        ax2_status_name(result),
        if good { "PASS" } else { "FAIL" }
    )
    .as_bytes());
    good
}

fn ax2_matrix_never(initrd: Handle, root: Handle) -> bool {
    const ROUNDS: &str = "1";
    const BYTES: &str = "8";
    let deadline_text = alloc::format!("{}", AX2_NEVER_DEADLINE_MS);
    let Some(before) = actor_snapshot(root, 1) else {
        log(b"AX2 matrix=never result=Status FAIL\n");
        return false;
    };
    let server_args = [&b"actorcall"[..], &b"never"[..]];
    let client_args = [
        &b"actorcall"[..],
        &b"ping"[..],
        ROUNDS.as_bytes(),
        BYTES.as_bytes(),
        deadline_text.as_bytes(),
    ];
    let Some((server, caller)) = ax2_pair(initrd, &server_args, &client_args) else {
        log(b"AX2 matrix=never result=Status FAIL\n");
        return false;
    };
    let Some(base) = actor_snapshot(root, 1) else {
        ax2_stop(caller);
        ax2_stop(server);
        log(b"AX2 matrix=never result=Status FAIL\n");
        return false;
    };
    let two_ok = base.live == before.live + 2;
    let go = ax2_go(caller);
    let ping = ax2_read(caller.control);
    let silent = ax2_read(server.control);
    ax2_stop(caller);
    ax2_stop(server);
    let clean = actor_snapshot(root, 0);
    let timed_out = Errno::TimedOut.status();
    let deadline_ns = AX2_NEVER_DEADLINE_MS * 1_000_000;
    let good = two_ok
        && go
        && clean.is_some_and(|s| s.live == before.live)
        && ping.is_some_and(|f| {
            f.role == b'p'
                && f.rounds == 1
                && f.status == timed_out
                && f.errors == 0
                && f.simd_errors == 0
                && f.total_ns >= deadline_ns
        })
        && silent.is_some_and(|f| {
            f.role == b'n' && f.rounds == 1 && f.status == Errno::BadState.status() && f.errors == 0
        });
    let result = ping.map(|f| f.status).unwrap_or(0);
    log(alloc::format!(
        "AX2 matrix=never result={} {}\n",
        ax2_status_name(result),
        if good { "PASS" } else { "FAIL" }
    )
    .as_bytes());
    good
}

fn ax2_matrix_hold(initrd: Handle, root: Handle) -> bool {
    const ROUNDS: &str = "3";
    const BYTES: &str = "8";
    const DEADLINE: &str = "5000";
    let hold_text = alloc::format!("{}", AX2_HOLD_MS);
    let Some(before) = actor_snapshot(root, 1) else {
        log(b"AX2 census hold switches=0 rounds=0\n");
        log(b"AX2 matrix=hold result=Status FAIL\n");
        return false;
    };
    let server_args = [&b"actorcall"[..], &b"hold"[..], hold_text.as_bytes()];
    let client_args = [
        &b"actorcall"[..],
        &b"ping"[..],
        ROUNDS.as_bytes(),
        BYTES.as_bytes(),
        DEADLINE.as_bytes(),
    ];
    let Some((server, caller)) = ax2_pair(initrd, &server_args, &client_args) else {
        log(b"AX2 census hold switches=0 rounds=0\n");
        log(b"AX2 matrix=hold result=Status FAIL\n");
        return false;
    };
    let Some(base) = actor_snapshot(root, 1) else {
        ax2_stop(caller);
        ax2_stop(server);
        log(b"AX2 census hold switches=0 rounds=0\n");
        log(b"AX2 matrix=hold result=Status FAIL\n");
        return false;
    };
    let two_ok = base.live == before.live + 2;
    let go = ax2_go(caller);
    let ping = ax2_read(caller.control);
    let held = ax2_read(server.control);
    let costs = actor_snapshot(root, 2);
    ax2_stop(caller);
    ax2_stop(server);
    let clean = actor_snapshot(root, 0);
    let switches = costs
        .map(|c| c.switches.saturating_sub(base.switches))
        .unwrap_or(0);
    let rounds = held.map(|f| f.rounds).unwrap_or(0);
    log(alloc::format!("AX2 census hold switches={} rounds={}\n", switches, rounds).as_bytes());
    let hold_ns = AX2_HOLD_MS * 1_000_000;
    let good = two_ok
        && go
        && switches > 0
        && clean.is_some_and(|s| s.live == before.live)
        && ping.is_some_and(|f| {
            f.role == b'p'
                && f.rounds == 3
                && f.status == 0
                && f.errors == 0
                && f.simd_errors == 0
                && f.total_ns >= 3 * hold_ns
        })
        && held.is_some_and(|f| f.role == b'h' && f.rounds == 3 && f.status == 0 && f.errors == 0);
    log(alloc::format!(
        "AX2 matrix=hold result={} {}\n",
        ax2_status_name(ping.map(|f| f.status).unwrap_or(0)),
        if good { "PASS" } else { "FAIL" }
    )
    .as_bytes());
    good
}

fn ax2_matrix_repeat(initrd: Handle, root: Handle) -> bool {
    let rounds_text = alloc::format!("{}", AX2_REPEAT_ROUNDS);
    let deadline_text = alloc::format!("{}", AX2_TRIAL_DEADLINE_MS);
    let before = actor_snapshot(root, 1);
    let server_args = [&b"actorcall"[..], &b"pong"[..]];
    let client_args = [
        &b"actorcall"[..],
        &b"ping"[..],
        rounds_text.as_bytes(),
        &b"8"[..],
        deadline_text.as_bytes(),
    ];
    let mut frames = u64::MAX;
    let mut live = 0u32;
    let mut stored = 0u32;
    let mut free = 0u32;
    let mut outcome = false;
    if let Some(before) = before {
        if let Some((server, caller)) = ax2_pair(initrd, &server_args, &client_args) {
            let base = actor_snapshot(root, 1);
            let two_ok = base.is_some_and(|b| b.live == before.live + 2) && ax2_go(caller);
            let ping = ax2_read(caller.control);
            let pong = ax2_read(server.control);
            let costs = actor_snapshot(root, 2);
            ax2_stop(caller);
            ax2_stop(server);
            let clean = actor_snapshot(root, 0);
            frames = match (base, costs) {
                (Some(base), Some(costs)) => costs.frames_issued.saturating_sub(base.frames_issued),
                _ => u64::MAX,
            };
            live = clean.map(|s| s.live).unwrap_or(0);
            stored = clean.map(|s| s.stored).unwrap_or(0);
            free = clean
                .map(|s| s.capacity.saturating_sub(s.live))
                .unwrap_or(0);
            outcome = two_ok
                && clean.is_some_and(|s| s.live == before.live)
                && frames == 0
                && ping.is_some_and(|f| {
                    f.role == b'p'
                        && f.rounds == AX2_REPEAT_ROUNDS
                        && f.status == 0
                        && f.errors == 0
                        && f.simd_errors == 0
                })
                && pong.is_some_and(|f| {
                    f.role == b'o'
                        && f.rounds == AX2_REPEAT_ROUNDS
                        && f.status == 0
                        && f.errors == 0
                });
        }
    }
    log(alloc::format!(
        "AX2 census repeat frames={} live={} stored={} free={}\n",
        frames,
        live,
        stored,
        free
    )
    .as_bytes());
    log(alloc::format!(
        "AX2 matrix=repeat result={} {}\n",
        if outcome { "Ok" } else { "Status" },
        if outcome { "PASS" } else { "FAIL" }
    )
    .as_bytes());
    outcome
}

/// Wait for boot bring-up work to drain before the recorded series. Two cost snapshots one
/// settle interval apart must differ by at most `AX2_SETTLE_SYSCALLS`; retry for up to
/// ten seconds and log every sample. A boot that never settles fails the run instead of
/// charging background work to trial 1.
fn ax2_settle(root: Handle) -> bool {
    let mut sample = 0u32;
    let mut last = 0u64;
    while sample < AX2_SETTLE_TRIES {
        let Some(a) = actor_snapshot(root, 2) else {
            log(b"AX2 settle snapshot failed\n");
            return false;
        };
        let _ = kumo_rt::sleep_ns(AX2_SETTLE_SAMPLE_NS);
        let Some(b) = actor_snapshot(root, 2) else {
            log(b"AX2 settle snapshot failed\n");
            return false;
        };
        sample = sample.saturating_add(1);
        last = b.syscalls.saturating_sub(a.syscalls);
        log(alloc::format!(
            "AX2 settle sample={} syscalls={} delta={}\n",
            sample,
            b.syscalls,
            last
        )
        .as_bytes());
        if last <= AX2_SETTLE_SYSCALLS {
            log(alloc::format!("AX2 settle ok sample={} delta={}\n", sample, last).as_bytes());
            return true;
        }
    }
    log(alloc::format!("AX2 settle gave up delta={}\n", last).as_bytes());
    false
}

fn ax2_benchmark(initrd: Handle, root: Handle) {
    if BG_STDOUT.load(Ordering::Relaxed) != 0 || NET_SUPERVISOR.get().initrd.0 != 0 {
        log(b"AX2 FAIL needs a fresh boot and no background job\n");
        return;
    }
    log(b"AX2 begins: ChannelCall register fast path (PLAN/021 AX2)\n");
    if !ax2_settle(root) {
        log(b"AX2 FAIL settle\n");
        return;
    }
    let Some(base) = actor_snapshot(root, 0) else {
        log(b"AX2 FAIL snapshot\n");
        return;
    };
    log(alloc::format!(
        "AX2 census default-login capacity={} live={} stored={} blocked={} ready={} running={} free={} frames={} heap={}\n",
        base.capacity,
        base.live,
        base.stored,
        base.blocked,
        base.ready,
        base.running,
        base.capacity.saturating_sub(base.live),
        base.frames_issued,
        base.heap_used
    )
    .as_bytes());
    let free = base.capacity.saturating_sub(base.live);
    log(alloc::format!(
        "AX2 capacity-gate free={} requires=2 {}\n",
        free,
        if free >= 2 { "PASS" } else { "AX4-REQUIRED" }
    )
    .as_bytes());
    if free < 2 {
        log(b"AX2 FAIL capacity (AX4-REQUIRED)\n");
        return;
    }
    let mut ok = true;
    for kind in [8u32, 32u32] {
        for index in 1..=5u32 {
            if !ax2_trial(initrd, root, kind, index) {
                ok = false;
            }
        }
    }
    if !ax2_matrix_die(initrd, root) {
        ok = false;
    }
    if !ax2_matrix_never(initrd, root) {
        ok = false;
    }
    if !ax2_matrix_hold(initrd, root) {
        ok = false;
    }
    if !ax2_matrix_repeat(initrd, root) {
        ok = false;
    }
    let _ = actor_snapshot(root, 2);
    log(if ok { b"AX2 PASS\n" } else { b"AX2 FAIL\n" });
}

// ---------------------------------------------------------------------------------------
// AX4: resident actor capacity (PLAN/021 AX4).
//
// AXB2/AXR2 is the variable-length capacity snapshot: a 96-byte header in AXR1's field
// order followed by `capacity` little-endian process koids. The kernel/ABI owner lands the
// matching codec in `kumo_abi::actor`; until it exists this driver carries the documented
// layout locally. AXB1/AXR1 ops 0..=8 are untouched, so `actorbench` and `ax2bench` stay
// byte-identical.
// ---------------------------------------------------------------------------------------
const AX4_MAX_CAPACITY: usize = 288;
const AX4_BATCH: u32 = 256;
const AX4_CYCLES: u32 = 5;
const AX4_SCHED_DECISIONS: u64 = 32_768;
const AX4_WAKE_NS: u64 = 15_000_000_000;
// The kernel heap is a fixed 8 MiB first-fit arena. A cycle's teardown must return every
// kernel object it created, so the census heap must be flat across the cycle; a residual
// means kernel state (Process/Thread/Channel/Port/Timer storage) outlived the actors.
// Measured with exact reclamation after the heap-stable clean settle: QEMU shows cycle 1
// carrying ~26 KiB of one-time Vec capacity and later cycles within ~1 KiB, and the pre-fix
// leak was 213-535 KiB *every* cycle. The gate is 64 KiB for every cycle and for the
// end-to-end final census.
const AX4_HEAP_ALLOWANCE: i128 = 64 * 1024;
const AX4_QUERY: [u8; 4] = *b"AXB2";
const AX4_HEADER_LEN: usize = 96;
const AX4_FRAME_MAX: usize = AX4_HEADER_LEN + 8 * AX4_MAX_CAPACITY;
// The syscall discipline is shared with AX2: two samples one interval apart differ by at
// most AX2_SETTLE_SYSCALLS IPC syscalls, retried for up to ten seconds. A quiet syscall
// counter is not enough on metal: boot bring-up (display sessions, supervision) can change
// the resident set well after login. The settle therefore also requires the state counts
// and the live koid list to hold still for a full quiescence window, retried for up to
// 360 s. On the 2026-11-06 metal bring-up the census-visible drv-gpu session switch landed
// 31.7 s after the settle began, and the boot gpu-client exited ~120 s after it (19:56:38
// and 20:19:29 on the two metal boots). A window shorter than the gpu-client exit anchors
// the default census on an actor that then leaves mid-boundary: the fill's clean 279/279
// ready became live=287 stored=288, the over-limit spawn was legally admitted (status 0),
// and recovery/final read live=9/8. The window is 150 s so it restarts at the exit and
// completes ~270 s after the settle began, inside the 360 s retry budget.
const AX4_SETTLE_SYSCALLS: u64 = AX2_SETTLE_SYSCALLS;
const AX4_SETTLE_SAMPLE_NS: u64 = AX2_SETTLE_SAMPLE_NS;
const AX4_SETTLE_TRIES: u32 = AX2_SETTLE_TRIES;
const AX4_QUIESCE_STABLE_NS: u64 = 150_000_000_000;
const AX4_QUIESCE_SAMPLE_NS: u64 = 250_000_000;
const AX4_QUIESCE_MAX_NS: u64 = 360_000_000_000;
// Retirement and console delivery finish at different scheduling points. Matching process
// counts alone does not establish a clean heap: long-lived IPC inboxes must drain and return
// their burst capacity too. Require a bounded heap-stable window, then keep the 64 KiB gate.
// — TIMBERDOODLE
const AX4_CLEAN_STABLE_NS: u64 = 500_000_000;
const AX4_CLEAN_SAMPLE_NS: u64 = 100_000_000;
const AX4_CLEAN_MAX_NS: u64 = 10_000_000_000;
const AX4_CLEAN_EPSILON: u64 = 4 * 1024;

// AX4 and its capacity gate are opt-in like AX0/AX2, with independent pending flags and
// timers so the earlier commands and their statics stay untouched. One benchmark per boot.
static AX4_BOOT_WAIT_START: AtomicU64 = AtomicU64::new(0);
static AX4_PENDING: AtomicBool = AtomicBool::new(false);
static AX4_INITRD: AtomicU32 = AtomicU32::new(0);
static AX4_ROOT: AtomicU32 = AtomicU32::new(0);
static AX4_TIMER: Shared<Option<(Handle, u64)>> = Shared(core::cell::UnsafeCell::new(None));
// `ax4bench fill` / `ax4bench fill2` are development diagnostics: settle, snapshot, then
// run the boundary phase once or twice so capacity wedges and heap residuals can be studied
// in seconds. A second fill separates one-time capacity growth from per-actor retention.
// The proof always runs the full benchmark (no argument).
static AX4_BOUNDARY_RUNS: AtomicU32 = AtomicU32::new(0);
static AX4_GATE_PENDING: AtomicBool = AtomicBool::new(false);
static AX4_GATE_INITRD: AtomicU32 = AtomicU32::new(0);
static AX4_GATE_ROOT: AtomicU32 = AtomicU32::new(0);
static AX4_GATE_TIMER: Shared<Option<(Handle, u64)>> = Shared(core::cell::UnsafeCell::new(None));

fn ax4_arm(
    port: Handle,
    initrd: Handle,
    root: Handle,
    pending: &AtomicBool,
    initrd_slot: &AtomicU32,
    root_slot: &AtomicU32,
    timer_slot: &Shared<Option<(Handle, u64)>>,
) {
    if !pending.swap(false, Ordering::Relaxed) {
        return;
    }
    initrd_slot.store(initrd.0, Ordering::Relaxed);
    root_slot.store(root.0, Ordering::Relaxed);
    AX4_BOOT_WAIT_START.store(kumo_rt::clock_get(), Ordering::Relaxed);
    if let Some(timer) = syscall_handle(timer_create(5_000_000_000)) {
        if port_bind(port, timer) == 0 {
            timer_slot.set(Some((timer, handle_koid(timer))));
        } else {
            let _ = handle_close(timer);
        }
    }
}
fn ax4_gate_arm(port: Handle, initrd: Handle, root: Handle) {
    ax4_arm(
        port,
        initrd,
        root,
        &AX4_GATE_PENDING,
        &AX4_GATE_INITRD,
        &AX4_GATE_ROOT,
        &AX4_GATE_TIMER,
    );
}
// Keep Sora's display broker serving until the boot GPU client terminates. Blocking in
// ax4_settle before that point prevents its retirement replies, making the benchmark itself
// force a 120 s client timeout. This is bounded readiness, not a larger heap allowance.
// — TIMBERDOODLE
fn ax4_defer_boot(port: Handle, busy: bool, slot: &Shared<Option<(Handle, u64)>>) -> bool {
    if !busy {
        log(b"AX4 boot-services ready\n");
        return false;
    }
    let waited = kumo_rt::clock_get().saturating_sub(AX4_BOOT_WAIT_START.load(Ordering::Relaxed));
    if waited >= AX4_QUIESCE_MAX_NS {
        log(b"AX4 FAIL boot-services readiness timed out\n");
        return true;
    }
    if let Some(timer) = syscall_handle(timer_create(1_000_000_000)) {
        if port_bind(port, timer) == 0 {
            slot.set(Some((timer, handle_koid(timer))));
            return true;
        }
        let _ = handle_close(timer);
    }
    log(b"AX4 FAIL boot-services readiness timer unavailable\n");
    true
}

fn ax4_event(source: u64, boot_busy: bool) -> bool {
    if let Some((timer, _)) = AX4_TIMER.get().filter(|t| t.1 == source) {
        AX4_TIMER.set(None);
        let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
        let _ = port_unbind(port, timer);
        let _ = handle_close(timer);
        if ax4_defer_boot(port, boot_busy, &AX4_TIMER) {
            return true;
        }
        let boundary_runs = AX4_BOUNDARY_RUNS.swap(0, Ordering::Relaxed);
        ax4_benchmark(
            Handle(AX4_INITRD.load(Ordering::Relaxed)),
            Handle(AX4_ROOT.load(Ordering::Relaxed)),
            boundary_runs,
        );
        return true;
    }
    false
}
fn ax4_gate_event(source: u64, boot_busy: bool) -> bool {
    if let Some((timer, _)) = AX4_GATE_TIMER.get().filter(|t| t.1 == source) {
        AX4_GATE_TIMER.set(None);
        let port = Handle(SERVE_PORT.load(Ordering::Relaxed) as u32);
        let _ = port_unbind(port, timer);
        let _ = handle_close(timer);
        if ax4_defer_boot(port, boot_busy, &AX4_GATE_TIMER) {
            return true;
        }
        ax4_gate(
            Handle(AX4_GATE_INITRD.load(Ordering::Relaxed)),
            Handle(AX4_GATE_ROOT.load(Ordering::Relaxed)),
        );
        return true;
    }
    false
}

/// The decoded AXR2 snapshot. `processes` is capacity-long and holds the live process
/// koids in kernel order; comparing it across a cycle is the process-list equality test.
struct Ax4Snapshot {
    enabled: u32,
    capacity: u32,
    live: u32,
    stored: u32,
    blocked: u32,
    ready: u32,
    running: u32,
    frames_issued: u64,
    heap_used: u64,
    roots: u64,
    root_ns: u64,
    root_min_ns: u64,
    root_max_ns: u64,
    syscalls: u64,
    switches: u64,
    processes: alloc::vec::Vec<u64>,
}
impl Ax4Snapshot {
    fn empty() -> Self {
        Self {
            enabled: 0,
            capacity: 0,
            live: 0,
            stored: 0,
            blocked: 0,
            ready: 0,
            running: 0,
            frames_issued: 0,
            heap_used: 0,
            roots: 0,
            root_ns: 0,
            root_min_ns: 0,
            root_max_ns: 0,
            syscalls: 0,
            switches: 0,
            processes: alloc::vec::Vec::with_capacity(AX4_MAX_CAPACITY),
        }
    }
}

fn ax4_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn ax4_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        b[at],
        b[at + 1],
        b[at + 2],
        b[at + 3],
        b[at + 4],
        b[at + 5],
        b[at + 6],
        b[at + 7],
    ])
}

/// Decode AXR2 into `out`. The wire frame is fixed: the 96-byte header followed by all
/// `AX4_MAX_CAPACITY` process slots (the ABI writes every slot, zero after `live`). The
/// three runnable-state counts must partition `live`, exactly `live` koids must be set, and
/// they must occupy `[0, live)`; anything else is refused rather than half-adopted.
fn ax4_decode(bytes: &[u8], out: &mut Ax4Snapshot) -> bool {
    if bytes.len() != AX4_HEADER_LEN + 8 * AX4_MAX_CAPACITY || &bytes[..4] != b"AXR2" {
        return false;
    }
    let capacity = ax4_u32(bytes, 8);
    if capacity as usize > AX4_MAX_CAPACITY {
        return false;
    }
    out.enabled = ax4_u32(bytes, 4);
    out.capacity = capacity;
    out.live = ax4_u32(bytes, 12);
    out.stored = ax4_u32(bytes, 16);
    out.blocked = ax4_u32(bytes, 20);
    out.ready = ax4_u32(bytes, 24);
    out.running = ax4_u32(bytes, 28);
    out.frames_issued = ax4_u64(bytes, 32);
    out.heap_used = ax4_u64(bytes, 40);
    out.roots = ax4_u64(bytes, 48);
    out.root_ns = ax4_u64(bytes, 56);
    out.root_min_ns = ax4_u64(bytes, 64);
    out.root_max_ns = ax4_u64(bytes, 72);
    out.syscalls = ax4_u64(bytes, 80);
    out.switches = ax4_u64(bytes, 88);
    out.processes.clear();
    let mut nonzero = 0u32;
    let mut packed = true;
    for index in 0..AX4_MAX_CAPACITY {
        let koid = ax4_u64(bytes, AX4_HEADER_LEN + 8 * index);
        if koid != 0 {
            nonzero += 1;
            if index >= out.live as usize {
                packed = false;
            }
        } else if index < out.live as usize {
            packed = false;
        }
        out.processes.push(koid);
    }
    out.enabled <= 1
        && out.live <= out.capacity
        && out.stored >= out.live
        && out.stored <= out.capacity
        && out.blocked + out.ready + out.running == out.live
        && nonzero == out.live
        && packed
}

/// Send one AXB2 query and decode the AXR2 reply into `out`.
fn ax4_query(root: Handle, op: u8, frame: &mut [u8], out: &mut Ax4Snapshot) -> bool {
    let mut query = [0u8; 5];
    query[..4].copy_from_slice(&AX4_QUERY);
    query[4] = op;
    if channel_write(root, query.as_ptr(), query.len()) != 0 {
        return false;
    }
    match actor_read(root, frame) {
        Some(n) if n <= frame.len() => ax4_decode(&frame[..n], out),
        _ => false,
    }
}

fn ax4_print(label: &str, s: &Ax4Snapshot) {
    log(alloc::format!(
        "AX4 census {} capacity={} live={} stored={} blocked={} ready={} running={} free={} frames={} heap={}\n",
        label,
        s.capacity,
        s.live,
        s.stored,
        s.blocked,
        s.ready,
        s.running,
        s.capacity.saturating_sub(s.live),
        s.frames_issued,
        s.heap_used
    )
    .as_bytes());
}

fn ax4_states_equal(a: &Ax4Snapshot, b: &Ax4Snapshot) -> bool {
    a.capacity == b.capacity
        && a.live == b.live
        && a.stored == b.stored
        && a.blocked == b.blocked
        && a.ready == b.ready
        && a.running == b.running
}

/// The census fields that define a settled resident set. `capacity` is included because a
/// policy change would invalidate the anchor; only the live koid prefix participates in the
/// list comparison (decode guarantees it is packed and zero after `live`).
fn ax4_census_equal(a: &Ax4Snapshot, b: &Ax4Snapshot) -> bool {
    let live = a.live as usize;
    a.capacity == b.capacity
        && a.live == b.live
        && a.stored == b.stored
        && a.blocked == b.blocked
        && a.ready == b.ready
        && a.running == b.running
        && live <= a.processes.len()
        && live <= b.processes.len()
        && a.processes[..live] == b.processes[..live]
}

fn ax4_settle(root: Handle, frame: &mut [u8], a: &mut Ax4Snapshot, b: &mut Ax4Snapshot) -> bool {
    let mut sample = 0u32;
    let mut last = 0u64;
    let mut quiet = false;
    while sample < AX4_SETTLE_TRIES {
        if !ax4_query(root, 0, frame, a) {
            log(b"AX4 settle snapshot failed\n");
            return false;
        }
        let _ = kumo_rt::sleep_ns(AX4_SETTLE_SAMPLE_NS);
        if !ax4_query(root, 0, frame, b) {
            log(b"AX4 settle snapshot failed\n");
            return false;
        }
        sample = sample.saturating_add(1);
        last = b.syscalls.saturating_sub(a.syscalls);
        log(alloc::format!(
            "AX4 settle sample={} syscalls={} delta={}\n",
            sample,
            b.syscalls,
            last
        )
        .as_bytes());
        if last <= AX4_SETTLE_SYSCALLS {
            quiet = true;
            break;
        }
    }
    if !quiet {
        log(alloc::format!("AX4 settle gave up delta={} phase=syscalls\n", last).as_bytes());
        return false;
    }
    // `b` holds the freshest syscall-quiet snapshot; make it the quiescence baseline.
    core::mem::swap(a, b);
    let started = kumo_rt::clock_get();
    let mut stable_since = started;
    let mut quiesce = 0u32;
    loop {
        let _ = kumo_rt::sleep_ns(AX4_QUIESCE_SAMPLE_NS);
        if !ax4_query(root, 0, frame, b) {
            log(b"AX4 settle snapshot failed\n");
            return false;
        }
        quiesce = quiesce.saturating_add(1);
        let now = kumo_rt::clock_get();
        let stable = ax4_census_equal(a, b);
        let live = b.live;
        let stored = b.stored;
        let blocked = b.blocked;
        let ready = b.ready;
        let running = b.running;
        if !stable {
            core::mem::swap(a, b);
            stable_since = now;
        }
        let stable_ns = now.saturating_sub(stable_since);
        let waited_ns = now.saturating_sub(started);
        log(alloc::format!(
            "AX4 quiesce sample={} stable-ms={} waited-ms={} live={} stored={} blocked={} ready={} running={} change={}\n",
            quiesce,
            stable_ns / 1_000_000,
            waited_ns / 1_000_000,
            live,
            stored,
            blocked,
            ready,
            running,
            u8::from(!stable)
        )
        .as_bytes());
        if stable && stable_ns >= AX4_QUIESCE_STABLE_NS {
            // The last query `b` is the default-census anchor: it has been census-equal
            // to the baseline for the whole window, so the caller needs no extra query.
            log(alloc::format!("AX4 settle ok sample={} delta={}\n", sample, last).as_bytes());
            return true;
        }
        if waited_ns >= AX4_QUIESCE_MAX_NS {
            log(alloc::format!(
                "AX4 settle gave up stable-ms={} waited-ms={} live={} stored={} blocked={} ready={} running={} phase=census\n",
                stable_ns / 1_000_000,
                waited_ns / 1_000_000,
                live,
                stored,
                blocked,
                ready,
                running
            )
            .as_bytes());
            return false;
        }
    }
}

/// Wait for teardown to finish returning to `baseline`, then for the kernel heap to hold
/// still. `out` always receives the last AXR2 sample; the returned flag says whether the
/// full window completed. `change` reports whether that last sample moved the heap (or the
/// census) and is false on success.
fn ax4_clean_settle(
    root: Handle,
    frame: &mut [u8],
    baseline: &Ax4Snapshot,
    out: &mut Ax4Snapshot,
) -> (bool, u64, u64, bool) {
    let started = kumo_rt::clock_get();
    let mut waited_ns = 0u64;
    // Teardown can lag the last ProcessWait, so the resident set is itself a bounded wait.
    loop {
        if !ax4_query(root, 0, frame, out) {
            return (false, 0, waited_ns, false);
        }
        let now = kumo_rt::clock_get();
        waited_ns = now.saturating_sub(started);
        if ax4_census_equal(baseline, out) {
            break;
        }
        if waited_ns >= AX4_CLEAN_MAX_NS {
            return (false, 0, waited_ns, false);
        }
        let _ = kumo_rt::sleep_ns(AX4_CLEAN_SAMPLE_NS);
    }
    // The first census-equal sample begins the heap window; the final sample of a completed
    // window is the settled census.
    let mut previous_heap = out.heap_used;
    let mut stable_since = kumo_rt::clock_get();
    let mut change = false;
    loop {
        let _ = kumo_rt::sleep_ns(AX4_CLEAN_SAMPLE_NS);
        if !ax4_query(root, 0, frame, out) {
            return (false, 0, waited_ns, change);
        }
        let now = kumo_rt::clock_get();
        waited_ns = now.saturating_sub(started);
        let moved = if out.heap_used >= previous_heap {
            out.heap_used - previous_heap > AX4_CLEAN_EPSILON
        } else {
            previous_heap - out.heap_used > AX4_CLEAN_EPSILON
        };
        let census_held = ax4_census_equal(baseline, out);
        change = moved || !census_held;
        if change {
            stable_since = now;
        }
        previous_heap = out.heap_used;
        let stable_ns = now.saturating_sub(stable_since);
        if !change && stable_ns >= AX4_CLEAN_STABLE_NS {
            return (true, stable_ns, waited_ns, false);
        }
        if waited_ns >= AX4_CLEAN_MAX_NS {
            return (false, stable_ns, waited_ns, change);
        }
    }
}

#[derive(Clone, Copy)]
struct Ax4Child {
    process: Handle,
    control: Handle,
    wake: Handle,
    control_koid: u64,
}

/// Spawn one `ax4idle` child: two transferred channels (`x0` control, `x1` wake), no
/// ambient authority. `status` receives thread zero's raw ProcessRun result; an over-limit
/// Job leaves `BadState` (-11) there and returns `None`.
fn ax4_spawn(initrd: Handle, status: &mut i64) -> Option<Ax4Child> {
    let (control_raw, control_child_raw) = channel_create_pair();
    let (wake_raw, wake_child_raw) = channel_create_pair();
    if control_raw == u64::MAX
        || control_child_raw == u64::MAX
        || wake_raw == u64::MAX
        || wake_child_raw == u64::MAX
    {
        for raw in [control_raw, control_child_raw, wake_raw, wake_child_raw] {
            if raw != u64::MAX {
                let _ = handle_close(Handle(raw as u32));
            }
        }
        return None;
    }
    let control = Handle(control_raw as u32);
    let wake = Handle(wake_raw as u32);
    let mut process = None;
    *status = 0;
    let flags =
        (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG | ProcessRunFlags::TRANSFER_ARG2)
            .bits();
    let loaded = run_elf_threads_status(
        initrd,
        kumo_abi::initrd::AX4IDLE_PATH.as_bytes(),
        control_child_raw,
        wake_child_raw,
        flags,
        b"ax4idle",
        0,
        Some(&mut process),
        status,
        false,
    );
    let Some(process) = process else {
        // Admission never happened; both child ends are still Sora's to close.
        let _ = handle_close(Handle(control_child_raw as u32));
        let _ = handle_close(Handle(wake_child_raw as u32));
        let _ = handle_close(control);
        let _ = handle_close(wake);
        return None;
    };
    if !loaded {
        // Thread zero was admitted but a later step failed (only possible with peers).
        let _ = handle_close(process);
        let _ = handle_close(control);
        let _ = handle_close(wake);
        return None;
    }
    Some(Ax4Child {
        process,
        control,
        wake,
        control_koid: handle_koid(control),
    })
}

fn ax4_spawn_batch(initrd: Handle, count: u32, children: &mut alloc::vec::Vec<Ax4Child>) -> bool {
    let mut ok = true;
    for _ in 0..count {
        let mut status = 0i64;
        match ax4_spawn(initrd, &mut status) {
            Some(child) => children.push(child),
            None => {
                log(
                    alloc::format!("AX4 spawn-failed status={} at={}\n", status, children.len())
                        .as_bytes(),
                );
                ok = false;
                break;
            }
        }
    }
    ok
}

/// One AXP3 control frame: role `R` (ready) or `W` (first wake, carrying the clock read
/// immediately after the blocking wait), the wake count, and the child's SIMD errors.
#[derive(Clone, Copy, Default)]
struct Ax4Frame {
    role: u8,
    clock: u64,
    wakes: u64,
    simd_errors: u64,
}
fn ax4_frame_decode(bytes: &[u8]) -> Option<Ax4Frame> {
    if bytes.len() != 32 || &bytes[..4] != b"AXP3" {
        return None;
    }
    Some(Ax4Frame {
        role: bytes[4],
        clock: ax4_u64(bytes, 8),
        wakes: ax4_u64(bytes, 16),
        simd_errors: ax4_u64(bytes, 24),
    })
}

/// Read one frame of `want` role from each child with one Port and one 15 s timer for the
/// whole batch (one timer per child would allocate hundreds of kernel objects per phase).
/// `frames[index]` receives `children[index]`'s frame; returns how many children produced
/// a valid frame with no SIMD error. Children that close instead are left unresolved.
fn ax4_collect(children: &[Ax4Child], want: u8, frames: &mut [Ax4Frame]) -> u32 {
    if children.is_empty() || children.len() > frames.len() || children.len() > AX4_MAX_CAPACITY {
        return 0;
    }
    let Some(port) = reply_port() else {
        return 0;
    };
    let Some(timer) = syscall_handle(timer_create(AX4_WAKE_NS)) else {
        return 0;
    };
    let tk = handle_koid(timer);
    if port_bind(port, timer) != 0 {
        let _ = handle_close(timer);
        return 0;
    }
    let mut done = [false; AX4_MAX_CAPACITY];
    let mut remaining = 0u32;
    for (index, child) in children.iter().enumerate() {
        if port_bind(port, child.control) == 0 {
            remaining += 1;
        } else {
            done[index] = true;
        }
    }
    let mut got = 0u32;
    while remaining > 0 {
        let source = port_wait(port);
        if source == tk || source == u64::MAX {
            break;
        }
        if dispatch_service_event(source) {
            continue;
        }
        let Some(index) = children
            .iter()
            .position(|child| child.control_koid == source)
        else {
            continue;
        };
        if done[index] {
            continue;
        }
        let mut bytes = [0u8; 32];
        let n = channel_read(children[index].control, bytes.as_mut_ptr(), bytes.len()) as usize;
        let _ = port_unbind(port, children[index].control);
        done[index] = true;
        remaining -= 1;
        if n == 32 {
            match ax4_frame_decode(&bytes) {
                Some(frame) if frame.role == want && frame.simd_errors == 0 => {
                    frames[index] = frame;
                    got += 1;
                }
                Some(frame) => {
                    log(alloc::format!(
                        "AX4 child-invalid index={} role={} wakes={} simd-errors={}\n",
                        index,
                        frame.role,
                        frame.wakes,
                        frame.simd_errors
                    )
                    .as_bytes());
                }
                None => {
                    log(alloc::format!("AX4 child-invalid index={} frame=bad\n", index).as_bytes());
                }
            }
        } else if n != 0 {
            log(alloc::format!("AX4 child-invalid index={} read={}\n", index, n).as_bytes());
        }
    }
    for (index, child) in children.iter().enumerate() {
        if !done[index] {
            let _ = port_unbind(port, child.control);
        }
    }
    let _ = port_unbind(port, timer);
    let _ = handle_close(timer);
    got
}

/// Drive every child to exit with `Q` and wait, with one Port and one timer, for each
/// control channel to report PEER_CLOSED. `closed[index]` is set only for children that
/// really closed; the caller kills the rest. A stale frame is drained, then the binding is
/// replayed so a queued frame cannot hide a later close.
fn ax4_close_all(children: &[Ax4Child], closed: &mut [bool]) -> u32 {
    if children.len() > closed.len() || children.len() > AX4_MAX_CAPACITY {
        return 0;
    }
    for child in children {
        let _ = channel_write(child.control, b"Q".as_ptr(), 1);
    }
    // Let the woken children run and exit before the first blocking read.
    for _ in 0..3 {
        let _ = process_wait();
    }
    let Some(port) = reply_port() else {
        return 0;
    };
    let Some(timer) = syscall_handle(timer_create(AX4_WAKE_NS)) else {
        return 0;
    };
    let tk = handle_koid(timer);
    if port_bind(port, timer) != 0 {
        let _ = handle_close(timer);
        return 0;
    }
    let mut done = [false; AX4_MAX_CAPACITY];
    let mut remaining = 0u32;
    for (index, child) in children.iter().enumerate() {
        closed[index] = false;
        if port_bind(port, child.control) == 0 {
            remaining += 1;
        } else {
            done[index] = true;
        }
    }
    let mut count = 0u32;
    while remaining > 0 {
        let source = port_wait(port);
        if source == tk || source == u64::MAX {
            break;
        }
        if dispatch_service_event(source) {
            continue;
        }
        let Some(index) = children
            .iter()
            .position(|child| child.control_koid == source)
        else {
            continue;
        };
        if done[index] {
            continue;
        }
        let mut bytes = [0u8; 32];
        let n = channel_read(children[index].control, bytes.as_mut_ptr(), bytes.len()) as usize;
        if n == 0 {
            let _ = port_unbind(port, children[index].control);
            done[index] = true;
            remaining -= 1;
            closed[index] = true;
            count += 1;
        } else if n == 32 {
            // A queued frame is not close. Rebind so the channel's current state (more
            // data, or PEER_CLOSED after the queue drains) is replayed.
            let _ = port_unbind(port, children[index].control);
            if port_bind(port, children[index].control) != 0 {
                done[index] = true;
                remaining -= 1;
            }
        } else {
            let _ = port_unbind(port, children[index].control);
            done[index] = true;
            remaining -= 1;
        }
    }
    for (index, child) in children.iter().enumerate() {
        if !done[index] {
            let _ = port_unbind(port, child.control);
        }
    }
    let _ = port_unbind(port, timer);
    let _ = handle_close(timer);
    count
}

/// Release all children, kill any that did not close, and return the census to its
/// pre-spawn state. The resident login services keep `ProcessWait` at `ShouldWait`, so the
/// pumps only let killed children run and be reaped; they cannot reach `Ok`.
fn ax4_teardown(children: &mut alloc::vec::Vec<Ax4Child>) {
    let mut closed = [false; AX4_MAX_CAPACITY];
    if children.len() <= closed.len() {
        let _ = ax4_close_all(children, &mut closed);
    }
    for (index, child) in children.iter().enumerate() {
        if index >= closed.len() || !closed[index] {
            let _ = process_kill(child.process);
        }
        let _ = handle_close(child.process);
        let _ = handle_close(child.control);
        let _ = handle_close(child.wake);
    }
    children.clear();
    for _ in 0..4 {
        let _ = process_wait();
    }
}

#[allow(clippy::too_many_arguments)]
fn ax4_cycle(
    initrd: Handle,
    root: Handle,
    cycle: u32,
    frame: &mut [u8],
    frames: &mut [Ax4Frame],
    children: &mut alloc::vec::Vec<Ax4Child>,
    pre: &mut Ax4Snapshot,
    peak: &mut Ax4Snapshot,
    clean: &mut Ax4Snapshot,
    contaminated: &mut bool,
) -> bool {
    if !ax4_query(root, 0, frame, pre) {
        log(alloc::format!("AX4 cycle={} pre-snapshot-failed FAIL\n", cycle).as_bytes());
        return false;
    }
    let spawned = ax4_spawn_batch(initrd, AX4_BATCH, children);
    let ready = if spawned {
        ax4_collect(children, b'R', frames)
    } else {
        0
    };
    let peak_ok = ax4_query(root, 0, frame, peak);
    ax4_teardown(children);
    // Wait for both retirement and console delivery; the process census alone does not
    // account for burst storage retained by a still-live IPC inbox.
    let (settled, stable_ns, waited_ns, change) = ax4_clean_settle(root, frame, pre, clean);
    let clean_line = if settled {
        alloc::format!(
            "AX4 clean-settle cycle={} stable-ms={} waited-ms={} heap={} change={}\n",
            cycle,
            stable_ns / 1_000_000,
            waited_ns / 1_000_000,
            clean.heap_used,
            u8::from(change)
        )
    } else {
        alloc::format!(
            "AX4 clean-settle gave up cycle={} stable-ms={} waited-ms={} heap={} change={}\n",
            cycle,
            stable_ns / 1_000_000,
            waited_ns / 1_000_000,
            clean.heap_used,
            u8::from(change)
        )
    };
    log(clean_line.as_bytes());
    let clean_ok = settled;
    let peak_live_ok = peak_ok && peak.live == pre.live.saturating_add(AX4_BATCH);
    let state_equal = clean_ok && ax4_states_equal(pre, clean);
    let koids_equal = clean_ok && pre.processes == clean.processes;
    let frames_delta = if clean_ok {
        i128::from(clean.frames_issued) - i128::from(pre.frames_issued)
    } else {
        i128::MAX
    };
    let heap_delta = if clean_ok {
        i128::from(clean.heap_used) - i128::from(pre.heap_used)
    } else {
        0
    };
    // AX0's rule: the first cycle is the only one allowed to absorb boot bring-up work,
    // and only when the settle gate was green (it ran immediately before this).
    let contamination = cycle == 1 && frames_delta > 0;
    if contamination {
        *contaminated = true;
    }
    let frames_ok = frames_delta == 0 || contamination;
    let ok = spawned
        && ready == AX4_BATCH
        && peak_ok
        && peak_live_ok
        && clean_ok
        && state_equal
        && koids_equal
        && frames_ok
        && heap_delta <= AX4_HEAP_ALLOWANCE;
    log(alloc::format!(
        "AX4 cycle={} children={} peak-live={} peak-stored={} clean-live={} clean-stored={} frames-delta={} heap-delta={} state-equal={} koids-equal={} contaminated={} {}\n",
        cycle,
        AX4_BATCH,
        peak.live,
        peak.stored,
        clean.live,
        clean.stored,
        frames_delta,
        heap_delta,
        u8::from(state_equal),
        u8::from(koids_equal),
        u8::from(contamination),
        if ok { "PASS" } else { "FAIL" }
    )
    .as_bytes());
    if contamination {
        log(alloc::format!(
            "AX4 contamination cycle={} frames-delta={} settle=green source=first-cycle-allowance\n",
            cycle,
            frames_delta
        )
        .as_bytes());
    }
    ok
}

fn ax4_scheduler_series(root: Handle, frame: &mut [u8], s: &mut Ax4Snapshot) -> bool {
    let mut ok = true;
    for (index, n) in [16u32, 32, 64, 128, 256].into_iter().enumerate() {
        // AX0 owns ops 0..=8; AX4 extends the same private-Dispatcher series with one op
        // per size at 9..=13 (the kernel agent's actorstat work).
        let op = 9u8 + index as u8;
        if ax4_query(root, op, frame, s) {
            log(alloc::format!(
                "AX4 scheduler runnable={} decisions={} total-ns={}\n",
                n,
                s.roots,
                s.root_ns
            )
            .as_bytes());
            if s.roots != AX4_SCHED_DECISIONS || s.root_ns == 0 || s.root_ns == u64::MAX {
                ok = false;
            }
        } else {
            log(alloc::format!("AX4 scheduler runnable={} decisions=0 total-ns=0\n", n).as_bytes());
            log(alloc::format!("AX4 scheduler unavailable op={}\n", op).as_bytes());
            ok = false;
        }
    }
    ok
}

#[allow(clippy::too_many_arguments)]
fn ax4_wake_series(
    initrd: Handle,
    root: Handle,
    frame: &mut [u8],
    frames: &mut [Ax4Frame],
    children: &mut alloc::vec::Vec<Ax4Child>,
    latencies: &mut alloc::vec::Vec<u64>,
    base: &mut Ax4Snapshot,
    post: &mut Ax4Snapshot,
) -> bool {
    let mut ok = true;
    for n in [16u32, 32, 64, 128, 256] {
        children.clear();
        if !ax4_spawn_batch(initrd, n, children) {
            ok = false;
        }
        let ready = ax4_collect(children, b'R', frames);
        if ready != n {
            ok = false;
        }
        if children.is_empty() {
            log(alloc::format!(
                "AX4 wake n={} mode=single samples=0 min-ns=0 median-ns=0 max-ns=0 FAIL\n",
                n
            )
            .as_bytes());
            continue;
        }
        // Single target: one ClockGet immediately before the wake byte. The child reads
        // its own clock immediately after the blocking wait returns.
        let t0 = kumo_rt::clock_get();
        if channel_write(children[0].wake, b"W".as_ptr(), 1) != 0 {
            ok = false;
        }
        let single = ax4_collect(&children[..1], b'W', frames) == 1;
        let single_ns = if single {
            frames[0].clock.saturating_sub(t0)
        } else {
            u64::MAX
        };
        log(alloc::format!(
            "AX4 wake n={} mode=single samples=1 min-ns={} median-ns={} max-ns={} {}\n",
            n,
            single_ns,
            single_ns,
            single_ns,
            if single { "PASS" } else { "FAIL" }
        )
        .as_bytes());
        if !single {
            ok = false;
        }
        // Broadcast to every child still parked behind its first wake. The single target is
        // deliberately excluded: its protocol reports only the first wake.
        let t1 = kumo_rt::clock_get();
        for child in children.iter().skip(1) {
            let _ = channel_write(child.wake, b"W".as_ptr(), 1);
        }
        let got = if children.len() > 1 {
            ax4_collect(&children[1..], b'W', frames)
        } else {
            0
        } as usize;
        latencies.clear();
        for index in 0..got {
            latencies.push(frames[index].clock.saturating_sub(t1));
        }
        latencies.sort_unstable();
        let min_ns = latencies.first().copied().unwrap_or(0);
        let median_ns = latencies.get(latencies.len() / 2).copied().unwrap_or(0);
        let max_ns = latencies.last().copied().unwrap_or(0);
        let broadcast = got as u32 == n.saturating_sub(1) && got > 0;
        ax4_teardown(children);
        let clean = ax4_query(root, 0, frame, post)
            && post.live == base.live
            && post.stored == base.stored
            && post.processes == base.processes;
        log(alloc::format!(
            "AX4 wake n={} mode=broadcast targets={} samples={} min-ns={} median-ns={} max-ns={} {} clean={}\n",
            n,
            n.saturating_sub(1),
            got,
            min_ns,
            median_ns,
            max_ns,
            if broadcast { "PASS" } else { "FAIL" },
            u8::from(clean)
        )
        .as_bytes());
        if !broadcast || !clean {
            ok = false;
        }
    }
    ok
}

// A ready frame is sent before its actor parks, and proof logging wakes drv-fb. Capture
// both sides of refusal after those actors park, so the comparison measures admission rather
// than the benchmark's own console traffic. Failure to idle remains a bounded, visible error.
// — TIMBERDOODLE
fn ax4_idle_snapshot(root: Handle, frame: &mut [u8], out: &mut Ax4Snapshot) -> bool {
    let started = kumo_rt::clock_get();
    loop {
        if !ax4_query(root, 0, frame, out) {
            return false;
        }
        if out.live == out.stored && out.ready == 0 && out.running == 0 {
            return true;
        }
        if kumo_rt::clock_get().saturating_sub(started) >= AX4_CLEAN_MAX_NS {
            log(b"AX4 idle census timed out\n");
            return false;
        }
        let _ = kumo_rt::sleep_ns(AX4_CLEAN_SAMPLE_NS);
    }
}

#[allow(clippy::too_many_arguments)]
fn ax4_boundary(
    initrd: Handle,
    root: Handle,
    frame: &mut [u8],
    frames: &mut [Ax4Frame],
    children: &mut alloc::vec::Vec<Ax4Child>,
    base: &mut Ax4Snapshot,
    before: &mut Ax4Snapshot,
    full: &mut Ax4Snapshot,
    after: &mut Ax4Snapshot,
    post: &mut Ax4Snapshot,
) -> bool {
    let mut ok = true;
    if !ax4_query(root, 0, frame, before) {
        log(b"AX4 boundary FAIL pre-snapshot\n");
        return false;
    }
    let fill = before.capacity.saturating_sub(before.live);
    let spawned = ax4_spawn_batch(initrd, fill, children);
    log(alloc::format!(
        "AX4 boundary spawn-done requested={} spawned={} {}\n",
        fill,
        children.len(),
        if spawned { "Ok" } else { "Err" }
    )
    .as_bytes());
    if !spawned {
        ok = false;
    }
    let ready = ax4_collect(children, b'R', frames);
    log(alloc::format!("AX4 boundary ready={} expected={}\n", ready, fill).as_bytes());
    let full_ok =
        ax4_idle_snapshot(root, frame, full) && full.live == full.capacity && ready == fill;
    log(alloc::format!(
        "AX4 boundary census capacity={} live={} stored={}\n",
        full.capacity,
        full.live,
        full.stored
    )
    .as_bytes());
    log(alloc::format!(
        "AX4 boundary fill capacity={} live={} stored={} spawns={} {}\n",
        full.capacity,
        full.live,
        full.stored,
        fill,
        if full_ok { "PASS" } else { "FAIL" }
    )
    .as_bytes());
    if !full_ok {
        ok = false;
    }
    // One over-limit admission. BadState (-11) is the documented refusal; a panic or a
    // TOWER would take the boot with it, so reaching this line is itself part of the gate.
    log(b"AX4 boundary over-limit attempt\n");
    let mut status = 0i64;
    if let Some(extra) = ax4_spawn(initrd, &mut status) {
        children.push(extra);
    }
    let snap_ok = ax4_idle_snapshot(root, frame, after);
    let state_equal = snap_ok && ax4_states_equal(full, after);
    let koids_equal = snap_ok && full.processes == after.processes;
    let refused = status == i64::from(Errno::BadState.status());
    let refusal_ok = refused && state_equal && koids_equal;
    log(alloc::format!(
        "AX4 refusal status={} capacity={} live={} stored={} state-equal={} koids-equal={} {}\n",
        status,
        after.capacity,
        after.live,
        after.stored,
        u8::from(state_equal),
        u8::from(koids_equal),
        if refusal_ok { "PASS" } else { "FAIL" }
    )
    .as_bytes());
    if !refusal_ok {
        ok = false;
    }
    ax4_teardown(children);
    // A later spawn must succeed once the capacity is returned.
    let mut recover_status = 0i64;
    let recovered = ax4_spawn(initrd, &mut recover_status);
    if let Some(child) = recovered {
        children.push(child);
    }
    let ready = if children.len() == 1 {
        ax4_collect(children, b'R', frames)
    } else {
        0
    };
    let live = if ax4_query(root, 0, frame, post) {
        post.live
    } else {
        0
    };
    let recover_ok = children.len() == 1 && ready == 1 && live == before.live.saturating_add(1);
    log(alloc::format!(
        "AX4 boundary recover spawn={} live={} {}\n",
        if children.len() == 1 { "Ok" } else { "Err" },
        live,
        if recover_ok { "PASS" } else { "FAIL" }
    )
    .as_bytes());
    if !recover_ok {
        ok = false;
    }
    ax4_teardown(children);
    // The benchmark ends where it started, kernel heap included: the fill/recovery children
    // used the same retirement path as a cycle, so any heap retained here is the same leak
    // the cycle gate rejects. 64 KiB tolerates one-time bookkeeping only. Use the same
    // heap-stable settle as a cycle so the final gate includes completed retirement and
    // console delivery, rather than merely matching process counts.
    let (settled, stable_ns, waited_ns, change) = ax4_clean_settle(root, frame, base, post);
    let settle_line = if settled {
        alloc::format!(
            "AX4 clean-settle final stable-ms={} waited-ms={} heap={} change={}\n",
            stable_ns / 1_000_000,
            waited_ns / 1_000_000,
            post.heap_used,
            u8::from(change)
        )
    } else {
        alloc::format!(
            "AX4 clean-settle gave up final stable-ms={} waited-ms={} heap={} change={}\n",
            stable_ns / 1_000_000,
            waited_ns / 1_000_000,
            post.heap_used,
            u8::from(change)
        )
    };
    log(settle_line.as_bytes());
    let heap_flat = i128::from(post.heap_used) - i128::from(base.heap_used) <= AX4_HEAP_ALLOWANCE;
    let clean_ok = settled
        && post.live == base.live
        && post.stored == base.stored
        && ax4_states_equal(base, post)
        && post.processes == base.processes
        && heap_flat;
    ax4_print("final", post);
    if !clean_ok {
        ok = false;
    }
    ok
}

fn ax4_benchmark(initrd: Handle, root: Handle, boundary_runs: u32) {
    if BG_STDOUT.load(Ordering::Relaxed) != 0 || NET_SUPERVISOR.get().initrd.0 != 0 {
        log(b"AX4 FAIL needs a fresh boot and no background job\n");
        return;
    }
    log(b"AX4 begins: resident actor capacity (PLAN/021 AX4)\n");
    let mut ok = true;
    // Exercise the Job policy ABI before measuring. 0 and >288 are InvalidArgs, and the
    // ABI bound itself is accepted (idempotent for the kernel's default root policy).
    let zero = kumo_rt::job_set_policy(Handle(ROOT_JOB_HANDLE.load(Ordering::Relaxed) as u32), 0);
    log(alloc::format!(
        "AX4 policy limit=0 status={} {}\n",
        zero,
        if zero == i64::from(Errno::InvalidArgs.status()) {
            "PASS"
        } else {
            "FAIL"
        }
    )
    .as_bytes());
    let too_big = kumo_rt::job_set_policy(
        Handle(ROOT_JOB_HANDLE.load(Ordering::Relaxed) as u32),
        AX4_MAX_CAPACITY as u64 + 1,
    );
    log(alloc::format!(
        "AX4 policy limit={} status={} {}\n",
        AX4_MAX_CAPACITY + 1,
        too_big,
        if too_big == i64::from(Errno::InvalidArgs.status()) {
            "PASS"
        } else {
            "FAIL"
        }
    )
    .as_bytes());
    let bound = kumo_rt::job_set_policy(
        Handle(ROOT_JOB_HANDLE.load(Ordering::Relaxed) as u32),
        AX4_MAX_CAPACITY as u64,
    );
    log(alloc::format!(
        "AX4 policy limit={} status={} {}\n",
        AX4_MAX_CAPACITY,
        bound,
        if bound == 0 { "PASS" } else { "FAIL" }
    )
    .as_bytes());
    ok &= zero == i64::from(Errno::InvalidArgs.status())
        && too_big == i64::from(Errno::InvalidArgs.status())
        && bound == 0;
    // Every buffer and Vec is allocated before the first measured window: Sora-heap growth
    // inside a cycle would issue frames and contaminate the flat-census measurement.
    let mut frame = [0u8; AX4_FRAME_MAX];
    let mut frames = alloc::vec![Ax4Frame::default(); AX4_MAX_CAPACITY];
    let mut children: alloc::vec::Vec<Ax4Child> = alloc::vec::Vec::with_capacity(AX4_MAX_CAPACITY);
    let mut latencies: alloc::vec::Vec<u64> = alloc::vec::Vec::with_capacity(AX4_MAX_CAPACITY);
    let mut base = Ax4Snapshot::empty();
    let mut pre = Ax4Snapshot::empty();
    let mut peak = Ax4Snapshot::empty();
    let mut clean = Ax4Snapshot::empty();
    let mut before = Ax4Snapshot::empty();
    let mut full = Ax4Snapshot::empty();
    let mut after = Ax4Snapshot::empty();
    let mut post = Ax4Snapshot::empty();
    let mut contaminated = false;
    if !ax4_settle(root, &mut frame, &mut pre, &mut base) {
        log(b"AX4 FAIL settle-quiesce\n");
        return;
    }
    // The settle's last quiescence sample is the default-login census, so the heap anchor
    // is taken only after the resident set has held still for the whole window.
    ax4_print("default-login", &base);
    if base.capacity < AX4_BATCH || base.live.saturating_add(AX4_BATCH) > base.capacity {
        log(alloc::format!(
            "AX4 FAIL batch does not fit live={} capacity={} batch={}\n",
            base.live,
            base.capacity,
            AX4_BATCH
        )
        .as_bytes());
        return;
    }
    if boundary_runs > 0 {
        for run in 1..=boundary_runs {
            log(alloc::format!("AX4 boundary-only diagnostic run={}\n", run).as_bytes());
            if !ax4_boundary(
                initrd,
                root,
                &mut frame,
                &mut frames,
                &mut children,
                &mut base,
                &mut before,
                &mut full,
                &mut after,
                &mut post,
            ) {
                ok = false;
            }
        }
        log(if ok { b"AX4 PASS\n" } else { b"AX4 FAIL\n" });
        return;
    }
    for cycle in 1..=AX4_CYCLES {
        if !ax4_cycle(
            initrd,
            root,
            cycle,
            &mut frame,
            &mut frames,
            &mut children,
            &mut pre,
            &mut peak,
            &mut clean,
            &mut contaminated,
        ) {
            ok = false;
        }
    }
    if !ax4_scheduler_series(root, &mut frame, &mut clean) {
        ok = false;
    }
    if !ax4_wake_series(
        initrd,
        root,
        &mut frame,
        &mut frames,
        &mut children,
        &mut latencies,
        &mut base,
        &mut post,
    ) {
        ok = false;
    }
    if !ax4_boundary(
        initrd,
        root,
        &mut frame,
        &mut frames,
        &mut children,
        &mut base,
        &mut before,
        &mut full,
        &mut after,
        &mut post,
    ) {
        ok = false;
    }
    log(if ok { b"AX4 PASS\n" } else { b"AX4 FAIL\n" });
}

/// Post-AX4 capacity gate (J819/J822): reproduce the AX0 sequence — default-login census,
/// then netstack plus one `netprobe hold` client — and require two free resident slots.
fn ax4_gate(initrd: Handle, root: Handle) {
    if BG_STDOUT.load(Ordering::Relaxed) != 0 || NET_SUPERVISOR.get().initrd.0 != 0 {
        log(b"AX4 gate FAIL needs a fresh boot and no background job\n");
        return;
    }
    log(b"AX4 gate begins: post-AX4 capacity gate (J819)\n");
    let mut frame = [0u8; AX4_FRAME_MAX];
    let mut a = Ax4Snapshot::empty();
    let mut b = Ax4Snapshot::empty();
    if !ax4_settle(root, &mut frame, &mut a, &mut b) {
        log(b"AX4 gate FAIL settle-quiesce\n");
        return;
    }
    // The settle ends on a quiescence sample; it is the gate's default-login anchor.
    ax4_print("default-login", &b);
    let base_live = b.live;
    let mut ok = true;
    let args = [
        alloc::string::String::from("netprobe"),
        alloc::string::String::from("hold"),
    ];
    run_program_background(initrd, &args, true);
    if ax4_query(root, 0, &mut frame, &mut b) {
        ax4_print("netstack-client", &b);
        let free = b.capacity.saturating_sub(b.live);
        ok &= b.live == base_live.saturating_add(2);
        log(alloc::format!(
            "AX4 capacity-gate free={} requires=2 {}\n",
            free,
            if free >= 2 { "PASS" } else { "AX4-REQUIRED" }
        )
        .as_bytes());
        ok &= free >= 2;
    } else {
        ok = false;
    }
    if BG_STDOUT.load(Ordering::Relaxed) != 0 {
        wait_background();
    }
    if ax4_query(root, 0, &mut frame, &mut b) {
        ax4_print("gate-clean", &b);
        // The supervised netstack remains available after its hold client exits, just as
        // in AX0. Verify that service, rather than expecting both actors to disappear.
        // — TIMBERDOODLE
        let netstack = NET_SUPERVISOR.get().instance;
        ok &= b.live == base_live.saturating_add(1)
            && netstack.is_some_and(|instance| b.processes.contains(&instance.koid));
    } else {
        ok = false;
    }
    log(if ok {
        b"AX4 gate PASS\n"
    } else {
        b"AX4 gate FAIL\n"
    });
}

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
            shell_output(HOSTNAME.get().as_str().as_bytes());
            shell_output(b" login: ");
        }
        sora::LoginStage::Password => shell_output(b"Password: "),
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

/// One serialized identity exchange. ProcessWait is not reply readiness: an unrelated
/// resident can return control while argon2 is still running. Bind before sending and
/// wait for this channel, so a successful check cannot authorize the next attempt.
/// — TIMBERDOODLE
fn identity_request_reply(request: IdentityRequest<'_>, reply: &mut [u8]) -> Option<usize> {
    let client = Handle(IDENTITYD.load(Ordering::Relaxed));
    if client.0 == 0 {
        return None;
    }
    let mut frame = [0u8; IDENTITY_REQUEST_MAX];
    let len = request.encode_into(&mut frame)?;
    let Some(port) = reply_port() else {
        frame.fill(0);
        return None;
    };
    if port_bind(port, client) != 0 {
        frame.fill(0);
        return None;
    }
    let written = channel_write(client, frame.as_ptr(), len) == 0;
    frame.fill(0);
    let result = if written && port_wait(port) == handle_koid(client) {
        let n = channel_read(client, reply.as_mut_ptr(), reply.len()) as usize;
        (n > 0 && n <= reply.len()).then_some(n)
    } else {
        None
    };
    let _ = port_unbind(port, client);
    if written && result.is_none() {
        // A request may still finish after a transport failure. Never reuse its
        // uncorrelated reply for a later login; fail closed until service restart.
        IDENTITYD.store(0, Ordering::Relaxed);
        close_owned(client);
    }
    result
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
                    shell_write(chunk.as_ptr(), chunk.len())
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

/// Log `parts` as one line, then fail with `status` when it is not 0.
fn log_line(parts: &[&[u8]], status: u8) {
    for part in parts {
        log(part);
    }
    if status != 0 {
        fail_status(status);
    }
}

/// `mount [/dev/diskNs1 [/media/NAME]]` (J766). FAT32 is KUMO's only filesystem, so there is no
/// `-t`. With no operand it lists every offered volume: the mounted one as Linux prints it, the
/// others as waiting. A device mounts at the given `/media/<name>`, or without one at the name it
/// takes by itself: its label, else its serial. Failures exit 32, as Linux's `mount` does.
fn mount_command(cmd: &kumoza::Command, cwd: &WorkingDirectory) {
    let mounted = media();
    let Some(operand) = cmd.args.first() else {
        for (index, device) in DEVICES.get().iter().enumerate() {
            let Some(device) = device else { continue };
            let path = sora::disk_path(index);
            match mounted.filter(|media| media.device == index) {
                Some(media) => log_line(
                    &[
                        &path,
                        b" on /media/",
                        media.name.as_bytes(),
                        b" type fat32 (",
                        if device.ready.read_only { b"ro" } else { b"rw" },
                        b")\n",
                    ],
                    0,
                ),
                None => log_line(&[&path, b" type fat32 (not mounted)\n"], 0),
            }
        }
        return;
    };
    if cmd.args.len() > 2 {
        usage(b"usage: mount [/dev/diskNs1 [/media/NAME]]\n");
        return;
    }
    let operand = operand.as_bytes();
    let Some(index) = sora::parse_disk(operand) else {
        log_line(
            &[
                b"mount: ",
                operand,
                b": not a device; KUMO names them /dev/diskNs1\n",
            ],
            32,
        );
        return;
    };
    let Some(device) = DEVICES.get()[index] else {
        log_line(&[b"mount: ", operand, b": no such device\n"], 32);
        return;
    };
    let name = match cmd.args.get(1) {
        None => sora::MediaName::new(device.ready.label, device.ready.serial),
        Some(point) => {
            let resolved = match cwd.resolve(point) {
                Ok(resolved) => resolved,
                Err(error) => return log_directory_error(b"mount", error),
            };
            let Some(name) = sora::mount_point(resolved.as_bytes()) else {
                log_line(
                    &[
                        b"mount: ",
                        resolved.as_bytes(),
                        b": the mount point must be /media/NAME, NAME being letters, digits, - _ or .\n",
                    ],
                    32,
                );
                return;
            };
            name
        }
    };
    match mounted {
        Some(media) if media.device == index => log_line(
            &[
                b"mount: ",
                operand,
                b": already mounted on /media/",
                media.name.as_bytes(),
                b"\n",
            ],
            32,
        ),
        Some(media) => log_line(
            &[
                b"mount: /media/",
                media.name.as_bytes(),
                b" is mounted; KUMO mounts one volume at a time, so umount it first\n",
            ],
            32,
        ),
        None => mount_device(index, device, name),
    }
}

/// `umount /media/NAME` or `umount /dev/diskNs1` (J766). The volume leaves `/media`; its device
/// stays offered, so `mount` can bring it back. The shell's working directory inside it makes
/// the mount busy, as Linux reports a busy target.
fn umount_command(cmd: &kumoza::Command, cwd: &WorkingDirectory) {
    let [operand] = cmd.args.as_slice() else {
        usage(b"usage: umount /media/NAME | /dev/diskNs1\n");
        return;
    };
    let Some(media) = media() else {
        log_line(&[b"umount: ", operand.as_bytes(), b": not mounted\n"], 32);
        return;
    };
    let target = match sora::parse_disk(operand.as_bytes()) {
        Some(index) => index == media.device,
        None => match cwd.resolve(operand) {
            Ok(resolved) => sora::mount_point(resolved.as_bytes())
                .is_some_and(|name| name.as_bytes() == media.name.as_bytes()),
            Err(error) => return log_directory_error(b"umount", error),
        },
    };
    if !target {
        log_line(&[b"umount: ", operand.as_bytes(), b": not mounted\n"], 32);
        return;
    }
    let busy = matches!(sora::media_route(cwd.path().as_bytes()),
        sora::MediaRoute::Volume { name, .. } if name == media.name.as_bytes());
    if busy {
        log_line(
            &[
                b"umount: /media/",
                media.name.as_bytes(),
                b": target is busy\n",
            ],
            32,
        );
        return;
    }
    set_media(None);
    notice(&[
        b"media: ",
        &sora::disk_path(media.device),
        b" unmounted from /media/",
        media.name.as_bytes(),
        b"\n",
    ]);
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
        let n = read_channel_ready(fs, &mut reply).ok_or(DirectoryError::Unavailable)?;
        if n == 0 {
            return Err(DirectoryError::Unavailable);
        }
        match kumo_vfs::parse_dir_reply(&reply[..n]) {
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
    grant_net: bool,
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
    // J815: a network session exists only because this launcher granted it (DESIGN/026 §4).
    let net_child = if grant_net {
        netstack_session(initrd)
    } else {
        None
    };
    let argv_bad = argv.is_some() && (argv_child_raw == 0 || argv_child_raw == u64::MAX);
    let initrd_bad = grant_initrd && (initrd_child_raw == 0 || initrd_child_raw == u64::MAX);
    let fs_bad = fs != FsGrant::None && fs_child.is_none();
    let tty_bad = grant_tty && tty_child.is_none();
    let net_bad = grant_net && net_child.is_none();
    if argv_bad || initrd_bad || fs_bad || tty_bad || net_bad {
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
        if let Some(net) = net_child {
            let _ = handle_close(net);
        }
        if let Some(writer) = stdin_writer {
            let _ = handle_close(writer);
        }
        log(name);
        log(b": startup grant unavailable\n");
        return None;
    }

    // Assemble the tagged set: [stdin], stdout, [argv], [cap0=initrd], [fs], [tty], [net]. Order
    // is immaterial — the child installs by tag — but kept deterministic for serial diagnosis.
    let mut tags: [(u8, Handle); 7] = [(0, Handle(0)); 7];
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
    if let Some(net) = net_child {
        tags[expected] = (STARTUP_TAG_NET, net);
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
        let Some(n) = read_channel_ready(read_end, &mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        if !sora::relay_console_output(&buf[..n], |chunk| shell_write(chunk.as_ptr(), chunk.len()))
        {
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
    shell_write(buf.as_ptr(), n as usize);
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
        false,
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
        shell_write(chunk.as_ptr(), chunk.len())
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
        false,
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

/// Pack `slots` into a per-launch snapshot, then retain only read+transfer authority. A
/// background child's first read may follow another launch; a shared writable scratch VMO
/// would silently replace its arguments (observed in the restart proof). — TIMBERDOODLE
/// Returns `0` if the VMO is unavailable or argv overflows the 256-byte buffer. The child `vmo_read`s the
/// handle and walks it with `kumo_abi::unpack_argv`.
fn argv_transfer_handle(slots: &[&[u8]]) -> u64 {
    let Some(vmo) = write_argv(slots) else {
        return 0;
    };
    let grant = handle_duplicate(vmo, Rights::READ | Rights::TRANSFER);
    let _ = handle_close(vmo);
    grant
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
    let mut buf = [0u8; 256];
    let written = kumo_abi::pack_argv(argv, &mut buf)?;
    let vmo = syscall_handle(vmo_create(256))?;
    if vmo_write(vmo, 0, buf.as_ptr(), written) != 0 {
        let _ = handle_close(vmo);
        return None;
    }
    Some(vmo)
}

/// Run an initrd program: spawn `bin/<argv[0]>` to completion via the same finite bootstrap
/// startup message every program now receives. The child gets stdout + a one-shot read-only argv
/// VMO; it is **not** granted the initrd, so a plain `run` stays authority-free (the `cat`/`wc`/
/// `ls` builtins are the capability-granting launchers). The `bin/` prefix is built into a fixed
/// buffer (no heap). Returns whether the spawn was attempted (false also logs the reason).
fn run_program(initrd: Handle, argv: &[alloc::string::String]) -> bool {
    run_program_granted(initrd, argv, false)
}

/// [`run_program`], with a netstack session when `grant_net` (the `net` launcher).
/// `bin/<name>` in `path`, or `None` when the name does not fit.
fn program_path<'a>(name: &[u8], path: &'a mut [u8; 64]) -> Option<&'a [u8]> {
    const PREFIX: &[u8] = b"bin/";
    if PREFIX.len() + name.len() > path.len() {
        debug_write(b"run: name too long\n".as_ptr(), 19);
        return None;
    }
    path[..PREFIX.len()].copy_from_slice(PREFIX);
    path[PREFIX.len()..PREFIX.len() + name.len()].copy_from_slice(name);
    Some(&path[..PREFIX.len() + name.len()])
}

fn run_program_granted(initrd: Handle, argv: &[alloc::string::String], grant_net: bool) -> bool {
    let name = match argv.first() {
        Some(n) => n.as_bytes(),
        None => return false,
    };
    let mut path = [0u8; 64];
    let Some(full) = program_path(name, &mut path) else {
        return false;
    };
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
            false,
        );
    } else {
        run_with_startup_input(
            initrd,
            full,
            full,
            Some(&slots[..count]),
            false,
            FsGrant::None,
            false,
            false,
            StdinMode::None,
            grant_net,
        );
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
fn run_elf_threads_status(
    initrd: Handle,
    path: &[u8],
    arg: u64,
    arg2: u64,
    flags: u64,
    name: &[u8],
    extra_threads: u32,
    kept: Option<&mut Option<Handle>>,
    status: &mut i64,
    log_spawn: bool,
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
    let mut stack_request = 0u64;

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
        if ph.kind == linux_elf::PT_GNU_STACK {
            stack_request = ph.mem_size;
        }
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
    //j741
    // Thread zero's stack is what the image's PT_GNU_STACK p_memsz asks for (the field
    // `-z stack-size=` sets), never less than the 16 KiB every program has had, and refused
    // rather than clamped above MAX_MAIN_STACK. Mesa's GLSL compiler outran 16 KiB on its first KUMO run (G7n). An image
    // that asks for nothing keeps exactly the old layout. — TIMBERDOODLE
    const MAX_MAIN_STACK: u64 = 8 * 1024 * 1024;
    let main_stack = match align_up(stack_request) {
        Some(size) if size <= MAX_MAIN_STACK => size.max(0x4000),
        _ => {
            log(name);
            log(b": stack request too large\n");
            return false;
        }
    };
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
        // The capacity probe reports aggregate census/results. Per-actor diagnostics
        // otherwise queue thousands of small framebuffer updates and measure console
        // backlog instead of actor retirement. Ordinary launches retain their diagnostics.
        // — TIMBERDOODLE
        if log_spawn {
            log_process_koid(name, child_as);
        }

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
        let arena_top = STACK_TOP + u64::from(extra_threads) * STACK_STRIDE;
        let arena_size = arena_top - (STACK_TOP - main_stack);
        if address_space_create(child_as, arena_top, arena_size) == u64::MAX {
            log(name);
            log(b": as fail\n");
            break 'load false;
        }

        let sp = |i: u64| STACK_TOP + i * STACK_STRIDE - 0x10;
        // AX4: a refusal is a status, not a panic. The caller may want the raw widened
        // `BadState` (-11) an over-limit Job returns. — TIMBERDOODLE
        let run_status = process_run(child_as, elf.entry, sp(0), arg, arg2, flags);
        // Kernel statuses arrive zero-extended in x0; Errno values are i32, so the low
        // word must be reinterpreted as signed (`BadState` -> -11, not 4294967285).
        *status = i64::from(run_status as u32 as i32);
        if run_status != 0 {
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
    } else if let Some(process) = loader_handles[1] {
        // A refused ProcessRun created an address space but no resident thread. Closing
        // its handle alone does not retire that record or its mappings. The creator's
        // MANAGE capability permits the normal teardown even with zero threads; preserve
        // the original load/admission status while surrendering every partial allocation.
        // A partially admitted child may already have exited, in which case kill is inert.
        // — TIMBERDOODLE
        let _ = process_kill(process);
    }
    let cleanup_ok = sora::close_handles(&loader_handles, handle_close);
    if !cleanup_ok {
        log(name);
        log(b": handle cleanup fail\n");
    }
    loaded && cleanup_ok
}

/// [`run_elf_threads`] with thread zero's raw `ProcessRun` result (PLAN/021 AX4).
///
/// A refused admission returns `false` and leaves the widened Errno in `status`
/// (`BadState` = -11 for an over-limit Job). Failures before `ProcessRun` (a missing
/// file, a bad header, an allocation failure) leave the caller's initial value intact.
/// Existing callers keep using [`run_elf_threads`] unchanged.
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
    let mut status = 0i64;
    run_elf_threads_status(
        initrd,
        path,
        arg,
        arg2,
        flags,
        name,
        extra_threads,
        kept,
        &mut status,
        true,
    )
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

/// Close a handle only while it is still this process's. A transferred handle's koid is
/// `u64::MAX`; closing it would target an unrelated handle instead. — TIMBERDOODLE
fn close_owned(handle: Handle) {
    if handle_koid(handle) != u64::MAX {
        let _ = handle_close(handle);
    }
}

/// Stop a smoke child without assuming it is still alive. — TIMBERDOODLE
fn stop_smoke_child(process: Option<Handle>) {
    if let Some(process) = process {
        let _ = process_kill(process);
        let _ = process_wait();
        let _ = handle_close(process);
    }
}

/// G8d-a's contained-close integration proof, QEMU-only and off the boot path by default.
///
/// The fake client speaks the real `kumo-gpu` wire over a real channel and arms two buffers,
/// two bindings and one fence wait; the fake server owns that session with a scripted backend.
/// Sora waits for the server's ready signal, kills the client, and then requires one report
/// carrying the exact records of both paths: a confirmed reset releases 2/2/1, and a refused
/// reset releases nothing and retains its two claims. The fake server scripts the reset
/// result — QEMU cannot power the GPU — so this proves ordering and accounting, not metal.
/// — TIMBERDOODLE
fn run_gpu_session_kill_smoke(initrd: Handle) -> bool {
    let (request_client_raw, request_server_raw) = channel_create_pair();
    let (Some(request_client), Some(request_server)) = (
        syscall_handle(request_client_raw),
        syscall_handle(request_server_raw),
    ) else {
        log(b"gpu-contain: request pair fail\n");
        return false;
    };
    let (report_read_raw, report_write_raw) = channel_create_pair();
    let (Some(report_read), Some(report_write)) = (
        syscall_handle(report_read_raw),
        syscall_handle(report_write_raw),
    ) else {
        close_owned(request_client);
        close_owned(request_server);
        log(b"gpu-contain: report pair fail\n");
        return false;
    };

    let server_flags =
        (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG | ProcessRunFlags::TRANSFER_ARG2)
            .bits();
    let mut server_proc = None;
    if !run_elf_threads(
        initrd,
        GPU_CONTAIN_SERVER_PATH.as_bytes(),
        request_server.0 as u64,
        report_write.0 as u64,
        server_flags,
        b"gpu-contain-server",
        0,
        Some(&mut server_proc),
    ) {
        close_owned(request_client);
        close_owned(request_server);
        close_owned(report_read);
        close_owned(report_write);
        log(b"gpu-contain: server launch fail\n");
        return false;
    }
    // Both arguments must be *moved*: Sora may keep no reference that would survive the
    // client's kill and keep the server from seeing a peer close. — TIMBERDOODLE
    if handle_koid(request_server) != u64::MAX || handle_koid(report_write) != u64::MAX {
        close_owned(request_client);
        close_owned(report_read);
        stop_smoke_child(server_proc);
        log(b"gpu-contain: server transfer fail\n");
        return false;
    }

    let client_flags = (ProcessRunFlags::ASYNC | ProcessRunFlags::TRANSFER_ARG).bits();
    let mut client_proc = None;
    if !run_elf_threads(
        initrd,
        GPU_CONTAIN_CLIENT_PATH.as_bytes(),
        request_client.0 as u64,
        0,
        client_flags,
        b"gpu-contain-client",
        0,
        Some(&mut client_proc),
    ) {
        close_owned(request_client);
        close_owned(report_read);
        stop_smoke_child(server_proc);
        log(b"gpu-contain: client launch fail\n");
        return false;
    }
    if handle_koid(request_client) != u64::MAX {
        close_owned(report_read);
        stop_smoke_child(client_proc);
        stop_smoke_child(server_proc);
        log(b"gpu-contain: client transfer fail\n");
        return false;
    }

    let (Some(server), Some(client)) = (server_proc, client_proc) else {
        close_owned(report_read);
        stop_smoke_child(server_proc);
        stop_smoke_child(client_proc);
        log(b"gpu-contain: process handle missing\n");
        return false;
    };

    let Some(port) = syscall_handle(port_create()) else {
        close_owned(report_read);
        stop_smoke_child(Some(client));
        stop_smoke_child(Some(server));
        log(b"gpu-contain: port fail\n");
        return false;
    };
    let Some(timer) = syscall_handle(timer_create(GPU_CONTAIN_WAIT_NS)) else {
        close_owned(port);
        close_owned(report_read);
        stop_smoke_child(Some(client));
        stop_smoke_child(Some(server));
        log(b"gpu-contain: timer fail\n");
        return false;
    };
    let timer_koid = handle_koid(timer);
    let report_koid = handle_koid(report_read);
    if timer_koid == u64::MAX
        || report_koid == u64::MAX
        || port_bind(port, timer) != 0
        || port_bind(port, report_read) != 0
    {
        close_owned(timer);
        close_owned(port);
        close_owned(report_read);
        stop_smoke_child(Some(client));
        stop_smoke_child(Some(server));
        log(b"gpu-contain: port bind fail\n");
        return false;
    }

    let mut report = [0u8; 512];
    let ready = port_wait(port) == report_koid && {
        let len = channel_read(report_read, report.as_mut_ptr(), report.len()) as usize;
        report.get(..len) == Some(b"ready\n".as_slice())
    };
    if !ready {
        stop_smoke_child(Some(client));
        stop_smoke_child(Some(server));
        let _ = port_unbind(port, report_read);
        let _ = port_unbind(port, timer);
        close_owned(timer);
        close_owned(port);
        close_owned(report_read);
        log(b"gpu-contain: client never armed\n");
        return false;
    }

    // The client is parked with the session armed. Killing it closes the only remaining
    // endpoint; the server acts only on that peer close. — TIMBERDOODLE
    let _ = process_kill(client);
    let _ = handle_close(client);
    let mut report_len = 0usize;
    if port_wait(port) == report_koid {
        report_len = channel_read(report_read, report.as_mut_ptr(), report.len()) as usize;
    }
    if report_len > 0 {
        log(&report[..report_len]);
    }
    const CONFIRMED: &str =
        "drv-gpu: containment reset=ok buffers=2 bindings=2 waits=1 retained=no";
    const REFUSED: &str =
        "drv-gpu: containment reset=fail buffers=0 bindings=0 waits=0 retained=yes";
    let text = core::str::from_utf8(&report[..report_len]).unwrap_or("");
    let passed =
        text.contains(CONFIRMED) && text.contains(REFUSED) && text.contains("gpu-contain: pass");
    // Give the server its exit before dropping the process handle; the report is in hand.
    for _ in 0..256 {
        if all_residents_gone(process_wait()) {
            break;
        }
    }
    let _ = handle_close(server);
    let _ = port_unbind(port, report_read);
    let _ = port_unbind(port, timer);
    close_owned(timer);
    close_owned(port);
    close_owned(report_read);
    passed
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
    kumo_rt::boot_ok(&[b"ttyd: online, console line discipline\n"]);
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
        shell_write(parsed.echo.as_ptr(), parsed.echo.len());
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

// Sora serializes synchronous replies and output reads. Reuse one port: the engine does not
// yet reap closed ports, so a port per read would grow the registry. — TIMBERDOODLE
static REPLY_PORT: AtomicU32 = AtomicU32::new(0);

fn reply_port() -> Option<Handle> {
    let mut port = Handle(REPLY_PORT.load(Ordering::Relaxed));
    if port.0 == 0 {
        port = syscall_handle(port_create())?;
        REPLY_PORT.store(port.0, Ordering::Relaxed);
    }
    Some(port)
}

/// A synchronous read cannot treat an unrelated supervisor wake as EOF. Binding replays
/// READABLE/PEER_CLOSED, including a reply or output queued before the bind. — TIMBERDOODLE
fn read_channel_ready(channel: Handle, reply: &mut [u8]) -> Option<usize> {
    let port = reply_port()?;
    if port_bind(port, channel) != 0 {
        return None;
    }
    let result = loop {
        let source = port_wait(port);
        if dispatch_service_event(source) {
            continue;
        }
        if source == handle_koid(channel) {
            let n = channel_read(channel, reply.as_mut_ptr(), reply.len()) as usize;
            break (n <= reply.len()).then_some(n);
        }
        if source == u64::MAX {
            break None;
        }
        // A packet already queued on the other wait context can outlive its retired watch.
    };
    let _ = port_unbind(port, channel);
    result
}

fn ttyd_request_reply(client: Handle, request: TtyRequest, reply: &mut [u8]) -> Option<usize> {
    let mut req = [0u8; ttyd::REQUEST_BUF_BYTES];
    let req_len = request.encode_into(&mut req)?;
    let port = reply_port()?;
    if port_bind(port, client) != 0 {
        return None;
    }
    // ChannelRead may return empty after an unrelated wake of the supervisor. Advancing
    // the next request then consumes the previous reply; a login mode/history ACK can
    // hide the initial empty read and leave every later key's echo one request behind.
    // PortWait retries unrelated wakes, and binding before sending also covers a fast
    // reply. Read only after this exchange's channel has a packet. — TIMBERDOODLE
    let result = if channel_write(client, req.as_ptr(), req_len) == 0 {
        loop {
            let source = port_wait(port);
            if dispatch_service_event(source) {
                continue;
            }
            if source == handle_koid(client) {
                let n = channel_read(client, reply.as_mut_ptr(), reply.len()) as usize;
                break (n > 0 && n <= reply.len()).then_some(n);
            }
            if source == u64::MAX {
                break None;
            }
        }
    } else {
        None
    };
    let _ = port_unbind(port, client);
    result
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
        false,
    );
}

#[cfg(feature = "nvme-write-proof")]
fn nvme_alias_proof(writable: bool) -> bool {
    // Send an actual write frame through both explicit access requests. Payload
    // equals the current VBR, so even a regression cannot corrupt its bytes.
    // RO refusals prove unchanged doorbells; admitted RW also proves Flush. — TIMBERDOODLE
    for access in [BlockAccess::ReadOnly, BlockAccess::ReadWrite] {
        let (client, server) = channel_create_pair();
        let port = port_create();
        if [client, server, port].contains(&u64::MAX) {
            for raw in [client, server, port]
                .into_iter()
                .filter(|&h| h != u64::MAX)
            {
                let _ = handle_close(Handle(raw as u32));
            }
            notice(&[b"NV8 RO proof setup failed\n"]);
            return false;
        }
        let client = Handle(client as u32);
        let server = Handle(server as u32);
        let port = Handle(port as u32);
        let control = Handle(STORAGE_CONTROLS[sora::NVME_STORAGE].load(Ordering::Relaxed));
        let tag = drv_blk::encode_connect(access);
        if port_bind(port, client) != 0
            || channel_write_with_handle(control, tag.as_ptr(), tag.len(), server) != 0
        {
            let _ = port_unbind(port, client);
            for h in [client, server, port] {
                let _ = handle_close(h);
            }
            notice(&[b"NV8 RO proof connect failed\n"]);
            return false;
        }
        let mut before = [0; 513];
        let mut after = [0; 513];
        let read = drv_blk::Request::read(0, 1).encode();
        let mut write = [0; drv_blk::REQUEST_LEN + 512];
        write[..drv_blk::REQUEST_LEN].copy_from_slice(&drv_blk::Request::write(0, 1).encode());
        let mut status = [0; 1];
        let ok = channel_write(client, read.as_ptr(), read.len()) == 0
            && nvme_receive(port, client, &mut before) == Some((513, 0))
            && before[0] == drv_blk::STATUS_OK;
        write[drv_blk::REQUEST_LEN..].copy_from_slice(&before[1..]);
        let expected = if writable && access == BlockAccess::ReadWrite {
            drv_blk::STATUS_OK
        } else {
            drv_blk::STATUS_READ_ONLY
        };
        let mut ok = ok
            && channel_write(client, write.as_ptr(), write.len()) == 0
            && nvme_receive(port, client, &mut status) == Some((1, 0))
            && status[0] == expected
            && channel_write(client, read.as_ptr(), read.len()) == 0
            && nvme_receive(port, client, &mut after) == Some((513, 0))
            && after == before;
        if ok && expected == drv_blk::STATUS_OK {
            let flush = drv_blk::Request::flush().encode();
            let mut result = [0; 1];
            ok = channel_write(client, flush.as_ptr(), flush.len()) == 0
                && nvme_receive(port, client, &mut result) == Some((1, 0))
                && result[0] == drv_blk::STATUS_OK;
        }
        notice(&[alloc::format!(
            "NV9 ALIAS asked={} expected={} got={} VBR unchanged={} {}\n",
            if access == BlockAccess::ReadOnly {
                "ro"
            } else {
                "rw"
            },
            expected,
            status[0],
            after == before,
            if ok { "OK" } else { "FAILED" }
        )
        .as_bytes()]);
        let _ = port_unbind(port, client);
        let _ = handle_close(client);
        let _ = handle_close(port);
        if !ok {
            return false;
        }
    }
    true
}
