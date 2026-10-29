//j636
//j638
//j639
//j683
//j685

//! R1 live adapter: read-only VOP2/TX diagnostic mappings and checked VOP writes through
//! Resource WRITE authority (DESIGN/022 D1d-a). The kernel protects all VOP/MMU pages. It
//! blanks HDMI0 for about two seconds and rebuilds the mode from KUMO's table. No clock, reset,
//! PHY, GRF or PMU write exists here, and the TX page is only read (see `rk3588_mode`).
//!
//! The dwell and the verification wait are one-shot timers on drv-fb's own port, so the
//! console keeps draining while the glass is blank. J607's first metal run slept instead, and
//! Sora stalled behind drv-fb's unread console channel. — TIMBERDOODLE
//!
//! J608 adds R1b between the dwell and removing black: run a distinguishable program with
//! HDMI0 clocked under black, observe its active registers and completed load, restore and
//! observe the target under black, then remove black. Every wait uses the port. — KESTREL
//!
//! J610 adds D1a after R2: paint a KUMO-owned contiguous buffer, flip ESMART2 region0 to it at
//! a frame start, hold it on the glass, then flip back to the GOP console buffer. Each flip's
//! load is timed until VP2 consumes it; no interrupt is used yet. — TIMBERDOODLE
//!
//! J611 adds D1b after D1a: arm the VOP's interrupt (INTID 188) with VP2's frame start as its
//! only source, count 120 frame starts on drv-fb's port and report their rate. — TIMBERDOODLE
//! The retained VOP grant closes on every terminal path; the final delivery is completed only
//! after the device source is disabled. — KESTREL
//!
//! J612 follows D1b with two owned buffers, 24 alternating consumed loads and explicit
//! retirement before repainting. IRQs only prompt observations; the console is restored on
//! completion or timeout, and uncertain buffers remain allocated. — KESTREL

use crate::phy_relock;
use core::fmt::{self, Write};
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};
use drv_fb::probe_va::VOP_D0A;
use drv_fb::probe_va::{TX_R1, VOP_R1};
use drv_fb::rk3588::{self, Power};
use drv_fb::rk3588_hdmi::{self, Tx, D0A_TIMING};
use drv_fb::rk3588_latch as latch;
use drv_fb::rk3588_mode::{
    blank, rebuild, verify, Blanked, Block, Framebuffer, Hardware, Outcome, Program, CHECKS,
    CHECK_BIT, DMT_1600X900_60_RB, PLAN, RECORDED, REG_CFG_DONE, TX_WINDOW_LEN, VOP_WINDOW_LEN,
    WORDS,
};
use drv_fb::rk3588_scanout as scanout;
use drv_fb::rk3588_vblank as vblank;
use drv_fb::PixelOrder;
use kumo_abi::{BootInfo, FramebufferFormat, Handle, Rights, VmarFlags};
use kumo_rt::{
    handle_close, handle_duplicate, handle_koid, interrupt_complete, interrupt_create,
    interrupt_wait, port_bind, port_unbind, resource_mint_mmio, syscall_handle, timer_create,
    vmar_map,
};

struct Log {
    bytes: [u8; 384],
    len: usize,
}

impl Write for Log {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
        let dst = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        dst.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn log_at(level: u8, args: fmt::Arguments<'_>) {
    let mut line = Log {
        bytes: [0; 384],
        len: 0,
    };
    if line.write_fmt(args).is_ok() {
        let _ = kumo_rt::log_write(level, &line.bytes[..line.len]);
    }
}

fn log(args: fmt::Arguments<'_>) {
    log_at(kumo_abi::sys::log_level::NOTICE, args);
}

fn error(args: fmt::Arguments<'_>) {
    log_at(kumo_abi::sys::log_level::ERR, args);
}

struct Device(Handle);

impl Device {
    fn address(block: Block, offset: u32) -> u64 {
        let (base, len) = match block {
            Block::Vop => (VOP_R1, VOP_WINDOW_LEN),
            Block::Tx => (TX_R1, TX_WINDOW_LEN),
        };
        assert!(
            offset % 4 == 0 && u64::from(offset) + 4 <= len,
            "R1 access outside its window"
        );
        base + u64::from(offset)
    }
}

impl Hardware for Device {
    fn read(&mut self, block: Block, offset: u32) -> u32 {
        unsafe { core::ptr::read_volatile(Self::address(block, offset) as *const u32) }
    }

    fn write(&mut self, block: Block, offset: u32, value: u32) {
        assert!(block == Block::Vop, "R1 never writes the HDMI TX");
        let index = drv_fb::rk3588_mode::register_index(offset).expect("unapproved VOP register");
        let status = kumo_rt::device_register_write(self.0, rk3588::VOP_BASE, index, value);
        assert_eq!(status, 0, "checked VOP write refused");
    }

    fn barrier(&mut self) {
        #[cfg(target_arch = "aarch64")]
        unsafe {
            core::arch::asm!("dsb sy", options(nostack, preserves_flags));
        }
    }

    fn sleep_ms(&mut self, ms: u64) {
        let _ = kumo_rt::sleep_ns(ms * 1_000_000);
    }
}

/// Exercise the new SVC with live vectors. The selected index is the forbidden +0x24
/// bypass/reset offset, not a table slot; this must return before any register access. — KESTREL
#[cfg(target_arch = "aarch64")]
fn register_boundary_probe(resource: Handle) -> bool {
    let mut status = u64::from(resource.0);
    let (a, b, c, d): (u64, u64, u64, u64);
    unsafe {
        core::arch::asm!(
            "movi v0.16b, #0x5a", "movi v31.16b, #0xa5", "svc #0",
            "umov x9, v0.d[0]", "umov x10, v0.d[1]",
            "umov x11, v31.d[0]", "umov x12, v31.d[1]",
            inlateout("x0") status,
            in("x1") rk3588::VOP_BASE, in("x2") 0x24u64, in("x3") 0u64,
            in("x8") kumo_abi::Syscall::DeviceRegisterWrite as u64,
            lateout("x9") a, lateout("x10") b, lateout("x11") c, lateout("x12") d,
            clobber_abi("C"), options(nostack),
        );
    }
    status as u32 as i32 == kumo_abi::Errno::InvalidArgs.status()
        && a == 0x5a5a5a5a5a5a5a5a
        && b == a
        && c == 0xa5a5a5a5a5a5a5a5
        && d == c
}
#[cfg(not(target_arch = "aarch64"))]
fn register_boundary_probe(_resource: Handle) -> bool {
    false
}

/// Mint diagnostic views. VOP Resource WRITE authorizes checked calls/IRQ, never a writable
/// VMO. Probe the denial before the real read-only mapping, without writing a register. — KESTREL
fn map(resource: Handle, base: u64, len: u64, va: u64, label: &str) -> bool {
    if resource.0 == 0 {
        log(format_args!("display: R1 {} grant absent\n", label));
        return false;
    }
    let raw = resource_mint_mmio(resource, base, len);
    let Some(vmo) = syscall_handle(raw) else {
        log(format_args!(
            "display: R1 {} mint failed (errno {})\n",
            label, raw as u32 as i32
        ));
        return false;
    };
    if base == rk3588::VOP_BASE {
        let rw = VmarFlags::READ | VmarFlags::WRITE | VmarFlags::DEVICE;
        let map_denied = vmar_map(Handle(0), vmo, 0, va, len, rw.0) as u32 as i32
            == kumo_abi::Errno::AccessDenied.status();
        let value = 0u32;
        let write_denied = kumo_rt::vmo_write(vmo, 0, &value as *const u32 as *const u8, 4) as u32
            as i32
            == kumo_abi::Errno::AccessDenied.status();
        let index_denied = register_boundary_probe(resource);
        if !(map_denied && write_denied && index_denied) {
            let _ = handle_close(vmo);
            error(format_args!(
                "display: D1d-a protection FAILED map={} write={} index/SIMD={}\n",
                map_denied, write_denied, index_denied
            ));
            return false;
        }
        log(format_args!("display: D1d-a raw writable map/VMO write and offset injection denied; SVC SIMD preserved\n"));
    }
    let flags = VmarFlags::READ | VmarFlags::DEVICE;
    let ok = vmar_map(Handle(0), vmo, 0, va, len, flags.0) == 0;
    let _ = handle_close(vmo);
    if !ok {
        log(format_args!("display: R1 {} mapping failed\n", label));
    }
    ok
}

#[derive(Clone, Copy)]
enum Phase {
    /// Blank and scrubbed; the timer ends the dwell.
    Dwell,
    /// VP2 runs the black-pixel proof, then the black-pixel target; one retry per stage.
    Proof {
        retried: bool,
    },
    Restore {
        retried: bool,
    },
    /// Rebuilt and out of standby; the timer starts the verification.
    Verify,
    R2Blank,
    R2Relock,
    R2Restore,
    R2Verify,
    /// D1a: the owned buffer is on the glass; the timer returns the console.
    D1Hold,
    /// D1b: let the console drain the scanout diagnostics before arming the source.
    D1bReady,
    /// D1b: counting VP2 frame starts; the timer is the watchdog.
    D1bCount,
    D1cReady,
    D1cRun,
    NativeWait,
    NativeHold,
}

/// What D1b needs from the earlier probes.
#[derive(Clone, Copy)]
pub struct Vblank {
    /// The live DTB's VOP interrupt as D0b decoded it, or 0.
    pub live_irq: u32,
    /// Whether D0a's read-only VOP window (with the MMU page) is mapped at `VOP_D0A`.
    pub mmu_view: bool,
    pub context: Handle,
    pub control: Handle,
    pub native: Handle,
    pub buffers: [Handle; 2],
}

/// Own the grant across timer transitions, including failures before D1b. — KESTREL
struct VopResource(Handle);

/// The channel outlives every phase move. Drop may cancel only while no native
/// image is possibly reachable; an uncertain publication closes without a receipt.
/// No cancellation is sent after successful completion. — KESTREL
/// Bounded frame-retired waits armed on the native channel; the presentations are bounded
/// the same way, and one outstanding wait per present is the client's shape. — TIMBERDOODLE
pub const MAX_NATIVE_WAITS: usize = 4;

/// True once the diagnostic completed through its retirement path. The serving loop arms
/// the production vblank gate only on this signal: a refused or timed-out diagnostic must
/// not leave a claimed frame-start authority behind. — TIMBERDOODLE
static DIAGNOSTIC_COMPLETED: AtomicBool = AtomicBool::new(false);

pub fn completed() -> bool {
    DIAGNOSTIC_COMPLETED.load(Ordering::Relaxed)
}

struct NativeChannel {
    handle: Handle,
    cancel_safe: bool,
    finished: bool,
}
pub fn cancel_native(handle: Handle) {
    if handle.0 == 0 {
        return;
    }
    let reply = kumo_graphics::native::Reply::cancelled().encode();
    let _ = kumo_rt::channel_write(handle, reply.as_ptr(), reply.len());
    log(format_args!(
        "display: native diagnostic cancelled with no image reachable; no reuse granted\n"
    ));
    close(handle);
}
impl Drop for NativeChannel {
    fn drop(&mut self) {
        if self.cancel_safe && !self.finished {
            cancel_native(self.handle);
        } else {
            close(self.handle);
        }
    }
}

#[derive(Clone, Copy)]
struct OwnedBuffer {
    vmo: Handle,
    address: u32,
    va: u64,
}

#[derive(Clone, Copy)]
struct NativeFrame {
    image: kumo_graphics::native::Image,
    vmo: Handle,
}

struct Alternation {
    pair: scanout::Pair,
    buffers: [OwnedBuffer; 2],
    painted: [u32; 2],
    started: u64,
    front_since: u64,
    submitted: u64,
    irqs: u32,
}

// As with J611's samples, phase moves must not copy this state through the 16 KiB stack.
// The one-time claim owns the sole reference for this boot diagnostic. — KESTREL
static ALTERNATION_TAKEN: AtomicBool = AtomicBool::new(false);
static mut ALTERNATION: MaybeUninit<Alternation> = MaybeUninit::uninit();

// A buffer inside Pending multiplies across phase moves and exhausts drv-fb's 16 KiB stack.
// This boot diagnostic claims one static sample buffer exactly once; the atomic prevents
// aliasing even if a future caller attempts a second sequence. No heap or DMA is needed.
// — KESTREL
static SAMPLES_TAKEN: AtomicBool = AtomicBool::new(false);
static mut INTERVALS: [u64; vblank::FRAMES as usize - 1] = [0; vblank::FRAMES as usize - 1];

fn take_samples() -> Option<&'static mut [u64; vblank::FRAMES as usize - 1]> {
    if SAMPLES_TAKEN.swap(true, Ordering::Relaxed) {
        return None;
    }
    // SAFETY: the claim can succeed only once and the reference is moved with Pending.
    Some(unsafe { &mut *core::ptr::addr_of_mut!(INTERVALS) })
}

impl Drop for VopResource {
    fn drop(&mut self) {
        close(self.0);
    }
}

/// R1 in flight between port wakes.
pub struct Pending {
    program: Program,
    blanked: Blanked,
    phase: Phase,
    port: Handle,
    timer: Handle,
    /// The timer's koid, which is what `port_wait` reports.
    pub source: u64,
    started: u64,
    proof_verified: bool,
    power: Power,
    grants: phy_relock::Grants,
    /// The console's colour convention, which G5b confirmed on the glass.
    pixel_order: PixelOrder,
    /// D1a's buffer handle, kept open while the VOP may scan it.
    owned: OwnedBuffer,
    /// The VOP window Resource, kept until D1b creates its interrupt from it.
    vop: VopResource,
    vblank: Vblank,
    /// D1b's interrupt object and its koid, once armed.
    irq: Handle,
    irq_source: u64,
    meter: vblank::Meter,
    cadence_attempt: u8,
    intervals: Option<&'static mut [u64; vblank::FRAMES as usize - 1]>,
    alternation: Option<&'static mut Alternation>,
    native_channel: NativeChannel,
    native_frame: Option<NativeFrame>,
    native_source: u64,
    /// The highest presentation sequence whose consumed-load retirement is proven.
    native_retired: u64,
    /// Armed frame-retired waits: (point, wake write end). — TIMBERDOODLE
    native_waits: [Option<(u64, Handle)>; MAX_NATIVE_WAITS],
    native_lease: kumo_graphics::native::Lease,
    native_cpu: u64,
}

impl Pending {
    pub fn measuring_vblank(&self) -> bool {
        matches!(self.phase, Phase::D1bCount)
    }
    /// Whether a port wake belongs to R1-D1b: its timer, or D1b's interrupt.
    pub fn owns(&self, source: Handle) -> bool {
        Handle(self.source as u32) == source
            || (matches!(self.phase, Phase::NativeWait)
                && Handle(self.native_source as u32) == source)
            || (self.irq.0 != 0 && Handle(self.irq_source as u32) == source)
    }
}

/// Route a wake that [`Pending::owns`]: D1b's interrupt, or the phase timer.
pub fn wake(pending: Pending, source: Handle) -> Option<Pending> {
    if matches!(pending.phase, Phase::NativeWait) && Handle(pending.native_source as u32) == source
    {
        return native_present(pending);
    }
    if pending.irq.0 != 0 && Handle(pending.irq_source as u32) == source {
        on_vblank(pending)
    } else {
        advance(pending)
    }
}

fn arm(port: Handle, ms: u64) -> Option<(Handle, u64)> {
    let raw = timer_create(ms * 1_000_000);
    let timer = syscall_handle(raw)?;
    let koid = handle_koid(timer);
    if koid == u64::MAX || port_bind(port, timer) != 0 {
        let _ = handle_close(timer);
        return None;
    }
    Some((timer, koid))
}

fn disarm(port: Handle, timer: Handle) {
    let _ = port_unbind(port, timer);
    let _ = handle_close(timer);
}

/// Blank HDMI0 and scrub the VP2/ESMART2 program, when R0's five expectations all matched,
/// and arm the dwell timer on `port`. Consumes both grants, retaining the VOP grant while
/// work remains for [`wake`].
pub fn start(
    boot: &BootInfo,
    power: Option<Power>,
    r0_matched: bool,
    vop: Handle,
    tx: Handle,
    grants: phy_relock::Grants,
    vblank: Vblank,
    port: Handle,
) -> Option<Pending> {
    let eligible = rk3588::eligible(
        boot.board_id(),
        boot.has_framebuffer() && boot.framebuffer.is_plausible(),
    );
    let mut pending = None;
    if eligible {
        match power {
            Some(power) if power.vop_accessible() && rk3588_hdmi::hdmi_accessible(power) => {
                if r0_matched {
                    pending = begin(boot, vop, tx, grants, vblank, power, port);
                } else {
                    log(format_args!(
                        "display: R1 skipped: R0's shared state did not match 5/5\n"
                    ));
                }
            }
            _ => log(format_args!(
                "display: R1 skipped: no PMU observation, or VOP/VO1 down or idle\n"
            )),
        }
    }
    // The VOP Resource stays open in `Pending` for D1b's interrupt; otherwise close it.
    if pending.is_none() && vop.0 != 0 {
        let _ = handle_close(vop);
    }
    if tx.0 != 0 {
        let _ = handle_close(tx);
    }
    pending
}

fn begin(
    boot: &BootInfo,
    vop: Handle,
    tx: Handle,
    grants: phy_relock::Grants,
    vblank: Vblank,
    power: Power,
    port: Handle,
) -> Option<Pending> {
    let fb = Framebuffer {
        phys: boot.framebuffer.phys,
        width: boot.framebuffer.width,
        height: boot.framebuffer.height,
        stride: boot.framebuffer.stride,
    };
    let mode = DMT_1600X900_60_RB;
    let Some(program) = Program::build(mode, fb) else {
        log(format_args!(
            "display: R1 skipped: GOP {}x{} stride {} at {:#x} does not fit KUMO's DMT 1600x900@60 entry\n",
            fb.width, fb.height, fb.stride, fb.phys
        ));
        return None;
    };
    if !map(vop, rk3588::VOP_BASE, VOP_WINDOW_LEN, VOP_R1, "VOP2")
        || !map(tx, rk3588_hdmi::HDMI_BASE, TX_WINDOW_LEN, TX_R1, "HDMI0 TX")
    {
        return None;
    }
    let mut device = Device(vop);
    log(format_args!(
        "display: R1 KUMO mode table: DMT 1600x900@60 RB, H {}/{}/{}/{} V {}/{}/{}/{}, {} Hz; {} VOP words written, {} TX words checked; scanout {:#x} stride {}\n",
        mode.h_active,
        mode.h_front,
        mode.h_sync,
        mode.h_back,
        mode.v_active,
        mode.v_front,
        mode.v_sync,
        mode.v_back,
        mode.pixel_hz,
        WORDS,
        CHECKS,
        fb.phys,
        fb.stride
    ));
    let recorded = RECORDED.map(|offset| device.read(Block::Vop, offset));
    log(format_args!(
        "display: R1 recorded, not written: REG_CFG_DONE={:#010x} POST2 +0x30..+0x40={:#010x} {:#010x} {:#010x} {:#010x} {:#010x}\n",
        device.read(Block::Vop, REG_CFG_DONE),
        recorded[0],
        recorded[1],
        recorded[2],
        recorded[3],
        recorded[4]
    ));
    let started = kumo_rt::clock_get();
    let blanked = match blank(&mut device, &program, PLAN) {
        Ok(blanked) => blanked,
        Err(Outcome::NotImmediate { pol }) => {
            log(format_args!(
                "display: R1 refused, nothing written: DSP_INFACE_POL={:#010x} lacks regdone_imd_en\n",
                pol
            ));
            return None;
        }
        Err(Outcome::Refused { mismatched }) => {
            log(format_args!(
                "display: R1 refused, nothing written: {} word(s) differ from KUMO's table\n",
                mismatched.count_ones()
            ));
            report_mismatches(&mut device, &program, mismatched);
            return None;
        }
        Err(Outcome::Rebuilt { .. }) => return None,
    };
    let s = blanked.scrub_readback;
    log(format_args!(
        "display: R1 blank: VP2 standby, HDMI0 interface off, scrubbed to reset values (readback clk/ifctrl/timing x4/esmart x6: {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}); rebuilding in {} ms\n",
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7], s[8], s[9], s[10], s[11],
        PLAN.dwell_ms
    ));
    let Some((timer, source)) = arm(port, PLAN.dwell_ms) else {
        // Without a timer, rebuild now rather than leave the glass blank.
        error(format_args!(
            "display: R1 dwell timer unavailable; rebuilding immediately\n"
        ));
        rebuild(&mut device, &program);
        finish(&mut device, &program, blanked, started, false);
        return None;
    };
    let pixel_order = match boot.framebuffer.format {
        FramebufferFormat::Rgb => PixelOrder::Rgb,
        FramebufferFormat::Bgr | FramebufferFormat::Unknown => PixelOrder::Bgr,
    };
    Some(Pending {
        program,
        blanked,
        phase: Phase::Dwell,
        port,
        timer,
        source,
        started,
        proof_verified: false,
        power,
        grants,
        pixel_order,
        owned: OwnedBuffer {
            vmo: Handle(0),
            address: 0,
            va: 0,
        },
        vop: VopResource(vop),
        vblank,
        irq: Handle(0),
        irq_source: 0,
        meter: vblank::Meter::default(),
        cadence_attempt: 0,
        intervals: None,
        alternation: None,
        native_channel: NativeChannel {
            handle: vblank.native,
            cancel_safe: true,
            finished: false,
        },
        native_frame: None,
        native_source: 0,
        native_retired: 0,
        native_waits: [None; MAX_NATIVE_WAITS],
        native_lease: kumo_graphics::native::Lease::default(),
        native_cpu: 0,
    })
}

/// Handle the timer `port_wait` reported as `pending.source`. Returns the next step, if any.
pub fn advance(mut pending: Pending) -> Option<Pending> {
    disarm(pending.port, pending.timer);
    let mut device = Device(pending.vop.0);
    match pending.phase {
        Phase::Dwell => {
            latch::stage(&mut device, &latch::proof(&pending.program));
            log(format_args!(
                "display: R1b staged black-pixel proof: H total {} V total {}, ESMART2 region0 disabled; VP2 running\n",
                latch::PROOF_MODE.h_total(), latch::PROOF_MODE.v_total()
            ));
            next(pending, Phase::Proof { retried: false }, latch::SETTLE_MS)
        }
        Phase::Proof { retried } => {
            let image = latch::proof(&pending.program);
            let observed = latch::observe(&mut device, &image);
            if !observed.matches() && !retried {
                latch::stage(&mut device, &image);
                return next(pending, Phase::Proof { retried: true }, latch::SETTLE_MS);
            }
            pending.proof_verified = observed.matches();
            log_at(if observed.matches() { kumo_abi::sys::log_level::NOTICE } else { kumo_abi::sys::log_level::ERR }, format_args!(
                "display: R1b black-pixel proof {}: mismatch mask {:#x}, load_pending={}{}; restoring target\n",
                if observed.matches() { "latched" } else { "FAILED" },
                observed.mismatched, u8::from(observed.load_pending),
                if retried { " (after one retry)" } else { "" }
            ));
            latch::stage(&mut device, &latch::blacked(&pending.program));
            next(pending, Phase::Restore { retried: false }, latch::SETTLE_MS)
        }
        Phase::Restore { retried } => {
            let image = latch::blacked(&pending.program);
            let observed = latch::observe(&mut device, &image);
            if !observed.matches() && !retried {
                latch::stage(&mut device, &image);
                return next(pending, Phase::Restore { retried: true }, latch::SETTLE_MS);
            }
            let observed = latch::unblack(&mut device, &pending.program);
            if !observed.matches() {
                error(format_args!(
                    "display: R1b restoration FAILED after one retry: mismatch mask {:#x}, load_pending={}; HDMI0 off, VP2 in standby\n",
                    observed.mismatched, u8::from(observed.load_pending)
                ));
                return None;
            }
            log(format_args!(
                "display: R1b target latched under black{}; removing black\n",
                if retried { " (after one retry)" } else { "" }
            ));
            next(pending, Phase::Verify, PLAN.verify_after_ms)
        }
        Phase::Verify => {
            let restored = finish(
                &mut device,
                &pending.program,
                pending.blanked,
                pending.started,
                true,
            );
            if restored && pending.proof_verified {
                log(format_args!(
                    "display: R1b complete: distinct timing and disabled region latched, target restored and TX timing matched\n"
                ));
                if phy_relock::prepare(&pending.grants) {
                    latch::stage(&mut device, &latch::blacked(&pending.program));
                    return next(pending, Phase::R2Blank, latch::SETTLE_MS);
                }
            }
            None
        }
        Phase::R2Blank => {
            let observed = latch::observe(&mut device, &latch::blacked(&pending.program));
            if !observed.matches() {
                report_mismatches(
                    &mut device,
                    &latch::blacked(&pending.program),
                    observed.mismatched,
                );
                latch::stop(&mut device, &pending.program);
                error(format_args!(
                    "display: R2 black program did not latch: {:?}; no PHY reset, VP2 in standby\n",
                    observed
                ));
                return None;
            }
            let ctrl = pending.program.words[drv_fb::rk3588_mode::DSP_CTRL_INDEX];
            device.write(
                ctrl.block,
                ctrl.offset,
                ctrl.value | latch::BLACK | drv_fb::rk3588_mode::STANDBY,
            );
            device.barrier();
            next(pending, Phase::R2Relock, PLAN.standby_settle_ms)
        }
        Phase::R2Relock => {
            phy_relock::run(&pending.grants)?;
            latch::stage(&mut device, &latch::blacked(&pending.program));
            next(pending, Phase::R2Restore, latch::SETTLE_MS)
        }
        Phase::R2Restore => {
            let observed = latch::unblack(&mut device, &pending.program);
            if !observed.matches() {
                error(format_args!(
                    "display: R2 target failed to latch after PHY re-lock: {:?}; VP2 in standby\n",
                    observed
                ));
                return None;
            }
            next(pending, Phase::R2Verify, PLAN.verify_after_ms)
        }
        Phase::R2Verify => {
            if finish(
                &mut device,
                &pending.program,
                pending.blanked,
                pending.started,
                true,
            ) && phy_relock::verify(pending.power)
            {
                log(format_args!("display: R2 complete: native HDMI0 1600x900@60 reconstructed; D0 hardware gates complete\n"));
                if d1a_show(&mut device, &mut pending) {
                    // Without a hold timer, hand the glass straight back to the console rather
                    // than let `next` stop the display.
                    return match arm(pending.port, scanout::HOLD_MS) {
                        Some((timer, source)) => Some(Pending {
                            phase: Phase::D1Hold,
                            timer,
                            source,
                            ..pending
                        }),
                        None => {
                            d1a_return(&mut device, &pending);
                            None
                        }
                    };
                }
            } else {
                latch::stop(&mut device, &pending.program);
                error(format_args!(
                    "display: R2 final verification failed; VP2 in standby\n"
                ));
            }
            None
        }
        Phase::D1Hold => {
            if d1a_return(&mut device, &pending) {
                // Console scrolling can exceed a frame; our own diagnostics must drain before
                // measuring deliveries on the same event loop. The wait uses the port. — KESTREL
                log(format_args!(
                    "display: D1b waiting {} ms for console diagnostics to drain before counting {} VP2 frame starts on INTID {}\n",
                    vblank::SETTLE_MS, vblank::FRAMES, rk3588::VOP_INTID
                ));
                match arm(pending.port, vblank::SETTLE_MS) {
                    Some((timer, source)) => Some(Pending {
                        phase: Phase::D1bReady,
                        timer,
                        source,
                        ..pending
                    }),
                    None => {
                        error(format_args!(
                            "display: D1b settle timer unavailable; nothing armed\n"
                        ));
                        None
                    }
                }
            } else {
                None
            }
        }
        Phase::D1bReady => d1b_start(pending, &mut device),
        Phase::D1bCount => {
            // The watchdog fired before the frames arrived; the timer is already disarmed.
            d1b_finish(pending, &mut device, true)
        }
        Phase::D1cReady => d1c_start(pending, &mut device),
        Phase::D1cRun => d1c_finish(pending, &mut device, false, "watchdog"),
        Phase::NativeWait => {
            error(format_args!(
                "display: J0a image wait timed out; console retained\n"
            ));
            let _ = port_unbind(pending.port, pending.vblank.native);
            for slot in &mut pending.native_waits {
                if let Some((_, handle)) = slot.take() {
                    close(handle);
                }
            }
            None
        }
        Phase::NativeHold => native_retire(pending, &mut device),
    }
}

fn close(handle: Handle) {
    if handle.0 != 0 {
        let _ = handle_close(handle);
    }
}

/// D1b: arm the VOP interrupt with VP2's frame start as its only source and count frames.
fn open_vop_irq(pending: &Pending, device: &mut Device, label: &str) -> Option<(Handle, u64)> {
    let intid = rk3588::VOP_INTID;
    if pending.vblank.live_irq != intid || !pending.vblank.mmu_view {
        log(format_args!(
            "display: {} skipped: live DTB IRQ or read-only MMU view unavailable\n",
            label
        ));
        return None;
    }
    // SAFETY: D0a mapped VOP_LEN read-only at VOP_D0A; all four MMU offsets lie within it.
    let mmu = |offset: u32| unsafe {
        assert!(u64::from(offset) + 4 <= rk3588::VOP_LEN);
        core::ptr::read_volatile((VOP_D0A + u64::from(offset)) as *const u32)
    };
    let preflight = if pending.vblank.context.0 != 0 {
        let mut info = kumo_abi::sys::DeviceCtxInfo {
            fault_record: 0,
            fault_addr: 0,
        };
        if kumo_rt::device_ctx_info(pending.vblank.context, &mut info) != 0
            || info.fault_record
                != kumo_abi::sys::device_ctx_state::RECORD | kumo_abi::sys::device_ctx_state::LIVE
            || info.fault_addr == 0
        {
            error(format_args!(
                "display: {} refused: DeviceCtx is not LIVE\n",
                label
            ));
            return None;
        }
        vblank::preflight_owned(device, mmu)
    } else {
        vblank::preflight(device, mmu)
    };
    if let Err(conflict) = preflight {
        log(format_args!(
            "display: {} refused, nothing armed: shared VOP line conflict {:?}\n",
            label, conflict
        ));
        return None;
    }
    let raw = interrupt_create(pending.vop.0, intid);
    let Some(irq) = syscall_handle(raw) else {
        error(format_args!(
            "display: {} interrupt {} unavailable (errno {})\n",
            label, intid, raw as u32 as i32
        ));
        return None;
    };
    let koid = handle_koid(irq);
    // Exercise ownership before enabling a device source: a second binding must
    // refuse, while a duplicate survives closing its original handle. — KESTREL
    let second = interrupt_create(pending.vop.0, intid);
    if second as u32 as i32 != kumo_abi::Errno::BadState as i32 {
        if let Some(extra) = syscall_handle(second) {
            close(extra);
        }
        close(irq);
        error(format_args!(
            "display: {} independent IRQ owner was not denied\n",
            label
        ));
        return None;
    }
    let Some(duplicate) = syscall_handle(handle_duplicate(
        irq,
        Rights::READ | Rights::WAIT | Rights::DUPLICATE,
    )) else {
        close(irq);
        return None;
    };
    if handle_close(irq) != 0 || handle_koid(duplicate) != koid {
        close(duplicate);
        return None;
    }
    let irq = duplicate;
    log(format_args!(
        "display: D1d-c1 {} IRQ {}: second owner denied, original closed, duplicate retained\n",
        label, intid
    ));
    if koid == u64::MAX || port_bind(pending.port, irq) != 0 {
        error(format_args!(
            "display: {} could not bind interrupt {}\n",
            label, intid
        ));
        close(irq);
        return None;
    }
    Some((irq, koid))
}

fn d1b_start(mut pending: Pending, device: &mut Device) -> Option<Pending> {
    let Some(intervals) = pending.intervals.take().or_else(take_samples) else {
        error(format_args!(
            "display: D1b sample buffer already claimed; nothing armed\n"
        ));
        return None;
    };
    let (irq, koid) = open_vop_irq(&pending, device, "D1b")?;
    let Some((timer, source)) = arm(pending.port, vblank::WATCHDOG_MS) else {
        let _ = port_unbind(pending.port, irq);
        close(irq);
        error(format_args!(
            "display: D1b watchdog timer unavailable; disarmed\n"
        ));
        return None;
    };
    // Recover a stale masked GIC delivery while device sources are still off. Both port
    // bindings exist before enabling; no console log runs inside the measurement. — KESTREL
    let _ = interrupt_complete(irq);
    vblank::enable(device);
    pending.irq = irq;
    pending.irq_source = koid;
    pending.meter = vblank::Meter::default();
    pending.cadence_attempt += 1;
    pending.intervals = Some(intervals);
    Some(Pending {
        phase: Phase::D1bCount,
        timer,
        source,
        ..pending
    })
}

/// One D1b delivery: take it, clear the device's status, record it, then let the kernel unmask
/// the line (DESIGN/016's order).
fn on_vblank(mut pending: Pending) -> Option<Pending> {
    if matches!(pending.phase, Phase::D1cRun) {
        return d1c_vblank(pending);
    }
    let mut device = Device(pending.vop.0);
    let _ = interrupt_wait(pending.irq);
    let status = vblank::acknowledge(&mut device);
    let now = kumo_rt::clock_get();
    let previous = pending.meter.last_ns;
    let count = pending.meter.count;
    pending.meter.record(status, now);
    if status & vblank::FS != 0 && count != 0 {
        if let Some(sample) = pending
            .intervals
            .as_mut()
            .and_then(|samples| samples.get_mut(count as usize - 1))
        {
            *sample = now.saturating_sub(previous);
        }
    }
    if pending.meter.done() {
        disarm(pending.port, pending.timer);
        return d1b_finish(pending, &mut device, false);
    }
    let _ = interrupt_complete(pending.irq);
    Some(pending)
}

fn d1b_finish(mut pending: Pending, device: &mut Device, timed_out: bool) -> Option<Pending> {
    vblank::disable(device);
    let _ = vblank::acknowledge(device);
    let _ = interrupt_complete(pending.irq);
    let _ = port_unbind(pending.port, pending.irq);
    close(pending.irq);
    pending.irq = Handle(0);
    pending.irq_source = 0;
    let meter = pending.meter;
    let mode_mhz = vblank::mode_millihertz(&DMT_1600X900_60_RB);
    let delivered = meter.millihertz().unwrap_or(0);
    let cadence = pending.intervals.as_mut().and_then(|samples| {
        vblank::cadence(&mut samples[..meter.count.saturating_sub(1) as usize])
    });
    if vblank::retry_delivery_window(meter, cadence, mode_mhz, pending.cadence_attempt, timed_out) {
        let rate = cadence.expect("retry requires a cadence");
        log(format_args!("display: D1b delivery window {}/{} inconclusive: {}/{} stable at {}.{} Hz; fresh120-frame retry, thresholds unchanged\n",pending.cadence_attempt,vblank::MAX_CADENCE_ATTEMPTS,rate.stable,rate.total,rate.millihertz/1000,rate.millihertz%1000));
        return next(pending, Phase::D1bReady, vblank::SETTLE_MS);
    }
    let passed = !timed_out
        && meter.spurious == 0
        && meter.done()
        && cadence
            .is_some_and(|rate| rate.credible() && vblank::matches(rate.millihertz, mode_mhz));
    match cadence {
        Some(rate) if !timed_out && meter.spurious == 0 && meter.done() && rate.credible() && vblank::matches(rate.millihertz, mode_mhz) => log(format_args!(
            "display: D1b vblank: {} IRQs, median {}.{:03} Hz (mode {}.{:03}; {}/{} stable), delivery {}.{:03} Hz, interval min/max {}/{} us, spurious {}; interrupt disarmed; D1b complete\n",
            meter.count,
            rate.millihertz / 1_000,
            rate.millihertz % 1_000,
            mode_mhz / 1_000,
            mode_mhz % 1_000,
            rate.stable,
            rate.total,
            delivered / 1_000,
            delivered % 1_000,
            meter.min_interval_ns / 1_000,
            meter.max_interval_ns / 1_000,
            meter.spurious
        )),
        Some(rate) => error(format_args!(
            "display: D1b DISAGREEMENT: {} IRQs{}, median {}.{:03} Hz against mode {}.{:03}, {}/{} stable, delivery {}.{:03} Hz (interval min/max {}/{} us, spurious {}); disarmed\n",
            meter.count,
            if timed_out { " before the watchdog" } else { "" },
            rate.millihertz / 1_000,
            rate.millihertz % 1_000,
            mode_mhz / 1_000,
            mode_mhz % 1_000,
            rate.stable,
            rate.total,
            delivered / 1_000,
            delivered % 1_000,
            meter.min_interval_ns / 1_000,
            meter.max_interval_ns / 1_000,
            meter.spurious
        )),
        None => error(format_args!(
            "display: D1b FAILED: {} frame-start interrupt(s) within {} ms (spurious {}); interrupt disarmed\n",
            meter.count,
            vblank::WATCHDOG_MS,
            meter.spurious
        )),
    }
    if passed {
        d1c_prepare(pending, device)
    } else {
        None
    }
}

fn d1c_allocate(stride: u32, height: u32, prepared: Handle, iova: u32) -> Option<OwnedBuffer> {
    let len = (u64::from(stride) * u64::from(height) * 4).next_multiple_of(4096);
    let (vmo, phys) = if prepared.0 != 0 {
        (prepared, u64::from(iova))
    } else {
        let (raw, phys) = kumo_rt::vmo_create_contiguous(len);
        (syscall_handle(raw)?, phys)
    };
    let buffer = scanout::Buffer {
        address: phys,
        len,
        stride,
        height,
    };
    let Some(address) = buffer.scanout() else {
        close(vmo);
        return None;
    };
    let flags = VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED;
    let (status, va) = kumo_rt::vmar_map_anywhere(vmo, len, flags.0);
    if status != 0 || va == 0 {
        close(vmo);
        return None;
    }
    Some(OwnedBuffer { vmo, address, va })
}

fn d1c_prepare(mut pending: Pending, device: &mut Device) -> Option<Pending> {
    let (gop, stride) = scanout::target_scanout(&pending.program);
    if pending.owned.vmo.0 == 0 || !scanout::on_glass(device, gop) {
        error(format_args!(
            "display: D1c refused: D1a buffer or confirmed console unavailable\n"
        ));
        return None;
    }
    if ALTERNATION_TAKEN.swap(true, Ordering::Relaxed) {
        error(format_args!(
            "display: D1c already attempted; nothing armed\n"
        ));
        return None;
    }
    let height = DMT_1600X900_60_RB.v_active;
    let Some(second) = d1c_allocate(stride, height, pending.vblank.buffers[1], 0x61000000) else {
        error(format_args!(
            "display: D1c second buffer unavailable; console retained\n"
        ));
        return None;
    };
    let buffers = [pending.owned, second];
    let len = (u64::from(stride) * u64::from(height) * 4).next_multiple_of(4096);
    let Some(pair) = scanout::Pair::new(
        buffers.map(|b| scanout::Buffer {
            address: u64::from(b.address),
            len,
            stride,
            height,
        }),
        gop,
    ) else {
        error(format_args!(
            "display: D1c invalid/overlapping pair; console retained\n"
        ));
        return None;
    };
    // SAFETY: the atomic claim above succeeds only once. This state is initialized in place,
    // and its sole mutable reference moves with Pending; buffer handles remain open. — KESTREL
    pending.alternation = Some(unsafe {
        let ptr = core::ptr::addr_of_mut!(ALTERNATION).cast::<Alternation>();
        ptr.write(Alternation {
            pair,
            buffers,
            painted: [0; 2],
            started: 0,
            front_since: 0,
            submitted: 0,
            irqs: 0,
        });
        &mut *ptr
    });
    log(format_args!(
        "display: D1c prepared owned A={:#x} B={:#x}, {} bytes each uncached; {} alternating loads, {} ms holds, repaint only after consumed replacement; console then returns\n",
        buffers[0].address, buffers[1].address, len,
        scanout::ALTERNATIONS, scanout::FRAME_HOLD_NS / 1_000_000
    ));
    let Some((timer, source)) = arm(pending.port, vblank::SETTLE_MS) else {
        error(format_args!(
            "display: D1c settle timer unavailable; console retained\n"
        ));
        return None;
    };
    Some(Pending {
        phase: Phase::D1cReady,
        timer,
        source,
        ..pending
    })
}

fn d1c_start(mut pending: Pending, device: &mut Device) -> Option<Pending> {
    pending.alternation.as_mut()?.started = kumo_rt::clock_get();
    let (irq, koid) = open_vop_irq(&pending, device, "D1c")?;
    let Some((timer, source)) = arm(pending.port, scanout::ALTERNATION_WATCHDOG_MS) else {
        let _ = port_unbind(pending.port, irq);
        close(irq);
        error(format_args!(
            "display: D1c watchdog unavailable; nothing armed\n"
        ));
        return None;
    };
    let _ = interrupt_complete(irq);
    vblank::enable(device);
    pending.irq = irq;
    pending.irq_source = koid;
    Some(Pending {
        phase: Phase::D1cRun,
        timer,
        source,
        ..pending
    })
}

fn d1c_paint(
    device: &mut Device,
    buffer: OwnedBuffer,
    slot: usize,
    generation: u32,
    order: PixelOrder,
    stride: u32,
) {
    let mode = DMT_1600X900_60_RB;
    for y in 0..mode.v_active {
        for x in 0..mode.h_active {
            let (r, g, b) =
                scanout::alternating_pattern(slot, generation, x, y, mode.h_active, mode.v_active);
            let index = u64::from(y) * u64::from(stride) + u64::from(x);
            // SAFETY: each uncached mapping covers stride * height * 4; the pair has certified
            // this slot retired, with no outstanding load. No new load runs during painting.
            unsafe {
                ((buffer.va + index * 4) as *mut u32).write_volatile(order.pixel(r, g, b));
            }
        }
    }
    device.barrier();
}

fn d1c_vblank(mut pending: Pending) -> Option<Pending> {
    let mut device = Device(pending.vop.0);
    let _ = interrupt_wait(pending.irq);
    if !context_ready(pending.vblank.context) {
        recover_interrupted_d1c(&mut pending, &mut device);
        return None;
    }
    let status = vblank::acknowledge(&mut device);
    if status & vblank::FS == 0 {
        let _ = interrupt_complete(pending.irq);
        return Some(pending);
    }
    let now = kumo_rt::clock_get();
    let (_, stride) = scanout::target_scanout(&pending.program);
    let state = pending.alternation.as_mut().expect("D1c state");
    state.irqs += 1;
    let mut finished = false;
    let mut stalled = false;
    if let Some((_, front)) = state.pair.observe(&mut device, true) {
        state.front_since = now;
        state.submitted = 0;
        finished = front == scanout::CONSOLE;
    }
    if !finished {
        if state.pair.pending().is_some() {
            // An IRQ count is not retirement. Allow delayed userspace delivery, but fail a
            // load still unconfirmed after 500 ms; the 15 s watchdog also covers lost IRQs.
            // — KESTREL
            stalled = now.saturating_sub(state.submitted) > 500_000_000;
        } else if state.pair.front() == scanout::CONSOLE
            || now.saturating_sub(state.front_since) >= scanout::FRAME_HOLD_NS
        {
            if state.pair.confirmed() >= scanout::ALTERNATIONS {
                state.pair.restore_console(&mut device);
                state.submitted = kumo_rt::clock_get();
            } else {
                let slot = if state.pair.front() == scanout::CONSOLE {
                    0
                } else {
                    1 - state.pair.front()
                };
                if state.pair.can_paint(slot) {
                    d1c_paint(
                        &mut device,
                        state.buffers[slot],
                        slot,
                        state.pair.confirmed() + 1,
                        pending.pixel_order,
                        stride,
                    );
                    if state.pair.stage(&mut device, slot) {
                        state.painted[slot] += 1;
                        state.submitted = kumo_rt::clock_get();
                    } else {
                        stalled = true;
                    }
                } else {
                    stalled = true;
                }
            }
        }
    }
    if finished || stalled {
        disarm(pending.port, pending.timer);
        return d1c_finish(
            pending,
            &mut device,
            finished,
            "load unconfirmed or invalid painting target",
        );
    }
    let _ = interrupt_complete(pending.irq);
    Some(pending)
}

/// The caller has disarmed the timer. Keep both VMO handles/mappings until reboot, including
/// on recovery failure: an unverified front must never be freed or reused. — KESTREL
fn d1c_finish(
    mut pending: Pending,
    device: &mut Device,
    success: bool,
    reason: &str,
) -> Option<Pending> {
    if !context_ready(pending.vblank.context) {
        recover_interrupted_d1c(&mut pending, device);
        return None;
    }
    vblank::disable(device);
    let state = pending.alternation.as_mut().expect("D1c state");
    let (gop, _) = scanout::target_scanout(&pending.program);
    let mut restored = state.pair.front() == scanout::CONSOLE
        && state.pair.pending().is_none()
        && scanout::on_glass(device, gop);
    if !restored {
        for _ in 0..2 {
            state.pair.restore_console(device);
            let latched =
                scanout::wait_latched(device, kumo_rt::clock_get, scanout::LATCH_TIMEOUT_NS);
            if latched.is_some() && state.pair.observe(device, true).is_some() {
                restored = true;
                break;
            }
        }
    }
    let _ = vblank::acknowledge(device);
    let _ = interrupt_complete(pending.irq);
    let _ = port_unbind(pending.port, pending.irq);
    close(pending.irq);
    pending.irq = Handle(0);
    pending.irq_source = 0;
    let expected = scanout::ALTERNATIONS / 2;
    let verified = success
        && restored
        && state.pair.confirmed() == scanout::ALTERNATIONS
        && state.painted == [expected; 2]
        && state.pair.retirements() == [expected; 2];
    let elapsed = kumo_rt::clock_get().saturating_sub(state.started) / 1_000_000;
    if verified {
        log(format_args!(
            "display: D1c {} owned loads consumed in {} ms on {} IRQs; paints A/B={}/{}, retired A/B={}/{}, post-retirement reuses A/B={}/{}\n",
            state.pair.confirmed(), elapsed, state.irqs, state.painted[0], state.painted[1],
            state.pair.retirements()[0], state.pair.retirements()[1],
            state.painted[0] - 1, state.painted[1] - 1
        ));
        log(format_args!("display: D1c console {:#x} latched; both owned buffers retired; interrupt disarmed; D1c complete\n", gop));
        let (_, stride) = scanout::target_scanout(&pending.program);
        let len =
            (u64::from(stride) * u64::from(DMT_1600X900_60_RB.v_active) * 4).next_multiple_of(4096);
        if pending.vblank.context.0 != 0 {
            for buffer in state.buffers {
                if kumo_rt::device_vmar_unmap(
                    pending.vblank.context,
                    u64::from(buffer.address),
                    len,
                ) != 0
                {
                    error(format_args!(
                        "display: D1d retired unmap refused; backing retained\n"
                    ));
                    return None;
                }
            }
            log(format_args!("display: D1d both retired buffers unmapped by checked replacements; console domain retained\n"));
            if !holder_death_recovery(&mut pending, device) {
                error(format_args!(
                    "display: D1d holder-death recovery FAILED; ownership retained/blocked\n"
                ));
            } else if !fault_probes(&mut pending, device) {
                error(format_args!(
                    "display: D1d fault/recovery FAILED; owners retained/blocked\n"
                ));
            } else {
                return native_start(pending);
            }
        }
    } else {
        if !restored {
            latch::stop(device, &pending.program);
        }
        error(format_args!("display: D1c FAILED ({}): {} owned loads, console restored={}; buffers retained, interrupt disarmed{}\n",
            reason, state.pair.confirmed(), u8::from(restored),
            if restored { "" } else { "; VP2 in standby" }));
    }
    None
}

fn context_ready(ctx: Handle) -> bool {
    if ctx.0 == 0 {
        return true;
    }
    let mut info = kumo_abi::sys::DeviceCtxInfo {
        fault_record: 0,
        fault_addr: 0,
    };
    kumo_rt::device_ctx_info(ctx, &mut info) == 0
        && info.fault_record
            == kumo_abi::sys::device_ctx_state::RECORD | kumo_abi::sys::device_ctx_state::LIVE
}
fn recover_fault(pending: &mut Pending, device: &mut Device, front: u32) -> bool {
    let mut request = [b'F', 0, 0, 0, 0];
    request[1..].copy_from_slice(&front.to_le_bytes());
    if kumo_rt::channel_write_with_handle(
        pending.vblank.control,
        request.as_ptr(),
        request.len(),
        pending.vblank.context,
    ) != 0
    {
        return false;
    }
    let mut reply = [0u8; 1];
    let (n, ctx) = kumo_rt::channel_read_with_handle(pending.vblank.control, reply.as_mut_ptr(), 1);
    if n != 1 || reply[0] != b'R' || ctx == 0 {
        return false;
    }
    pending.vblank.context = Handle(ctx as u32);
    if !context_ready(pending.vblank.context) {
        return false;
    }
    let (gop, _) = scanout::target_scanout(&pending.program);
    scanout::flip(device, gop);
    scanout::wait_latched(device, kumo_rt::clock_get, scanout::LATCH_TIMEOUT_NS).is_some()
        && scanout::on_glass(device, gop)
}
fn recover_interrupted_d1c(pending: &mut Pending, device: &mut Device) {
    let (gop, _) = scanout::target_scanout(&pending.program);
    let state = pending.alternation.as_ref().expect("D1c state");
    let slot = state.pair.pending().unwrap_or(state.pair.front());
    let front = if slot == scanout::CONSOLE {
        gop
    } else {
        state.buffers[slot].address
    };
    close(pending.irq);
    let recovered = recover_fault(pending, device, front);
    error(format_args!(
        "display: D1c fault bypassed retirement; buffers retained, GOP recovery={}\n",
        u8::from(recovered)
    ));
}
/// The canary's physical address is deliberately an unmapped device address. A
/// bypass would read that allocation; a confined read must fault instead. — KESTREL
fn fault_probes(pending: &mut Pending, device: &mut Device) -> bool {
    use kumo_abi::sys::device_ctx_fault as f;
    let (_, stride) = scanout::target_scanout(&pending.program);
    let len =
        (u64::from(stride) * u64::from(DMT_1600X900_60_RB.v_active) * 4).next_multiple_of(4096);
    let (raw, phys) = kumo_rt::vmo_create_contiguous(len);
    let Some(canary) = syscall_handle(raw) else {
        return false;
    };
    let Some(address) = (scanout::Buffer {
        address: phys,
        len,
        stride,
        height: DMT_1600X900_60_RB.v_active,
    })
    .scanout() else {
        return false;
    };
    let (status, va) = kumo_rt::vmar_map_anywhere(
        canary,
        len,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    );
    if status != 0 || va == 0 {
        return false;
    }
    for i in 0..len / 4 {
        unsafe {
            ((va + i * 4) as *mut u32).write_volatile(0xc0ffee00 ^ (i as u32));
        }
    }
    device.barrier();
    for masked in [false, true] {
        let Some(port) = syscall_handle(kumo_rt::port_create()) else {
            return false;
        };
        if port_bind(port, pending.vblank.context) != 0 {
            return false;
        }
        let ctx_source = handle_koid(pending.vblank.context);
        let Some((timer, timeout)) = arm(port, 2000) else {
            return false;
        };
        let mut irq = Handle(0);
        let mut irq_source = 0;
        if masked {
            let Some(handle) =
                syscall_handle(kumo_rt::interrupt_create(pending.vop.0, rk3588::VOP_INTID))
            else {
                return false;
            };
            irq = handle;
            irq_source = handle_koid(irq);
            if port_bind(port, irq) != 0 {
                return false;
            }
            vblank::enable(device);
            let source = kumo_rt::port_wait(port);
            if source != irq_source {
                return false;
            }
            let fired = interrupt_wait(irq);
            if fired == 0
                || fired > 1024
                || device.read(Block::Vop, vblank::PORT2_INTR_STATUS) & vblank::FS == 0
            {
                return false;
            }
        }
        let before = pending.alternation.as_ref().unwrap().pair.retirements();
        let (gop, _) = scanout::target_scanout(&pending.program);
        let Some(mut probe) = scanout::Pair::new(
            [
                scanout::Buffer {
                    address: phys,
                    len,
                    stride,
                    height: DMT_1600X900_60_RB.v_active,
                },
                scanout::Buffer {
                    address: 0x61000000,
                    len,
                    stride,
                    height: DMT_1600X900_60_RB.v_active,
                },
            ],
            gop,
        ) else {
            return false;
        };
        if !probe.stage(device, 0) {
            return false;
        }
        loop {
            let source = kumo_rt::port_wait(port);
            if source == ctx_source {
                break;
            }
            if source == timeout || source == irq_source {
                return false;
            }
        }
        let mut found = false;
        for _ in 0..2 {
            let mut info = kumo_abi::sys::DeviceCtxInfo {
                fault_record: 0,
                fault_addr: 0,
            };
            if kumo_rt::device_ctx_info(pending.vblank.context, &mut info) != 0 {
                return false;
            }
            log(format_args!(
                "display: D1d fault snapshot record={:#x} address={:#x}\n",
                info.fault_record, info.fault_addr
            ));
            let raw = (info.fault_record >> f::IRQ_SHIFT) & 3;
            if raw & 1 != 0 {
                let status = info.fault_record as u32;
                if info.fault_record & (f::KERNEL_DISPLAY | f::SNAPSHOT_VALID | f::LINE_MASKED)
                    != (f::KERNEL_DISPLAY | f::SNAPSHOT_VALID | f::LINE_MASKED)
                    || raw != 1
                    || status & 0x22 != 2
                    || info.fault_addr < phys
                    || info.fault_addr >= phys + len
                {
                    return false;
                }
                log(format_args!("display: D1d {} READ fault bank {} master {} status {:#x} IOVA {:#x}; canary outside domain\n",if masked {"masked-completion"}else{"orphan"},(info.fault_record>>f::BANK_SHIFT)&3,(status>>6)&31,status,info.fault_addr));
                found = true;
            }
        }
        if !found {
            return false;
        }
        if kumo_rt::device_register_write(
            pending.vop.0,
            rk3588::VOP_BASE,
            kumo_abi::sys::DisplayRegister::Commit as u32,
            scanout::COMMIT_VP2,
        ) != kumo_abi::Errno::BadState.status()
        {
            return false;
        }
        if masked {
            if interrupt_complete(irq) != kumo_abi::Errno::BadState.status()
                || interrupt_complete(irq) != kumo_abi::Errno::BadState.status()
            {
                return false;
            }
            let Some((quiet, qsource)) = arm(port, 50) else {
                return false;
            };
            if kumo_rt::port_wait(port) != qsource {
                return false;
            }
            disarm(port, quiet);
            let _ = port_unbind(port, irq);
            close(irq);
        }
        let ready = context_ready(pending.vblank.context);
        let state = pending.alternation.as_mut().unwrap();
        if ready
            || probe.observe(device, ready).is_some()
            || probe.front() != scanout::CONSOLE
            || probe.pending() != Some(0)
            || probe.confirmed() != 0
            || state.pair.observe(device, ready).is_some()
            || state.pair.retirements() != before
        {
            return false;
        }
        disarm(port, timer);
        let _ = port_unbind(port, pending.vblank.context);
        close(port);
        for i in 0..len / 4 {
            if unsafe { ((va + i * 4) as *const u32).read_volatile() } != 0xc0ffee00 ^ (i as u32) {
                return false;
            }
        }
        if !recover_fault(pending, device, address) {
            return false;
        }
        if kumo_rt::device_vmar_unmap(pending.vblank.context, phys, len) != 0 {
            return false;
        }
        log(format_args!("display: D1d {} fault: canary unchanged, no retirement/stale write; {}approved GOP-only recovery, console latched, alias unmapped\n",if masked {"masked-completion"}else{"orphan"},if masked {"completion twice refused, no IRQ storm; "}else{""}));
    }
    for i in 0..len / 4 {
        if unsafe { ((va + i * 4) as *const u32).read_volatile() } != 0xc0ffee00 ^ (i as u32) {
            return false;
        }
    }
    close(canary);
    log(format_args!("display: D1d negative/final-holder/recovery gates complete; GOP-only console live, canary intact\n"));
    true
}

/// An actual final context holder dies while the owned front is active. Sora retains
/// only the recovery recipe; this process loses MAP authority during the handoff. — KESTREL
fn holder_death_recovery(pending: &mut Pending, device: &mut Device) -> bool {
    if pending.vblank.control.0 == 0 {
        return false;
    }
    let (gop, stride) = scanout::target_scanout(&pending.program);
    let len =
        (u64::from(stride) * u64::from(DMT_1600X900_60_RB.v_active) * 4).next_multiple_of(4096);
    if kumo_rt::device_vmar_map(
        pending.vblank.context,
        pending.owned.vmo,
        0,
        len,
        0x60000000,
        kumo_abi::Rights::READ,
    ) != 0
    {
        return false;
    }
    scanout::flip(device, 0x60000000);
    if scanout::wait_latched(device, kumo_rt::clock_get, scanout::LATCH_TIMEOUT_NS).is_none()
        || !scanout::on_glass(device, 0x60000000)
    {
        return false;
    }
    let sent = kumo_rt::channel_write_with_handle(
        pending.vblank.control,
        b"K".as_ptr(),
        1,
        pending.vblank.context,
    );
    if sent != 0 {
        return false;
    }
    let denied = kumo_rt::device_register_write(
        pending.vop.0,
        rk3588::VOP_BASE,
        kumo_abi::sys::DisplayRegister::Commit as u32,
        scanout::COMMIT_VP2,
    ) == kumo_abi::Errno::BadState.status();
    if !denied {
        return false;
    }
    log(format_args!(
        "display: D1d live owned front handed to sole holder; stale checked VOP write denied\n"
    ));
    let mut reply = [0u8; 1];
    let (n, ctx) = kumo_rt::channel_read_with_handle(pending.vblank.control, reply.as_mut_ptr(), 1);
    if n != 1 || reply[0] != b'R' || ctx == 0 {
        return false;
    }
    pending.vblank.context = Handle(ctx as u32);
    let mut info = kumo_abi::sys::DeviceCtxInfo {
        fault_record: 0,
        fault_addr: 0,
    };
    if kumo_rt::device_ctx_info(pending.vblank.context, &mut info) != 0
        || info.fault_record
            != kumo_abi::sys::device_ctx_state::RECORD | kumo_abi::sys::device_ctx_state::LIVE
    {
        return false;
    }
    scanout::flip(device, gop);
    if scanout::wait_latched(device, kumo_rt::clock_get, scanout::LATCH_TIMEOUT_NS).is_none()
        || !scanout::on_glass(device, gop)
    {
        return false;
    }
    log(format_args!("display: D1d holder killed during live DMA; approved GOP-only generation {} recovered and console latched\n",info.fault_addr));
    true
}

/// D1a: allocate, paint and flip to a KUMO-owned buffer. Returns whether it is on the glass;
/// on any failure the GOP console buffer is (re)latched before returning.
fn d1a_show(device: &mut Device, pending: &mut Pending) -> bool {
    let (gop, stride) = scanout::target_scanout(&pending.program);
    let width = DMT_1600X900_60_RB.h_active;
    let height = DMT_1600X900_60_RB.v_active;
    let len = (u64::from(stride) * u64::from(height) * 4).next_multiple_of(4096);
    let Some(buffer) = d1c_allocate(stride, height, pending.vblank.buffers[0], 0x60000000) else {
        error(format_args!(
            "display: D1a prepared buffer unavailable; console retained\n"
        ));
        return false;
    };
    let OwnedBuffer { vmo, address, va } = buffer;
    let painted = kumo_rt::clock_get();
    for y in 0..height {
        for x in 0..width {
            let (r, g, b) = scanout::pattern(x, y, width, height);
            let index = u64::from(y) * u64::from(stride) + u64::from(x);
            // SAFETY: index < stride * height <= len / 4 inside the uncached mapping.
            unsafe {
                ((va + index * 4) as *mut u32).write_volatile(pending.pixel_order.pixel(r, g, b))
            };
        }
    }
    device.barrier();
    let paint_ms = kumo_rt::clock_get().saturating_sub(painted) / 1_000_000;
    pending.owned = OwnedBuffer { vmo, address, va };
    log(format_args!(
        "display: D1a owned scanout buffer {:#x}, {} bytes uncached, KUMO test pattern painted in {} ms\n",
        address, len, paint_ms
    ));
    scanout::flip(device, address);
    match scanout::wait_latched(device, kumo_rt::clock_get, scanout::LATCH_TIMEOUT_NS) {
        Some(ns) if scanout::on_glass(device, address) => {
            log(format_args!(
                "display: D1a flipped to the owned buffer: VP2 consumed the load {} us after the commit (next frame start); GOP buffer {:#x} retired from scanout; holding {} ms\n",
                ns / 1_000,
                gop,
                scanout::HOLD_MS
            ));
            true
        }
        result => {
            error(format_args!(
                "display: D1a flip FAILED (latched={}, active={:#010x}); returning to the GOP buffer\n",
                u8::from(result.is_some()),
                device.read(Block::Vop, drv_fb::rk3588_mode::ESMART2_R0_MST_YRGB)
            ));
            scanout::flip(device, gop);
            let _ = scanout::wait_latched(device, kumo_rt::clock_get, scanout::LATCH_TIMEOUT_NS);
            false
        }
    }
}

/// D1a: flip back to the GOP console buffer and confirm it is latched.
fn d1a_return(device: &mut Device, pending: &Pending) -> bool {
    let (gop, _) = scanout::target_scanout(&pending.program);
    for attempt in 0..2 {
        scanout::flip(device, gop);
        let latched = scanout::wait_latched(device, kumo_rt::clock_get, scanout::LATCH_TIMEOUT_NS);
        if let Some(ns) = latched.filter(|_| scanout::on_glass(device, gop)) {
            log(format_args!(
                "display: D1a returned to the GOP console buffer {:#x}: load consumed {} us after the commit{}; owned buffer retired; D1a complete\n",
                gop,
                ns / 1_000,
                if attempt > 0 { " (after one retry)" } else { "" }
            ));
            return true;
        }
    }
    error(format_args!(
        "display: D1a FAILED to return to the GOP buffer; the owned buffer stays on the glass (handle {} kept)\n",
        pending.owned.vmo.0
    ));
    false
}

fn next(pending: Pending, phase: Phase, ms: u64) -> Option<Pending> {
    match arm(pending.port, ms) {
        Some((timer, source)) => Some(Pending {
            phase,
            timer,
            source,
            ..pending
        }),
        None => {
            latch::stop(&mut Device(pending.vop.0), &pending.program);
            error(format_args!(
                "display: R1b timer unavailable; HDMI0 off, VP2 in standby\n"
            ));
            None
        }
    }
}

fn report_mismatches(device: &mut Device, program: &Program, mask: u32) {
    let words = program.words.iter().enumerate().map(|(i, w)| (i as u32, w));
    let checks = program
        .checks
        .iter()
        .enumerate()
        .map(|(i, w)| (CHECK_BIT + i as u32, w));
    for (bit, word) in words.chain(checks) {
        if mask & (1 << bit) != 0 {
            log(format_args!(
                "display: R1 DISAGREEMENT: {:?}+{:#06x} live={:#010x} KUMO={:#010x}\n",
                word.block,
                word.offset,
                device.read(word.block, word.offset),
                word.value
            ));
        }
    }
}

fn finish(
    device: &mut Device,
    program: &Program,
    blanked: Blanked,
    started: u64,
    require_latch: bool,
) -> bool {
    let (retried, mismatched) = if require_latch {
        let observed = latch::observe(device, program);
        if !observed.matches() {
            latch::stop(device, program);
            error(format_args!(
                "display: R1b final program FAILED: mismatch mask {:#x}, load_pending={}; HDMI0 off, VP2 in standby\n",
                observed.mismatched, u8::from(observed.load_pending)
            ));
            return false;
        }
        (false, 0)
    } else {
        let Outcome::Rebuilt {
            retried,
            mismatched,
            ..
        } = verify(device, program, blanked)
        else {
            return false;
        };
        (retried, mismatched)
    };
    let elapsed_ms = kumo_rt::clock_get().saturating_sub(started) / 1_000_000;
    if mismatched == 0 {
        log(format_args!(
            "display: R1 rebuilt from KUMO's table, {} ms blank to verify{}: all {} written words and {} checks read back equal\n",
            elapsed_ms,
            if retried { " (after one retry)" } else { "" },
            WORDS,
            CHECKS
        ));
    } else {
        error(format_args!(
            "display: R1 FAILED: {} word(s) still differ after a retry; VP2 left out of standby\n",
            mismatched.count_ones()
        ));
        report_mismatches(device, program, mismatched);
    }
    let monitor =
        [0x884, 0x888, 0x88c, 0x890, 0x894, 0x898].map(|offset| device.read(Block::Tx, offset));
    let measured = Tx {
        core_id: 0,
        version: 0,
        video_status: 0,
        frame_composer: 0,
        monitor,
        scrambler: 0,
        link_config: 0,
    }
    .timing();
    match measured {
        Some(timing) if timing == D0A_TIMING => log(format_args!(
            "display: R1 HDMI0 TX measures KUMO's mode: {}x{} H total {} V total {}\n",
            timing.h_active, timing.v_active, timing.h_total, timing.v_total
        )),
        Some(timing) => error(format_args!(
            "display: R1 DISAGREEMENT: TX measures {}x{} H total {} V total {} after the rebuild\n",
            timing.h_active, timing.v_active, timing.h_total, timing.v_total
        )),
        None => error(format_args!(
            "display: R1 TX timing unreadable after the rebuild: {:#010x} {:#010x} {:#010x} {:#010x} {:#010x} {:#010x}\n",
            monitor[0], monitor[1], monitor[2], monitor[3], monitor[4], monitor[5]
        )),
    }
    let restored = mismatched == 0 && measured == Some(D0A_TIMING);
    if require_latch && !restored {
        latch::stop(device, program);
        error(format_args!(
            "display: R1b final timing FAILED; HDMI0 off, VP2 in standby\n"
        ));
    }
    restored
}

fn native_start(mut pending: Pending) -> Option<Pending> {
    let channel = pending.vblank.native;
    if channel.0 == 0 {
        return None;
    }
    if pending.pixel_order != PixelOrder::Bgr
        || !context_ready(pending.vblank.context)
        || port_bind(pending.port, channel) != 0
    {
        return None;
    }
    pending.native_source = handle_koid(channel);
    log(format_args!(
        "display: J0c waiting for completed GPU image sequence{}; console remains live\n",
        pending.native_lease.retired() + 1
    ));
    next(pending, Phase::NativeWait, 10_000)
}
fn native_present(mut pending: Pending) -> Option<Pending> {
    use kumo_graphics::native::{Image, IMAGE_IOVA, REQUEST_LEN};
    disarm(pending.port, pending.timer);
    let channel = pending.vblank.native;
    let _ = port_unbind(pending.port, channel);
    let mut bytes = [0; REQUEST_LEN + 1];
    let (n, raw) = kumo_rt::channel_read_with_handle(channel, bytes.as_mut_ptr(), bytes.len());
    let grant = syscall_handle(raw);
    // G8c-b2: a frame-retired wait registers on the same channel and precedes its image
    // frame by construction, so the arm is read first. A wait for the next point arms a
    // wake pair; a wait for an already-proven point is answered unarmed (done). Any other
    // point, or an arm failure, refuses the frame and ends the bounded presentation.
    // — TIMBERDOODLE
    if let Some(wait) = bytes
        .get(..n)
        .and_then(kumo_graphics::native::RetireWait::decode)
    {
        if let Some(handle) = grant {
            close(handle);
            return None;
        }
        if wait.point <= pending.native_retired {
            let reply = kumo_graphics::native::WaitReply {
                point: wait.point,
                armed: false,
            };
            let frame = reply.encode();
            if kumo_rt::channel_write_with_handle(channel, frame.as_ptr(), frame.len(), Handle(0))
                != 0
            {
                return None;
            }
            log(format_args!(
                "display: retire wait already retired point {}\n",
                wait.point
            ));
            // Rebind the channel watch before re-entering the wait: this function unbinds
            // it on entry, and the queued image frame must be able to wake the port
            // (native_start's arm-before-release rule; J683's first metal run timed out
            // here). — TIMBERDOODLE
            if port_bind(pending.port, channel) != 0 {
                return None;
            }
            return next(pending, Phase::NativeWait, 10_000);
        }
        if wait.point != pending.native_retired + 1
            || wait.point > u64::from(kumo_graphics::native::PRESENTATIONS)
        {
            return None;
        }
        let Some(free) = pending.native_waits.iter().position(|slot| slot.is_none()) else {
            return None;
        };
        let (read_raw, write_raw) = kumo_rt::channel_create_pair();
        let (Some(read), Some(write)) = (syscall_handle(read_raw), syscall_handle(write_raw))
        else {
            if let Some(read) = syscall_handle(read_raw) {
                close(read);
            }
            if let Some(write) = syscall_handle(write_raw) {
                close(write);
            }
            return None;
        };
        pending.native_waits[free] = Some((wait.point, write));
        let reply = kumo_graphics::native::WaitReply {
            point: wait.point,
            armed: true,
        };
        let frame = reply.encode();
        if kumo_rt::channel_write_with_handle(channel, frame.as_ptr(), frame.len(), read) != 0 {
            close(read);
            return None;
        }
        log(format_args!(
            "display: retire wait armed point {}\n",
            wait.point
        ));
        // Same rule for the armed path: the watch must exist before the queued image
        // frame can wake the port. — TIMBERDOODLE
        if port_bind(pending.port, channel) != 0 {
            return None;
        }
        return next(pending, Phase::NativeWait, 10_000);
    }
    let Some(vmo) = grant else {
        return None;
    };
    let image = bytes.get(..n).and_then(Image::decode);
    let Some(image) = image.filter(|_| context_ready(pending.vblank.context)) else {
        close(vmo);
        return None;
    };
    let koid = handle_koid(vmo);
    if !pending.native_lease.begin(image, koid) {
        error(format_args!(
            "display: J0c stale/overlapping/substituted image refused before mapping\n"
        ));
        close(vmo);
        return None;
    }
    let Some(program) = scanout::image_program(&pending.program, image) else {
        close(vmo);
        return None;
    };
    // Lease admission proves the same actual VMO/span on every presentation, so
    // one retained READ-only CPU view suffices to verify all three GPU outputs. — KESTREL
    if pending.native_cpu == 0 {
        let (status, cpu) = kumo_rt::vmar_map_anywhere(
            vmo,
            image.vmo_bytes,
            (VmarFlags::READ | VmarFlags::UNCACHED).0,
        );
        if status != 0 || cpu == 0 {
            close(vmo);
            return None;
        }
        pending.native_cpu = cpu;
    }
    let cpu = pending.native_cpu;
    // SAFETY: the validated image lies wholly in the actual READ-only mapping. This
    // volatile check reads the original pages; there is no CPU copy into scanout. — KESTREL
    let checksum =
        image.checksum(|n| unsafe { ((cpu + image.offset + n * 4) as *const u32).read_volatile() });
    if checksum != image.checksum {
        error(format_args!(
            "display: J0a shared image checksum mismatch; nothing staged\n"
        ));
        close(vmo);
        return None;
    }
    // Even an error may leave a candidate publication retained. Never turn
    // that uncertainty into the no-image cancellation receipt. — KESTREL
    pending.native_channel.cancel_safe = false;
    let status = kumo_rt::device_vmar_map(
        pending.vblank.context,
        vmo,
        image.offset,
        image.bytes(),
        IMAGE_IOVA,
        Rights::READ,
    );
    if status != 0 {
        close(vmo);
        return None;
    }
    // From publication onward failures retain the handle, mapping and lease. No Drop
    // cleanup can fabricate retirement or make possibly reachable pages reusable. — KESTREL
    pending.native_frame = Some(NativeFrame { image, vmo });
    let mut device = Device(pending.vop.0);
    scanout::stage_region(&mut device, &program);
    let latched = scanout::wait_latched(&mut device, kumo_rt::clock_get, scanout::LATCH_TIMEOUT_NS);
    if latched.is_none()
        || !scanout::region_loaded(&mut device, &program, context_ready(pending.vblank.context))
    {
        error(format_args!(
            "display: J0a image load uncertain; owners retained, no retired reply\n"
        ));
        return None;
    }
    log(format_args!("display: J0c sequence{} point{} ZERO-COPY: VMO{} offset{:#x}, display IOVA{:#x}, checksum{:#x}; complete region/load consumed, holding5000ms\n",image.sequence,image.gpu_point,koid,image.offset,IMAGE_IOVA,checksum));
    next(pending, Phase::NativeHold, 5_000)
}
fn native_retire(mut pending: Pending, device: &mut Device) -> Option<Pending> {
    use kumo_graphics::native::{Reply, IMAGE_IOVA};
    let frame = pending.native_frame?;
    if !context_ready(pending.vblank.context) {
        error(format_args!(
            "display: J0a fault prevents retirement; owners retained\n"
        ));
        return None;
    }
    scanout::stage_region(device, &pending.program);
    let loaded = scanout::wait_latched(device, kumo_rt::clock_get, scanout::LATCH_TIMEOUT_NS)
        .is_some()
        && scanout::region_loaded(
            device,
            &pending.program,
            context_ready(pending.vblank.context),
        );
    if !loaded
        || kumo_rt::device_vmar_unmap(pending.vblank.context, IMAGE_IOVA, frame.image.bytes()) != 0
    {
        error(format_args!(
            "display: J0a console/unmap uncertain; owners retained, no retired reply\n"
        ));
        return None;
    }
    let reply = Reply {
        sequence: frame.image.sequence,
        gpu_point: frame.image.gpu_point,
        retired: true,
    };
    if !pending.native_lease.retire(reply) {
        return None;
    }
    close(frame.vmo);
    pending.native_frame = None;
    pending.native_channel.cancel_safe = true;
    let channel = pending.vblank.native;
    let complete = pending.native_lease.complete();
    // Arm the next bounded wait before releasing the producer, so an immediate
    // next render/transfer cannot race a missing channel watch. — KESTREL
    let mut pending = if complete {
        pending
    } else {
        native_start(pending)?
    };
    let bytes = reply.encode();
    let status = kumo_rt::channel_write(channel, bytes.as_ptr(), bytes.len());
    log(format_args!("display: J0c sequence{} point{} retired: complete GOP geometry/load consumed, checked image unmap finished; reply={}\n",reply.sequence,reply.gpu_point,status));
    if status != 0 {
        if !complete {
            disarm(pending.port, pending.timer);
            let _ = port_unbind(pending.port, channel);
        }
        return None;
    }
    // G8c-b2: the consumed load is proven for this sequence, so every armed wait at or
    // below it is satisfied. The byte is the frame-retired wake; the reply above is the
    // same fact on the cancellation channel. — TIMBERDOODLE
    pending.native_retired = u64::from(reply.sequence);
    for slot in &mut pending.native_waits {
        let due = match slot {
            Some((point, _)) => *point <= pending.native_retired,
            None => false,
        };
        if due {
            if let Some((point, handle)) = slot.take() {
                let byte = [1u8];
                let _ = kumo_rt::channel_write(handle, byte.as_ptr(), 1);
                close(handle);
                log(format_args!("display: retire wait woken point {}\n", point));
            }
        }
    }
    if complete {
        pending.native_channel.finished = true;
        DIAGNOSTIC_COMPLETED.store(true, Ordering::Relaxed);
        log(format_args!(
            "display: J0c complete: 3 original-VMO presentations retired, GOP-only console live\n"
        ));
        None
    } else {
        Some(pending)
    }
}
