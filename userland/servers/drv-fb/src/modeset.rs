//j607
//j608

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

use core::fmt::{self, Write};
use drv_fb::probe_va::{TX_R1, VOP_R1};
use drv_fb::rk3588::{self, Power};
use drv_fb::rk3588_hdmi::{self, Tx, D0A_TIMING};
use drv_fb::rk3588_latch as latch;
use drv_fb::rk3588_mode::{
    blank, rebuild, verify, Blanked, Block, Framebuffer, Hardware, Outcome, Program, CHECKS,
    CHECK_BIT, DMT_1600X900_60_RB, PLAN, RECORDED, REG_CFG_DONE, TX_WINDOW_LEN, VOP_WINDOW_LEN,
    WORDS,
};
use kumo_abi::{BootInfo, Handle, VmarFlags};
use kumo_rt::{
    handle_close, handle_koid, port_bind, port_unbind, resource_mint_mmio, syscall_handle,
    timer_create, vmar_map,
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
/// and arm the dwell timer on `port`. Both handles are closed on every path. Returns the
/// work left for [`advance`].
pub fn start(
    boot: &BootInfo,
    power: Option<Power>,
    r0_matched: bool,
    vop: Handle,
    tx: Handle,
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
                    pending = begin(boot, vop, tx, port);
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
    for handle in [vop, tx] {
        if handle.0 != 0 {
            let _ = handle_close(handle);
        }
    }
    pending
}

fn begin(boot: &BootInfo, vop: Handle, tx: Handle, port: Handle) -> Option<Pending> {
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
    Some(Pending {
        program,
        blanked,
        phase: Phase::Dwell,
        port,
        timer,
        source,
        started,
        proof_verified: false,
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
            }
            None
        }
    }
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
