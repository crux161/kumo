//! KUMO's kernel glass console: the Stage-A text renderer both HAL backends draw with.
//!
//! j905 (PLAN/025 X2a) lifted this out of `kumo-hal-aarch64` unchanged so the x86_64 backend
//! can draw the same console on a NUC's GOP framebuffer. The renderer is pure CPU drawing into
//! a linear 32-bpp buffer; the only architecture facts are how a written line reaches the
//! scanout and where the live kernel map puts the aperture, which a backend supplies through
//! [`Scanout`]. arm64 cleans each line to PoC (`dc cvac`) and barriers with `dsb ish`; x86
//! needs neither (its aperture is write-combining and coherent) and fences once.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//j905

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

mod cjk_font;

pub use cjk_font::{CJK_FONT, GLYPH_COUNT as CJK_GLYPH_COUNT, RECORD as CJK_RECORD};

const ORD: Ordering = Ordering::Relaxed;

/// How the drawn pixels reach the display, per architecture.
pub trait Scanout {
    /// Make the line holding `addr` (a live kernel VA inside the framebuffer) visible to the
    /// scanout. Called after every glyph row, scroll and fill, exactly where arm64 has always
    /// cleaned; a coherent backend makes it a no-op.
    fn clean_line(addr: usize);
    /// Complete every earlier [`Self::clean_line`] and pixel write before returning.
    fn barrier();
    /// Added to the framebuffer's physical base to address it from the active kernel map
    /// (0 while the identity map is live).
    fn va_offset() -> u64;
}

/// Cell geometry of the console font, in pixels.
pub const GLYPH_W: usize = 8;
/// See [`GLYPH_W`].
pub const GLYPH_H: usize = 16;

/// Embedded 8x16 console font (PSF2). Kept in-tree so KUMO stays self-contained.
const FONT: &[u8] = include_bytes!("../font8x16.psf");
// PLAN/016 C0: crisp off-white on black. R, G and B are equal, so 0x00ee_eeee is the
// same pixel in RGB888x and BGR888x — format-agnostic, like the green it replaced.
pub const FG: u32 = 0x00ee_eeee; // off-white
pub const BG: u32 = 0x0000_0000; // black

static FB_PRESENT: AtomicBool = AtomicBool::new(false);
static FB_BASE: AtomicU64 = AtomicU64::new(0);
static FB_LEN_PX: AtomicUsize = AtomicUsize::new(0);
static FB_WIDTH: AtomicU32 = AtomicU32::new(0);
static FB_HEIGHT: AtomicU32 = AtomicU32::new(0);
static FB_STRIDE: AtomicU32 = AtomicU32::new(0);
static FB_COL: AtomicU32 = AtomicU32::new(0);
static FB_ROW: AtomicU32 = AtomicU32::new(0);
/// True only while the Stage-A text console owns the framebuffer. A successful
/// userspace driver claim clears this permanently until a fatal kernel path reclaims
/// the glass; two independent cursors must never paint it concurrently.
static FB_KERNEL_OWNER: AtomicBool = AtomicBool::new(true);

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

/// Blit one printable ASCII glyph (the 8x16 PSF cell). Thin wrapper over
/// [`blit_glyph_rows`]; the box-drawing path shares the same single-width blit.
///
/// # Safety
/// `base` must point at `len_px` writable `u32` pixels.
unsafe fn blit_glyph<S: Scanout>(
    base: *mut u32,
    len_px: usize,
    stride: usize,
    x_px: usize,
    y_px: usize,
    ch: u8,
    fg: u32,
    bg: u32,
) {
    unsafe { blit_glyph_rows::<S>(base, len_px, stride, x_px, y_px, glyph_rows(ch), fg, bg) };
}

/// Blit a single-width 8x16 cell from a raw `rows` bitmap — one byte per scanline, MSB the
/// leftmost pixel. Shared by the ASCII PSF path ([`blit_glyph`]) and the box-drawing path
/// ([`box_bitmap`]). Bounds-checked against `len_px`, so a bad geometry truncates instead of
/// scribbling past the buffer. Pure with respect to console state, which makes it host-testable
/// against an in-memory buffer.
///
/// # Safety
/// `base` must point at `len_px` writable `u32` pixels.
unsafe fn blit_glyph_rows<S: Scanout>(
    base: *mut u32,
    len_px: usize,
    stride: usize,
    x_px: usize,
    y_px: usize,
    rows: &[u8],
    fg: u32,
    bg: u32,
) {
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
        // [`Scanout::clean_line`]). The 8-pixel span is one cache line; clean both ends in case
        // it straddles a line boundary.
        if row_start < len_px {
            unsafe {
                S::clean_line(base.add(row_start) as usize);
                let row_end = row_start + GLYPH_W - 1;
                if row_end < len_px {
                    S::clean_line(base.add(row_end) as usize);
                }
            }
        }
    }
    S::barrier();
}

// ---- Light box-drawing glyphs (single-width 8x16) -------------------
//
// Box-drawing scalars are >= 0x80, so they would otherwise fall into the double-width CJK
// path and render as tofu (absent from the curated set) — and even if added there, the
// double-width cell would misalign TUI frames against single-width text. Instead a small
// explicit set renders single-width 8x16, generated from the four arms that reach the cell
// centre so adjacent cells join flush. Heavy/double/dashed/rounded variants and block
// elements are deliberately out of scope and still fall through to the CJK/tofu path.

/// The four arms of a light box-drawing cell that reach the cell centre.
struct BoxArms {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
}

/// Map a light single-line box-drawing scalar (a subset of U+2500..U+257F) to its arms, or
/// `None` for any codepoint this slice does not render single-width.
fn box_arms(scalar: u32) -> Option<BoxArms> {
    let (up, down, left, right) = match scalar {
        0x2500 => (false, false, true, true), // ─ horizontal
        0x2502 => (true, true, false, false), // │ vertical
        0x250c => (false, true, false, true), // ┌ down + right
        0x2510 => (false, true, true, false), // ┐ down + left
        0x2514 => (true, false, false, true), // └ up + right
        0x2518 => (true, false, true, false), // ┘ up + left
        0x251c => (true, true, false, true),  // ├ vertical + right
        0x2524 => (true, true, true, false),  // ┤ vertical + left
        0x252c => (false, true, true, true),  // ┬ down + horizontal
        0x2534 => (true, false, true, true),  // ┴ up + horizontal
        0x253c => (true, true, true, true),   // ┼ cross
        _ => return None,
    };
    Some(BoxArms {
        up,
        down,
        left,
        right,
    })
}

/// Render a light box-drawing cell to an 8x16 single-width bitmap from its arms. The cell
/// crosses at the 8x16 centre — columns 3-4 (`VBAR`) and scanlines 7-8 — so each arm runs
/// from an edge to that centre and adjacent cells' arms meet flush.
fn box_bitmap(arms: &BoxArms) -> [u8; GLYPH_H] {
    const VBAR: u8 = 0b0001_1000; // columns 3-4 (the vertical run)
    const LEFT: u8 = 0b1111_1000; // columns 0-4 (left edge → centre)
    const RIGHT: u8 = 0b0001_1111; // columns 3-7 (centre → right edge)
    let mut g = [0u8; GLYPH_H];
    if arms.up {
        for row in g.iter_mut().take(9) {
            *row |= VBAR; // scanlines 0..=8
        }
    }
    if arms.down {
        for row in g.iter_mut().skip(7) {
            *row |= VBAR; // scanlines 7..=15
        }
    }
    if arms.left {
        g[7] |= LEFT;
        g[8] |= LEFT;
    }
    if arms.right {
        g[7] |= RIGHT;
        g[8] |= RIGHT;
    }
    g
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
unsafe fn blit_wide_glyph<S: Scanout>(
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
        // Flush the 16-pixel span to RAM for the scanout (see [`Scanout::clean_line`]); 16 px is
        // two cache lines, so clean both ends.
        if row_start < len_px {
            unsafe {
                S::clean_line(base.add(row_start) as usize);
                let row_end = row_start + 15;
                if row_end < len_px {
                    S::clean_line(base.add(row_end) as usize);
                }
            }
        }
    }
    S::barrier();
}

/// Switch the Stage-A console to a linear framebuffer and clear it. Called from
/// kmain when BootInfo carries a usable GOP framebuffer.
pub fn set_framebuffer<S: Scanout>(
    base: u64,
    len_bytes: u64,
    width: u32,
    height: u32,
    stride: u32,
) {
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
    FB_KERNEL_OWNER.store(true, ORD);

    // Clear to the black backdrop, then flush to RAM so it shows on real
    // hardware where the framebuffer may be write-back cached (see [`Scanout::clean_line`]).
    let ptr = base as *mut u32;
    let mut i = 0;
    while i < len_px {
        unsafe { ptr.add(i).write_volatile(BG) };
        i += 1;
    }
    unsafe {
        let mut j = 0;
        while j < len_px {
            S::clean_line(ptr.add(j) as usize);
            j += 16; // 64-byte cache line / 4-byte pixel
        }
        S::barrier();
    }

    FB_PRESENT.store(true, ORD);
}

pub fn framebuffer_claim_matches(
    present: bool,
    configured_base: u64,
    configured_len_px: usize,
    phys_base: u64,
    len_bytes: u64,
) -> bool {
    present
        && phys_base == configured_base
        && len_bytes != 0
        && (len_bytes / 4) >= configured_len_px as u64
}

/// Relinquish the Stage-A framebuffer cursor to a userspace console driver. The
/// caller has already checked the driver's Resource capability; this backend check
/// binds that authority to the framebuffer actually configured from BootInfo.
pub fn handoff_framebuffer_console(phys_base: u64, len_bytes: u64) -> bool {
    if !framebuffer_claim_matches(
        FB_PRESENT.load(ORD),
        FB_BASE.load(ORD),
        FB_LEN_PX.load(ORD),
        phys_base,
        len_bytes,
    ) {
        return false;
    }
    FB_KERNEL_OWNER
        .compare_exchange(true, false, ORD, ORD)
        .is_ok()
}

pub fn framebuffer_console_owned_by_kernel() -> bool {
    FB_KERNEL_OWNER.load(ORD)
}

/// Fatal kernel paths take the glass back because userspace can no longer be trusted
/// to render the report. The stale Stage-A cursor is acceptable after the other owner
/// has died; legibility matters, and there is again only one painter.
pub fn reclaim_framebuffer_console() {
    if FB_PRESENT.load(ORD) {
        FB_KERNEL_OWNER.store(true, ORD);
    }
}

pub fn framebuffer_console_writes_enabled(present: bool, kernel_owner: bool) -> bool {
    present && kernel_owner
}

/// Fill the whole linear framebuffer with `color` (little-endian BGRx u32) and flush it
/// to RAM. Used as a bring-up backdrop and a "the kernel is executing and this
/// framebuffer is visible" proof that persists until something repaints over it. Painted
/// before any fallible work, so on a fresh board: a solid colour means entry +
/// relocation + cache coherency + a live framebuffer all work; an unchanged screen (or a
/// reset) means the kernel never got here (or these writes are not reaching the panel).
pub fn fb_fill<S: Scanout>(phys: u64, len_bytes: u64, color: u32) {
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
    unsafe {
        let mut j = 0;
        while j < len_px {
            S::clean_line(base.add(j) as usize);
            j += 16; // 64-byte cache line / 4-byte pixel
        }
        S::barrier();
    }
}

/// Bring-up "POST code on screen": paint a solid 24px band of `color` across the full
/// width at row `y0`, **directly** into a linear BGRx framebuffer described by the raw
/// `BootInfo` fields — bypassing all console/`FB_PRESENT` state — and flush it to RAM.
/// Lets a boot milestone (or the exact point of an early hang) be seen on real hardware
/// before, and independent of, the text console. `color` is a little-endian BGRx u32:
/// blue = `0x0000_00ff`, green = `0x0000_ff00`, red = `0x00ff_0000`.
pub fn fb_paint_band<S: Scanout>(
    phys: u64,
    len_bytes: u64,
    width: u32,
    stride: u32,
    y0: u32,
    color: u32,
) {
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
        // Flush this row to RAM so the display scanout sees it (see [`Scanout::clean_line`]).
        {
            let start = row.min(len_px);
            let stop_px = (row + width).min(len_px);
            let mut a = ((base as usize) + start * 4) & !63;
            let stop = (base as usize) + stop_px * 4;
            while a < stop {
                S::clean_line(a);
                a += 64;
            }
        }
        ry += 1;
    }
    S::barrier();
}

/// Scroll the framebuffer text grid up by one glyph row: move every full glyph band up
/// `GLYPH_H` pixel rows, clear the freed bottom band, and flush the moved region to RAM
/// for the scanout. Replaces the old bottom-row clamp — the POST outgrew small displays
/// (QEMU ramfb is 37 rows), where the tail lines overwrote each other.
/// j882: how many text rows one scroll moves on a console of `rows` rows. A whole-glass
/// scroll is one memmove of the visible framebuffer plus a cache clean over all of it (5.8 MB
/// on the opi5's 1600x900); paid once per new line it made every echoed LogWrite cost
/// 35-170 ms while the kernel owned the glass. Jumping a third of the screen pays it once per
/// ~rows/3 lines instead (CRUX's ruling, 2026-11-11). — TEAL
pub fn jump_rows(rows: usize) -> usize {
    (rows / 3).max(1)
}

/// Scroll the visible console up by `bands` text rows, clearing the rows freed at the bottom.
fn fb_scroll_up<S: Scanout>(
    base: *mut u32,
    len_px: usize,
    stride: usize,
    height_px: usize,
    bands: usize,
) {
    let band = stride * GLYPH_H * bands.max(1); // pixels in the scrolled-away row bands
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
    let mut a = (base as usize) & !63;
    let end = base as usize + visible_px * 4;
    while a < end {
        S::clean_line(a);
        a += 64;
    }
    S::barrier();
}

/// Render one Unicode scalar to the framebuffer. ASCII (and `\n`/`\r`) use the 8x16 PSF
/// cell; any scalar >= `0x80` takes a curated 16x16 **double-width** CJK glyph (or the tofu
/// box if absent), advancing the cursor by two cells.
fn fb_putchar<S: Scanout>(scalar: u32) {
    let stride = FB_STRIDE.load(ORD) as usize;
    let cols = (FB_WIDTH.load(ORD) as usize / GLYPH_W).max(1);
    let rows = (FB_HEIGHT.load(ORD) as usize / GLYPH_H).max(1);
    let mut col = FB_COL.load(ORD) as usize;
    let mut row = FB_ROW.load(ORD) as usize;
    // Address pixels through the console VA offset (identity early, physmap once the
    // split tables are live) so text rendering works under any active TTBR0.
    let base = (FB_BASE.load(ORD) + S::va_offset()) as *mut u32;
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
                let jump = jump_rows(rows);
                fb_scroll_up::<S>(base, len_px, stride, FB_HEIGHT.load(ORD) as usize, jump);
                row = rows - jump;
            }
            unsafe {
                blit_glyph::<S>(
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
        c if box_arms(c).is_some() => {
            // Light box-drawing renders single-width (8x16), like ASCII — TUI frames must
            // align with text, so these do NOT take the double-width CJK cell.
            if col >= cols {
                col = 0;
                row += 1;
            }
            if row >= rows {
                let jump = jump_rows(rows);
                fb_scroll_up::<S>(base, len_px, stride, FB_HEIGHT.load(ORD) as usize, jump);
                row = rows - jump;
            }
            let glyph = box_bitmap(&box_arms(c).unwrap());
            unsafe {
                blit_glyph_rows::<S>(
                    base,
                    len_px,
                    stride,
                    col * GLYPH_W,
                    row * GLYPH_H,
                    &glyph,
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
                let jump = jump_rows(rows);
                fb_scroll_up::<S>(base, len_px, stride, FB_HEIGHT.load(ORD) as usize, jump);
                row = rows - jump;
            }
            let glyph = cjk_glyph(c).unwrap_or(&TOFU[..]);
            unsafe {
                blit_wide_glyph::<S>(
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
        let jump = jump_rows(rows);
        fb_scroll_up::<S>(base, len_px, stride, FB_HEIGHT.load(ORD) as usize, jump);
        row = rows - jump;
    }
    FB_COL.store(col as u32, ORD);
    FB_ROW.store(row as u32, ORD);
}

/// Render `bytes` (UTF-8) on the glass while the kernel owns it. Returns whether the glass
/// took them; the caller writes its serial sinks either way.
pub fn write<S: Scanout>(bytes: &[u8]) -> bool {
    if !framebuffer_console_writes_enabled(FB_PRESENT.load(ORD), FB_KERNEL_OWNER.load(ORD)) {
        return false;
    }
    // The framebuffer is a glyph grid, so decode UTF-8 and look glyphs up per scalar.
    for_each_scalar(bytes, fb_putchar::<S>);
    true
}

/// Whether [`set_framebuffer`] configured a glass (independent of who owns it now).
pub fn present() -> bool {
    FB_PRESENT.load(ORD)
}

/// Move the cursor to an absolute cell. Returns false (and moves nothing) unless the kernel
/// owns a configured glass, so the caller can fall back to its serial line discipline.
pub fn set_cursor(col: u32, row: u32) -> bool {
    if FB_PRESENT.load(ORD) && FB_KERNEL_OWNER.load(ORD) {
        FB_COL.store(col, ORD);
        FB_ROW.store(row, ORD);
        true
    } else {
        false
    }
}

/// Current cursor cell `(col, row)`.
pub fn cursor() -> (u32, u32) {
    (FB_COL.load(ORD), FB_ROW.load(ORD))
}

/// Current framebuffer geometry `(width, height, stride)` in pixels. The TOWER prints this in
/// the (reliable) text banner so even a clipped-QR boot reveals the real panel dimensions —
/// ground truth for the QR placement. Zeros before any framebuffer is set.
pub fn fb_geometry() -> (u32, u32, u32) {
    (FB_WIDTH.load(ORD), FB_HEIGHT.load(ORD), FB_STRIDE.load(ORD))
}

/// The configured glass as a backend's own painters (the arm64 TOWER QR) address it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Surface {
    /// Physical base; add the backend's VA offset to draw.
    pub phys: u64,
    pub len_px: usize,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// The configured glass, if any.
pub fn surface() -> Option<Surface> {
    if !FB_PRESENT.load(ORD) {
        return None;
    }
    Some(Surface {
        phys: FB_BASE.load(ORD),
        len_px: FB_LEN_PX.load(ORD),
        width: FB_WIDTH.load(ORD),
        height: FB_HEIGHT.load(ORD),
        stride: FB_STRIDE.load(ORD),
    })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::{boxed::Box, vec};

    /// The host has no scanout: lines are already visible, nothing to fence.
    struct Host;

    impl Scanout for Host {
        fn clean_line(_addr: usize) {}
        fn barrier() {}
        fn va_offset() -> u64 {
            0
        }
    }

    /// j905: the renderer left kumo-hal-aarch64 pixel-identical. This script (ASCII, light box
    /// glyphs, CJK, wrap, malformed UTF-8, tofu, CR overwrite, jump-scroll, wide-cell wrap,
    /// cursor moves, handoff + reclaim) hashed to this FNV-1a value on the j904 arm64 renderer,
    /// before the lift, on a 160x96 glass with a 168-pixel stride. Any pixel change fails it.
    #[test]
    fn the_lifted_renderer_paints_the_pre_lift_pixels() {
        const W: u32 = 160;
        const H: u32 = 96;
        const STRIDE: u32 = 168;
        let glass: &'static mut [u32] =
            Box::leak(vec![0x1234_5678u32; (STRIDE * H) as usize + 37].into_boxed_slice());
        let base = glass.as_mut_ptr() as u64;
        let len = (glass.len() * 4) as u64;
        set_framebuffer::<Host>(base, len, W, H, STRIDE);
        let script: &[&[u8]] = &[
            b"KUMO glass\n",
            "\u{250c}\u{2500}\u{252c}\u{2510}\n\u{2502} \u{253c} \u{2502}\n".as_bytes(),
            "\u{7d2b}\u{96f2} \u{3042}\u{30ab}\u{300c}\u{ff1a}\n".as_bytes(),
            b"0123456789abcdefghijklmnopqrstuvwxyz wraps\n",
            &[0x80, 0xe7, b'Z', b'\n'],
            "\u{10000}\u{3400}!\n".as_bytes(),
            b"overwrite me\rOVER\n",
            b"1\n2\n3\n4\n5\n6\n7\n",
            b"\x07bell\ttab\x1b[0m\n",
            "\u{7d2b}\u{7d2b}\u{7d2b}\u{7d2b}\u{7d2b}\u{7d2b}\u{7d2b}\u{7d2b}\u{7d2b}\u{7d2b}x"
                .as_bytes(),
        ];
        for chunk in script {
            assert!(write::<Host>(chunk));
        }
        assert!(set_cursor(3, 2));
        write::<Host>(b"@cursor");
        assert!(handoff_framebuffer_console(base, len));
        assert!(!write::<Host>(b"INVISIBLE\n"));
        assert!(!set_cursor(0, 0));
        reclaim_framebuffer_console();
        write::<Host>(b"\nback");
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for px in glass.iter() {
            for b in px.to_le_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
        assert_eq!((h, cursor()), (0x1318_995d_de05_184b, (4, 3)));
        assert_eq!(fb_geometry(), (W, H, STRIDE));
        assert_eq!(
            surface(),
            Some(Surface {
                phys: base,
                len_px: (STRIDE * H) as usize,
                width: W,
                height: H,
                stride: STRIDE,
            })
        );
    }

    #[test]
    fn jump_scroll_moves_a_third_of_the_console() {
        assert_eq!(super::jump_rows(56), 18); // the opi5's 1600x900 at 16 px rows
        assert_eq!(super::jump_rows(3), 1);
        assert_eq!(super::jump_rows(2), 1);
        assert_eq!(super::jump_rows(1), 1);
    }

    #[test]
    fn a_multi_band_scroll_moves_rows_up_and_clears_the_freed_ones() {
        // 2 px wide, 4 text rows: row r holds pixels of value r + 1.
        let stride = 2usize;
        let rows = 4usize;
        let height = rows * super::GLYPH_H;
        let mut glass = [0u32; 2 * 4 * super::GLYPH_H];
        for (i, px) in glass.iter_mut().enumerate() {
            *px = (i / (stride * super::GLYPH_H)) as u32 + 1;
        }
        assert_eq!(glass.len(), stride * height);
        let len = glass.len();
        super::fb_scroll_up::<Host>(glass.as_mut_ptr(), len, stride, height, 2);
        let row = |glass: &[u32], r: usize| glass[r * stride * super::GLYPH_H];
        assert_eq!((row(&glass, 0), row(&glass, 1)), (3, 4));
        assert_eq!((row(&glass, 2), row(&glass, 3)), (super::BG, super::BG));
    }

    #[test]
    fn framebuffer_handoff_requires_the_configured_range() {
        assert!(framebuffer_claim_matches(
            true,
            0x9000_0000,
            0x2000,
            0x9000_0000,
            0x8000,
        ));
        assert!(!framebuffer_claim_matches(
            true,
            0x9000_0000,
            0x2000,
            0x9000_1000,
            0x8000,
        ));
        assert!(!framebuffer_claim_matches(
            false,
            0x9000_0000,
            0x2000,
            0x9000_0000,
            0x8000,
        ));
        assert!(framebuffer_console_writes_enabled(true, true));
        assert!(!framebuffer_console_writes_enabled(true, false));
        assert!(!framebuffer_console_writes_enabled(false, true));
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
        let font = CJK_FONT;
        let rec = CJK_RECORD;
        assert_eq!(font.len() % rec, 0);
        assert_eq!(font.len() / rec, CJK_GLYPH_COUNT);
        // Strictly ascending codepoints, so the binary search is valid.
        let code = |i: usize| u16::from_le_bytes([font[i * rec], font[i * rec + 1]]);
        assert!((1..CJK_GLYPH_COUNT).all(|i| code(i - 1) < code(i)));
        // Broad set: common Han (一/的), a Hangul compatibility jamo (ㄱ), the full precomposed
        // Hangul Syllables block (가 first … 힣 last), and the Japanese kana + CJK punctuation /
        // fullwidth forms the boot banner mixes with kanji (あ/カ/「/：) are all in. Full CJK
        // Unified beyond the newspaper set (㐀, CJK Ext-A) and astral codepoints stay out — that
        // megabyte-class coverage waits for the compressed/BDF font stage.
        assert_eq!(cjk_glyph('一' as u32).map(<[u8]>::len), Some(32));
        assert!(cjk_glyph('的' as u32).is_some());
        assert!(cjk_glyph('ㄱ' as u32).is_some());
        assert!(cjk_glyph('가' as u32).is_some()); // U+AC00, first precomposed syllable
        assert!(cjk_glyph('힣' as u32).is_some()); // U+D7A3, last precomposed syllable
        assert!(cjk_glyph('あ' as u32).is_some()); // U+3042, hiragana (banner)
        assert!(cjk_glyph('カ' as u32).is_some()); // U+30AB, katakana (banner)
        assert!(cjk_glyph('「' as u32).is_some()); // U+300C, CJK punctuation (banner)
        assert!(cjk_glyph('：' as u32).is_some()); // U+FF1A, fullwidth colon (banner)
        assert!(cjk_glyph('㐀' as u32).is_none()); // U+3400, CJK Ext-A, outside the set
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
        unsafe { blit_glyph::<Host>(fb.as_mut_ptr(), fb.len(), STRIDE, 0, 0, b'A', FG, BG) };
        let lit = (0..GLYPH_H)
            .flat_map(|ry| (0..GLYPH_W).map(move |rx| (ry, rx)))
            .filter(|&(ry, rx)| fb[ry * STRIDE + rx] == FG)
            .count();
        assert!(lit > 5, "expected 'A' to light pixels, got {lit}");

        let mut blank = [0xdead_beefu32; STRIDE * 32];
        unsafe { blit_glyph::<Host>(blank.as_mut_ptr(), blank.len(), STRIDE, 0, 0, b' ', FG, BG) };
        assert_eq!(blank[0], BG, "space glyph should clear its cell to bg");
    }

    #[test]
    fn blit_respects_bounds() {
        // A 1-pixel buffer must not be overrun by an 8x16 glyph.
        let mut fb = [0u32; 1];
        unsafe { blit_glyph::<Host>(fb.as_mut_ptr(), fb.len(), 8, 0, 0, b'A', FG, BG) };
        // No panic / no out-of-bounds is the assertion; the one pixel is valid.
        let _ = fb[0];
    }

    #[test]
    fn box_arms_cover_the_light_single_line_set() {
        // The cross has all four arms; horizontal/vertical have exactly two.
        let cross = box_arms(0x253c).unwrap();
        assert!(cross.up && cross.down && cross.left && cross.right);
        let horiz = box_arms(0x2500).unwrap();
        assert!(horiz.left && horiz.right && !horiz.up && !horiz.down);
        let vert = box_arms(0x2502).unwrap();
        assert!(vert.up && vert.down && !vert.left && !vert.right);
        // Out-of-set forms (heavy ━ U+2501, double ═ U+2550) and plain ASCII are NOT claimed,
        // so they fall through unchanged to the CJK/tofu path.
        assert!(box_arms(0x2501).is_none());
        assert!(box_arms(0x2550).is_none());
        assert!(box_arms('A' as u32).is_none());
    }

    #[test]
    fn box_bitmap_draws_only_the_present_arms() {
        // Horizontal ─: the two centre scanlines are lit edge-to-edge, every other row blank.
        let h = box_bitmap(&box_arms(0x2500).unwrap());
        assert_eq!(h[7], 0xff);
        assert_eq!(h[8], 0xff);
        assert!(h
            .iter()
            .enumerate()
            .all(|(r, &b)| r == 7 || r == 8 || b == 0));
        // Vertical │: the centre columns (3-4) are lit on every scanline, nothing else.
        let v = box_bitmap(&box_arms(0x2502).unwrap());
        assert!(v.iter().all(|&b| b == 0b0001_1000));
        // Cross ┼: centre scanlines span full width; centre columns span full height.
        let x = box_bitmap(&box_arms(0x253c).unwrap());
        assert_eq!(x[7], 0xff);
        assert_eq!(x[8], 0xff);
        assert_eq!(x[0], 0b0001_1000);
        assert_eq!(x[15], 0b0001_1000);
    }

    #[test]
    fn box_glyph_blits_within_one_single_width_cell() {
        // A box-drawing cell must paint inside ONE 8-wide column, unlike the double-width CJK
        // path — otherwise TUI frames misalign with text. Render ┼ at column 0 and confirm
        // every lit pixel stays in columns 0..GLYPH_W.
        const STRIDE: usize = 64;
        let mut fb = [0u32; STRIDE * 32];
        let glyph = box_bitmap(&box_arms(0x253c).unwrap());
        unsafe { blit_glyph_rows::<Host>(fb.as_mut_ptr(), fb.len(), STRIDE, 0, 0, &glyph, FG, BG) };
        let lit = (0..GLYPH_H)
            .flat_map(|ry| (0..STRIDE).map(move |rx| (ry, rx)))
            .filter(|&(ry, rx)| fb[ry * STRIDE + rx] == FG)
            .count();
        assert!(lit > 8, "cross should light pixels, got {lit}");
        assert!(
            (0..GLYPH_H).all(|ry| (GLYPH_W..STRIDE).all(|rx| fb[ry * STRIDE + rx] == BG)),
            "box glyph must stay within a single 8px cell"
        );
    }
}
