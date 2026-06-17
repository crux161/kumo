#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

mod cjk_font;

pub const ARCH: &str = "aarch64";

pub fn arch_name() -> &'static str {
    ARCH
}

// ---- Thread contexts -------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ThreadContext {
    x19_entry: u64,
    x20_arg: u64,
    x21: u64,
    x22: u64,
    x23: u64,
    x24: u64,
    x25: u64,
    x26: u64,
    x27: u64,
    x28: u64,
    x29_fp: u64,
    x30_lr: u64,
    sp: u64,
    user: bool,
}

impl ThreadContext {
    pub fn new(entry: usize, arg: usize, stack_top: usize, user: bool) -> Self {
        Self {
            x19_entry: entry as u64,
            x20_arg: arg as u64,
            x30_lr: context_trampoline_addr(),
            sp: stack_top as u64,
            user,
            ..Self::default()
        }
    }

    pub const fn entry(self) -> u64 {
        self.x19_entry
    }

    pub const fn arg(self) -> u64 {
        self.x20_arg
    }

    pub const fn stack_top(self) -> u64 {
        self.sp
    }

    pub const fn is_user(self) -> bool {
        self.user
    }
}

#[cfg(target_os = "none")]
fn context_trampoline_addr() -> u64 {
    extern "C" {
        fn kumo_context_trampoline();
    }
    kumo_context_trampoline as *const () as usize as u64
}

#[cfg(not(target_os = "none"))]
fn context_trampoline_addr() -> u64 {
    0
}

#[cfg(target_os = "none")]
core::arch::global_asm!(
    ".section .text.kumo_context",
    ".global kumo_context_switch",
    "kumo_context_switch:",
    "  stp x19, x20, [x0, #0]",
    "  stp x21, x22, [x0, #16]",
    "  stp x23, x24, [x0, #32]",
    "  stp x25, x26, [x0, #48]",
    "  stp x27, x28, [x0, #64]",
    "  stp x29, x30, [x0, #80]",
    "  mov x2, sp",
    "  str x2, [x0, #96]",
    "  ldp x19, x20, [x1, #0]",
    "  ldp x21, x22, [x1, #16]",
    "  ldp x23, x24, [x1, #32]",
    "  ldp x25, x26, [x1, #48]",
    "  ldp x27, x28, [x1, #64]",
    "  ldp x29, x30, [x1, #80]",
    "  ldr x2, [x1, #96]",
    "  mov sp, x2",
    "  ret",
    ".global kumo_context_trampoline",
    "kumo_context_trampoline:",
    // A thread first entered from an IRQ-context switch inherits the IRQ mask from
    // exception entry. Drop it before jumping into the body so timer preemption keeps
    // working on freshly-started threads.
    "  msr daifclr, #2",
    "  mov x0, x20",
    "  blr x19",
    "1:",
    "  wfe",
    "  b 1b",
);

#[cfg(target_os = "none")]
pub unsafe fn switch_context(prev: *mut ThreadContext, next: *const ThreadContext) {
    extern "C" {
        fn kumo_context_switch(prev: *mut ThreadContext, next: *const ThreadContext);
    }
    unsafe { kumo_context_switch(prev, next) };
}

#[cfg(not(target_os = "none"))]
pub unsafe fn switch_context(_prev: *mut ThreadContext, _next: *const ThreadContext) {}

/// Unmask IRQs. A context resumed from a switch performed inside an exception handler
/// (e.g. the boot flow resumed by a user thread's `ProcessExit` SVC) inherits that
/// handler's masked DAIF — the same hazard `kumo_context_trampoline` clears for fresh
/// kernel threads. Call this after such a resume so the timer keeps ticking.
#[cfg(target_os = "none")]
pub fn irq_unmask() {
    unsafe { core::arch::asm!("msr daifclr, #0x2", "isb", options(nostack, nomem)) };
}

#[cfg(not(target_os = "none"))]
pub fn irq_unmask() {}

// =====================================================================
// Stage-A early console
//
// Two sinks, chosen at runtime by what the board actually has:
//   * A linear framebuffer (from the UEFI GOP, handed over in BootInfo) — used on
//     real hardware like the ThinkPad X13s, which has no UART at the PL011 address.
//   * The PL011 UART0 at 0x09000000 — used on QEMU `virt` and boards that expose it.
// `set_framebuffer` (called from kmain when BootInfo carries one) switches the
// console to the framebuffer; otherwise output goes to PL011.
// =====================================================================

const ORD: Ordering = Ordering::Relaxed;

// ---- PL011 UART0 ----------------------------------------------------

const PL011_BASE: usize = 0x0900_0000;
const UARTDR: usize = 0x00;
const UARTFR: usize = 0x18;
const UARTIBRD: usize = 0x24;
const UARTFBRD: usize = 0x28;
const UARTLCR_H: usize = 0x2c;
const UARTCR: usize = 0x30;
const UARTFR_RXFE: u32 = 1 << 4;
const UARTFR_TXFF: u32 = 1 << 5;
const UARTLCR_H_FEN: u32 = 1 << 4;
const UARTLCR_H_WLEN_8: u32 = 3 << 5;
const UARTCR_UARTEN: u32 = 1 << 0;
const UARTCR_TXE: u32 = 1 << 8;
const UARTCR_RXE: u32 = 1 << 9;

static UART_READY: AtomicBool = AtomicBool::new(false);

/// Added to every console MMIO physical address before dereferencing. 0 during early
/// boot (the identity/firmware map); [`console_use_physmap`] raises it to
/// [`mmu::PHYSMAP_BASE`] so console access rides TTBR1 — mapped no matter which TTBR0
/// (kernel identity or a user process tree) is active.
static CONSOLE_VA_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Switch the Stage-A console (PL011 + framebuffer) to physmap addressing. Call once
/// the kernel's split tables are live (`enable_kernel_mmu`): afterwards console output
/// needs no identity/EL1 window in user page tables — the journal-052 scaffold bridge.
#[cfg(target_os = "none")]
pub fn console_use_physmap() {
    CONSOLE_VA_OFFSET.store(mmu::PHYSMAP_BASE, ORD);
}

#[cfg(not(target_os = "none"))]
pub fn console_use_physmap() {}

/// Read bytes from a physical address into `dest`. Only safe when the physmap is live
/// (after `enable_kernel_mmu`) — the physical address is accessed at `PHYSMAP_BASE + phys`.
#[cfg(target_os = "none")]
pub fn read_phys(phys: u64, dest: &mut [u8]) {
    let src = (mmu::PHYSMAP_BASE + phys) as *const u8;
    unsafe { core::ptr::copy_nonoverlapping(src, dest.as_mut_ptr(), dest.len()) };
}

#[cfg(not(target_os = "none"))]
pub fn read_phys(_phys: u64, _dest: &mut [u8]) {}

fn pl011_reg(offset: usize) -> *mut u32 {
    (PL011_BASE as u64 + CONSOLE_VA_OFFSET.load(ORD) + offset as u64) as *mut u32
}

fn pl011_init() {
    unsafe {
        pl011_reg(UARTCR).write_volatile(0);
        pl011_reg(UARTIBRD).write_volatile(1);
        pl011_reg(UARTFBRD).write_volatile(0);
        pl011_reg(UARTLCR_H).write_volatile(UARTLCR_H_FEN | UARTLCR_H_WLEN_8);
        pl011_reg(UARTCR).write_volatile(UARTCR_UARTEN | UARTCR_TXE | UARTCR_RXE);
    }
}

fn pl011_putc(byte: u8) {
    unsafe {
        while pl011_reg(UARTFR).read_volatile() & UARTFR_TXFF != 0 {
            core::hint::spin_loop();
        }
        pl011_reg(UARTDR).write_volatile(byte as u32);
    }
}

// ---- Framebuffer text console (PSF2 8x16 font) ----------------------

/// Embedded 8x16 console font (PSF2). Kept in-tree so KUMO stays self-contained.
const FONT: &[u8] = include_bytes!("../font8x16.psf");
const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;
// Jet Alone-style phosphor green on black. Green is the middle byte, so 0x0000_ff00 is
// the same pixel in RGB888x and BGR888x — format-agnostic like white was.
const FG: u32 = 0x0000_ff00; // phosphor green
const BG: u32 = 0x0000_0000; // black

static FB_PRESENT: AtomicBool = AtomicBool::new(false);
static FB_BASE: AtomicU64 = AtomicU64::new(0);
static FB_LEN_PX: AtomicUsize = AtomicUsize::new(0);
static FB_WIDTH: AtomicU32 = AtomicU32::new(0);
static FB_HEIGHT: AtomicU32 = AtomicU32::new(0);
static FB_STRIDE: AtomicU32 = AtomicU32::new(0);
static FB_COL: AtomicU32 = AtomicU32::new(0);
static FB_ROW: AtomicU32 = AtomicU32::new(0);

fn font_field(offset: usize) -> usize {
    u32::from_le_bytes([
        FONT[offset],
        FONT[offset + 1],
        FONT[offset + 2],
        FONT[offset + 3],
    ]) as usize
}

/// The `charsize`-byte glyph for a printable ASCII byte, falling back to `?`.
fn glyph_rows(ch: u8) -> &'static [u8] {
    let header_size = font_field(8); // PSF2 headersize
    let charsize = font_field(20); // bytes per glyph
    let index = if (0x20..=0x7e).contains(&ch) {
        ch as usize
    } else {
        b'?' as usize
    };
    let start = header_size + index * charsize;
    if start + charsize <= FONT.len() {
        &FONT[start..start + charsize]
    } else {
        &FONT[header_size..header_size + charsize]
    }
}

/// Clean one cache line containing `addr` to the point of coherency. The display
/// controller scans the framebuffer from RAM; until `mm::enable_paging` remaps the
/// framebuffer as Normal-NC, CPU pixel writes can sit in the D-cache where the scanout
/// never sees them (a blank screen on real hardware even though the kernel is running).
/// No-op on the host; harmless on QEMU and on WC/Device buffers (a clean of a line
/// that was never cached does nothing).
#[cfg(target_os = "none")]
#[inline]
unsafe fn fb_clean_line(addr: usize) {
    unsafe {
        core::arch::asm!("dc cvac, {a}", a = in(reg) addr, options(nostack, preserves_flags))
    };
}

/// Blit one glyph into a 32-bpp framebuffer. Bounds-checked against `len_px`, so a
/// bad geometry truncates instead of scribbling past the buffer. Pure with respect
/// to console state, which makes it host-testable against an in-memory buffer.
///
/// # Safety
/// `base` must point at `len_px` writable `u32` pixels.
unsafe fn blit_glyph(
    base: *mut u32,
    len_px: usize,
    stride: usize,
    x_px: usize,
    y_px: usize,
    ch: u8,
    fg: u32,
    bg: u32,
) {
    let rows = glyph_rows(ch);
    for (ry, &bits) in rows.iter().enumerate() {
        let py = y_px + ry;
        let row_start = py.wrapping_mul(stride).wrapping_add(x_px);
        for rx in 0..GLYPH_W {
            let on = (bits >> (7 - rx)) & 1 != 0;
            let idx = row_start.wrapping_add(rx);
            if idx < len_px {
                unsafe { base.add(idx).write_volatile(if on { fg } else { bg }) };
            }
        }
        // Flush this row's pixels to RAM so the display actually shows them (see
        // `fb_clean_line`). The 8-pixel span is one cache line; clean both ends in case
        // it straddles a line boundary.
        #[cfg(target_os = "none")]
        if row_start < len_px {
            unsafe {
                fb_clean_line(base.add(row_start) as usize);
                let row_end = row_start + GLYPH_W - 1;
                if row_end < len_px {
                    fb_clean_line(base.add(row_end) as usize);
                }
            }
        }
    }
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
}

// ---- CJK console glyphs (DESIGN/005) --------------------------------
//
// ASCII keeps the 8x16 PSF path; non-ASCII Unicode scalars look up a curated 16x16
// double-width glyph (`cjk_font::CJK_GLYPHS`, binary-searchable). A codepoint absent from
// the table renders as the tofu box below — never blank, never a panic.

/// Missing-glyph "tofu": a hollow 16x16 box (the DESIGN/005 fail-safe — a CJK cell the
/// font lacks still leaves a legible, distinct mark on a diagnostic line).
const TOFU: [u8; 32] = [
    0x00, 0x00, 0x00, 0x00, 0x3f, 0xfc, 0x20, 0x04, 0x20, 0x04, 0x20, 0x04, 0x20, 0x04, 0x20, 0x04,
    0x20, 0x04, 0x20, 0x04, 0x20, 0x04, 0x20, 0x04, 0x20, 0x04, 0x3f, 0xfc, 0x00, 0x00, 0x00, 0x00,
];

/// The 16x16 bitmap (a 32-byte slice) for a Unicode scalar, or `None` if it is outside the
/// embedded set. Binary search over `cjk_font::CJK_FONT`'s sorted fixed-size records.
fn cjk_glyph(scalar: u32) -> Option<&'static [u8]> {
    let cp = u16::try_from(scalar).ok()?;
    let font = cjk_font::CJK_FONT;
    let rec = cjk_font::RECORD;
    let (mut lo, mut hi) = (0usize, cjk_font::GLYPH_COUNT);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let off = mid * rec;
        let code = u16::from_le_bytes([font[off], font[off + 1]]);
        if code == cp {
            return Some(&font[off + 2..off + rec]);
        } else if code < cp {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    None
}

/// Decode `bytes` (a valid-UTF-8 console fragment) into Unicode scalars, emitting each via
/// `emit`. Malformed input degrades to `U+FFFD` (which renders as tofu) rather than
/// faulting — a diagnostic console must never wedge on a stray byte. Pure, so host-tested.
fn for_each_scalar(bytes: &[u8], mut emit: impl FnMut(u32)) {
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            emit(b as u32);
            i += 1;
            continue;
        }
        let (need, init) = if b >= 0xf0 {
            (3usize, (b & 0x07) as u32)
        } else if b >= 0xe0 {
            (2, (b & 0x0f) as u32)
        } else if b >= 0xc0 {
            (1, (b & 0x1f) as u32)
        } else {
            (0, 0) // stray continuation byte
        };
        if need == 0 {
            emit(0xfffd);
            i += 1;
            continue;
        }
        let mut cp = init;
        let mut consumed = 0;
        let mut ok = true;
        while consumed < need {
            let j = i + 1 + consumed;
            if j >= bytes.len() || bytes[j] & 0xc0 != 0x80 {
                ok = false;
                break;
            }
            cp = (cp << 6) | (bytes[j] & 0x3f) as u32;
            consumed += 1;
        }
        if ok {
            emit(cp);
            i += 1 + need;
        } else {
            emit(0xfffd);
            i += 1 + consumed;
        }
    }
}

/// Blit a 16x16 double-width glyph (2 bytes/row, MSB = leftmost pixel) into a 32-bpp
/// framebuffer. Bounds-checked against `len_px` like [`blit_glyph`].
///
/// # Safety
/// `base` must point at `len_px` writable `u32` pixels.
unsafe fn blit_wide_glyph(
    base: *mut u32,
    len_px: usize,
    stride: usize,
    x_px: usize,
    y_px: usize,
    rows: &[u8],
    fg: u32,
    bg: u32,
) {
    for ry in 0..GLYPH_H {
        let bits = ((rows[ry * 2] as u16) << 8) | (rows[ry * 2 + 1] as u16);
        let py = y_px + ry;
        let row_start = py.wrapping_mul(stride).wrapping_add(x_px);
        for rx in 0..16 {
            let on = (bits >> (15 - rx)) & 1 != 0;
            let idx = row_start.wrapping_add(rx);
            if idx < len_px {
                unsafe { base.add(idx).write_volatile(if on { fg } else { bg }) };
            }
        }
        // Flush the 16-pixel span to RAM for the scanout (see `fb_clean_line`); 16 px is
        // two cache lines, so clean both ends.
        #[cfg(target_os = "none")]
        if row_start < len_px {
            unsafe {
                fb_clean_line(base.add(row_start) as usize);
                let row_end = row_start + 15;
                if row_end < len_px {
                    fb_clean_line(base.add(row_end) as usize);
                }
            }
        }
    }
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
}

/// Switch the Stage-A console to a linear framebuffer and clear it. Called from
/// kmain when BootInfo carries a usable GOP framebuffer.
pub fn set_framebuffer(base: u64, len_bytes: u64, width: u32, height: u32, stride: u32) {
    let stride = if stride == 0 { width } else { stride };
    let len_px = core::cmp::min(
        (len_bytes / 4) as usize,
        (stride as usize).saturating_mul(height as usize),
    );

    FB_BASE.store(base, ORD);
    FB_LEN_PX.store(len_px, ORD);
    FB_WIDTH.store(width, ORD);
    FB_HEIGHT.store(height, ORD);
    FB_STRIDE.store(stride, ORD);
    FB_COL.store(0, ORD);
    FB_ROW.store(0, ORD);

    // Clear to the black phosphor backdrop, then flush to RAM so it shows on real
    // hardware where the framebuffer may be write-back cached (see `fb_clean_line`).
    let ptr = base as *mut u32;
    let mut i = 0;
    while i < len_px {
        unsafe { ptr.add(i).write_volatile(BG) };
        i += 1;
    }
    #[cfg(target_os = "none")]
    unsafe {
        let mut j = 0;
        while j < len_px {
            fb_clean_line(ptr.add(j) as usize);
            j += 16; // 64-byte cache line / 4-byte pixel
        }
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }

    FB_PRESENT.store(true, ORD);
}

/// Fill the whole linear framebuffer with `color` (little-endian BGRx u32) and flush it
/// to RAM. Used as a bring-up backdrop and a "the kernel is executing and this
/// framebuffer is visible" proof that persists until something repaints over it. Painted
/// before any fallible work, so on a fresh board: a solid colour means entry +
/// relocation + cache coherency + a live framebuffer all work; an unchanged screen (or a
/// reset) means the kernel never got here (or these writes are not reaching the panel).
pub fn fb_fill(phys: u64, len_bytes: u64, color: u32) {
    if phys == 0 {
        return;
    }
    let len_px = (len_bytes / 4) as usize;
    let base = phys as *mut u32;
    let mut i = 0;
    while i < len_px {
        unsafe { base.add(i).write_volatile(color) };
        i += 1;
    }
    #[cfg(target_os = "none")]
    unsafe {
        let mut j = 0;
        while j < len_px {
            fb_clean_line(base.add(j) as usize);
            j += 16; // 64-byte cache line / 4-byte pixel
        }
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
}

/// Bring-up "POST code on screen": paint a solid 24px band of `color` across the full
/// width at row `y0`, **directly** into a linear BGRx framebuffer described by the raw
/// `BootInfo` fields — bypassing all console/`FB_PRESENT` state — and flush it to RAM.
/// Lets a boot milestone (or the exact point of an early hang) be seen on real hardware
/// before, and independent of, the text console. `color` is a little-endian BGRx u32:
/// blue = `0x0000_00ff`, green = `0x0000_ff00`, red = `0x00ff_0000`.
pub fn fb_paint_band(phys: u64, len_bytes: u64, width: u32, stride: u32, y0: u32, color: u32) {
    if phys == 0 || width == 0 {
        return;
    }
    let stride = if stride == 0 { width } else { stride } as usize;
    let width = width as usize;
    let len_px = (len_bytes / 4) as usize;
    let base = phys as *mut u32;
    const BAND_H: usize = 24;

    let mut ry = 0;
    while ry < BAND_H {
        let row = (y0 as usize + ry).wrapping_mul(stride);
        let mut x = 0;
        while x < width {
            let idx = row + x;
            if idx < len_px {
                unsafe { base.add(idx).write_volatile(color) };
            }
            x += 1;
        }
        // Flush this row to RAM so the display scanout sees it (see `fb_clean_line`).
        #[cfg(target_os = "none")]
        {
            let start = row.min(len_px);
            let stop_px = (row + width).min(len_px);
            let mut a = ((base as usize) + start * 4) & !63;
            let stop = (base as usize) + stop_px * 4;
            while a < stop {
                unsafe { fb_clean_line(a) };
                a += 64;
            }
        }
        ry += 1;
    }
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
}

/// Scroll the framebuffer text grid up by one glyph row: move every full glyph band up
/// `GLYPH_H` pixel rows, clear the freed bottom band, and flush the moved region to RAM
/// for the scanout. Replaces the old bottom-row clamp — the POST outgrew small displays
/// (QEMU ramfb is 37 rows), where the tail lines overwrote each other.
fn fb_scroll_up(base: *mut u32, len_px: usize, stride: usize, height_px: usize) {
    let band = stride * GLYPH_H; // pixels per text row band
    let visible_px = stride * ((height_px / GLYPH_H) * GLYPH_H);
    if band == 0 || visible_px <= band || visible_px > len_px {
        return;
    }
    let keep = visible_px - band;
    // Forward overlapping copy (dst < src) — safe with `copy`.
    unsafe { core::ptr::copy(base.add(band), base, keep) };
    let mut i = keep;
    while i < visible_px {
        unsafe { base.add(i).write_volatile(BG) };
        i += 1;
    }
    #[cfg(target_os = "none")]
    unsafe {
        let mut a = (base as usize) & !63;
        let end = base as usize + visible_px * 4;
        while a < end {
            fb_clean_line(a);
            a += 64;
        }
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
}

/// Render one Unicode scalar to the framebuffer. ASCII (and `\n`/`\r`) use the 8x16 PSF
/// cell; any scalar >= `0x80` takes a curated 16x16 **double-width** CJK glyph (or the tofu
/// box if absent), advancing the cursor by two cells.
fn fb_putchar(scalar: u32) {
    let stride = FB_STRIDE.load(ORD) as usize;
    let cols = (FB_WIDTH.load(ORD) as usize / GLYPH_W).max(1);
    let rows = (FB_HEIGHT.load(ORD) as usize / GLYPH_H).max(1);
    let mut col = FB_COL.load(ORD) as usize;
    let mut row = FB_ROW.load(ORD) as usize;
    // Address pixels through the console VA offset (identity early, physmap once the
    // split tables are live) so text rendering works under any active TTBR0.
    let base = (FB_BASE.load(ORD) + CONSOLE_VA_OFFSET.load(ORD)) as *mut u32;
    let len_px = FB_LEN_PX.load(ORD);

    match scalar {
        0x0a => {
            col = 0;
            row += 1;
        }
        0x0d => col = 0,
        0x20..=0x7e => {
            if col >= cols {
                col = 0;
                row += 1;
            }
            if row >= rows {
                fb_scroll_up(base, len_px, stride, FB_HEIGHT.load(ORD) as usize);
                row = rows - 1;
            }
            unsafe {
                blit_glyph(
                    base,
                    len_px,
                    stride,
                    col * GLYPH_W,
                    row * GLYPH_H,
                    scalar as u8,
                    FG,
                    BG,
                )
            };
            col += 1;
        }
        c if c >= 0x80 => {
            // A wide cell needs two columns; wrap first if it would not fit.
            if col + 2 > cols {
                col = 0;
                row += 1;
            }
            if row >= rows {
                fb_scroll_up(base, len_px, stride, FB_HEIGHT.load(ORD) as usize);
                row = rows - 1;
            }
            let glyph = cjk_glyph(c).unwrap_or(&TOFU[..]);
            unsafe {
                blit_wide_glyph(
                    base,
                    len_px,
                    stride,
                    col * GLYPH_W,
                    row * GLYPH_H,
                    glyph,
                    FG,
                    BG,
                )
            };
            col += 2;
        }
        _ => {} // other C0 controls: ignore
    }

    // A trailing newline at the bottom scrolls eagerly, leaving a blank last row for
    // the next character (normal terminal behaviour).
    if row >= rows {
        fb_scroll_up(base, len_px, stride, FB_HEIGHT.load(ORD) as usize);
        row = rows - 1;
    }
    FB_COL.store(col as u32, ORD);
    FB_ROW.store(row as u32, ORD);
}

pub fn early_console_write(bytes: &[u8]) {
    if FB_PRESENT.load(ORD) {
        // The framebuffer is a glyph grid, so decode UTF-8 and look glyphs up per scalar.
        for_each_scalar(bytes, fb_putchar);
        return;
    }

    // PL011 is a byte stream; pass the raw UTF-8 through and let the host terminal decode.
    if !UART_READY.swap(true, ORD) {
        pl011_init();
    }
    for &byte in bytes {
        if byte == b'\n' {
            pl011_putc(b'\r');
        }
        pl011_putc(byte);
    }
}

/// Move the console cursor. On the framebuffer this is an absolute character cell;
/// on PL011 it returns to the start of the current line (`\r`), which lets a single
/// status line redraw in place (e.g. the heartbeat).
pub fn console_set_cursor(col: u32, row: u32) {
    if FB_PRESENT.load(ORD) {
        FB_COL.store(col, ORD);
        FB_ROW.store(row, ORD);
    } else if UART_READY.load(ORD) {
        pl011_putc(b'\r');
    }
}

/// Non-blocking read of one byte of console input. Only the PL011 path has an input
/// device (QEMU serial); a framebuffer console (e.g. the X13s) has no keyboard yet,
/// so this returns `None` there rather than touching a nonexistent UART.
pub fn console_read_byte() -> Option<u8> {
    if FB_PRESENT.load(ORD) || !UART_READY.load(ORD) {
        return None;
    }
    let flags = unsafe { pl011_reg(UARTFR).read_volatile() };
    if flags & UARTFR_RXFE != 0 {
        None
    } else {
        Some((unsafe { pl011_reg(UARTDR).read_volatile() } & 0xff) as u8)
    }
}

// ---- ARM generic timer (monotonic clock) ----------------------------
//
// Board-independent: the counter and its frequency are CPU system registers, the
// same on QEMU `virt` and the X13s. Interrupt delivery (which routes the timer PPI
// through the GIC) is a separate, board-specific slice.

/// Counter frequency in Hz (`CNTFRQ_EL0`).
#[cfg(target_os = "none")]
pub fn timer_frequency() -> u64 {
    let freq: u64;
    unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq, options(nostack, nomem)) };
    freq
}

/// Monotonic counter value (`CNTPCT_EL0`).
#[cfg(target_os = "none")]
pub fn timer_ticks() -> u64 {
    let ticks: u64;
    unsafe { core::arch::asm!("mrs {}, cntpct_el0", out(reg) ticks, options(nostack, nomem)) };
    ticks
}

#[cfg(not(target_os = "none"))]
pub fn timer_frequency() -> u64 {
    0
}

#[cfg(not(target_os = "none"))]
pub fn timer_ticks() -> u64 {
    0
}

/// Monotonic time since power-on, in nanoseconds.
pub fn monotonic_nanos() -> u64 {
    let freq = timer_frequency();
    if freq == 0 {
        return 0;
    }
    ((timer_ticks() as u128 * 1_000_000_000u128) / freq as u128) as u64
}

// ---- Kernel-owned MMU ------------------------------------------------
//
// Nijigumo enters through a temporary TTBR1 map. This replaces it with KUMO-owned
// tables: TTBR0 retains the Stage-A identity map until per-process address spaces land,
// while TTBR1 permanently maps the high-linked kernel and a non-executable physmap.

#[cfg(target_os = "none")]
pub mod mmu {
    use core::sync::atomic::{AtomicU64, Ordering};

    const GB: u64 = 1 << 30;
    const BLOCK_2M: u64 = 1 << 21;
    const PAGE_4K: u64 = 1 << 12;
    pub const PHYSMAP_BASE: u64 = 0xffff_9000_0000_0000;

    // Descriptor bits.
    const DESC_VALID: u64 = 1 << 0;
    const DESC_TABLE: u64 = 1 << 1; // table (L0-L2) / page (L3) descriptor
    const DESC_AF: u64 = 1 << 10; // access flag
    const SH_INNER: u64 = 3 << 8;
    const PXN: u64 = 1 << 53;
    const UXN: u64 = 1 << 54;

    // MAIR attribute indices.
    const MAIR_WB: u64 = 0; // Normal write-back
    const MAIR_DEVICE: u64 = 1; // Device-nGnRnE
    const MAIR_NC: u64 = 2; // Normal non-cacheable
    const MAIR_VALUE: u64 = 0xff | (0x00 << 8) | (0x44 << 16);

    /// T0SZ for a 48-bit VA space.
    const T0SZ: u64 = 16;
    /// T1SZ for a 48-bit upper VA space.
    const T1SZ: u64 = 16;
    const ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;
    static KERNEL_PHYS_BASE: AtomicU64 = AtomicU64::new(0);
    static KERNEL_VIRT_BASE: AtomicU64 = AtomicU64::new(0);
    static KERNEL_IMAGE_LEN: AtomicU64 = AtomicU64::new(0);

    fn write_desc(table_phys: u64, index: usize, value: u64) {
        unsafe { ((table_phys + (index as u64) * 8) as *mut u64).write_volatile(value) };
    }

    fn block_desc(pa: u64, mair_index: u64, no_execute: bool) -> u64 {
        let mut desc = pa | DESC_VALID | DESC_AF | (mair_index << 2);
        if no_execute {
            desc |= PXN | UXN; // device / framebuffer: not executable, not shareable
        } else {
            desc |= SH_INNER; // normal RAM: inner shareable, executable
        }
        desc
    }

    fn page_desc(pa: u64, mair_index: u64, executable: bool) -> u64 {
        let mut desc = pa | DESC_VALID | DESC_TABLE | DESC_AF | SH_INNER | (mair_index << 2) | UXN;
        if !executable {
            desc |= PXN;
        }
        desc
    }

    fn read_desc(table_phys: u64, index: usize) -> u64 {
        unsafe { ((table_phys + (index as u64) * 8) as *const u64).read_volatile() }
    }

    fn ensure_table(
        parent: u64,
        index: usize,
        alloc: &mut dyn FnMut() -> Option<u64>,
        tables: &mut usize,
    ) -> Result<u64, ()> {
        let current = read_desc(parent, index);
        if current & DESC_VALID != 0 {
            return Ok(current & ADDR_MASK);
        }
        let child = alloc().ok_or(())?;
        *tables += 1;
        write_desc(parent, index, child | DESC_VALID | DESC_TABLE);
        Ok(child)
    }

    fn map_page(
        root: u64,
        va: u64,
        pa: u64,
        desc: u64,
        alloc: &mut dyn FnMut() -> Option<u64>,
        tables: &mut usize,
    ) -> Result<(), ()> {
        let l1 = ensure_table(root, ((va >> 39) & 0x1ff) as usize, alloc, tables)?;
        let l2 = ensure_table(l1, ((va >> 30) & 0x1ff) as usize, alloc, tables)?;
        let l3 = ensure_table(l2, ((va >> 21) & 0x1ff) as usize, alloc, tables)?;
        write_desc(l3, ((va >> 12) & 0x1ff) as usize, pa | desc);
        Ok(())
    }

    fn map_block(
        root: u64,
        va: u64,
        pa: u64,
        desc: u64,
        alloc: &mut dyn FnMut() -> Option<u64>,
        tables: &mut usize,
    ) -> Result<(), ()> {
        let l1 = ensure_table(root, ((va >> 39) & 0x1ff) as usize, alloc, tables)?;
        let l2 = ensure_table(l1, ((va >> 30) & 0x1ff) as usize, alloc, tables)?;
        write_desc(l2, ((va >> 21) & 0x1ff) as usize, pa | desc);
        Ok(())
    }

    fn parange() -> u64 {
        let mmfr0: u64;
        unsafe {
            core::arch::asm!("mrs {}, id_aa64mmfr0_el1", out(reg) mmfr0, options(nostack, nomem))
        };
        // IPS uses the same encoding as PARange; clamp to 48-bit (5) for a 4 KiB map.
        (mmfr0 & 0xf).min(5)
    }

    /// Build the permanent Stage-A split map and switch to it. TTBR0 identity-maps
    /// `[0, top)` for the current bootstrap code; TTBR1 maps the kernel image at its
    /// linked virtual range and maps all physical addresses at [`PHYSMAP_BASE`].
    ///
    /// # Safety
    /// Must run at EL1 with the firmware identity map active; `[0, top)` must include
    /// the executing code, stack, and the frames `alloc` returns.
    pub unsafe fn enable_kernel(
        top: u64,
        kernel_phys: u64,
        kernel_virt: u64,
        kernel_len: u64,
        fb_phys: u64,
        fb_len: u64,
        is_ram: &dyn Fn(u64) -> bool,
        alloc: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<(usize, u64), ()> {
        KERNEL_PHYS_BASE.store(kernel_phys, Ordering::Relaxed);
        KERNEL_VIRT_BASE.store(kernel_virt, Ordering::Relaxed);
        KERNEL_IMAGE_LEN.store(kernel_len, Ordering::Relaxed);

        let ttbr0 = alloc().ok_or(())?;
        let ttbr1 = alloc().ok_or(())?;
        let mut tables = 2usize;

        let fb_end = fb_phys.saturating_add(fb_len);
        let gb_count = top.div_ceil(GB);
        let mut gi = 0u64;
        while gi < gb_count {
            let mut bi = 0u64;
            while bi < 512 {
                let pa = gi * GB + bi * BLOCK_2M;
                if pa >= top {
                    break;
                }
                let desc = if fb_len != 0 && pa + BLOCK_2M > fb_phys && pa < fb_end {
                    block_desc(pa, MAIR_NC, true)
                } else if is_ram(pa) {
                    block_desc(pa, MAIR_WB, false)
                } else {
                    block_desc(pa, MAIR_DEVICE, true)
                };
                map_block(ttbr0, pa, pa, desc, alloc, &mut tables)?;
                map_block(
                    ttbr1,
                    PHYSMAP_BASE + pa,
                    pa,
                    desc | PXN | UXN,
                    alloc,
                    &mut tables,
                )?;
                bi += 1;
            }
            gi += 1;
        }

        let kernel_pages = kernel_len.div_ceil(PAGE_4K);
        let mut page = 0u64;
        while page < kernel_pages {
            let pa = kernel_phys + page * PAGE_4K;
            let va = kernel_virt + page * PAGE_4K;
            let desc = page_desc(0, MAIR_WB, true);
            map_page(ttbr1, va, pa, desc, alloc, &mut tables)?;
            page += 1;
        }

        let ips = parange();
        // Both halves use 4 KiB granules, inner-shareable WBWA walks, and 48-bit VAs.
        let tcr: u64 = T0SZ
            | (1 << 8)
            | (1 << 10)
            | (3 << 12)
            | (0 << 14)
            | (T1SZ << 16)
            | (1 << 24)
            | (1 << 26)
            | (3 << 28)
            | (2 << 30)
            | (ips << 32);

        unsafe {
            core::arch::asm!(
                "dsb ish",                 // page-table writes visible to the walker
                "msr mair_el1, {mair}",
                "msr tcr_el1, {tcr}",
                "msr ttbr0_el1, {ttbr0}",
                "msr ttbr1_el1, {ttbr1}",
                "isb",
                "tlbi vmalle1",            // drop stale firmware TLB entries
                "dsb ish",
                "isb",
                mair = in(reg) MAIR_VALUE,
                tcr = in(reg) tcr,
                ttbr0 = in(reg) ttbr0,
                ttbr1 = in(reg) ttbr1,
                options(nostack, preserves_flags),
            );
        }

        Ok((tables, gb_count * GB))
    }

    // Stage-1 AP[2:1] (descriptor bits [7:6]) access-permission encodings.
    const AP_EL1RW_EL0RW: u64 = 1 << 6; // AP=0b01: EL1 RW / EL0 RW
    const AP_EL1RO_EL0RO: u64 = (1 << 6) | (1 << 7); // AP=0b11: EL1 RO / EL0 RO

    /// Build a 4 KiB **user** (TTBR0) page descriptor with true per-page W^X:
    /// * `writable`  → EL0 RW, never executable (`UXN|PXN`) — data / stack.
    /// * `executable` (read-only) → EL0 RO + executable (`UXN` clear), EL1 noexec — code.
    /// * otherwise   → EL0 RO, never executable — read-only data.
    /// Writable-and-executable is impossible by construction (W wins → non-exec), so no
    /// page is ever both, with `SCTLR.WXN` left untouched.
    pub fn user_page_desc(executable: bool, writable: bool) -> u64 {
        let base = DESC_VALID | DESC_TABLE | DESC_AF | SH_INNER | (MAIR_WB << 2);
        if writable {
            base | AP_EL1RW_EL0RW | UXN | PXN
        } else if executable {
            base | AP_EL1RO_EL0RO | PXN
        } else {
            base | AP_EL1RO_EL0RO | UXN | PXN
        }
    }

    /// Map one 4 KiB page `va -> pa` with `desc` into the page-table tree rooted at `root`,
    /// allocating intermediate tables from `alloc`. `desc` should come from
    /// [`user_page_desc`]; `alloc` must return zeroed frames.
    ///
    /// # Safety
    /// `root` must be a live L0 table reachable by the active page tables; `alloc` frames
    /// must be writable RAM mapped now.
    pub unsafe fn map_user_page(
        root: u64,
        va: u64,
        pa: u64,
        desc: u64,
        alloc: &mut dyn FnMut() -> Option<u64>,
        tables: &mut usize,
    ) -> Result<(), ()> {
        map_page(root, va, pa, desc, alloc, tables)
    }

    // (`map_kernel_device_block` — the EL1-only console-window mapper — is gone with the
    // P5-mmu-b console bridge: the console rides the TTBR1 physmap now.)

    /// Map a 2 MiB block `va -> pa` into `root` with **EL0 access** (RW if `writable`,
    /// RO otherwise). `nc` means Normal-NC (for framebuffers); otherwise Device-nGnRnE
    /// (for MMIO registers). EL0 can never execute (`UXN`), and the kernel does not
    /// execute from device pages (`PXN`).
    ///
    /// # Safety
    /// As [`map_user_page`].
    pub unsafe fn map_user_device_block(
        root: u64,
        va: u64,
        pa: u64,
        nc: bool,
        writable: bool,
        alloc: &mut dyn FnMut() -> Option<u64>,
        tables: &mut usize,
    ) -> Result<(), ()> {
        let mair = if nc { MAIR_NC } else { MAIR_DEVICE };
        let ap = if writable {
            AP_EL1RW_EL0RW
        } else {
            AP_EL1RO_EL0RO
        };
        let desc = pa | DESC_VALID | DESC_AF | (mair << 2) | PXN | UXN | ap;
        map_block(root, va, pa, desc, alloc, tables)
    }

    /// Build a 4 KiB **Device-nGnRnE** page descriptor for EL0 (MMIO registers).
    /// Follows [`user_page_desc`]'s shape but uses `MAIR_DEVICE` and omits `SH_INNER`
    /// (device memory has no shareability domain). Use with [`map_user_page`].
    pub fn user_device_page_desc(writable: bool) -> u64 {
        let base = DESC_VALID | DESC_TABLE | DESC_AF | (MAIR_DEVICE << 2);
        if writable {
            base | AP_EL1RW_EL0RW | UXN | PXN
        } else {
            base | AP_EL1RO_EL0RO | UXN | PXN
        }
    }

    /// Build a 4 KiB **Normal-NC** page descriptor for EL0 (framebuffer scanout, bootinfo).
    /// Follows [`user_page_desc`]'s shape but uses `MAIR_NC` instead of `MAIR_WB`.
    /// Use with [`map_user_page`].
    pub fn user_nc_page_desc(writable: bool) -> u64 {
        let base = DESC_VALID | DESC_TABLE | DESC_AF | SH_INNER | (MAIR_NC << 2);
        if writable {
            base | AP_EL1RW_EL0RW | UXN | PXN
        } else {
            base | AP_EL1RO_EL0RO | UXN | PXN
        }
    }

    /// Read `TTBR0_EL1` (the active low-half root), so the kernel identity map can be
    /// restored after a process address space has run.
    pub fn read_ttbr0() -> u64 {
        let value: u64;
        unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) value, options(nostack, nomem)) };
        value
    }

    /// Switch `TTBR0_EL1` to `root` and flush the low-half TLB. TTBR1 (the kernel) is
    /// untouched, so EL1 keeps executing across the swap. ASID 0 throughout; a full
    /// `tlbi vmalle1` avoids stale aliasing between the kernel identity map and a process
    /// tree.
    ///
    /// # Safety
    /// `root` must be a valid L0 table for the intended address space.
    pub unsafe fn set_ttbr0(root: u64) {
        unsafe {
            core::arch::asm!(
                "dsb ish",
                "msr ttbr0_el1, {root}",
                "isb",
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                root = in(reg) root,
                options(nostack, preserves_flags),
            );
        }
    }

    /// Translate an address inside the high-linked kernel image to its physical backing.
    pub fn kernel_image_phys(va: u64) -> Option<u64> {
        let virt = KERNEL_VIRT_BASE.load(Ordering::Relaxed);
        let phys = KERNEL_PHYS_BASE.load(Ordering::Relaxed);
        let len = KERNEL_IMAGE_LEN.load(Ordering::Relaxed);
        let offset = va.checked_sub(virt)?;
        (offset < len).then_some(phys + offset)
    }
}

#[cfg(target_os = "none")]
pub use mmu::enable_kernel as enable_kernel_mmu;
#[cfg(target_os = "none")]
pub use mmu::{
    map_user_device_block, map_user_page, user_device_page_desc, user_nc_page_desc, user_page_desc,
};
#[cfg(target_os = "none")]
pub use mmu::{read_ttbr0, set_ttbr0};

// ---- Host stubs for mmu items (kernel tests run on host) ----

/// Host stub: build a 4 KiB user page descriptor.
#[cfg(not(target_os = "none"))]
pub fn user_page_desc(_executable: bool, _writable: bool) -> u64 {
    0
}

/// Host stub: build a 4 KiB Device-nGnRnE page descriptor.
#[cfg(not(target_os = "none"))]
pub fn user_device_page_desc(_writable: bool) -> u64 {
    0
}

/// Host stub: build a 4 KiB Normal-NC page descriptor.
#[cfg(not(target_os = "none"))]
pub fn user_nc_page_desc(_writable: bool) -> u64 {
    0
}

/// Host stub: map one 4 KiB user page.
///
/// # Safety
/// Stub; `unsafe` to match the hardware contract.
#[cfg(not(target_os = "none"))]
pub unsafe fn map_user_page(
    _root: u64,
    _va: u64,
    _pa: u64,
    _desc: u64,
    _alloc: &mut dyn FnMut() -> Option<u64>,
    _tables: &mut usize,
) -> Result<(), ()> {
    Ok(())
}

/// Host stub: map a 2 MiB device block.
///
/// # Safety
/// Stub; `unsafe` to match the hardware contract.
#[cfg(not(target_os = "none"))]
pub unsafe fn map_user_device_block(
    _root: u64,
    _va: u64,
    _pa: u64,
    _nc: bool,
    _writable: bool,
    _alloc: &mut dyn FnMut() -> Option<u64>,
    _tables: &mut usize,
) -> Result<(), ()> {
    Ok(())
}

/// Arch-neutral name the kernel uses to switch the user address-space root. This backend
/// programs `TTBR0_EL1`. Inherently unsafe: `root` must be a valid table for the space.
#[cfg(target_os = "none")]
pub unsafe fn set_user_aspace_root(root: u64) {
    unsafe { mmu::set_ttbr0(root) }
}

/// Arch-neutral name the kernel uses to read the current user address-space root
/// (this backend: `TTBR0_EL1`).
#[cfg(target_os = "none")]
pub fn read_user_aspace_root() -> u64 {
    mmu::read_ttbr0()
}

// ---- EL0 / userspace smoke -----------------------------------------------------------
//
// The first time KUMO drops below EL1. A tiny position-independent payload runs at EL0
// in a dedicated, EL0-accessible 2 MiB window, issues `SVC #0` syscalls (a "ping" and an
// "exit"), and the kernel handles them at EL1 and trampolines back. This proves the whole
// privilege round-trip — user mapping, `eret` to EL0, the SVC trap, dispatch, and return
// — the foundation the real syscall ABI (`SyscallEngine`) and Sora build on.
#[cfg(target_os = "none")]
pub mod el0 {
    use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    const PAGE_4K: u64 = 1 << 12;

    static SYSCALLS: AtomicU32 = AtomicU32::new(0);

    /// Read the syscall counter (useful when the kernel drives EL0 entry via the
    /// scheduler instead of the synchronous `run_el0_image` path).
    pub fn syscall_count() -> u32 {
        SYSCALLS.load(Ordering::Relaxed)
    }

    /// The kernel's syscall handler, registered via [`set_svc_hook`]. It receives a
    /// pointer to the 31 saved EL0 GP registers (x0..x30): the ABI is x8 = syscall
    /// number, x0.. = args, results written back to x0/x1. 0 means "not installed".
    static SVC_HOOK: AtomicUsize = AtomicUsize::new(0);

    /// Register the kernel's EL0 syscall handler (see [`SVC_HOOK`]).
    pub fn set_svc_hook(hook: extern "C" fn(*mut u64)) {
        SVC_HOOK.store(hook as usize, Ordering::Release);
    }

    /// The kernel's EL0-fault handler, registered via [`set_fault_hook`]. Called when a
    /// lower-EL (EL0) thread takes a non-SVC synchronous exception — a *user* fault (bad
    /// access, illegal instruction). It receives `(esr, elr, far)` and must NOT return: it
    /// terminates the faulting thread and switches away, so one process's crash never halts
    /// the kernel (DESIGN/002). 0 means "not installed" → fall back to the Tower halt.
    static FAULT_HOOK: AtomicUsize = AtomicUsize::new(0);

    /// Register the kernel's EL0-fault handler (see [`FAULT_HOOK`]).
    pub fn set_fault_hook(hook: extern "C" fn(u64, u64, u64) -> !) {
        FAULT_HOOK.store(hook as usize, Ordering::Release);
    }

    /// Outcome of the EL0 round-trip, for the boot report.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct El0Report {
        pub entered: bool,
        pub syscalls: u32,
        pub ping_echo: u64,
        pub exit_code: u64,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum UserImageError {
        Empty,
        BadSegment,
        BadStack,
        OutOfFrames,
        ImageTooLarge,
        SegmentOutsideImageBlock,
        StackOutsideStackBlock,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct UserLoadSegment<'a> {
        /// The `p_filesz` bytes of file content; the page builder copies these at
        /// `virt_addr`'s page offset and zero-fills the rest of `mem_size` (BSS).
        pub source: &'a [u8],
        pub virt_addr: u64,
        pub mem_size: u64,
        /// `PF_W` — maps the segment RW (EL0 read/write, never executable).
        pub writable: bool,
        /// `PF_X` — maps a non-writable segment RX (EL0 read/execute).
        pub executable: bool,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct UserMapping {
        /// Physical base address of the mapping (need not be 2 MiB-aligned).
        pub phys_base: u64,
        /// For device/framebuffer mappings, the 2 MiB-aligned virtual **slot** the covering
        /// blocks are mapped at (usable VA = `virt_addr + (phys_base & (2 MiB - 1))`). For
        /// `executable` mappings this is the exact, page-aligned VA `phys_base` maps to.
        pub virt_addr: u64,
        /// Length in bytes.
        pub len: u64,
        /// Whether EL0 can write to this mapping.
        pub writable: bool,
        /// If true, map as Device-nGnRnE; otherwise Normal-NC (for framebuffers). Ignored
        /// when `executable` is set (code is always Normal-WB).
        pub device: bool,
        /// If true, map as Normal-WB **RX** 4 KiB pages (EL0-executable code) rather than the
        /// device-block path. Page-granular, so it coexists with a stack in the same 2 MiB
        /// region; mutually exclusive with `writable` (W^X). The mapped range is cleaned to
        /// PoC + the I-cache invalidated so EL0 fetches the freshly written bytes.
        pub executable: bool,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct UserImage<'a> {
        pub entry: u64,
        pub stack_top: u64,
        /// Bytes of RW stack to map immediately below `stack_top` (page-aligned). One
        /// unmapped guard page sits below that, so a stack overflow faults to the Tower.
        pub stack_size: u64,
        /// Handed to the process in `x0` at entry (processargs-style): the bootstrap
        /// root-channel handle. 0 = none.
        pub bootstrap: u64,
        pub segments: &'a [UserLoadSegment<'a>],
        /// Extra physical mappings (e.g. framebuffer, MMIO). Mapped as 2 MiB blocks
        /// with EL0 access. Each mapping must be page-aligned.
        pub extra_mappings: &'a [UserMapping],
    }

    /// Saved kernel callee-saved registers + SP, so the exit SVC can return to the boot
    /// flow as if `kumo_enter_el0` had returned. Touched only from asm.
    #[no_mangle]
    #[used]
    static mut KERNEL_RESUME: [u64; 16] = [0; 16];

    extern "C" {
        fn kumo_enter_el0(entry: u64, user_sp: u64, arg0: u64) -> u64;
        static el0_payload_start: u8;
        static el0_payload_end: u8;
    }

    // ---- P5-sched: UserState + scheduler-driven EL0 entry ------------------------
    //
    // A user thread's ThreadContext stores x19_entry = &UserState and
    // x30_lr = kumo_user_enter (not kumo_context_trampoline). When the scheduler
    // first switches to the thread, kumo_context_switch restores callee-saved regs
    // + sp and `ret`s to kumo_user_enter, which loads the full EL0 register file,
    // switches TTBR0, and erets to EL0 with DAIF unmasked (so the timer preempts).

    /// Saved EL0 execution context. Used for first entry (loaded by
    /// `kumo_user_enter`) and for save/restore across scheduler-driven blocks.
    #[repr(C)]
    pub struct UserState {
        /// x0..x30 at EL0 entry. x0 = bootstrap arg (channel handle) on first entry.
        pub x: [u64; 31],
        /// EL0 entry point (restored to ELR_EL1)
        pub elr: u64,
        /// Saved processor state (restored to SPSR_EL1): EL0t, DAIF setting
        pub spsr: u64,
        /// EL0 stack pointer (restored to SP_EL0)
        pub sp_el0: u64,
        /// Process page table root physical address (restored to TTBR0_EL1)
        pub ttbr0: u64,
    }

    core::arch::global_asm!(
        ".global kumo_user_enter",
        ".balign 4",
        "kumo_user_enter:",
        // x19 = *const UserState (set by `user_entry_context` for user threads).
        // Order matters: program every system register FIRST (through scratch regs that
        // have not received their EL0 values yet), THEN load the GP file, with the base
        // register (x9) loaded dead last. The previous version loaded the GP file first
        // and then pulled elr/spsr/sp_el0/ttbr0 through x2/x3 — handing EL0 the TTBR0
        // physical address in x2 instead of its third bootstrap argument.
        "  mov  x9, x19",              // x9 = &UserState (base)
        "  ldp  x10, x11, [x9, #248]", // elr, spsr
        "  msr  elr_el1, x10",
        "  msr  spsr_el1, x11",
        "  ldr  x10, [x9, #264]", // sp_el0
        "  msr  sp_el0, x10",
        "  ldr  x10, [x9, #272]", // ttbr0 (process page tables)
        "  msr  ttbr0_el1, x10",
        "  isb",
        "  tlbi vmalle1",
        "  dsb  ish",
        "  isb",
        "  ldp  x0, x1, [x9, #0]", // EL0 register file (x9 last — it is the base)
        "  ldp  x2, x3, [x9, #16]",
        "  ldp  x4, x5, [x9, #32]",
        "  ldp  x6, x7, [x9, #48]",
        "  ldr  x8, [x9, #64]",
        "  ldp  x10, x11, [x9, #80]",
        "  ldp  x12, x13, [x9, #96]",
        "  ldp  x14, x15, [x9, #112]",
        "  ldp  x16, x17, [x9, #128]",
        "  ldp  x18, x19, [x9, #144]",
        "  ldp  x20, x21, [x9, #160]",
        "  ldp  x22, x23, [x9, #176]",
        "  ldp  x24, x25, [x9, #192]",
        "  ldp  x26, x27, [x9, #208]",
        "  ldp  x28, x29, [x9, #224]",
        "  ldr  x30, [x9, #240]",
        "  ldr  x9, [x9, #72]", // finally x9 itself
        "  eret",
    );

    // The EL0 program (position-independent: register ops + svc + adr/relative branch).
    // Uses the real syscall ABI (x8 = number, args in x0..): DebugWrite(29) a greeting,
    // ChannelCreate(3), then ProcessExit(21). The string is embedded and addressed
    // PC-relative, so it resolves to its user-VA after the blob is copied into the window.
    core::arch::global_asm!(
        ".section .text.el0_payload",
        ".global el0_payload_start",
        ".global el0_payload_end",
        ".balign 4",
        "el0_payload_start:",
        "  adr  x0, 5f",  // x0 = &msg (user VA)
        "  movz x1, #23", // x1 = msg len
        "  movz x8, #29", // SYS DebugWrite
        "  svc  #0",
        "  movz x8, #3", // SYS ChannelCreate -> x0=h0, x1=h1
        "  svc  #0",
        "  movz x0, #0",  // exit code 0
        "  movz x8, #21", // SYS ProcessExit (does not return to EL0)
        "  svc  #0",
        "1: b 1b",
        "5: .ascii \"hello from EL0 via SVC\\n\"",
        ".balign 4",
        "el0_payload_end:",
    );

    // Save kernel callee-saved + SP, then `eret` to EL0 (entry=x0, user SP=x1).
    core::arch::global_asm!(
        ".global kumo_enter_el0",
        ".balign 4",
        "kumo_enter_el0:", // x0=entry, x1=user_sp, x2=arg0 (-> EL0 x0, the bootstrap handle)
        "  mov  x9, x2",   // stash arg0 before x2 is reused as the save base
        "  adrp x2, KERNEL_RESUME",
        "  add  x2, x2, :lo12:KERNEL_RESUME",
        "  stp  x19, x20, [x2, #0]",
        "  stp  x21, x22, [x2, #16]",
        "  stp  x23, x24, [x2, #32]",
        "  stp  x25, x26, [x2, #48]",
        "  stp  x27, x28, [x2, #64]",
        "  stp  x29, x30, [x2, #80]",
        "  mov  x3, sp",
        "  str  x3, [x2, #96]",
        "  msr  elr_el1, x0", // EL0 entry point
        "  movz x4, #0x3c0",  // SPSR: EL0t, DAIF masked (no preemption during the smoke)
        "  msr  spsr_el1, x4",
        "  msr  sp_el0, x1", // EL0 stack
        "  mov  x0, x9",     // x0 at EL0 = the bootstrap handle (processargs-style)
        "  isb",
        "  eret",
    );

    // Restore kernel callee-saved + SP and `ret` to `kumo_enter_el0`'s caller (x0 = code).
    core::arch::global_asm!(
        ".global kumo_resume_kernel",
        ".balign 4",
        "kumo_resume_kernel:",
        "  adrp x2, KERNEL_RESUME",
        "  add  x2, x2, :lo12:KERNEL_RESUME",
        "  ldp  x19, x20, [x2, #0]",
        "  ldp  x21, x22, [x2, #16]",
        "  ldp  x23, x24, [x2, #32]",
        "  ldp  x25, x26, [x2, #48]",
        "  ldp  x27, x28, [x2, #64]",
        "  ldp  x29, x30, [x2, #80]",
        "  ldr  x3, [x2, #96]",
        "  mov  sp, x3",
        "  ret",
    );

    // Lower-EL synchronous handler: save the EL0 frame, and if the cause is SVC, dispatch
    // it and `eret` back to EL0; otherwise fall through to the Tower (a real EL0 fault).
    // The syscall boundary is transparent to EL0 FP/SIMD: Rust kernel code may use NEON
    // during the dispatch, so save q0-q31 plus FPCR/FPSR before calling into Rust.
    core::arch::global_asm!(
        ".global kumo_svc_common",
        ".balign 4",
        "kumo_svc_common:",
        "  sub  sp, sp, #800",
        "  stp  x0,  x1,  [sp, #0]",
        "  stp  x2,  x3,  [sp, #16]",
        "  stp  x4,  x5,  [sp, #32]",
        "  stp  x6,  x7,  [sp, #48]",
        "  stp  x8,  x9,  [sp, #64]",
        "  stp  x10, x11, [sp, #80]",
        "  stp  x12, x13, [sp, #96]",
        "  stp  x14, x15, [sp, #112]",
        "  stp  x16, x17, [sp, #128]",
        "  stp  x18, x19, [sp, #144]",
        "  stp  x20, x21, [sp, #160]",
        "  stp  x22, x23, [sp, #176]",
        "  stp  x24, x25, [sp, #192]",
        "  stp  x26, x27, [sp, #208]",
        "  stp  x28, x29, [sp, #224]",
        "  str  x30,      [sp, #240]",
        "  mrs  x0, elr_el1",
        "  mrs  x1, spsr_el1",
        "  stp  x0,  x1,  [sp, #248]",
        "  mrs  x0, sp_el0",
        "  str  x0,      [sp, #264]",
        "  mrs  x0, fpsr",
        "  mrs  x1, fpcr",
        "  stp  x0,  x1,  [sp, #272]",
        "  stp  q0,  q1,  [sp, #288]",
        "  stp  q2,  q3,  [sp, #320]",
        "  stp  q4,  q5,  [sp, #352]",
        "  stp  q6,  q7,  [sp, #384]",
        "  stp  q8,  q9,  [sp, #416]",
        "  stp  q10, q11, [sp, #448]",
        "  stp  q12, q13, [sp, #480]",
        "  stp  q14, q15, [sp, #512]",
        "  stp  q16, q17, [sp, #544]",
        "  stp  q18, q19, [sp, #576]",
        "  stp  q20, q21, [sp, #608]",
        "  stp  q22, q23, [sp, #640]",
        "  stp  q24, q25, [sp, #672]",
        "  stp  q26, q27, [sp, #704]",
        "  stp  q28, q29, [sp, #736]",
        "  stp  q30, q31, [sp, #768]",
        "  mrs  x0, esr_el1", // EC = ESR[31:26]; 0x15 == SVC from AArch64
        "  lsr  x1, x0, #26",
        "  and  x1, x1, #0x3f",
        "  cmp  x1, #0x15",
        "  b.ne 8f",
        "  mov  x0, sp", // dispatch(frame*)
        "  bl   kumo_svc_dispatch",
        "  ldp  x0,  x1,  [sp, #248]", // restore EL0 return state (x0/x1 scratch)
        "  msr  elr_el1, x0",
        "  msr  spsr_el1, x1",
        "  ldr  x0,      [sp, #264]",
        "  msr  sp_el0, x0",
        "  ldp  x0,  x1,  [sp, #272]",
        "  msr  fpsr, x0",
        "  msr  fpcr, x1",
        "  ldp  q0,  q1,  [sp, #288]",
        "  ldp  q2,  q3,  [sp, #320]",
        "  ldp  q4,  q5,  [sp, #352]",
        "  ldp  q6,  q7,  [sp, #384]",
        "  ldp  q8,  q9,  [sp, #416]",
        "  ldp  q10, q11, [sp, #448]",
        "  ldp  q12, q13, [sp, #480]",
        "  ldp  q14, q15, [sp, #512]",
        "  ldp  q16, q17, [sp, #544]",
        "  ldp  q18, q19, [sp, #576]",
        "  ldp  q20, q21, [sp, #608]",
        "  ldp  q22, q23, [sp, #640]",
        "  ldp  q24, q25, [sp, #672]",
        "  ldp  q26, q27, [sp, #704]",
        "  ldp  q28, q29, [sp, #736]",
        "  ldp  q30, q31, [sp, #768]",
        "  ldp  x0,  x1,  [sp, #0]", // restore GPRs (x0 may carry the syscall result)
        "  ldp  x2,  x3,  [sp, #16]",
        "  ldp  x4,  x5,  [sp, #32]",
        "  ldp  x6,  x7,  [sp, #48]",
        "  ldp  x8,  x9,  [sp, #64]",
        "  ldp  x10, x11, [sp, #80]",
        "  ldp  x12, x13, [sp, #96]",
        "  ldp  x14, x15, [sp, #112]",
        "  ldp  x16, x17, [sp, #128]",
        "  ldp  x18, x19, [sp, #144]",
        "  ldp  x20, x21, [sp, #160]",
        "  ldp  x22, x23, [sp, #176]",
        "  ldp  x24, x25, [sp, #192]",
        "  ldp  x26, x27, [sp, #208]",
        "  ldp  x28, x29, [sp, #224]",
        "  ldr  x30,      [sp, #240]",
        "  add  sp, sp, #800",
        "  eret",
        "8:", // not SVC: an EL0 fault. Try the kernel's fault hook (contain it to this
        // process); it terminates the thread and never returns. Only if no hook is
        // installed does the fault fall through to the Tower (a full halt).
        "  mrs  x0, esr_el1",
        "  mrs  x1, elr_el1",
        "  mrs  x2, far_el1",
        "  bl   kumo_el0_fault",
        // Unhandled (no hook): the Tower.
        "  mrs  x1, esr_el1",
        "  mrs  x2, elr_el1",
        "  mrs  x3, far_el1",
        "  mov  x0, #8",
        "  b    kumo_exception_entry",
    );

    /// The saved EL0 register frame `kumo_svc_common` hands to the dispatcher.
    #[repr(C)]
    struct SvcFrame {
        x: [u64; 31], // x0..x30
        elr: u64,
        spsr: u64,
        sp_el0: u64,
    }

    extern "C" {
        fn kumo_resume_kernel(code: u64) -> !;
    }

    /// End the EL0 thread and return to `kumo_enter_el0`'s caller with `code` (the boot
    /// flow). The kernel's syscall hook calls this for `ProcessExit`. Never returns.
    pub fn el0_exit(code: u64) -> ! {
        unsafe { kumo_resume_kernel(code) }
    }

    /// Dispatch one EL0 syscall (called from `kumo_svc_common`). Hands the saved x0..x30
    /// register file to the kernel's registered hook (the real syscall ABI lives there);
    /// with no hook installed it ends the thread so EL0 can never wedge the boot.
    #[no_mangle]
    extern "C" fn kumo_svc_dispatch(frame: *mut SvcFrame) {
        SYSCALLS.fetch_add(1, Ordering::Relaxed);
        let hook = SVC_HOOK.load(Ordering::Acquire);
        if hook == 0 {
            let f = unsafe { &mut *frame };
            el0_exit(f.x[0]);
        }
        // SAFETY: SVC_HOOK only ever holds an `extern "C" fn(*mut u64)` set by
        // `set_svc_hook`; `frame.x` is the 31-entry x0..x30 register file.
        let hook: extern "C" fn(*mut u64) = unsafe { core::mem::transmute(hook) };
        hook(frame as *mut u64);
    }

    /// Called from `kumo_svc_common` when a lower-EL (EL0) thread takes a non-SVC sync
    /// exception — a user fault. If the kernel registered a [`FAULT_HOOK`], hand off to it
    /// (it terminates the faulting thread and never returns); otherwise return so the
    /// vector falls through to the Tower (an *unhandled* fault still halts, as before).
    #[no_mangle]
    extern "C" fn kumo_el0_fault(esr: u64, elr: u64, far: u64) {
        let hook = FAULT_HOOK.load(Ordering::Acquire);
        if hook != 0 {
            // SAFETY: FAULT_HOOK only ever holds an `extern "C" fn(u64,u64,u64) -> !` set by
            // `set_fault_hook`.
            let hook: extern "C" fn(u64, u64, u64) -> ! = unsafe { core::mem::transmute(hook) };
            hook(esr, elr, far);
        }
        // No hook installed: return to the vector, which branches to the Tower.
    }

    /// Clean the written user code to PoC + invalidate the I-cache over it, so EL0 fetches
    /// the real instructions (same handshake the loader does for the kernel image).
    unsafe fn flush_for_exec(base: u64, len: usize) {
        let mut a = base & !63;
        let end = base + len as u64;
        while a < end {
            unsafe {
                core::arch::asm!("dc civac, {a}", a = in(reg) a, options(nostack, preserves_flags))
            };
            a += 64;
        }
        unsafe { core::arch::asm!("dsb ish", "ic iallu", "dsb ish", "isb", options(nostack)) };
    }

    fn align_down_4k(value: u64) -> u64 {
        value & !(PAGE_4K - 1)
    }

    fn align_up_4k(value: u64) -> Option<u64> {
        value
            .checked_add(PAGE_4K - 1)
            .map(|value| value & !(PAGE_4K - 1))
    }

    // The P5-mmu-b "console window" (an EL1-only console mapping copied into every user
    // tree) is gone: the console addresses MMIO through the TTBR1 physmap once the split
    // tables are live (`console_use_physmap`), so user page tables carry user state only.

    /// Build a fresh per-process TTBR0 tree: each `PT_LOAD` segment as 4 KiB pages with
    /// true per-page **W^X** (RX code / RW data, file bytes copied, the rest zeroed for
    /// BSS), an RW stack below `stack_top` with one unmapped guard page beneath it, and
    /// any requested extra device mappings. Frames (`alloc`, returning zeroed RAM) are
    /// touched by physical address, so this must run with the kernel identity map still
    /// active in TTBR0 — i.e. before [`enter_user_image`] switches to the tree it
    /// returns. Returns the L0 root physical address.
    fn build_user_space(
        image: &UserImage<'_>,
        alloc: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<u64, UserImageError> {
        // A tree with neither code segments nor extra mappings has nothing to run. But an
        // extra-mappings-only image is valid: that is how a child built incrementally via
        // VmarMap + AddressSpaceCreate arrives here (its code rides an executable mapping,
        // its stack comes from `stack_size` below).
        if image.segments.is_empty() && image.extra_mappings.is_empty() {
            return Err(UserImageError::Empty);
        }

        let mut tables = 0usize;
        let root = alloc().ok_or(UserImageError::OutOfFrames)?; // zeroed L0 table

        for segment in image.segments {
            if segment.writable && segment.executable {
                return Err(UserImageError::BadSegment); // W^X: never both
            }
            if segment.source.len() as u64 > segment.mem_size {
                return Err(UserImageError::BadSegment);
            }
            let seg_end = segment
                .virt_addr
                .checked_add(segment.mem_size)
                .ok_or(UserImageError::BadSegment)?;
            if segment.mem_size == 0 {
                continue;
            }
            let src_lo = segment.virt_addr;
            let src_hi = src_lo + segment.source.len() as u64;
            let desc = super::mmu::user_page_desc(segment.executable, segment.writable);

            let mut page = align_down_4k(segment.virt_addr);
            let last = align_down_4k(seg_end - 1);
            loop {
                let frame = alloc().ok_or(UserImageError::OutOfFrames)?; // zeroed -> BSS
                let copy_lo = core::cmp::max(page, src_lo);
                let copy_hi = core::cmp::min(page + PAGE_4K, src_hi);
                if copy_hi > copy_lo {
                    let dst_off = copy_lo - page;
                    let src_off = (copy_lo - src_lo) as usize;
                    let len = (copy_hi - copy_lo) as usize;
                    // SAFETY: `frame` is a zeroed RAM page mapped by the identity map; the
                    // copied span lies inside `segment.source`.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            segment.source.as_ptr().add(src_off),
                            (frame + dst_off) as *mut u8,
                            len,
                        );
                    }
                }
                unsafe { super::mmu::map_user_page(root, page, frame, desc, alloc, &mut tables) }
                    .map_err(|()| UserImageError::OutOfFrames)?;
                if segment.executable {
                    // SAFETY: clean this code frame to PoC + drop the I-cache so EL0 fetches
                    // the bytes we just wrote (the loader does the same for the kernel image).
                    unsafe { flush_for_exec(frame, PAGE_4K as usize) };
                }
                if page == last {
                    break;
                }
                page += PAGE_4K;
            }
        }

        // RW stack directly below `stack_top`; one unmapped guard page beneath it.
        let stack_size = align_up_4k(image.stack_size).ok_or(UserImageError::BadStack)?;
        if stack_size == 0 || stack_size > image.stack_top {
            return Err(UserImageError::BadStack);
        }
        let stack_base = image.stack_top - stack_size;
        let stack_desc = super::mmu::user_page_desc(false, true); // RW, never executable
        let mut sp = stack_base;
        while sp < image.stack_top {
            let frame = alloc().ok_or(UserImageError::OutOfFrames)?;
            unsafe { super::mmu::map_user_page(root, sp, frame, stack_desc, alloc, &mut tables) }
                .map_err(|()| UserImageError::OutOfFrames)?;
            sp += PAGE_4K;
        }
        // [stack_base - PAGE_4K, stack_base): intentionally left unmapped (the guard page).

        // Extra physical mappings (e.g. framebuffer VMO for userspace console).
        //
        // `virt_addr` must be 2 MiB-aligned (the mapping *slot*); the physical range is
        // covered with aligned va/pa block pairs starting at `phys_base & !mask`. The
        // caller derives the usable VA as `virt_addr + (phys_base & mask)` — see
        // `UserMapping`. (The previous offset arithmetic underflowed for a non-aligned
        // `phys_base`, landing the mapping displaced — the journal-061 paint wart.)
        const BLOCK_2M: u64 = 1 << 21;
        for mapping in image.extra_mappings {
            // Normal anonymous mappings (child code/data) map as 4 KiB pages directly from
            // the backing physical range — page-granular so code, rodata, data, and stack can
            // share an L3 table without fighting over a 2 MiB L2 slot. W^X holds in
            // `user_page_desc`: writable pages are never executable.
            if !mapping.device {
                if mapping.virt_addr & (PAGE_4K - 1) != 0 || mapping.phys_base & (PAGE_4K - 1) != 0
                {
                    return Err(UserImageError::BadSegment);
                }
                let desc = super::mmu::user_page_desc(mapping.executable, mapping.writable);
                let pages = mapping.len.div_ceil(PAGE_4K);
                let mut i = 0;
                while i < pages {
                    let off = i * PAGE_4K;
                    let va = mapping.virt_addr + off;
                    let pa = mapping.phys_base + off;
                    unsafe { super::mmu::map_user_page(root, va, pa, desc, alloc, &mut tables) }
                        .map_err(|()| UserImageError::OutOfFrames)?;
                    if mapping.executable {
                        // The code bytes were written through the TTBR1 physmap (D-cache);
                        // clean to PoC + drop the I-cache so EL0 fetches them. `pa` is
                        // reachable now via the active kernel identity map.
                        unsafe { flush_for_exec(pa, PAGE_4K as usize) };
                    }
                    i += 1;
                }
                continue;
            }
            let block_mask = BLOCK_2M - 1;
            if mapping.virt_addr & block_mask != 0 {
                return Err(UserImageError::BadSegment);
            }
            let base = mapping.phys_base & !block_mask;
            let end = mapping
                .phys_base
                .checked_add(mapping.len)
                .and_then(|end| end.checked_add(block_mask))
                .ok_or(UserImageError::ImageTooLarge)?
                & !block_mask;
            let mut offset = 0;
            while base + offset < end {
                unsafe {
                    super::mmu::map_user_device_block(
                        root,
                        mapping.virt_addr + offset,
                        base + offset,
                        !mapping.device, // false = Normal-NC (fb), true = Device (MMIO)
                        mapping.writable,
                        alloc,
                        &mut tables,
                    )
                }
                .map_err(|()| UserImageError::OutOfFrames)?;
                offset += BLOCK_2M;
            }
        }

        Ok(root)
    }

    /// Build the process address space, switch `TTBR0` to it, `eret` to EL0, and on return
    /// (a `ProcessExit` syscall) restore the kernel identity map. TTBR1 — the kernel —
    /// stays mapped throughout, so EL1 keeps executing across both swaps.
    fn enter_user_image(
        image: UserImage<'_>,
        alloc: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<El0Report, UserImageError> {
        let root = build_user_space(&image, alloc)?;
        let kernel_ttbr0 = super::mmu::read_ttbr0();

        SYSCALLS.store(0, Ordering::Relaxed);
        // SAFETY: `root` is a complete user tree; the kernel half (TTBR1) is untouched.
        unsafe { super::mmu::set_ttbr0(root) };
        let exit_code =
            unsafe { kumo_enter_el0(image.entry, image.stack_top - 16, image.bootstrap) };
        // SAFETY: restore the identity map the rest of Stage-A (and the console) expects.
        unsafe { super::mmu::set_ttbr0(kernel_ttbr0) };

        Ok(El0Report {
            entered: true,
            syscalls: SYSCALLS.load(Ordering::Relaxed),
            ping_echo: 0,
            exit_code,
        })
    }

    /// Materialize `image` into a per-process address space and run it at EL0.
    pub fn run_el0_image(
        image: UserImage<'_>,
        alloc: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<El0Report, UserImageError> {
        enter_user_image(image, alloc)
    }

    /// Build a per-process TTBR0 tree and return its L0 root physical address. The caller
    /// owns the tree and is responsible for switching to it and tearing it down. This is
    /// the page-table half of `run_el0_image`; P5-sched uses it directly so it can enter
    /// the user process via the scheduler instead of the synchronous detour.
    pub fn build_user_tables(
        image: &UserImage<'_>,
        alloc: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<u64, UserImageError> {
        build_user_space(image, alloc)
    }

    /// Run the embedded smoke payload (DebugWrite + ChannelCreate + ProcessExit) as a real
    /// frame-backed RX process at `base`, with an RW stack below `stack_top`. The fallback
    /// when there is no Sora image; exercises the same per-process page-table path.
    pub fn run_el0_smoke(
        base: u64,
        stack_top: u64,
        stack_size: u64,
        alloc: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<El0Report, UserImageError> {
        let start = core::ptr::addr_of!(el0_payload_start) as usize;
        let end = core::ptr::addr_of!(el0_payload_end) as usize;
        // SAFETY: the payload symbols bracket the embedded blob in our own .text section.
        let payload = unsafe { core::slice::from_raw_parts(start as *const u8, end - start) };
        let segment = UserLoadSegment {
            source: payload,
            virt_addr: base,
            mem_size: payload.len() as u64,
            writable: false,
            executable: true,
        };
        let image = UserImage {
            entry: base,
            stack_top,
            stack_size,
            bootstrap: 0,
            segments: core::slice::from_ref(&segment),
            extra_mappings: &[],
        };
        enter_user_image(image, alloc)
    }
}

#[cfg(target_os = "none")]
pub use el0::{
    build_user_tables, el0_exit, run_el0_image, run_el0_smoke, set_fault_hook, set_svc_hook,
    syscall_count, El0Report, UserImage, UserImageError, UserLoadSegment, UserMapping, UserState,
};

/// Host/x86 builds have no EL0 path yet; report "not entered" so the shared kernel can
/// call this unconditionally.
#[cfg(not(target_os = "none"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct El0Report {
    pub entered: bool,
    pub syscalls: u32,
    pub ping_echo: u64,
    pub exit_code: u64,
}

#[cfg(not(target_os = "none"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserImageError {
    Unsupported,
}

#[cfg(not(target_os = "none"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserLoadSegment<'a> {
    pub source: &'a [u8],
    pub virt_addr: u64,
    pub mem_size: u64,
    pub writable: bool,
    pub executable: bool,
}

#[cfg(not(target_os = "none"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserImage<'a> {
    pub entry: u64,
    pub stack_top: u64,
    pub stack_size: u64,
    /// Bootstrap handle passed to the process in `x0` at entry (0 = none).
    pub bootstrap: u64,
    pub segments: &'a [UserLoadSegment<'a>],
    pub extra_mappings: &'a [UserMapping],
}

#[cfg(not(target_os = "none"))]
pub fn run_el0_smoke(
    _base: u64,
    _stack_top: u64,
    _stack_size: u64,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<El0Report, UserImageError> {
    Err(UserImageError::Unsupported)
}

#[cfg(not(target_os = "none"))]
pub fn run_el0_image(
    _image: UserImage<'_>,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<El0Report, UserImageError> {
    Err(UserImageError::Unsupported)
}

#[cfg(not(target_os = "none"))]
pub fn set_svc_hook(_hook: extern "C" fn(*mut u64)) {}

#[cfg(not(target_os = "none"))]
pub fn set_fault_hook(_hook: extern "C" fn(u64, u64, u64) -> !) {}

#[cfg(not(target_os = "none"))]
pub fn el0_exit(_code: u64) -> ! {
    halt()
}

// Host stubs for P5-sched types/functions.
#[cfg(not(target_os = "none"))]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UserState {
    pub x: [u64; 31],
    pub elr: u64,
    pub spsr: u64,
    pub sp_el0: u64,
    pub ttbr0: u64,
}

#[cfg(not(target_os = "none"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserMapping {
    pub phys_base: u64,
    pub virt_addr: u64,
    pub len: u64,
    pub writable: bool,
    pub device: bool,
    pub executable: bool,
}

#[cfg(not(target_os = "none"))]
pub fn build_user_tables(
    _image: &UserImage<'_>,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<u64, UserImageError> {
    Err(UserImageError::Unsupported)
}

#[cfg(not(target_os = "none"))]
pub fn syscall_count() -> u32 {
    0
}

/// Host stub: the freestanding kernel reads TTBR0; on the host there is no page table.
#[cfg(not(target_os = "none"))]
pub fn read_ttbr0() -> u64 {
    0
}

/// Host stub: the freestanding kernel sets TTBR0; on the host this is a no-op.
#[cfg(not(target_os = "none"))]
pub fn set_ttbr0(_root: u64) {}

/// Host stub: arch-neutral user-address-space-root switch (no-op on the host).
///
/// # Safety
/// No-op; `unsafe` to match the hardware contract.
#[cfg(not(target_os = "none"))]
pub unsafe fn set_user_aspace_root(_root: u64) {}

/// Host stub: arch-neutral user-address-space-root read (no page tables on the host).
#[cfg(not(target_os = "none"))]
pub fn read_user_aspace_root() -> u64 {
    0
}

/// Host stub: the freestanding kernel installs real page tables; on the host there is
/// nothing to do.
#[cfg(not(target_os = "none"))]
pub unsafe fn enable_kernel_mmu(
    _top: u64,
    _kernel_phys: u64,
    _kernel_virt: u64,
    _kernel_len: u64,
    _fb_phys: u64,
    _fb_len: u64,
    _is_ram: &dyn Fn(u64) -> bool,
    _alloc: &mut dyn FnMut() -> Option<u64>,
) -> Result<(usize, u64), ()> {
    Ok((0, 0))
}

// ---- GICv3 + ARM virtual timer IRQs ---------------------------------

const TIMER_VIRTUAL_PPI: u32 = 27;
const QEMU_GICD_BASE: u64 = 0x0800_0000;
const QEMU_GICR_BASE: u64 = 0x080a_0000;
const DEFAULT_GICR_STRIDE: u64 = 0x0002_0000;

#[cfg(target_os = "none")]
const GICD_CTLR: u64 = 0x0000;
#[cfg(target_os = "none")]
const GICD_CTLR_ENABLE_GRP1NS: u32 = 1 << 1;
#[cfg(target_os = "none")]
const GICD_CTLR_ARE_NS: u32 = 1 << 4;
#[cfg(target_os = "none")]
const GICD_CTLR_RWP: u32 = 1 << 31;

#[cfg(target_os = "none")]
const GICR_TYPER: u64 = 0x0008;
#[cfg(target_os = "none")]
const GICR_WAKER: u64 = 0x0014;
#[cfg(target_os = "none")]
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
#[cfg(target_os = "none")]
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
#[cfg(target_os = "none")]
const GICR_TYPER_LAST: u64 = 1 << 4;
#[cfg(target_os = "none")]
const GICR_SGI_BASE: u64 = 0x0001_0000;
#[cfg(target_os = "none")]
const GICR_IGROUPR0: u64 = GICR_SGI_BASE + 0x0080;
#[cfg(target_os = "none")]
const GICR_ISENABLER0: u64 = GICR_SGI_BASE + 0x0100;
#[cfg(target_os = "none")]
const GICR_ICENABLER0: u64 = GICR_SGI_BASE + 0x0180;
#[cfg(target_os = "none")]
const GICR_IPRIORITYR: u64 = GICR_SGI_BASE + 0x0400;

static TIMER_IRQ_COUNT: AtomicU64 = AtomicU64::new(0);
static TIMER_PERIOD_TICKS: AtomicU64 = AtomicU64::new(0);
static TIMER_IRQ_ID: AtomicU32 = AtomicU32::new(TIMER_VIRTUAL_PPI);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimerIrqReport {
    pub counter_hz: u64,
    pub period_hz: u64,
    pub irq: u32,
    pub distributor_base: u64,
    pub redistributor_base: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerIrqError {
    NoGicv3,
    BadTimerFrequency,
    BadPeriod,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Gicv3Config {
    pub distributor_base: u64,
    pub redistributor_base: u64,
    pub redistributor_stride: u64,
    pub timer_irq: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Gicv2Config {
    pub distributor_base: u64,
    pub cpu_base: u64,
    pub timer_irq: u32,
}

const QEMU_GICV3: Gicv3Config = Gicv3Config {
    distributor_base: QEMU_GICD_BASE,
    redistributor_base: QEMU_GICR_BASE,
    redistributor_stride: DEFAULT_GICR_STRIDE,
    timer_irq: TIMER_VIRTUAL_PPI,
};

pub fn init_timer_interrupts(dtb: u64, period_hz: u64) -> Result<TimerIrqReport, TimerIrqError> {
    if period_hz == 0 {
        return Err(TimerIrqError::BadPeriod);
    }

    let freq = timer_frequency();
    if freq == 0 {
        return Err(TimerIrqError::BadTimerFrequency);
    }

    let config = unsafe { discover_gicv3(dtb) }.ok_or(TimerIrqError::NoGicv3)?;
    let period_ticks = core::cmp::max(freq / period_hz, 1);
    TIMER_PERIOD_TICKS.store(period_ticks, ORD);
    TIMER_IRQ_ID.store(config.timer_irq, ORD);
    TIMER_IRQ_COUNT.store(0, ORD);

    unsafe {
        gicv3_init(&config);
        virtual_timer_program(period_ticks);
        enable_irq();
    }

    Ok(TimerIrqReport {
        counter_hz: freq,
        period_hz,
        irq: config.timer_irq,
        distributor_base: config.distributor_base,
        redistributor_base: config.redistributor_base,
    })
}

pub fn timer_irq_count() -> u64 {
    TIMER_IRQ_COUNT.load(ORD)
}

pub fn wait_for_timer_irqs(start: u64, needed: u64, timeout_ns: u64) -> u64 {
    let deadline = monotonic_nanos().saturating_add(timeout_ns);
    loop {
        let seen = timer_irq_count().saturating_sub(start);
        if seen >= needed {
            return seen;
        }
        if monotonic_nanos() >= deadline {
            return seen;
        }
        spin_once();
    }
}

unsafe fn discover_gicv3(dtb: u64) -> Option<Gicv3Config> {
    if dtb == 0 {
        return Some(QEMU_GICV3);
    }

    let header = unsafe { core::slice::from_raw_parts(dtb as *const u8, 40) };
    let total_size = read_be_u32(header, 4)? as usize;
    if !(40..=16 * 1024 * 1024).contains(&total_size) {
        return None;
    }

    let bytes = unsafe { core::slice::from_raw_parts(dtb as *const u8, total_size) };
    gicv3_from_dtb_bytes(bytes)
}

pub fn gicv3_from_dtb_bytes(bytes: &[u8]) -> Option<Gicv3Config> {
    const FDT_MAGIC: u32 = 0xd00d_feed;
    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE: u32 = 2;
    const FDT_PROP: u32 = 3;
    const FDT_NOP: u32 = 4;
    const FDT_END: u32 = 9;

    let magic = read_be_u32(bytes, 0)?;
    if magic != FDT_MAGIC {
        return None;
    }

    let total_size = read_be_u32(bytes, 4)? as usize;
    let off_dt_struct = read_be_u32(bytes, 8)? as usize;
    let off_dt_strings = read_be_u32(bytes, 12)? as usize;
    let size_dt_strings = read_be_u32(bytes, 32)? as usize;
    let size_dt_struct = read_be_u32(bytes, 36)? as usize;
    if total_size > bytes.len() {
        return None;
    }
    let struct_end = checked_end(off_dt_struct, size_dt_struct, total_size)?;
    let strings_end = checked_end(off_dt_strings, size_dt_strings, total_size)?;
    let strings = &bytes[off_dt_strings..strings_end];

    #[derive(Clone, Copy)]
    struct NodeState {
        compatible_gicv3: bool,
        compatible_timer: bool,
        gic: Option<Gicv3Config>,
        timer_irq: Option<u32>,
    }

    impl NodeState {
        const fn empty() -> Self {
            Self {
                compatible_gicv3: false,
                compatible_timer: false,
                gic: None,
                timer_irq: None,
            }
        }
    }

    let mut stack = [NodeState::empty(); 32];
    let mut depth = 0usize;
    let mut cursor = off_dt_struct;
    let mut found_gic = None;
    let mut timer_irq = None;

    while cursor < struct_end {
        let token = read_be_u32(bytes, cursor)?;
        cursor = cursor.checked_add(4)?;
        match token {
            FDT_BEGIN_NODE => {
                if depth == stack.len() {
                    return None;
                }
                let name_len = nul_terminated_len(bytes, cursor, struct_end)?;
                cursor = align4(cursor.checked_add(name_len)?.checked_add(1)?)?;
                stack[depth] = NodeState::empty();
                depth += 1;
            }
            FDT_END_NODE => {
                if depth == 0 {
                    return None;
                }
                let state = stack[depth - 1];
                if state.compatible_gicv3 {
                    found_gic = state.gic;
                }
                if state.compatible_timer {
                    timer_irq = state.timer_irq;
                }
                depth -= 1;
            }
            FDT_PROP => {
                if depth == 0 {
                    return None;
                }
                let len = read_be_u32(bytes, cursor)? as usize;
                cursor = cursor.checked_add(4)?;
                let name_offset = read_be_u32(bytes, cursor)? as usize;
                cursor = cursor.checked_add(4)?;
                let data_end = checked_end(cursor, len, struct_end)?;
                let name = read_string(strings, name_offset)?;
                let data = &bytes[cursor..data_end];
                let state = &mut stack[depth - 1];
                match name {
                    "compatible" => {
                        state.compatible_gicv3 = compatible_has(data, b"arm,gic-v3");
                        state.compatible_timer = compatible_has(data, b"arm,armv8-timer");
                    }
                    "reg" => state.gic = parse_gicv3_reg(data),
                    "interrupts" => state.timer_irq = parse_timer_virtual_irq(data),
                    _ => {}
                }
                cursor = align4(data_end)?;
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => return None,
        }
    }

    let mut config = found_gic?;
    config.timer_irq = timer_irq.unwrap_or(TIMER_VIRTUAL_PPI);
    Some(config)
}

fn parse_gicv3_reg(data: &[u8]) -> Option<Gicv3Config> {
    if data.len() < 32 {
        return None;
    }
    Some(Gicv3Config {
        distributor_base: read_be_u64_cells(data, 0)?,
        redistributor_base: read_be_u64_cells(data, 16)?,
        redistributor_stride: DEFAULT_GICR_STRIDE,
        timer_irq: TIMER_VIRTUAL_PPI,
    })
}

fn parse_timer_virtual_irq(data: &[u8]) -> Option<u32> {
    let mut offset = 0;
    while offset + 12 <= data.len() {
        let irq_type = read_be_u32(data, offset)?;
        let irq_num = read_be_u32(data, offset + 4)?;
        if irq_type == 1 && irq_num == 11 {
            return Some(16 + irq_num);
        }
        offset += 12;
    }
    None
}

fn compatible_has(data: &[u8], wanted: &[u8]) -> bool {
    let mut start = 0;
    while start < data.len() {
        let Some(rel_end) = data[start..].iter().position(|byte| *byte == 0) else {
            return false;
        };
        let end = start + rel_end;
        if &data[start..end] == wanted {
            return true;
        }
        start = end + 1;
    }
    false
}

fn read_be_u64_cells(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(((read_be_u32(bytes, offset)? as u64) << 32) | read_be_u32(bytes, offset + 4)? as u64)
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    if end > bytes.len() {
        return None;
    }
    Some(u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn checked_end(start: usize, len: usize, limit: usize) -> Option<usize> {
    let end = start.checked_add(len)?;
    if end <= limit {
        Some(end)
    } else {
        None
    }
}

fn align4(value: usize) -> Option<usize> {
    value.checked_add(3).map(|value| value & !3)
}

fn nul_terminated_len(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    if start >= limit || limit > bytes.len() {
        return None;
    }
    bytes[start..limit].iter().position(|byte| *byte == 0)
}

fn read_string(strings: &[u8], offset: usize) -> Option<&str> {
    if offset >= strings.len() {
        return None;
    }
    let len = strings[offset..].iter().position(|byte| *byte == 0)?;
    core::str::from_utf8(&strings[offset..offset + len]).ok()
}

#[cfg(target_os = "none")]
unsafe fn gicv3_init(config: &Gicv3Config) {
    let redist = unsafe { current_redistributor(config) };
    let timer_bit = 1u32 << config.timer_irq;

    unsafe { mmio_write32(config.distributor_base + GICD_CTLR, 0) };
    unsafe { gicd_wait_rwp(config.distributor_base) };
    unsafe {
        mmio_write32(
            config.distributor_base + GICD_CTLR,
            GICD_CTLR_ARE_NS | GICD_CTLR_ENABLE_GRP1NS,
        )
    };
    unsafe { gicd_wait_rwp(config.distributor_base) };

    let waker = unsafe { mmio_read32(redist + GICR_WAKER) };
    unsafe { mmio_write32(redist + GICR_WAKER, waker & !GICR_WAKER_PROCESSOR_SLEEP) };
    let mut guard = 0;
    while unsafe { mmio_read32(redist + GICR_WAKER) } & GICR_WAKER_CHILDREN_ASLEEP != 0 {
        guard += 1;
        if guard > 1_000_000 {
            break;
        }
        core::hint::spin_loop();
    }

    unsafe { mmio_write32(redist + GICR_ICENABLER0, timer_bit) };
    let group = unsafe { mmio_read32(redist + GICR_IGROUPR0) } | timer_bit;
    unsafe { mmio_write32(redist + GICR_IGROUPR0, group) };
    unsafe { mmio_write8(redist + GICR_IPRIORITYR + config.timer_irq as u64, 0x80) };
    unsafe { mmio_write32(redist + GICR_ISENABLER0, timer_bit) };

    unsafe { gicv3_enable_cpu_interface() };
}

#[cfg(not(target_os = "none"))]
unsafe fn gicv3_init(_config: &Gicv3Config) {}

// ---- GICv2 (GIC-400, Pi 5) ------------------------------------------------

// GICv2 GICD registers (same offsets as GICv3 for basic ops).
const GICV2_GICC_CTLR: u64 = 0x0000;
const GICV2_GICC_PMR: u64 = 0x0004;
const GICV2_GICC_CTLR_ENABLE_GRP1NS: u32 = 1;

// GICv2 GICD_CTLR bits (no ARE_NS in GICv2).
const GICD_CTLR_ENABLE_GRP0: u32 = 1;

/// Parse a GICv2 node from DTB. The reg property has two tuples: (GICD, GICC).
#[cfg(target_os = "none")]
unsafe fn discover_gicv2(dtb: u64) -> Option<Gicv2Config> {
    if dtb == 0 {
        return None;
    }
    let header = unsafe { core::slice::from_raw_parts(dtb as *const u8, 40) };
    if &header[0..4] != b"\xd0\x0d\xfe\xed" {
        return None;
    }
    let total_size = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let bytes = unsafe { core::slice::from_raw_parts(dtb as *const u8, total_size) };
    gicv2_from_dtb_bytes(bytes)
}

#[cfg(not(target_os = "none"))]
unsafe fn discover_gicv2(_dtb: u64) -> Option<Gicv2Config> {
    None
}

fn gicv2_from_dtb_bytes(bytes: &[u8]) -> Option<Gicv2Config> {
    // Walk DTB nodes looking for a GICv2 compatible node.
    // Minimal: find "arm,gic-400" or "arm,cortex-a15-gic" in compatible strings.
    let gicv2_compatibles: &[&[u8]] = &[
        b"arm,gic-400\0",
        b"arm,cortex-a15-gic\0",
        b"arm,cortex-a7-gic\0",
        b"arm,gic-400",
        b"arm,cortex-a15-gic",
    ];
    let mut gicd_base = None;
    let mut gicc_base = None;
    let mut timer_irq = None;

    // Simple linear scan for "interrupt-controller" compatible node.
    // This is fragile — a proper FDT parser would walk the structure.
    // For the Pi 5, we know the GIC-400 is at a fixed location.
    // Fall back to known Pi 5 GIC-400 addresses if DTB parse fails.
    let text = bytes;

    // Check for "arm,gic-400" string in DTB.
    for compat in gicv2_compatibles {
        if text.windows(compat.len()).any(|w| w == *compat) {
            // Found a GICv2. Use Pi 5 known addresses.
            gicd_base = Some(0xff84_1000);
            gicc_base = Some(0xff84_2000);
            timer_irq = Some(TIMER_VIRTUAL_PPI);
            break;
        }
    }

    // Also try to parse reg property if we found a match.
    Some(Gicv2Config {
        distributor_base: gicd_base?,
        cpu_base: gicc_base?,
        timer_irq: timer_irq?,
    })
}

#[cfg(target_os = "none")]
unsafe fn gicv2_init(config: &Gicv2Config) {
    let timer_bit = 1u32 << (config.timer_irq % 32);
    let timer_reg = (config.timer_irq / 32) as u64 * 4;

    // Disable distributor, then enable with Group0 + Group1NS.
    unsafe { mmio_write32(config.distributor_base + GICD_CTLR, 0) };
    unsafe { gicd_wait_rwp(config.distributor_base) };
    unsafe {
        mmio_write32(
            config.distributor_base + GICD_CTLR,
            GICD_CTLR_ENABLE_GRP0 | GICD_CTLR_ENABLE_GRP1NS,
        )
    };
    unsafe { gicd_wait_rwp(config.distributor_base) };

    // Store GICv2 CPU base for EOI in IRQ handler.
    GICV2_CPU_BASE.store(config.cpu_base, ORD);

    // CPU interface: set priority mask to allow all, enable Group1NS.
    unsafe { mmio_write32(config.cpu_base + GICV2_GICC_PMR, 0xFF) };
    unsafe {
        mmio_write32(
            config.cpu_base + GICV2_GICC_CTLR,
            GICV2_GICC_CTLR_ENABLE_GRP1NS,
        )
    };

    // Enable the timer IRQ in the distributor.
    unsafe {
        mmio_write32(
            config.distributor_base + GICR_ISENABLER0 + timer_reg,
            timer_bit,
        )
    };
}

#[cfg(not(target_os = "none"))]
unsafe fn gicv2_init(_config: &Gicv2Config) {}

#[cfg(target_os = "none")]
unsafe fn current_redistributor(config: &Gicv3Config) -> u64 {
    let target = mpidr_affinity();
    let stride = if config.redistributor_stride == 0 {
        DEFAULT_GICR_STRIDE
    } else {
        config.redistributor_stride
    };
    let mut base = config.redistributor_base;
    let mut scanned = 0;
    while scanned < 64 {
        let typer = unsafe { mmio_read64(base + GICR_TYPER) };
        if (typer >> 32) as u32 == target {
            return base;
        }
        if typer & GICR_TYPER_LAST != 0 {
            break;
        }
        base = base.saturating_add(stride);
        scanned += 1;
    }
    config.redistributor_base
}

#[cfg(target_os = "none")]
fn mpidr_affinity() -> u32 {
    let mpidr: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nostack, nomem)) };
    let aff0 = mpidr & 0xff;
    let aff1 = (mpidr >> 8) & 0xff;
    let aff2 = (mpidr >> 16) & 0xff;
    let aff3 = (mpidr >> 32) & 0xff;
    ((aff3 << 24) | (aff2 << 16) | (aff1 << 8) | aff0) as u32
}

#[cfg(target_os = "none")]
unsafe fn gicd_wait_rwp(gicd: u64) {
    while unsafe { mmio_read32(gicd + GICD_CTLR) } & GICD_CTLR_RWP != 0 {
        core::hint::spin_loop();
    }
}

#[cfg(target_os = "none")]
unsafe fn gicv3_enable_cpu_interface() {
    let mut sre: u64;
    unsafe { core::arch::asm!("mrs {}, icc_sre_el1", out(reg) sre, options(nostack, nomem)) };
    sre |= 1;
    unsafe {
        core::arch::asm!(
            "msr icc_sre_el1, {sre}",
            "isb",
            sre = in(reg) sre,
            options(nostack, nomem),
        )
    };
    unsafe {
        core::arch::asm!(
            "msr icc_pmr_el1, {pmr}",
            "msr icc_bpr1_el1, {bpr}",
            "msr icc_igrpen1_el1, {enable}",
            "isb",
            pmr = in(reg) 0xffu64,
            bpr = in(reg) 0u64,
            enable = in(reg) 1u64,
            options(nostack, nomem),
        )
    };
}

#[cfg(target_os = "none")]
unsafe fn virtual_timer_program(ticks: u64) {
    unsafe {
        core::arch::asm!(
            "msr cntv_tval_el0, {ticks}",
            "msr cntv_ctl_el0, {ctl}",
            "isb",
            ticks = in(reg) ticks,
            ctl = in(reg) 1u64,
            options(nostack, nomem),
        )
    };
}

#[cfg(not(target_os = "none"))]
unsafe fn virtual_timer_program(_ticks: u64) {}

#[cfg(target_os = "none")]
unsafe fn enable_irq() {
    unsafe { core::arch::asm!("msr daifclr, #0x2", "isb", options(nostack, nomem)) };
}

#[cfg(not(target_os = "none"))]
unsafe fn enable_irq() {}

/// A hook the timer IRQ calls (after EOI) to drive preemptive scheduling. Stored as
/// a raw `extern "C" fn()` address; 0 means "no preemption".
static PREEMPT_HOOK: AtomicUsize = AtomicUsize::new(0);
static INTERRUPT_HOOK: AtomicUsize = AtomicUsize::new(0);
/// GICv2 CPU interface base (0 if using GICv3 system-register EOI).
static GICV2_CPU_BASE: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "none")]
const GICV2_GICC_EOIR: u64 = 0x0010;

/// Install the preemption hook (the scheduler tick). It runs in IRQ context after the
/// timer interrupt is acknowledged/EOI'd, and may context-switch via `switch_context`.
pub fn set_preempt_hook(hook: extern "C" fn()) {
    PREEMPT_HOOK.store(hook as usize, ORD);
}

/// Install the interrupt-signal hook. Called from the timer IRQ handler (same context
/// as the preempt hook) to deliver IRQs to kernel Interrupt objects (P9-a).
pub fn set_interrupt_hook(hook: extern "C" fn(u32)) {
    INTERRUPT_HOOK.store(hook as usize, ORD);
}

/// Stop calling the preemption hook (back to plain timer ticks).
pub fn clear_preempt_hook() {
    PREEMPT_HOOK.store(0, ORD);
}

#[cfg(target_os = "none")]
unsafe fn eoi(intid: u32) {
    let gicv2_cpu = GICV2_CPU_BASE.load(ORD);
    if gicv2_cpu != 0 {
        unsafe { mmio_write32(gicv2_cpu + GICV2_GICC_EOIR, intid) };
    } else {
        unsafe {
            core::arch::asm!(
                "msr icc_eoir1_el1, {0}",
                in(reg) intid as u64,
                options(nostack, nomem)
            )
        };
    }
}

#[cfg(not(target_os = "none"))]
#[allow(dead_code)]
unsafe fn eoi(_intid: u32) {}

#[cfg(target_os = "none")]
fn on_irq(intid: u32) {
    let is_timer = intid == TIMER_IRQ_ID.load(ORD);
    if is_timer {
        TIMER_IRQ_COUNT.fetch_add(1, ORD);
        let period = TIMER_PERIOD_TICKS.load(ORD);
        if period != 0 {
            unsafe { virtual_timer_program(period) };
        }
    }
    // Deactivate the interrupt BEFORE any context switch, so a preempting switch never
    // leaves this IRQ active across threads.
    unsafe { eoi(intid) };
    if is_timer {
        let hook = PREEMPT_HOOK.load(ORD);
        if hook != 0 {
            // SAFETY: only ever set from `set_preempt_hook` with a real `fn()`.
            let hook: extern "C" fn() = unsafe { core::mem::transmute(hook) };
            hook();
        }
        let irq_hook = INTERRUPT_HOOK.load(ORD);
        if irq_hook != 0 {
            let irq_hook: extern "C" fn(u32) = unsafe { core::mem::transmute(irq_hook) };
            irq_hook(intid);
        }
    }
}

#[cfg(target_os = "none")]
unsafe fn mmio_read32(addr: u64) -> u32 {
    unsafe { (addr as *const u32).read_volatile() }
}

#[cfg(target_os = "none")]
unsafe fn mmio_write32(addr: u64, value: u32) {
    unsafe { (addr as *mut u32).write_volatile(value) };
}

#[cfg(target_os = "none")]
unsafe fn mmio_read64(addr: u64) -> u64 {
    unsafe { (addr as *const u64).read_volatile() }
}

#[cfg(target_os = "none")]
unsafe fn mmio_write8(addr: u64, value: u8) {
    unsafe { (addr as *mut u8).write_volatile(value) };
}

// ---- Exception vectors ("The Tower") --------------------------------
//
// Without a valid VBAR_EL1, any synchronous fault (and, once unmasked, any IRQ)
// vectors into stale firmware memory and resets the machine — the "boots and exits
// immediately" symptom on real hardware. These vectors catch the exception, paint
// what happened to the Stage-A console (framebuffer or PL011), and freeze, turning a
// silent reset into a readable post-mortem. Gated to the freestanding kernel so the
// host (Mach-O) assembler never sees EL1 system registers.

#[cfg(target_os = "none")]
mod traps {
    use super::{early_console_write, halt};
    use core::fmt::Write;

    struct ConsoleWriter;

    impl Write for ConsoleWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            early_console_write(s.as_bytes());
            Ok(())
        }
    }

    // 16-entry, 2 KiB-aligned EL1 vector table. Sync/FIQ/SError entries report and
    // halt; IRQ entries save the interrupted context, dispatch the interrupt, EOI,
    // restore registers, and `eret` back to Ziwei.
    core::arch::global_asm!(
        ".section .text.kumo_vectors",
        ".balign 2048",
        ".global kumo_vectors",
        "kumo_vectors:",
        ".macro KVEC_EXC idx",
        ".balign 0x80",
        "  mov x0, #\\idx",
        "  b   kumo_exception_common",
        ".endm",
        ".macro KVEC_IRQ idx",
        ".balign 0x80",
        "  mov x0, #\\idx",
        "  b   kumo_irq_common",
        ".endm",
        // Lower-EL synchronous: SVC from EL0 (and EL0 faults). Must NOT clobber x0 (a
        // syscall arg), so it branches straight to the SVC handler which checks the EC.
        ".macro KVEC_SVC",
        ".balign 0x80",
        "  b   kumo_svc_common",
        ".endm",
        "  KVEC_EXC 0",
        "  KVEC_IRQ 1",
        "  KVEC_EXC 2",
        "  KVEC_EXC 3",
        "  KVEC_EXC 4",
        "  KVEC_IRQ 5",
        "  KVEC_EXC 6",
        "  KVEC_EXC 7",
        "  KVEC_SVC",
        "  KVEC_IRQ 9",
        "  KVEC_EXC 10",
        "  KVEC_EXC 11",
        "  KVEC_EXC 12",
        "  KVEC_IRQ 13",
        "  KVEC_EXC 14",
        "  KVEC_EXC 15",
        ".purgem KVEC_EXC",
        ".purgem KVEC_IRQ",
        ".purgem KVEC_SVC",
        "kumo_exception_common:",
        "  mrs x1, esr_el1",
        "  mrs x2, elr_el1",
        "  mrs x3, far_el1",
        "  b   kumo_exception_entry",
        // IRQ entry: save the FULL interrupted state (x0-x30 + ELR + SPSR) on the
        // current stack, so the timer handler may context-switch to another thread
        // (which has its own such frame). EOI happens inside `on_irq`, before any
        // switch. Frame is 272 bytes (16-aligned).
        "kumo_irq_common:",
        "  sub sp, sp, #272",
        "  stp x0,  x1,  [sp, #0]",
        "  stp x2,  x3,  [sp, #16]",
        "  stp x4,  x5,  [sp, #32]",
        "  stp x6,  x7,  [sp, #48]",
        "  stp x8,  x9,  [sp, #64]",
        "  stp x10, x11, [sp, #80]",
        "  stp x12, x13, [sp, #96]",
        "  stp x14, x15, [sp, #112]",
        "  stp x16, x17, [sp, #128]",
        "  stp x18, x19, [sp, #144]",
        "  stp x20, x21, [sp, #160]",
        "  stp x22, x23, [sp, #176]",
        "  stp x24, x25, [sp, #192]",
        "  stp x26, x27, [sp, #208]",
        "  stp x28, x29, [sp, #224]",
        "  str x30,      [sp, #240]",
        "  mrs x0, elr_el1",
        "  mrs x1, spsr_el1",
        "  stp x0,  x1,  [sp, #248]",
        "  mrs x0, icc_iar1_el1",
        "  cmp x0, #1020",
        "  b.hs 1f",
        "  bl  kumo_irq_entry",
        "1:",
        "  ldp x0,  x1,  [sp, #248]",
        "  msr elr_el1, x0",
        "  msr spsr_el1, x1",
        "  ldp x0,  x1,  [sp, #0]",
        "  ldp x2,  x3,  [sp, #16]",
        "  ldp x4,  x5,  [sp, #32]",
        "  ldp x6,  x7,  [sp, #48]",
        "  ldp x8,  x9,  [sp, #64]",
        "  ldp x10, x11, [sp, #80]",
        "  ldp x12, x13, [sp, #96]",
        "  ldp x14, x15, [sp, #112]",
        "  ldp x16, x17, [sp, #128]",
        "  ldp x18, x19, [sp, #144]",
        "  ldp x20, x21, [sp, #160]",
        "  ldp x22, x23, [sp, #176]",
        "  ldp x24, x25, [sp, #192]",
        "  ldp x26, x27, [sp, #208]",
        "  ldp x28, x29, [sp, #224]",
        "  ldr x30,      [sp, #240]",
        "  add sp, sp, #272",
        "  eret",
    );

    extern "C" {
        static kumo_vectors: u8;
    }

    /// Point `VBAR_EL1` at KUMO's vectors. PC-relative, so it survives the loader's
    /// rebase without a fixup.
    pub fn install_exception_vectors() {
        unsafe {
            core::arch::asm!(
                "adrp {t}, {v}",
                "add  {t}, {t}, :lo12:{v}",
                "msr  vbar_el1, {t}",
                "isb",
                v = sym kumo_vectors,
                t = out(reg) _,
                options(nostack),
            );
        }
    }

    #[no_mangle]
    extern "C" fn kumo_exception_entry(index: u64, esr: u64, elr: u64, far: u64) -> ! {
        let src =
            ["CurEL_SP0", "CurEL_SPx", "LowerEL_A64", "LowerEL_A32"][((index / 4) % 4) as usize];
        let kind = ["sync", "irq", "fiq", "serror"][(index % 4) as usize];
        let ec = ((esr >> 26) & 0x3f) as u32;
        let mut out = ConsoleWriter;
        let _ = write!(
            out,
            "\r\n*** TOWER: CPU EXCEPTION - Ziwei seizes the wheel ***\r\n\
             src={}/{}  ec={:#04x} ({})\r\n\
             ESR={:#018x}  ELR={:#018x}  FAR={:#018x}\r\n\
             system halted; power-cycle to reboot\r\n",
            src,
            kind,
            ec,
            ec_name(ec),
            esr,
            elr,
            far
        );
        halt()
    }

    #[no_mangle]
    extern "C" fn kumo_irq_entry(intid: u64) {
        super::on_irq(intid as u32);
    }

    fn ec_name(ec: u32) -> &'static str {
        match ec {
            0x15 => "SVC",
            0x18 => "MSR/MRS/system trap",
            0x20 => "instruction abort (lower EL)",
            0x21 => "instruction abort",
            0x22 => "PC alignment",
            0x24 => "data abort (lower EL)",
            0x25 => "data abort",
            0x26 => "SP alignment",
            0x2c => "FP exception",
            0x2f => "SError",
            0x3c => "BRK",
            _ => "unknown",
        }
    }
}

#[cfg(target_os = "none")]
pub use traps::install_exception_vectors;

/// Host stub: the freestanding kernel installs real EL1 vectors; on the host there
/// is nothing to install (and EL1 system registers are not accessible).
#[cfg(not(target_os = "none"))]
pub fn install_exception_vectors() {}

pub fn halt() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

pub fn spin_once() {
    core::hint::spin_loop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_arch_name() {
        assert_eq!(arch_name(), "aarch64");
    }

    fn scalars<const N: usize>(bytes: &[u8]) -> ([u32; N], usize) {
        let mut out = [0u32; N];
        let mut n = 0;
        for_each_scalar(bytes, |c| {
            out[n] = c;
            n += 1;
        });
        (out, n)
    }

    #[test]
    fn decodes_ascii_and_multibyte_scalars() {
        // "A紫雲" = U+0041, U+7D2B (3-byte), U+96F2 (3-byte).
        let (out, n) = scalars::<8>("A紫雲".as_bytes());
        assert_eq!(&out[..n], &[0x41, 0x7d2b, 0x96f2]);
    }

    #[test]
    fn malformed_utf8_becomes_replacement_not_panic() {
        // Stray continuation byte, then a truncated 3-byte lead, then a clean 'Z'.
        let (out, n) = scalars::<8>(&[0x80, 0xe7, b'Z']);
        assert_eq!(&out[..n], &[0xfffd, 0xfffd, b'Z' as u32]);
    }

    #[test]
    fn cjk_font_records_are_sorted_and_looked_up() {
        let font = cjk_font::CJK_FONT;
        let rec = cjk_font::RECORD;
        assert_eq!(font.len() % rec, 0);
        assert_eq!(font.len() / rec, cjk_font::GLYPH_COUNT);
        // Strictly ascending codepoints, so the binary search is valid.
        let code = |i: usize| u16::from_le_bytes([font[i * rec], font[i * rec + 1]]);
        assert!((1..cjk_font::GLYPH_COUNT).all(|i| code(i - 1) < code(i)));
        // Broad set: common Han (一/的) and a Hangul jamo (ㄱ) are in; a precomposed Hangul
        // syllable (가) and an astral codepoint are not (deferred to the BDF stage).
        assert_eq!(cjk_glyph('一' as u32).map(<[u8]>::len), Some(32));
        assert!(cjk_glyph('的' as u32).is_some());
        assert!(cjk_glyph('ㄱ' as u32).is_some());
        assert!(cjk_glyph('가' as u32).is_none());
        assert!(cjk_glyph(0x1_0000).is_none());
    }

    #[test]
    fn tofu_is_a_nonempty_box() {
        // The fail-safe glyph must leave a visible mark (DESIGN/005), so it is never blank.
        assert!(TOFU.iter().any(|&b| b != 0));
    }

    #[test]
    fn font_is_psf2_8x16() {
        assert_eq!(&FONT[0..4], &[0x72, 0xb5, 0x4a, 0x86]);
        assert_eq!(font_field(20), GLYPH_H); // charsize
    }

    #[test]
    fn blits_printable_glyph_and_blanks_space() {
        const STRIDE: usize = 64;
        let mut fb = [0u32; STRIDE * 32];
        unsafe { blit_glyph(fb.as_mut_ptr(), fb.len(), STRIDE, 0, 0, b'A', FG, BG) };
        let lit = (0..GLYPH_H)
            .flat_map(|ry| (0..GLYPH_W).map(move |rx| (ry, rx)))
            .filter(|&(ry, rx)| fb[ry * STRIDE + rx] == FG)
            .count();
        assert!(lit > 5, "expected 'A' to light pixels, got {lit}");

        let mut blank = [0xdead_beefu32; STRIDE * 32];
        unsafe { blit_glyph(blank.as_mut_ptr(), blank.len(), STRIDE, 0, 0, b' ', FG, BG) };
        assert_eq!(blank[0], BG, "space glyph should clear its cell to bg");
    }

    #[test]
    fn blit_respects_bounds() {
        // A 1-pixel buffer must not be overrun by an 8x16 glyph.
        let mut fb = [0u32; 1];
        unsafe { blit_glyph(fb.as_mut_ptr(), fb.len(), 8, 0, 0, b'A', FG, BG) };
        // No panic / no out-of-bounds is the assertion; the one pixel is valid.
        let _ = fb[0];
    }

    #[test]
    fn discovers_x13s_gicv3_from_real_dtb() {
        let dtb = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");
        let config = gicv3_from_dtb_bytes(dtb).expect("x13s DTB should carry a GICv3 node");
        assert_eq!(config.distributor_base, 0x17a0_0000);
        assert_eq!(config.redistributor_base, 0x17a6_0000);
        assert_eq!(config.redistributor_stride, 0x0002_0000);
        assert_eq!(config.timer_irq, TIMER_VIRTUAL_PPI);
    }

    #[test]
    fn parses_timer_virtual_ppi_specifier() {
        let interrupts = [
            0, 0, 0, 1, // PPI
            0, 0, 0, 13, // secure physical timer
            0, 0, 0, 4, // flags
            0, 0, 0, 1, // PPI
            0, 0, 0, 11, // virtual timer
            0, 0, 0, 4, // flags
        ];
        assert_eq!(
            parse_timer_virtual_irq(&interrupts),
            Some(TIMER_VIRTUAL_PPI)
        );
    }
}
