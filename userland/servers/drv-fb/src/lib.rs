#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j152
//j219
//j344
//j519

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

/// Byte order of the three color channels in one little-endian framebuffer pixel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelOrder {
    Rgb,
    Bgr,
}

impl PixelOrder {
    const fn pixel(self, red: u8, green: u8, blue: u8) -> u32 {
        match self {
            Self::Rgb => red as u32 | (green as u32) << 8 | (blue as u32) << 16,
            Self::Bgr => blue as u32 | (green as u32) << 8 | (red as u32) << 16,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParseState {
    Ground,
    Escape,
    Csi,
}

const ESCAPE: u8 = 0x1b;
const CSI_PARAM_LIMIT: usize = 4;
const CSI_BYTE_LIMIT: u8 = 32;

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
    pixel_order: PixelOrder,
    foreground: u32,
    background: u32,
    reverse: bool,
    parse_state: ParseState,
    csi_params: [u16; CSI_PARAM_LIMIT],
    csi_param_count: usize,
    csi_value: u16,
    csi_has_value: bool,
    csi_private: bool,
    csi_overflow: bool,
    csi_bytes: u8,
    cursor_visible: bool,
    cursor_drawn: bool,
}

impl Console {
    /// Build a console over `width_px` x `height_px` pixels at `stride` pixels per
    /// scanline (0 ⇒ `width_px`), clearing the screen to [`BG`].
    ///
    /// # Safety
    /// `base` must point at a writable framebuffer of at least `stride * height_px`
    /// `u32` pixels and outlive the `Console`.
    pub unsafe fn new(base: *mut u32, width_px: usize, height_px: usize, stride: usize) -> Console {
        unsafe { Self::new_with_pixel_order(base, width_px, height_px, stride, PixelOrder::Bgr) }
    }

    /// Build a console with the framebuffer's declared channel byte order.
    ///
    /// # Safety
    /// `base` must point at a writable framebuffer of at least `stride * height_px`
    /// `u32` pixels and outlive the `Console`.
    pub unsafe fn new_with_pixel_order(
        base: *mut u32,
        width_px: usize,
        height_px: usize,
        stride: usize,
        pixel_order: PixelOrder,
    ) -> Console {
        let stride = if stride == 0 { width_px } else { stride };
        let mut con = Console {
            base,
            len_px: stride.saturating_mul(height_px),
            stride,
            cols: width_px / GLYPH_W,
            rows: height_px / GLYPH_H,
            col: 0,
            row: 0,
            pixel_order,
            foreground: FG,
            background: BG,
            reverse: false,
            parse_state: ParseState::Ground,
            csi_params: [0; CSI_PARAM_LIMIT],
            csi_param_count: 0,
            csi_value: 0,
            csi_has_value: false,
            csi_private: false,
            csi_overflow: false,
            csi_bytes: 0,
            cursor_visible: false,
            cursor_drawn: false,
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
        self.foreground = FG;
        self.background = BG;
        self.reverse = false;
        self.reset_parser();
        self.cursor_visible = false;
        self.cursor_drawn = false;
    }

    /// Feed one byte through the persistent terminal parser. CSI sequences may span
    /// separate calls, matching channel reads that split a repaint arbitrarily.
    pub fn write_byte(&mut self, b: u8) {
        if !self.is_renderable() {
            return;
        }

        self.remove_cursor();
        self.consume_byte(b);
        if self.cursor_visible && self.parse_state == ParseState::Ground {
            self.draw_cursor();
        }
    }

    fn consume_byte(&mut self, b: u8) {
        match self.parse_state {
            ParseState::Ground => {
                if b == ESCAPE {
                    self.parse_state = ParseState::Escape;
                } else {
                    self.render_byte(b);
                }
            }
            ParseState::Escape => match b {
                b'[' => self.begin_csi(),
                ESCAPE => {}
                _ => self.reset_parser(),
            },
            ParseState::Csi => self.consume_csi(b),
        }
    }

    /// Render one non-ANSI byte: printable ASCII draws a glyph and advances the
    /// cursor; newline, carriage return, and backspace move it. Other bytes use
    /// the font's question-mark fallback.
    fn render_byte(&mut self, b: u8) {
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
                        self.effective_foreground(),
                        self.effective_background(),
                    );
                }
                self.col += 1;
            }
        }
    }

    fn begin_csi(&mut self) {
        self.parse_state = ParseState::Csi;
        self.csi_params = [0; CSI_PARAM_LIMIT];
        self.csi_param_count = 0;
        self.csi_value = 0;
        self.csi_has_value = false;
        self.csi_private = false;
        self.csi_overflow = false;
        self.csi_bytes = 0;
    }

    fn reset_parser(&mut self) {
        self.parse_state = ParseState::Ground;
        self.csi_param_count = 0;
        self.csi_value = 0;
        self.csi_has_value = false;
        self.csi_private = false;
        self.csi_overflow = false;
        self.csi_bytes = 0;
    }

    fn consume_csi(&mut self, b: u8) {
        self.csi_bytes = self.csi_bytes.saturating_add(1);
        if self.csi_bytes > CSI_BYTE_LIMIT {
            self.reset_parser();
            return;
        }

        match b {
            ESCAPE => {
                self.reset_parser();
                self.parse_state = ParseState::Escape;
            }
            b'?' if self.csi_bytes == 1 => self.csi_private = true,
            b'0'..=b'9' => {
                self.csi_value = self
                    .csi_value
                    .saturating_mul(10)
                    .saturating_add(u16::from(b - b'0'));
                self.csi_has_value = true;
            }
            b';' => self.push_csi_param(),
            0x40..=0x7e => {
                if self.csi_has_value || self.csi_param_count != 0 {
                    self.push_csi_param();
                }
                self.execute_csi(b);
                self.reset_parser();
            }
            _ => self.reset_parser(),
        }
    }

    fn push_csi_param(&mut self) {
        if self.csi_param_count < CSI_PARAM_LIMIT {
            self.csi_params[self.csi_param_count] = self.csi_value;
            self.csi_param_count += 1;
        } else {
            self.csi_overflow = true;
        }
        self.csi_value = 0;
        self.csi_has_value = false;
    }

    fn execute_csi(&mut self, final_byte: u8) {
        if self.csi_overflow {
            return;
        }
        if self.csi_private {
            if self.csi_param_count == 1 && self.csi_params[0] == 25 {
                match final_byte {
                    b'h' => self.cursor_visible = true,
                    b'l' => self.cursor_visible = false,
                    _ => {}
                }
            }
            return;
        }

        match final_byte {
            b'H' | b'f' => {
                let row = self.csi_one_based(0).saturating_sub(1) as usize;
                let col = self.csi_one_based(1).saturating_sub(1) as usize;
                self.row = row.min(self.rows - 1);
                self.col = col.min(self.cols - 1);
            }
            b'K' => self.erase_line(self.csi_param(0)),
            b'm' => self.apply_sgr(),
            _ => {}
        }
    }

    fn csi_param(&self, index: usize) -> u16 {
        self.csi_params.get(index).copied().unwrap_or(0)
    }

    fn csi_one_based(&self, index: usize) -> u16 {
        match self.csi_param(index) {
            0 => 1,
            value => value,
        }
    }

    fn apply_sgr(&mut self) {
        if self.csi_param_count == 0 {
            self.reset_sgr();
            return;
        }
        for index in 0..self.csi_param_count {
            match self.csi_params[index] {
                0 => self.reset_sgr(),
                7 => self.reverse = true,
                27 => self.reverse = false,
                30..=37 => self.foreground = self.ansi_color(self.csi_params[index] - 30),
                39 => self.foreground = FG,
                40..=47 => self.background = self.ansi_color(self.csi_params[index] - 40),
                49 => self.background = BG,
                _ => {}
            }
        }
    }

    fn reset_sgr(&mut self) {
        self.foreground = FG;
        self.background = BG;
        self.reverse = false;
    }

    fn ansi_color(&self, index: u16) -> u32 {
        let (red, green, blue) = match index {
            0 => (0x00, 0x00, 0x00),
            1 => (0xaa, 0x00, 0x00),
            2 => (0x00, 0xaa, 0x00),
            3 => (0xaa, 0xaa, 0x00),
            4 => (0x00, 0x00, 0xaa),
            5 => (0xaa, 0x00, 0xaa),
            6 => (0x00, 0xaa, 0xaa),
            _ => (0xaa, 0xaa, 0xaa),
        };
        self.pixel_order.pixel(red, green, blue)
    }

    fn effective_foreground(&self) -> u32 {
        if self.reverse {
            self.background
        } else {
            self.foreground
        }
    }

    fn effective_background(&self) -> u32 {
        if self.reverse {
            self.foreground
        } else {
            self.background
        }
    }

    /// Render a run of console bytes.
    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_byte(b);
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let background = self.effective_background();
        let col = self.col.min(self.cols);
        match mode {
            0 => self.fill_cells(self.row, self.row + 1, col, self.cols, background),
            1 => self.fill_cells(
                self.row,
                self.row + 1,
                0,
                col.saturating_add(1).min(self.cols),
                background,
            ),
            2 => self.fill_cells(self.row, self.row + 1, 0, self.cols, background),
            _ => {}
        }
    }

    fn fill_cells(
        &mut self,
        row_start: usize,
        row_end: usize,
        col_start: usize,
        col_end: usize,
        color: u32,
    ) {
        let row_start = row_start.min(self.rows);
        let row_end = row_end.min(self.rows);
        let col_start = col_start.min(self.cols);
        let col_end = col_end.min(self.cols);
        if row_start >= row_end || col_start >= col_end {
            return;
        }

        for cell_row in row_start..row_end {
            for pixel_row in 0..GLYPH_H {
                let y = cell_row * GLYPH_H + pixel_row;
                let start = y * self.stride + col_start * GLYPH_W;
                let end = (y * self.stride + col_end * GLYPH_W).min(self.len_px);
                for index in start.min(self.len_px)..end {
                    unsafe { self.base.add(index).write_volatile(color) };
                }
                #[cfg(target_os = "none")]
                if start < end {
                    unsafe {
                        fb_clean_line(self.base.add(start) as usize);
                        fb_clean_line(self.base.add(end - 1) as usize);
                    }
                }
            }
        }
        #[cfg(target_os = "none")]
        unsafe {
            core::arch::asm!("dsb ish", options(nostack, preserves_flags));
        }
    }

    fn draw_cursor(&mut self) {
        if !self.cursor_drawn && self.col < self.cols && self.row < self.rows {
            self.invert_cursor_cell();
            self.cursor_drawn = true;
        }
    }

    fn remove_cursor(&mut self) {
        if self.cursor_drawn {
            self.invert_cursor_cell();
            self.cursor_drawn = false;
        }
    }

    fn invert_cursor_cell(&mut self) {
        for pixel_row in 0..GLYPH_H {
            let y = self.row * GLYPH_H + pixel_row;
            let start = y * self.stride + self.col * GLYPH_W;
            let end = (start + GLYPH_W).min(self.len_px);
            for index in start..end {
                let pixel = unsafe { self.base.add(index).read_volatile() };
                unsafe { self.base.add(index).write_volatile(pixel ^ 0x00ff_ffff) };
            }
            #[cfg(target_os = "none")]
            if start < end {
                unsafe {
                    fb_clean_line(self.base.add(start) as usize);
                    fb_clean_line(self.base.add(end - 1) as usize);
                }
            }
        }
        #[cfg(target_os = "none")]
        unsafe {
            core::arch::asm!("dsb ish", options(nostack, preserves_flags));
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
        assert_cell_colors(buf, STRIDE, ch, cx, cy, FG, BG);
    }

    fn assert_cell_colors(
        buf: &[u32],
        stride: usize,
        ch: u8,
        cx: usize,
        cy: usize,
        foreground: u32,
        background: u32,
    ) {
        let rows = glyph_rows(ch);
        for ry in 0..GLYPH_H {
            for rx in 0..GLYPH_W {
                let got = buf[(cy * GLYPH_H + ry) * stride + (cx * GLYPH_W + rx)];
                let want = if (rows[ry] >> (7 - rx)) & 1 != 0 {
                    foreground
                } else {
                    background
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

    #[test]
    fn ansi_home_and_erase_line_replace_instead_of_printing_escapes() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        con.write(b"ABC\nDEF");

        con.write(b"\x1b[HXY\x1b[0K");

        assert_cell(&buf, b'X', 0, 0);
        assert_cell(&buf, b'Y', 1, 0);
        assert_cell(&buf, b' ', 2, 0);
        assert_cell(&buf, b'D', 0, 1);
    }

    #[test]
    fn erase_to_end_after_a_full_row_preserves_its_last_cell() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };

        con.write(b"ABC\x1b[0K");

        assert_cell(&buf, b'C', 2, 0);
    }

    #[test]
    fn ansi_cursor_position_survives_split_channel_reads() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };

        con.write(b"\x1b[2;");
        con.write(b"3H");
        con.write(b"Z");

        assert_cell(&buf, b'Z', 2, 1);
        assert_eq!(con.parse_state, ParseState::Ground);
    }

    #[test]
    fn ansi_sgr_renders_kilo_colors_and_reverse_video() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };

        con.write(b"\x1b[31mR\x1b[34mB\x1b[0m\r\n\x1b[7mS\x1b[0m");

        assert_cell_colors(&buf, STRIDE, b'R', 0, 0, 0x00aa_0000, BG);
        assert_cell_colors(&buf, STRIDE, b'B', 1, 0, 0x0000_00aa, BG);
        assert_cell_colors(&buf, STRIDE, b'S', 0, 1, BG, FG);
    }

    #[test]
    fn ansi_palette_respects_rgb_framebuffer_byte_order() {
        let mut buf = fresh();
        let mut con = unsafe {
            Console::new_with_pixel_order(buf.as_mut_ptr(), W, H, STRIDE, PixelOrder::Rgb)
        };

        con.write(b"\x1b[31mR\x1b[34mB");

        assert_cell_colors(&buf, STRIDE, b'R', 0, 0, 0x0000_00aa, BG);
        assert_cell_colors(&buf, STRIDE, b'B', 1, 0, 0x00aa_0000, BG);
    }

    #[test]
    fn kilo_cursor_show_and_hide_is_a_reversible_overlay() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };

        con.write(b"\x1b[2;2H\x1b[?25h");
        for ry in 0..GLYPH_H {
            for rx in 0..GLYPH_W {
                assert_eq!(buf[(GLYPH_H + ry) * STRIDE + GLYPH_W + rx], 0x00ff_ffff);
            }
        }
        assert!(con.cursor_drawn);

        con.write(b"\x1b[?25l");
        assert_cell(&buf, b' ', 1, 1);
        assert!(!con.cursor_drawn);
    }

    #[test]
    fn excessive_csi_parameters_are_swallowed_without_wedging_text() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };

        con.write(b"\x1b[1;2;3;4;5mA");

        assert_cell(&buf, b'A', 0, 0);
        assert_eq!(con.parse_state, ParseState::Ground);
    }

    #[test]
    fn kilo_repaint_transcript_produces_cells_not_escape_glyphs() {
        const KILO_W: usize = 40;
        const KILO_H: usize = 48;
        let mut buf = [BG; KILO_W * KILO_H];
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), KILO_W, KILO_H, KILO_W) };

        for chunk in [
            b"\x1b[?25l\x1b[".as_slice(),
            b"Habc\x1b[39m\x1b[0K\r\n".as_slice(),
            b"\x1b[0K\x1b[7mSTAT \x1b[0m\r\n".as_slice(),
            b"\x1b[0Kok\x1b[1;2".as_slice(),
            b"H\x1b[?25h".as_slice(),
        ] {
            con.write(chunk);
        }
        assert!(con.cursor_drawn);
        con.write(b"\x1b[?25l");

        assert_cell_colors(&buf, KILO_W, b'a', 0, 0, FG, BG);
        assert_cell_colors(&buf, KILO_W, b'b', 1, 0, FG, BG);
        assert_cell_colors(&buf, KILO_W, b'c', 2, 0, FG, BG);
        assert_cell_colors(&buf, KILO_W, b' ', 4, 0, FG, BG);
        assert_cell_colors(&buf, KILO_W, b'S', 0, 1, BG, FG);
        assert_cell_colors(&buf, KILO_W, b'T', 1, 1, BG, FG);
        assert_cell_colors(&buf, KILO_W, b'o', 0, 2, FG, BG);
        assert_cell_colors(&buf, KILO_W, b'k', 1, 2, FG, BG);
    }
}
