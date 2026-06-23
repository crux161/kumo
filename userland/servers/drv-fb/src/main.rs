#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use drv_fb::{Console, BG};
use kumo_abi::{BootInfo, Handle, VmarFlags};
use kumo_rt::{
    channel_read, channel_read_with_handle, debug_write, framebuffer_claim, handle_koid, port_bind,
    port_create, port_wait, resource_mint_mmio, vmar_map,
};

kumo_rt::entry!(main);

/// Print `label` then `v` in decimal and a newline to the debug console. The bootstrap path only
/// offers raw `debug_write(ptr, len)` (no `core::fmt`, no alloc), so format the integer by hand.
/// Used by the J231 X13s framebuffer-geometry diagnostic.
fn dbg_kv(label: &[u8], v: u64) {
    debug_write(label.as_ptr(), label.len());
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
    debug_write(line.as_ptr(), n + 1);
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
    debug_write(b"drv-fb starting\n".as_ptr(), 16);

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
        debug_write(b"drv-fb: missing handles\n".as_ptr(), 24);
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
        debug_write(b"drv-fb: bootinfo map failed\n".as_ptr(), 28);
        kumo_rt::process_exit(1);
    }

    let bootinfo = unsafe { &*(bootinfo_va as *const BootInfo) };
    if !bootinfo.has_framebuffer() {
        debug_write(b"drv-fb: no fb in bootinfo\n".as_ptr(), 26);
        kumo_rt::process_exit(1);
    }

    let fb_phys = bootinfo.framebuffer.phys;
    let fb_len = bootinfo.framebuffer.len;
    let width = bootinfo.framebuffer.width as usize;
    let height = bootinfo.framebuffer.height as usize;
    let stride = bootinfo.framebuffer.stride as usize;

    // J231 diagnostic: dump the framebuffer geometry drv-fb reads from the BootInfo VMO, so the
    // X13s scroll/overwrite bug can be pinned from serial. The scroll math is host-proven; if the
    // screen corrupts, suspect geometry — chiefly `stride != width` (scroll() copies misaligned
    // pixel bands) or an implausible `height`/`len`.
    dbg_kv(b"drv-fb fb.width=", width as u64);
    dbg_kv(b"drv-fb fb.height=", height as u64);
    dbg_kv(b"drv-fb fb.stride=", stride as u64);
    dbg_kv(b"drv-fb fb.len=", fb_len);

    // Mint and map the actual framebuffer.
    let fb_vmo_h = resource_mint_mmio(res, fb_phys, fb_len);
    if fb_vmo_h == u64::MAX {
        debug_write(b"drv-fb: fb vmo mint failed\n".as_ptr(), 27);
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
        fb_len,
        (VmarFlags::READ | VmarFlags::WRITE | VmarFlags::UNCACHED).0,
    ) != 0
    {
        debug_write(b"drv-fb: fb map failed\n".as_ptr(), 22);
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
    debug_write(b"drv-fb: probing fb write\n".as_ptr(), 25);
    let fb_words = fb_va as *mut u32;
    let last_px = stride.saturating_mul(height).saturating_sub(1);
    unsafe {
        fb_words.write_volatile(BG);
        fb_words.add(last_px).write_volatile(BG);
    }
    let probe_ok = b"drv-fb: fb probe ok\n";
    debug_write(probe_ok.as_ptr(), probe_ok.len());

    // Bind the console port before taking ownership. Diagnostics emitted immediately
    // after the claim are queued to this channel; binding first guarantees each queued
    // message also gets a port packet for the loop below.
    let port_h = port_create();
    let console_koid = handle_koid(console);
    if port_h == u64::MAX
        || console_koid == u64::MAX
        || port_bind(Handle(port_h as u32), console) != 0
    {
        debug_write(b"drv-fb: port setup failed\n".as_ptr(), 26);
        kumo_rt::process_exit(1);
    }
    let port = Handle(port_h as u32);
    let console_source = Handle(console_koid as u32);

    // This successful, capability-checked claim is the single ownership boundary:
    // before it the HAL console alone may paint the GOP framebuffer; after it every
    // kernel/user diagnostic is routed to this driver and the HAL cursor is dormant.
    // Claim only after the mapping works, so a failed driver leaves the early console live.
    if framebuffer_claim(res, fb_phys, fb_len) != 0 {
        debug_write(b"drv-fb: framebuffer claim failed\n".as_ptr(), 33);
        kumo_rt::process_exit(1);
    }

    // Build the text console over the framebuffer (clears the screen) and show first
    // light.
    let mut con = unsafe { Console::new(fb_va as *mut u32, width, height, stride) };
    con.write(b"KUMO drv-fb console ready\n");

    debug_write(b"drv-fb: initialized\n".as_ptr(), 20);
    debug_write(b"drv-fb: console ready\n".as_ptr(), 22);

    // Pump the console channel: the bytes Sora writes become glyphs on the framebuffer.
    // Output-only — no keyboard device sits behind the framebuffer yet.
    let mut rx = [0u8; 256];
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
