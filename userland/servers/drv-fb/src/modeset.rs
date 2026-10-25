//j607
//j608
//j609
//j610
//j611

//! R1 live adapter: writable mappings of the VOP2 registers (`0xfdd90000` + `0x7000`, not the
//! MMU page) and the HDMI0 TX page, granted by Sora as device windows (DESIGN/020 D0-2). It
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

use crate::phy_relock;
use core::fmt::{self, Write};
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
use kumo_abi::{BootInfo, FramebufferFormat, Handle, VmarFlags};
use kumo_rt::{
    handle_close, handle_koid, interrupt_complete, interrupt_create, interrupt_wait, port_bind,
    port_unbind, resource_mint_mmio, syscall_handle, timer_create, vmar_map,
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

struct Device;

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
        unsafe { core::ptr::write_volatile(Self::address(block, offset) as *mut u32, value) }
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

/// Mint and map one device window. The VOP2 grant keeps `WRITE` because it is the display
/// holder's own device (DESIGN/020 D0-2); the TX page is mapped read-only.
fn map(resource: Handle, base: u64, len: u64, va: u64, writable: bool, label: &str) -> bool {
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
    let flags = if writable {
        VmarFlags::READ | VmarFlags::WRITE | VmarFlags::DEVICE
    } else {
        VmarFlags::READ | VmarFlags::DEVICE
    };
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
}

/// What D1b needs from the earlier probes.
#[derive(Clone, Copy)]
pub struct Vblank {
    /// The live DTB's VOP interrupt as D0b decoded it, or 0.
    pub live_irq: u32,
    /// Whether D0a's read-only VOP window (with the MMU page) is mapped at `VOP_D0A`.
    pub mmu_view: bool,
}

/// Own the grant across timer transitions, including failures before D1b. — KESTREL
struct VopResource(Handle);

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
    owned: Handle,
    /// The VOP window Resource, kept until D1b creates its interrupt from it.
    vop: VopResource,
    vblank: Vblank,
    /// D1b's interrupt object and its koid, once armed.
    irq: Handle,
    irq_source: u64,
    meter: vblank::Meter,
    intervals: Option<&'static mut [u64; vblank::FRAMES as usize - 1]>,
}

impl Pending {
    /// Whether a port wake belongs to R1-D1b: its timer, or D1b's interrupt.
    pub fn owns(&self, source: Handle) -> bool {
        Handle(self.source as u32) == source
            || (self.irq.0 != 0 && Handle(self.irq_source as u32) == source)
    }
}

/// Route a wake that [`Pending::owns`]: D1b's interrupt, or the phase timer.
pub fn wake(pending: Pending, source: Handle) -> Option<Pending> {
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
    if !map(vop, rk3588::VOP_BASE, VOP_WINDOW_LEN, VOP_R1, true, "VOP2")
        || !map(
            tx,
            rk3588_hdmi::HDMI_BASE,
            TX_WINDOW_LEN,
            TX_R1,
            false,
            "HDMI0 TX",
        )
    {
        return None;
    }
    let mut device = Device;
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
        owned: Handle(0),
        vop: VopResource(vop),
        vblank,
        irq: Handle(0),
        irq_source: 0,
        meter: vblank::Meter::default(),
        intervals: None,
    })
}

/// Handle the timer `port_wait` reported as `pending.source`. Returns the next step, if any.
pub fn advance(mut pending: Pending) -> Option<Pending> {
    disarm(pending.port, pending.timer);
    let mut device = Device;
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
            if !latch::observe(&mut device, &latch::blacked(&pending.program)).matches() {
                latch::stop(&mut device, &pending.program);
                error(format_args!(
                    "display: R2 black program did not latch; no PHY reset, VP2 in standby\n"
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
            d1b_finish(pending, &mut device, true);
            None
        }
    }
}

fn close(handle: Handle) {
    if handle.0 != 0 {
        let _ = handle_close(handle);
    }
}

/// D1b: arm the VOP interrupt with VP2's frame start as its only source and count frames.
fn d1b_start(mut pending: Pending, device: &mut Device) -> Option<Pending> {
    let intid = rk3588::VOP_INTID;
    if pending.vblank.live_irq != intid {
        log(format_args!(
            "display: D1b skipped: live DTB VOP interrupt {} is not the granted INTID {}\n",
            pending.vblank.live_irq, intid
        ));
        return None;
    }
    if !pending.vblank.mmu_view {
        log(format_args!(
            "display: D1b skipped: no read-only view of the display IOMMU's interrupt registers\n"
        ));
        return None;
    }
    // SAFETY: D0a mapped VOP_LEN (0x8000) bytes read-only at VOP_D0A; every offset read is
    // inside the MMU page below that length.
    let mmu = |offset: u32| unsafe {
        assert!(u64::from(offset) + 4 <= rk3588::VOP_LEN);
        core::ptr::read_volatile((VOP_D0A + u64::from(offset)) as *const u32)
    };
    if let Err(conflict) = vblank::preflight(device, mmu) {
        log(format_args!(
            "display: D1b refused, nothing armed: another source could drive the shared VOP line ({:?})\n",
            conflict
        ));
        return None;
    }
    let Some(intervals) = take_samples() else {
        error(format_args!(
            "display: D1b sample buffer already claimed; nothing armed\n"
        ));
        return None;
    };
    let raw = interrupt_create(pending.vop.0, intid);
    let Some(irq) = syscall_handle(raw) else {
        error(format_args!(
            "display: D1b interrupt {} unavailable (errno {})\n",
            intid, raw as u32 as i32
        ));
        return None;
    };
    let koid = handle_koid(irq);
    if koid == u64::MAX || port_bind(pending.port, irq) != 0 {
        error(format_args!(
            "display: D1b could not bind interrupt {}\n",
            intid
        ));
        close(irq);
        return None;
    }
    let Some((timer, source)) = arm(pending.port, vblank::WATCHDOG_MS) else {
        let _ = port_unbind(pending.port, irq);
        close(irq);
        error(format_args!(
            "display: D1b watchdog timer unavailable; disarmed\n"
        ));
        return None;
    };
    // A stale GIC delivery can race InterruptCreate's binding. Complete it while every
    // device source is still off, then enable only after both port bindings exist. — KESTREL
    let _ = interrupt_complete(irq);
    vblank::enable(device);
    // No console log between enable and finish: rendering it could coalesce frame starts
    // while this same event loop is scrolling the console. — KESTREL
    pending.irq = irq;
    pending.irq_source = koid;
    pending.meter = vblank::Meter::default();
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
    let mut device = Device;
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
        d1b_finish(pending, &mut device, false);
        return None;
    }
    let _ = interrupt_complete(pending.irq);
    Some(pending)
}

fn d1b_finish(mut pending: Pending, device: &mut Device, timed_out: bool) {
    vblank::disable(device);
    let _ = vblank::acknowledge(device);
    let _ = interrupt_complete(pending.irq);
    let _ = port_unbind(pending.port, pending.irq);
    close(pending.irq);
    let meter = pending.meter;
    let mode_mhz = vblank::mode_millihertz(&DMT_1600X900_60_RB);
    let delivered = meter.millihertz().unwrap_or(0);
    let cadence = pending.intervals.as_mut().and_then(|samples| {
        vblank::cadence(&mut samples[..meter.count.saturating_sub(1) as usize])
    });
    match cadence {
        Some(rate) if !timed_out && meter.done() && rate.credible() && vblank::matches(rate.millihertz, mode_mhz) => log(format_args!(
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
}

/// D1a: allocate, paint and flip to a KUMO-owned buffer. Returns whether it is on the glass;
/// on any failure the GOP console buffer is (re)latched before returning.
fn d1a_show(device: &mut Device, pending: &mut Pending) -> bool {
    let (gop, stride) = scanout::target_scanout(&pending.program);
    let width = DMT_1600X900_60_RB.h_active;
    let height = DMT_1600X900_60_RB.v_active;
    let len = (u64::from(stride) * u64::from(height) * 4).next_multiple_of(4096);
    let (raw, phys) = kumo_rt::vmo_create_contiguous(len);
    let Some(vmo) = syscall_handle(raw) else {
        error(format_args!(
            "display: D1a contiguous buffer of {} bytes unavailable; console stays on the GOP buffer\n",
            len
        ));
        return false;
    };
    let buffer = scanout::Buffer {
        phys,
        len,
        stride,
        height,
    };
    let Some(address) = buffer.scanout() else {
        error(format_args!(
            "display: D1a buffer at {:#x} is not VOP-addressable; console stays on the GOP buffer\n",
            phys
        ));
        let _ = handle_close(vmo);
        return false;
    };
    let flags = VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED;
    let (status, va) = kumo_rt::vmar_map_anywhere(vmo, len, flags.0);
    if status != 0 || va == 0 {
        error(format_args!(
            "display: D1a buffer mapping failed; console stays on the GOP buffer\n"
        ));
        let _ = handle_close(vmo);
        return false;
    }
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
    pending.owned = vmo;
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
        pending.owned.0
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
            latch::stop(&mut Device, &pending.program);
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
