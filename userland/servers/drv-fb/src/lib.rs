#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

//! `drv-fb` rendering core — a pure, host-tested 8x16 text console over a 32-bpp
//! linear framebuffer.
//!
//! Per `PLAN §9` (server logic is arch-neutral and host-testable before it ever runs
//! on metal) the load-bearing content lives here, not in the bin: a [`Console`] that
//! turns console-channel bytes into pixels, exercised by `cargo test -p drv-fb` against
//! an in-memory buffer. The thin `kumo-rt` bootstrap (mapping the framebuffer VMO,
//! pumping the channel) is the only part that needs the image pipeline.
//!
//! The glyph blitter is the Stage-A kernel console's (`kumo-hal-aarch64`), ported to
//! userspace. The framebuffer driver — not the kernel — now owns the glass (`DESIGN/007`,
//! the Redox posture of `PLAN_IV`).

/// Embedded 8x16 console font (PSF2). Kept in-tree so the driver stays self-contained,
/// mirroring the Stage-A kernel console. (Font *staging* via initrd/VMO — `DESIGN/005` —
/// is a deferred refinement; embedding is the smallest provable step.)
const FONT: &[u8] = include_bytes!("../font8x16.psf");

/// Glyph cell dimensions of the embedded PSF2 font.
pub const GLYPH_W: usize = 8;
pub const GLYPH_H: usize = 16;

/// Phosphor green on black. Green is the middle byte, so this single constant is the
/// same pixel under both RGB888x and BGR888x — the renderer never needs to know the
/// framebuffer's channel order (identical reasoning to the Stage-A console).
pub const FG: u32 = 0x0000_ff00;
pub const BG: u32 = 0x0000_0000;

/// Read a little-endian `u32` PSF2 header field at `offset`.
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
/// controller scans the framebuffer from RAM; on metal, CPU pixel writes can sit in the
/// D-cache where the scanout never sees them (a blank screen even though the driver is
/// running). No-op on the host; harmless on QEMU and on WC/Device buffers.
#[cfg(target_os = "none")]
#[inline]
unsafe fn fb_clean_line(addr: usize) {
    unsafe {
        core::arch::asm!("dc cvac, {a}", a = in(reg) addr, options(nostack, preserves_flags))
    };
}

/// Blit one glyph into a 32-bpp framebuffer. Bounds-checked against `len_px`, so a bad
/// geometry truncates instead of scribbling past the buffer.
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
        // Flush this row's pixels to RAM so the display actually shows them. The 8-pixel
        // span is one cache line; clean both ends in case it straddles a line boundary.
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

/// An 8x16 text console over a 32-bpp linear framebuffer. Holds only cursor state; the
/// pixels live in the caller's framebuffer mapping. Recoverable by construction — the
/// console grid is derivable, never critical private RAM (`DESIGN/002`).
pub struct Console {
    base: *mut u32,
    len_px: usize,
    stride: usize,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
}

impl Console {
    /// Build a console over `width_px` x `height_px` pixels at `stride` pixels per
    /// scanline (0 ⇒ `width_px`), clearing the screen to [`BG`].
    ///
    /// # Safety
    /// `base` must point at a writable framebuffer of at least `stride * height_px`
    /// `u32` pixels and outlive the `Console`.
    pub unsafe fn new(base: *mut u32, width_px: usize, height_px: usize, stride: usize) -> Console {
        let stride = if stride == 0 { width_px } else { stride };
        let mut con = Console {
            base,
            len_px: stride.saturating_mul(height_px),
            stride,
            cols: width_px / GLYPH_W,
            rows: height_px / GLYPH_H,
            col: 0,
            row: 0,
        };
        con.clear();
        con
    }

    /// True when this console can actually paint: a non-null base over a non-empty extent
    /// with a usable text grid. A console that fails this must never touch `base` — every
    /// write path (`clear`, `write_byte`) bails early, so a null/implausible framebuffer
    /// (e.g. garbage BootInfo geometry, see the `is_plausible` doctrine) raises no fault and
    /// leaves the HAL console live (DESIGN/002: never hold critical state you cannot paint).
    fn is_renderable(&self) -> bool {
        !self.base.is_null() && self.len_px > 0 && self.cols > 0 && self.rows > 0
    }

    /// Fill the whole framebuffer with [`BG`].
    pub fn clear(&mut self) {
        if !self.is_renderable() {
            return;
        }
        for idx in 0..self.len_px {
            unsafe { self.base.add(idx).write_volatile(BG) };
        }
        #[cfg(target_os = "none")]
        unsafe {
            let mut j = 0;
            while j < self.len_px {
                fb_clean_line(self.base.add(j) as usize);
                j += 16; // 64-byte cache line / 4-byte pixel
            }
            core::arch::asm!("dsb ish", options(nostack, preserves_flags));
        }
        self.col = 0;
        self.row = 0;
    }

    /// Render one console byte: printable ASCII draws a glyph and advances the cursor;
    /// `\n`, `\r`, and backspace move it; anything else draws the font's `?` fallback. Wraps at the
    /// right edge and scrolls at the bottom.
    pub fn write_byte(&mut self, b: u8) {
        // A null/implausible framebuffer must never be dereferenced — `newline`→`scroll`
        // touches `base` just as the glyph path does, so gate the whole byte here.
        if !self.is_renderable() {
            return;
        }
        match b {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            0x08 => {
                if self.col > 0 {
                    self.col -= 1;
                }
            }
            _ => {
                if self.col >= self.cols {
                    self.newline();
                }
                unsafe {
                    blit_glyph(
                        self.base,
                        self.len_px,
                        self.stride,
                        self.col * GLYPH_W,
                        self.row * GLYPH_H,
                        b,
                        FG,
                        BG,
                    );
                }
                self.col += 1;
            }
        }
    }

    /// Render a run of console bytes.
    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_byte(b);
        }
    }

    /// Advance to the start of the next line, scrolling when the cursor falls off the
    /// bottom so the newest line is always the last visible row.
    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 >= self.rows {
            self.scroll();
        } else {
            self.row += 1;
        }
    }

    /// Scroll the text grid up by one glyph row: move every full glyph band up
    /// `GLYPH_H` pixel rows and clear the freed bottom band. Leaves the cursor on the
    /// (now blank) last row.
    fn scroll(&mut self) {
        let shift = GLYPH_H * self.stride;
        let visible = self.rows * GLYPH_H * self.stride;
        for idx in 0..visible {
            let src = idx + shift;
            let v = if src < self.len_px {
                unsafe { self.base.add(src).read_volatile() }
            } else {
                BG
            };
            if idx < self.len_px {
                unsafe { self.base.add(idx).write_volatile(v) };
            }
        }
        // Clear the freed bottom band.
        let bottom = (self.rows - 1) * GLYPH_H * self.stride;
        for idx in bottom..visible {
            if idx < self.len_px {
                unsafe { self.base.add(idx).write_volatile(BG) };
            }
        }
        #[cfg(target_os = "none")]
        unsafe {
            let mut j = 0;
            while j < visible.min(self.len_px) {
                fb_clean_line(self.base.add(j) as usize);
                j += 16;
            }
            core::arch::asm!("dsb ish", options(nostack, preserves_flags));
        }
        self.col = 0;
        self.row = self.rows - 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small in-memory framebuffer: 24x32 px (3 cols x 2 rows of 8x16 cells).
    const W: usize = 24;
    const H: usize = 32;
    const STRIDE: usize = W;

    fn fresh() -> [u32; W * H] {
        [BG; W * H]
    }

    /// Assert that glyph-cell `(cx, cy)` holds exactly the bitmap for `ch`.
    fn assert_cell(buf: &[u32], ch: u8, cx: usize, cy: usize) {
        let rows = glyph_rows(ch);
        for ry in 0..GLYPH_H {
            for rx in 0..GLYPH_W {
                let got = buf[(cy * GLYPH_H + ry) * STRIDE + (cx * GLYPH_W + rx)];
                let want = if (rows[ry] >> (7 - rx)) & 1 != 0 {
                    FG
                } else {
                    BG
                };
                assert_eq!(
                    got, want,
                    "pixel mismatch for {:?} at cell ({cx},{cy}) sub ({rx},{ry})",
                    ch as char
                );
            }
        }
    }

    #[test]
    fn new_clears_to_background() {
        let mut buf = fresh();
        buf[0] = 0xDEAD_BEEF;
        let _con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        assert!(buf.iter().all(|&p| p == BG));
    }

    #[test]
    fn renders_one_glyph_exactly() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        con.write(b"A");
        assert_cell(&buf, b'A', 0, 0);
        assert_cell(&buf, b' ', 1, 0);
    }

    #[test]
    fn newline_moves_to_next_row_column_zero() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        con.write(b"A\nB");
        assert_cell(&buf, b'A', 0, 0);
        assert_cell(&buf, b'B', 0, 1);
    }

    #[test]
    fn carriage_return_overwrites_from_column_zero() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        con.write(b"A\rB");
        assert_cell(&buf, b'B', 0, 0);
    }

    #[test]
    fn backspace_moves_left_so_space_can_erase() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        con.write(b"AB\x08 \x08C");
        assert_cell(&buf, b'A', 0, 0);
        assert_cell(&buf, b'C', 1, 0);
        assert_cell(&buf, b' ', 2, 0);
    }

    #[test]
    fn wraps_at_right_edge() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        con.write(b"123W");
        assert_cell(&buf, b'1', 0, 0);
        assert_cell(&buf, b'2', 1, 0);
        assert_cell(&buf, b'3', 2, 0);
        assert_cell(&buf, b'W', 0, 1);
    }

    #[test]
    fn non_printable_renders_tofu_question_mark() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        con.write(&[0x01]);
        assert_cell(&buf, b'?', 0, 0);
    }

    #[test]
    fn scrolls_when_cursor_falls_off_the_bottom() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        con.write(b"X\nY\n");
        assert_cell(&buf, b'Y', 0, 0);
        assert_cell(&buf, b' ', 0, 1);
        con.write(b"Z");
        assert_cell(&buf, b'Z', 0, 1);
    }
}
