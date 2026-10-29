#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

//j660
//j662
//j667
//j668
//j670

mod clock_probe;
mod display_probe;
mod display_topology;
mod hdmi_probe;
mod modeset;
mod phy_relock;
mod shared_probe;

use drv_fb::{clean_pixels, paint_band, Console, PixelOrder, BG, GLYPH_H, GLYPH_W};
use kumo_abi::{BootInfo, FramebufferFormat, Handle, Rights, VmarFlags};
use kumo_graphics::{Present, Reply, Status, SURFACE_BYTES};
use kumo_rt::{
    channel_read, channel_read_with_handle, framebuffer_claim, handle_duplicate, handle_koid,
    port_bind, port_create, port_wait, resource_mint_mmio, syscall_handle, vmar_map, vmo_create,
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

/// One bound presentation channel: its port source koID, the server end of the channel,
/// the mapped read-only surface, and the server-owned view it presents into.
#[derive(Clone, Copy)]
struct GraphicsLane {
    source: Handle,
    channel: Handle,
    surface: u64,
    view: usize,
    /// A complete image has reached this view; a partial first present is refused. — TIMBERDOODLE
    full_seen: bool,
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
    if _arg0 == drv_fb::rk3588_scanout::HOLDER_ARG {
        let channel = Handle(bootstrap_channel as u32);
        let mut message = [0u8; 1];
        let (n, ctx) = channel_read_with_handle(channel, message.as_mut_ptr(), 1);
        if n != 1 || message[0] != b'H' || ctx == 0 {
            kumo_rt::process_exit(1);
        }
        let _ = kumo_rt::channel_write(channel, b"H".as_ptr(), 1);
        // The live handle stays solely in this process's table until Sora kills it. — KESTREL
        loop {
            let _ = kumo_rt::channel_read(channel, message.as_mut_ptr(), 1);
        }
    }
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

    // Kernel-owned display context and its exact GOP/A/B VMOs; fixed records on every board.
    let (_, ctx_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, gop_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, a_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, b_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());

    let (_, control_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, native_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let power_len = channel_read(bootstrap, buf.as_mut_ptr(), buf.len());
    let power = buf
        .get(..power_len as usize)
        .and_then(drv_fb::rk3588::Power::decode);
    let (_, display_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    // D0c: the grant data record precedes the one CRU Resource handle. Decode it
    // immediately so the 32-byte buffer is free for the graphics reads below; an
    // absent record (one zero byte) or any malformed record becomes None and the
    // probe performs zero CRU reads. — TIMBERDOODLE
    let clock_grant_len = channel_read(bootstrap, buf.as_mut_ptr(), buf.len());
    let clock_grant = buf
        .get(..clock_grant_len as usize)
        .and_then(drv_fb::rk3588_clock::Grant::decode);
    let (_, clock_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());

    // D0d: the three read-only display apertures, always sent, in H/P/R order.
    // — TIMBERDOODLE
    let (_, hdmi_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, phy_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, grf_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());

    // R0: three read-only GRF pages, always sent, in VOP_GRF/VO1_GRF/SYS_GRF order (tags
    // V/W/Y). An ineligible or powered-down path sends Handle(0). — TIMBERDOODLE
    let (_, vop_grf_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, vo1_grf_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, sys_grf_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());

    // R1: the two writable device windows, always sent as U (VOP2 registers) and X (HDMI0 TX
    // page); Handle(0) off the Orange Pi or with the display down. — TIMBERDOODLE
    let (_, vop_rw_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, tx_rw_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, phy_rw_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let (_, grf_rw_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());

    // D0b: the supervisor's private copy of the live firmware DTB follows D0a's handle,
    // D0c's clock grant and D0d's H/P/R records. The record is fixed-size on every board;
    // an absent copy arrives as Handle(0). — TIMBERDOODLE
    let (topology_len, topology_raw) =
        channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
    let topology_transfer =
        drv_fb::dtb_topology::Transfer::decode(buf.get(..topology_len as usize).unwrap_or(&[]));

    // Optional graphics lanes: one private channel and one read-only ordinary VMO per
    // server-owned view. Sora always sends one G record per view; Handle(0) disables that
    // view without preventing console startup.
    let mut graphics = [Handle(0); kumo_graphics::VIEW_COUNT];
    let mut surface_handles = [Handle(0); kumo_graphics::VIEW_COUNT];
    for view in 0..kumo_graphics::VIEW_COUNT {
        let (_, graphics_raw) = channel_read_with_handle(bootstrap, buf.as_mut_ptr(), buf.len());
        graphics[view] = Handle(graphics_raw as u32);
        if graphics_raw != 0 {
            let (_, surface_raw) =
                channel_read_with_handle(graphics[view], buf.as_mut_ptr(), buf.len());
            surface_handles[view] = Handle(surface_raw as u32);
        }
    }

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

    // D0b: duplicate the VOP Resource before D0a's report consumes it. Since J605 the grant is
    // READ|DUPLICATE|TRANSFER (DESIGN/020 D0-1) and READ is all a mint needs, so the duplicate
    // asks for READ alone: no new authority and no second mint path. — TIMBERDOODLE
    let topology_vop = if display_raw != 0 {
        let duplicated = handle_duplicate(Handle(display_raw as u32), Rights::READ);
        syscall_handle(duplicated).unwrap_or(Handle(0))
    } else {
        Handle(0)
    };
    let vop_view_mapped = display_probe::report(bootinfo, power, Handle(display_raw as u32));
    let cru_mapped = clock_probe::report_clock_probe(clock_grant, Handle(clock_raw as u32));
    let tx_mapped = hdmi_probe::report(
        bootinfo,
        power,
        Handle(hdmi_raw as u32),
        Handle(phy_raw as u32),
        Handle(grf_raw as u32),
    );
    let r0_matched = shared_probe::report(
        bootinfo,
        power,
        cru_mapped,
        tx_mapped,
        Handle(vop_grf_raw as u32),
        Handle(vo1_grf_raw as u32),
        Handle(sys_grf_raw as u32),
    );
    let live_vop_irq = display_topology::report(
        bootinfo,
        power,
        topology_vop,
        topology_transfer,
        Handle(topology_raw as u32),
    );

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
    let fb_vmo_h = if gop_raw != 0 {
        gop_raw
    } else {
        resource_mint_mmio(res, fb_phys, fb_map_len)
    };
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

    // The server owns the layout: two views in a band above the text console. When the
    // framebuffer cannot hold both, graphics is disabled and the console keeps the screen.
    let layout = kumo_graphics::band(width as u32, height as u32);
    let region = kumo_graphics::console_region(width as u32, height as u32);
    let console_px = region.y as usize * stride;
    // Build the text console below the band (clears only that region).
    let mut con = unsafe {
        Console::new_shadowed(
            (fb_va as *mut u32).add(console_px),
            shadow.add(console_px),
            region.width as usize,
            region.height as usize,
            stride,
            pixel_order,
        )
    };
    // Paint the band: background, server-drawn chrome, and empty view wells. Client pixels
    // reach the glass only through present_view, and only inside a view rectangle.
    if let Some(band) = &layout {
        paint_band(band, width, stride, pixel_order, |index, color| unsafe {
            (fb_va as *mut u32).add(index).write_volatile(color)
        });
        unsafe {
            clean_pixels(fb_va as *mut u32, 0, band.height as usize * stride);
        }
    }
    // The glass starts blank: first light is a log line (`dmesg`), not text on the screen. The
    // console's size is the boot milestone the default level shows (J559).
    let mut line = Line::new();
    line.push(b"drv-fb: online, console ");
    line.push_decimal(region.width as usize);
    line.push(b"x");
    line.push_decimal(region.height as usize);
    line.push(b", ");
    line.push_decimal(region.width as usize / GLYPH_W);
    line.push(b"x");
    line.push_decimal(region.height as usize / GLYPH_H);
    line.push(b" cells");
    if layout.is_some() {
        line.push(b", ");
        line.push_decimal(kumo_graphics::VIEW_COUNT);
        line.push(b" views");
    }
    line.push(b"\n");
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, line.bytes());
    log_info(b"drv-fb: initialized\n");
    log_info(b"drv-fb: console ready\n");

    // Map each view's read-only surface after the fixed framebuffer and shadow mappings, so
    // the kernel's placement cannot be overwritten by them. Mapping failure disables only
    // that view; the console survives.
    // The live surface base per view. It outlives a closed lane (retained content) and is
    // reclaimed only when a regrant replaces it. — TIMBERDOODLE
    let mut view_surface = [0u64; kumo_graphics::VIEW_COUNT];
    for view in 0..kumo_graphics::VIEW_COUNT {
        let handle = surface_handles[view];
        if handle.0 != 0 {
            let (status, addr) =
                kumo_rt::vmar_map_anywhere(handle, SURFACE_BYTES as u64, VmarFlags::READ.0);
            let _ = kumo_rt::handle_close(handle);
            if status == 0 {
                view_surface[view] = addr;
            }
        }
    }

    // Bind each presentation channel to its port after the surface mapping, so a lane never
    // exists without the base it will read.
    let mut lanes: [Option<GraphicsLane>; kumo_graphics::VIEW_COUNT] =
        core::array::from_fn(|_| None);
    for view in 0..kumo_graphics::VIEW_COUNT {
        if graphics[view].0 == 0 {
            continue;
        }
        let koid = handle_koid(graphics[view]);
        if koid != u64::MAX && port_bind(port, graphics[view]) == 0 {
            lanes[view] = Some(GraphicsLane {
                source: Handle(koid as u32),
                channel: graphics[view],
                surface: view_surface[view],
                view,
                full_seen: false,
            });
        } else {
            let _ = kumo_rt::handle_close(graphics[view]);
        }
    }

    // Retained per-view content from the last accepted present. The read-only surface
    // mapping outlives a client channel, so a closed client's view keeps composing in its
    // server-assigned order. — TIMBERDOODLE
    let mut view_content: [Option<(u64, kumo_graphics::Surface)>; kumo_graphics::VIEW_COUNT] =
        [None; kumo_graphics::VIEW_COUNT];
    // True when a client closed the lane and left the view available to a regrant. — TIMBERDOODLE
    let mut view_released = [false; kumo_graphics::VIEW_COUNT];

    // F2b slice-1: the software cursor. drv-fb owns the sprite; a `P` record on the control
    // lane only says where. The sprite is drawn last, and a move repairs the scene beneath it
    // from the console shadow and the retained band layers, so no saved-background buffer
    // exists to go stale. — TIMBERDOODLE
    #[derive(Clone, Copy)]
    struct Cursor {
        x: u32,
        y: u32,
        #[allow(dead_code)]
        buttons: u8,
    }
    let mut cursor: Option<Cursor> = None;
    let mut pointer_records: u32 = 0;
    // F2b focus: the view Sora last named as the keyboard owner, painted with highlighted
    // chrome. A focus change repaints only the old and new chrome frames. — TIMBERDOODLE
    let mut focused: Option<usize> = None;

    // F2c: server-drawn band labels through the glyph atlas, toggled by a `T` record and
    // redrawn once per wake beneath the cursor overlay. — TIMBERDOODLE
    let mut titles = false;
    let draw_labels =
        |on: bool, fb: u64, stride: usize, order: PixelOrder, width: u32, height: u32| {
            if !on {
                return;
            }
            drv_fb::blit_ascii(
                b"VIEW 0",
                width,
                height,
                stride,
                order,
                8,
                8,
                0x00ff_ffff,
                |at, color| unsafe { (fb as *mut u32).add(at).write_volatile(color) },
            );
            drv_fb::blit_ascii(
                b"VIEW 1",
                width,
                height,
                stride,
                order,
                8,
                32,
                0x00ff_ffff,
                |at, color| unsafe { (fb as *mut u32).add(at).write_volatile(color) },
            );
        };

    // Sora retains this bootstrap channel's peer; a runtime `R` record on it regrants one
    // view with a fresh presentation channel and surface. — TIMBERDOODLE
    let control_koid = handle_koid(bootstrap);
    let control_source = Handle(if control_koid == u64::MAX {
        0
    } else {
        control_koid as u32
    });
    let control_bound = control_source.0 != 0 && port_bind(port, bootstrap) == 0;
    let control_message: &[u8] = if control_bound {
        b"drv-fb: view control armed\n"
    } else {
        b"drv-fb: view control unavailable\n"
    };
    let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, control_message);

    // Pump the console channel: the bytes Sora writes become glyphs on the framebuffer.
    // Output-only — no keyboard device sits behind the framebuffer yet. A read takes a whole
    // message and drops what does not fit, so the buffer is a whole message (J559: a 256-byte
    // buffer cut the boot replay after the banner's second line).
    let mut rx = [0u8; 4096];
    // R1: blank and scrub now, once the console drains through the port; the rebuild and its
    // verification run from one-shot timers on the same port, so the console never stalls
    // behind the deliberate blank. — TIMBERDOODLE
    let mut r1 = modeset::start(
        bootinfo,
        power,
        r0_matched,
        Handle(vop_rw_raw as u32),
        Handle(tx_rw_raw as u32),
        phy_relock::Grants {
            phy: Handle(phy_rw_raw as u32),
            grf: Handle(grf_rw_raw as u32),
        },
        modeset::Vblank {
            live_irq: live_vop_irq,
            mmu_view: vop_view_mapped,
            context: Handle(ctx_raw as u32),
            control: Handle(control_raw as u32),
            native: Handle(native_raw as u32),
            buffers: [Handle(a_raw as u32), Handle(b_raw as u32)],
        },
        port,
    );
    if r1.is_none() && native_raw != 0 {
        modeset::cancel_native(Handle(native_raw as u32));
    }
    let mut was_deferred = false;
    let (mut deferred_bytes, mut deferred_max_ns) = (0u64, 0u64);
    loop {
        let measuring = r1.as_ref().is_some_and(|p| p.measuring_vblank());
        let deferred = con.defer_output(measuring);
        if deferred && !was_deferred {
            deferred_bytes = 0;
            deferred_max_ns = 0;
        }
        if !deferred && was_deferred {
            let mut line = Line::new();
            line.push(b"display: D1b cached console bytes=");
            line.push_decimal(deferred_bytes as usize);
            line.push(b", max burst us=");
            line.push_decimal((deferred_max_ns / 1000) as usize);
            line.push(b"\n");
            let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, line.bytes());
        }
        was_deferred = deferred;
        // F2b slice-1: the sprite is a pure overlay over the scene. Console output, presents
        // and modeset repaints write under it, so it is redrawn once per wake before the next
        // wait; a pointer move repairs its old and new rectangles from the scene first.
        // — TIMBERDOODLE
        // F2c: the band labels are an overlay like the cursor; a band recompose can erase
        // them, so both redraw once per wake, the labels beneath the sprite. — TIMBERDOODLE
        draw_labels(
            titles,
            fb_va,
            stride,
            pixel_order,
            width as u32,
            height as u32,
        );
        if let Some(cursor) = cursor {
            drv_fb::draw_cursor(
                width as u32,
                height as u32,
                stride,
                pixel_order,
                cursor.x,
                cursor.y,
                |at, color| unsafe { (fb_va as *mut u32).add(at).write_volatile(color) },
            );
        }
        let source = Handle(port_wait(port) as u32);
        if r1.as_ref().is_some_and(|pending| pending.owns(source)) {
            r1 = r1.take().and_then(|pending| modeset::wake(pending, source));
            continue;
        }
        if control_bound && source == control_source {
            let (n, regrant_raw) = channel_read_with_handle(bootstrap, rx.as_mut_ptr(), rx.len());
            let channel = Handle(regrant_raw as u32);
            if n == 1 && rx[0] == kumo_graphics::RELEASE_QUERY_TAG {
                if channel.0 != 0 {
                    let _ = kumo_rt::handle_close(channel);
                }
                let mut mask = 0u8;
                for (index, released) in view_released.iter().enumerate() {
                    if *released {
                        mask |= 1u8 << index;
                    }
                }
                let reply = kumo_graphics::release_reply(mask);
                let _ = kumo_rt::channel_write(bootstrap, reply.as_ptr(), reply.len());
                continue;
            }
            if n == kumo_graphics::POINTER_RECORD_LEN && rx[0] == kumo_graphics::POINTER_TAG {
                if channel.0 != 0 {
                    let _ = kumo_rt::handle_close(channel);
                }
                if let Some((x, y, buttons)) = kumo_graphics::parse_pointer(&rx[..n]) {
                    let previous = cursor;
                    let placed = Cursor {
                        x: x as u32,
                        y: y as u32,
                        buttons,
                    };
                    cursor = Some(placed);
                    pointer_records = pointer_records.saturating_add(1);
                    if pointer_records == 1 {
                        let _ = kumo_rt::log_write(
                            kumo_abi::sys::log_level::NOTICE,
                            b"drv-fb: pointer ready\n",
                        );
                    }
                    if pointer_records <= 8 {
                        let mut line = Line::new();
                        line.push(b"drv-fb: pointer x=");
                        line.push_decimal(x as usize);
                        line.push(b" y=");
                        line.push_decimal(y as usize);
                        line.push(b" buttons=");
                        line.push_decimal(buttons as usize);
                        line.push(b"\n");
                        log_info(line.bytes());
                    }
                    let new_rect = drv_fb::cursor_damage(placed.x, placed.y);
                    let damage = match previous {
                        Some(old) => {
                            drv_fb::union_damage(drv_fb::cursor_damage(old.x, old.y), new_rect)
                        }
                        None => new_rect,
                    };
                    if let Some(rect) = drv_fb::clip_damage(damage, width as u32, height as u32) {
                        let console_top = layout.as_ref().map_or(0, |band| band.height);
                        let bottom = rect.y.saturating_add(rect.height);
                        if bottom > console_top {
                            let top = rect.y.max(console_top);
                            drv_fb::copy_shadow_rect(
                                stride,
                                |at| unsafe { shadow.add(at).read_volatile() },
                                kumo_graphics::Damage {
                                    x: rect.x,
                                    y: top,
                                    width: rect.width,
                                    height: bottom - top,
                                },
                                |at, pixel| unsafe {
                                    (fb_va as *mut u32).add(at).write_volatile(pixel)
                                },
                            );
                        }
                        if let Some(band) = layout.as_ref() {
                            if rect.y < band.height {
                                let mut content = [None; kumo_graphics::VIEW_COUNT];
                                let mut bases = [0u64; kumo_graphics::VIEW_COUNT];
                                for (other, entry) in view_content.iter().enumerate() {
                                    if let Some((base, description)) = entry {
                                        bases[other] = *base;
                                        content[other] = Some(*description);
                                    }
                                }
                                drv_fb::repaint_band_rect(
                                    band,
                                    stride,
                                    kumo_graphics::Damage {
                                        x: rect.x,
                                        y: rect.y,
                                        width: rect.width,
                                        height: band.height.min(bottom) - rect.y,
                                    },
                                    &content,
                                    pixel_order,
                                    focused,
                                    |layer, at| unsafe {
                                        (bases[layer] as *const u32).add(at).read_volatile()
                                    },
                                    |at, color| unsafe {
                                        (fb_va as *mut u32).add(at).write_volatile(color)
                                    },
                                );
                            }
                        }
                        drv_fb::draw_cursor(
                            width as u32,
                            height as u32,
                            stride,
                            pixel_order,
                            placed.x,
                            placed.y,
                            |at, color| unsafe {
                                (fb_va as *mut u32).add(at).write_volatile(color)
                            },
                        );
                    }
                }
                continue;
            }
            if n == 2 && rx[0] == kumo_graphics::FOCUS_TAG {
                if channel.0 != 0 {
                    let _ = kumo_rt::handle_close(channel);
                }
                if let Some(next) = kumo_graphics::parse_focus(&rx[..n]) {
                    if focused != next {
                        let previous = focused;
                        focused = next;
                        let mut line = Line::new();
                        line.push(b"drv-fb: focus ");
                        match next {
                            Some(view) => {
                                line.push(b"view ");
                                line.push_decimal(view);
                            }
                            None => line.push(b"none"),
                        }
                        line.push(b"\n");
                        let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, line.bytes());
                        if let Some(band) = layout.as_ref() {
                            let mut content = [None; kumo_graphics::VIEW_COUNT];
                            let mut bases = [0u64; kumo_graphics::VIEW_COUNT];
                            for (other, entry) in view_content.iter().enumerate() {
                                if let Some((base, description)) = entry {
                                    bases[other] = *base;
                                    content[other] = Some(*description);
                                }
                            }
                            for view in [previous, next].into_iter().flatten() {
                                let chrome = drv_fb::chrome_damage(&band.views[view], band.height);
                                if let Some(rect) =
                                    drv_fb::clip_damage(chrome, width as u32, height as u32)
                                {
                                    drv_fb::repaint_band_rect(
                                        band,
                                        stride,
                                        rect,
                                        &content,
                                        pixel_order,
                                        focused,
                                        |layer, at| unsafe {
                                            (bases[layer] as *const u32).add(at).read_volatile()
                                        },
                                        |at, color| unsafe {
                                            (fb_va as *mut u32).add(at).write_volatile(color)
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
                continue;
            }
            if n == 2 && rx[0] == kumo_graphics::TITLES_TAG {
                if channel.0 != 0 {
                    let _ = kumo_rt::handle_close(channel);
                }
                if let Some(on) = kumo_graphics::parse_titles(&rx[..n]) {
                    if titles != on {
                        titles = on;
                        // Repair the label rectangles from the band, then redraw if turning on.
                        if let Some(band) = layout.as_ref() {
                            let mut content = [None; kumo_graphics::VIEW_COUNT];
                            let mut bases = [0u64; kumo_graphics::VIEW_COUNT];
                            for (other, entry) in view_content.iter().enumerate() {
                                if let Some((base, description)) = entry {
                                    bases[other] = *base;
                                    content[other] = Some(*description);
                                }
                            }
                            for rect in [
                                kumo_graphics::Damage {
                                    x: 8,
                                    y: 8,
                                    width: 48,
                                    height: 16,
                                },
                                kumo_graphics::Damage {
                                    x: 8,
                                    y: 32,
                                    width: 48,
                                    height: 16,
                                },
                            ] {
                                if let Some(rect) =
                                    drv_fb::clip_damage(rect, width as u32, height as u32)
                                {
                                    drv_fb::repaint_band_rect(
                                        band,
                                        stride,
                                        rect,
                                        &content,
                                        pixel_order,
                                        focused,
                                        |layer, at| unsafe {
                                            (bases[layer] as *const u32).add(at).read_volatile()
                                        },
                                        |at, color| unsafe {
                                            (fb_va as *mut u32).add(at).write_volatile(color)
                                        },
                                    );
                                }
                            }
                        }
                        draw_labels(
                            titles,
                            fb_va,
                            stride,
                            pixel_order,
                            width as u32,
                            height as u32,
                        );
                        let _ = kumo_rt::log_write(
                            kumo_abi::sys::log_level::NOTICE,
                            if titles {
                                b"drv-fb: titles on\n" as &[u8]
                            } else {
                                b"drv-fb: titles off\n"
                            },
                        );
                    }
                }
                continue;
            }
            let _ = kumo_rt::log_write(
                kumo_abi::sys::log_level::NOTICE,
                b"drv-fb: view control record\n",
            );
            let view = (n <= rx.len())
                .then(|| kumo_graphics::parse_regrant(&rx[..n]))
                .flatten();
            match view {
                Some(view) => {
                    let mut sbuf = [0u8; 32];
                    let (sn, surface_raw) =
                        channel_read_with_handle(channel, sbuf.as_mut_ptr(), sbuf.len());
                    let surface = Handle(surface_raw as u32);
                    let mut mapped = None;
                    if sn == 1 && surface.0 != 0 {
                        let (status, addr) = kumo_rt::vmar_map_anywhere(
                            surface,
                            SURFACE_BYTES as u64,
                            VmarFlags::READ.0,
                        );
                        if status == 0 && addr != 0 {
                            mapped = Some(addr);
                        }
                        let _ = kumo_rt::handle_close(surface);
                    }
                    let koid = handle_koid(channel);
                    let bound =
                        mapped.is_some() && koid != u64::MAX && port_bind(port, channel) == 0;
                    if bound {
                        let old_surface = view_surface[view];
                        let new_surface = mapped.unwrap_or(0);
                        if let Some(old) = lanes[view].take() {
                            let _ = kumo_rt::port_unbind(port, old.channel);
                            let _ = kumo_rt::handle_close(old.channel);
                        }
                        lanes[view] = Some(GraphicsLane {
                            source: Handle(koid as u32),
                            channel,
                            surface: new_surface,
                            view,
                            full_seen: false,
                        });
                        view_surface[view] = new_surface;
                        view_released[view] = false;
                        view_content[view] = None;
                        if old_surface != 0 {
                            let mut line = Line::new();
                            line.push(b"drv-fb: view ");
                            line.push_decimal(view);
                            if kumo_rt::vmar_unmap(old_surface, SURFACE_BYTES as u64) == 0 {
                                line.push(b" surface reclaimed\n");
                            } else {
                                line.push(b" surface reclaim failed\n");
                            }
                            let _ =
                                kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, line.bytes());
                        }
                        let mut line = Line::new();
                        line.push(b"drv-fb: view ");
                        line.push_decimal(view);
                        line.push(b" regranted\n");
                        let _ = kumo_rt::log_write(kumo_abi::sys::log_level::NOTICE, line.bytes());
                    } else {
                        let _ = kumo_rt::handle_close(channel);
                        let _ = kumo_rt::log_write(
                            kumo_abi::sys::log_level::NOTICE,
                            b"drv-fb: view regrant refused\n",
                        );
                    }
                }
                None => {
                    if channel.0 != 0 {
                        let _ = kumo_rt::handle_close(channel);
                    }
                }
            }
            continue;
        }
        if source == console_source {
            let n = channel_read(console, rx.as_mut_ptr(), rx.len()) as usize;
            if n > 0 && n <= rx.len() {
                let started = if deferred { kumo_rt::clock_get() } else { 0 };
                con.write(&rx[..n]);
                if deferred {
                    deferred_bytes += n as u64;
                    deferred_max_ns =
                        deferred_max_ns.max(kumo_rt::clock_get().saturating_sub(started));
                }
            }
        } else if let Some(index) = lanes
            .iter()
            .position(|lane| lane.as_ref().is_some_and(|lane| lane.source == source))
        {
            let lane = lanes[index].expect("lane selected");
            let (n, extra_handle) =
                channel_read_with_handle(lane.channel, rx.as_mut_ptr(), rx.len());
            if n == 0 && extra_handle == 0 {
                // The client exited and closed its end. The view keeps its last pixels; the
                // lane retires so a dead channel cannot keep waking the port.
                let _ = kumo_rt::port_unbind(port, lane.channel);
                let _ = kumo_rt::handle_close(lane.channel);
                lanes[index] = None;
                view_released[index] = true;
                log_info(b"drv-fb: presentation client closed\n");
                continue;
            }
            if extra_handle != 0 {
                let _ = kumo_rt::handle_close(Handle(extra_handle as u32));
            }
            let request = rx.get(..n).and_then(Present::decode);
            let reply = match (request, &layout) {
                (Some(request), Some(band)) if extra_handle == 0 && lane.surface != 0 => {
                    let view = &band.views[lane.view];
                    let result = if !request.damage.is_full() && !lane.full_seen {
                        // The view has no complete image yet; a partial first present would
                        // leave undeclared pixels uninitialized.
                        Err(Status::BadRequest)
                    } else {
                        let mut content = [None; kumo_graphics::VIEW_COUNT];
                        let mut bases = [0u64; kumo_graphics::VIEW_COUNT];
                        for (other, entry) in view_content.iter().enumerate() {
                            if let Some((base, description)) = entry {
                                bases[other] = *base;
                                content[other] = Some(*description);
                            }
                        }
                        // The presenting view's new surface applies before it is stored.
                        bases[index] = lane.surface;
                        content[index] = Some(request.surface);
                        drv_fb::compose_present_focused(
                            band,
                            stride,
                            index,
                            request.damage,
                            request.surface,
                            SURFACE_BYTES,
                            &content,
                            pixel_order,
                            focused,
                            |layer, at| unsafe {
                                // SAFETY: every Some(description) in `content` came from an
                                // accepted present backed by this retained mapping.
                                (bases[layer] as *const u32).add(at).read_volatile()
                            },
                            |at, color| unsafe {
                                (fb_va as *mut u32).add(at).write_volatile(color)
                            },
                        )
                    };
                    if result.is_ok() {
                        view_content[index] = Some((lane.surface, request.surface));
                        if let Some(entry) = lanes[index].as_mut() {
                            entry.full_seen |= request.damage.is_full();
                        }
                        // Clean only what the accepted present wrote. A refused present wrote
                        // nothing, and a partial one wrote just its declared spans. — TIMBERDOODLE
                        drv_fb::clean_present(
                            view,
                            stride,
                            request.surface,
                            request.damage,
                            |start, len| unsafe { clean_pixels(fb_va as *mut u32, start, len) },
                        );
                    }
                    Reply {
                        sequence: request.sequence,
                        status: result.err().unwrap_or(Status::Copied),
                    }
                }
                _ => Reply {
                    sequence: request.map_or(0, |r| r.sequence),
                    status: Status::BadRequest,
                },
            }
            .encode();
            let _ = kumo_rt::channel_write(lane.channel, reply.as_ptr(), reply.len());
        }
    }
}
