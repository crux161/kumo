#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

//j249
//j480
//j519
//j554
//j559

use drv_fb::{Console, PixelOrder, BG, GLYPH_H, GLYPH_W};
use kumo_abi::{BootInfo, FramebufferFormat, Handle, VmarFlags};
use kumo_rt::{
    channel_read, channel_read_with_handle, framebuffer_claim, handle_koid, port_bind, port_create,
    port_wait, resource_mint_mmio, vmar_map, vmo_create,
};

/// Diagnostics go to the kernel log (`dmesg`) since J559: shown on the console only when its level
/// allows. Failures are ERR, so a default boot still shows them.
fn log_info(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::INFO, message);
}

fn log_err(message: &[u8]) {
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::ERR, message);
}

kumo_rt::entry!(main);

/// One log line built without `core::fmt` (J559).
struct Line {
    buf: [u8; 96],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self {
            buf: [0; 96],
            len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let take = bytes.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
    }

    fn push_decimal(&mut self, mut value: usize) {
        let mut digits = [0u8; 20];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.push(&digits[start..]);
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// Log `label` then `v` in decimal and a newline. The bootstrap path offers no `core::fmt` and no
/// alloc, so format the integer by hand.
/// Used by the J231 X13s framebuffer-geometry diagnostic.
fn dbg_kv(label: &[u8], v: u64) {
    log_info(label);
    let mut digits = [0u8; 20];
    let mut n = 0;
    let mut x = v;
    loop {
        digits[n] = b'0' + (x % 10) as u8;
        x /= 10;
        n += 1;
        if x == 0 {
            break;
        }
    }
    let mut line = [0u8; 21];
    for i in 0..n {
        line[i] = digits[n - 1 - i];
    }
    line[n] = b'\n';
    log_info(&line[..n + 1]);
}

#[no_mangle]
extern "C" fn main(
    _arg0: u64,
    bootstrap_channel: u64,
    _arg2: u64,
    _arg3: u64,
    _arg4: u64,
    _arg5: u64,
    _arg6: u64,
    _arg7: u64,
) -> ! {
    log_info(b"drv-fb starting\n");

    let bootstrap = Handle(bootstrap_channel as u32);
    let mut buf = [0u8; 32];

    // Read fb_res, console, and bootinfo VMO sequentially from the bootstrap channel.
    let (_n1, res_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let res = Handle(res_raw as u32);
    let (_n2, console_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let console = Handle(console_raw as u32);
    let (_n3, bootinfo_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let bootinfo_vmo = Handle(bootinfo_raw as u32);

    if res_raw == 0 || console_raw == 0 || bootinfo_raw == 0 {
        log_err(b"drv-fb: missing handles\n");
        kumo_rt::process_exit(1);
    }

    // Map BootInfo to read framebuffer geometry (exercises J156 live-write path).
    let bootinfo_va = 0x0000_0000_1000_0000;
    if vmar_map(
        Handle(0),
        bootinfo_vmo,
        0,
        bootinfo_va,
        4096,
        (VmarFlags::READ).0,
    ) != 0
    {
        log_err(b"drv-fb: bootinfo map failed\n");
        kumo_rt::process_exit(1);
    }

    let bootinfo = unsafe { &*(bootinfo_va as *const BootInfo) };
    if !bootinfo.has_framebuffer() {
        log_err(b"drv-fb: no fb in bootinfo\n");
        kumo_rt::process_exit(1);
    }

    let fb_phys = bootinfo.framebuffer.phys;
    let fb_len = bootinfo.framebuffer.len;
    let width = bootinfo.framebuffer.width as usize;
    let height = bootinfo.framebuffer.height as usize;
    let stride = bootinfo.framebuffer.stride as usize;
    let pixel_order = match bootinfo.framebuffer.format {
        FramebufferFormat::Rgb => PixelOrder::Rgb,
        // Live GOP paths declare an order. Preserve BGR as the legacy best effort for an
        // unknown tag; the default green remains order-independent either way. — KESTREL
        FramebufferFormat::Bgr | FramebufferFormat::Unknown => PixelOrder::Bgr,
    };
    let fb_map_len = match bootinfo.framebuffer.mapping_len() {
        Some(len) => len,
        None => {
            log_err(b"drv-fb: fb map length overflow\n");
            kumo_rt::process_exit(1);
        }
    };

    // J231 diagnostic: dump the framebuffer geometry drv-fb reads from the BootInfo VMO, so the
    // X13s scroll/overwrite bug can be pinned from serial. The scroll math is host-proven; if the
    // screen corrupts, suspect geometry — chiefly `stride != width` (scroll() copies misaligned
    // pixel bands) or an implausible `height`/`len`.
    dbg_kv(b"drv-fb fb.width=", width as u64);
    dbg_kv(b"drv-fb fb.height=", height as u64);
    dbg_kv(b"drv-fb fb.stride=", stride as u64);
    dbg_kv(b"drv-fb fb.len=", fb_len);

    // Mint and map the actual framebuffer.
    // Map the page-granular backing span, but retain `fb_len` for framebuffer ownership and
    // visible-pixel bounds. This includes only the unavoidable tail of the final page.
    let fb_vmo_h = resource_mint_mmio(res, fb_phys, fb_map_len);
    if fb_vmo_h == u64::MAX {
        log_err(b"drv-fb: fb vmo mint failed\n");
        kumo_rt::process_exit(1);
    }

    // Keep the multi-megabyte framebuffer clear of this process's fixed user stack at
    // [0x1000_C000, 0x1001_0000). The former 0x1000_1000 base silently remapped those
    // stack PTEs, so drv-fb faulted immediately after VmarMap returned. 0x1100_0000
    // leaves a full 16 MiB gap and still fits comfortably in the 512 MiB child VMAR.
    let fb_va = 0x0000_0000_1100_0000;
    if vmar_map(
        Handle(0),
        Handle(fb_vmo_h as u32),
        0,
        fb_va,
        fb_map_len,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    ) != 0
    {
        log_err(b"drv-fb: fb map failed\n");
        kumo_rt::process_exit(1);
    }

    // Probe the framebuffer mapping BEFORE taking the glass. drv-fb must not claim ownership
    // from the HAL unless it can actually write the panel: on the X13s the first write to this
    // mapping faults, and because the claim had already made the HAL dormant, every subsequent
    // line routed into an unrendered framebuffer (a ~20-line blank gap) until drv-fb died and
    // the kernel reclaimed the glass. Touch both ends of the exact span Console will use (pixel
    // 0 and the last pixel of `stride*height`) so a bad mapping OR an out-of-extent geometry
    // faults HERE, pre-claim — drv-fb dies, the HAL keeps the console, and the boot stays
    // legible (DESIGN/002: never hold critical state you cannot recover). If the probe survives,
    // the marker confirms on the next boot that the mapping is writable.
    // Pinpoint breadcrumb for the X13s fault hunt: if the panel shows this line but not
    // "fb probe ok", the FIRST framebuffer write faulted — compare the kernel's FAR dump
    // against fb_va (0x1100_0000) to confirm it is this mapping.
    log_info(b"drv-fb: probing fb write\n");
    let fb_words = fb_va as *mut u32;
    let last_px = stride.saturating_mul(height).saturating_sub(1);
    unsafe {
        fb_words.write_volatile(BG);
        fb_words.add(last_px).write_volatile(BG);
    }
    let probe_ok = b"drv-fb: fb probe ok\n";
    log_info(probe_ok);

    // Bind the console port before taking ownership. Diagnostics emitted immediately
    // after the claim are queued to this channel; binding first guarantees each queued
    // message also gets a port packet for the loop below.
    let port_h = port_create();
    let console_koid = handle_koid(console);
    if port_h == u64::MAX
        || console_koid == u64::MAX
        || port_bind(Handle(port_h as u32), console) != 0
    {
        log_err(b"drv-fb: port setup failed\n");
        kumo_rt::process_exit(1);
    }
    let port = Handle(port_h as u32);
    let console_source = Handle(console_koid as u32);

    // This successful, capability-checked claim is the single ownership boundary:
    // before it the HAL console alone may paint the GOP framebuffer; after it every
    // kernel/user diagnostic is routed to this driver and the HAL cursor is dormant.
    // Claim only after the mapping works, so a failed driver leaves the early console live.
    if framebuffer_claim(res, fb_phys, fb_len) != 0 {
        log_err(b"drv-fb: framebuffer claim failed\n");
        kumo_rt::process_exit(1);
    }

    // J554: a copy of the screen in ordinary cached memory. The framebuffer is mapped
    // non-cacheable, and reading it back to scroll cost about 165 ms a line at 1600x900; with
    // the shadow, the glass is only ever written. It sits above the framebuffer's mapping,
    // rounded to 16 MiB, inside the 512 MiB child VMAR. Without one, the console reads the
    // glass as before. — TIMBERDOODLE
    let shadow_va = fb_va + fb_map_len.next_multiple_of(0x100_0000);
    let shadow_vmo = vmo_create(fb_map_len);
    let shadowed = shadow_vmo != 0
        && shadow_vmo != u64::MAX
        && vmar_map(
            Handle(0),
            Handle(shadow_vmo as u32),
            0,
            shadow_va,
            fb_map_len,
            (VmarFlags::READ | VmarFlags::WRITE).0,
        ) == 0;
    let shadow = if shadowed {
        shadow_va as *mut u32
    } else {
        let _ = kumo_rt::log_write(
            kumo_abi::sys::log_level::WARNING,
            b"drv-fb: no shadow; scrolling reads the glass\n",
        );
        fb_va as *mut u32
    };

    // Build the text console over the framebuffer (clears the screen).
    let mut con = unsafe {
        Console::new_shadowed(
            fb_va as *mut u32,
            shadow,
            width,
            height,
            stride,
            pixel_order,
        )
    };
    // The glass starts blank: first light is a log line (`dmesg`), not text on the screen. The
    // console's size is the boot milestone the default level shows (J559).
    let mut line = Line::new();
    line.push(b"drv-fb: online, console ");
    line.push_decimal(width);
    line.push(b"x");
    line.push_decimal(height);
    line.push(b", ");
    line.push_decimal(width / GLYPH_W);
    line.push(b"x");
    line.push_decimal(height / GLYPH_H);
    line.push(b" cells\n");
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, line.bytes());
    log_info(b"drv-fb: initialized\n");
    log_info(b"drv-fb: console ready\n");

    // Pump the console channel: the bytes Sora writes become glyphs on the framebuffer.
    // Output-only — no keyboard device sits behind the framebuffer yet. A read takes a whole
    // message and drops what does not fit, so the buffer is a whole message (J559: a 256-byte
    // buffer cut the boot replay after the banner's second line).
    let mut rx = [0u8; 4096];
    loop {
        let source = Handle(port_wait(port) as u32);
        if source == console_source {
            let n = channel_read(console, rx.as_mut_ptr(), rx.len()) as usize;
            if n > 0 {
                con.write(&rx[..n]);
            }
        }
    }
}
