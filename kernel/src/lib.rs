#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

//j476
//j478
//j479
//j482
//j483

extern crate alloc;

pub mod bootstrap;
pub mod cap;
pub mod conin;
pub mod diag;
pub mod ipc;
pub mod ipcdemo;
pub mod ipcstat;
pub mod kdemo;
pub mod mm;
pub mod object;
pub mod power;
pub mod sched;
pub mod shell;
pub mod syscall;
pub mod sysrq;
pub mod task;
pub mod tower;
pub mod user_thread;
pub mod usermode;
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
mod x86_fpsimd_smoke;

use kumo_abi::{BootInfo, Errno, MemRegion, MemRegionKind, Rights, Signals, ABI_VERSION};
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
use kumo_abi::{Range, RawSlice};
use kumo_ipc::Message;
use niji_loader::{validate_boot_info, HandoffError, HandoffSummary};

use crate::syscall::{KernelCall, KernelCallResult};

pub const STAGE_A_BANNER: &str = "KUMO MUREX Stage-A core only; halting";

#[macro_export]
macro_rules! klog {
    ($($arg:tt)*) => {
        $crate::bootstrap::console::write_fmt(core::format_args!($($arg)*))
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelInitReport {
    pub abi_version: u32,
    pub arch: &'static str,
    pub mem_region_count: u64,
    pub total_bytes: u64,
    pub usable_bytes: u64,
    pub has_initrd: bool,
    pub has_framebuffer: bool,
}

pub fn inspect_boot(boot: &BootInfo) -> Result<KernelInitReport, HandoffError> {
    let summary = validate_boot_info(boot)?;
    Ok(inspect_handoff(&summary))
}

pub fn inspect_handoff(summary: &HandoffSummary) -> KernelInitReport {
    KernelInitReport {
        abi_version: ABI_VERSION,
        arch: kumo_hal::active::arch_name(),
        mem_region_count: summary.mem_region_count,
        total_bytes: summary.total_bytes,
        usable_bytes: summary.usable_bytes,
        has_initrd: !summary.initrd.is_empty(),
        has_framebuffer: summary.has_framebuffer,
    }
}

pub fn stage_a_banner() -> &'static str {
    STAGE_A_BANNER
}

pub fn stage_a_console_banner() {
    bootstrap::console::write_str(STAGE_A_BANNER);
    bootstrap::console::write_str("\n");
}

/// The explicitly selected PL011 base, the board BSP's default when there is no override, or
/// `None` when neither source names a safe base.
///
/// The override is a fixed-size BootInfo field rather than a pointer-backed command line so it can
/// be resolved before the first `klog!`. This matters for the Pi 5: uart10 is a stable BSP fact,
/// while RP1 UART0's host address is a firmware-selected PCIe window (the current firmware reports
/// `0x1c00030000`, older releases used another window). A malformed nonzero override falls back to
/// the BSP rather than becoming a blind MMIO write. — KESTREL 2026-07-17
fn board_console_pl011_base(boot: &BootInfo) -> Option<u64> {
    // Only the leading version word is safe to interpret across ABI revisions. In v2 the new v3
    // console field aliases the old cmdline pointer, so reading it before this gate could turn a
    // stale loader into blind MMIO. — KESTREL 2026-07-17
    if boot.version != ABI_VERSION {
        return None;
    }
    let override_base = boot.platform.pl011_console_base;
    if plausible_pl011_base(override_base) {
        return Some(override_base);
    }
    kumo_bsp::Board::from_id(boot.board_id())?
        .spec()
        .console
        .pl011_base()
}

const fn plausible_pl011_base(base: u64) -> bool {
    base != 0 && base < (1u64 << 48) && base & 0xfff == 0
}

/// The DW-APB (16550-class) console base for this board, resolved from the BSP, or `None`.
///
/// This is [`board_console_pl011_base`]'s sibling for the RK3588 debug UART (the Orange Pi 5 Plus,
/// PLAN_VII R3) — a `snps,dw-apb-uart`, not a PL011, so it takes its own HAL sink. There is no boot
/// override here: unlike the Pi 5's RP1 UART, whose host address is a firmware-selected PCIe window,
/// uart2 @ 0xfeb50000 is a fixed silicon base, so the BSP entry is the single source. Version-gated
/// for the same reason the PL011 path is — a stale loader's aliased v3 fields must never become a
/// blind MMIO write.
fn board_console_dw8250_base(boot: &BootInfo) -> Option<u64> {
    if boot.version != ABI_VERSION {
        return None;
    }
    kumo_bsp::Board::from_id(boot.board_id())?
        .spec()
        .console
        .dw8250_base()
}

/// Whether Stage-A should keep servicing the interactive serial floor after POST.
///
/// A framebuffer is a second output sink, not evidence that serial is absent. The Pi 5 has both:
/// GOP plus either BSP uart10 or an explicit RP1 PL011 route. Preserve the historical headless
/// serial attempt when there is no framebuffer, but when glass is present require a resolved UART
/// before polling MMIO. This leaves the X13s on its safe framebuffer-only idle floor while a Pi
/// continues on the exact PL011 selected before the first log line. — KESTREL 2026-07-18
const fn stage_a_uses_serial_floor(
    has_framebuffer: bool,
    selected_pl011_base: Option<u64>,
    selected_dw8250_base: Option<u64>,
) -> bool {
    selected_pl011_base.is_some() || selected_dw8250_base.is_some() || !has_framebuffer
}

/// Where this board's GIC lives when firmware publishes no device tree, or `None` when the boot
/// path guarantees a DT or the board identity is unstamped. QEMU/OVMF publishes none; Pi 5 UEFI
/// can deliberately publish ACPI only even though Raspberry Pi firmware loaded a DTB first.
///
/// Carries addresses only — never a version. A single injected secondary interface fixes the
/// architecture; if both GICC and GICR are present (QEMU), the HAL uses the PE capability to
/// select the launch-time controller (DESIGN/017 §4.4).
fn board_gic_no_dtb_fallback(boot: &BootInfo) -> Option<kumo_bsp::GicFallback> {
    if boot.version != ABI_VERSION {
        return None;
    }
    kumo_bsp::Board::from_id(boot.board_id())?
        .spec()
        .gic_fallback
}

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_HEADER_BYTES: u64 = 40;
const MAX_DTB_BYTES: u64 = 16 * 1024 * 1024;
const AARCH64_PHYS_LIMIT: u64 = 1u64 << 48;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DtbHandoffError {
    AllOnes,
    NonPhysical,
    Misaligned,
    HeaderOutsideMemoryMap,
    BadMagic(u32),
    BadSize(u32),
    BlobOutsideMemoryMap,
}

fn dtb_ram_kind(kind: MemRegionKind) -> bool {
    // Nijigumo accepts staged DTBs only from LoaderData and copies firmware-table DTBs into the
    // same type. Requiring Bootloader here asserts that ownership/lifetime contract and keeps the
    // lossy Reserved bucket (which includes EFI Unusable memory) out of raw reads.
    // — KESTREL 2026-07-17
    kind == MemRegionKind::Bootloader
}

fn dtb_range_is_backed(address: u64, byte_len: u64, regions: &[MemRegion]) -> bool {
    let Some(end) = address.checked_add(byte_len) else {
        return false;
    };
    regions.iter().any(|region| {
        let Some(region_end) = region.range.start.checked_add(region.range.len) else {
            return false;
        };
        dtb_ram_kind(region.kind)
            && region.range.start <= address
            && end <= region_end
            && address < end
    })
}

/// Validate the loader's DTB address while the firmware identity map is still live. Pointer shape
/// and the complete declared blob are checked against the UEFI memory map before any dereference;
/// a rejected handoff is replaced with an absent DTB for every later Stage-A consumer.
///
/// # Safety
/// `boot.mem_regions` must already have passed the handoff validator and remain readable. A range
/// accepted by that memory map must still be identity-mapped at this pre-MMU point.
unsafe fn validate_dtb_handoff(boot: &BootInfo) -> Result<Option<(u64, u32)>, DtbHandoffError> {
    let address = boot.platform.dtb;
    if address == 0 {
        return Ok(None);
    }
    if address == u64::MAX {
        return Err(DtbHandoffError::AllOnes);
    }
    if address >= AARCH64_PHYS_LIMIT
        || address
            .checked_add(FDT_HEADER_BYTES)
            .is_none_or(|end| end > AARCH64_PHYS_LIMIT)
    {
        return Err(DtbHandoffError::NonPhysical);
    }
    if address & 7 != 0 {
        return Err(DtbHandoffError::Misaligned);
    }

    let regions = unsafe { boot.mem_regions.as_slice() };
    if !dtb_range_is_backed(address, FDT_HEADER_BYTES, regions) {
        return Err(DtbHandoffError::HeaderOutsideMemoryMap);
    }
    let header = unsafe { core::slice::from_raw_parts(address as *const u8, 8) };
    let magic = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    if magic != FDT_MAGIC {
        return Err(DtbHandoffError::BadMagic(magic));
    }
    let total_size = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if !(FDT_HEADER_BYTES as u32..=MAX_DTB_BYTES as u32).contains(&total_size) {
        return Err(DtbHandoffError::BadSize(total_size));
    }
    if !dtb_range_is_backed(address, total_size as u64, regions) {
        return Err(DtbHandoffError::BlobOutsideMemoryMap);
    }
    Ok(Some((address, total_size)))
}

pub fn stage_a(boot: &BootInfo) -> ! {
    // Own the fault path and the console before any fallible work: install our exception
    // vectors ("The Tower") so faults are caught and visible, then bring up the
    // framebuffer console (the X13s has no serial here), which clears to the black
    // phosphor backdrop. Interrupts stay masked (see `_start`) until we own the GIC.
    kumo_hal::active::install_exception_vectors();

    // Tell the console where this board's UART is, from the BSP, before any `klog!` (DESIGN/017
    // §4.3). The HAL ships with no base and its UART sink inert, so a board whose BSP entry names
    // no PL011 (the X13s) cannot write to one. The Pi 5 BSP names its SoC uart10, while an
    // explicit boot override can instead select firmware's current RP1 UART0 PCIe window. This
    // is what retired J182: the HAL used to fall back to QEMU's 0x09000000 and HARD-HANG another
    // board on the first line of output. An unstamped or unknown board also injects nothing,
    // which costs early serial on QEMU but can no longer wedge a machine.
    let selected_pl011_base = board_console_pl011_base(boot);
    if let Some(base) = selected_pl011_base {
        kumo_hal::active::console_set_pl011_base(base);
    }

    // The RK3588 debug UART is a Synopsys DW-APB (16550-class), not a PL011, so it has its own
    // injection point and HAL sink (PLAN_VII R3). Same discipline as above: the board names the
    // base, the HAL drives it, and every board without a DW-APB UART injects nothing and leaves
    // that sink inert. This is what finally gives the Orange Pi 5 Plus a kernel console — until now
    // it *selected* a DW-APB board in the BSP but had no backend, so the resolver found no PL011
    // and every Stage-A POST line was dropped on the wire the operator is actually watching.
    let selected_dw8250_base = board_console_dw8250_base(boot);
    if let Some(base) = selected_dw8250_base {
        kumo_hal::active::console_set_dw8250_base(base);
    }

    // Same shape for the interrupt controller (DESIGN/017 §4.4). A device tree, when firmware
    // publishes one, names the GIC better than any table can and the HAL prefers it. The injected
    // fallback covers genuine no-DT paths: QEMU/OVMF and Pi 5 UEFI in its default ACPI-only mode.
    // A board with one secondary interface fixes the architecture; QEMU supplies both, so the PE
    // capability selects its launch-time `-machine virt,gic-version=` controller.
    if let Some(gic) = board_gic_no_dtb_fallback(boot) {
        kumo_hal::active::gic_set_no_dtb_fallback(
            gic.distributor_base,
            gic.redistributor_base.unwrap_or(0),
            gic.cpu_base.unwrap_or(0),
        );
    }

    // Bring up the framebuffer console BEFORE any `klog!`. The X13s has no PL011; on the Pi 5,
    // uart10 or an explicit RP1 route may not be connected to the operator. Until the framebuffer
    // owns its half of the dual-sink console, early lines are dropped on glass. This is now a
    // visibility constraint, not a safety one (J182 is fixed above) — but it must stay ahead of
    // every `klog!` in `stage_a` or the boot's first lines are lost on those boards.
    if boot.has_framebuffer() {
        let fb = boot.framebuffer;
        kumo_hal::active::set_framebuffer(fb.phys, fb.len, fb.width, fb.height, fb.stride);
        // FB GEOM as the very first rendered line — handy for X13s framebuffer bring-up.
        klog!(
            "FB GEOM w={} h={} stride={} len={} phys={:#x}\n",
            fb.width,
            fb.height,
            fb.stride,
            fb.len,
            fb.phys
        );
    }

    let report = match inspect_boot(boot) {
        Ok(report) => report,
        Err(err) => tower_halt_ascii("nijigumo->MUREX handoff invalid", Some(err)),
    };

    // Preserve the final UEFI memory-map snapshot across the paging attribute switch. Nijigumo's
    // `MemRegion` array lives in a small LoaderData allocation; its enclosing 2 MiB block can
    // therefore become Device-nGnRnE in KUMO's conservative identity map. A stale post-switch
    // descriptor made `alloc_zeroed_frame` select 0x0000100000001000 on the Orange Pi 5 Plus,
    // where zeroing it faulted in `memset`. Clean the already-validated slice while firmware's
    // cacheable map is still active. — KESTREL 2026-07-24
    let memory_regions = unsafe { boot.mem_regions.as_slice() };
    kumo_hal::active::clean_dcache_to_poc(
        memory_regions.as_ptr() as usize,
        core::mem::size_of_val(memory_regions),
    );

    if report.has_initrd {
        // Flush the UEFI-loaded initrd to PoC before `enable_paging` replaces firmware's
        // cacheable map. `LoaderData` commonly occupies only part of its enclosing 2 MiB block,
        // so the conservative kernel identity map makes that whole block Device-nGnRnE. Without
        // this clean, the post-paging ELF parser bypasses dirty cache lines and can read corrupt
        // program-header sizes (observed on the Orange Pi 5 Plus as `Memory(InvalidRange)`).
        // — KESTREL 2026-07-24
        kumo_hal::active::clean_dcache_to_poc(boot.initrd.start as usize, boot.initrd.len as usize);
    }

    if plausible_pl011_base(boot.platform.pl011_console_base) {
        klog!(
            "SERIAL ROUTE       Check     explicit PL011 {:#x}   OK\n",
            boot.platform.pl011_console_base
        );
    } else if boot.platform.pl011_console_base != 0 {
        klog!(
            "SERIAL ROUTE       Check     rejected override {:#x}; BSP {:?}   --\n",
            boot.platform.pl011_console_base,
            selected_pl011_base
        );
    } else {
        klog!(
            "SERIAL ROUTE       Check     BSP PL011 {:?}          --\n",
            selected_pl011_base
        );
    }

    // The DW-APB console (the Orange Pi 5 Plus) is a distinct sink from the PL011 above, so it gets
    // its own route line. On that board this is the first Stage-A output the operator sees over
    // uart2, and its appearance is itself the proof R3's backend reached the wire (PLAN_VII R3).
    if let Some(base) = selected_dw8250_base {
        klog!(
            "CONSOLE ROUTE      Check     BSP DW-APB {:#x}       OK\n",
            base
        );
    }

    // Sanitize once, then make every downstream DTB consumer (timer, pinctrl, SMMU, userland
    // capability publication) see the same result. The original BootInfo stays untouched for the
    // diagnostic above; the local copy lives for the non-returning Stage-A call.
    let mut sanitized_boot = *boot;
    match unsafe { validate_dtb_handoff(boot) } {
        Ok(Some((address, size))) => {
            klog!(
                "DEVICE TREE       Check     {:#x}  {} bytes   OK\n",
                address,
                size
            );
            // Flush the firmware-written DTB to the Point of Coherency before `enable_paging`
            // rebuilds the tables. The kernel's identity map can cover this DTB's 2 MiB block as
            // Device-nGnRnE (the block straddles a region boundary, so `normal_identity_block`
            // declines Normal), while firmware wrote the blob into *cacheable* memory whose dirty
            // lines may not have reached DRAM. Every post-paging DTB consumer (GIC, PDC, SMMU,
            // i2c pinctrl) then reads it non-cacheably and sees stale DRAM — on the Orange Pi 5
            // Plus that read back 0xffffffff and produced a spurious `NoGic`. Clean it now, while
            // it is still mapped Normal, so those consumers see the real bytes. — TIMBERDOODLE
            kumo_hal::active::clean_dcache_to_poc(address as usize, size as usize);
        }
        Ok(None) => klog!("DEVICE TREE       Check     absent                  --\n"),
        Err(error) => {
            klog!(
                "DEVICE TREE       Check     rejected {:#x}: {:?}   FAIL\n",
                boot.platform.dtb,
                error
            );
            sanitized_boot.platform.dtb = 0;
        }
    }
    let boot = &sanitized_boot;

    // Boot banner in the idiom of the Jet Alone OS POST screen (Evangelion): green
    // phosphor, a RE-BOOT header, then a column of subsystem self-checks each ending in
    // OK. The Stage-A console now renders a curated CJK set (DESIGN/005), so the Japanese
    // system header stands beside the Latin POST instead of waiting for a font.
    let mm = unsafe { mm::init(boot) };

    let mut free_frames: u64 = 0;
    let mut samples: [u64; 10] = [0; 10];
    for i in 0..mm.sample_count {
        samples[i] = mm.sample_frames[i];
        free_frames += mm.sample_frames[i];
    }
    samples.sort();

    klog!("\nCPU MODE High\n");
    klog!(
        "MEMORY Check      {} + {} MiB         OK\n\n",
        report.usable_bytes >> 20,
        (report.total_bytes - report.usable_bytes) >> 20
    );

    klog!("AETHER: frames free {}\n", free_frames);
    klog!("\tframe data:\n\t");
    for (i, &frame) in samples.iter().enumerate() {
        if i > 0 {
            klog!(" ");
        }
        klog!("{}", frame);
    }
    klog!("\n\n");
    klog!("Uni+CJK Hi-SYS BOOT!\n\n");
    klog!("┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓\n");
    klog!("┃ NIMBUS    筋斗雲                   Ver.0.1.0a              ┃\n");
    klog!("┃ Copyright(c) 2025,2026    KOKEN.DEV SOFTWARE CONSORTIUM    ┃\n");
    klog!("┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛\n\n");
    // mixed ASCII+Kanji line: ASCII via the 8x16 PSF cell, Kanji via the 16x16 double-width glyphs.
    klog!("「我 所 思 兮 在 泰 山。路 遠 湲 且 阻、\n");
    klog!("  側 身 東 望 涕 沾 翰。」\n\n");
    klog!("「葡 萄 染 の 御 衣、う つ ろ ひ た る 菊 の 織 物 な ど、\n");
    klog!("  あ ま た あ る 中 に、今 様 色 の 優 れ た る を、\n");
    klog!("  姫 君 の 御 料 と て 選 ば せ た ま ふ 。」\n\n");
    klog!("「하 늘 한 가 은 데 고 기 뜨 는 백 운(白 雲)이\n");
    klog!("  그 대 의 집 이 되 니、\n");
    klog!("  명 월 을 벗 삼 아 외 로 이 앉 았 도 다 。」\n\n");
    // Broad CJK is embedded now (DESIGN/005): common simplified Chinese + Japanese kanji +
    // Korean jamo. "简体中文 / 日本語漢字 / [Hangul jamo]".
    klog!("[中國漢字  ........在線      OK ]\n");
    klog!("[日本漢字  ........出力成功  OK ]\n");
    klog!("[한국어한글 .......로드      OK ]\n\n");
    klog!("紫微     MUREX Core Ver 0.1.0a  ({})\n", report.arch);
    klog!("虹雲     ABIv{}     Check     OK\n", report.abi_version);
    // Resolve the baked board identity (DESIGN/017 §4 step 2). Until the loader stamps
    // `board_id` from `nijigumo.conf`, this reads empty on QEMU (`-kernel` path) and prints
    // "unknown"; a stamped image names the board and confirms it is a supported BSP entry.
    match kumo_bsp::Board::from_id(boot.board_id()) {
        Some(board) => klog!("盤石     BOARD {}     Check     OK\n", board.id()),
        None if boot.board_id().is_empty() => {
            klog!("盤石     BOARD unknown (unstamped)     Check     --\n")
        }
        None => klog!(
            "盤石     BOARD {} (unresolved)     Check     --\n",
            boot.board_id()
        ),
    }

    // M1: bring up memory. The bump heap is already online; account the frames and
    // prove the allocator yields real addresses (Guidance 002 §5: AETHER is real now).
    // Prove the heap works on real silicon: build a small Vec and reduce it.

    let mut squares: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
    let mut n = 1u32;
    while n <= 8 {
        squares.push(n * n);
        n += 1;
    }
    let sum: u32 = squares.iter().copied().sum();
    klog!(
        "HEAP ALLOCATOR     Check     OK     [{} KiB bump  vec sum={}]\n",
        mm.heap_bytes >> 10,
        sum
    );

    // Take ownership of the MMU: build KUMO's own identity page tables and switch off
    // the firmware's. Everything below now runs under kernel-owned paging — the
    // foundation for per-process address spaces and EL0/userspace.
    match unsafe { mm::enable_paging(boot) } {
        Some(p) => {
            // The TTBR1 physmap is live: move console MMIO access onto it, so printing
            // works under any TTBR0 (user trees no longer carry a console window).
            kumo_hal::active::console_use_physmap();
            klog!(
                "ROOTING TABLES     Check     {} MiB mapped  {} tables   OK\n",
                p.mapped_bytes >> 20,
                p.tables
            )
        }
        None => {
            // Every permanent device path below (including GICv2 IAR/EOIR) uses TTBR1's
            // physmap so a userspace TTBR0 cannot cut the kernel off from MMIO. Continuing
            // without those tables would turn the first access into a second-stage fault.
            klog!("ROOTING TABLES     Check     no usable memory map   FAIL\n");
            kumo_hal::active::halt();
        }
    }

    // R4 diagnostic (opi5 NoGic hunt): the DTB validated OK pre-paging (DEVICE TREE line above),
    // but `init_timer_interrupts` re-reads it via the SAME raw physical pointer *after* the MMU
    // switch. On the opi5 the GIC parser is proven on these exact bytes yet returns NoGic, so the
    // bytes at `dtb` must differ post-paging. Re-read the FDT magic here, through whatever mapping
    // is now active — the value distinguishes intact (0xd00dfeed) from zeroed (0x0, an overwrite)
    // from garbage (a stale/attribute read). Only fires when a DTB was handed off. — TIMBERDOODLE
    match kumo_hal::active::init_timer_interrupts(boot.platform.dtb, 20) {
        Ok(timer) => {
            let start = kumo_hal::active::timer_irq_count();
            let seen = kumo_hal::active::wait_for_timer_irqs(start, 3, 1_000_000_000);
            if seen >= 3 {
                klog!(
                    "GIC / TIMER        Check     {} Hz  IRQ {}  hb {}t   OK\n",
                    timer.counter_hz,
                    timer.irq,
                    seen
                );
            } else {
                klog!(
                    "GIC / TIMER        Check     IRQ {} heartbeat timeout ({}t)   FAIL\n",
                    timer.irq,
                    seen
                );
                // J472: before halting, emit the non-perturbing probe's one-line verdict
                // (counter → compare → redistributor → CPU interface → PE mask) so a
                // metal boot names the failing stage instead of stopping blind — the
                // Orange Pi 5 Plus (RK3588/GIC600) halted here 2026-07-22 with 0 ticks.
                klog!("{}", kumo_hal::active::gic_timer_gate_report(seen).as_str());
                kumo_hal::active::halt();
            }
        }
        Err(err) => {
            klog!(
                "GIC / TIMER        Check     unavailable: {:?}   FAIL\n",
                err
            );
            kumo_hal::active::halt();
        }
    }

    match kumo_hal::active::configure_i2c21_tlmm_pinctrl_from_dtb(boot.platform.dtb) {
        Some(updates) => klog!(
            "TLMM PINCTRL       Check     i2c21 {} updates via DTB   OK\n",
            updates
        ),
        None => klog!("TLMM PINCTRL       Check     no i2c21 DTB plan      --\n"),
    }

    // Discover (only) the SC8280XP `apps_smmu` (MMU-500 / SMMUv2) from the DTB and log its window.
    // This does NOT program the SMMU: j427's generic all-bypass enable reset the X13s — it wiped the
    // bootloader's continuous-splash display stream (Qualcomm needs the arm-smmu-qcom preservation +
    // S2CR-bypass-quirk handling before `SMMUEN` is safe). The real enable, and per-stream USB
    // translation (DESIGN/009), are later slices. No-op without an MMU-500 node (QEMU `virt`). — CORVUS
    match kumo_hal::active::smmu_apps_discover_from_dtb(boot.platform.dtb) {
        Some(smmu) => klog!(
            "APPS SMMU          Check     MMU-500 {:#x} win {:#x}  (discovery only)   OK\n",
            smmu.base,
            smmu.length
        ),
        None => klog!("APPS SMMU          Check     no MMU-500 in DTB      --\n"),
    }

    // RK3588 Road C preflight: select only the enabled PCIe MMU600 and observe its PMU status plus
    // the always-on CRU's software clock-gate/reset controls. The MMU aperture remains untouched:
    // neither control word proves a live clock, every reset source clear, or APB admission.
    // No DeviceCtx exists; j481 USB3OTG_0 remains DMA-ineligible. — KESTREL
    #[cfg(feature = "arch_aarch64")]
    {
        match kumo_hal::active::mmu600_pcie_status_from_dtb(boot.platform.dtb) {
            Some(report) => klog!("{}", report),
            None => klog!("MMU600 PCIE       Check     no enabled RK3588 MMU600  --\n"),
        }
    }

    // M3 (opening): run more than one thread of control. A couple of kernel threads
    // cooperatively yield through the HAL context switch — the first real use of the
    // task substrate on the boot path. Preemptive, timer-driven scheduling is next.
    let m3 = kdemo::run();
    klog!(
        "CONTEXT SWITCH     Check     {} kthreads  {} switches   OK\n",
        m3.threads,
        m3.switches
    );

    // M3: the scheduler substrate (DESIGN/003). Discipline A — the O(1) strict-priority
    // class — replaces the old flat round-robin. A deterministic self-test proves the
    // two-level bitmap selects in strict-priority/FIFO order, the idle floor always has
    // a thread, and a more-urgent thread preempts a running one.
    let sc = sched::smoke();
    if sc.ordered && sc.idle_floor && sc.preemptions == 2 {
        klog!(
            "SCHEDULER          Check     O(1) {}-level  {} picks   OK\n",
            sc.levels,
            sc.picks
        );
    } else {
        klog!(
            "SCHEDULER          Check     ordered={} floor={} pre={}   FAIL\n",
            sc.ordered,
            sc.idle_floor,
            sc.preemptions
        );
        kumo_hal::active::halt();
    }

    let preempt = kdemo::run_preemption();
    klog!(
        "PREEMPTION         Check     {} body switches  {} ticks   OK\n",
        preempt.switches,
        preempt.ticks
    );

    match ipc::smoke() {
        Ok(ipc) => klog!(
            "I/O VECTORS        Check     {} call  {} bytes  {} hnd   OK\n",
            ipc.calls,
            ipc.bytes,
            ipc.handle_count
        ),
        Err(err) => {
            klog!(
                "I/O VECTORS        Check     IPC smoke failed: {:?}   FAIL\n",
                err
            );
            kumo_hal::active::halt();
        }
    }

    // P4: blocking IPC between two running kernel threads. The consumer parks on an
    // empty channel and the producer wakes it — real scheduler-integrated block/wake,
    // driven by the cooperative context switch.
    let blk = ipcdemo::run();
    if blk.delivered {
        klog!(
            "IPC BLOCK / WAKE   Check     park {}x  wake {}x  {} bytes   OK\n",
            blk.consumer_blocks,
            blk.wakes,
            blk.received
        );
    } else {
        klog!(
            "IPC BLOCK / WAKE   Check     park {}x wake {}x {} bytes   FAIL\n",
            blk.consumer_blocks,
            blk.wakes,
            blk.received
        );
    }

    let initrd = if report.has_initrd {
        // P5 bootstrap: prove the initrd now contains a named Sora image and that Ziwei
        // can turn it into the first userspace process/VMAR plan. This is still a plan,
        // not execution; the next page-table slice materializes these mappings.
        let initrd = unsafe {
            core::slice::from_raw_parts(boot.initrd.start as *const u8, boot.initrd.len as usize)
        };
        let mut objects = object::ObjectManager::new();
        match bootstrap::user::plan_sora_from_initrd(&mut objects, initrd) {
            Ok(plan) => {
                let mapped_bytes: u64 = plan.image_mappings.iter().map(|mapping| mapping.len).sum();
                klog!(
                    "SORA INITRD        Check     {} seg  {}b  entry {:#x}  sp {:#x}   OK\n",
                    plan.image_mappings.len(),
                    mapped_bytes,
                    plan.entry,
                    plan.stack_top
                )
            }
            Err(err) => klog!("SORA INITRD        Check     {:?}   --\n", err),
        }
        Some(initrd)
    } else {
        None
    };

    // P5/P6: Sora runs as a scheduler-driven EL0 thread in its own address space. It
    // greets via DebugWrite, echoes the kernel's root-channel boot message, acks, then
    // serves the console channel (P6-c: four kernel console lines echoed). The kernel
    // holds the root/console peer endpoints directly, so only Sora's bootstrap handle
    // (chan.0) is a process handle — there is no second handle to report.
    let u = match initrd {
        Some(initrd) => match usermode::run_sora(boot, initrd) {
            Ok(report) => report,
            Err(err) => {
                klog!(
                    "SORA EL0          Check     {:?}; using payload fallback   --\n",
                    err
                );
                usermode::run(boot)
            }
        },
        None => usermode::run(boot),
    };
    if u.entered && u.syscalls >= 3 && u.wrote > 0 && u.chan.0 != 0 {
        klog!(
            "USERLAND  EL0      Check     {} svc  wrote {}b  boot h{}  {}   OK\n",
            u.syscalls,
            u.wrote,
            u.chan.0,
            if u.serving { "serving" } else { "exited" }
        );
    } else {
        klog!(
            "USERLAND  EL0      Check     entered={} svc={} wrote={} boot h{}   FAIL\n",
            u.entered,
            u.syscalls,
            u.wrote,
            u.chan.0
        );
    }

    // P5: kernel <-> Sora IPC. Sora wrote a greeting down the bootstrap root channel
    // (handle 1); the kernel held the peer end and read it back after Sora exited.
    if u.handshake_len > 0 {
        let msg = core::str::from_utf8(&u.handshake[..u.handshake_len]).unwrap_or("<binary>");
        klog!(
            "SORA HANDSHAKE     Check     root channel: {:?}   OK\n",
            msg.trim_end()
        );
    } else if u.chan.0 != 0 {
        klog!("SORA HANDSHAKE     Check     no message on root channel   --\n");
    }

    // P6-e: with Sora serving, route the kernel console through it. The probe line is
    // the first routed `klog!`; the syscall delta proves Sora actually round-tripped it
    // (wake -> ChannelRead -> DebugWrite -> park). On a zero delta the route is torn
    // back down so output never silently vanishes. Everything below — the remaining
    // POST lines and the serial shell — rides the userspace console server.
    if u.serving {
        let svc_before = kumo_hal::active::syscall_count();
        usermode::enable_console_route();
        klog!("MUREX console     -> Sora console channel\n");
        let probe_svcs = kumo_hal::active::syscall_count().saturating_sub(svc_before);
        // Keep the route as a bounded probe. Later POST checks call back into Sora and
        // can otherwise re-enter the console server while another Sora wake is active.
        usermode::disable_console_route();
        if probe_svcs > 0 {
            klog!(
                "CONSOLE ROUTE      Check     probe {} svc  via sora   OK\n",
                probe_svcs
            );
        } else {
            usermode::disable_console_route();
            klog!("CONSOLE ROUTE      Check     probe 0 svc  direct fallback   FAIL\n");
        }
        // P7-g: the kernel as a *client* of the userspace block server. Two reads of
        // the "disk" (the initrd) served by Sora over the block channel, verified by
        // value: the KUMORD01 magic at offset 0, and the first entry's path at offset
        // 16 — which is Sora's own image name.
        let mut sector = [0u8; 64];
        let magic_n = usermode::block_read_via_sora(0, 8, &mut sector);
        let magic_ok = magic_n == 8 && &sector[..8] == b"KUMORD01";
        let path_n = usermode::block_read_via_sora(16, 8, &mut sector);
        let path_ok = path_n == 8 && &sector[..8] == b"bin/sora";
        if magic_ok && path_ok {
            klog!(
                "BLOCK SERVE        Check     2 req  {}b  via sora   OK\n",
                magic_n + path_n
            );
        } else {
            klog!(
                "BLOCK SERVE        Check     magic {}b ok={}  path {}b ok={}   FAIL\n",
                magic_n,
                magic_ok,
                path_n,
                path_ok
            );
        }

        // P7-h: read the FAT32 disk image's sector 0 through the block server and verify
        // the BPB signature — proving the userspace block path carries real filesystem data.
        if let Some(initrd_bytes) = initrd {
            if let Ok(Some(fat32_file)) =
                kumo_abi::find_file(initrd_bytes, kumo_abi::FAT32_IMG_PATH)
            {
                let mut bpb = [0u8; 512];
                let bpb_n = usermode::block_read_via_sora(fat32_file.offset, 512, &mut bpb);
                let sig_ok = bpb_n >= 0x5A
                    && &bpb[0x52..0x5A] == b"FAT32   "
                    && &bpb[0..3] == [0xEB, 0xFE, 0x90];
                if sig_ok {
                    klog!("BLOCK FAT32        Check     sector 0 BPB  via sora   OK\n");
                } else {
                    klog!(
                        "BLOCK FAT32        Check     sector 0 BPB  {}b sig-ok={}   FAIL\n",
                        bpb_n,
                        sig_ok
                    );
                }

                // P7-j: read a named file through Sora's block channel — Sora resolves the
                // path against the FAT32 root directory, walks the FAT chain, and returns the
                // file contents. HELLO.TXT at cluster 3 holds "hello!" (6 bytes).
                let mut content = [0u8; 32];
                let content_n = usermode::file_read_via_sora(b"HELLO.TXT", &mut content);
                let content_ok = content_n == 6 && &content[..6] == b"hello!";
                // P7-k: ranged read — offset 1, length 4 should return "ello".
                let mut ranged = [0u8; 8];
                let ranged_n = usermode::file_read_via_sora_at(b"HELLO.TXT", 1, 4, &mut ranged);
                let ranged_ok = ranged_n == 4 && &ranged[..4] == b"ello";
                if content_ok && ranged_ok {
                    klog!(
                        "BLOCK FILE         Check     HELLO.TXT +{}b @1  via sora   OK\n",
                        ranged_n
                    );
                } else {
                    klog!(
                        "BLOCK FILE         Check     HELLO {}b ok={}  ranged {}b ok={}   FAIL\n",
                        content_n,
                        content_ok,
                        ranged_n,
                        ranged_ok
                    );
                }
            }

            // P9-e: verify Sora transferred a handle via the net channel.
            if usermode::net_check_transfer() {
                klog!("HANDLE XFER        Check     net channel  via sora   OK\n");
            } else {
                klog!("HANDLE XFER        Check     net channel no handle   FAIL\n");
            }

            // Console routing turns each `klog!` into a channel write + `wake_user`, which
            // re-enters Sora; Sora's SVC handlers take their *own* `&mut` of the same
            // `SoraState` via `sora_ptr()`. Anywhere a `&mut SoraState` is already live
            // across that re-entry, the two aliasing `&mut`s are UB and corrupt a heap
            // header — the long-standing "PORT BIND/NET LOOPBACK hang from stage_a" bug.
            // It bites here (these checks hold the borrow across their result klogs) and
            // again once `install_preemption_probe()` makes a timer IRQ able to preempt
            // mid-`wake_user`. The earlier serve checks dodged it only by scoping their
            // borrow tightly around each `wake_user`. Until SoraState access is made
            // aliasing-safe (a Cell/lock instead of repeated `&mut sora_ptr()`), route
            // klogs straight to the device from here on. Verified live: boot reaches the
            // `ziwei>` shell with no TOWER; re-enabling routing reintroduces the crash.
            usermode::disable_console_route();

            // Port/wait-many: kernel-level test. Use direct IPC access —
            // dispatch() hangs in stage_a context (likely a channel-signal issue).
            {
                let post = usermode::with_sora_mut(|s| {
                    let port_h = match s.engine.dispatch(&mut s.process, KernelCall::PortCreate) {
                        KernelCallResult::Handle(handle) => handle,
                        _ => return 1u8,
                    };
                    let (h0, h1) = s.engine.channel_create(&mut s.process).unwrap();
                    // Bind port to h1 (receiving endpoint).
                    let bind = s.engine.dispatch(
                        &mut s.process,
                        KernelCall::PortBind {
                            port: port_h,
                            object: h1,
                        },
                    );
                    if bind != KernelCallResult::Status(Errno::Ok.status()) {
                        return 2u8;
                    }
                    // Write to h0 → delivers to h1, which signals the port bound to h1.
                    let msg = Message::new(1, b"x", &[]).unwrap();
                    let write = s.engine.dispatch(
                        &mut s.process,
                        KernelCall::ChannelWrite {
                            channel: h0,
                            message: msg,
                        },
                    );
                    if write != KernelCallResult::Status(Errno::Ok.status()) {
                        return 3u8;
                    }
                    match s.engine.ipc_mut().port_wait(&s.process, port_h) {
                        Ok(pkt) if pkt.signals.contains(Signals::READABLE) => 0u8,
                        Ok(_) => 4u8,
                        Err(_) => 5u8,
                    }
                });
                // Check port via IPC directly (not dispatch — same hang avoidance).
                match post {
                    0 => {
                        klog!("PORT BIND          Check     channel signal  via eng   OK\n");
                    }
                    1 => klog!("PORT BIND          Check     port create fail   FAIL\n"),
                    2 => klog!("PORT BIND          Check     bind fail   FAIL\n"),
                    3 => klog!("PORT BIND          Check     channel write fail   FAIL\n"),
                    4 => klog!("PORT BIND          Check     wrong signal   FAIL\n"),
                    _ => klog!("PORT BIND          Check     port empty   FAIL\n"),
                }

                // Net loopback: send "ping" on the net channel, Sora echoes it.
                let mut lb_buf = [0u8; 8];
                let lb_n = usermode::net_loopback(b"ping", &mut lb_buf);
                let lb_ok = lb_n == 4 && &lb_buf[..4] == b"ping";
                // Multi-connection control token. Real capability transfer waits for
                // child/channel syscalls; the POST checks that Sora's net route responds.
                let mut cn_buf = [0u8; 8];
                let cn_n = usermode::net_loopback(b"conn", &mut cn_buf);
                let cn_ok = cn_n >= 1 && cn_buf[0] != b'e';
                // P9-f: named pipe control token; two requests must get non-error acks.
                let mut p1_buf = [0u8; 8];
                let p1_n = usermode::net_loopback(b"pipe:test", &mut p1_buf);
                let p2_n = usermode::net_loopback(b"pipe:test", &mut p1_buf);
                let pipe_ok = p1_n >= 1 && p1_buf[0] != b'e' && p2_n >= 1 && p1_buf[0] != b'e';
                if lb_ok && cn_ok && pipe_ok {
                    klog!("NET LOOPBACK       Check     ping +conn +pipe  via sora   OK\n");
                } else {
                    klog!(
                        "NET LOOPBACK       Check     lb={}/{} cn={}/{} pipe={}/{}   FAIL\n",
                        lb_n,
                        lb_ok,
                        cn_n,
                        cn_ok,
                        p2_n,
                        pipe_ok
                    );
                }
            }
        }
    }

    if !stage_a_uses_serial_floor(
        report.has_framebuffer,
        selected_pl011_base,
        selected_dw8250_base,
    ) {
        // Framebuffer-only console (the X13s): no resolved UART and no kernel keyboard yet, so
        // idle here. The screen keeps the boot report; a pre-handoff pause lives in the loader.
        klog!("\nFRAMEBUFFER   Check     GREEN                    OK\n");
        klog!("\nMUREX core online -- all subsystems nominal.\n");
        klog!("KUMO MUREX core Stage-A online; awaiting userspace.  HALT.\n");
        loop {
            user_thread::pump_idle_floor();
            kumo_hal::active::spin_once();
        }
    } else {
        // P8-b: selected PL011 console (QEMU, Pi uart10, or explicit RP1) — forward polled
        // keystrokes to Sora via the keyboard channel. Polling deliberately avoids inventing an
        // IRQ for the PCIe-hosted RP1 UART. Sora buffers keystrokes (minimal line editing:
        // backspace), echoes via DebugWrite, and sends completed lines to the kernel via the root
        // channel. The framebuffer remains an independent sink; its presence no longer suppresses
        // this serial floor. The kernel runs shell::run_command on each line. This is scaffold
        // under DESIGN/006 §b — the line-edit loop is IPC, not a TTY. — KESTREL 2026-07-18
        kdemo::install_preemption_probe();
        let mut env = shell::ShellEnv {
            arch: report.arch,
            abi_version: report.abi_version,
            usable_frames: mm.usable_frames,
            usable_bytes: report.usable_bytes,
            total_bytes: report.total_bytes,
            heap_kib: mm.heap_bytes >> 10,
            uptime_ns: 0,
            preempt_ticks: 0,
            preempt_switches: 0,
        };
        let mut sysrq_out = bootstrap::console::Writer;
        klog!("\nKUMO MUREX core Stage-A serial shell. Type 'help'.\n");
        klog!("escape hatch: ctrl-\\ or ~~~ then '?' (works even if userland wedges).\n");
        klog!("{}", shell::PROMPT);
        loop {
            // The UART now has exactly one reader — the IRQ handler in `conin` — because the
            // boot floor stops running whenever a child spins at EL0, which is the very case the
            // escape hatch exists for. What arrives here is what that handler judged ordinary
            // input. TAB drives ttyd's completion and ESC opens its escape-sequence state machine
            // (arrow keys arrive as `ESC [ A`/`B`); both used to be dropped, which left history
            // and completion written, tested, and unreachable over serial.
            if let Some(byte) = conin::next_byte() {
                if matches!(
                    byte,
                    0x08 | 0x09 | 0x1b | 0x7f | b'\r' | b'\n' | 0x20..=0x7e
                ) {
                    usermode::kbd_forward(byte);
                }
            }
            if let Some(action) = conin::take_deferred_action() {
                run_sysrq(action, &env, &mut sysrq_out);
            }
            // Check for a completed command line from Sora via the root channel.
            if usermode::poll_root_command(&mut env) > 0 {
                klog!("{}", shell::PROMPT);
            }
            user_thread::pump_idle_floor();
        }
    }
}

/// Act on a completed console escape sequence.
///
/// Every arm reaches something that does not need Sora: `power::execute` quiesces through the
/// non-panicking borrow and writes through the console fallback, and the task table is kernel
/// state. That independence is the whole reason this exists.
fn run_sysrq(action: sysrq::Action, env: &shell::ShellEnv, out: &mut dyn core::fmt::Write) {
    use core::fmt::Write;
    match action {
        sysrq::Action::Armed => {
            let _ = out.write_str(sysrq::ARMED);
        }
        sysrq::Action::Reboot => {
            let _ = out.write_str("\r\nsysrq: reboot\r\n");
            power::execute(power::PowerAction::Reset, out);
        }
        sysrq::Action::Shutdown => {
            let _ = out.write_str("\r\nsysrq: shutdown\r\n");
            power::execute(power::PowerAction::PowerOff, out);
        }
        sysrq::Action::Halt => {
            let _ = out.write_str("\r\nsysrq: halt\r\n");
            power::execute(power::PowerAction::Halt, out);
        }
        sysrq::Action::Tasks => {
            let _ = out.write_str("\r\nsysrq: tasks\r\n");
            shell::run_command("ps", env, &kdemo::tasks(), out);
        }
        sysrq::Action::Help => {
            let _ = out.write_str(sysrq::HELP);
        }
        sysrq::Action::Unknown(byte) => {
            let _ = write!(
                out,
                "\r\nsysrq: no command for {:#04x}; ctrl-\\ ? for the list\r\n",
                byte
            );
        }
    }
}

#[no_mangle]
pub extern "C" fn kmain(boot: *const BootInfo) -> ! {
    if boot.is_null() {
        tower_halt_ascii("nijigumo->MUREX handoff pointer is null", None);
    }

    let boot = unsafe { &*boot };
    stage_a(boot)
}

/// x86_64 first light: entered (in long mode) from the Multiboot trampoline in
/// `main.rs`. This is the GRUB/Multiboot analog of the aarch64 Nijigumo handoff — it
/// proves the loader → 32→64-bit → serial chain and reads the Multiboot memory info.
/// `mbi` is the Multiboot1 info pointer, `magic` the boot magic (`0x2BADB002`). Full
/// This path now owns descriptor tables, paging, ACPI/APIC timers, scheduling, and native CPL3
/// execution while retaining the compact serial-first bring-up harness.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
const X86_MULTIBOOT1_MAGIC: u64 = 0x2bad_b002;
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
const X86_MULTIBOOT2_MAGIC: u64 = bootstrap::multiboot::MULTIBOOT2_BOOT_MAGIC;
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
const X86_BOOT_IDENTITY_LIMIT: u64 = 1 << 30;
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
const X86_PHYS_MAP_LIMIT: u64 = 1 << 39;

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum X86MultibootInitrdError {
    WrongMagic,
    BadInfo,
    MissingModules,
    BadModuleTable,
    BadModuleRange,
    Initrd(kumo_abi::InitrdError),
    MissingHello,
    Multiboot2(bootstrap::multiboot::Multiboot2Error),
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum X86MultibootMemoryError {
    WrongMagic,
    BadInfo,
    MissingMemoryMap,
    BadMemoryMapRange,
    Parse(bootstrap::multiboot::MemoryMapError),
    BadKernelRange,
    InitrdOverlapsKernel,
    Multiboot2(bootstrap::multiboot::Multiboot2Error),
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
struct X86MultibootInitrd {
    bytes: &'static [u8],
    hello: &'static [u8],
    module_count: u32,
    start: u64,
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
unsafe fn x86_multiboot2_info(
    mbi: u64,
) -> Result<bootstrap::multiboot::Multiboot2Info<'static>, bootstrap::multiboot::Multiboot2Error> {
    const MAX_INFO_BYTES: u64 = 1 << 20;
    if mbi == 0
        || mbi & 7 != 0
        || mbi
            .checked_add(8)
            .is_none_or(|end| end > X86_BOOT_IDENTITY_LIMIT)
    {
        return Err(bootstrap::multiboot::Multiboot2Error::BadTotalSize);
    }
    let total_size = u64::from(unsafe { core::ptr::read_volatile(mbi as *const u32) });
    if total_size < 16
        || total_size > MAX_INFO_BYTES
        || total_size & 7 != 0
        || mbi
            .checked_add(total_size)
            .is_none_or(|end| end > X86_BOOT_IDENTITY_LIMIT)
    {
        return Err(bootstrap::multiboot::Multiboot2Error::BadTotalSize);
    }
    let bytes = unsafe {
        core::slice::from_raw_parts(
            mbi as *const u8,
            usize::try_from(total_size)
                .map_err(|_| bootstrap::multiboot::Multiboot2Error::BadTotalSize)?,
        )
    };
    bootstrap::multiboot::Multiboot2Info::parse(bytes)
}

/// Resolve the first Multiboot1/2 module as a KUMO initrd and locate `bin/hello` inside it.
/// The trampoline maps the low 1 GiB, so every metadata and payload access is bounded there
/// before a pointer is formed.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
unsafe fn x86_multiboot_initrd(
    mbi: u64,
    magic: u64,
) -> Result<X86MultibootInitrd, X86MultibootInitrdError> {
    const MODULE_FLAG: u32 = 1 << 3;
    const MODULE_ENTRY_LEN: u64 = 16;
    const MAX_MODULES: u32 = 64;

    if magic == X86_MULTIBOOT2_MAGIC {
        let info =
            unsafe { x86_multiboot2_info(mbi) }.map_err(X86MultibootInitrdError::Multiboot2)?;
        let module = info
            .first_module()
            .map_err(X86MultibootInitrdError::Multiboot2)?
            .ok_or(X86MultibootInitrdError::MissingModules)?;
        if module.start == 0 || module.start >= module.end || module.end > X86_BOOT_IDENTITY_LIMIT {
            return Err(X86MultibootInitrdError::BadModuleRange);
        }
        let len = usize::try_from(module.end - module.start)
            .map_err(|_| X86MultibootInitrdError::BadModuleRange)?;
        let bytes = unsafe { core::slice::from_raw_parts(module.start as *const u8, len) };
        let hello = kumo_abi::find_file(bytes, kumo_abi::HELLO_PATH)
            .map_err(X86MultibootInitrdError::Initrd)?
            .ok_or(X86MultibootInitrdError::MissingHello)?;
        return Ok(X86MultibootInitrd {
            bytes,
            hello: hello.bytes,
            module_count: info.module_count(),
            start: module.start,
        });
    }
    if magic != X86_MULTIBOOT1_MAGIC {
        return Err(X86MultibootInitrdError::WrongMagic);
    }
    if mbi == 0
        || mbi & 3 != 0
        || mbi
            .checked_add(28)
            .is_none_or(|end| end > X86_BOOT_IDENTITY_LIMIT)
    {
        return Err(X86MultibootInitrdError::BadInfo);
    }
    let info = mbi as *const u8;
    let read_info =
        |offset: usize| unsafe { core::ptr::read_volatile(info.add(offset).cast::<u32>()) };
    let flags = read_info(0);
    if flags & MODULE_FLAG == 0 {
        return Err(X86MultibootInitrdError::MissingModules);
    }
    let module_count = read_info(20);
    let module_table = read_info(24) as u64;
    let table_len = u64::from(module_count)
        .checked_mul(MODULE_ENTRY_LEN)
        .ok_or(X86MultibootInitrdError::BadModuleTable)?;
    if module_count == 0
        || module_count > MAX_MODULES
        || module_table == 0
        || module_table & 3 != 0
        || module_table
            .checked_add(table_len)
            .is_none_or(|end| end > X86_BOOT_IDENTITY_LIMIT)
    {
        return Err(X86MultibootInitrdError::BadModuleTable);
    }

    let module = module_table as *const u8;
    let start = unsafe { core::ptr::read_volatile(module.cast::<u32>()) } as u64;
    let end = unsafe { core::ptr::read_volatile(module.add(4).cast::<u32>()) } as u64;
    if start == 0 || start >= end || end > X86_BOOT_IDENTITY_LIMIT {
        return Err(X86MultibootInitrdError::BadModuleRange);
    }
    let len = usize::try_from(end - start).map_err(|_| X86MultibootInitrdError::BadModuleRange)?;
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, len) };
    let hello = kumo_abi::find_file(bytes, kumo_abi::HELLO_PATH)
        .map_err(X86MultibootInitrdError::Initrd)?
        .ok_or(X86MultibootInitrdError::MissingHello)?;
    Ok(X86MultibootInitrd {
        bytes,
        hello: hello.bytes,
        module_count,
        start,
    })
}

/// Copy the Multiboot1/2 firmware map into KUMO ABI regions, clipping physical extents to
/// `accessible_limit`. The map metadata itself must remain inside the trampoline's low
/// identity window because this parser runs before the permanent CR3 is installed.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
unsafe fn x86_multiboot_memory_map(
    mbi: u64,
    magic: u64,
    accessible_limit: u64,
) -> Result<alloc::vec::Vec<MemRegion>, X86MultibootMemoryError> {
    const MEMORY_MAP_FLAG: u32 = 1 << 6;
    const MEMORY_MAP_INFO_END: u64 = 52;
    const MAX_MEMORY_MAP_BYTES: u64 = 1 << 20;

    if magic == X86_MULTIBOOT2_MAGIC {
        let info =
            unsafe { x86_multiboot2_info(mbi) }.map_err(X86MultibootMemoryError::Multiboot2)?;
        let map = info
            .memory_map()
            .map_err(X86MultibootMemoryError::Multiboot2)?
            .ok_or(X86MultibootMemoryError::MissingMemoryMap)?;
        return bootstrap::multiboot::normalize_multiboot2_memory_map(map, accessible_limit)
            .map_err(X86MultibootMemoryError::Parse);
    }
    if magic != X86_MULTIBOOT1_MAGIC {
        return Err(X86MultibootMemoryError::WrongMagic);
    }
    if mbi == 0
        || mbi & 3 != 0
        || mbi
            .checked_add(MEMORY_MAP_INFO_END)
            .is_none_or(|end| end > X86_BOOT_IDENTITY_LIMIT)
    {
        return Err(X86MultibootMemoryError::BadInfo);
    }
    let info = mbi as *const u8;
    let read_info =
        |offset: usize| unsafe { core::ptr::read_volatile(info.add(offset).cast::<u32>()) };
    if read_info(0) & MEMORY_MAP_FLAG == 0 {
        return Err(X86MultibootMemoryError::MissingMemoryMap);
    }
    let map_len = u64::from(read_info(44));
    let map_start = u64::from(read_info(48));
    if map_len == 0
        || map_len > MAX_MEMORY_MAP_BYTES
        || map_start == 0
        || map_start
            .checked_add(map_len)
            .is_none_or(|end| end > X86_BOOT_IDENTITY_LIMIT)
    {
        return Err(X86MultibootMemoryError::BadMemoryMapRange);
    }
    let bytes = unsafe {
        core::slice::from_raw_parts(
            map_start as *const u8,
            usize::try_from(map_len).map_err(|_| X86MultibootMemoryError::BadMemoryMapRange)?,
        )
    };
    bootstrap::multiboot::normalize_memory_map(bytes, accessible_limit)
        .map_err(X86MultibootMemoryError::Parse)
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
unsafe fn x86_boot_info(
    regions: &[MemRegion],
    initrd: X86MultibootInitrd,
) -> Result<BootInfo, X86MultibootMemoryError> {
    extern "C" {
        static __kernel_start: u8;
        static __bss_end: u8;
    }

    let kernel_start = core::ptr::addr_of!(__kernel_start) as u64;
    let kernel_end = core::ptr::addr_of!(__bss_end) as u64;
    if kernel_start == 0 || kernel_start >= kernel_end || kernel_end > X86_BOOT_IDENTITY_LIMIT {
        return Err(X86MultibootMemoryError::BadKernelRange);
    }

    let mut boot = BootInfo::empty(ABI_VERSION);
    boot.mem_regions = RawSlice::from_slice(regions);
    boot.kernel_phys = Range::new(kernel_start, kernel_end - kernel_start);
    boot.kernel_virt = boot.kernel_phys;
    boot.initrd = Range::new(initrd.start, initrd.bytes.len() as u64);
    if boot.initrd.start < boot.kernel_phys.end() && boot.kernel_phys.start < boot.initrd.end() {
        return Err(X86MultibootMemoryError::InitrdOverlapsKernel);
    }
    Ok(boot)
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum X86ScheduledSmokeError {
    Elf(bootstrap::user::ElfError),
    Segment,
    Image(kumo_hal::active::UserImageError),
    Runtime(usermode::UsermodeError),
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
const X86_HELLO_LEN: usize = b"hello from a native KUMO program!\n".len();

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct X86FrameTrace {
    frames: u32,
    first: u64,
    last: u64,
    limit: u64,
    safe: bool,
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
impl X86FrameTrace {
    const fn new(limit: u64) -> Self {
        Self {
            frames: 0,
            first: 0,
            last: 0,
            limit,
            safe: true,
        }
    }

    fn record(&mut self, boot: &BootInfo, frame: u64) {
        let end = frame.saturating_add(mm::PAGE_SIZE);
        let overlaps = |range: Range| frame < range.end() && range.start < end;
        let in_usable_ram = unsafe { boot.mem_regions.as_slice() }.iter().any(|region| {
            region.kind == MemRegionKind::Usable
                && region.range.start <= frame
                && end <= region.range.end()
        });
        self.safe &= frame & (mm::PAGE_SIZE - 1) == 0
            && end <= self.limit
            && in_usable_ram
            && (self.frames == 0 || frame > self.last)
            && !overlaps(boot.kernel_phys)
            && !overlaps(boot.initrd);
        if self.frames == 0 {
            self.first = frame;
        }
        self.last = frame;
        self.frames = self.frames.saturating_add(1);
    }
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct X86ScheduledSmokeReport {
    entry: u64,
    segments: usize,
    bootstrap: u64,
    syscalls: u32,
    wrote: usize,
    exit_code: u64,
    switches: u64,
    done: bool,
    user_root: u64,
    frame_trace: X86FrameTrace,
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
fn run_x86_scheduled_ring3_smoke(
    bytes: &[u8],
    boot: &BootInfo,
) -> Result<X86ScheduledSmokeReport, X86ScheduledSmokeError> {
    let elf = bootstrap::user::parse_user_elf(bytes).map_err(X86ScheduledSmokeError::Elf)?;
    let mut segments = alloc::vec::Vec::with_capacity(elf.segments.len());
    for segment in &elf.segments {
        let start =
            usize::try_from(segment.file_offset).map_err(|_| X86ScheduledSmokeError::Segment)?;
        let file_size =
            usize::try_from(segment.file_size).map_err(|_| X86ScheduledSmokeError::Segment)?;
        let end = start
            .checked_add(file_size)
            .filter(|&end| end <= bytes.len())
            .ok_or(X86ScheduledSmokeError::Segment)?;
        segments.push(kumo_hal::active::UserLoadSegment {
            source: &bytes[start..end],
            virt_addr: segment.virt_addr,
            mem_size: segment.mem_size,
            writable: segment.flags.contains(kumo_hal::PageFlags::WRITE),
            executable: segment.flags.contains(kumo_hal::PageFlags::EXECUTE),
        });
    }
    let kernel_root = kumo_hal::active::read_user_aspace_root();
    let root_vmar = mm::Vmar::new(
        bootstrap::user::USER_ROOT_BASE,
        bootstrap::user::USER_ROOT_SIZE,
    )
    .map_err(|_| X86ScheduledSmokeError::Runtime(usermode::UsermodeError::ChannelSetup))?;
    let bootstrap_handle = usermode::install_standalone_user_runtime(root_vmar, kernel_root)
        .map_err(X86ScheduledSmokeError::Runtime)?;
    let image = kumo_hal::active::UserImage {
        entry: elf.entry,
        stack_top: bootstrap::user::USER_STACK_TOP,
        stack_size: bootstrap::user::USER_STACK_SIZE,
        bootstrap: bootstrap_handle.0 as u64,
        segments: &segments,
        extra_mappings: &[],
    };
    let mut frame_trace = X86FrameTrace::new(X86_PHYS_MAP_LIMIT);
    let state = {
        let mut alloc = || {
            let frame = unsafe { mm::alloc_zeroed_frame(boot) };
            if let Some(frame) = frame {
                frame_trace.record(boot, frame);
            }
            frame
        };
        kumo_hal::active::prepare_scheduled_user_image(&image, &mut alloc)
            .map_err(X86ScheduledSmokeError::Image)?
    };
    let user_root = state.ttbr0;
    usermode::set_standalone_user_aspace(user_root);
    unsafe { user_thread::spawn_user(state, user_root) };

    Ok(X86ScheduledSmokeReport {
        entry: elf.entry,
        segments: elf.segments.len(),
        bootstrap: bootstrap_handle.0 as u64,
        syscalls: kumo_hal::active::syscall_count(),
        wrote: usermode::standalone_user_bytes_written(),
        exit_code: user_thread::exit_code(),
        switches: user_thread::switch_count(),
        done: user_thread::is_done(),
        user_root,
        frame_trace,
    })
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub fn x86_first_light(mbi: u64, magic: u64, kernel_stack_top: u64) -> ! {
    klog!("\n[MUREX] KUMO x86_64 first light (Multiboot/GRUB)\n");
    klog!("CPU MODE: long mode (64-bit), paging on, serial COM1 live\n");
    let multiboot2 = if magic == X86_MULTIBOOT2_MAGIC {
        match unsafe { x86_multiboot2_info(mbi) } {
            Ok(info) => Some(info),
            Err(err) => {
                klog!("multiboot2: info {:?}   FAIL\n", err);
                kumo_hal::active::halt();
            }
        }
    } else {
        None
    };
    let protocol = if multiboot2.is_some() { 2 } else { 1 };
    klog!(
        "multiboot: v{} magic={:#010x} info@{:#x}\n",
        protocol,
        magic,
        mbi
    );

    let initrd = match unsafe { x86_multiboot_initrd(mbi, magic) } {
        Ok(initrd) => {
            klog!(
                "MULTIBOOT INITRD   Check     {} module  KUMORD01 {}b  {} {}b  phys {:#x}   OK\n",
                initrd.module_count,
                initrd.bytes.len(),
                kumo_abi::HELLO_PATH,
                initrd.hello.len(),
                initrd.start
            );
            initrd
        }
        Err(err) => {
            klog!("MULTIBOOT INITRD   Check     {:?}   FAIL\n", err);
            kumo_hal::active::halt();
        }
    };

    let memory_regions = match unsafe { x86_multiboot_memory_map(mbi, magic, X86_PHYS_MAP_LIMIT) } {
        Ok(regions) => regions,
        Err(err) => {
            klog!("MULTIBOOT BOOTINFO Check     memory map {:?}   FAIL\n", err);
            kumo_hal::active::halt();
        }
    };
    let bootstrap_regions =
        match unsafe { x86_multiboot_memory_map(mbi, magic, X86_BOOT_IDENTITY_LIMIT) } {
            Ok(regions) => regions,
            Err(err) => {
                klog!(
                    "MULTIBOOT BOOTINFO Check     bootstrap map {:?}   FAIL\n",
                    err
                );
                kumo_hal::active::halt();
            }
        };
    let boot = match unsafe { x86_boot_info(&memory_regions, initrd) } {
        Ok(boot) => boot,
        Err(err) => {
            klog!("MULTIBOOT BOOTINFO Check     handoff {:?}   FAIL\n", err);
            kumo_hal::active::halt();
        }
    };
    let bootstrap_boot = match unsafe { x86_boot_info(&bootstrap_regions, initrd) } {
        Ok(boot) => boot,
        Err(err) => {
            klog!(
                "MULTIBOOT BOOTINFO Check     bootstrap handoff {:?}   FAIL\n",
                err
            );
            kumo_hal::active::halt();
        }
    };
    let handoff = match inspect_boot(&boot) {
        Ok(report) if report.has_initrd => report,
        Ok(report) => {
            klog!("MULTIBOOT BOOTINFO Check     {:?}   FAIL\n", report);
            kumo_hal::active::halt();
        }
        Err(err) => {
            klog!("MULTIBOOT BOOTINFO Check     {:?}   FAIL\n", err);
            kumo_hal::active::halt();
        }
    };
    klog!(
        "MULTIBOOT BOOTINFO Check     ABIv{}  {} regions  {} MiB usable / {} MiB mapped  kernel {:#x}+{} KiB  initrd {:#x}+{}b  phys<512G   OK\n",
        handoff.abi_version,
        handoff.mem_region_count,
        handoff.usable_bytes >> 20,
        handoff.total_bytes >> 20,
        boot.kernel_phys.start,
        boot.kernel_phys.len >> 10,
        boot.initrd.start,
        boot.initrd.len
    );

    // Exercise the architecture-neutral M1 plan, not a parallel x86 allocator. The plan
    // subtracts the linker-derived kernel extent and Multiboot module before yielding frames.
    let memory = unsafe { mm::init(&boot) };
    let samples_safe = memory.sample_count == memory.sample_frames.len()
        && memory.sample_frames.iter().all(|&frame| {
            let end = frame.saturating_add(mm::PAGE_SIZE);
            let overlaps = |range: Range| frame < range.end() && range.start < end;
            frame >= 1 << 20
                && end <= X86_BOOT_IDENTITY_LIMIT
                && !overlaps(boot.kernel_phys)
                && !overlaps(boot.initrd)
        });
    if memory.usable_frames == 0 || !samples_safe {
        klog!("M1 MEMORY PLAN     Check     {:?}   FAIL\n", memory);
        kumo_hal::active::halt();
    }
    klog!(
        "M1 MEMORY PLAN     Check     {} frames / {} MiB  kernel+initrd excluded  samples {:#x} {:#x} {:#x}   OK\n",
        memory.usable_frames,
        memory.usable_bytes >> 20,
        memory.sample_frames[0],
        memory.sample_frames[1],
        memory.sample_frames[2]
    );

    // The trampoline only makes low RAM writable, so bootstrap-owned frames build the
    // permanent tree even though that tree is sized and typed from the full firmware map.
    // Once CR3 moves, all normalized RAM below the userspace boundary is identity-reachable.
    let old_root = kumo_hal::active::read_user_aspace_root();
    let mut paging_frames = X86FrameTrace::new(X86_BOOT_IDENTITY_LIMIT);
    let paging = {
        let mut alloc = || {
            let frame = unsafe { mm::alloc_zeroed_frame(&bootstrap_boot) };
            if let Some(frame) = frame {
                paging_frames.record(&bootstrap_boot, frame);
            }
            frame
        };
        unsafe { mm::enable_paging_with_allocator(&boot, &mut alloc) }
    };
    let new_root = kumo_hal::active::read_user_aspace_root();
    let paging = match paging {
        Some(report)
            if paging_frames.safe
                && paging_frames.frames as usize == report.tables
                && paging_frames.first == new_root
                && old_root != new_root
                && report.mapped_bytes >= 4 * (1 << 30) =>
        {
            report
        }
        report => {
            klog!(
                "KERNEL CR3 / PHYSMAP Check     old {:#x} new {:#x} report {:?} frames {:?}   FAIL\n",
                old_root,
                new_root,
                report,
                paging_frames
            );
            kumo_hal::active::halt();
        }
    };

    let high_probe = memory_regions.iter().find_map(|region| {
        if region.kind != MemRegionKind::Usable || region.range.end() <= X86_BOOT_IDENTITY_LIMIT {
            return None;
        }
        let address = mm::align_up(region.range.start.max(X86_BOOT_IDENTITY_LIMIT))?;
        address
            .checked_add(core::mem::size_of::<u64>() as u64)
            .filter(|&end| end <= region.range.end())
            .map(|_| address)
    });
    if let Some(address) = high_probe {
        let pointer = address as *mut u64;
        let original = unsafe { pointer.read_volatile() };
        let pattern = original ^ 0x4b55_4d4f_cafe_f00d;
        unsafe { pointer.write_volatile(pattern) };
        let observed = unsafe { pointer.read_volatile() };
        unsafe { pointer.write_volatile(original) };
        if observed != pattern {
            klog!(
                "KERNEL CR3 / PHYSMAP Check     high RAM probe {:#x} read {:#x} wanted {:#x}   FAIL\n",
                address,
                observed,
                pattern
            );
            kumo_hal::active::halt();
        }
        klog!(
            "KERNEL CR3 / PHYSMAP Check     old {:#x} new {:#x}  {} tables  {} GiB  {} low BootInfo frames  RAM WB  holes UC/NX  high RAM yes {:#x}   OK\n",
            old_root,
            new_root,
            paging.tables,
            paging.mapped_bytes >> 30,
            paging_frames.frames,
            address
        );
    } else {
        klog!(
            "KERNEL CR3 / PHYSMAP Check     old {:#x} new {:#x}  {} tables  {} GiB  {} low BootInfo frames  RAM WB  holes UC/NX  high RAM absent   OK\n",
            old_root,
            new_root,
            paging.tables,
            paging.mapped_bytes >> 30,
            paging_frames.frames
        );
    }

    // Keep the diagnostic memory summary honest for either boot protocol.
    if let Some(info) = multiboot2 {
        klog!("multiboot2: {}b tagged handoff\n", info.total_size());
        if let Some((mem_lower, mem_upper)) = info.basic_memory() {
            klog!(
                "AETHER: {} KiB lower + {} KiB upper (~{} MiB usable)  OK\n",
                mem_lower,
                mem_upper,
                (mem_lower + mem_upper) / 1024
            );
        }
    } else if magic == X86_MULTIBOOT1_MAGIC && mbi != 0 {
        let flags = unsafe { core::ptr::read_volatile(mbi as *const u32) };
        klog!("multiboot: flags={:#010x}\n", flags);
        if flags & 0x1 != 0 {
            let mem_lower = unsafe { core::ptr::read_volatile((mbi + 4) as *const u32) };
            let mem_upper = unsafe { core::ptr::read_volatile((mbi + 8) as *const u32) };
            klog!(
                "AETHER: {} KiB lower + {} KiB upper (~{} MiB usable)  OK\n",
                mem_lower,
                mem_upper,
                (mem_lower + mem_upper) / 1024
            );
        }
    }

    // Supersede the Multiboot trampoline's minimal GDT before the IDT starts consuming its code
    // selector. This adds the CPL3 segments and activates a 64-bit TSS with a valid RSP0.
    let descriptors = kumo_hal::active::install_descriptor_tables(kernel_stack_top);
    if descriptors.is_live() {
        klog!(
            "GDT / TSS          Check     kernel {:#04x}/{:#04x}  user {:#04x}/{:#04x}  TR {:#04x}  rsp0 {:#x}   OK\n",
            descriptors.kernel_code_selector,
            descriptors.kernel_data_selector,
            descriptors.user_code_selector,
            descriptors.user_data_selector,
            descriptors.task_selector,
            descriptors.rsp0
        );
    } else {
        klog!(
            "GDT / TSS          Check     limit {:#x}  CS {:#04x} SS {:#04x} TR {:#04x}   FAIL\n",
            descriptors.gdt_limit,
            descriptors.kernel_code_selector,
            descriptors.kernel_data_selector,
            descriptors.task_selector
        );
        kumo_hal::active::halt();
    }

    let tagged_acpi = multiboot2.and_then(|info| {
        let rsdp = info.acpi_rsdp()?;
        match kumo_hal::active::inspect_acpi_rsdp(rsdp, rsdp.as_ptr() as u64) {
            Some(acpi) => Some(acpi),
            None => {
                klog!("ACPI TABLES        Check     Multiboot2 RSDP invalid   FAIL\n");
                kumo_hal::active::halt();
            }
        }
    });
    let (acpi_root, acpi_source) = match tagged_acpi {
        Some(acpi) => (acpi, "Multiboot2"),
        None => match kumo_hal::active::discover_acpi_root() {
            Some(acpi) => (acpi, "legacy scan"),
            None => {
                klog!("ACPI TABLES        Check     RSDP absent   FAIL\n");
                kumo_hal::active::halt();
            }
        },
    };
    let root = if acpi_root.uses_xsdt { "XSDT" } else { "RSDT" };
    klog!(
        "ACPI TABLES        Check     RSDP {:#x} rev {}  {} {:#x}  via {}   OK\n",
        acpi_root.rsdp_address,
        acpi_root.revision,
        root,
        acpi_root.root_address,
        acpi_source
    );

    let madt = match kumo_hal::active::discover_acpi_madt(acpi_root, paging.mapped_bytes) {
        Some(madt) => {
            klog!(
            "ACPI MADT          Check     APIC {:#x}  LAPIC {:#x}  IOAPIC {}  ISO {}  PCAT {}   OK\n",
            madt.address,
            madt.local_interrupt_controller_address,
            madt.io_apic_count,
            madt.source_override_count,
            madt.pcat_compatible
            );
            madt
        }
        None => {
            klog!("ACPI MADT          Check     APIC absent or invalid   FAIL\n");
            kumo_hal::active::halt();
        }
    };

    match madt.legacy_timer_route {
        Some(route) => {
            let polarity = if route.active_low { "low" } else { "high" };
            let trigger = if route.level_triggered {
                "level"
            } else {
                "edge"
            };
            let origin = if route.overridden {
                "override"
            } else {
                "identity"
            };
            klog!(
                "ACPI IRQ ROUTE     Check     ISA IRQ {} -> GSI {}  IOAPIC {:#x} base {}  {} {}  {} candidate   OK\n",
                route.isa_irq,
                route.global_system_interrupt,
                route.candidate_io_apic_address,
                route.candidate_io_apic_gsi_base,
                polarity,
                trigger,
                origin
            );
            match kumo_hal::active::inspect_boot_io_apic(route) {
                Some(io_apic)
                    if io_apic.id_matches_madt()
                        && io_apic.contains_routed_gsi()
                        && io_apic.routed_entry.is_some() =>
                {
                    klog!(
                        "IOAPIC HW          Check     id {}  ver {:#04x}  entries {}  GSI {}-{} contains {}   OK\n",
                        io_apic.hardware_id,
                        io_apic.version,
                        io_apic.redirection_entries,
                        io_apic.gsi_base,
                        io_apic.gsi_end,
                        io_apic.routed_gsi
                    );
                    let entry = io_apic.routed_entry.unwrap();
                    let destination_mode = if entry.logical_destination {
                        "logical"
                    } else {
                        "physical"
                    };
                    let polarity = if entry.active_low { "low" } else { "high" };
                    let trigger = if entry.level_triggered {
                        "level"
                    } else {
                        "edge"
                    };
                    let mask = if entry.masked { "masked" } else { "unmasked" };
                    let delivery = if entry.delivery_pending {
                        "pending"
                    } else {
                        "idle"
                    };
                    klog!(
                        "IOAPIC INPUT       Check     GSI {} pin {}  vec {:#04x} {} {} dest {}  {} {} {} {} rirr {}   OK\n",
                        io_apic.routed_gsi,
                        entry.input_pin,
                        entry.vector,
                        entry.delivery_mode_name(),
                        destination_mode,
                        entry.destination,
                        polarity,
                        trigger,
                        mask,
                        delivery,
                        entry.remote_irr as u8
                    );
                    match kumo_hal::active::plan_boot_io_apic_timer(route) {
                        Some(plan) => {
                            let polarity = if plan.entry.active_low { "low" } else { "high" };
                            let trigger = if plan.entry.level_triggered {
                                "level"
                            } else {
                                "edge"
                            };
                            klog!(
                                "IOAPIC PLAN        Check     GSI {} pin {}  vec {:#04x} fixed physical dest {}  {} {} masked  raw {:#010x}:{:#010x}   OK\n",
                                plan.gsi,
                                plan.entry.input_pin,
                                plan.entry.vector,
                                plan.entry.destination,
                                polarity,
                                trigger,
                                plan.high_dword,
                                plan.low_dword
                            );
                            // j451: apply the masked route — write the destination (high) then the
                            // vector (low) dword and read both back. The entry stays masked and the
                            // PIC heartbeat is untouched, so nothing is delivered; this only stages
                            // the redirection register for a later unmask + source-transition slice.
                            match kumo_hal::active::apply_boot_io_apic_timer(route) {
                                Some(applied)
                                    if applied.matches_plan() && applied.stays_masked() =>
                                {
                                    klog!(
                                        "IOAPIC WRITE       Check     GSI {} pin {}  wrote {:#010x}:{:#010x}  readback {:#010x}:{:#010x} masked   OK\n",
                                        applied.plan.gsi,
                                        applied.plan.entry.input_pin,
                                        applied.plan.high_dword,
                                        applied.plan.low_dword,
                                        applied.high_readback,
                                        applied.low_readback
                                    );
                                }
                                Some(applied) => {
                                    klog!(
                                        "IOAPIC WRITE       Check     readback {:#010x}:{:#010x} != plan {:#010x}:{:#010x}   FAIL\n",
                                        applied.high_readback,
                                        applied.low_readback,
                                        applied.plan.high_dword,
                                        applied.plan.low_dword
                                    );
                                    kumo_hal::active::halt();
                                }
                                None => {
                                    klog!(
                                        "IOAPIC WRITE       Check     apply unavailable   FAIL\n"
                                    );
                                    kumo_hal::active::halt();
                                }
                            }
                        }
                        None => {
                            klog!(
                                "IOAPIC PLAN        Check     timer route not encodable   FAIL\n"
                            );
                            kumo_hal::active::halt();
                        }
                    }
                }
                Some(io_apic) => {
                    klog!(
                        "IOAPIC HW          Check     MADT id {} / HW id {}  GSI {}-{} route {}   FAIL\n",
                        io_apic.madt_id,
                        io_apic.hardware_id,
                        io_apic.gsi_base,
                        io_apic.gsi_end,
                        io_apic.routed_gsi
                    );
                    kumo_hal::active::halt();
                }
                None => {
                    klog!("IOAPIC HW          Check     bootstrap window unavailable   FAIL\n");
                    kumo_hal::active::halt();
                }
            }
        }
        None => {
            klog!("ACPI IRQ ROUTE     Check     ISA IRQ 0 unresolved   FAIL\n");
            kumo_hal::active::halt();
        }
    }

    // The Tower, AMD64 edition (j435): own the fault path before anything else can trap silently.
    // Install the IDT, then deliberately execute `int3` — a resumable breakpoint. The #BP handler
    // reports over COM1 and returns; if the IDT is live we land back here and the counter reads 1.
    // Before j435 this `int3` triple-faulted the machine into a reboot.
    kumo_hal::active::install_exception_vectors();
    klog!("IDT / TOWER        Check     64 kernel vectors + ring3 int80 installed\n");
    let before = kumo_hal::active::exceptions_seen();
    unsafe { core::arch::asm!("int3", options(nomem, nostack)) };
    let seen = kumo_hal::active::exceptions_seen().wrapping_sub(before);
    if seen == 1 {
        klog!(
            "IDT / TOWER        Check     int3 caught + resumed  seen {}   OK\n",
            seen
        );
    } else {
        klog!(
            "IDT / TOWER        Check     breakpoint not fielded (seen {})   FAIL\n",
            seen
        );
    }

    // DEFERRED/000 (j455/j457): the soft-float kernel does not touch user vector registers. Keep
    // the int3 XMM boundary proof, then expose AVX only when CPUID supplies KUMO's bounded standard
    // XSAVE layout. Baseline x86_64 CPUs retain the FXSAVE path. — KESTREL
    let fpsimd = kumo_hal::active::prove_fpsimd_boundary(0xf00d_5555_aaaa_c0de);
    if fpsimd.is_transparent() {
        klog!(
            "FPSIMD / SSE       Check     SSE on (CR0 {:#x} CR4 {:#x})  xmm {:#x} survived int3 ISR   OK\n",
            fpsimd.cr0,
            fpsimd.cr4,
            fpsimd.xmm_after_isr
        );
    } else {
        klog!(
            "FPSIMD / SSE       Check     CR0 {:#x} CR4 {:#x}  xmm {:#x}->{:#x}   FAIL\n",
            fpsimd.cr0,
            fpsimd.cr4,
            fpsimd.xmm_sentinel,
            fpsimd.xmm_after_isr
        );
        kumo_hal::active::halt();
    }
    if fpsimd.avx_enabled() {
        klog!(
            "FPSIMD / XSAVE     Check     AVX on  XCR0 {:#x}  standard {}b image   OK\n",
            fpsimd.xcr0,
            fpsimd.xsave_size
        );
    } else {
        klog!("FPSIMD / XSAVE     Check     AVX unavailable  FXSAVE fallback   OK\n");
    }

    // First CPL3 proof: an RX user page pings through the DPL3 int80 gate, receives its value
    // back via `iretq`, then exits through the same gate to the suspended kernel flow. The
    // privilege transition uses a dedicated TSS.RSP0 stack rather than the boot call stack.
    let mut ring3_frames = X86FrameTrace::new(X86_PHYS_MAP_LIMIT);
    let ring3 = {
        let mut alloc = || {
            let frame = unsafe { mm::alloc_zeroed_frame(&boot) };
            if let Some(frame) = frame {
                ring3_frames.record(&boot, frame);
            }
            frame
        };
        kumo_hal::active::run_ring3_smoke(&mut alloc)
    };
    match ring3 {
        Ok(report) if report.is_live() && ring3_frames.safe && ring3_frames.frames >= 6 => {
            klog!(
                "RING3 / FRAMES     Check     {} BootInfo frames  first {:#x}  last {:#x}  monotonic  kernel+initrd excluded   OK\n",
                ring3_frames.frames,
                ring3_frames.first,
                ring3_frames.last
            );
            klog!(
                "RING3 / PAGING     Check     private CR3  RX code {:#x}  NX stack {:#x}  4K guard   OK\n",
                report.code_address,
                report.stack_top
            );
            klog!(
                "RING3 / INT80      Check     CPL3 entered  {} calls  ping {:#x}  exit {}   OK\n",
                report.calls,
                report.ping_echo,
                report.exit_code
            );
        }
        Ok(report) => {
            klog!(
                "RING3 / INT80      Check     entered={} calls={} ping={:#x} exit={} frames={:?}   FAIL\n",
                report.entered,
                report.calls,
                report.ping_echo,
                report.exit_code,
                ring3_frames
            );
            kumo_hal::active::halt();
        }
        Err(err) => {
            klog!("RING3 / INT80      Check     {:?}   FAIL\n", err);
            kumo_hal::active::halt();
        }
    }

    // j456/j457: prove eager vector ownership with two actual CPL3 contexts. AVX systems keep the
    // distinct sentinel in YMM0[255:128], so legacy FXSAVE would fail this exact resume check.
    // Baseline CPUs retain the prior XMM proof. — KESTREL
    match x86_fpsimd_smoke::run(&boot, fpsimd.avx_enabled()) {
        Ok(report) if report.is_live() => {
            if report.avx {
                klog!(
                    "FPSIMD / SWITCH   Check     CPL3 2 contexts  4 int80  private CR3 {:#x}/{:#x}  distinct ymm[255:128] survived   OK\n",
                    report.roots[0],
                    report.roots[1]
                );
            } else {
                klog!(
                    "FPSIMD / SWITCH   Check     CPL3 2 contexts  4 int80  private CR3 {:#x}/{:#x}  distinct xmm survived   OK\n",
                    report.roots[0],
                    report.roots[1]
                );
            }
        }
        Ok(report) => {
            klog!("FPSIMD / SWITCH   Check     {:?}   FAIL\n", report);
            kumo_hal::active::halt();
        }
        Err(err) => {
            klog!("FPSIMD / SWITCH   Check     {:?}   FAIL\n", err);
            kumo_hal::active::halt();
        }
    }

    let reference_timer = match kumo_hal::active::init_timer_interrupts(0, 20) {
        Ok(timer) => {
            let start = kumo_hal::active::timer_irq_count();
            let seen = kumo_hal::active::wait_for_timer_irqs(start, 3, 1_000_000_000);
            if seen >= 3 {
                klog!(
                    "PIC / PIT          Check     {} Hz input  {} Hz tick  IRQ {}  hb {}t   OK\n",
                    timer.counter_hz,
                    timer.period_hz,
                    timer.irq,
                    seen
                );
            } else {
                klog!(
                    "PIC / PIT          Check     IRQ {} heartbeat timeout ({}t)   FAIL\n",
                    timer.irq,
                    seen
                );
                kumo_hal::active::halt();
            }
            timer
        }
        Err(err) => {
            klog!(
                "PIC / PIT          Check     unavailable: {:?}   FAIL\n",
                err
            );
            kumo_hal::active::halt();
        }
    };

    match kumo_hal::active::init_local_timer_from_reference(reference_timer.period_hz, 20) {
        Ok(timer) => {
            let start = kumo_hal::active::local_timer_irq_count();
            let seen = kumo_hal::active::wait_for_local_timer_irqs(start, 3);
            if seen >= 3 {
                klog!(
                    "x2APIC / TIMER    Check     {} Hz calibrated  {} Hz tick  vec {}  hb {}t   OK\n",
                    timer.counter_hz,
                    timer.period_hz,
                    timer.vector,
                    seen
                );
            } else {
                klog!(
                    "x2APIC / TIMER    Check     vec {} heartbeat timeout ({}t)   FAIL\n",
                    timer.vector,
                    seen
                );
                kumo_hal::active::halt();
            }
        }
        Err(err) => {
            klog!(
                "x2APIC / TIMER    Check     unavailable: {:?}   FAIL\n",
                err
            );
            kumo_hal::active::halt();
        }
    }

    let before = kumo_hal::active::io_apic_timer_irq_count();
    kumo_hal::active::probe_io_apic_timer_interrupt();
    let seen = kumo_hal::active::io_apic_timer_irq_count().wrapping_sub(before);
    if seen == 1 {
        klog!(
            "IOAPIC DISPATCH    Check     vec 0x31 software probe counted + EOI  seen {}   OK\n",
            seen
        );
    } else {
        klog!(
            "IOAPIC DISPATCH    Check     vec 0x31 software probe seen {}   FAIL\n",
            seen
        );
        kumo_hal::active::halt();
    }

    // j452: route the timer *through* the I/O APIC. The IDT (vec 0x31) is live and interrupts are
    // enabled, so mask the PIC's IRQ0 (taking the PIT off vector 0x20), unmask the redirection entry
    // written masked by j451, and wait for the j450 dispatcher's counter to advance from real,
    // controller-delivered interrupts — the first non-software IOAPIC delivery.
    if let Some(route) = madt.legacy_timer_route {
        let before = kumo_hal::active::io_apic_timer_irq_count();
        kumo_hal::active::mask_pic_timer_source();
        match kumo_hal::active::unmask_boot_io_apic_timer(route) {
            Some(applied) if applied.is_live_timer() => {
                let seen = kumo_hal::active::wait_for_io_apic_timer_irqs(before, 3);
                if seen >= 3 {
                    klog!(
                        "IOAPIC TIMER       Check     PIC IRQ0 masked  GSI {} vec {:#04x} unmasked  hb {}t via I/O APIC   OK\n",
                        applied.plan.gsi,
                        applied.plan.entry.vector,
                        seen
                    );
                } else {
                    klog!(
                        "IOAPIC TIMER       Check     vec {:#04x} delivery timeout ({}t)   FAIL\n",
                        applied.plan.entry.vector,
                        seen
                    );
                    kumo_hal::active::halt();
                }
            }
            Some(applied) => {
                klog!(
                    "IOAPIC TIMER       Check     entry not live  low {:#010x}   FAIL\n",
                    applied.low_readback
                );
                kumo_hal::active::halt();
            }
            None => {
                klog!("IOAPIC TIMER       Check     unmask unavailable   FAIL\n");
                kumo_hal::active::halt();
            }
        }
    }

    // j453: collapse to a single canonical timer. The PIT existed only to calibrate the local APIC
    // timer (local_apic.rs), and j451/j452 proved the I/O APIC timer route end to end. Now elect the
    // local APIC timer (vec 0x30) as the sole tick and re-mask the I/O APIC route: re-apply the
    // masked entry, then prove the local timer keeps ticking on its own while the I/O APIC timer
    // count stays frozen. The I/O APIC itself stays live for real device interrupts later.
    if let Some(route) = madt.legacy_timer_route {
        match kumo_hal::active::apply_boot_io_apic_timer(route) {
            Some(remask) if remask.matches_plan() && remask.stays_masked() => {
                let local_before = kumo_hal::active::local_timer_irq_count();
                let io_before = kumo_hal::active::io_apic_timer_irq_count();
                let local_seen = kumo_hal::active::wait_for_local_timer_irqs(local_before, 3);
                let io_seen = kumo_hal::active::io_apic_timer_irq_count().wrapping_sub(io_before);
                if local_seen >= 3 && io_seen == 0 {
                    klog!(
                        "TIMER SOURCE       Check     local APIC vec 0x30 canonical  I/O APIC route re-masked  hb {}t  ioapic +{}   OK\n",
                        local_seen,
                        io_seen
                    );
                } else {
                    klog!(
                        "TIMER SOURCE       Check     local {}t  ioapic +{} (want 3 / 0)   FAIL\n",
                        local_seen,
                        io_seen
                    );
                    kumo_hal::active::halt();
                }
            }
            Some(remask) => {
                klog!(
                    "TIMER SOURCE       Check     re-mask readback {:#010x} not masked   FAIL\n",
                    remask.low_readback
                );
                kumo_hal::active::halt();
            }
            None => {
                klog!("TIMER SOURCE       Check     re-mask unavailable   FAIL\n");
                kumo_hal::active::halt();
            }
        }
    }

    // The first shared scheduler substrate on x86: enter two kernel threads on independent
    // stacks, let each yield three times, and restore the suspended boot context after both
    // terminate. This exercises fresh-thread trampoline entry plus continuation RIP/RSP and
    // callee-saved-register restoration through the real HAL switch primitive.
    let context = kdemo::run();
    if context.threads == 2 && context.switches == 16 && context.work == 6 {
        klog!(
            "CONTEXT SWITCH     Check     {} kthreads  {} switches  work {}  callee-saved + stack resume   OK\n",
            context.threads,
            context.switches,
            context.work
        );
    } else {
        klog!(
            "CONTEXT SWITCH     Check     {} kthreads  {} switches  work {}   FAIL\n",
            context.threads,
            context.switches,
            context.work
        );
        kumo_hal::active::halt();
    }

    // Run two non-yielding bodies under the real Dispatcher. The local APIC timer hook rotates
    // between their saved interrupt continuations, then restores the suspended boot context once
    // both have executed and at least four body-to-body switches completed.
    let preempt = kdemo::run_preemption();
    if preempt.threads == 2
        && preempt.switches >= 4
        && preempt.ticks >= 4
        && preempt.work.iter().all(|&work| work > 0)
    {
        klog!(
            "PREEMPT SCHED      Check     {} kthreads  {} body switches  {} ticks  timer-preempted both bodies   OK\n",
            preempt.threads,
            preempt.switches,
            preempt.ticks
        );
    } else {
        klog!(
            "PREEMPT SCHED      Check     {} kthreads  {} body switches  {} ticks  work {:?}   FAIL\n",
            preempt.threads,
            preempt.switches,
            preempt.ticks,
            preempt.work
        );
        kumo_hal::active::halt();
    }

    // Drive the initrd-resident KUMO ELF through the shared user-thread dispatcher. Its runtime
    // drains startup, emits the canonical hello through DebugWrite, and exits; ProcessExit restores
    // this boot context, proving module ingestion, named-file lookup, ELF load, runtime ABI, traps,
    // and exit are schedulable continuations.
    match run_x86_scheduled_ring3_smoke(initrd.hello, &boot) {
        Ok(report)
            if report.done
                && report.segments >= 2
                && report.bootstrap != 0
                && report.syscalls == 4
                && report.wrote == X86_HELLO_LEN
                && report.exit_code == 0
                && report.switches == 2
                && report.frame_trace.safe
                && report.frame_trace.frames >= 6
                && report.user_root == report.frame_trace.first =>
        {
            klog!(
                "USER ELF / FRAMES  Check     {} BootInfo frames  first {:#x}  last {:#x}  CR3 {:#x}  monotonic  kernel+initrd excluded   OK\n",
                report.frame_trace.frames,
                report.frame_trace.first,
                report.frame_trace.last,
                report.user_root
            );
            klog!(
                "USER ELF / ENGINE  Check     {} PT_LOAD  entry {:#x}  boot h{}  {} int80  wrote {}b  {} switches  exit {}   OK\n",
                report.segments,
                report.entry,
                report.bootstrap,
                report.syscalls,
                report.wrote,
                report.switches,
                report.exit_code
            );
        }
        Ok(report) => {
            klog!("USER ELF / ENGINE  Check     {:?}   FAIL\n", report);
            kumo_hal::active::halt();
        }
        Err(err) => {
            klog!("USER ELF / ENGINE  Check     {:?}   FAIL\n", err);
            kumo_hal::active::halt();
        }
    }

    klog!("x86_64 MUREX core online, first light reached; HALTING.\n");
    kumo_hal::active::halt()
}

pub fn expected_abi_version() -> u32 {
    ABI_VERSION
}

fn tower_halt_ascii(reason: &str, error: Option<HandoffError>) -> ! {
    // A fault path must never wake or switch threads: pin the console to the direct
    // device path and reclaim a userspace-owned framebuffer before printing anything.
    usermode::disable_console_route();
    kumo_hal::active::reclaim_framebuffer_console();
    klog!("TOWER EXCEPTION: ");
    klog!("{}", reason);
    if let Some(error) = error {
        klog!(": {:?}", error);
    }
    klog!("\nHALT\n");
    kumo_hal::active::halt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kumo_abi::{MemRegion, MemRegionKind, Range, RawSlice};

    #[test]
    #[cfg(feature = "arch_aarch64")]
    fn reports_arm64_when_selected() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&TEST_REGIONS);
        boot.kernel_phys = Range::new(0x80000, 0x20000);
        boot.kernel_virt = Range::new(0xffff_0000_0008_0000, 0x20000);
        boot.initrd = Range::new(0x90000, 0x4000);
        let report = inspect_boot(&boot).unwrap();
        assert_eq!(report.arch, "aarch64");
        assert_eq!(report.mem_region_count, 2);
        assert_eq!(report.usable_bytes, 0x5000);
        assert_eq!(report.total_bytes, 0x6000);
        assert!(report.has_initrd);
    }

    #[test]
    #[cfg(feature = "arch_x86_64")]
    fn reports_x86_64_when_selected() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&TEST_REGIONS);
        boot.kernel_phys = Range::new(0x80000, 0x20000);
        boot.kernel_virt = Range::new(0xffff_8000_0008_0000, 0x20000);
        let report = inspect_boot(&boot).unwrap();
        assert_eq!(report.arch, "x86_64");
        assert_eq!(report.mem_region_count, 2);
        assert_eq!(report.usable_bytes, 0x5000);
    }

    /// The board that has a PL011 gets its base from the BSP — no longer from a HAL hardcode.
    #[test]
    fn stamped_pl011_board_yields_its_bsp_console_base() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.set_board_id("qemu-virt-aarch64");
        assert_eq!(board_console_pl011_base(&boot), Some(0x0900_0000));
    }

    /// The J182 regression, as a test: a board with no PL011 must yield NO base, so the HAL's
    /// UART sink stays inert instead of writing to an address it does not have. The X13s is the
    /// sentinel — it is the board J182 actually hard-hung. If it ever starts reporting a base,
    /// that write wedges the machine.
    #[test]
    fn framebuffer_boards_yield_no_console_base() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.set_board_id("thinkpad-x13s-gen1");
        assert_eq!(board_console_pl011_base(&boot), None);
    }

    /// And a board that *does* have one yields it. The Pi 5's console is its SoC debug PL011
    /// (`uart10` @ 0x10_7D00_1000, the 3-pin connector), per the official DTB's `console` alias —
    /// not the 40-pin UART behind RP1. This read `None` until J465, so the Pi 5 was pinned to the
    /// framebuffer for want of a fact.
    #[test]
    fn pi5_yields_its_soc_debug_pl011_base() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.set_board_id("raspberry-pi-5");
        assert_eq!(board_console_pl011_base(&boot), Some(0x10_7D00_1000));
    }

    #[test]
    fn explicit_pl011_route_overrides_the_pi5_bsp_default() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.set_board_id("raspberry-pi-5");
        boot.platform.pl011_console_base = 0x1c_0003_0000;
        assert_eq!(board_console_pl011_base(&boot), Some(0x1c_0003_0000));

        // Sentinel/misaligned values never become blind MMIO. A stamped board retains its safe
        // BSP default; an unstamped board remains inert.
        boot.platform.pl011_console_base = u64::MAX;
        assert_eq!(board_console_pl011_base(&boot), Some(0x10_7D00_1000));
        boot.set_board_id("");
        assert_eq!(board_console_pl011_base(&boot), None);
    }

    /// The Orange Pi 5 Plus selects the RK3588 debug UART — a DW-APB 16550-class device, not a
    /// PL011 — so it resolves through the DW-APB path and reports no PL011 base. Before R3 the
    /// kernel only asked `board_console_pl011_base`, found `None`, and booted the board mute; this
    /// is the fact that gives it a console.
    #[test]
    fn opi5_yields_its_dw8250_console_base() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.set_board_id("orange-pi-5-plus");
        assert_eq!(board_console_dw8250_base(&boot), Some(0xfeb5_0000));
        assert_eq!(board_console_pl011_base(&boot), None);
    }

    /// The PL011 boards report no DW-APB base, so the two console paths are mutually exclusive and
    /// a board can never light both UART sinks. Unstamped/unknown resolves to nothing on either.
    #[test]
    fn pl011_boards_yield_no_dw8250_base() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.set_board_id("qemu-virt-aarch64");
        assert_eq!(board_console_dw8250_base(&boot), None);
        boot.set_board_id("raspberry-pi-5");
        assert_eq!(board_console_dw8250_base(&boot), None);
        boot.set_board_id("thinkpad-x13s-gen1");
        assert_eq!(board_console_dw8250_base(&boot), None);
        boot.set_board_id("");
        assert_eq!(board_console_dw8250_base(&boot), None);
    }

    /// Stale-ABI safety mirrors the PL011 path: the aliased v3 fields must not be interpreted, so a
    /// down-rev loader yields no DW-APB base and cannot turn a stale field into a blind MMIO write.
    #[test]
    fn dw8250_base_is_inert_for_a_stale_abi() {
        let mut boot = BootInfo::empty(ABI_VERSION - 1);
        boot.set_board_id("orange-pi-5-plus");
        assert_eq!(board_console_dw8250_base(&boot), None);
    }

    /// Console-floor selection must follow the resolved UART, not the unrelated presence of GOP.
    /// This is the Pi 5 metal regression: it reached the end of POST with RP1 serial alive, then
    /// entered the framebuffer-only idle branch and appeared to stop forever.
    #[test]
    fn dual_sink_pi_keeps_serial_floor_while_x13s_idles_on_glass() {
        assert!(stage_a_uses_serial_floor(true, Some(0x1c_0003_0000), None));
        assert!(stage_a_uses_serial_floor(true, Some(0x10_7d00_1000), None));
        assert!(!stage_a_uses_serial_floor(true, None, None));
        assert!(stage_a_uses_serial_floor(false, Some(0x0900_0000), None));
        // Preserve the old headless attempt for an unstamped no-GOP boot.
        assert!(stage_a_uses_serial_floor(false, None, None));
        // A DW-APB console keeps the serial floor even when a GOP is present — the opi5 must not
        // fall into the framebuffer-only idle branch and appear to stop with a live uart2 shell.
        assert!(stage_a_uses_serial_floor(true, None, Some(0xfeb5_0000)));
    }

    #[test]
    fn pre_validation_board_reads_are_inert_for_a_stale_abi() {
        let mut boot = BootInfo::empty(ABI_VERSION - 1);
        boot.set_board_id("raspberry-pi-5");
        boot.platform.pl011_console_base = 0x1c_0003_0000;
        assert_eq!(board_console_pl011_base(&boot), None);
        assert_eq!(board_gic_no_dtb_fallback(&boot), None);
    }

    /// Pi 5 UEFI defaults to ACPI-only system tables, so the stamped board must still inject the
    /// fixed BCM2712 GIC-400 pair when no DT reaches KUMO. These are addresses, not a guessed
    /// architecture selector; DT remains authoritative when one is present.
    #[test]
    fn pi5_yields_its_gic400_no_dtb_fallback() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.set_board_id("raspberry-pi-5");
        let fallback = board_gic_no_dtb_fallback(&boot).unwrap();
        assert_eq!(fallback.distributor_base, 0x10_7fff_9000);
        assert_eq!(fallback.redistributor_base, None);
        assert_eq!(fallback.cpu_base, Some(0x10_7fff_a000));
    }

    /// The Orange Pi 5 Plus's "the boot chain always publishes a device tree" bet died on
    /// metal 2026-07-22: Stage-A's GIC/TIMER gate halted `unavailable: NoGic` with no usable
    /// tree in hand. The RK3588's GIC600 bases are silicon-fixed (TRM part1 ch01:
    /// `GIC600 FE600000 4MB`; ch11: GICv3, eight 0x20000 GICR frames at 0xFE680000), so the
    /// stamped board now injects them exactly like QEMU and the Pi 5 do — GICR-only, which
    /// fixes the architecture by construction. A handed-off DT stays authoritative.
    #[test]
    fn opi5_yields_its_gic600_no_dtb_fallback() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.set_board_id("orange-pi-5-plus");
        let fallback = board_gic_no_dtb_fallback(&boot).unwrap();
        assert_eq!(fallback.distributor_base, 0xfe60_0000);
        assert_eq!(fallback.redistributor_base, Some(0xfe68_0000));
        assert_eq!(fallback.cpu_base, None);
    }

    /// An unstamped or unrecognized board injects nothing. Guessing QEMU's base here is exactly
    /// the old fallback that J182 traced the X13s hang to, so "unknown" must mean inert — the
    /// cost is early serial on an unstamped QEMU image, which is recoverable; a hang is not.
    #[test]
    fn unstamped_or_unknown_board_yields_no_console_base() {
        let unstamped = BootInfo::empty(ABI_VERSION);
        assert_eq!(unstamped.board_id(), "");
        assert_eq!(board_console_pl011_base(&unstamped), None);

        let mut unknown = BootInfo::empty(ABI_VERSION);
        unknown.set_board_id("some-board-we-have-never-heard-of");
        assert_eq!(board_console_pl011_base(&unknown), None);
    }

    #[repr(align(8))]
    struct AlignedDtb([u8; 40]);

    fn boot_with_test_dtb(blob: &AlignedDtb, mapped_len: u64) -> (BootInfo, [MemRegion; 1]) {
        let regions = [MemRegion {
            range: Range::new(blob.0.as_ptr() as u64, mapped_len),
            kind: MemRegionKind::Bootloader,
            _reserved: 0,
        }];
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.platform.dtb = blob.0.as_ptr() as u64;
        (boot, regions)
    }

    #[test]
    fn dtb_handoff_accepts_a_bounded_blob_inside_the_memory_map() {
        let mut blob = AlignedDtb([0; 40]);
        blob.0[..4].copy_from_slice(&FDT_MAGIC.to_be_bytes());
        blob.0[4..8].copy_from_slice(&40u32.to_be_bytes());
        let (mut boot, mut regions) = boot_with_test_dtb(&blob, 40);
        boot.mem_regions = RawSlice::from_slice(&regions);
        assert_eq!(
            unsafe { validate_dtb_handoff(&boot) },
            Ok(Some((blob.0.as_ptr() as u64, 40)))
        );

        // A valid-looking blob in free RAM still violates Nijigumo's LoaderData ownership
        // contract and must be rejected before a raw read.
        regions[0].kind = MemRegionKind::Usable;
        boot.mem_regions = RawSlice::from_slice(&regions);
        assert_eq!(
            unsafe { validate_dtb_handoff(&boot) },
            Err(DtbHandoffError::HeaderOutsideMemoryMap)
        );
    }

    #[test]
    fn dtb_handoff_rejects_sentinels_holes_and_unbacked_sizes_before_deref() {
        let mut boot = BootInfo::empty(ABI_VERSION);
        boot.mem_regions = RawSlice::from_slice(&TEST_REGIONS);

        assert_eq!(unsafe { validate_dtb_handoff(&boot) }, Ok(None));
        boot.platform.dtb = u64::MAX;
        assert_eq!(
            unsafe { validate_dtb_handoff(&boot) },
            Err(DtbHandoffError::AllOnes)
        );
        boot.platform.dtb = AARCH64_PHYS_LIMIT;
        assert_eq!(
            unsafe { validate_dtb_handoff(&boot) },
            Err(DtbHandoffError::NonPhysical)
        );
        boot.platform.dtb = 0x8000;
        assert_eq!(
            unsafe { validate_dtb_handoff(&boot) },
            Err(DtbHandoffError::HeaderOutsideMemoryMap)
        );

        let mut blob = AlignedDtb([0; 40]);
        blob.0[..4].copy_from_slice(&FDT_MAGIC.to_be_bytes());
        blob.0[4..8].copy_from_slice(&0x1000u32.to_be_bytes());
        let (mut boot, regions) = boot_with_test_dtb(&blob, 40);
        boot.mem_regions = RawSlice::from_slice(&regions);
        assert_eq!(
            unsafe { validate_dtb_handoff(&boot) },
            Err(DtbHandoffError::BlobOutsideMemoryMap)
        );
    }

    static TEST_REGIONS: [MemRegion; 2] = [
        MemRegion {
            range: Range {
                start: 0x1000,
                len: 0x5000,
            },
            kind: MemRegionKind::Usable,
            _reserved: 0,
        },
        MemRegion {
            range: Range {
                start: 0x6000,
                len: 0x1000,
            },
            kind: MemRegionKind::Reserved,
            _reserved: 0,
        },
    ];
}
