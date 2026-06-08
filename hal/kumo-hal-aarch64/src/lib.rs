#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

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

fn pl011_reg(offset: usize) -> *mut u32 {
    (PL011_BASE + offset) as *mut u32
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
const FG: u32 = 0x00ff_ffff; // white — identical bytes in RGB and BGR
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
        for rx in 0..GLYPH_W {
            let on = (bits >> (7 - rx)) & 1 != 0;
            let idx = py.wrapping_mul(stride).wrapping_add(x_px + rx);
            if idx < len_px {
                unsafe { base.add(idx).write_volatile(if on { fg } else { bg }) };
            }
        }
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

    let ptr = base as *mut u32;
    let mut i = 0;
    while i < len_px {
        unsafe { ptr.add(i).write_volatile(BG) };
        i += 1;
    }

    FB_PRESENT.store(true, ORD);
}

fn fb_putc(byte: u8) {
    let stride = FB_STRIDE.load(ORD) as usize;
    let cols = (FB_WIDTH.load(ORD) as usize / GLYPH_W).max(1);
    let rows = (FB_HEIGHT.load(ORD) as usize / GLYPH_H).max(1);
    let mut col = FB_COL.load(ORD) as usize;
    let mut row = FB_ROW.load(ORD) as usize;

    match byte {
        b'\n' => {
            col = 0;
            row += 1;
        }
        b'\r' => col = 0,
        0x20..=0x7e => {
            if col >= cols {
                col = 0;
                row += 1;
            }
            if row < rows {
                let base = FB_BASE.load(ORD) as *mut u32;
                let len_px = FB_LEN_PX.load(ORD);
                unsafe {
                    blit_glyph(
                        base,
                        len_px,
                        stride,
                        col * GLYPH_W,
                        row * GLYPH_H,
                        byte,
                        FG,
                        BG,
                    )
                };
                col += 1;
            }
        }
        _ => {}
    }

    // No scrolling yet: clamp at the bottom row (Stage-A output is a few lines).
    if row >= rows {
        row = rows - 1;
        col = 0;
    }
    FB_COL.store(col as u32, ORD);
    FB_ROW.store(row as u32, ORD);
}

pub fn early_console_write(bytes: &[u8]) {
    if FB_PRESENT.load(ORD) {
        for &byte in bytes {
            fb_putc(byte);
        }
        return;
    }

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

/// Install the preemption hook (the scheduler tick). It runs in IRQ context after the
/// timer interrupt is acknowledged/EOI'd, and may context-switch via `switch_context`.
pub fn set_preempt_hook(hook: extern "C" fn()) {
    PREEMPT_HOOK.store(hook as usize, ORD);
}

/// Stop calling the preemption hook (back to plain timer ticks).
pub fn clear_preempt_hook() {
    PREEMPT_HOOK.store(0, ORD);
}

#[cfg(target_os = "none")]
unsafe fn eoi(intid: u32) {
    unsafe {
        core::arch::asm!("msr icc_eoir1_el1, {0}", in(reg) intid as u64, options(nostack, nomem))
    };
}

#[cfg(not(target_os = "none"))]
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
        "  KVEC_EXC 0",
        "  KVEC_IRQ 1",
        "  KVEC_EXC 2",
        "  KVEC_EXC 3",
        "  KVEC_EXC 4",
        "  KVEC_IRQ 5",
        "  KVEC_EXC 6",
        "  KVEC_EXC 7",
        "  KVEC_EXC 8",
        "  KVEC_IRQ 9",
        "  KVEC_EXC 10",
        "  KVEC_EXC 11",
        "  KVEC_EXC 12",
        "  KVEC_IRQ 13",
        "  KVEC_EXC 14",
        "  KVEC_EXC 15",
        ".purgem KVEC_EXC",
        ".purgem KVEC_IRQ",
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
