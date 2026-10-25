#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
//j568
//j597
//j598
//j599
//j600

//! `drv-fb` rendering core — a pure, host-tested UTF-8 console over a 32-bpp
//! linear framebuffer. ASCII and box drawing occupy 8x16 cells; CJK occupies 16x16.
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

pub mod dtb_topology;
mod font;
pub mod rk3588;
pub mod rk3588_clock;
pub mod rk3588_hdmi;

#[cfg(test)]
use font::ascii_rows as glyph_rows;

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

#[derive(Default)]
struct Utf8Decoder {
    value: u32,
    minimum: u32,
    remaining: u8,
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
    (base, shadow): (*mut u32, *mut u32),
    len_px: usize,
    stride: usize,
    position: (usize, usize),
    glyph: &font::Glyph,
    width: usize,
    colors: (u32, u32),
) {
    let (x_px, y_px) = position;
    let (fg, bg) = colors;
    for (ry, &bits) in glyph.rows.iter().enumerate() {
        let py = y_px + ry;
        let row_start = py.wrapping_mul(stride).wrapping_add(x_px);
        for rx in 0..width {
            let on = (bits >> (15 - rx)) & 1 != 0;
            let idx = row_start.wrapping_add(rx);
            if idx < len_px {
                let color = if on { fg } else { bg };
                unsafe {
                    shadow.add(idx).write(color);
                    base.add(idx).write_volatile(color);
                }
            }
        }
        // Clean both endpoints, including a wide glyph's second cache line. — KESTREL
        #[cfg(target_os = "none")]
        if row_start < len_px {
            unsafe {
                fb_clean_line(base.add(row_start) as usize);
                let row_end = row_start + width - 1;
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

/// An 8x16 text grid over a 32-bpp linear framebuffer. Holds cursor/parser state; the
/// pixels live in the caller's framebuffer mapping. Recoverable by construction — the
/// console grid is derivable, never critical private RAM (`DESIGN/002`).
pub struct Console {
    base: *mut u32,
    /// A copy of every pixel in ordinary cached memory, which every draw writes beside `base`
    /// and every read uses instead (J554). The glass is mapped non-cacheable, and a read there
    /// is a full memory round trip per pixel: scrolling a 1600x900 screen by reading it back
    /// took about 165 ms a line on the opi5. Equal to `base` when no shadow was given.
    shadow: *mut u32,
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
    utf8: Utf8Decoder,
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
        unsafe { Self::new_shadowed(base, base, width_px, height_px, stride, pixel_order) }
    }

    /// Build a console that draws to `base` and keeps a copy of every pixel at `shadow`, so
    /// scrolling and the cursor never read the framebuffer back (J554).
    ///
    /// # Safety
    /// `base` and `shadow` must each point at a writable buffer of at least
    /// `stride * height_px` `u32` pixels, outlive the `Console`, and either be the same buffer
    /// or not overlap.
    pub unsafe fn new_shadowed(
        base: *mut u32,
        shadow: *mut u32,
        width_px: usize,
        height_px: usize,
        stride: usize,
        pixel_order: PixelOrder,
    ) -> Console {
        let stride = if stride == 0 { width_px } else { stride };
        let mut con = Console {
            base,
            shadow,
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
            utf8: Utf8Decoder::default(),
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
        self.fill_display(BG);
        self.col = 0;
        self.row = 0;
        self.foreground = FG;
        self.background = BG;
        self.reverse = false;
        self.reset_parser();
        self.utf8 = Utf8Decoder::default();
        self.cursor_visible = false;
        self.cursor_drawn = false;
    }

    // Full-display erase includes scanlines outside the last complete text row
    // (900px is 56 rows plus four pixels), without changing terminal state. — KESTREL
    fn fill_display(&mut self, color: u32) {
        for idx in 0..self.len_px {
            unsafe { self.put(idx, color) };
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
    }

    /// Feed one byte through the persistent terminal parser. UTF-8 and CSI sequences
    /// may span separate calls, matching arbitrarily split console-channel reads.
    pub fn write_byte(&mut self, b: u8) {
        if !self.is_renderable() {
            return;
        }

        self.remove_cursor();
        self.consume_byte(b);
        if self.cursor_visible && self.parse_state == ParseState::Ground && self.utf8.remaining == 0
        {
            self.draw_cursor();
        }
    }

    fn consume_byte(&mut self, b: u8) {
        match self.parse_state {
            ParseState::Ground => self.consume_utf8(b),
            ParseState::Escape => match b {
                b'[' => self.begin_csi(),
                ESCAPE => {}
                _ => self.reset_parser(),
            },
            ParseState::Csi => self.consume_csi(b),
        }
    }

    fn consume_utf8(&mut self, b: u8) {
        if self.utf8.remaining != 0 {
            if b & 0xc0 == 0x80 {
                self.utf8.value = (self.utf8.value << 6) | u32::from(b & 0x3f);
                self.utf8.remaining -= 1;
                if self.utf8.remaining == 0 {
                    let scalar = self.utf8.value;
                    let valid = scalar >= self.utf8.minimum && char::from_u32(scalar).is_some();
                    self.render_scalar(if valid { scalar } else { 0xfffd });
                }
                return;
            }
            self.utf8 = Utf8Decoder::default();
            self.render_scalar(0xfffd);
            // Reprocess a non-continuation byte, including ESC and newlines:
            // malformed text must not swallow the next terminal control. — KESTREL
        }
        let (remaining, value, minimum) = match b {
            ESCAPE => {
                self.parse_state = ParseState::Escape;
                return;
            }
            0..=0x7f => {
                self.render_scalar(u32::from(b));
                return;
            }
            0xc2..=0xdf => (1, u32::from(b & 0x1f), 0x80),
            0xe0..=0xef => (2, u32::from(b & 0x0f), 0x800),
            0xf0..=0xf4 => (3, u32::from(b & 0x07), 0x10000),
            _ => {
                self.render_scalar(0xfffd);
                return;
            }
        };
        self.utf8 = Utf8Decoder {
            value,
            minimum,
            remaining,
        };
    }

    fn render_scalar(&mut self, scalar: u32) {
        match scalar {
            0x0a => self.newline(),
            0x0d => self.col = 0,
            0x08 => {
                if self.col > 0 {
                    self.col -= 1;
                }
            }
            _ => {
                let glyph = font::glyph(scalar);
                // A one-column framebuffer clips a wide glyph to its visible
                // half instead of writing through stride padding. — KESTREL
                let columns = glyph.columns.min(self.cols);
                if self.col + columns > self.cols {
                    self.newline();
                }
                unsafe {
                    blit_glyph(
                        (self.base, self.shadow),
                        self.len_px,
                        self.stride,
                        (self.col * GLYPH_W, self.row * GLYPH_H),
                        &glyph,
                        columns * GLYPH_W,
                        (self.effective_foreground(), self.effective_background()),
                    );
                }
                self.col += columns;
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
            b'J' => self.erase_display(self.csi_param(0)),
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
        let mut index = 0;
        while index < self.csi_param_count {
            match self.csi_params[index] {
                0 => self.reset_sgr(),
                7 => self.reverse = true,
                27 => self.reverse = false,
                30..=37 => self.foreground = self.ansi_color(self.csi_params[index] - 30),
                39 => self.foreground = FG,
                40..=47 => self.background = self.ansi_color(self.csi_params[index] - 40),
                49 => self.background = BG,
                // Extended colours (`38;5;N`, `38;2;R;G;B`, and 48/58 alike) are not drawn on
                // this console, but their sub-parameters must not be read as attributes:
                // `38;5;7` is not reverse video, and `38;5;0` is not a reset (J565).
                38 | 48 | 58 if index + 1 < self.csi_param_count => {
                    index += match self.csi_params[index + 1] {
                        5 => 2,
                        2 => 4,
                        _ => 0,
                    };
                }
                _ => {}
            }
            index += 1;
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

    /// Erase in display (CSI `J`), what `clear` sends (J553): `0` from the cursor to the end of
    /// the screen, `1` from the start of the screen through the cursor, `2` all of it. `3` asks
    /// a terminal to drop its saved lines; this console keeps none, so the screen is left alone,
    /// as xterm leaves it. Like `K`, the cursor stays where it is and erased cells take the
    /// current background.
    ///
    /// Without this the kernel shell's `clear` (`ESC[2J ESC[H`) only homed the cursor, and the
    /// old text stayed on the glass under the prompt. — TIMBERDOODLE
    fn erase_display(&mut self, mode: u16) {
        let background = self.effective_background();
        let col = self.col.min(self.cols);
        match mode {
            0 => {
                self.fill_cells(self.row, self.row + 1, col, self.cols, background);
                self.fill_cells(self.row + 1, self.rows, 0, self.cols, background);
            }
            1 => {
                self.fill_cells(0, self.row, 0, self.cols, background);
                self.fill_cells(
                    self.row,
                    self.row + 1,
                    0,
                    col.saturating_add(1).min(self.cols),
                    background,
                );
            }
            2 => self.fill_display(background),
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
                    unsafe { self.put(index, color) };
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
                let pixel = unsafe { self.shadow.add(index).read() };
                unsafe { self.put(index, pixel ^ 0x00ff_ffff) };
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
    /// Copy the first `len` pixels of the shadow to the glass, two pixels per store where both
    /// buffers allow it. A no-op when there is no separate shadow.
    ///
    /// # Safety
    /// `len` must be at most `len_px`.
    unsafe fn flush_shadow(&self, len: usize) {
        if self.shadow == self.base {
            return;
        }
        let paired =
            (self.base as usize).is_multiple_of(8) && (self.shadow as usize).is_multiple_of(8);
        let mut idx = 0;
        if paired {
            let (glass, copy) = (self.base as *mut u64, self.shadow as *const u64);
            while idx + 1 < len {
                unsafe { glass.add(idx / 2).write_volatile(copy.add(idx / 2).read()) };
                idx += 2;
            }
        }
        while idx < len {
            unsafe {
                self.base
                    .add(idx)
                    .write_volatile(self.shadow.add(idx).read())
            };
            idx += 1;
        }
    }

    /// Write one pixel to the shadow and the glass.
    ///
    /// # Safety
    /// `index` must be below `len_px`.
    unsafe fn put(&self, index: usize, color: u32) {
        unsafe {
            self.shadow.add(index).write(color);
            self.base.add(index).write_volatile(color);
        }
    }

    fn scroll(&mut self) {
        let shift = GLYPH_H * self.stride;
        let visible = (self.rows * GLYPH_H * self.stride).min(self.len_px);
        let kept = visible.saturating_sub(shift);
        // Move the picture up one text row in the cached shadow, blank the freed bottom row
        // there, then copy the result out. The glass is only ever written. — TIMBERDOODLE
        unsafe {
            core::ptr::copy(self.shadow.add(shift), self.shadow, kept);
            for idx in kept..visible {
                self.shadow.add(idx).write(BG);
            }
            self.flush_shadow(visible);
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

/// Paint the server-owned band: background, a chrome frame per view, and each view's well.
/// `write` receives absolute framebuffer indices (`row * stride + x`); callers write through
/// volatile pointers because this is device memory. Client pixels never reach the chrome.
pub fn paint_band<F: FnMut(usize, u32)>(
    band: &kumo_graphics::Band,
    width: usize,
    stride: usize,
    order: PixelOrder,
    mut write: F,
) {
    let rgb = |pixel: u32| order.pixel((pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8);
    let background = rgb(kumo_graphics::BAND_BG);
    for y in 0..band.height as usize {
        for x in 0..width {
            write(y * stride + x, background);
        }
    }
    let chrome = rgb(kumo_graphics::VIEW_CHROME_COLOR);
    for view in &band.views {
        let left = view.x as usize - kumo_graphics::VIEW_CHROME as usize;
        let top = view.y as usize - kumo_graphics::VIEW_CHROME as usize;
        let width = view.width as usize + 2 * kumo_graphics::VIEW_CHROME as usize;
        let height = view.height as usize + 2 * kumo_graphics::VIEW_CHROME as usize;
        for y in 0..height {
            for x in 0..width {
                write((top + y) * stride + left + x, chrome);
            }
        }
    }
    let well = rgb(kumo_graphics::VIEW_BG);
    for view in &band.views {
        for y in 0..view.height as usize {
            for x in 0..view.width as usize {
                write((view.y as usize + y) * stride + view.x as usize + x, well);
            }
        }
    }
}

/// Present one client surface into its server-owned view. The server copies only the
/// intersection of the surface and the view: `read_pixel` is never called outside the view,
/// and `write_pixel` never touches chrome or the other view. A surface larger than its view
/// is clipped, not rejected. Invalid geometry is rejected before any read or write.
/// `read_pixel` receives an index inside the validated backing extent; shared mappings
/// should read volatile, never through a Rust reference.
pub fn present_view<R: FnMut(usize) -> u32, W: FnMut(usize, u32)>(
    view: &kumo_graphics::View,
    stride: usize,
    surface: kumo_graphics::Surface,
    backing_bytes: usize,
    order: PixelOrder,
    mut read_pixel: R,
    mut write_pixel: W,
) -> Result<(), kumo_graphics::Status> {
    if !surface.fits(backing_bytes) || view.width == 0 || view.height == 0 {
        return Err(kumo_graphics::Status::BadSurface);
    }
    let well = order.pixel(0, 0, 0);
    for y in 0..view.height as usize {
        for x in 0..view.width as usize {
            let color = if x < surface.width as usize && y < surface.height as usize {
                let pixel = read_pixel(y * surface.stride as usize + x);
                order.pixel((pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8)
            } else {
                well
            };
            write_pixel((view.y as usize + y) * stride + view.x as usize + x, color);
        }
    }
    Ok(())
}

/// Clean the cache lines covering `start..start + len` pixels, then DSB. No-op off target.
///
/// # Safety
/// `base` must point at a framebuffer mapping covering `start..start + len` pixels, and the
/// caller must own that mapping for the duration of the call.
#[cfg(not(target_os = "none"))]
pub unsafe fn clean_pixels(_base: *mut u32, _start: usize, _len: usize) {}

/// Clean the cache lines covering `start..start + len` pixels, then DSB.
///
/// # Safety
/// `base` must point at a framebuffer mapping covering `start..start + len` pixels, and the
/// caller must own that mapping for the duration of the call.
#[cfg(target_os = "none")]
pub unsafe fn clean_pixels(base: *mut u32, start: usize, len: usize) {
    unsafe {
        let mut offset = 0;
        while offset < len {
            fb_clean_line(base.add(start + offset) as usize);
            offset += 16;
        }
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
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

    #[test]
    fn band_paint_draws_background_chrome_and_view_wells() {
        let layout = kumo_graphics::Band {
            height: 64,
            views: [
                kumo_graphics::View {
                    x: 8,
                    y: 4,
                    width: 32,
                    height: 24,
                },
                kumo_graphics::View {
                    x: 56,
                    y: 4,
                    width: 32,
                    height: 24,
                },
            ],
        };
        let width = 96;
        const STRIDE: usize = 100;
        let mut glass = [0xf00d_u32; STRIDE * 64 + 1];
        for order in [PixelOrder::Rgb, PixelOrder::Bgr] {
            glass.fill(0xf00d);
            paint_band(&layout, width, STRIDE, order, |index, color| {
                glass[index] = color
            });
            let rgb =
                |pixel: u32| order.pixel((pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8);
            assert_eq!(glass[0], rgb(kumo_graphics::BAND_BG));
            assert_eq!(glass[2 * STRIDE + 6], rgb(kumo_graphics::VIEW_CHROME_COLOR));
            assert_eq!(glass[4 * STRIDE + 8], rgb(kumo_graphics::VIEW_BG));
            assert_eq!(glass[4 * STRIDE + 96], 0xf00d);
            assert_eq!(glass[STRIDE * 64], 0xf00d);
        }
    }

    #[test]
    fn present_clips_to_the_view_and_never_touches_guards_or_chrome() {
        let view = kumo_graphics::View {
            x: 8,
            y: 4,
            width: 8,
            height: 4,
        };
        const STRIDE: usize = 20;
        let mut glass = [0xdead_beef_u32; STRIDE * 16];
        let surface = kumo_graphics::Surface {
            width: 12,
            height: 6,
            stride: 12,
            format: kumo_graphics::XRGB8888,
        };
        let mut source = [0u32; 12 * 6];
        for y in 0..6 {
            for x in 0..12 {
                source[y * 12 + x] = if x < 8 && y < 4 {
                    0x0012_3456
                } else {
                    kumo_graphics::CLIP_POISON
                };
            }
        }
        let mut reads = 0;
        present_view(
            &view,
            STRIDE,
            surface,
            source.len() * 4,
            PixelOrder::Rgb,
            |index| {
                reads += 1;
                source[index]
            },
            |index, color| glass[index] = color,
        )
        .unwrap();
        assert_eq!(reads, 8 * 4);
        let expected = PixelOrder::Rgb.pixel(0x12, 0x34, 0x56);
        for y in 0..4 {
            for x in 0..8 {
                assert_eq!(glass[(4 + y) * STRIDE + 8 + x], expected);
            }
        }
        assert_eq!(glass[(4 + 4) * STRIDE + 8], 0xdead_beef);
        assert_eq!(glass[4 * STRIDE + 16], 0xdead_beef);
        assert_eq!(glass[3 * STRIDE + 8], 0xdead_beef);
        assert_eq!(glass[4 * STRIDE + 7], 0xdead_beef);
    }

    #[test]
    fn invalid_surfaces_never_read_or_write() {
        let view = kumo_graphics::View {
            x: 2,
            y: 2,
            width: 8,
            height: 8,
        };
        let mut glass = [0xdead_beef_u32; 16 * 16];
        let before = glass;
        let valid = kumo_graphics::Surface {
            width: 8,
            height: 8,
            stride: 8,
            format: kumo_graphics::XRGB8888,
        };
        for (surface, backing) in [
            (valid, 8 * 8 * 4 - 1),
            (kumo_graphics::Surface { format: 9, ..valid }, 4096),
            (kumo_graphics::Surface { width: 0, ..valid }, 4096),
            (kumo_graphics::Surface { stride: 1, ..valid }, 4096),
        ] {
            let mut reads = 0;
            assert_eq!(
                present_view(
                    &view,
                    16,
                    surface,
                    backing,
                    PixelOrder::Rgb,
                    |_| {
                        reads += 1;
                        0
                    },
                    |index, color| glass[index] = color,
                ),
                Err(kumo_graphics::Status::BadSurface)
            );
            assert_eq!(reads, 0);
            assert_eq!(glass, before);
        }
    }

    #[test]
    fn two_views_stay_independent() {
        let views = [
            kumo_graphics::View {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            kumo_graphics::View {
                x: 8,
                y: 0,
                width: 4,
                height: 4,
            },
        ];
        const STRIDE: usize = 16;
        let mut glass = [0xdead_beef_u32; STRIDE * 16];
        let surface = kumo_graphics::Surface {
            width: 4,
            height: 4,
            stride: 4,
            format: kumo_graphics::XRGB8888,
        };
        let red = [0x00ff_0000_u32; 16];
        present_view(
            &views[0],
            STRIDE,
            surface,
            red.len() * 4,
            PixelOrder::Rgb,
            |index| red[index],
            |index, color| glass[index] = color,
        )
        .unwrap();
        assert_eq!(glass[8], 0xdead_beef);
        let blue = [0x0000_00ff_u32; 16];
        present_view(
            &views[1],
            STRIDE,
            surface,
            blue.len() * 4,
            PixelOrder::Rgb,
            |index| blue[index],
            |index, color| glass[index] = color,
        )
        .unwrap();
        assert_eq!(glass[0], PixelOrder::Rgb.pixel(0xff, 0, 0));
        assert_eq!(glass[8], PixelOrder::Rgb.pixel(0, 0, 0xff));
    }

    #[test]
    fn console_text_stays_below_the_band() {
        const WIDTH: usize = 32;
        const HEIGHT: usize = 64;
        const BAND: usize = 16;
        let mut glass = [0xdead_beef_u32; WIDTH * HEIGHT + 1];
        let mut shadow = [0u32; WIDTH * HEIGHT];
        let mut con = unsafe {
            Console::new_shadowed(
                glass.as_mut_ptr().add(BAND * WIDTH),
                shadow.as_mut_ptr().add(BAND * WIDTH),
                WIDTH,
                HEIGHT - BAND,
                WIDTH,
                PixelOrder::Bgr,
            )
        };
        for _ in 0..12 {
            con.write(b"A\r\nB\r\nC\r\nD\r\nE\r\n");
        }
        assert!(glass[..BAND * WIDTH]
            .iter()
            .all(|pixel| *pixel == 0xdead_beef));
        assert_eq!(glass[WIDTH * HEIGHT], 0xdead_beef);
        assert!(glass[BAND * WIDTH..].contains(&FG));
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

    fn assert_wide_cell(buf: &[u32], stride: usize, rows: &[u16; 16], cx: usize, cy: usize) {
        for (y, &bits) in rows.iter().enumerate() {
            for x in 0..16 {
                let want = if bits & (1 << (15 - x)) != 0 { FG } else { BG };
                assert_eq!(buf[(cy * GLYPH_H + y) * stride + cx * GLYPH_W + x], want);
            }
        }
    }

    #[test]
    fn japanese_bitmap_and_mixed_cell_advances_are_exact() {
        const WIDTH: usize = 48;
        let mut buf = [BG; WIDTH * GLYPH_H];
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), WIDTH, GLYPH_H, WIDTH) };
        con.write("AあB".as_bytes());

        // Golden U+3042 rows from the existing Unifont asset, independent of
        // the new lookup and width-aware blitter. — KESTREL
        const HIRAGANA_A: [u16; 16] = [
            0x0000, 0x0400, 0x0200, 0x02c0, 0x1f00, 0x0480, 0x0480, 0x07e0, 0x0d10, 0x1508, 0x2208,
            0x2608, 0x2a10, 0x1060, 0x0000, 0x0000,
        ];
        assert_cell_colors(&buf, WIDTH, b'A', 0, 0, FG, BG);
        assert_wide_cell(&buf, WIDTH, &HIRAGANA_A, 1, 0);
        assert_cell_colors(&buf, WIDTH, b'B', 3, 0, FG, BG);
        assert_eq!(con.col, 4);
    }

    #[test]
    fn japanese_and_ansi_repaint_survives_every_read_boundary() {
        const WIDTH: usize = 64;
        const HEIGHT: usize = 48;
        let bytes =
            "\x1b[?25l\x1b[H┏━━━━━━┓\r\n┃\x1b[31m日本語\x1b[0m┃\r\n┗━━━━━━┛\x1b[2;2H\x1b[?25h"
                .as_bytes();
        let mut expected = [BG; WIDTH * HEIGHT];
        let mut reference = unsafe { Console::new(expected.as_mut_ptr(), WIDTH, HEIGHT, WIDTH) };
        reference.write(bytes);
        for split in 0..=bytes.len() {
            let mut buf = [BG; WIDTH * HEIGHT];
            let mut con = unsafe { Console::new(buf.as_mut_ptr(), WIDTH, HEIGHT, WIDTH) };
            con.write(&bytes[..split]);
            con.write(&bytes[split..]);
            assert!(buf == expected, "split at byte {split}");
        }
        let mut buf = [BG; WIDTH * HEIGHT];
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), WIDTH, HEIGHT, WIDTH) };
        for &byte in bytes {
            con.write_byte(byte);
        }
        assert!(buf == expected);
        assert_eq!((con.col, con.row), (1, 1));
        assert!(con.cursor_drawn);
        con.write(b"\x1b[?25l");
        // Cursor hiding restores the wide glyph's first cell exactly. — KESTREL
        reference.write(b"\x1b[?25l");
        assert!(buf == expected);
    }

    #[test]
    fn malformed_utf8_cannot_hide_the_next_ascii_or_ansi_control() {
        const WIDTH: usize = 192;
        let mut buf = [BG; WIDTH * GLYPH_H];
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), WIDTH, GLYPH_H, WIDTH) };
        con.write(&[
            0xe7, b'A', 0xed, 0xa0, 0x80, b'B', 0xf4, 0x90, 0x80, 0x80, b'C', 0xc0, 0xaf, b'D',
            0xe0, 0x80, 0xaf, b'E', 0xf0, 0x80, 0x80, 0xaf, b'F', 0xc3, 0xa9, b'G',
        ]);
        let mut expected = [BG; WIDTH * GLYPH_H];
        let mut reference = unsafe { Console::new(expected.as_mut_ptr(), WIDTH, GLYPH_H, WIDTH) };
        reference.write("�A�B�C��D�E�F�G".as_bytes());
        assert!(buf == expected);
        assert_eq!(con.col, 23);

        con.clear();
        con.write(b"\xe7\x1b[31mR");
        assert_cell_colors(&buf, WIDTH, b'R', 2, 0, 0x00aa_0000, BG);
        assert_eq!(con.parse_state, ParseState::Ground);
        assert_eq!(con.utf8.remaining, 0);

        con.clear();
        con.write(&[0xe7]);
        con.clear();
        con.write(b"A");
        assert_eq!(con.col, 1);
        assert_cell_colors(&buf, WIDTH, b'A', 0, 0, FG, BG);
    }

    #[test]
    fn wide_glyph_wraps_before_the_margin_and_scrolls_whole_rows() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        con.write("ABあ".as_bytes());
        assert_cell(&buf, b'A', 0, 0);
        assert_cell(&buf, b'B', 1, 0);
        assert_cell(&buf, b' ', 2, 0);
        assert_wide_cell(&buf, STRIDE, &font::glyph('あ' as u32).rows, 0, 1);
        con.write("日".as_bytes());
        assert_wide_cell(&buf, STRIDE, &font::glyph('あ' as u32).rows, 0, 0);
        assert_wide_cell(&buf, STRIDE, &font::glyph('日' as u32).rows, 0, 1);
        assert_eq!((con.col, con.row), (2, 1));
    }

    #[test]
    fn one_column_wide_glyph_never_touches_stride_padding_or_guards() {
        const STRIDE: usize = 16;
        const CANARY: u32 = 0x1234_5678;
        let mut buf = [CANARY; STRIDE * GLYPH_H + 16];
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), 8, GLYPH_H, STRIDE) };
        for y in 0..GLYPH_H {
            buf[y * STRIDE + 8..(y + 1) * STRIDE].fill(CANARY);
        }
        con.write("あ".as_bytes());
        for y in 0..GLYPH_H {
            assert!(buf[y * STRIDE + 8..(y + 1) * STRIDE]
                .iter()
                .all(|&p| p == CANARY));
        }
        assert!(buf[STRIDE * GLYPH_H..].iter().all(|&p| p == CANARY));
        assert_eq!(con.col, 1);
    }

    #[test]
    fn missing_four_byte_scalar_is_one_visible_wide_glyph() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };
        con.write("🙂A".as_bytes());
        assert_eq!(con.col, 3);
        assert_wide_cell(&buf, STRIDE, &font::glyph(0xfffd).rows, 0, 0);
        assert_cell(&buf, b'A', 2, 0);
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

    // A 5x4-cell screen for erase-in-display: room above, beside and below a cursor.
    const ED_W: usize = 5 * GLYPH_W;
    const ED_H: usize = 4 * GLYPH_H;

    /// A screen whose every cell holds a letter, `A` to `T` in reading order.
    fn full_screen(buf: &mut [u32; ED_W * ED_H]) -> Console {
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), ED_W, ED_H, ED_W) };
        con.write(b"ABCDE\r\nFGHIJ\r\nKLMNO\r\nPQRST");
        con
    }

    /// The cells of `buf` that still hold their `full_screen` letter, row by row (`.` = blank).
    fn cells(buf: &[u32; ED_W * ED_H]) -> [[u8; 5]; 4] {
        let mut out = [[b'.'; 5]; 4];
        for (row, line) in out.iter_mut().enumerate() {
            for (col, cell) in line.iter_mut().enumerate() {
                let letter = b'A' + (row * 5 + col) as u8;
                let rows = glyph_rows(letter);
                let holds = (0..GLYPH_H).all(|ry| {
                    (0..GLYPH_W).all(|rx| {
                        let on = (rows[ry] >> (7 - rx)) & 1 != 0;
                        buf[(row * GLYPH_H + ry) * ED_W + col * GLYPH_W + rx]
                            == if on { FG } else { BG }
                    })
                });
                let blank = (0..GLYPH_H).all(|ry| {
                    (0..GLYPH_W)
                        .all(|rx| buf[(row * GLYPH_H + ry) * ED_W + col * GLYPH_W + rx] == BG)
                });
                *cell = match (holds, blank) {
                    (true, _) => letter,
                    (false, true) => b'.',
                    _ => b'?',
                };
            }
        }
        out
    }

    #[test]
    fn the_kernel_shells_clear_blanks_every_cell_and_the_prompt_returns_home() {
        // `kernel/src/shell.rs` answers `clear` with ESC[2J then ESC[H; the prompt follows.
        let mut buf = [BG; ED_W * ED_H];
        let mut con = full_screen(&mut buf);
        con.write(b"\x1b[2J\x1b[H");
        assert_eq!(cells(&buf), [*b"....."; 4]);
        assert_eq!((con.col, con.row), (0, 0));
        con.write(b"P");
        assert_cell_colors(&buf, ED_W, b'P', 0, 0, FG, BG);
        assert_eq!((con.col, con.row), (1, 0));
    }

    #[test]
    fn busybox_and_ncurses_clear_sequences_blank_the_screen_at_any_read_split() {
        // busybox `clear` sends ESC[H ESC[J; ncurses `clear` adds ESC[2J and ESC[3J.
        for sequence in [&b"\x1b[H\x1b[J"[..], b"\x1b[H\x1b[2J\x1b[3J"] {
            for split in 0..=sequence.len() {
                let mut buf = [BG; ED_W * ED_H];
                let mut con = full_screen(&mut buf);
                con.write(&sequence[..split]);
                con.write(&sequence[split..]);
                assert_eq!(cells(&buf), [*b"....."; 4], "split at {split}");
                assert_eq!((con.col, con.row), (0, 0));
            }
        }
    }

    #[test]
    fn erase_below_and_above_keep_the_other_side_and_the_cursor() {
        let mut buf = [BG; ED_W * ED_H];
        let mut con = full_screen(&mut buf);
        con.write(b"\x1b[2;3H\x1b[0J");
        assert_eq!(cells(&buf), [*b"ABCDE", *b"FG...", *b".....", *b"....."]);
        assert_eq!((con.col, con.row), (2, 1));

        let mut buf = [BG; ED_W * ED_H];
        let mut con = full_screen(&mut buf);
        con.write(b"\x1b[2;3H\x1b[1J");
        assert_eq!(cells(&buf), [*b".....", *b"...IJ", *b"KLMNO", *b"PQRST"]);
        assert_eq!((con.col, con.row), (2, 1));
    }

    #[test]
    fn erase_display_leaves_the_cursor_where_it_was() {
        let mut buf = [BG; ED_W * ED_H];
        let mut con = full_screen(&mut buf);
        con.write(b"\x1b[3;2H\x1b[2J");
        assert_eq!(cells(&buf), [*b"....."; 4]);
        assert_eq!((con.col, con.row), (1, 2));
        con.write(b"X");
        assert_cell_colors(&buf, ED_W, b'X', 1, 2, FG, BG);
    }

    #[test]
    fn erasing_saved_lines_leaves_the_visible_screen() {
        let mut buf = [BG; ED_W * ED_H];
        let mut con = full_screen(&mut buf);
        con.write(b"\x1b[3J");
        assert_eq!(cells(&buf), [*b"ABCDE", *b"FGHIJ", *b"KLMNO", *b"PQRST"]);
    }

    #[test]
    fn erased_cells_take_the_current_background_like_erase_line() {
        let mut buf = [BG; ED_W * ED_H];
        let mut con = full_screen(&mut buf);
        con.write(b"\x1b[44m\x1b[2J");
        let blue = con.ansi_color(4);
        assert!(buf.iter().all(|&pixel| pixel == blue));
    }

    /// The same bytes through an unshadowed console (the reference) and a shadowed one.
    fn shadowed_against_reference(
        bytes: &[u8],
    ) -> ([u32; ED_W * ED_H], [u32; ED_W * ED_H], [u32; ED_W * ED_H]) {
        let mut reference = [BG; ED_W * ED_H];
        let mut glass = [0x00ee_eeee; ED_W * ED_H];
        let mut shadow = [0x0012_3456; ED_W * ED_H];
        unsafe { Console::new(reference.as_mut_ptr(), ED_W, ED_H, ED_W) }.write(bytes);
        unsafe {
            Console::new_shadowed(
                glass.as_mut_ptr(),
                shadow.as_mut_ptr(),
                ED_W,
                ED_H,
                ED_W,
                PixelOrder::Bgr,
            )
        }
        .write(bytes);
        (reference, glass, shadow)
    }

    #[test]
    fn a_shadowed_console_paints_exactly_what_an_unshadowed_one_does() {
        // Text, colours, Japanese, a wrap, enough newlines to scroll twice, an erase and a
        // visible cursor: every drawing path (J554).
        let bytes = "AB\x1b[44mC\x1b[0mD\r\nあE\r\nFGHIJKL\r\nM\r\nN\r\nO\x1b[1;2H\x1b[0J\x1b[?25h"
            .as_bytes();
        let (reference, glass, shadow) = shadowed_against_reference(bytes);
        assert!(glass == reference);
        assert!(shadow == reference);
    }

    #[test]
    fn scrolling_and_the_cursor_never_read_the_glass() {
        let mut glass = [BG; ED_W * ED_H];
        let mut shadow = [BG; ED_W * ED_H];
        let mut con = unsafe {
            Console::new_shadowed(
                glass.as_mut_ptr(),
                shadow.as_mut_ptr(),
                ED_W,
                ED_H,
                ED_W,
                PixelOrder::Bgr,
            )
        };
        con.write(b"A\r\nB\r\nC\r\nD");
        // Scribble over the glass behind the console's back. Anything read from the glass
        // would carry the scribble along.
        glass.fill(0x00ab_cdef);
        con.write(b"\r\nE");
        let mut reference = [BG; ED_W * ED_H];
        unsafe { Console::new(reference.as_mut_ptr(), ED_W, ED_H, ED_W) }
            .write(b"A\r\nB\r\nC\r\nD\r\nE");
        assert!(glass == reference, "scroll copied out of the glass");

        glass.fill(0x00ab_cdef);
        con.write(b"\x1b[?25h\x1b[?25l");
        // Only the cursor's cell (column 1 after `E`) was drawn, inverted and restored, and
        // both from the shadow: it is the blank cell the shadow holds, not the scribble.
        assert_eq!((con.col, con.row), (1, 3));
        for y in 0..GLYPH_H {
            let start = (3 * GLYPH_H + y) * ED_W + GLYPH_W;
            assert!(glass[start..start + GLYPH_W]
                .iter()
                .all(|&pixel| pixel == BG));
        }
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
    fn extended_colour_sub_parameters_are_skipped_not_misread() {
        let mut buf = fresh();
        let mut con = unsafe { Console::new(buf.as_mut_ptr(), W, H, STRIDE) };

        // `7` and `0` here are colour indices, not reverse video and reset; `31` after a
        // complete `38;5;N` still applies.
        con.write(b"\x1b[31m\x1b[38;5;7mA\x1b[38;5;0;34mB\x1b[48;2;1;2;3mC");

        assert_cell_colors(&buf, STRIDE, b'A', 0, 0, 0x00aa_0000, BG);
        assert_cell_colors(&buf, STRIDE, b'B', 1, 0, 0x0000_00aa, BG);
        assert_cell_colors(&buf, STRIDE, b'C', 2, 0, 0x0000_00aa, BG);
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
